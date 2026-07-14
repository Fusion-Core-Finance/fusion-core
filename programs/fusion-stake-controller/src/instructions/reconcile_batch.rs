//! Anyone: RECONCILE — CPI `UpdateValidatorListBalance` over the next bounded validator-list
//! slice (merging completed transients), record every covered validator's canonical balances
//! and health observations, accumulate the global-liveness-guard aggregates, and advance
//! `reconcile_cursor`.
//!
//! **Observations here, lifecycle advancement in PLAN-DIRECTED.** The global liveness guard
//! reads `healthy/total delegated` over the WHOLE pool; advancing any lifecycle before the
//! aggregate is complete would evaluate the guard on a partial sum. Reconcile therefore only
//! records per-validator observations (`observed_commission_ok` / `observed_liveness_ok`,
//! balances, `has_pool_stake`) stamped with `observed_epoch`, and PLAN-DIRECTED — which runs
//! strictly after every entry is covered — advances lifecycles from one consistent snapshot.
//!
//! Every pair address is re-derived on-chain from the live validator list (upstream silently
//! SKIPS a mismatched pair, leaving the entry stale and wedging finalization — we fail loudly
//! WITHOUT advancing instead). An unreadable vote account (closed, truncated, or a future
//! VoteState version) observes `liveness_ok = false, commission_ok = true`: systemic parse
//! failures must surface through the guard-suppressible liveness channel, never as a mass
//! commission drain (see `logic::observe_vote`).
//!
//! `remaining_accounts` layout: `4N` accounts — QUADS
//! `(validator_stake_i [w], transient_stake_i [w], validator_record_i [w], vote_account_i [])`
//! for consecutive list indices starting at `reconcile_cursor`. (Stage-A note said 2N pairs;
//! the record+vote extension is what lets observations live here — see the module doc above.)
//!
//! Reward: `CRANK_REWARD_RECONCILE_BATCH` iff at least one previously stale entry became
//! current. Completion (cursor == list length) transitions to FINALIZE.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke;
use anchor_lang::solana_program::sysvar;
use anchor_spl::token::{Token, TokenAccount};

use crate::constants::{
    COMMISSION_CAP_PERCENT, CONTROLLER_SEED, EPOCH_STATE_SEED, FUSION_STAKE_POOL_PROGRAM_ID,
    MAINTENANCE_AUTHORITY_SEED, VOTE_FRESHNESS_WINDOW_DIVISOR, VOTE_PROGRAM_ID,
};
use crate::errors::ControllerError;
use crate::logic::{observe_vote, phase_transition_allowed};
use crate::spl_cpi;
use crate::state::{ControllerConfig, EpochState, ValidatorRecord, PHASE_FINALIZE, PHASE_RECONCILE};

#[event_cpi]
#[derive(Accounts)]
pub struct ReconcileBatch<'info> {
    #[account(
        seeds = [CONTROLLER_SEED],
        bump = config.bump,
        constraint = config.sealed @ ControllerError::PoolNotInitialized,
    )]
    pub config: Box<Account<'info, ControllerConfig>>,

    /// The single crank write lane (cursor + phase).
    #[account(mut, seeds = [EPOCH_STATE_SEED], bump)]
    pub epoch_state: AccountLoader<'info, EpochState>,

    /// CHECK: read-only in `UpdateValidatorListBalance`.
    #[account(
        address = config.stake_pool @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub stake_pool: UncheckedAccount<'info>,

    /// CHECK: the recorded stake-pool withdraw authority.
    #[account(address = config.pool_withdraw_authority @ ControllerError::AddressMismatch)]
    pub pool_withdraw_authority: UncheckedAccount<'info>,

    /// CHECK: the recorded validator list (entries stamped + balances recorded by the CPI).
    #[account(
        mut,
        address = config.validator_list @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub validator_list: UncheckedAccount<'info>,

    /// CHECK: the recorded reserve stake (receives merged inactive transients).
    #[account(mut, address = config.reserve_stake @ ControllerError::AddressMismatch)]
    pub reserve_stake: UncheckedAccount<'info>,

    /// CHECK: clock sysvar (CPI account; address-pinned).
    #[account(address = sysvar::clock::ID)]
    pub clock: UncheckedAccount<'info>,

    /// CHECK: stake-history sysvar (CPI account; address-pinned, never deserialized).
    #[account(address = sysvar::stake_history::ID)]
    pub stake_history: UncheckedAccount<'info>,

    /// CHECK: the native stake program (CPI account).
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
    // remaining_accounts: 4N — (validator_stake [w], transient_stake [w], validator_record [w],
    // vote_account []) quads for consecutive list indices starting at `reconcile_cursor`.
}

