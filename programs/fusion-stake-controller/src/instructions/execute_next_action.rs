//! Anyone: REBALANCE — execute THE one deterministic valid action (add / increase / decrease /
//! remove) for the current walk slot. The caller chooses nothing: it must supply the accounts
//! for the validator the controller derives.
//!
//! ## Determinism model (documented deviation from the spec's global-priority ordering)
//!
//! The spec's per-epoch rule orders ordinary moves by GLOBAL greatest deficit/surplus
//! (greatest-deficit-first). This handler does NOT implement that priority — deliberately.
//! Verifying a global maximum on-chain requires the full record set in one transaction —
//! infeasible at `MAX_VALIDATORS = 1024` under transaction account limits — and any
//! subset-based selection would let a caller steer the choice by omission. What it enforces
//! instead is **monotonic cursor-order execution** (`logic::rebalance_slot`): two full passes
//! over the planned validator ordinals — pass 0 executes ONLY Draining decreases and removals
//! ("drains first" preserved globally), pass 1 the ordinary deficit/surplus moves in cursor
//! order, NOT deficit order — each pass walking from the epoch-rotating start index
//! (`fusion_stake_math::targets::rotation_start`, the spec's tie-break rotation applied as
//! walk order, so scarce budget/reserve is not permanently biased toward low indices).
//! The trade is priority for batchability and non-steerability: under a binding churn budget
//! or reserve shortfall, budget goes to the rotated walk order's earlier visits rather than
//! the largest deviations; every planned validator is still visited exactly once per pass, and
//! the walk converges to the same finalized targets over epochs. Exactly one validator per
//! call; the passed record must sit at the cursor's index or the call fails WITHOUT advancing. Skips (hysteresis, live
//! transient, budget, minimum-action floor, frozen increases) advance the cursor, pay zero,
//! and emit `RebalanceActionExecuted { action: ACTION_SKIP }` — the executed CHOICE is always
//! returned in the event. Every action amount is `fusion_stake_math::churn::action_amount`
//! (deviation, remaining global budget, per-validator cap, source capacity, minimum action).
//!
//! **Admission adds** are cursor-INDEPENDENT: a planned Candidate with no list slot
//! (`validator_list_index == UNSET`) may be added at any time during REBALANCE. Per validator
//! the action is uniquely determined ("add"), and adds commute — each touches a distinct
//! validator stake account, funds only the minimum delegation + rent from the reserve
//! (operational funding, not churn), and list capacity races resolve upstream (list-full add
//! fails cleanly). The only caller freedom is WHICH pending admission lands first, which has
//! no allocation effect. Newly added validators receive increases from the NEXT epoch's plan
//! (their entry ordinal lies outside this epoch's planned walk) — flow-first carry-forward.
//!
//! ## Upstream minimum reconciliation (plan obligation)
//!
//! Increases and decreases are floored at the EFFECTIVE minimum delegation, derived at
//! runtime per call (`spl_cpi::effective_minimum_delegation`: the stake program's
//! `GetMinimumDelegation` with the upstream `MINIMUM_ACTIVE_STAKE` floor — exactly what the
//! pool re-computes at CPI time); a decrease must also leave `stake_rent + minimum_delegation`
//! on the validator account. Sizing from a constant instead would let a sub-minimum action
//! fail its CPI, which rolls back the WHOLE instruction — the cursor would never advance past
//! that validator and the walk would wedge until the next epoch preemption. A Draining
//! residual below one minimum move can NEVER leave via a decrease — it exits via
//! `RemoveValidatorFromPool` (whole-account deactivation) once the record reaches Removable:
//! that is how the crate-level full-drain exemption (D15) reconciles with the pinned pool
//! rules. Draining decreases are hysteresis-EXEMPT (a lifecycle exit, not an optimization);
//! ordinary moves are hysteresis-gated.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::solana_program::sysvar;
use anchor_spl::token::{Token, TokenAccount};

use crate::constants::{
    CONTROLLER_SEED, EPOCH_STATE_SEED, FUSION_STAKE_POOL_PROGRAM_ID, HYSTERESIS_BPS,
    HYSTERESIS_MIN_LAMPORTS, MAINTENANCE_AUTHORITY_SEED, MIN_ACTIVATION_TARGET_LAMPORTS,
    POOL_AUTHORITY_SEED, STAKE_ACCOUNT_SPACE, STAKE_CONFIG_ID, VALIDATOR_LIST_INDEX_UNSET,
    VALIDATOR_MOVE_CAP_BPS, VALIDATOR_RECORD_SEED,
};
use crate::errors::ControllerError;
use crate::logic::{decide_action, rebalance_slot, Action, ActionInputs};
use crate::spl_cpi;
use crate::state::{ControllerConfig, EpochState, ValidatorRecord, PHASE_REBALANCE};
use fusion_stake_math::churn::{hysteresis, validator_move_cap};
use fusion_stake_math::lifecycle::ValidatorStatus;

