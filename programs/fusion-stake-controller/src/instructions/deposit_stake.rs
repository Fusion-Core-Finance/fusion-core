//! Stake owner: deposit a fully active, unlocked stake account delegated to a pool validator.
//! The stake pool absorbs the ENTIRE account (no partial deposits upstream), mints fuSOL at
//! the pool rate, and credits the physical stake against the deposited validator's next
//! finalized target (flow-first: a deposit is never split across validators in-transaction).
//!
//! **Client contract (upstream custom-deposit-authority semantics):** BEFORE this instruction,
//! the user must have assigned the stake account's STAKER and WITHDRAWER authorities to the
//! controller's `[b"deposit_authority"]` PDA (two `stake authorize` instructions, normally in
//! the same transaction). The stake-pool program requires its recorded deposit authority to
//! sign the inner re-authorize CPI; our `invoke_signed` provides that signature, and CPI
//! signer privilege extends through the stake-pool program's nested stake-program calls.
//! Activating, deactivating, locked, or wrong-validator stake is rejected upstream.
//!
//! Controller pre-check (spec §8.6 + the plan's controller obligations): the deposit is
//! rejected when it would push the validator past its LIFECYCLE cap — checked against the
//! CANONICAL validator-list entry (live active + transient lamports; the controller never
//! trusts its own cached balances) plus this account's lamports. Draining/Registered
//! validators accept no stake deposits at all.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::solana_program::sysvar;
use anchor_spl::token::{Mint, Token, TokenAccount};

use crate::constants::{
    ACTIVE_VALIDATOR_CAP_BPS, CANDIDATE_CAP_BPS, CONTROLLER_SEED, DEPOSIT_AUTHORITY_SEED,
    FUSION_STAKE_POOL_PROGRAM_ID, VALIDATOR_LIST_INDEX_UNSET, VALIDATOR_RECORD_SEED,
};
use crate::errors::ControllerError;
use crate::spl_cpi;
use crate::state::{ControllerConfig, ValidatorRecord};
use fusion_stake_math::lifecycle::{lifecycle_cap, ValidatorStatus};

