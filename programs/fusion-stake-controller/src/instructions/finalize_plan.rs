//! Anyone: PLAN-FINALIZE — prove conservation, record the aggregate-capacity shortfall, set
//! the deterministic preferred-withdraw validator, and enter REBALANCE.
//!
//! The conservation proof is recomputed from the controller's OWN accumulators — never a
//! caller value: `sum_directed_targets` (checked-summed record-by-record in PLAN-DIRECTED)
//! plus `neutral_granted_total` (checked-summed grant-by-grant in PLAN-NEUTRAL) plus the
//! recorded `capacity_shortfall` must equal the finalized `productive_lamports` EXACTLY. A
//! mismatch aborts the plan (fail closed — no rebalancing on unproven targets; the epoch
//! preemption is the recovery path).
//!
//! The preferred WITHDRAW validator (the deterministic drain-first source that stops
//! withdrawal cherry-picking and converges allocations) is the validator with the greatest
//! positive `observed active − final target` surplus over the last full plan walk — folded
//! per-record during the final capacity round (PLAN-DIRECTED seeds the zero-round case), ties
//! resolved to the first in canonical order. No surplus ⇒ the preference is UNSET (None).
//! Only the Withdraw variant is ever constructed (`spl_cpi` exposes no Deposit builder).
//!
//! Reward: `CRANK_REWARD_PLAN_BATCH` — the phase gate makes it once per epoch.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_spl::token::{Token, TokenAccount};

use crate::constants::{
    CONTROLLER_SEED, EPOCH_STATE_SEED, FUSION_STAKE_POOL_PROGRAM_ID, MAINTENANCE_AUTHORITY_SEED,
    POOL_AUTHORITY_SEED,
};
use crate::errors::ControllerError;
use crate::logic::phase_transition_allowed;
use crate::spl_cpi;
use crate::state::{ControllerConfig, EpochState, PHASE_PLAN_FINALIZE, PHASE_REBALANCE};

#[event_cpi]
#[derive(Accounts)]
pub struct FinalizePlan<'info> {
    #[account(
        seeds = [CONTROLLER_SEED],
        bump = config.bump,
        constraint = config.sealed @ ControllerError::PoolNotInitialized,
    )]
    pub config: Box<Account<'info, ControllerConfig>>,

    #[account(mut, seeds = [EPOCH_STATE_SEED], bump)]
    pub epoch_state: AccountLoader<'info, EpochState>,

    /// CHECK: writable — `SetPreferredValidator` records the preference on the pool.
    #[account(
        mut,
        address = config.stake_pool @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub stake_pool: UncheckedAccount<'info>,

    /// CHECK: `[b"pool_authority"]` PDA — the staker; signs the preferred-validator CPI.
    #[account(seeds = [POOL_AUTHORITY_SEED], bump = config.pool_authority_bump)]
    pub pool_authority: UncheckedAccount<'info>,

    /// CHECK: the recorded validator list (upstream verifies the preferred vote is an Active
    /// list member).
    #[account(
        address = config.validator_list @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub validator_list: UncheckedAccount<'info>,

    /// CHECK: the pinned stake-pool FORK program.
    #[account(address = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch)]
    pub stake_pool_program: UncheckedAccount<'info>,

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
}

pub fn handler(ctx: Context<FinalizePlan>) -> Result<()> {
    let clock = Clock::get()?;

    // Phase gate + the conservation proof over the controller's own accumulators.
    let (controller_epoch, plan, preferred);
    {
        let es = ctx.accounts.epoch_state.load()?;
        require!(es.phase == PHASE_PLAN_FINALIZE, ControllerError::WrongPhase);
        controller_epoch = es.controller_epoch;

        let assigned = es
            .sum_directed_targets
            .checked_add(es.neutral_granted_total)
            .and_then(|v| v.checked_add(es.capacity_shortfall))
            .ok_or(ControllerError::MathOverflow)?;
        require!(
            assigned == es.productive_lamports,
            ControllerError::PlanConservationViolated
        );

        plan = (
            es.productive_lamports,
            es.reserve_target,
            es.total_directed_shares,
            // The ORIGINAL neutral pool: productive minus the directed floors.
            es.productive_lamports.saturating_sub(es.sum_directed_targets),
            es.capacity_shortfall,
            es.churn_budget_total,
        );
        preferred = if es.preferred_withdraw_surplus > 0 {
            Some(Pubkey::new_from_array(es.preferred_withdraw_vote))
        } else {
            None
        };
    }

    // Deterministic drain-first withdrawal source (or an explicit unset).
    let ix = spl_cpi::set_preferred_withdraw_validator(
        &ctx.accounts.stake_pool.key(),
        &ctx.accounts.pool_authority.key(),
        &ctx.accounts.validator_list.key(),
        preferred.as_ref(),
    );
    invoke_signed(
        &ix,
        &[
            ctx.accounts.stake_pool.to_account_info(),
            ctx.accounts.pool_authority.to_account_info(),
            ctx.accounts.validator_list.to_account_info(),
            ctx.accounts.stake_pool_program.to_account_info(),
        ],
        &[&[POOL_AUTHORITY_SEED, &[ctx.accounts.config.pool_authority_bump]]],
    )?;

    let paid;
    {
        let mut es = ctx.accounts.epoch_state.load_mut()?;
        require!(
            phase_transition_allowed(es.phase, PHASE_REBALANCE),
            ControllerError::WrongPhase
        );
        es.phase = PHASE_REBALANCE;
        es.rebalance_cursor = 0;
        es.rebalance_actions_done = 0;

        paid = crate::maintenance::pay_crank_reward(
            &ctx.accounts.token_program,
            &mut ctx.accounts.maintenance_vault,
            &ctx.accounts.maintenance_authority,
            &ctx.accounts.crank_reward_account,
            ctx.accounts.config.maintenance_authority_bump,
            crate::constants::CRANK_REWARD_PLAN_BATCH,
            &mut es.epoch_payout_budget_used,
        )?;
    }

    emit_cpi!(crate::events::PlanFinalized {
        epoch: controller_epoch,
        productive_lamports: plan.0,
        reserve_target: plan.1,
        total_directed_shares: plan.2,
        neutral_total: plan.3,
        capacity_shortfall: plan.4,
        churn_budget: plan.5,
    });
    emit_cpi!(crate::events::EpochPhaseChanged {
        epoch: controller_epoch,
        from_phase: PHASE_PLAN_FINALIZE,
        to_phase: PHASE_REBALANCE,
        slot: clock.slot,
    });
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
