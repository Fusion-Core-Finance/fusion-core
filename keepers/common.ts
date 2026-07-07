/**
 * Shared helpers for the permissionless keeper bots (liquidator.ts, redeemer.ts) — provider/program
 * setup, the per-market PDA bundle, position scanning, the liquidation health predicate (a faithful
 * BigInt port of the on-chain cdp::is_healthy, priced at debt_spot), and on-demand ATA creation.
 *
 * These bots are PERMISSIONLESS reference implementations: anyone can run them, kept profitable by the
 * liquidation bonus + the redemption-fee spread. The on-chain instruction is the source of truth — the
 * bots only select candidates and submit; a healthy position is rejected on-chain (no harm, just a
 * wasted tx), so the off-chain math is an approximation tuned to avoid obviously-wasted submissions.
 */
import * as anchor from "@coral-xyz/anchor";
import * as fs from "fs";
import * as os from "os";

export const { PublicKey, Keypair, Connection } = anchor.web3;
export type Pk = anchor.web3.PublicKey;
export const BN = anchor.BN;

export const TOKEN_PROGRAM = new PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const ATA_PROGRAM = new PublicKey("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

// On-chain constants (programs/fusd-core/src/constants.rs).
export const RAY = 10n ** 27n;
export const BPS = 10_000n;
export const REDIST_PRECISION = 10n ** 18n; // fusd_math::redistribution::PRECISION (1e18 reward-per-unit)
export const FUSD_DECIMALS = 6;
export const SECONDS_PER_YEAR = 31_536_000n;
export const MAX_PRICE_STALENESS_SLOTS = 250n;
/** shutdown.rs: past this spot staleness (~1h) anyone can trip the IRREVERSIBLE market shutdown. */
export const SHUTDOWN_ORACLE_STALENESS_SLOTS = 9000n;
export const ZOMBIE_BUCKET = 256;
export const MAX_REDEMPTION_CANDIDATES = 20;

export const seed = (s: string) => Buffer.from(s);
export function pda(seeds: (Buffer | Pk)[], pid: Pk): Pk {
  return PublicKey.findProgramAddressSync(seeds.map((s) => (s instanceof PublicKey ? s.toBuffer() : s)), pid)[0];
}
export const ataFor = (mint: Pk, owner: Pk): Pk => pda([owner, TOKEN_PROGRAM, mint], ATA_PROGRAM);
export const log = (m: string) => console.log(`${new Date().toISOString()} ${m}`);

export function loadWallet(): anchor.Wallet {
  const path = process.env.ANCHOR_WALLET || `${os.homedir()}/.config/solana/id.json`;
  return new anchor.Wallet(Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(path, "utf8")))));
}

/** The program IDL: a local anchor build (target/idl) wins; a build-less checkout (e.g. the keeper
 * server) falls back to the committed production copy in sdk/src/idl (kept current via
 * `yarn --cwd sdk sync-idl`). Both are the full production IDL — no dev_set_price. */
export function loadIdl(): anchor.Idl {
  for (const p of [`${__dirname}/../target/idl/fusd_core.json`, `${__dirname}/../sdk/src/idl/fusd_core.json`]) {
    if (fs.existsSync(p)) return JSON.parse(fs.readFileSync(p, "utf8"));
  }
  throw new Error("no IDL found — run `anchor build` or restore sdk/src/idl/fusd_core.json");
}

/** Anchor program + provider (IDL via loadIdl). Pass a wallet to override the
 * default keypair file — e.g. a throwaway `new anchor.Wallet(Keypair.generate())` for read-only use. */
export function makeProgram(wallet: anchor.Wallet = loadWallet()) {
  const url = process.env.ANCHOR_PROVIDER_URL || "http://127.0.0.1:8899";
  const provider = new anchor.AnchorProvider(new Connection(url, "confirmed"), wallet, { commitment: "confirmed" });
  anchor.setProvider(provider);
  const program: any = new anchor.Program(loadIdl(), provider);
  return { program, provider, pid: program.programId as Pk, me: wallet.publicKey as Pk, url };
}