#[event_cpi]
#[derive(Accounts)]
pub struct DepositStake<'info> {
    /// The depositing user (event attribution + fee payer; the stake account's authorities
    /// must already point at the deposit-authority PDA — see the module doc).
    #[account(mut)]
    pub depositor: Signer<'info>,

    #[account(
        seeds = [CONTROLLER_SEED],
        bump = config.bump,
        constraint = config.sealed @ ControllerError::PoolNotInitialized,
    )]
    pub config: Box<Account<'info, ControllerConfig>>,

    /// CHECK: pinned to the recorded pool address + FORK-program owner; parsed read-only for
    /// the cap denominator (total pool lamports).
    #[account(
        mut,
        address = config.stake_pool @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub stake_pool: UncheckedAccount<'info>,

    /// CHECK: the recorded validator list; parsed read-only for the live entry balances.
    #[account(
        mut,
        address = config.validator_list @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub validator_list: UncheckedAccount<'info>,

    /// CHECK: `[b"deposit_authority"]` PDA — the pool's stake deposit authority; co-signs.
    #[account(seeds = [DEPOSIT_AUTHORITY_SEED], bump = config.deposit_authority_bump)]
    pub deposit_authority: UncheckedAccount<'info>,

    /// CHECK: the recorded stake-pool withdraw authority.
    #[account(address = config.pool_withdraw_authority @ ControllerError::AddressMismatch)]
    pub pool_withdraw_authority: UncheckedAccount<'info>,

    /// CHECK: the user's stake account to absorb — its full validation (state, lockup,
    /// delegation to `vote_account`, authorities) is upstream's; only its lamports feed the
    /// cap pre-check here.
    #[account(mut)]
    pub user_stake_account: UncheckedAccount<'info>,

    /// CHECK: the vote account the deposited stake is delegated to (upstream rejects a
    /// mismatch between this and the stake's actual voter via the validator stake account).
    pub vote_account: UncheckedAccount<'info>,

    /// The controller's record for that validator — must exist (register + admission first).
    #[account(
        seeds = [VALIDATOR_RECORD_SEED, vote_account.key().as_ref()],
        bump = validator_record.bump,
    )]
    pub validator_record: Box<Account<'info, ValidatorRecord>>,

    /// CHECK: the validator's pool stake account (re-derived and enforced upstream).
    #[account(mut)]
    pub validator_stake_account: UncheckedAccount<'info>,

    /// CHECK: the recorded reserve stake (receives the deposit's rent/extra-SOL portion).
    #[account(mut, address = config.reserve_stake @ ControllerError::AddressMismatch)]
    pub reserve_stake: UncheckedAccount<'info>,

    #[account(mut, address = config.fusol_mint @ ControllerError::AddressMismatch)]
    pub fusol_mint: Box<Account<'info, Mint>>,

    /// Receives the minted fuSOL.
    #[account(mut, token::mint = fusol_mint)]
    pub user_fusol_account: Box<Account<'info, TokenAccount>>,

    /// The maintenance vault (manager fee account; also the referrer slot, fee 0).
    #[account(mut, address = config.maintenance_vault @ ControllerError::AddressMismatch)]
    pub maintenance_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: clock sysvar (CPI account; address-pinned).
    #[account(address = sysvar::clock::ID)]
    pub clock: UncheckedAccount<'info>,

    /// CHECK: stake-history sysvar (CPI account; address-pinned — deliberately NOT
    /// deserialized, it is ~16 KiB).
    #[account(address = sysvar::stake_history::ID)]
    pub stake_history: UncheckedAccount<'info>,

    /// CHECK: the native stake program (CPI account; address-pinned).
    #[account(address = crate::constants::STAKE_PROGRAM_ID)]
    pub stake_program: UncheckedAccount<'info>,

    /// CHECK: the pinned stake-pool FORK program.
    #[account(address = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch)]
    pub stake_pool_program: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<DepositStake>) -> Result<()> {
    let record = &ctx.accounts.validator_record;

    // Lifecycle gate: only Candidate/Active validators accept stake deposits. Draining takes
    // no increases of any kind; Registered has no pool stake account to merge into.
    let status = ValidatorStatus::from_u8(record.status)
        .ok_or(ControllerError::CorruptValidatorStatus)?;
    require!(
        matches!(status, ValidatorStatus::Candidate | ValidatorStatus::Active),
        ControllerError::ValidatorNotInPool
    );
    require!(
        record.validator_list_index != VALIDATOR_LIST_INDEX_UNSET,
        ControllerError::ValidatorNotInPool
    );

    // Cap pre-check against CANONICAL state: live entry balances + the whole deposited
    // account vs the lifecycle cap over the canonical total.
    {
        let pool_data = ctx.accounts.stake_pool.try_borrow_data()?;
        let pool = fusion_stake_view::stake_pool::parse(&pool_data)
            .map_err(|_| error!(ControllerError::InvalidStakePoolAccount))?;
        let list_data = ctx.accounts.validator_list.try_borrow_data()?;
        let entry =
            fusion_stake_view::validator_list::entry_at(&list_data, record.validator_list_index)
                .ok_or(ControllerError::InvalidValidatorListEntry)?;
        require!(
            entry.vote_account_address == record.vote_account.to_bytes(),
            ControllerError::InvalidValidatorListEntry
        );

        let deposit_lamports = ctx.accounts.user_stake_account.lamports();
        require!(deposit_lamports > 0, ControllerError::ZeroAmount);
        let physical_after = entry
            .active_stake_lamports
            .checked_add(entry.transient_stake_lamports)
            .and_then(|v| v.checked_add(deposit_lamports))
            .ok_or(ControllerError::MathOverflow)?;
        let cap = lifecycle_cap(
            status,
            pool.total_lamports,
            CANDIDATE_CAP_BPS,
            ACTIVE_VALIDATOR_CAP_BPS,
        );
        require!(physical_after <= cap, ControllerError::ValidatorCapExceeded);
    }

    let deposit_lamports = ctx.accounts.user_stake_account.lamports();
    let ix = spl_cpi::deposit_stake(
        &ctx.accounts.stake_pool.key(),
        &ctx.accounts.validator_list.key(),
        &ctx.accounts.deposit_authority.key(),
        &ctx.accounts.pool_withdraw_authority.key(),
        &ctx.accounts.user_stake_account.key(),
        &ctx.accounts.validator_stake_account.key(),
        &ctx.accounts.reserve_stake.key(),
        &ctx.accounts.user_fusol_account.key(),
        &ctx.accounts.maintenance_vault.key(), // manager fee account
        &ctx.accounts.maintenance_vault.key(), // referrer (fee 0; same account)
        &ctx.accounts.fusol_mint.key(),
        &ctx.accounts.token_program.key(),
    );
    invoke_signed(
        &ix,
        &[
            ctx.accounts.stake_pool.to_account_info(),
            ctx.accounts.validator_list.to_account_info(),
            ctx.accounts.deposit_authority.to_account_info(),
            ctx.accounts.pool_withdraw_authority.to_account_info(),
            ctx.accounts.user_stake_account.to_account_info(),
            ctx.accounts.validator_stake_account.to_account_info(),
            ctx.accounts.reserve_stake.to_account_info(),
            ctx.accounts.user_fusol_account.to_account_info(),
            ctx.accounts.maintenance_vault.to_account_info(), // covers fee + referrer metas
            ctx.accounts.fusol_mint.to_account_info(),
            ctx.accounts.clock.to_account_info(),
            ctx.accounts.stake_history.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
            ctx.accounts.stake_program.to_account_info(),
            ctx.accounts.stake_pool_program.to_account_info(),
        ],
        &[&[DEPOSIT_AUTHORITY_SEED, &[ctx.accounts.config.deposit_authority_bump]]],
    )?;

    emit_cpi!(crate::events::PoolDeposit {
        depositor: ctx.accounts.depositor.key(),
        kind: crate::events::DEPOSIT_KIND_STAKE,
        vote_account: ctx.accounts.vote_account.key(),
        lamports: deposit_lamports,
    });
    Ok(())
}
