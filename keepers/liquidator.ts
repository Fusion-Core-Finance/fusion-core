/**
 * fUSD liquidator — a permissionless bot that scans positions and liquidates any that have fallen
 * below their MCR, earning the liquidation bonus (collateral gas-comp + the position's SOL reserve bond).
 *
 * Each scan, per market: fetch the Market, check the liquidation GATES (a liquidation can't land while
 * any of these hold, so skip to save fees) — market shut down, spot == 0, the cached price stale
 * (slot − spot_updated_slot > MAX_PRICE_STALENESS_SLOTS), the on-resume grace (liq_grace_until) or the
 * oracle-divergence pause (liq_divergence_until) active — then enumerate Position accounts via getProgram-
 * Accounts and liquidate any whose interest-accrued debt exceeds the max at Market.debt_spot (the HIGH
 * liquidation price). The on-chain instruction re-checks health, so a position that cured between scan
 * and send is rejected (PositionHealthy) and skipped, not retried.
 *
 * The waterfall (RP offset → redistribution → buffer → backstop → un-homed) is on-chain; this bot just
 * triggers it. The global-backstop accounts are passed null (tier 3.5 stays off unless governance arms it).
 *
 * USAGE
 *   ANCHOR_PROVIDER_URL=<rpc> ANCHOR_WALLET=~/.config/solana/id.json \
 *     npx ts-node keepers/liquidator.ts [config.json]
 *   No config arg → the built-in WSOL fork default below.
 */
import * as anchor from "@coral-xyz/anchor";
import * as fs from "fs";
import {
  PublicKey, Pk, TOKEN_PROGRAM, MAX_PRICE_STALENESS_SLOTS, log, makeProgram, bundle, scanPositions,
  currentDebt, isLiquidatable, pendingRedist, ensureAta, errLine, priorityIxs, redactUrl,
} from "./common";

// A liquidation must land during a collateral crash — precisely when Solana's fee market spikes — so it
// carries a priority fee + CU headroom for the waterfall (RP offset → redistribution → buffer). A
// zero-fee liquidate is dropped exactly when it matters (finding: keeper-security).
const CU_LIMIT_LIQUIDATE = 300_000;

interface LiqCfg {
  scanIntervalSecs?: number;
  markets: string[]; // collateral mints to watch
  // Optional explicit position pubkeys to check (from an indexer/monitor). When set, the bot fetches
  // these directly instead of getProgramAccounts — required on RPCs that throttle/disable gPA.
  watchPositions?: string[];
}
const DEFAULT_CFG: LiqCfg = {
  scanIntervalSecs: 20,
  markets: ["So11111111111111111111111111111111111111112"], // WSOL
};

