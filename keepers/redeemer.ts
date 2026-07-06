/**
 * fUSD redeemer — a permissionless bot that defends the peg floor: it burns fUSD for $1-of-collateral
 * (minus the flat redemption fee) against the lowest-interest-rate borrowers, exactly the path that
 * pulls fUSD back to $1 when it trades below peg.
 *
 * Each scan, per market: check the redemption GATES (skip if shut down / no price / stale — redemption
 * deliberately IGNORES the liquidation grace + divergence pauses, the peg floor stays open), enumerate
 * positions, find the LOWEST non-empty normal rate bucket (the program requires candidates start there),
 * take up to MAX_REDEMPTION_CANDIDATES of its members, and submit redeem(amount) with them as writable
 * remaining_accounts. The on-chain instruction sorts candidates by collateral ratio and consumes them.
 *
 * PROFITABILITY (operator decision): redeeming returns $1 of collateral per $1 fUSD burned, minus
 * `redemption_fee_bps`. It only profits when you sourced fUSD BELOW $1 (i.e. fUSD is off-peg low). The
 * protocol has no on-chain fUSD/USD oracle (its peg IS this redemption floor), so the bot can't detect
 * the discount itself — it is GATED by `redeemAmountFusd` per market: leave it 0 (default) to stay idle,
 * and raise it (with fUSD acquired below peg) when you intend to redeem. The per-tick redeem is capped by
 * your fUSD balance.
 *
 * USAGE
 *   ANCHOR_PROVIDER_URL=<rpc> ANCHOR_WALLET=~/.config/solana/id.json \
 *     npx ts-node keepers/redeemer.ts [config.json]
 *   No config arg → the built-in WSOL fork default (redeemAmountFusd 0 = idle until you set it).
 */
import * as anchor from "@coral-xyz/anchor";
import * as fs from "fs";
import {
  PublicKey, Pk, BN, TOKEN_PROGRAM, FUSD_DECIMALS, MAX_PRICE_STALENESS_SLOTS, ZOMBIE_BUCKET,
  MAX_REDEMPTION_CANDIDATES, log, makeProgram, bundle, scanPositions, ensureAta, errLine, priorityIxs,
} from "./common";

// Redemption defends the peg floor during a depeg — a congested period — so it carries a priority fee,
// plus CU headroom for a full MAX_REDEMPTION_CANDIDATES batch (each candidate is realized + reweighted
// twice), well above the 200k default (finding: keeper-security).
const CU_LIMIT_REDEEM = 400_000;

interface RedeemMarketCfg { collateralMint: string; redeemAmountFusd?: number; }
interface RedeemerCfg {
  scanIntervalSecs?: number;
  markets: RedeemMarketCfg[];
  // Optional explicit position pubkeys (from an indexer/monitor). When set, the bot fetches these
  // directly instead of getProgramAccounts — required on RPCs that throttle/disable gPA.
  watchPositions?: string[];
}
const DEFAULT_CFG: RedeemerCfg = {
  scanIntervalSecs: 30,
  markets: [{ collateralMint: "So11111111111111111111111111111111111111112", redeemAmountFusd: 0 }],
};

export function validateConfig(cfg: RedeemerCfg): void {
  const bail = (m: string): never => { throw new Error(`config: ${m}`); };
  const v = cfg.scanIntervalSecs;
  if (v !== undefined && (typeof v !== "number" || !Number.isFinite(v) || v <= 0))
    bail(`scanIntervalSecs must be a positive number (got ${v})`);
  if (!Array.isArray(cfg.markets) || cfg.markets.length === 0) bail("markets must be a non-empty array");
  cfg.markets.forEach((m, i) => {
    try { new PublicKey(m.collateralMint); } catch { bail(`markets[${i}].collateralMint is not a valid pubkey`); }
    if (m.redeemAmountFusd !== undefined && (typeof m.redeemAmountFusd !== "number" || !Number.isFinite(m.redeemAmountFusd) || m.redeemAmountFusd < 0))
      bail(`markets[${i}].redeemAmountFusd must be a non-negative number`);
  });
  if (cfg.watchPositions !== undefined) {
    if (!Array.isArray(cfg.watchPositions)) bail("watchPositions must be an array of position pubkeys");
    cfg.watchPositions.forEach((p, i) => { try { new PublicKey(p); } catch { bail(`watchPositions[${i}] is not a valid pubkey: ${p}`); } });
  }
}

