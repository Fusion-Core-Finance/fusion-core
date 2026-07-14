//! Anyone: FINALIZE — CPI `UpdateStakePoolBalance` (canonical totals + the epoch maintenance
//! fee mint) then `CleanupRemovedValidatorEntries`, snapshot NAV/supply, and open the
//! preference window.
//!
//! Completion proof is upstream's: `UpdateStakePoolBalance` fails `StakeListOutOfDate` unless
//! EVERY list entry was stamped current by reconcile. The controller never estimates balances
//! — it snapshots the canonical totals the upstream program just computed.
//!
//! Negative NAV is committed IMMEDIATELY (never smoothed): a strictly lower exchange rate than
//! the previous finalized snapshot emits `NegativeNavObserved` so Fusion's collateral oracle
//! recognizes the loss at once. The epoch maintenance fee on non-positive rewards is zero by
//! upstream `Fee::apply` semantics.
//!
//! Reward: `CRANK_REWARD_FINALIZE_POOL` iff this call advanced the canonical snapshot to the
//! new epoch (someone running the permissionless upstream update out-of-band forfeits the
//! reward but never blocks the phase — this instruction still performs the controller-side
//! snapshot + window opening).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke;
use anchor_spl::token::{Mint, Token, TokenAccount};

use crate::constants::{
    CONTROLLER_SEED, EPOCH_STATE_SEED, FUSION_STAKE_POOL_PROGRAM_ID, GLOBAL_CHURN_CAP_BPS,
    MAINTENANCE_AUTHORITY_SEED, PREFERENCE_WINDOW_SLOT_DIVISOR, RESERVE_MINIMUM_LAMPORTS,
    RESERVE_TARGET_BPS,
};
use crate::errors::ControllerError;
use crate::logic::{nav_rate_decreased, phase_transition_allowed};
use crate::spl_cpi;
use crate::state::{ControllerConfig, EpochState, PHASE_FINALIZE, PHASE_PREFERENCES};
use fusion_stake_math::churn::global_churn_budget;
use fusion_stake_math::reserve::{productive_lamports, reserve_target};

