//! Anyone: PLAN-NEUTRAL — fold one bounded slice of the current deterministic capacity round
//! (`fusion_stake_math::targets::begin_round`/`step` via the `EpochState` mirror fields).
//!
//! **Round membership is structural, not caller-claimed:** every round walks EVERY planned
//! list ordinal in canonical order via `neutral_cursor`. The i-th passed record must carry
//! `plan_epoch == controller_epoch` and `validator_list_index == neutral_cursor` (the stamp
//! PLAN-DIRECTED certified against the live list), so a caller can neither omit, reorder, nor
//! duplicate a member — omission of a qualifying validator would shift every later binding and
//! fail. Non-members (non-Active, zero remaining capacity, or saturated in an earlier round)
//! are visited and skipped; members are `step`ped exactly once per round with their
//! remaining capacity, grants applied with CHECKED adds onto `neutral_granted` / `final_target`
//! and checked-summed into `EpochState.neutral_granted_total` (the conservation proof's
//! independent leg).
//!
//! Round end (cursor == planned length): if lamports remain and unsaturated Actives remain,
//! the next round begins (each completed non-final round saturates at least one validator, so
//! rounds terminate within `MAX_NEUTRAL_ROUNDS`); otherwise the remainder is recorded as the
//! aggregate-capacity shortfall and the phase moves to PLAN-FINALIZE.
//!
//! The preferred-withdraw argmax is re-folded over every visited record each round (reset at
//! round start), so the LAST completed round's fold reflects final targets exactly;
//! PLAN-DIRECTED seeds the zero-round case.
//!
//! `remaining_accounts` layout: N writable `ValidatorRecord`s for consecutive planned list
//! ordinals starting at `neutral_cursor`.
//!
//! Reward: `CRANK_REWARD_PLAN_BATCH` iff at least one record was visited.

use anchor_lang::prelude::*;
use anchor_spl::token::{Token, TokenAccount};

use crate::constants::{CONTROLLER_SEED, EPOCH_STATE_SEED, MAINTENANCE_AUTHORITY_SEED};
use crate::errors::ControllerError;
use crate::logic::phase_transition_allowed;
use crate::state::{
    ControllerConfig, EpochState, ValidatorRecord, PHASE_PLAN_FINALIZE, PHASE_PLAN_NEUTRAL,
};
use fusion_stake_math::lifecycle::ValidatorStatus;
use fusion_stake_math::targets::{begin_round, step, MAX_NEUTRAL_ROUNDS};

#[event_cpi]
#[derive(Accounts)]
pub struct PlanNeutralBatch<'info> {
    #[account(
        seeds = [CONTROLLER_SEED],
        bump = config.bump,
        constraint = config.sealed @ ControllerError::PoolNotInitialized,
    )]
    pub config: Box<Account<'info, ControllerConfig>>,

    #[account(mut, seeds = [EPOCH_STATE_SEED], bump)]
    pub epoch_state: AccountLoader<'info, EpochState>,

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
    // remaining_accounts: N writable ValidatorRecords — consecutive planned list ordinals
    // starting at `neutral_cursor`.
}

