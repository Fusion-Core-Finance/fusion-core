//! Permissionless: deposit SOL through the controller's deposit authority. Lamports enter the
//! pool reserve and fuSOL mints to the depositor immediately at the finalized pool rate (all
//! conversion math is upstream's — the controller never re-computes NAV). The new backing is
//! undirected until an eligible preference is snapshotted; the next epoch plan allocates all
//! reserve surplus above the operational target.
//!
//! Slippage: v1 forwards slippage-free (`DepositSol`, not the WithSlippage variant) — the pool
//! rate can only move between the user's read and execution by a finalization crank, and the
//! fixed 5 bps deposit fee is the constant, known cost. (Deviation-free per the plan.)

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_spl::token::{Mint, Token, TokenAccount};

use crate::constants::{CONTROLLER_SEED, DEPOSIT_AUTHORITY_SEED, FUSION_STAKE_POOL_PROGRAM_ID};
use crate::errors::ControllerError;
use crate::spl_cpi;
use crate::state::ControllerConfig;

#[event_cpi]
#[derive(Accounts)]
pub struct DepositSol<'info> {
    /// The lamports source (`[s,w]` in the CPI).
    #[account(mut)]
    pub depositor: Signer<'info>,

    #[account(
        seeds = [CONTROLLER_SEED],
        bump = config.bump,
        constraint = config.sealed @ ControllerError::PoolNotInitialized,
    )]
    pub config: Box<Account<'info, ControllerConfig>>,

    /// CHECK: pinned to the recorded pool address + FORK-program owner.
    #[account(
        mut,
        address = config.stake_pool @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub stake_pool: UncheckedAccount<'info>,

    /// CHECK: the recorded stake-pool withdraw authority (re-derived upstream).
    #[account(address = config.pool_withdraw_authority @ ControllerError::AddressMismatch)]
    pub pool_withdraw_authority: UncheckedAccount<'info>,

    /// CHECK: the recorded reserve stake account (receives the lamports).
    #[account(mut, address = config.reserve_stake @ ControllerError::AddressMismatch)]
    pub reserve_stake: UncheckedAccount<'info>,

    #[account(mut, address = config.fusol_mint @ ControllerError::AddressMismatch)]
    pub fusol_mint: Box<Account<'info, Mint>>,

    /// Receives the minted fuSOL.
    #[account(mut, token::mint = fusol_mint)]
    pub user_fusol_account: Box<Account<'info, TokenAccount>>,

    /// The maintenance vault — the pool's manager fee account; ALSO passed as the referrer
    /// slot (referral fee is 0, upstream only requires a valid pool-token account there).
    #[account(mut, address = config.maintenance_vault @ ControllerError::AddressMismatch)]
    pub maintenance_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: `[b"deposit_authority"]` PDA — co-signs the CPI (the pool's sol deposit
    /// authority; the trailing signer account of `DepositSol`).
    #[account(seeds = [DEPOSIT_AUTHORITY_SEED], bump = config.deposit_authority_bump)]
    pub deposit_authority: UncheckedAccount<'info>,

    /// CHECK: the pinned stake-pool FORK program.
    #[account(address = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch)]
    pub stake_pool_program: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handler(ctx: Context<DepositSol>, lamports: u64) -> Result<()> {
    require!(lamports > 0, ControllerError::ZeroAmount);

    let ix = spl_cpi::deposit_sol(
        &ctx.accounts.stake_pool.key(),
        &ctx.accounts.pool_withdraw_authority.key(),
        &ctx.accounts.reserve_stake.key(),
        &ctx.accounts.depositor.key(),
        &ctx.accounts.user_fusol_account.key(),
        &ctx.accounts.maintenance_vault.key(), // manager fee account
        &ctx.accounts.maintenance_vault.key(), // referrer (fee 0; same account)
        &ctx.accounts.fusol_mint.key(),
        &ctx.accounts.token_program.key(),
        &ctx.accounts.deposit_authority.key(),
        lamports,
    );
    invoke_signed(
        &ix,
        &[
            ctx.accounts.stake_pool.to_account_info(),
            ctx.accounts.pool_withdraw_authority.to_account_info(),
            ctx.accounts.reserve_stake.to_account_info(),
            ctx.accounts.depositor.to_account_info(),
            ctx.accounts.user_fusol_account.to_account_info(),
            ctx.accounts.maintenance_vault.to_account_info(), // covers fee + referrer metas
            ctx.accounts.fusol_mint.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
            ctx.accounts.deposit_authority.to_account_info(),
            ctx.accounts.stake_pool_program.to_account_info(),
        ],
        &[&[DEPOSIT_AUTHORITY_SEED, &[ctx.accounts.config.deposit_authority_bump]]],
    )?;

    emit_cpi!(crate::events::PoolDeposit {
        depositor: ctx.accounts.depositor.key(),
        kind: crate::events::DEPOSIT_KIND_SOL,
        vote_account: Pubkey::default(),
        lamports,
    });
    Ok(())
}
