/**
 * Fusion (FUSD) SDK — everything a client needs to read state and build instructions.
 *
 * - PDA derivation for every account (seeds mirror `programs/fusd-core/src/constants.rs`).
 * - A `Program` factory over the bundled, production (non-dev) IDL — instruction builders + typed
 *   account decoders come from Anchor (`program.methods.*`, `program.account.*`); Anchor auto-resolves
 *   most PDA seeds, so callers usually pass only the signer, ATAs, and oracle accounts.
 * - Pure fixed-point health math (ports `cdp.rs` / `accrual.rs`) so a UI can show a borrower's CURRENT
 *   debt, collateral ratio, health, and remaining borrow capacity without re-running the program.
 */
import { PublicKey } from "@solana/web3.js";
import { Program, AnchorProvider, type Idl } from "@coral-xyz/anchor";
import idlJson from "./idl/fusd_core.json";

export const FUSD_CORE_PROGRAM_ID = new PublicKey(
  "FuSiontgYvCc2N2Cinvo5gxSuxt2UfGxKMcbzkB67kud"
);
export const TOKEN_PROGRAM_ID = new PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
export const ASSOCIATED_TOKEN_PROGRAM_ID = new PublicKey(
  "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
);

export const IDL = idlJson as Idl;

/** Build an Anchor `Program` (instruction builders + account decoders) from the bundled IDL. */
export function getProgram(provider: AnchorProvider): Program {
  return new Program(IDL, provider);
}

// --- seeds (byte strings, exactly as in constants.rs) -------------------------------------------
export const SEEDS = {
  config: Buffer.from("config"),
  fusdMint: Buffer.from("fusd_mint"),
  mintAuthority: Buffer.from("mint_authority"),
  govGate: Buffer.from("gov_gate"),
  timelock: Buffer.from("timelock"),
  globalTimelock: Buffer.from("gtimelock"),
  backstop: Buffer.from("backstop"),
  backstopFusdVault: Buffer.from("backstop_fusd"),
  supplyRecon: Buffer.from("supply_recon"),
  registry: Buffer.from("registry"),
  market: Buffer.from("market"),
  collateralVault: Buffer.from("coll_vault"),
  marketOracle: Buffer.from("oracle"),
  dexTwap: Buffer.from("twap"),
  redemptionBitmap: Buffer.from("redeem_bitmap"),
  rateLimiter: Buffer.from("ratelimit"),
  reactorPool: Buffer.from("reactor"),
  epochToScaleToSum: Buffer.from("ess"),
  reactorFusdVault: Buffer.from("reactor_fusd"),
  reactorCollVault: Buffer.from("reactor_coll"),
  insuranceBuffer: Buffer.from("buffer"),
  bufferFusdVault: Buffer.from("buffer_fusd"),
  position: Buffer.from("position"),
  reactorDeposit: Buffer.from("reactor_dep"),
} as const;

const pda = (seeds: (Buffer | Uint8Array)[], programId: PublicKey) =>
  PublicKey.findProgramAddressSync(seeds, programId)[0];
const PID = FUSD_CORE_PROGRAM_ID;

// --- protocol-wide PDAs -------------------------------------------------------------------------
export const deriveConfig = (p = PID) => pda([SEEDS.config], p);
export const deriveFusdMint = (p = PID) => pda([SEEDS.fusdMint], p);
export const deriveMintAuthority = (p = PID) => pda([SEEDS.mintAuthority], p);
export const deriveGovGate = (p = PID) => pda([SEEDS.govGate], p);
export const deriveBackstop = (p = PID) => pda([SEEDS.backstop], p);
export const deriveBackstopFusdVault = (p = PID) => pda([SEEDS.backstopFusdVault], p);
export const deriveSupplyRecon = (p = PID) => pda([SEEDS.supplyRecon], p);
/** Queued-param timelock entry, keyed by the gate's queue nonce (u64 little-endian). */
export const deriveTimelock = (nonce: bigint, p = PID) =>
  pda([SEEDS.timelock, u64le(nonce)], p);
export const deriveGlobalTimelock = (nonce: bigint, p = PID) =>
  pda([SEEDS.globalTimelock, u64le(nonce)], p);

// --- per-market PDAs ----------------------------------------------------------------------------
const perMarket = (seed: Buffer) => (collateralMint: PublicKey, p = PID) =>
  pda([seed, collateralMint.toBuffer()], p);
