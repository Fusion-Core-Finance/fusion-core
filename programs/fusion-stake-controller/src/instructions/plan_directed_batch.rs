//! Anyone: PLAN-DIRECTED — advance lifecycles from the completed reconcile snapshot, compute
//! directed targets, lifecycle caps and remaining Active capacity, and checked-accumulate the
//! plan aggregates for the next validator slice.
//!
//! Lifecycle advancement happens HERE (not at reconcile) because the global liveness guard
//! needs the COMPLETE healthy/total delegated aggregate — reconcile finishes it, this pass
//! consumes it, so every validator's lifecycle decision sees the same full-pool snapshot.
//!
//! `remaining_accounts` layout: `2N` accounts — `(validator_record_i [w], vote_account_i [])`
//! pairs. The FIRST pairs bind to consecutive validator-list indices starting at
//! `plan_directed_cursor` (the handler re-derives the binding from the live list bytes; wrong,
//! duplicate or out-of-order entries fail WITHOUT advancing). Pairs beyond the list length are
//! ADMISSION EXTRAS: records with `validator_list_index == UNSET` (Registered validators
//! seeking admission, or admitted-but-not-yet-added Candidates), evaluated once per epoch via
//! the `plan_epoch` stamp. Extras are permissionless and skippable — omitting one only delays
//! that validator's admission to a later epoch (fail-safe), while LIST coverage is total and
//! cursor-proved.
//!
//! Admission (Registered → Candidate) requires: healthy observation (commission + liveness)
//! AND a raw directed floor `floor(P·d/S)` of at least `MIN_ACTIVATION_TARGET_LAMPORTS`. The
//! RAW (uncapped) floor is used — the spec's "calculated target" — because the Registered cap
//! is 0 and the Candidate cap may sit below the activation minimum on a small pool; real
//! directed support is what admission measures. The physical `AddValidatorToPool` happens in
//! REBALANCE (list capacity is enforced upstream at that point).
//!
//! Records not in the pool NEVER expose neutral capacity here (they cannot physically receive
//! neutral stake, and the neutral walk covers list ordinals only), so the unsaturated count
//! stays exactly the neutral round membership.
//!
//! Completion (cursor == list length): evaluate the `D ≤ S` plan guard
//! (`fusion_stake_math::targets::neutral_total`; violation ABORTS the plan — fail closed, the
//! epoch preemption is the recovery path), derive `neutral_total`, transition to PLAN-NEUTRAL.
//! Reward: `CRANK_REWARD_PLAN_BATCH` iff at least one record received a current-epoch result.

use anchor_lang::prelude::*;
use anchor_spl::token::{Token, TokenAccount};

use crate::constants::{
    ACTIVE_VALIDATOR_CAP_BPS, CANDIDATE_CAP_BPS, COMMISSION_CAP_PERCENT, CONTROLLER_SEED,
    EPOCH_STATE_SEED, FUSION_STAKE_POOL_PROGRAM_ID, MAINTENANCE_AUTHORITY_SEED,
    MIN_ACTIVATION_TARGET_LAMPORTS, POOL_ENTRY_STATUS_NONE, REMOVAL_DELAY_EPOCHS,
    STAKE_ACCOUNT_SPACE, UPSTREAM_MINIMUM_DELEGATION, VALIDATOR_LIST_INDEX_UNSET,
    VOTE_FRESHNESS_WINDOW_DIVISOR, VOTE_PROGRAM_ID,
};
use crate::errors::ControllerError;
use crate::logic::{observe_vote, phase_transition_allowed, VoteObservation};
use crate::state::{
    ControllerConfig, EpochState, ValidatorRecord, PHASE_PLAN_DIRECTED, PHASE_PLAN_NEUTRAL,
};
use fusion_stake_math::lifecycle::{
    advance_lifecycle, global_liveness_guard, lifecycle_cap, LifecycleInput, ValidatorStatus,
};
use fusion_stake_math::targets::{directed_target, neutral_capacity, neutral_total};