/** Every PDA the liquidate/redeem instructions need for one market (seeds from constants.rs). */
export function bundle(pid: Pk, coll: Pk) {
  return {
    coll,
    config: pda([seed("config")], pid),
    fusdMint: pda([seed("fusd_mint")], pid),
    mintAuthority: pda([seed("mint_authority")], pid),
    market: pda([seed("market"), coll], pid),
    collateralVault: pda([seed("coll_vault"), coll], pid),
    redemptionBitmap: pda([seed("redeem_bitmap"), coll], pid),
    reactorPool: pda([seed("reactor"), coll], pid),
    ess: pda([seed("ess"), coll], pid),
    reactorFusdVault: pda([seed("reactor_fusd"), coll], pid),
    reactorCollVault: pda([seed("reactor_coll"), coll], pid),
    buffer: pda([seed("buffer"), coll], pid),
    bufferFusdVault: pda([seed("buffer_fusd"), coll], pid),
  };
}
export const positionPda = (pid: Pk, coll: Pk, owner: Pk): Pk => pda([seed("position"), coll, owner], pid);

/** BN/number/string → bigint (Anchor account fields decode as BN). */
export const bi = (v: any): bigint => BigInt(v.toString());

/** A scanned position (BigInt-normalized). collateral_mint is at byte offset 40 (8 disc + 32 owner). */
export interface ScannedPosition {
  pubkey: Pk;
  owner: Pk;
  ink: bigint;
  recordedDebt: bigint;
  userRateBps: number;
  lastDebtUpdate: bigint;
  bucket: number;
  // tier-2 redistribution inputs (accrual::realize folds these into ink/recorded_debt on every touch)
  stake: bigint;
  redistLCollSnapshot: bigint;
  redistLArtSnapshot: bigint;
}
// Default: getProgramAccounts with a memcmp on collateral_mint (offset 40) — the permissionless
// discovery path, but it needs a gPA-capable RPC. If `watch` is given, fetch exactly those position
// accounts directly via getAccountInfo and keep the ones for `coll`: the path for RPCs that throttle
// or disable gPA (most providers) or don't implement it (surfpool), fed by an indexer/monitor list.
export async function scanPositions(program: any, coll: Pk, watch?: Pk[]): Promise<ScannedPosition[]> {
  let accts: any[];
  if (watch && watch.length) {
    const fetched = await Promise.all(watch.map((pk) => program.account.position.fetchNullable(pk)));
    accts = fetched
      .map((account, i) => (account ? { publicKey: watch[i], account } : null))
      .filter((a) => a && a.account.collateralMint.equals(coll));
  } else {
    accts = await program.account.position.all([{ memcmp: { offset: 40, bytes: coll.toBase58() } }]);
  }
  return accts.map((a: any) => ({
    pubkey: a.publicKey,
    owner: a.account.owner,
    ink: bi(a.account.ink),
    recordedDebt: bi(a.account.recordedDebt),
    userRateBps: Number(a.account.userRateBps),
    lastDebtUpdate: bi(a.account.lastDebtUpdate),
    bucket: Number(a.account.bucket),
    stake: bi(a.account.stake),
    redistLCollSnapshot: bi(a.account.redistLCollSnapshot),
    redistLArtSnapshot: bi(a.account.redistLArtSnapshot),
  }));
}

/** Present (interest-accrued) debt — port of accrual::accrued_interest + the SDK currentDebt. */
export function currentDebt(recordedDebt: bigint, rateBps: number, lastDebtUpdate: bigint, nowSecs: bigint): bigint {
  const period = nowSecs > lastDebtUpdate ? nowSecs - lastDebtUpdate : 0n;
  const accrued = (recordedDebt * BigInt(rateBps) * period) / (SECONDS_PER_YEAR * BPS);
  return recordedDebt + accrued;
}

/** A position's pending (unrealized) tier-2 redistribution gains `(coll, debt)` — a BigInt port of
 * redist::pending → fusd_math::redistribution::pending_one: `stake·(l − snapshot)/1e18`, floored per
 * leg (protocol-favoring dust). `lColl`/`lArt` are the market's accumulators; the snapshots are the
 * position's own. accrual::realize folds `coll` into `ink` and `debt` into `recorded_debt` BEFORE the
 * liquidation health check, so the bot must too — else a position pushed under MCR by a prior
 * redistribution is invisible. Best-effort estimate; the on-chain re-check is authoritative. */