export const deriveMarket = perMarket(SEEDS.market);
export const deriveCollateralVault = perMarket(SEEDS.collateralVault);
export const deriveMarketOracle = perMarket(SEEDS.marketOracle);
export const deriveDexTwap = perMarket(SEEDS.dexTwap);
export const deriveRedemptionBitmap = perMarket(SEEDS.redemptionBitmap);
export const deriveRateLimiter = perMarket(SEEDS.rateLimiter);
export const deriveReactorPool = perMarket(SEEDS.reactorPool);
export const deriveEpochToScaleToSum = perMarket(SEEDS.epochToScaleToSum);
export const deriveReactorFusdVault = perMarket(SEEDS.reactorFusdVault);
export const deriveReactorCollVault = perMarket(SEEDS.reactorCollVault);
export const deriveInsuranceBuffer = perMarket(SEEDS.insuranceBuffer);
export const deriveBufferFusdVault = perMarket(SEEDS.bufferFusdVault);

// --- per-user PDAs ------------------------------------------------------------------------------
export const derivePosition = (collateralMint: PublicKey, owner: PublicKey, p = PID) =>
  pda([SEEDS.position, collateralMint.toBuffer(), owner.toBuffer()], p);
export const deriveReactorDeposit = (collateralMint: PublicKey, owner: PublicKey, p = PID) =>
  pda([SEEDS.reactorDeposit, collateralMint.toBuffer(), owner.toBuffer()], p);

/** Associated token account (legacy SPL Token — fUSD + the supported collaterals are all legacy). */
export const deriveAta = (mint: PublicKey, owner: PublicKey) =>
  pda([owner.toBuffer(), TOKEN_PROGRAM_ID.toBuffer(), mint.toBuffer()], ASSOCIATED_TOKEN_PROGRAM_ID);

function u64le(n: bigint): Buffer {
  const b = Buffer.alloc(8);
  b.writeBigUInt64LE(n);
  return b;
}

// --- health math (mirrors cdp.rs / accrual.rs; all BigInt, round AGAINST the protocol) ----------
// `spot` is RAY-scaled fUSD-native per 1 native collateral unit; debt is fUSD-native. bps scale.
export const RAY = 10n ** 27n;
export const BPS = 10_000n;
export const SECONDS_PER_YEAR = 31_536_000n;
export const INTEREST_DENOM = SECONDS_PER_YEAR * BPS;

/** `floor(a*b / RAY)`. */
export const rayMul = (a: bigint, b: bigint) => (a * b) / RAY;
/** Collateral value in fUSD-native units (floored — conservative). */
export const collateralValue = (ink: bigint, spot: bigint) => rayMul(ink, spot);
/** Max fUSD debt for a collateral value at `mcrBps` (floored). 0 if `mcrBps == 0`. */
export const maxDebt = (value: bigint, mcrBps: bigint) => (mcrBps === 0n ? 0n : (value * BPS) / mcrBps);
/** Interest accrued over `periodSecs` at `rateBps`, floored (the per-position direction). */
export const accruedInterest = (recordedDebt: bigint, rateBps: bigint, periodSecs: bigint) =>
  (recordedDebt * rateBps * periodSecs) / INTEREST_DENOM;

/**
 * A position's CURRENT debt = `recorded_debt` + interest accrued since `lastDebtUpdate`.
 * NOTE: pending tier-2 redistribution (`Market.l_art`) is NOT included — it is applied lazily on the
 * next touch and is zero for the common case; this is a display estimate, not the exact on-touch value.
 * Assumes a LIVE (non-shutdown) market: interest STOPS at shutdown (`accrual.rs::realize` caps the
 * period at the frozen `Market.last_update_ts`), so for a shut-down market pass that frozen timestamp
 * as `nowSecs` — otherwise this over-estimates by accruing past the freeze.
 */
export function currentDebt(
  recordedDebt: bigint,
  rateBps: bigint,
  lastDebtUpdate: bigint,
  nowSecs: bigint
): bigint {
  const dt = nowSecs > lastDebtUpdate ? nowSecs - lastDebtUpdate : 0n;
  return recordedDebt + accruedInterest(recordedDebt, rateBps, dt);
}