#[event_cpi]
#[derive(Accounts)]
pub struct ExecuteNextAction<'info> {
    #[account(
        seeds = [CONTROLLER_SEED],
        bump = config.bump,
        constraint = config.sealed @ ControllerError::PoolNotInitialized,
    )]
    pub config: Box<Account<'info, ControllerConfig>>,

    #[account(mut, seeds = [EPOCH_STATE_SEED], bump)]
    pub epoch_state: AccountLoader<'info, EpochState>,

    /// CHECK: writable — add/remove mutate pool state (increase/decrease read it).
    #[account(
        mut,
        address = config.stake_pool @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub stake_pool: UncheckedAccount<'info>,

    /// CHECK: `[b"pool_authority"]` PDA — the staker; signs every rebalance CPI.
    #[account(seeds = [POOL_AUTHORITY_SEED], bump = config.pool_authority_bump)]
    pub pool_authority: UncheckedAccount<'info>,

    /// CHECK: the recorded stake-pool withdraw authority.
    #[account(address = config.pool_withdraw_authority @ ControllerError::AddressMismatch)]
    pub pool_withdraw_authority: UncheckedAccount<'info>,

    /// CHECK: the recorded validator list.
    #[account(
        mut,
        address = config.validator_list @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub validator_list: UncheckedAccount<'info>,

    /// CHECK: the recorded reserve stake (increase source / decrease rent-funder / add funder).
    #[account(mut, address = config.reserve_stake @ ControllerError::AddressMismatch)]
    pub reserve_stake: UncheckedAccount<'info>,

    /// CHECK: the TARGET validator's vote account — handler-verified to be the deterministic
    /// selection, never a caller choice.
    pub vote_account: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [VALIDATOR_RECORD_SEED, vote_account.key().as_ref()],
        bump = validator_record.bump,
    )]
    pub validator_record: Box<Account<'info, ValidatorRecord>>,

    /// CHECK: the validator's pool stake account (re-derived here AND upstream per CPI).
    #[account(mut)]
    pub validator_stake_account: UncheckedAccount<'info>,

    /// CHECK: the validator's transient stake account (created/consumed by increase/decrease;
    /// deactivated by remove; unused rider for add).
    #[account(mut)]
    pub transient_stake_account: UncheckedAccount<'info>,

    /// CHECK: clock sysvar (CPI account).
    #[account(address = sysvar::clock::ID)]
    pub clock: UncheckedAccount<'info>,

    /// CHECK: rent sysvar (CPI account — add/increase take it as an account).
    #[account(address = sysvar::rent::ID)]
    pub rent: UncheckedAccount<'info>,

    /// CHECK: stake-history sysvar (CPI account, never deserialized).
    #[account(address = sysvar::stake_history::ID)]
    pub stake_history: UncheckedAccount<'info>,

    /// CHECK: the (legacy) stake config sysvar account (add/increase interface shape).
    #[account(address = STAKE_CONFIG_ID)]
    pub stake_config: UncheckedAccount<'info>,

    /// CHECK: the native stake program.
    #[account(address = crate::constants::STAKE_PROGRAM_ID)]
    pub stake_program: UncheckedAccount<'info>,

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
    pub system_program: Program<'info, System>,
}