function every(label: string, secs: number, fn: () => Promise<void>) {
  const tick = async () => { try { await fn(); } catch (e: any) { log(`✗ ${label}: ${errLine(e)}`); } };
  tick();
  return setInterval(tick, secs * 1000);
}

async function scanAndRedeem(program: any, provider: anchor.AnchorProvider, pid: Pk, me: Pk, mc: RedeemMarketCfg, watch?: Pk[]) {
  const coll = new PublicKey(mc.collateralMint);
  const tag = mc.collateralMint.slice(0, 6);
  const wantFusd = BigInt(Math.round((mc.redeemAmountFusd ?? 0) * 10 ** FUSD_DECIMALS));
  if (wantFusd === 0n) return; // idle until the operator sets a redeem amount (see header)

  const p = bundle(pid, coll);
  const m: any = await program.account.market.fetch(p.market);
  const slot = BigInt(await provider.connection.getSlot());
  if (m.shutdown) return log(`· ${tag} skip: market shut down (use urgent_redeem post-shutdown)`);
  if (BigInt(m.spot.toString()) === 0n) return log(`· ${tag} skip: no price`);
  if (slot - BigInt(m.spotUpdatedSlot.toString()) > MAX_PRICE_STALENESS_SLOTS) return log(`· ${tag} skip: stale price`);

  // Lowest non-empty NORMAL bucket = the redemption target; candidates must all be in it.
  const live = (await scanPositions(program, coll, watch)).filter((q) => q.recordedDebt > 0n && q.bucket !== ZOMBIE_BUCKET);
  if (live.length === 0) return log(`· ${tag} skip: nothing to redeem (no live normal-bucket positions)`);
  const lowest = Math.min(...live.map((q) => q.bucket));
  const candidates = live.filter((q) => q.bucket === lowest).slice(0, MAX_REDEMPTION_CANDIDATES);

  // Cap the redeem by the redeemer's fUSD balance.
  const fusdAta = await ensureAta(provider, p.fusdMint, me);
  const collAta = await ensureAta(provider, coll, me);
  const fusdBal = BigInt((await provider.connection.getTokenAccountBalance(fusdAta)).value.amount);
  const amount = wantFusd < fusdBal ? wantFusd : fusdBal;
  if (amount === 0n) return log(`· ${tag} skip: redeemer holds no fUSD`);

  log(`! ${tag} redeem ${Number(amount) / 1e6} fUSD vs ${candidates.length} candidate(s) in bucket ${lowest}`);
  try {
    const sig = await program.methods.redeem(new BN(amount.toString())).accounts({
      redeemer: me, collateralMint: coll, market: p.market, redemptionBitmap: p.redemptionBitmap,
      fusdMint: p.fusdMint, marketCollVault: p.collateralVault, redeemerFusdAta: fusdAta,
      redeemerCollateralAta: collAta, tokenProgram: TOKEN_PROGRAM,
    }).remainingAccounts(candidates.map((q) => ({ pubkey: q.pubkey, isWritable: true, isSigner: false })))
      .preInstructions(priorityIxs(CU_LIMIT_REDEEM)).rpc();
    log(`  ✓ redeemed (${sig.slice(0, 16)}…)`);
  } catch (e: any) {
    log(`  · redeem skipped: ${errLine(e)}`);
  }
}

async function main() {
  const cfgPath = process.argv[2];
  const cfg: RedeemerCfg = cfgPath ? JSON.parse(fs.readFileSync(cfgPath, "utf8")) : DEFAULT_CFG;
  validateConfig(cfg);
  const { program, provider, pid, me, url } = makeProgram();
  log(`redeemer up — program ${pid.toBase58()}, wallet ${me.toBase58()}, RPC ${url}`);
  const watch = cfg.watchPositions?.map((s) => new PublicKey(s));
  log(`markets: ${cfg.markets.map((m) => `${m.collateralMint.slice(0, 6)}@${m.redeemAmountFusd ?? 0}fUSD`).join(", ")}${watch ? ` (watching ${watch.length} explicit position(s))` : " (getProgramAccounts scan)"}`);
  for (const mc of cfg.markets) {
    every(`redeem ${mc.collateralMint.slice(0, 6)}`, cfg.scanIntervalSecs ?? 30, () =>
      scanAndRedeem(program, provider, pid, me, mc, watch));
  }
}

if (require.main === module) {
  main().catch((e) => { console.error(e); process.exit(1); });
}
