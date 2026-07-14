/**
 * fuSOL stake-pool SDK — everything a client needs to talk to the fusion-stake-controller.
 *
 * - PDA derivation for every controller account (seeds mirror
 *   `programs/fusion-stake-controller/src/constants.rs`) plus the stake-pool-FORK-side PDAs
 *   (seeds mirror `vendor/spl-stake-pool/program/src/lib.rs`).
 * - A `Program` factory over the bundled, production controller IDL — instruction builders +
 *   typed account decoders come from Anchor (`program.methods.*`, `program.account.*`).
 * - Pure BigInt share math porting the vendored stake-pool conversions
 *   (`StakePool::calc_pool_tokens_for_deposit` / `calc_lamports_withdraw_amount`) and the
 *   ceiling `Fee::apply`, so a UI can quote deposits/withdrawals without re-running the program.
 */
import { PublicKey } from "@solana/web3.js";
import { Program, AnchorProvider, type Idl } from "@coral-xyz/anchor";
import idlJson from "./idl/fusion_stake_controller.json";

/** fusion-stake-controller (`declare_id!` in programs/fusion-stake-controller/src/lib.rs). */
export const CONTROLLER_PROGRAM_ID = new PublicKey(
  "Fz3z1yh21PQ59smsPjmjeyK6ngh8KoK6PiPxUgCgspFq"
);
/** The pinned SPL stake-pool FORK (`constants::FUSION_STAKE_POOL_PROGRAM_ID`). */
export const STAKE_POOL_FORK_ID = new PublicKey("3pYHXui7Zk21TKE6oqivqbVJWRXt74wdDkqsnb3Q8mMi");

export const CONTROLLER_IDL = idlJson as Idl;

/** Build an Anchor `Program` (instruction builders + account decoders) from the bundled IDL. */
export function getControllerProgram(provider: AnchorProvider): Program {
  return new Program(CONTROLLER_IDL, provider);
}

// --- seeds (byte strings, exactly as in the two on-chain sources) --------------------------------
export const SEEDS = {
  // controller-side (programs/fusion-stake-controller/src/constants.rs)
  controller: Buffer.from("controller"),
  epochState: Buffer.from("epoch_state"),
  validatorRecord: Buffer.from("validator"),
  preference: Buffer.from("preference"),
  poolAuthority: Buffer.from("pool_authority"),
  depositAuthority: Buffer.from("deposit_authority"),
  maintenanceAuthority: Buffer.from("maintenance"),
  // stake-pool-FORK-side (vendor/spl-stake-pool/program/src/lib.rs)
  withdraw: Buffer.from("withdraw"),
  transient: Buffer.from("transient"),
} as const;

const pda = (seeds: (Buffer | Uint8Array)[], programId: PublicKey) =>
  PublicKey.findProgramAddressSync(seeds, programId)[0];
const PID = CONTROLLER_PROGRAM_ID;
const FORK = STAKE_POOL_FORK_ID;

// --- controller PDAs ------------------------------------------------------------------------------
/** `[b"controller"]` — the singleton `ControllerConfig`. */
export const controllerConfig = (p = PID) => pda([SEEDS.controller], p);
/** `[b"epoch_state"]` — the singleton zero-copy crank state machine. */
export const epochState = (p = PID) => pda([SEEDS.epochState], p);
/** `[b"validator", vote_account]` — one `ValidatorRecord` per registered vote account. */
export const validatorRecord = (voteAccount: PublicKey, p = PID) =>
  pda([SEEDS.validatorRecord, voteAccount.toBuffer()], p);
/** `[b"preference", fusion_position]` — one `Preference` per fuSOL Fusion position. */
export const preference = (fusionPosition: PublicKey, p = PID) =>
  pda([SEEDS.preference, fusionPosition.toBuffer()], p);
/** `[b"pool_authority"]` — the stake pool's manager AND staker authority. */
export const poolAuthority = (p = PID) => pda([SEEDS.poolAuthority], p);
/** `[b"deposit_authority"]` — the pool's SOL + stake deposit authority (deposits flow through). */
export const depositAuthority = (p = PID) => pda([SEEDS.depositAuthority], p);
/** `[b"maintenance"]` — token authority of the maintenance vault (the manager fee account). */
export const maintenanceAuthority = (p = PID) => pda([SEEDS.maintenanceAuthority], p);

// --- stake-pool-side PDAs (derived under the FORK program id) -------------------------------------
/** `[stake_pool, b"withdraw"]` — the pool's withdraw authority (and the fuSOL mint authority). */
export const poolWithdrawAuthority = (stakePool: PublicKey, p = FORK) =>
  pda([stakePool.toBuffer(), SEEDS.withdraw], p);
/**
 * `[vote_account, stake_pool, (seed: u32 LE)?]` — a validator's pool stake account. The numeric
 * seed mirrors upstream `Option<NonZeroU32>`: appended ONLY when non-zero (omitted/0 = no suffix,
 * the canonical account every controller-managed validator uses).
 */
export const validatorStake = (
  voteAccount: PublicKey,
  stakePool: PublicKey,
  seed?: number,
  p = FORK
) => {
  const seeds: Buffer[] = [voteAccount.toBuffer(), stakePool.toBuffer()];
  if (seed) seeds.push(u32le(seed));
  return pda(seeds, p);
};
/** `[b"transient", vote_account, stake_pool, seed: u64 LE]` — the in-flight rebalance account. */
export const transientStake = (
  voteAccount: PublicKey,
  stakePool: PublicKey,
  seed: bigint,
  p = FORK
) => pda([SEEDS.transient, voteAccount.toBuffer(), stakePool.toBuffer(), u64le(seed)], p);

function u32le(n: number): Buffer {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(n);
  return b;
}
function u64le(n: bigint): Buffer {
  const b = Buffer.alloc(8);
  b.writeBigUInt64LE(n);
  return b;
}

// --- share math (mirrors vendor/spl-stake-pool state.rs; all BigInt) ------------------------------
/** Denominator for all bps-expressed pool fees (`constants::FEE_BPS_DENOMINATOR`). */
export const FEE_BPS_DENOMINATOR = 10_000n;

/**
 * Lamports -> fuSOL shares on deposit (`StakePool::calc_pool_tokens_for_deposit`): 1:1 when the
 * pool has zero total lamports OR zero share supply (the bootstrap case), otherwise
 * `floor(lamports · supply / totalLamports)` — rounds against the depositor.
 */
export const solToShares = (lamports: bigint, totalLamports: bigint, supply: bigint) =>
  totalLamports === 0n || supply === 0n ? lamports : (lamports * supply) / totalLamports;

/**
 * fuSOL shares -> lamports on withdrawal (`StakePool::calc_lamports_withdraw_amount`):
 * `floor(shares · totalLamports / supply)` — rounds against the withdrawer. NOTE the upstream
 * asymmetry: at zero supply this is 0 (nothing to redeem against), NOT the deposit-side 1:1.
 */
export const sharesToSol = (shares: bigint, totalLamports: bigint, supply: bigint) =>
  supply === 0n ? 0n : (shares * totalLamports) / supply;

/**
 * A bps fee on `amount`, CEILED exactly like upstream `Fee::apply`
 * (`(amount · bps + 9999) / 10000`): any non-zero amount at a non-zero fee pays at least 1.
 */
export const applyFeeBps = (amount: bigint, bps: bigint) =>
  (amount * bps + FEE_BPS_DENOMINATOR - 1n) / FEE_BPS_DENOMINATOR;