pub fn handler<'info>(ctx: Context<'_, '_, 'info, 'info, PlanNeutralBatch<'info>>) -> Result<()> {
    let clock = Clock::get()?;
    let mut es = ctx.accounts.epoch_state.load_mut()?;
    require!(es.phase == PHASE_PLAN_NEUTRAL, ControllerError::WrongPhase);
    let controller_epoch = es.controller_epoch;
    // PLAN-DIRECTED's completed cursor IS the planned list length for the rest of the epoch.
    let planned_len = es.plan_directed_cursor;

    let mut finished = false; // set when the phase completes this call

    // Lazily open round 1 (`neutral_round_number == 0` marks "no round opened this phase").
    if es.neutral_round_number == 0 {
        match begin_round(es.neutral_total, es.unsaturated_active_count, controller_epoch) {
            None => {
                // Nothing to distribute or nobody to take it: the remainder (possibly 0) is
                // the aggregate-capacity shortfall, recorded explicitly.
                es.capacity_shortfall = es.neutral_total;
                finished = true;
            }
            Some(round) => {
                es.set_neutral_round(&round);
                es.neutral_round_number = 1;
                es.neutral_cursor = 0;
                // Each round re-folds the preferred-withdraw argmax over final targets.
                es.preferred_withdraw_surplus = 0;
                es.preferred_withdraw_vote = [0u8; 32];
            }
        }
    }

    let mut visited = 0u64;
    if !finished {
        let n = ctx.remaining_accounts.len() as u64;
        require!(
            es.neutral_cursor
                .checked_add(n)
                .ok_or(ControllerError::MathOverflow)?
                <= planned_len,
            ControllerError::InvalidRemainingAccounts
        );
        // An empty batch is only ever the phase-entry transition call handled above.
        require!(n > 0 || planned_len == 0, ControllerError::InvalidRemainingAccounts);

        let mut round = es.neutral_round();
        for record_info in ctx.remaining_accounts {
            require!(record_info.is_writable, ControllerError::InvalidRemainingAccounts);
            let mut record = Account::<ValidatorRecord>::try_from(record_info)?;
            // Structural binding: current-plan record at exactly the cursor ordinal.
            require!(
                record.plan_epoch == controller_epoch
                    && u64::from(record.validator_list_index) == es.neutral_cursor,
                ControllerError::InvalidRemainingAccounts
            );

            let status = ValidatorStatus::from_u8(record.status)
                .ok_or(ControllerError::CorruptValidatorStatus)?;
            let is_member = status == ValidatorStatus::Active && record.remaining_capacity > 0;
            if is_member {
                let grant = step(&mut round, record.remaining_capacity);
                if grant > 0 {
                    record.neutral_granted = record
                        .neutral_granted
                        .checked_add(grant)
                        .ok_or(ControllerError::MathOverflow)?;
                    record.final_target = record
                        .final_target
                        .checked_add(grant)
                        .ok_or(ControllerError::MathOverflow)?;
                    record.remaining_capacity = record
                        .remaining_capacity
                        .checked_sub(grant)
                        .ok_or(ControllerError::MathOverflow)?;
                    es.neutral_granted_total = es
                        .neutral_granted_total
                        .checked_add(grant)
                        .ok_or(ControllerError::MathOverflow)?;
                    if record.remaining_capacity == 0 {
                        record.saturated_this_round = true;
                        record.saturated_round = es.neutral_round_number;
                    }
                }
            }
            // Preferred-withdraw fold over EVERY visited in-pool-Active record (member or
            // not) — a full round therefore folds the complete planned set.
            if record.pool_entry_status == 0 {
                let surplus = record.last_active_lamports.saturating_sub(record.final_target);
                let vote_bytes = record.vote_account.to_bytes();
                es.fold_preferred_withdraw(&vote_bytes, surplus);
            }

            record.exit(&crate::ID)?;
            es.neutral_cursor += 1;
            visited += 1;
        }
        es.set_neutral_round(&round);

        // Round completion: every planned ordinal visited exactly once.
        if es.neutral_cursor == planned_len {
            // The members stepped must be exactly the round's unsaturated set.
            require!(round.is_complete(), ControllerError::NeutralRoundInconsistent);
            let remaining = round.remaining_after();
            es.neutral_total = remaining;
            let next_unsaturated = round
                .n_unsaturated
                .checked_sub(round.saturated)
                .ok_or(ControllerError::NeutralRoundInconsistent)?;
            es.unsaturated_active_count = next_unsaturated;

            if remaining == 0 || next_unsaturated == 0 {
                es.capacity_shortfall = remaining;
                finished = true;
            } else {
                // Structurally bounded: every completed non-final round saturated >= 1.
                require!(
                    es.neutral_round_number < MAX_NEUTRAL_ROUNDS,
                    ControllerError::NeutralRoundInconsistent
                );
                let next = begin_round(remaining, next_unsaturated, controller_epoch)
                    .ok_or(ControllerError::NeutralRoundInconsistent)?;
                es.set_neutral_round(&next);
                es.neutral_round_number += 1;
                es.neutral_cursor = 0;
                es.preferred_withdraw_surplus = 0;
                es.preferred_withdraw_vote = [0u8; 32];
            }
        }
    }

    if finished {
        require!(
            phase_transition_allowed(es.phase, PHASE_PLAN_FINALIZE),
            ControllerError::WrongPhase
        );
        es.phase = PHASE_PLAN_FINALIZE;
    }

    let paid = if visited > 0 {
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

    if finished {
        emit_cpi!(crate::events::EpochPhaseChanged {
            epoch: controller_epoch,
            from_phase: PHASE_PLAN_NEUTRAL,
            to_phase: PHASE_PLAN_FINALIZE,
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