/**
 * Above-MCR at the GIVEN price: `debt <= maxDebt(collateralValue(ink, spot), mcrBps)` (mirrors
 * `cdp::is_healthy`). The price is the caller's choice and it MATTERS: on-chain the borrow/withdraw
 * gate prices at the LOW `Market.spot`, while liquidation eligibility prices at the HIGH
 * `Market.debt_spot` (`debt_spot >= spot`, ARCHITECTURE §7). Pass `spot` for "can I borrow / am I
 * above MCR"; pass `debt_spot` for "am I liquidatable" (which is strictly more lenient).
 */
export const isHealthy = (ink: bigint, debt: bigint, spot: bigint, mcrBps: bigint) =>
  debt <= maxDebt(collateralValue(ink, spot), mcrBps);
/** Collateral ratio in bps (value/debt), or `null` when there is no debt. */
export const collateralRatioBps = (ink: bigint, debt: bigint, spot: bigint) =>
  debt === 0n ? null : (collateralValue(ink, spot) * BPS) / debt;
/**
 * Remaining fUSD a position can still borrow at `mcrBps` (floored at 0). `borrowFeeBps` (C7,
 * `Market.borrow_fee_bps`) must be supplied when the market charges an upfront borrow fee: on-chain
 * `borrow.rs` gates the POST-fee debt `amount + ceil(amount*fee/10000)`, so the largest borrowable
 * `amount` is `floor(headroom * 10000 / (10000 + fee_bps))`, strictly less than the raw headroom.
 * Omitting it (default 0) reproduces the old fee-free answer and would overstate capacity by the fee.
 */
export const maxBorrow = (ink: bigint, debt: bigint, spot: bigint, mcrBps: bigint, borrowFeeBps: bigint = 0n) => {
  const m = maxDebt(collateralValue(ink, spot), mcrBps);
  const headroom = m > debt ? m - debt : 0n;
  return borrowFeeBps === 0n ? headroom : (headroom * BPS) / (BPS + borrowFeeBps);
};

/** A position's raw on-chain fields a UI needs for the health view (all native/bps units). */
export interface PositionInput {
  ink: bigint;
  recordedDebt: bigint;
  userRateBps: bigint;
  lastDebtUpdate: bigint;
}
export interface HealthView {
  currentDebt: bigint;
  collateralValue: bigint;
  collateralRatioBps: bigint | null;
  /**
   * Above-MCR at the LOW `Market.spot` — the BORROW/withdraw view (matches borrow.rs/withdraw.rs).
   * Distinct from `liquidatable`, which prices at the HIGH `Market.debt_spot`: a position can read
   * `healthy: false` (cannot borrow more) while `liquidatable: false` (not yet eligible for liquidation).
   */
  healthy: boolean;
  /**
   * Eligible for liquidation on-chain: `!isHealthy(ink, debt, debt_spot, mcrBps)` priced at the HIGH
   * `Market.debt_spot` (matches liquidate.rs). `false` when `debtSpot == 0` (an unpriced market —
   * liquidation is fail-closed). This is the liquidation predicate; `healthy` is the borrow predicate.
   */
  liquidatable: boolean;
  maxBorrow: bigint;
}
/**
 * One-call health view: current debt (interest-accrued), CR, `healthy` (the borrow view at the LOW
 * `spot`), `liquidatable` (the on-chain liquidation view at the HIGH `debtSpot`), and remaining borrow.
 * Pass both `Market.spot` and `Market.debt_spot` — both are fields on the Market account.
 */
export function positionHealth(
  pos: PositionInput,
  market: { spot: bigint; debtSpot: bigint; mcrBps: bigint; borrowFeeBps?: bigint },
  nowSecs: bigint
): HealthView {
  const debt = currentDebt(pos.recordedDebt, pos.userRateBps, pos.lastDebtUpdate, nowSecs);
  return {
    currentDebt: debt,
    collateralValue: collateralValue(pos.ink, market.spot),
    collateralRatioBps: collateralRatioBps(pos.ink, debt, market.spot),
    healthy: isHealthy(pos.ink, debt, market.spot, market.mcrBps),
    // Liquidation eligibility prices at the HIGH debt_spot; debtSpot == 0 (unpriced) is fail-closed.
    liquidatable: market.debtSpot > 0n && !isHealthy(pos.ink, debt, market.debtSpot, market.mcrBps),
    // maxBorrow nets out the C7 upfront borrow fee when the market charges one (Market.borrow_fee_bps).
    maxBorrow: maxBorrow(pos.ink, debt, market.spot, market.mcrBps, market.borrowFeeBps ?? 0n),
  };
}