pub fn handler<'info>(ctx: Context<'_, '_, 'info, 'info, ReconcileBatch<'info>>) -> Result<()> {
    let clock = Clock::get()?;
    let epoch_schedule = EpochSchedule::get()?;
    let freshness_window =
        epoch_schedule.get_slots_in_epoch(clock.epoch) / VOTE_FRESHNESS_WINDOW_DIVISOR;

    let mut es = ctx.accounts.epoch_state.load_mut()?;
    require!(es.phase == PHASE_RECONCILE, ControllerError::WrongPhase);
    let cursor = es.reconcile_cursor;
    let controller_epoch = es.controller_epoch;

    let quads = ctx.remaining_accounts.len() / 4;
    // `% 4 == 0` (not `.is_multiple_of(4)`): the SBF toolchain (platform-tools v1.48,
    // cargo 1.84) predates `is_multiple_of` — same note as the EpochState size assert.
    #[allow(clippy::manual_is_multiple_of)]
    {
        require!(
            ctx.remaining_accounts.len() % 4 == 0,
            ControllerError::InvalidRemainingAccounts
        );
    }

    // Pre-CPI: bind every quad to its list entry and derive the exact upstream pair addresses.
    let mut expected: Vec<(Pubkey, u64, bool)> = Vec::with_capacity(quads); // (vote, index, was_stale)
    let list_len: u64;
    {
        let list_data = ctx.accounts.validator_list.try_borrow_data()?;
        let header = fusion_stake_view::validator_list::parse_header(&list_data)
            .map_err(|_| error!(ControllerError::InvalidValidatorListEntry))?;
        list_len = u64::from(header.len);
        // A batch must cover at least one entry unless the list is (already) fully covered —
        // the empty-batch form exists solely to complete an empty/finished phase.
        require!(
            cursor.checked_add(quads as u64).ok_or(ControllerError::MathOverflow)? <= list_len,
            ControllerError::InvalidRemainingAccounts
        );
        require!(quads > 0 || cursor == list_len, ControllerError::InvalidRemainingAccounts);

        let pool_key = ctx.accounts.stake_pool.key();
        for i in 0..quads {
            let index = cursor + i as u64;
            let entry =
                fusion_stake_view::validator_list::entry_at(&list_data, index as u32)
                    .ok_or(ControllerError::InvalidValidatorListEntry)?;
            let vote = Pubkey::new_from_array(entry.vote_account_address);
            let expect_validator_stake =
                spl_cpi::derive_validator_stake(&vote, &pool_key, entry.validator_seed_suffix);
            let expect_transient =
                spl_cpi::derive_transient_stake(&vote, &pool_key, entry.transient_seed_suffix);
            let quad = &ctx.remaining_accounts[4 * i..4 * i + 4];
            require!(
                quad[0].key() == expect_validator_stake
                    && quad[1].key() == expect_transient
                    && quad[3].key() == vote,
                ControllerError::InvalidRemainingAccounts
            );
            expected.push((vote, index, entry.last_update_epoch < clock.epoch));
        }
    }

    // CPI: update the slice with merges enabled. Pool/list/reserve/sysvars are struct-pinned;
    // the pair addresses were just re-derived from the on-chain list, so upstream's
    // silent-skip-on-mismatch path is unreachable.
    if quads > 0 {
        let pairs: Vec<(Pubkey, Pubkey)> = (0..quads)
            .map(|i| {
                (
                    ctx.remaining_accounts[4 * i].key(),
                    ctx.remaining_accounts[4 * i + 1].key(),
                )
            })
            .collect();
        let ix = spl_cpi::update_validator_list_balance(
            &ctx.accounts.stake_pool.key(),
            &ctx.accounts.pool_withdraw_authority.key(),
            &ctx.accounts.validator_list.key(),
            &ctx.accounts.reserve_stake.key(),
            &pairs,
            cursor as u32,
            false, // no_merge = false: always merge completed transients
        );
        let mut infos: Vec<AccountInfo<'info>> = vec![
            ctx.accounts.stake_pool.to_account_info(),
            ctx.accounts.pool_withdraw_authority.to_account_info(),
            ctx.accounts.validator_list.to_account_info(),
            ctx.accounts.reserve_stake.to_account_info(),
            ctx.accounts.clock.to_account_info(),
            ctx.accounts.stake_history.to_account_info(),
            ctx.accounts.stake_program.to_account_info(),
        ];
        for i in 0..quads {
            infos.push(ctx.remaining_accounts[4 * i].clone());
            infos.push(ctx.remaining_accounts[4 * i + 1].clone());
        }
        infos.push(ctx.accounts.stake_pool_program.to_account_info());
        invoke(&ix, &infos)?; // permissionless upstream — no PDA signature needed
    }

    // Post-CPI: record canonical observations per covered validator and fold the guard
    // aggregates. `prior_epoch` is the last COMPLETED epoch relative to the cycle.
    let prior_epoch = controller_epoch.saturating_sub(1);
    let mut any_became_current = false;
    {
        let list_data = ctx.accounts.validator_list.try_borrow_data()?;
        for (i, (vote, index, was_stale)) in expected.iter().enumerate() {
            let entry =
                fusion_stake_view::validator_list::entry_at(&list_data, *index as u32)
                    .ok_or(ControllerError::InvalidValidatorListEntry)?;
            if *was_stale && entry.last_update_epoch == clock.epoch {
                any_became_current = true;
            }

            let record_info = &ctx.remaining_accounts[4 * i + 2];
            require!(record_info.is_writable, ControllerError::InvalidRemainingAccounts);
            let mut record = Account::<ValidatorRecord>::try_from(record_info)?;
            require!(record.vote_account == *vote, ControllerError::InvalidRemainingAccounts);

            // Health observation from the vote account (fail-closed liveness policy).
            let vote_info = &ctx.remaining_accounts[4 * i + 3];
            let sample = if *vote_info.owner == VOTE_PROGRAM_ID {
                let data = vote_info.try_borrow_data()?;
                fusion_stake_view::vote_state::parse(&data, prior_epoch).ok()
            } else {
                None
            };
            let obs = observe_vote(sample, COMMISSION_CAP_PERCENT, clock.slot, freshness_window);

            record.last_active_lamports = entry.active_stake_lamports;
            record.last_transient_lamports = entry.transient_stake_lamports;
            record.has_pool_stake =
                entry.active_stake_lamports > 0 || entry.transient_stake_lamports > 0;
            // Refresh the index cache: Cleanup compacts the list once per epoch, so stored
            // indices are only ever trusted with a current-epoch stamp.
            record.validator_list_index = *index as u32;
            record.observed_epoch = controller_epoch;
            record.observed_commission_ok = obs.commission_ok;
            record.observed_liveness_ok = obs.liveness_ok;
            record.exit(&crate::ID)?;

            // Guard aggregates: DELEGATED lamports only (the reserve is deliberately not in
            // the denominator).
            let delegated = entry
                .active_stake_lamports
                .checked_add(entry.transient_stake_lamports)
                .ok_or(ControllerError::MathOverflow)?;
            es.total_delegated_lamports = es
                .total_delegated_lamports
                .checked_add(delegated)
                .ok_or(ControllerError::MathOverflow)?;
            if obs.liveness_ok {
                es.healthy_delegated_lamports = es
                    .healthy_delegated_lamports
                    .checked_add(delegated)
                    .ok_or(ControllerError::MathOverflow)?;
            }
        }
    }

    // Advance the monotonic cursor; completion proof = every entry covered.
    es.reconcile_cursor = cursor + quads as u64;
    let completed = es.reconcile_cursor == list_len;
    if completed {
        require!(
            phase_transition_allowed(es.phase, PHASE_FINALIZE),
            ControllerError::WrongPhase
        );
        es.phase = PHASE_FINALIZE;
    }

    // Reward: only work that brought a stale entry current earns (no-op/duplicate = zero).
    let paid = if any_became_current {
        crate::maintenance::pay_crank_reward(
            &ctx.accounts.token_program,
            &mut ctx.accounts.maintenance_vault,
            &ctx.accounts.maintenance_authority,
            &ctx.accounts.crank_reward_account,
            ctx.accounts.config.maintenance_authority_bump,
            crate::constants::CRANK_REWARD_RECONCILE_BATCH,
            &mut es.epoch_payout_budget_used,
        )?
    } else {
        0
    };
    drop(es);

    if completed {
        emit_cpi!(crate::events::EpochPhaseChanged {
            epoch: controller_epoch,
            from_phase: PHASE_RECONCILE,
            to_phase: PHASE_FINALIZE,
            slot: clock.slot,
        });
    }
    if paid > 0 {
        emit_cpi!(crate::events::MaintenanceRewardPaid {
            crank: ctx.accounts.crank_reward_account.key(),
            task: crate::events::TASK_RECONCILE_BATCH,
            amount: paid,
            epoch: controller_epoch,
        });
    }
    Ok(())
}