pub fn handler(ctx: Context<ExecuteNextAction>) -> Result<()> {
    let stake_rent = Rent::get()?.minimum_balance(STAKE_ACCOUNT_SPACE);
    let pool_key = ctx.accounts.stake_pool.key();
    let vote_key = ctx.accounts.vote_account.key();

    let mut es = ctx.accounts.epoch_state.load_mut()?;
    require!(es.phase == PHASE_REBALANCE, ControllerError::WrongPhase);
    let controller_epoch = es.controller_epoch;
    let planned_len = es.plan_directed_cursor;

    let record = &mut ctx.accounts.validator_record;
    require!(record.plan_epoch == controller_epoch, ControllerError::StaleValidatorRecord);
    let status =
        ValidatorStatus::from_u8(record.status).ok_or(ControllerError::CorruptValidatorStatus)?;

    // -------- Admission mode: planned Candidate without a list slot → AddValidatorToPool ----
    if record.validator_list_index == VALIDATOR_LIST_INDEX_UNSET {
        require!(status == ValidatorStatus::Candidate, ControllerError::WrongActionTarget);
        // The spec's admission row re-checked at execution: calculated (raw) directed target
        // at/above the activation minimum, from THIS epoch's counted shares, plus a healthy
        // current observation (no adds for a failing validator).
        require!(
            record.observed_epoch == controller_epoch
                && record.observed_liveness_ok
                && record.observed_commission_ok,
            ControllerError::WrongActionTarget
        );
        let shares = if record.directed_shares_epoch == controller_epoch {
            record.directed_shares
        } else {
            0
        };
        require!(es.nav_fusol_supply > 0, ControllerError::WrongActionTarget);
        let raw = u128::from(es.productive_lamports) * u128::from(shares)
            / u128::from(es.nav_fusol_supply);
        require!(
            raw >= u128::from(MIN_ACTIVATION_TARGET_LAMPORTS),
            ControllerError::WrongActionTarget
        );

        // The new entry always uses seed 0 (no seed component) — the controller never creates
        // seeded validator stake accounts.
        let expect_stake = spl_cpi::derive_validator_stake(&vote_key, &pool_key, 0);
        require!(
            ctx.accounts.validator_stake_account.key() == expect_stake,
            ControllerError::AddressMismatch
        );
        // The appended entry's index = current list length (upstream pushes to the back).
        let new_index = {
            let list_data = ctx.accounts.validator_list.try_borrow_data()?;
            fusion_stake_view::validator_list::parse_header(&list_data)
                .map_err(|_| error!(ControllerError::InvalidValidatorListEntry))?
                .len
        };

        let ix = spl_cpi::add_validator_to_pool(
            &pool_key,
            &ctx.accounts.pool_authority.key(),
            &ctx.accounts.reserve_stake.key(),
            &ctx.accounts.pool_withdraw_authority.key(),
            &ctx.accounts.validator_list.key(),
            &expect_stake,
            &vote_key,
            0,
        );
        invoke_signed(
            &ix,
            &[
                ctx.accounts.stake_pool.to_account_info(),
                ctx.accounts.pool_authority.to_account_info(),
                ctx.accounts.reserve_stake.to_account_info(),
                ctx.accounts.pool_withdraw_authority.to_account_info(),
                ctx.accounts.validator_list.to_account_info(),
                ctx.accounts.validator_stake_account.to_account_info(),
                ctx.accounts.vote_account.to_account_info(),
                ctx.accounts.rent.to_account_info(),
                ctx.accounts.clock.to_account_info(),
                ctx.accounts.stake_history.to_account_info(),
                ctx.accounts.stake_config.to_account_info(),
                ctx.accounts.system_program.to_account_info(),
                ctx.accounts.stake_program.to_account_info(),
                ctx.accounts.stake_pool_program.to_account_info(),
            ],
            &[&[POOL_AUTHORITY_SEED, &[ctx.accounts.config.pool_authority_bump]]],
        )?;

        record.validator_list_index = new_index;
        record.pool_entry_status = 0; // upstream StakeStatus::Active
        es.rebalance_actions_done += 1;

        let paid = crate::maintenance::pay_crank_reward(
            &ctx.accounts.token_program,
            &mut ctx.accounts.maintenance_vault,
            &ctx.accounts.maintenance_authority,
            &ctx.accounts.crank_reward_account,
            ctx.accounts.config.maintenance_authority_bump,
            crate::constants::CRANK_REWARD_REBALANCE_ACTION,
            &mut es.epoch_payout_budget_used,
        )?;
        drop(es);

        emit_cpi!(crate::events::RebalanceActionExecuted {
            epoch: controller_epoch,
            action: spl_cpi::IX_ADD_VALIDATOR_TO_POOL,
            vote_account: vote_key,
            lamports: 0,
        });
        if paid > 0 {
            emit_cpi!(crate::events::MaintenanceRewardPaid {
                crank: ctx.accounts.crank_reward_account.key(),
                task: crate::events::TASK_REBALANCE_ACTION,
                amount: paid,
                epoch: controller_epoch,
            });
        }
        return Ok(());
    }

    // -------- Cursor mode: the walk names exactly one index; the passed record must be it ---
    let slot = rebalance_slot(es.rebalance_cursor, planned_len, controller_epoch)
        .ok_or(ControllerError::RebalanceComplete)?;
    require!(
        u64::from(record.validator_list_index) == slot.index,
        ControllerError::WrongActionTarget
    );

    // Live canonical entry (plan supplies targets; the chain supplies the physics).
    let (entry_active, entry_transient, transient_seed, validator_seed) = {
        let list_data = ctx.accounts.validator_list.try_borrow_data()?;
        let entry =
            fusion_stake_view::validator_list::entry_at(&list_data, record.validator_list_index)
                .ok_or(ControllerError::InvalidValidatorListEntry)?;
        require!(
            entry.vote_account_address == vote_key.to_bytes(),
            ControllerError::InvalidValidatorListEntry
        );
        (
            entry.active_stake_lamports,
            entry.transient_stake_lamports,
            entry.transient_seed_suffix,
            entry.validator_seed_suffix,
        )
    };

    // The EFFECTIVE minimum delegation, derived at runtime (never the constant alone): every
    // floor below must match what the pool itself will enforce inside the CPI, or a
    // sub-minimum action fails and wedges the cursor for the epoch (see the module doc).
    let min_delegation = spl_cpi::effective_minimum_delegation()?;

    let reserve_lamports = ctx.accounts.reserve_stake.lamports();
    let inputs = ActionInputs {
        pass: slot.pass,
        status,
        active_lamports: entry_active,
        transient_lamports: entry_transient,
        final_target: record.final_target,
        drained_floor: stake_rent.saturating_add(min_delegation),
        hysteresis_threshold: hysteresis(
            es.nav_total_lamports,
            HYSTERESIS_MIN_LAMPORTS,
            HYSTERESIS_BPS,
        ),
        budget_remaining: es.churn_budget_total.saturating_sub(es.churn_budget_used),
        validator_move_cap: validator_move_cap(es.nav_total_lamports, VALIDATOR_MOVE_CAP_BPS),
        reserve_available_for_increase: reserve_lamports
            .saturating_sub(es.reserve_target)
            .saturating_sub(stake_rent),
        reserve_lamports,
        stake_rent,
        min_delegation,
        increases_allowed: record.observed_epoch == controller_epoch
            && record.observed_liveness_ok,
        acted_this_epoch: record.last_increase_epoch == controller_epoch
            || record.last_decrease_epoch == controller_epoch,
    };
    let action = decide_action(&inputs);

    // Every non-skip action needs the exact upstream pair accounts.
    if action != Action::Skip {
        let expect_stake = spl_cpi::derive_validator_stake(&vote_key, &pool_key, validator_seed);
        let expect_transient =
            spl_cpi::derive_transient_stake(&vote_key, &pool_key, transient_seed);
        require!(
            ctx.accounts.validator_stake_account.key() == expect_stake
                && ctx.accounts.transient_stake_account.key() == expect_transient,
            ControllerError::AddressMismatch
        );
    }

    let signer_seeds: &[&[&[u8]]] =
        &[&[POOL_AUTHORITY_SEED, &[ctx.accounts.config.pool_authority_bump]]];
    let (action_tag, moved) = match action {
        Action::Skip => (crate::events::ACTION_SKIP, 0),
        Action::Remove => {
            let ix = spl_cpi::remove_validator_from_pool(
                &pool_key,
                &ctx.accounts.pool_authority.key(),
                &ctx.accounts.pool_withdraw_authority.key(),
                &ctx.accounts.validator_list.key(),
                &ctx.accounts.validator_stake_account.key(),
                &ctx.accounts.transient_stake_account.key(),
            );
            invoke_signed(
                &ix,
                &[
                    ctx.accounts.stake_pool.to_account_info(),
                    ctx.accounts.pool_authority.to_account_info(),
                    ctx.accounts.pool_withdraw_authority.to_account_info(),
                    ctx.accounts.validator_list.to_account_info(),
                    ctx.accounts.validator_stake_account.to_account_info(),
                    ctx.accounts.transient_stake_account.to_account_info(),
                    ctx.accounts.clock.to_account_info(),
                    ctx.accounts.stake_program.to_account_info(),
                    ctx.accounts.stake_pool_program.to_account_info(),
                ],
                signer_seeds,
            )?;
            // The entry deactivates now and physically leaves at next epoch's cleanup.
            record.pool_entry_status = 3; // upstream StakeStatus::DeactivatingValidator
            (spl_cpi::IX_REMOVE_VALIDATOR_FROM_POOL, entry_active)
        }
        Action::Decrease(amount) => {
            let ix = spl_cpi::decrease_validator_stake_with_reserve(
                &pool_key,
                &ctx.accounts.pool_authority.key(),
                &ctx.accounts.pool_withdraw_authority.key(),
                &ctx.accounts.validator_list.key(),
                &ctx.accounts.reserve_stake.key(),
                &ctx.accounts.validator_stake_account.key(),
                &ctx.accounts.transient_stake_account.key(),
                amount,
                transient_seed,
            );
            invoke_signed(
                &ix,
                &[
                    ctx.accounts.stake_pool.to_account_info(),
                    ctx.accounts.pool_authority.to_account_info(),
                    ctx.accounts.pool_withdraw_authority.to_account_info(),
                    ctx.accounts.validator_list.to_account_info(),
                    ctx.accounts.reserve_stake.to_account_info(),
                    ctx.accounts.validator_stake_account.to_account_info(),
                    ctx.accounts.transient_stake_account.to_account_info(),
                    ctx.accounts.clock.to_account_info(),
                    ctx.accounts.stake_history.to_account_info(),
                    ctx.accounts.system_program.to_account_info(),
                    ctx.accounts.stake_program.to_account_info(),
                    ctx.accounts.stake_pool_program.to_account_info(),
                ],
                signer_seeds,
            )?;
            record.last_decrease_epoch = controller_epoch;
            es.churn_budget_used = es
                .churn_budget_used
                .checked_add(amount)
                .ok_or(ControllerError::MathOverflow)?;
            (spl_cpi::IX_DECREASE_VALIDATOR_STAKE_WITH_RESERVE, amount)
        }
        Action::Increase(amount) => {
            let ix = spl_cpi::increase_validator_stake(
                &pool_key,
                &ctx.accounts.pool_authority.key(),
                &ctx.accounts.pool_withdraw_authority.key(),
                &ctx.accounts.validator_list.key(),
                &ctx.accounts.reserve_stake.key(),
                &ctx.accounts.transient_stake_account.key(),
                &ctx.accounts.validator_stake_account.key(),
                &vote_key,
                amount,
                transient_seed,
            );
            invoke_signed(
                &ix,
                &[
                    ctx.accounts.stake_pool.to_account_info(),
                    ctx.accounts.pool_authority.to_account_info(),
                    ctx.accounts.pool_withdraw_authority.to_account_info(),
                    ctx.accounts.validator_list.to_account_info(),
                    ctx.accounts.reserve_stake.to_account_info(),
                    ctx.accounts.transient_stake_account.to_account_info(),
                    ctx.accounts.validator_stake_account.to_account_info(),
                    ctx.accounts.vote_account.to_account_info(),
                    ctx.accounts.clock.to_account_info(),
                    ctx.accounts.rent.to_account_info(),
                    ctx.accounts.stake_history.to_account_info(),
                    ctx.accounts.stake_config.to_account_info(),
                    ctx.accounts.system_program.to_account_info(),
                    ctx.accounts.stake_program.to_account_info(),
                    ctx.accounts.stake_pool_program.to_account_info(),
                ],
                signer_seeds,
            )?;
            record.last_increase_epoch = controller_epoch;
            es.churn_budget_used = es
                .churn_budget_used
                .checked_add(amount)
                .ok_or(ControllerError::MathOverflow)?;
            (spl_cpi::IX_INCREASE_VALIDATOR_STAKE, amount)
        }
    };

    // The cursor advances on action AND on skip — the walk is monotonic either way; only a
    // state-changing CPI counts as an action or earns.
    es.rebalance_cursor += 1;
    let paid = if action_tag != crate::events::ACTION_SKIP {
        es.rebalance_actions_done += 1;
        crate::maintenance::pay_crank_reward(
            &ctx.accounts.token_program,
            &mut ctx.accounts.maintenance_vault,
            &ctx.accounts.maintenance_authority,
            &ctx.accounts.crank_reward_account,
            ctx.accounts.config.maintenance_authority_bump,
            crate::constants::CRANK_REWARD_REBALANCE_ACTION,
            &mut es.epoch_payout_budget_used,
        )?
    } else {
        0
    };
    drop(es);

    emit_cpi!(crate::events::RebalanceActionExecuted {
        epoch: controller_epoch,
        action: action_tag,
        vote_account: vote_key,
        lamports: moved,
    });
    if paid > 0 {
        emit_cpi!(crate::events::MaintenanceRewardPaid {
            crank: ctx.accounts.crank_reward_account.key(),
            task: crate::events::TASK_REBALANCE_ACTION,
            amount: paid,
            epoch: controller_epoch,
        });
    }
    Ok(())
}