#[event_cpi]
#[derive(Accounts)]
pub struct FinalizePool<'info> {
    #[account(
        seeds = [CONTROLLER_SEED],
        bump = config.bump,
        constraint = config.sealed @ ControllerError::PoolNotInitialized,
    )]
    pub config: Box<Account<'info, ControllerConfig>>,

    #[account(mut, seeds = [EPOCH_STATE_SEED], bump)]
    pub epoch_state: AccountLoader<'info, EpochState>,

    /// CHECK: writable — `UpdateStakePoolBalance` stamps totals; `Cleanup` may reset a
    /// dangling preferred validator.
    #[account(
        mut,
        address = config.stake_pool @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub stake_pool: UncheckedAccount<'info>,

    /// CHECK: the recorded stake-pool withdraw authority (mints the epoch fee).
    #[account(address = config.pool_withdraw_authority @ ControllerError::AddressMismatch)]
    pub pool_withdraw_authority: UncheckedAccount<'info>,

    /// CHECK: writable — cleanup drops ReadyForRemoval entries.
    #[account(
        mut,
        address = config.validator_list @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub validator_list: UncheckedAccount<'info>,

    /// CHECK: read-only in `UpdateStakePoolBalance`.
    #[account(address = config.reserve_stake @ ControllerError::AddressMismatch)]
    pub reserve_stake: UncheckedAccount<'info>,

    /// The pool mint (epoch fee is minted).
    #[account(mut, address = config.fusol_mint @ ControllerError::AddressMismatch)]
    pub fusol_mint: Box<Account<'info, Mint>>,

    /// The maintenance vault — receives the epoch fee (as the manager fee account) AND funds
    /// the crank reward.
    #[account(mut, address = config.maintenance_vault @ ControllerError::AddressMismatch)]
    pub maintenance_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: `[b"maintenance"]` PDA — signs the bounded reward transfer.
    #[account(seeds = [MAINTENANCE_AUTHORITY_SEED], bump = config.maintenance_authority_bump)]
    pub maintenance_authority: UncheckedAccount<'info>,

    /// The caller's fuSOL account for the crank reward.
    #[account(mut, constraint = crank_reward_account.mint == config.fusol_mint @ ControllerError::AddressMismatch)]
    pub crank_reward_account: Box<Account<'info, TokenAccount>>,

    /// CHECK: the pinned stake-pool FORK program.
    #[account(address = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch)]
    pub stake_pool_program: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<FinalizePool>) -> Result<()> {
    let clock = Clock::get()?;

    // Phase gate + previous finalized snapshot (for the negative-NAV comparison).
    let (controller_epoch, prev_total, prev_supply) = {
        let es = ctx.accounts.epoch_state.load()?;
        require!(es.phase == PHASE_FINALIZE, ControllerError::WrongPhase);
        (es.controller_epoch, es.nav_total_lamports, es.nav_fusol_supply)
    };

    // Reward condition input: was the canonical snapshot still stale before our CPI?
    let pre_update_epoch = {
        let data = ctx.accounts.stake_pool.try_borrow_data()?;
        fusion_stake_view::stake_pool::parse(&data)
            .map_err(|_| error!(ControllerError::InvalidStakePoolAccount))?
            .last_update_epoch
    };

    // CPI 1: canonical totals + epoch fee mint (fails StakeListOutOfDate if reconcile was
    // incomplete — the upstream completion proof).
    let ix = spl_cpi::update_stake_pool_balance(
        &ctx.accounts.stake_pool.key(),
        &ctx.accounts.pool_withdraw_authority.key(),
        &ctx.accounts.validator_list.key(),
        &ctx.accounts.reserve_stake.key(),
        &ctx.accounts.maintenance_vault.key(),
        &ctx.accounts.fusol_mint.key(),
        &ctx.accounts.token_program.key(),
    );
    invoke(
        &ix,
        &[
            ctx.accounts.stake_pool.to_account_info(),
            ctx.accounts.pool_withdraw_authority.to_account_info(),
            ctx.accounts.validator_list.to_account_info(),
            ctx.accounts.reserve_stake.to_account_info(),
            ctx.accounts.maintenance_vault.to_account_info(),
            ctx.accounts.fusol_mint.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
            ctx.accounts.stake_pool_program.to_account_info(),
        ],
    )?;

    // CPI 2: drop ReadyForRemoval entries (pool passed WRITABLE so a dangling
    // preferred-withdraw validator auto-resets upstream).
    let ix = spl_cpi::cleanup_removed_validator_entries(
        &ctx.accounts.stake_pool.key(),
        &ctx.accounts.validator_list.key(),
    );
    invoke(
        &ix,
        &[
            ctx.accounts.stake_pool.to_account_info(),
            ctx.accounts.validator_list.to_account_info(),
            ctx.accounts.stake_pool_program.to_account_info(),
        ],
    )?;

    // Snapshot the canonical totals the upstream program just finalized.
    let (new_total, new_supply) = {
        let data = ctx.accounts.stake_pool.try_borrow_data()?;
        let pool = fusion_stake_view::stake_pool::parse(&data)
            .map_err(|_| error!(ControllerError::InvalidStakePoolAccount))?;
        (pool.total_lamports, pool.pool_token_supply)
    };
    let nav_decreased = nav_rate_decreased(prev_total, prev_supply, new_total, new_supply);

    let window_close_slot = clock
        .slot
        .checked_add(
            EpochSchedule::get()?.get_slots_in_epoch(clock.epoch)
                / PREFERENCE_WINDOW_SLOT_DIVISOR,
        )
        .ok_or(ControllerError::MathOverflow)?;

    let paid;
    {
        let mut es = ctx.accounts.epoch_state.load_mut()?;
        es.nav_total_lamports = new_total;
        es.nav_fusol_supply = new_supply;
        es.reserve_target =
            reserve_target(new_total, RESERVE_MINIMUM_LAMPORTS, RESERVE_TARGET_BPS);
        es.productive_lamports = productive_lamports(new_total, es.reserve_target);
        // Refresh the churn budget from the FRESH snapshot (start_epoch seeded it from the
        // previous one; nothing can have spent any of it before REBALANCE).
        es.churn_budget_total = global_churn_budget(new_total, GLOBAL_CHURN_CAP_BPS);
        es.preference_window_close_slot = window_close_slot;
        require!(
            phase_transition_allowed(es.phase, PHASE_PREFERENCES),
            ControllerError::WrongPhase
        );
        es.phase = PHASE_PREFERENCES;

        // Reward only if OUR CPI advanced the canonical snapshot to this epoch.
        paid = if pre_update_epoch < clock.epoch {
            crate::maintenance::pay_crank_reward(
                &ctx.accounts.token_program,
                &mut ctx.accounts.maintenance_vault,
                &ctx.accounts.maintenance_authority,
                &ctx.accounts.crank_reward_account,
                ctx.accounts.config.maintenance_authority_bump,
                crate::constants::CRANK_REWARD_FINALIZE_POOL,
                &mut es.epoch_payout_budget_used,
            )?
        } else {
            0
        };
    }

    if nav_decreased {
        emit_cpi!(crate::events::NegativeNavObserved {
            epoch: controller_epoch,
            previous_total_lamports: prev_total,
            new_total_lamports: new_total,
            fusol_supply: new_supply,
        });
    }
    emit_cpi!(crate::events::EpochPhaseChanged {
        epoch: controller_epoch,
        from_phase: PHASE_FINALIZE,
        to_phase: PHASE_PREFERENCES,
        slot: clock.slot,
    });
    if paid > 0 {
        emit_cpi!(crate::events::MaintenanceRewardPaid {
            crank: ctx.accounts.crank_reward_account.key(),
            task: crate::events::TASK_FINALIZE_POOL,
            amount: paid,
            epoch: controller_epoch,
        });
    }
    Ok(())
}