export function validateConfig(cfg: LiqCfg): void {
  const bail = (m: string): never => { throw new Error(`config: ${m}`); };
  const v = cfg.scanIntervalSecs;
  if (v !== undefined && (typeof v !== "number" || !Number.isFinite(v) || v <= 0))
    bail(`scanIntervalSecs must be a positive number (got ${v})`);
  if (!Array.isArray(cfg.markets) || cfg.markets.length === 0) bail("markets must be a non-empty array");
  cfg.markets.forEach((m, i) => { try { new PublicKey(m); } catch { bail(`markets[${i}] is not a valid pubkey: ${m}`); } });
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

async function scanAndLiquidate(program: any, provider: anchor.AnchorProvider, pid: Pk, me: Pk, coll: Pk, watch?: Pk[]) {
  const tag = coll.toBase58().slice(0, 6);
  const p = bundle(pid, coll);
  const m: any = await program.account.market.fetch(p.market);
  const slot = BigInt(await provider.connection.getSlot());

  // Gates: a liquidation cannot land while any of these hold — skip the scan to save fees.
  const spot = BigInt(m.spot.toString());
  const debtSpot = BigInt(m.debtSpot.toString());
  if (m.shutdown) return log(`· ${tag} skip: market shut down`);
  if (spot === 0n || debtSpot === 0n) return log(`· ${tag} skip: no price`);
  if (slot - BigInt(m.spotUpdatedSlot.toString()) > MAX_PRICE_STALENESS_SLOTS) return log(`· ${tag} skip: stale price`);
  if (slot < BigInt(m.liqGraceUntil.toString())) return log(`· ${tag} skip: on-resume grace window`);
  if (slot < BigInt(m.liqDivergenceUntil.toString())) return log(`· ${tag} skip: oracle-divergence pause`);

  const mcrBps = Number(m.mcrBps);
  const now = BigInt(Math.floor(Date.now() / 1000));
  // Market redistribution accumulators — fold each position's pending share into its health, matching
  // accrual::realize (run on-chain before the liquidation check).
  const lColl = BigInt(m.lColl.toString());
  const lArt = BigInt(m.lArt.toString());
  const positions = await scanPositions(program, coll, watch);
  const targets = positions.filter((q) => {
    // Fold pending tier-2 redistribution into ink + present debt exactly as accrual::realize does
    // on-chain BEFORE the health check — else a position pushed under MCR by a prior redistribution
    // (its recorded_debt/ink still stale) is invisible. `debt > 0` mirrors liquidate.rs's require!.
    const pend = pendingRedist(q.stake, lColl, lArt, q.redistLCollSnapshot, q.redistLArtSnapshot);
    const debt = currentDebt(q.recordedDebt, q.userRateBps, q.lastDebtUpdate, now) + pend.debt;
    return debt > 0n && isLiquidatable(q.ink + pend.coll, debt, debtSpot, mcrBps);
  });
  if (targets.length === 0) return log(`· ${tag} scanned ${positions.length} positions, none liquidatable`);

  const liqAta = await ensureAta(provider, coll, me); // receives the gas-comp skim
  log(`! ${tag} ${targets.length} liquidatable of ${positions.length}`);
  for (const t of targets) {
    try {
      const sig = await program.methods.liquidate().accounts({
        liquidator: me, collateralMint: coll, market: p.market, position: t.pubkey,
        reactorPool: p.reactorPool, epochToScaleToSum: p.ess, marketCollVault: p.collateralVault,
        reactorFusdVault: p.reactorFusdVault, reactorCollVault: p.reactorCollVault, fusdMint: p.fusdMint,
        liquidatorCollateralAta: liqAta, redemptionBitmap: p.redemptionBitmap, insuranceBuffer: p.buffer,
        bufferFusdVault: p.bufferFusdVault, backstop: null, backstopFusdVault: null, tokenProgram: TOKEN_PROGRAM,
      }).preInstructions(priorityIxs(CU_LIMIT_LIQUIDATE)).rpc();
      log(`  ✓ liquidated ${t.owner.toBase58().slice(0, 6)} (${sig.slice(0, 16)}…)`);
    } catch (e: any) {
      // PositionHealthy / BelowMinCollateralRatio = cured or already taken between scan and send: skip, don't retry.
      log(`  · skip ${t.owner.toBase58().slice(0, 6)}: ${errLine(e)}`);
    }
  }
}

async function main() {
  const cfgPath = process.argv[2];
  const cfg: LiqCfg = cfgPath ? JSON.parse(fs.readFileSync(cfgPath, "utf8")) : DEFAULT_CFG;
  validateConfig(cfg);
  const { program, provider, pid, me, url } = makeProgram();
  log(`liquidator up — program ${pid.toBase58()}, wallet ${me.toBase58()}, RPC ${redactUrl(url)}`);
  const watch = cfg.watchPositions?.map((s) => new PublicKey(s));
  log(`markets: ${cfg.markets.join(", ")}${watch ? ` (watching ${watch.length} explicit position(s))` : " (getProgramAccounts scan)"}`);
  for (const mint of cfg.markets) {
    const coll = new PublicKey(mint);
    every(`liquidate ${mint.slice(0, 6)}`, cfg.scanIntervalSecs ?? 20, () =>
      scanAndLiquidate(program, provider, pid, me, coll, watch));
  }
}

if (require.main === module) {
  main().catch((e) => { console.error(e); process.exit(1); });
}
