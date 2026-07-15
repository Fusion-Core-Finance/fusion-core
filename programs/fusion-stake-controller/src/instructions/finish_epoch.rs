//! Anyone: REBALANCE → IDLE when the epoch's work is provably done. Unfinished physical moves
//! carry forward through native transient/reserve mechanics; they never invalidate NAV or user
//! ownership.
//!
//! Completion is proved from `EpochState` aggregates alone (never a caller claim):
//! - the deterministic walk visited every planned slot (`rebalance_cursor == 2 × planned`),
//!   which structurally implies every remaining deviation was within hysteresis, blocked by a
//!   live transient, or unfundable this epoch (the walk skipped it after inspection); OR
//! - the remaining churn budget is below one minimum action (the RUNTIME effective minimum
//!   delegation, the same floor the actions themselves are sized by) — nothing can move
//!   anymore.
//!
//! Pending admission adds do not block finishing: they remain executable next epoch after
//! re-planning (fail-safe delay, no allocation effect this epoch).
//!
//! Reward: the finalization task class (`CRANK_REWARD_FINALIZE_POOL`) — closing the cycle is
//! what re-arms `start_epoch` and keeps the machine live.

use anchor_lang::prelude::*;
use anchor_spl::token::{Token, TokenAccount};

use crate::constants::{CONTROLLER_SEED, EPOCH_STATE_SEED, MAINTENANCE_AUTHORITY_SEED};
use crate::errors::ControllerError;
use crate::logic::phase_transition_allowed;
use crate::state::{ControllerConfig, EpochState, PHASE_IDLE, PHASE_REBALANCE};

#[event_cpi]
#[derive(Accounts)]
pub struct FinishEpoch<'info> {
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

    /// CHECK: the native stake program — CPI'd (read-only, no accounts) for the runtime
    /// `GetMinimumDelegation` the budget-exhausted condition compares against.
    #[account(address = crate::constants::STAKE_PROGRAM_ID)]
    pub stake_program: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<FinishEpoch>) -> Result<()> {
    let clock = Clock::get()?;
    let mut es = ctx.accounts.epoch_state.load_mut()?;
    require!(es.phase == PHASE_REBALANCE, ControllerError::WrongPhase);

    let walk_complete = es.rebalance_cursor >= es.plan_directed_cursor.saturating_mul(2);
    // The RUNTIME effective minimum (the actions' own floor): a smaller constant here would
    // deny finishing an epoch whose remaining budget can no longer fund any legal action.
    let min_delegation = crate::spl_cpi::effective_minimum_delegation()?;
    let budget_exhausted =
        es.churn_budget_total.saturating_sub(es.churn_budget_used) < min_delegation;
    require!(walk_complete || budget_exhausted, ControllerError::EpochNotFinished);

    require!(phase_transition_allowed(es.phase, PHASE_IDLE), ControllerError::WrongPhase);
    es.phase = PHASE_IDLE;
    let controller_epoch = es.controller_epoch;

    let paid = crate::maintenance::pay_crank_reward(
        &ctx.accounts.token_program,
        &mut ctx.accounts.maintenance_vault,
        &ctx.accounts.maintenance_authority,
        &ctx.accounts.crank_reward_account,
        ctx.accounts.config.maintenance_authority_bump,
        crate::constants::CRANK_REWARD_FINALIZE_POOL,
        &mut es.epoch_payout_budget_used,
    )?;
    drop(es);

    emit_cpi!(crate::events::EpochPhaseChanged {
        epoch: controller_epoch,
        from_phase: PHASE_REBALANCE,
        to_phase: PHASE_IDLE,
        slot: clock.slot,
    });
    if paid > 0 {
        emit_cpi!(crate::events::MaintenanceRewardPaid {
            crank: ctx.accounts.crank_reward_account.key(),
            task: crate::events::TASK_FINISH_EPOCH,
            amount: paid,
            epoch: controller_epoch,
        });
    }
    Ok(())
}