#[event_cpi]
#[derive(Accounts)]
pub struct PlanDirectedBatch<'info> {
    #[account(
        seeds = [CONTROLLER_SEED],
        bump = config.bump,
        constraint = config.sealed @ ControllerError::PoolNotInitialized,
    )]
    pub config: Box<Account<'info, ControllerConfig>>,

    #[account(mut, seeds = [EPOCH_STATE_SEED], bump)]
    pub epoch_state: AccountLoader<'info, EpochState>,

    /// CHECK: read-only — binds records to post-cleanup list indices and reads entry statuses.
    /// All target math runs over the FINALIZED snapshot in `EpochState`, never a live total
    /// (deposits mid-epoch move the live pool total; the plan must not chase it).
    #[account(
        address = config.validator_list @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub validator_list: UncheckedAccount<'info>,

    /// The maintenance vault (crank reward source).
    #[account(mut, address = config.maintenance_vault @ ControllerError::AddressMismatch)]
    pub maintenance_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: `[b"maintenance"]` PDA — signs the bounded reward transfer.
    #[account(seeds = [MAINTENANCE_AUTHORITY_SEED], bump = config.maintenance_authority_bump)]
    pub maintenance_authority: UncheckedAccount<'info>,

    /// The caller's fuSOL account for the crank reward.
    #[account(mut, constraint = crank_reward_account.mint == config.fusol_mint @ ControllerError::AddressMismatch)]
    pub crank_reward_account: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    // remaining_accounts: 2N — (validator_record [w], vote_account []) pairs: list-slice pairs
    // from `plan_directed_cursor` first, then admission extras (UNSET-index records).
}

