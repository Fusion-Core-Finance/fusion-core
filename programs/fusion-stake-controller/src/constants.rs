//! PDA seeds, external program IDs, and the draft-default protocol constants.
//!
//! Keeping seeds in one place avoids drift between instructions and the SDK (the fusd-core
//! convention). Every allocation/fee/churn number below is a **draft default**: the intended
//! conservative starting point, subject to economic simulation and audit before code freeze.
//! Once the production controller is sealed they are immutable — there are NO on-chain setters
//! for any of them, by design (credible neutrality through constrained code).

use anchor_lang::prelude::{pubkey, Pubkey};

// --- PDA seeds -----------------------------------------------------------------------------

/// `[b"controller"]` — the singleton `ControllerConfig`.
pub const CONTROLLER_SEED: &[u8] = b"controller";
/// `[b"epoch_state"]` — the singleton zero-copy crank state machine (`EpochState`).
pub const EPOCH_STATE_SEED: &[u8] = b"epoch_state";
/// `[b"validator", vote_account]` — one `ValidatorRecord` per registered vote account.
pub const VALIDATOR_RECORD_SEED: &[u8] = b"validator";
/// `[b"preference", fusion_position]` — one `Preference` per fuSOL Fusion position (the seed
/// includes the position address, so duplicate preference accounts cannot exist).
pub const PREFERENCE_SEED: &[u8] = b"preference";
/// `[b"pool_authority"]` — the stake pool's manager AND staker authority. Signs only the exact
/// CPI allowlist in `spl_cpi.rs`; no discretionary power is ever exposed through it.
pub const POOL_AUTHORITY_SEED: &[u8] = b"pool_authority";
/// `[b"deposit_authority"]` — the stake pool's SOL + stake deposit authority. Deposits flow
/// THROUGH the controller (`deposit_sol` / `deposit_stake` co-sign via this PDA); withdrawals
/// are DIRECT against the stake-pool program and never gated.
pub const DEPOSIT_AUTHORITY_SEED: &[u8] = b"deposit_authority";
/// `[b"maintenance"]` — token authority of the maintenance vault (the manager fee account).
/// May move shares ONLY as bounded crank rewards; no generic withdrawal path exists.
pub const MAINTENANCE_AUTHORITY_SEED: &[u8] = b"maintenance";

// --- External program IDs ------------------------------------------------------------------

/// The pinned FORK of the SPL Stake Pool program (`vendor/spl-stake-pool`, upstream
/// solana-program/stake-pool @ a27629b / v2.0.3 with only the `declare_id!` swapped). Every
/// stake-pool CPI targets this ID and every stake-pool-side account is owner-checked against it.
pub const FUSION_STAKE_POOL_PROGRAM_ID: Pubkey =
    pubkey!("3pYHXui7Zk21TKE6oqivqbVJWRXt74wdDkqsnb3Q8mMi");

/// fusd-core — the owner of the `Position` accounts the Preference layer reads (read-only;
/// Fusion debt paths never CPI into this controller, and this controller never writes Fusion
/// state).
pub const FUSD_CORE_PROGRAM_ID: Pubkey = pubkey!("FuSiontgYvCc2N2Cinvo5gxSuxt2UfGxKMcbzkB67kud");

/// The native Vote program — owner of every vote account accepted by `register_validator`
/// (never trust by parse alone; the runtime owner check comes first).
pub const VOTE_PROGRAM_ID: Pubkey = pubkey!("Vote111111111111111111111111111111111111111");

/// The native Stake program — a CPI pass-through account of every stake-pool instruction that
/// moves stake. Pinned as a literal: anchor-lang 0.32's `solana_program` shim exposes no
/// `stake` module.
pub const STAKE_PROGRAM_ID: Pubkey = pubkey!("Stake11111111111111111111111111111111111111");

/// The (deprecated but still required-by-interface) stake config sysvar, taken as an ACCOUNT by
/// the legacy-shaped stake-pool instructions (`AddValidatorToPool`, `IncreaseValidatorStake`).
/// Pinned as a literal so we never touch the deprecated `solana_program::stake::config` module.
pub const STAKE_CONFIG_ID: Pubkey = pubkey!("StakeConfig11111111111111111111111111111111");

// --- Draft default constants (Appendix-A of the fuSOL spec) --------------------------------

/// Lamports per SOL, for the absolute-SOL constants below.
pub const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

/// Operational reserve target as bps of total pool lamports (2%). Operational SOL liquidity
/// only — every finalized surplus above target MUST be represented in the epoch target plan.
pub const RESERVE_TARGET_BPS: u64 = 200;
/// Absolute reserve floor (10 SOL). A small operational buffer, never a fallback holding for
/// undirected stake. The effective target is `min(total, max(minimum, bps))`
/// (`fusion_stake_math::reserve::reserve_target`).
pub const RESERVE_MINIMUM_LAMPORTS: u64 = 10 * LAMPORTS_PER_SOL;

