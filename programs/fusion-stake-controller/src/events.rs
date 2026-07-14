//! Anchor events — the off-chain observability layer (fusd-core conventions).
//!
//! Every value-moving / allocation-relevant state change emits one event over the Anchor
//! `#[event_cpi]` self-CPI transport (inner instructions survive RPC log truncation). Purely
//! informational, never load-bearing: no on-chain logic reads them, and the account-level
//! invariants (pool totals, plan conservation, budget bounds) remain the source of truth.
//!
//! Conventions:
//! - Lifecycle-ish ops share one event shape with an `op: u8` tag (`PREF_OP_*` /
//!   `DEPOSIT_KIND_*` constants below) — smaller IDL surface, single decode path for indexers.
//! - Amounts are native units (lamports, or fuSOL base units where noted).

use anchor_lang::prelude::*;

// ---- PoolDeposit kind tags ----------------------------------------------------------------
pub const DEPOSIT_KIND_SOL: u8 = 0;
pub const DEPOSIT_KIND_STAKE: u8 = 1;

// ---- PreferenceUpdated op tags ------------------------------------------------------------
pub const PREF_OP_SET: u8 = 0;
pub const PREF_OP_SYNCED: u8 = 1;
pub const PREF_OP_COUNTED: u8 = 2;
pub const PREF_OP_CLOSED: u8 = 3;

// ---- MaintenanceRewardPaid task-class tags -------------------------------------------------
pub const TASK_RECONCILE_BATCH: u8 = 0;
pub const TASK_FINALIZE_POOL: u8 = 1;
pub const TASK_PLAN_BATCH: u8 = 2;
pub const TASK_REBALANCE_ACTION: u8 = 3;
pub const TASK_FINISH_EPOCH: u8 = 4;

// ---- RebalanceActionExecuted action tags ---------------------------------------------------
// Real actions reuse the `spl_cpi` discriminant of the CPI performed (add = 1, remove = 2,
// increase = 4, set-preferred = 5, decrease-with-reserve = 21). A visit that deterministically
// resolved to "no move" (hysteresis, live transient, budget, minimum-action floor) emits this
// sentinel so the full rebalance walk is observable off-chain.
pub const ACTION_SKIP: u8 = u8::MAX;

/// One-time: `initialize_controller` recorded the immutable address set.
#[event]
pub struct ControllerInitialized {
    pub stake_pool: Pubkey,
    pub validator_list: Pubkey,
    pub reserve_stake: Pubkey,
    pub fusol_mint: Pubkey,
    pub pool_withdraw_authority: Pubkey,
    pub maintenance_vault: Pubkey,
    pub fusd_core_program: Pubkey,
}

/// One-time: `initialize_pool` CPI'd the stake-pool `Initialize` and sealed the controller.
#[event]
pub struct PoolInitialized {
    pub stake_pool: Pubkey,
    pub fusol_mint: Pubkey,
    pub max_validators: u32,
}

/// A `ValidatorRecord` was created for a vote account (registration, NOT admission).
#[event]
pub struct ValidatorRegistered {
    pub vote_account: Pubkey,
    pub payer: Pubkey,
}

/// A validator's lifecycle status changed (admission, promotion, draining, removal).
#[event]
pub struct ValidatorStatusChanged {
    pub vote_account: Pubkey,
    /// `fusion_stake_math::lifecycle::ValidatorStatus` bytes.
    pub old_status: u8,
    pub new_status: u8,
    pub epoch: u64,
}

/// A deposit flowed through the controller's deposit authority (`op` = `DEPOSIT_KIND_*`).
#[event]
pub struct PoolDeposit {
    pub depositor: Pubkey,
    pub kind: u8,
    /// The deposited stake account's voter for stake deposits; `Pubkey::default()` for SOL.
    pub vote_account: Pubkey,
    /// Lamports entering pool accounting (SOL amount, or the full absorbed stake account).
    pub lamports: u64,
}

/// A Preference lifecycle op (`op` = `PREF_OP_*`): set / synced / counted / closed.
#[event]
pub struct PreferenceUpdated {
    pub fusion_position: Pubkey,
    pub owner: Pubkey,
    pub vote_account: Pubkey,
    pub op: u8,
    /// Position ink observed (the directed weight a COUNTED snapshot added).
    pub observed_ink: u64,
    pub observed_ink_nonce: u64,
    pub eligible_from_epoch: u64,
    pub epoch: u64,
}

/// The crank state machine moved phases (`EpochState::PHASE_*` bytes).
#[event]
pub struct EpochPhaseChanged {
    pub epoch: u64,
    pub from_phase: u8,
    pub to_phase: u8,
    pub slot: u64,
}

/// PLAN-FINALIZE committed the epoch's final targets (conservation:
/// `Σ final targets + capacity_shortfall == productive_lamports`).
#[event]
pub struct PlanFinalized {
    pub epoch: u64,
    pub productive_lamports: u64,
    pub reserve_target: u64,
    pub total_directed_shares: u64,
    pub neutral_total: u64,
    /// Neutral lamports NO Active capacity could absorb (temporarily reserve-held).
    pub capacity_shortfall: u64,
    pub churn_budget: u64,
}

/// One deterministic rebalance action executed (`action` = the `spl_cpi` discriminant of the
/// CPI performed: add / remove / increase / decrease-with-reserve / set-preferred-withdraw).
#[event]
pub struct RebalanceActionExecuted {
    pub epoch: u64,
    pub action: u8,
    pub vote_account: Pubkey,
    pub lamports: u64,
}

/// A bounded crank reward left the maintenance vault (fuSOL base units).
#[event]
pub struct MaintenanceRewardPaid {
    /// The recipient fuSOL token account.
    pub crank: Pubkey,
    /// The task-class tag (`TASK_*` above — the instruction class that earned it).
    pub task: u8,
    pub amount: u64,
    pub epoch: u64,
}

/// Pool finalization observed a LOWER exchange rate than the previous snapshot. Committed
/// immediately (never smoothed) so Fusion's collateral oracle recognizes the loss at once.
#[event]
pub struct NegativeNavObserved {
    pub epoch: u64,
    pub previous_total_lamports: u64,
    pub new_total_lamports: u64,
    pub fusol_supply: u64,
}