pub fn handler<'info>(ctx: Context<'_, '_, 'info, 'info, PlanDirectedBatch<'info>>) -> Result<()> {
    let clock = Clock::get()?;
    let stake_rent = Rent::get()?.minimum_balance(STAKE_ACCOUNT_SPACE);
    // "Drained": transient empty AND no legal decrease remains (active below the retention
    // floor plus one minimum-delegation move) — the whole-account Remove is then the only exit.
    let drained_threshold = stake_rent
        .checked_add(UPSTREAM_MINIMUM_DELEGATION.saturating_mul(2))
        .ok_or(ControllerError::MathOverflow)?;
    let freshness_window =
        EpochSchedule::get()?.get_slots_in_epoch(clock.epoch) / VOTE_FRESHNESS_WINDOW_DIVISOR;

    // `% 2 == 0` (not `.is_multiple_of(2)`): the SBF toolchain (platform-tools v1.48,
    // cargo 1.84) predates `is_multiple_of` — same note as the EpochState size assert.
    #[allow(clippy::manual_is_multiple_of)]
    {
        require!(
            ctx.remaining_accounts.len() % 2 == 0,
            ControllerError::InvalidRemainingAccounts
        );
    }
    let pairs = ctx.remaining_accounts.len() / 2;

    let mut es = ctx.accounts.epoch_state.load_mut()?;
    require!(es.phase == PHASE_PLAN_DIRECTED, ControllerError::WrongPhase);
    let controller_epoch = es.controller_epoch;
    let guard_active =
        global_liveness_guard(es.healthy_delegated_lamports, es.total_delegated_lamports);
    let productive = es.productive_lamports;
    let supply = es.nav_fusol_supply;
    let snapshot_total = es.nav_total_lamports;

    let list_data = ctx.accounts.validator_list.try_borrow_data()?;
    let list_len = u64::from(
        fusion_stake_view::validator_list::parse_header(&list_data)
            .map_err(|_| error!(ControllerError::InvalidValidatorListEntry))?
            .len,
    );
    require!(
        pairs > 0 || es.plan_directed_cursor == list_len,
        ControllerError::InvalidRemainingAccounts
    );

    let mut status_changes: Vec<(Pubkey, u8, u8)> = Vec::new();
    let mut processed = 0u64;

    for p in 0..pairs {
        let record_info = &ctx.remaining_accounts[2 * p];
        let vote_info = &ctx.remaining_accounts[2 * p + 1];
        require!(record_info.is_writable, ControllerError::InvalidRemainingAccounts);
        let mut record = Account::<ValidatorRecord>::try_from(record_info)?;
        require!(
            record.vote_account == vote_info.key(),
            ControllerError::InvalidRemainingAccounts
        );
        require!(record.plan_epoch != controller_epoch, ControllerError::RecordAlreadyPlanned);

        let in_list = es.plan_directed_cursor < list_len;
        let (observation, entry_status) = if in_list {
            // Cursor slice: the pair MUST be the record of the entry at the cursor index.
            let entry = fusion_stake_view::validator_list::entry_at(
                &list_data,
                es.plan_directed_cursor as u32,
            )
            .ok_or(ControllerError::InvalidValidatorListEntry)?;
            require!(
                entry.vote_account_address == vote_info.key().to_bytes(),
                ControllerError::InvalidRemainingAccounts
            );
            // In-pool records must carry THIS epoch's reconcile observations.
            require!(
                record.observed_epoch == controller_epoch,
                ControllerError::StaleValidatorRecord
            );
            // Re-stamp the index cache post-cleanup: this stamp (certified by `plan_epoch`)
            // is what PLAN-NEUTRAL and REBALANCE bind against.
            record.validator_list_index = es.plan_directed_cursor as u32;
            es.plan_directed_cursor += 1;
            (
                VoteObservation {
                    commission_ok: record.observed_commission_ok,
                    liveness_ok: record.observed_liveness_ok,
                },
                entry.status,
            )
        } else {
            // Admission extra: not in the pool; observe its vote account NOW (reconcile never
            // covers it — it carries no delegated stake, so the guard aggregate is unaffected).
            require!(
                record.validator_list_index == VALIDATOR_LIST_INDEX_UNSET,
                ControllerError::InvalidRemainingAccounts
            );
            let sample = if *vote_info.owner == VOTE_PROGRAM_ID {
                let data = vote_info.try_borrow_data()?;
                fusion_stake_view::vote_state::parse(&data, controller_epoch.saturating_sub(1))
                    .ok()
            } else {
                None
            };
            let obs =
                observe_vote(sample, COMMISSION_CAP_PERCENT, clock.slot, freshness_window);
            record.observed_epoch = controller_epoch;
            record.observed_commission_ok = obs.commission_ok;
            record.observed_liveness_ok = obs.liveness_ok;
            record.has_pool_stake = false;
            (obs, POOL_ENTRY_STATUS_NONE)
        };

        let old_status = ValidatorStatus::from_u8(record.status)
            .ok_or(ControllerError::CorruptValidatorStatus)?;

        // Directed shares are valid ONLY with a current epoch stamp (stale = zero direction).
        let shares = if record.directed_shares_epoch == controller_epoch {
            record.directed_shares
        } else {
            0
        };

        // 1) Lifecycle advancement (one completed epoch, full-pool-consistent guard).
        let outcome = advance_lifecycle(
            old_status,
            &LifecycleInput {
                commission_ok: observation.commission_ok,
                liveness_ok: observation.liveness_ok,
                consecutive_failures: record.consecutive_liveness_failures,
                consecutive_healthy: record.consecutive_healthy_epochs,
                has_pool_stake: record.has_pool_stake,
                guard_active,
                zero_stake_and_target: record.last_transient_lamports == 0
                    && record.last_active_lamports < drained_threshold,
                removal_delay_elapsed: record.removal_delay_start > 0
                    && controller_epoch
                        >= record.removal_delay_start.saturating_add(REMOVAL_DELAY_EPOCHS),
            },
        );
        record.consecutive_liveness_failures = outcome.consecutive_failures;
        record.consecutive_healthy_epochs = outcome.consecutive_healthy;
        let mut new_status = outcome.status;

        // 2) Admission (an explicit program action, not a lifecycle transition): objective
        // eligibility + real directed support at the activation minimum.
        if new_status == ValidatorStatus::Registered
            && observation.commission_ok
            && observation.liveness_ok
            && supply > 0
        {
            let raw = u128::from(productive) * u128::from(shares) / u128::from(supply);
            if raw >= u128::from(MIN_ACTIVATION_TARGET_LAMPORTS) {
                new_status = ValidatorStatus::Candidate;
            }
        }

        if new_status != old_status {
            if new_status == ValidatorStatus::Draining {
                record.removal_delay_start = controller_epoch;
            }
            status_changes.push((record.vote_account, old_status.as_u8(), new_status.as_u8()));
        }
        record.status = new_status.as_u8();
        record.pool_entry_status = entry_status;

        // 3) Targets + capacity from the SAME finalized snapshot the grants will fold into.
        let cap = lifecycle_cap(
            new_status,
            snapshot_total,
            CANDIDATE_CAP_BPS,
            ACTIVE_VALIDATOR_CAP_BPS,
        );
        let target = directed_target(productive, shares, supply, cap);
        record.directed_target = target;
        record.neutral_granted = 0;
        record.final_target = target;
        // Out-of-pool records expose ZERO neutral capacity regardless of status: the neutral
        // walk covers list ordinals only, and stake cannot physically reach them this epoch.
        record.remaining_capacity =
            if in_list { neutral_capacity(new_status, cap, target) } else { 0 };
        record.saturated_this_round = false;
        record.saturated_round = 0;
        record.plan_epoch = controller_epoch;

        // 4) Checked plan aggregates (the D ≤ S guard is only as strong as this summation).
        es.total_directed_shares = es
            .total_directed_shares
            .checked_add(shares)
            .ok_or(ControllerError::MathOverflow)?;
        es.sum_directed_targets = es
            .sum_directed_targets
            .checked_add(target)
            .ok_or(ControllerError::MathOverflow)?;
        if record.remaining_capacity > 0 {
            es.unsaturated_active_count += 1;
        }
        // Preferred-withdraw fold (only meaningful for upstream-Active entries; each neutral
        // round re-folds, this seeds the zero-round case).
        if entry_status == 0 {
            let surplus = record.last_active_lamports.saturating_sub(record.final_target);
            let vote_bytes = record.vote_account.to_bytes();
            es.fold_preferred_withdraw(&vote_bytes, surplus);
        }

        record.exit(&crate::ID)?;
        processed += 1;
    }
    drop(list_data);

    // Completion: full list coverage proved by the cursor. Derive the neutral pool with the
    // plan-level guard built in.
    let completed = es.plan_directed_cursor == list_len;
    if completed {
        let neutral = neutral_total(
            productive,
            es.sum_directed_targets,
            es.total_directed_shares,
            supply,
        )
        .ok_or(ControllerError::DirectedSharesExceedSupply)?;
        es.neutral_total = neutral;
        require!(
            phase_transition_allowed(es.phase, PHASE_PLAN_NEUTRAL),
            ControllerError::WrongPhase
        );
        es.phase = PHASE_PLAN_NEUTRAL;
    }

    let paid = if processed > 0 {
        crate::maintenance::pay_crank_reward(
            &ctx.accounts.token_program,
            &mut ctx.accounts.maintenance_vault,
            &ctx.accounts.maintenance_authority,
            &ctx.accounts.crank_reward_account,
            ctx.accounts.config.maintenance_authority_bump,
            crate::constants::CRANK_REWARD_PLAN_BATCH,
            &mut es.epoch_payout_budget_used,
        )?
    } else {
        0
    };
    drop(es);

    for (vote, old, new) in status_changes {
        emit_cpi!(crate::events::ValidatorStatusChanged {
            vote_account: vote,
            old_status: old,
            new_status: new,
            epoch: controller_epoch,
        });
    }
    if completed {
        emit_cpi!(crate::events::EpochPhaseChanged {
            epoch: controller_epoch,
            from_phase: PHASE_PLAN_DIRECTED,
            to_phase: PHASE_PLAN_NEUTRAL,
            slot: clock.slot,
        });
    }
    if paid > 0 {
        emit_cpi!(crate::events::MaintenanceRewardPaid {
            crank: ctx.accounts.crank_reward_account.key(),
            task: crate::events::TASK_PLAN_BATCH,
            amount: paid,
            epoch: controller_epoch,
        });
    }
    Ok(())
}