/// Per-Active-validator cap on the FINAL target (directed + neutral), bps of total pool (2%).
pub const ACTIVE_VALIDATOR_CAP_BPS: u64 = 200;
/// Per-Candidate-validator cap, bps of total pool (0.25%). Candidates may fill it with
/// explicitly DIRECTED stake only — never neutral allocation.
pub const CANDIDATE_CAP_BPS: u64 = 25;
/// Minimum calculated directed target (500 SOL) before a Registered validator may be admitted
/// as Candidate — real directed support, not mere registration, earns list capacity.
pub const MIN_ACTIVATION_TARGET_LAMPORTS: u64 = 500 * LAMPORTS_PER_SOL;

/// Consecutive healthy completed epochs (while carrying pool stake) before a Candidate promotes
/// to Active. Single-sourced from the pure math crate (the lifecycle machine enforces it).
pub use fusion_stake_math::lifecycle::CANDIDATE_HEALTHY_EPOCHS;
/// Consecutive failed completed epochs of liveness before a validator drains. Single-sourced
/// from the pure math crate.
pub use fusion_stake_math::lifecycle::LIVENESS_FAILURE_EPOCHS;

/// Maximum normalized inflation-reward commission (percent). A breach begins draining
/// IMMEDIATELY and is never suppressed by the global liveness guard.
pub const COMMISSION_CAP_PERCENT: u8 = 10;

/// Per-epoch global churn cap, bps of total pool lamports (3%): total principal moved by
/// rebalance actions in one epoch never exceeds it.
pub const GLOBAL_CHURN_CAP_BPS: u64 = 300;
/// Per-epoch per-validator move cap, bps of total pool lamports (0.5%).
pub const VALIDATOR_MOVE_CAP_BPS: u64 = 50;

/// Hysteresis absolute floor (50 SOL): a rebalance move is valid only when the target deviation
/// STRICTLY exceeds `max(HYSTERESIS_MIN_LAMPORTS, bps_of(total, HYSTERESIS_BPS))`
/// (`fusion_stake_math::churn::hysteresis`).
pub const HYSTERESIS_MIN_LAMPORTS: u64 = 50 * LAMPORTS_PER_SOL;
/// Hysteresis pool-relative component (5 bps).
pub const HYSTERESIS_BPS: u64 = 5;

/// Consecutive epochs a validator must sit in Draining before it may advance to Removable
/// (and have its list entry removed). Damps add/remove list-slot churn. Draft default —
/// the spec's lifecycle table names a removal delay without an Appendix-A value.
pub const REMOVAL_DELAY_EPOCHS: u64 = 2;

/// Vote-freshness window divisor: a validator's latest landed vote must be within
/// `epoch_slots / 8` slots (~6 h on mainnet) of the reconcile observation or the epoch counts
/// as a liveness failure. Draft default — the spec requires a "fixed epoch-relative freshness
/// window" without an Appendix-A value.
pub const VOTE_FRESHNESS_WINDOW_DIVISOR: u64 = 8;

/// Preference submission window = `epoch_slots / 32` (~13.5 min on mainnet), opened at pool
/// finalization. Snapshots are permissionless; a position omitted from the window simply stays
/// in neutral allocation for the epoch — never a financial loss.
pub const PREFERENCE_WINDOW_SLOT_DIVISOR: u64 = 32;
/// Pool-update grace = `epoch_slots / 16`: how long after an epoch boundary permissionless
/// reconciliation may lag before Fusion (not this program) freezes new fuSOL-collateral debt on
/// staleness. Recorded here because the keeper + docs derive both fractions from one place.
pub const POOL_UPDATE_GRACE_SLOT_DIVISOR: u64 = 16;

// --- Fixed stake-pool fees (set once at `initialize_pool`, no setter exists) ----------------

/// Denominator for all bps-expressed pool fees.
pub const FEE_BPS_DENOMINATOR: u64 = 10_000;
/// SOL deposit fee numerator (5 bps → maintenance vault). Extraction resistance + crank funding.
pub const SOL_DEPOSIT_FEE_BPS: u64 = 5;
/// Stake deposit fee numerator (5 bps → maintenance vault).
pub const STAKE_DEPOSIT_FEE_BPS: u64 = 5;
/// SOL withdrawal fee numerator (5 bps → maintenance vault).
pub const SOL_WITHDRAW_FEE_BPS: u64 = 5;
/// Stake withdrawal fee numerator (5 bps → maintenance vault).
pub const STAKE_WITHDRAW_FEE_BPS: u64 = 5;
/// Epoch maintenance fee: 1/100 (1%) of positive net staking rewards, minted as fuSOL to the
/// maintenance vault by the stake-pool program. Zero whenever net rewards are non-positive
/// (upstream `Fee::apply` semantics).
pub const EPOCH_MAINTENANCE_FEE_NUMERATOR: u64 = 1;
pub const EPOCH_MAINTENANCE_FEE_DENOMINATOR: u64 = 100;
/// Referral fee percent — disabled. No referral economics.
pub const REFERRAL_FEE_PERCENT: u8 = 0;
// The upstream `Initialize` carries ONE deposit fee and ONE withdrawal fee, fanned out by the
// processor to both the stake and SOL variants — so the STAKE_* constants above are
// documentation of that fan-out, and these pins make a divergent edit fail to compile instead
// of being silently ignored (only the SOL_* values are passed to the CPI).
const _: () = assert!(SOL_DEPOSIT_FEE_BPS == STAKE_DEPOSIT_FEE_BPS);
const _: () = assert!(SOL_WITHDRAW_FEE_BPS == STAKE_WITHDRAW_FEE_BPS);

