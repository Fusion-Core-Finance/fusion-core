use anchor_lang::prelude::*;

/// Per-validator controller state. PDA `[b"validator", vote_account]`, created permissionlessly
/// by `register_validator` (registration is NOT admission — admission to the stake-pool list is
/// an explicit plan outcome requiring eligibility + minimum directed support + list capacity).
///
/// Balance fields (`last_active_lamports` / `last_transient_lamports`) are OBSERVATIONS of the
/// canonical validator-list entry recorded at reconcile time — the controller never maintains a
/// competing balance; the stake-pool list stays the source of truth.
#[account]
#[derive(Debug)]
pub struct ValidatorRecord {
    /// Layout version byte (1).
    pub version: u8,
    /// The validator's vote account (also a PDA seed).
    pub vote_account: Pubkey,
    /// Index of this validator's entry in the stake-pool `ValidatorList`;
    /// `VALIDATOR_LIST_INDEX_UNSET` (u32::MAX) until admitted.
    pub validator_list_index: u32,
    /// Lifecycle status (`fusion_stake_math::lifecycle::ValidatorStatus` byte; parse with
    /// `ValidatorStatus::from_u8`, which fails closed on corruption).
    pub status: u8,
    /// Consecutive failed completed epochs of liveness BEFORE the current plan pass
    /// (`LifecycleInput::consecutive_failures`).
    pub consecutive_liveness_failures: u8,
    /// Consecutive healthy-with-pool-stake completed epochs (`LifecycleInput::consecutive_healthy`
    /// — the Candidate promotion streak).
    pub consecutive_healthy_epochs: u8,
    /// Validator carried pool stake through the last completed epoch.
    pub has_pool_stake: bool,

    /// Last observed canonical active stake (validator-list entry, at reconcile).
    pub last_active_lamports: u64,
    /// Last observed canonical transient stake.
    pub last_transient_lamports: u64,

    /// Epoch stamp for `directed_shares`: shares are valid only when this equals the epoch
    /// being planned (a stale stamp reads as zero directed weight — no clearing pass needed).
    pub directed_shares_epoch: u64,
    /// Eligible directed fuSOL shares counted for `directed_shares_epoch` (checked-summed
    /// into `EpochState.total_directed_shares` during PLAN-DIRECTED).
    pub directed_shares: u64,

    /// This epoch's directed target floor (`min(floor(P·d/S), lifecycle_cap)`).
    pub directed_target: u64,
    /// Neutral lamports granted by capacity rounds this epoch.
    pub neutral_granted: u64,
    /// `directed_target + neutral_granted` once PLAN-FINALIZE commits.
    pub final_target: u64,
    /// Remaining neutral capacity (`lifecycle_cap - current target`), decremented as rounds
    /// grant — derived from the SAME snapshot the grants accumulate into.
    pub remaining_capacity: u64,

    /// The validator hit `remaining_capacity == 0` during round `saturated_round` (it leaves
    /// the unsaturated set for subsequent rounds). Valid only when `saturated_round` equals
    /// the current `EpochState.neutral_round_number` context.
    pub saturated_this_round: bool,
    /// The capacity round `saturated_this_round` refers to.
    pub saturated_round: u64,

    /// Last epoch a rebalance INCREASE executed for this validator.
    pub last_increase_epoch: u64,
    /// Last epoch a rebalance DECREASE executed for this validator.
    pub last_decrease_epoch: u64,
    /// Epoch this validator entered Draining (0 = never): the removal delay runs from here.
    pub removal_delay_start: u64,

    // --- reconcile-pass observations (valid only when `observed_epoch` is current) ----------
    /// Epoch stamp of the observations below (RECONCILE writes them; PLAN-DIRECTED requires
    /// the stamp to be current before advancing the lifecycle from them).
    pub observed_epoch: u64,
    /// Inflation commission was at or below the fixed cap at reconcile. An UNREADABLE vote
    /// account observes `true` here (and `false` for liveness): a systemic vote-state parse
    /// failure must surface as guard-suppressible liveness failure, never as a mass
    /// commission-drain that bypasses the global liveness guard.
    pub observed_commission_ok: bool,
    /// Passed BOTH liveness checks at reconcile (fresh landed vote AND positive prior-epoch
    /// credit growth). `false` freezes increases this epoch (single-failure rule).
    pub observed_liveness_ok: bool,

    /// Epoch stamp of the last PLAN-DIRECTED result (once-per-epoch plan idempotency; also
    /// certifies `validator_list_index` / targets / capacity as current-plan values).
    pub plan_epoch: u64,
    /// Upstream `StakeStatus` byte of this validator's list entry at plan time
    /// (`POOL_ENTRY_STATUS_NONE` when not in the pool). Gates preferred-withdraw candidacy.
    pub pool_entry_status: u8,

    pub bump: u8,
    /// Forward-compat reserve (carve from the HEAD).
    pub _reserved: [u8; 13],
}

impl ValidatorRecord {
    pub const SPACE: usize = 8 // discriminator
        + 1                    // version
        + 32                   // vote_account
        + 4                    // validator_list_index
        + 1 + 1 + 1 + 1        // status, liveness failures, healthy streak, has_pool_stake
        + 8 * 2                // last_active_lamports, last_transient_lamports
        + 8 * 2                // directed_shares_epoch, directed_shares
        + 8 * 4                // directed_target, neutral_granted, final_target, remaining_capacity
        + 1 + 8                // saturated_this_round, saturated_round
        + 8 * 3                // last_increase_epoch, last_decrease_epoch, removal_delay_start
        + 8 + 1 + 1            // observed_epoch, observed_commission_ok, observed_liveness_ok
        + 8 + 1                // plan_epoch, pool_entry_status
        + 1                    // bump
        + 13; // _reserved
}