export function pendingRedist(
  stake: bigint, lColl: bigint, lArt: bigint, snapColl: bigint, snapArt: bigint,
): { coll: bigint; debt: bigint } {
  // pending_one: delta = l.saturating_sub(snap); 0 when stake==0 or delta==0; else floor(stake*delta/1e18).
  const one = (l: bigint, snap: bigint): bigint =>
    stake === 0n || l <= snap ? 0n : (stake * (l - snap)) / REDIST_PRECISION;
  return { coll: one(lColl, snapColl), debt: one(lArt, snapArt) };
}
/** cdp::is_healthy at the HIGH debt_spot price: liquidatable when present debt exceeds the max. */
export function isLiquidatable(ink: bigint, presentDebt: bigint, debtSpot: bigint, mcrBps: number): boolean {
  if (presentDebt === 0n || mcrBps === 0 || debtSpot === 0n) return false;
  const collValue = (ink * debtSpot) / RAY; // fUSD-native
  const maxDebt = (collValue * BPS) / BigInt(mcrBps);
  return presentDebt > maxDebt;
}

/** Ensure `owner`'s ATA for `mint` exists, creating it (payer = provider wallet) if missing.
 * `preIxs` prepends e.g. compute-budget instructions to the creation tx (no-op when the ATA exists). */
export async function ensureAta(
  provider: anchor.AnchorProvider, mint: Pk, owner: Pk, preIxs: anchor.web3.TransactionInstruction[] = [],
): Promise<Pk> {
  const ata = ataFor(mint, owner);
  if (!(await provider.connection.getAccountInfo(ata))) {
    let spl: any;
    try { spl = require("@solana/spl-token"); }
    catch { throw new Error("ATA creation needs @solana/spl-token (npm i @solana/spl-token)"); }
    const tx = new anchor.web3.Transaction().add(
      ...preIxs,
      spl.createAssociatedTokenAccountInstruction(provider.wallet.publicKey, ata, owner, mint),
    );
    await provider.sendAndConfirm(tx);
  }
  return ata;
}

/** First line of a SendTransactionError/AnchorError message, for compact one-line logging —
 * PLUS the first meaningful simulation log line: a bare "Simulation failed." hid an
 * insufficient-fee-payer drain for hours (2026-07-06); the log line names the cause. */
export const errLine = (e: any): string => {
  const first = (e?.message || String(e)).split("\n")[0];
  const logs: string[] = e?.logs ?? e?.simulationResponse?.logs ?? [];
  const hint = Array.isArray(logs) ? logs.find((l) => /err|fail|insufficient|exceeded/i.test(l)) : undefined;
  return hint && !first.includes(hint) ? `${first} — ${hint.slice(0, 140)}` : first;
};

/** RPC url safe for logs/metrics: origin only — provider urls carry API keys in the query. */
export const redactUrl = (u: string): string => {
  try { return new URL(u).origin; } catch { return "<invalid-url>"; }
};

// Priority fee (µlamports/CU) on every keeper send path: congestion — volatility ⇒ fee spikes — is
// exactly when a tx must land, and a zero-fee tx is dropped first. Parsed once from
// PRIORITY_FEE_MICROLAMPORTS (default 20,000) on first use; a garbage value fails loud.
let _priorityFee: number | undefined;
export function priorityFeeMicroLamports(): number {
  if (_priorityFee === undefined) {
    const raw = process.env.PRIORITY_FEE_MICROLAMPORTS?.trim();
    _priorityFee = raw ? Number(raw) : 20_000;
    if (!Number.isInteger(_priorityFee) || _priorityFee < 0)
      throw new Error(`PRIORITY_FEE_MICROLAMPORTS must be a non-negative integer (got ${process.env.PRIORITY_FEE_MICROLAMPORTS})`);
  }
  return _priorityFee;
}
/** Compute-budget instructions to `.preInstructions([...])` on any anchor send: a priority-fee price
 *  (always) plus an optional CU limit. Position-independent (the runtime scans the whole message). */
export function priorityIxs(cuLimit?: number): anchor.web3.TransactionInstruction[] {
  const ixs = [anchor.web3.ComputeBudgetProgram.setComputeUnitPrice({ microLamports: priorityFeeMicroLamports() })];
  if (cuLimit !== undefined) ixs.unshift(anchor.web3.ComputeBudgetProgram.setComputeUnitLimit({ units: cuLimit }));
  return ixs;
}