/// Maximum validator-list entries, fixed at pool initialization (the pre-created ValidatorList
/// account is sized to exactly this; no dynamic resizing). Mirrors
/// `fusion_stake_math::targets::MAX_VALIDATORS` (compile-pinned below).
pub const MAX_VALIDATORS: u32 = 1_024;
const _: () = assert!(MAX_VALIDATORS as u64 == fusion_stake_math::targets::MAX_VALIDATORS);

// --- Crank reward calibration constants (fuSOL base units, 9 decimals) ----------------------
// Fixed share amounts by task class, paid from the maintenance vault for a SUCCESSFUL,
// previously incomplete crank transition; the actual payout is
// `min(task_reward, epoch_budget_remaining, vault_balance)` (`fusion_stake_math::rewards`).
// No-op / duplicate / stale-cursor / failed transactions earn zero, and an empty vault leaves
// every crank executable unpaid. Draft calibration values pending the M6 economic simulation.

/// Reward per reconcile batch that brings at least one stale validator-list entry current.
pub const CRANK_REWARD_RECONCILE_BATCH: u64 = 1_000_000; // 0.001 fuSOL
/// Reward for the epoch's pool finalization (canonical totals + NAV snapshot advance).
pub const CRANK_REWARD_FINALIZE_POOL: u64 = 1_000_000; // 0.001 fuSOL
/// Reward per plan batch (directed, neutral, or plan-finalize) that writes at least one
/// current-epoch eligibility/target result.
pub const CRANK_REWARD_PLAN_BATCH: u64 = 1_000_000; // 0.001 fuSOL
/// Reward per rebalance action whose CPI changes stake-pool state.
pub const CRANK_REWARD_REBALANCE_ACTION: u64 = 2_000_000; // 0.002 fuSOL
/// Hard per-epoch payout ceiling across ALL task classes (conserved in `EpochState`).
pub const CRANK_EPOCH_PAYOUT_BUDGET: u64 = 500_000_000; // 0.5 fuSOL

// --- Stake-program interface constants -------------------------------------------------------

/// `size_of::<StakeStateV2>()` — the fixed stake-account data size the rent floor derives from
/// (`stake_rent = Rent::minimum_balance(200)`), pinned at the vendored fork's solana-stake-
/// interface 4.2.0 (`vendor/spl-stake-pool`, UPSTREAM.md).
pub const STAKE_ACCOUNT_SPACE: usize = 200;

/// The minimum-delegation FLOOR — upstream `MINIMUM_ACTIVE_STAKE = 1_000_000` (vendor
/// `lib.rs:31`). This is NOT the effective minimum: the pool computes
/// `max(stake program GetMinimumDelegation, MINIMUM_ACTIVE_STAKE)` at every CPI (vendor
/// `lib.rs:73-75`; processor.rs:983/:1296/:1618), and the runtime value is cluster-dependent
/// (1 SOL where the raise feature is active). Every consumer therefore derives the effective
/// value at runtime via `spl_cpi::effective_minimum_delegation()` (a `GetMinimumDelegation`
/// CPI with this constant applied as the floor, mirroring upstream exactly); this constant
/// exists only as that fail-safe floor. Never size an action from it alone: a sub-minimum
/// amount makes the upstream CPI fail, which rolls back the WHOLE controller instruction —
/// the deterministic rebalance cursor would then re-select the same action forever and wedge
/// the walk until the next epoch preemption (and a Draining validator would never meet the
/// Removable threshold).
pub const UPSTREAM_MINIMUM_DELEGATION: u64 = 1_000_000;

// --- Misc sentinels --------------------------------------------------------------------------

/// `ValidatorRecord.validator_list_index` sentinel: registered but not (yet) admitted to the
/// stake-pool validator list. A real index is written when `AddValidatorToPool` lands.
pub const VALIDATOR_LIST_INDEX_UNSET: u32 = u32::MAX;

/// `ValidatorRecord.pool_entry_status` sentinel: no stake-pool list entry observed for this
/// record (not in the pool). Real values are the upstream `StakeStatus` bytes (0 = Active).
pub const POOL_ENTRY_STATUS_NONE: u8 = u8::MAX;
