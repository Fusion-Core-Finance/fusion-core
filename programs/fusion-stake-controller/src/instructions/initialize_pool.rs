//! One-time: CPI the stake-pool `Initialize` with the controller PDAs and the FIXED fee set,
//! then seal the controller forever (`sealed = true` — this path can never run again, and no
//! fee/authority setter exists anywhere in this program).
//!
//! Authority graph established here (spec §4.1):
//! - manager  = `[b"pool_authority"]` PDA (signs the CPI)
//! - staker   = `[b"pool_authority"]` PDA (same key — one narrow signer for both roles)
//! - stake + SOL deposit authority = `[b"deposit_authority"]` PDA (the optional 10th
//!   `Initialize` account sets BOTH at once; deposits flow through this controller only)
//! - manager fee account = the maintenance vault (fuSOL token account, authority =
//!   `[b"maintenance"]` PDA), so every pool fee funds permissionless cranks
//! - `sol_withdraw_authority` is NEVER set — withdrawals stay direct and ungated.
//!
//! Every passed account must equal its ControllerConfig-recorded address, and the fuSOL mint
//! must be legacy SPL, 9 decimals, freeze authority None, zero supply, mint authority = the
//! pool withdraw-authority PDA (spec §12.2 token requirements).

use anchor_lang::prelude::*;
use anchor_lang::solana_program::program::invoke_signed;
use anchor_lang::solana_program::program_option::COption;
use anchor_spl::token::{Mint, Token, TokenAccount};

use crate::constants::{
    CONTROLLER_SEED, DEPOSIT_AUTHORITY_SEED, EPOCH_MAINTENANCE_FEE_DENOMINATOR,
    EPOCH_MAINTENANCE_FEE_NUMERATOR, FEE_BPS_DENOMINATOR, FUSION_STAKE_POOL_PROGRAM_ID,
    MAINTENANCE_AUTHORITY_SEED, MAX_VALIDATORS, POOL_AUTHORITY_SEED, REFERRAL_FEE_PERCENT,
    SOL_DEPOSIT_FEE_BPS, SOL_WITHDRAW_FEE_BPS,
};
use crate::errors::ControllerError;
use crate::spl_cpi;
use crate::state::ControllerConfig;

#[event_cpi]
#[derive(Accounts)]
pub struct InitializePool<'info> {
    /// Transaction fee payer only; nothing is recorded about it.
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(
        mut,
        seeds = [CONTROLLER_SEED],
        bump = config.bump,
    )]
    pub config: Box<Account<'info, ControllerConfig>>,

    /// CHECK: the pre-created, still-uninitialized `StakePool` account — pinned to the
    /// recorded address and to the FORK program as owner; the CPI initializes it.
    #[account(
        mut,
        address = config.stake_pool @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub stake_pool: UncheckedAccount<'info>,

    /// CHECK: `[b"pool_authority"]` PDA — becomes the pool's manager AND staker; signs the
    /// `Initialize` CPI via `invoke_signed`.
    #[account(seeds = [POOL_AUTHORITY_SEED], bump = config.pool_authority_bump)]
    pub pool_authority: UncheckedAccount<'info>,

    /// CHECK: `[b"deposit_authority"]` PDA — set as BOTH the stake and SOL deposit authority.
    #[account(seeds = [DEPOSIT_AUTHORITY_SEED], bump = config.deposit_authority_bump)]
    pub deposit_authority: UncheckedAccount<'info>,

    /// CHECK: the stake-pool program's withdraw-authority PDA (recorded at genesis; the
    /// stake-pool program re-derives and enforces it in the CPI).
    #[account(address = config.pool_withdraw_authority @ ControllerError::AddressMismatch)]
    pub pool_withdraw_authority: UncheckedAccount<'info>,

    /// CHECK: the pre-created, uninitialized `ValidatorList` — pinned address + owner; its
    /// size must yield exactly `MAX_VALIDATORS` (the CPI enforces this).
    #[account(
        mut,
        address = config.validator_list @ ControllerError::AddressMismatch,
        owner = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch,
    )]
    pub validator_list: UncheckedAccount<'info>,

    /// CHECK: the reserve stake account (initialized stake account, staker + withdrawer = the
    /// pool withdraw authority; the CPI validates its state).
    #[account(address = config.reserve_stake @ ControllerError::AddressMismatch)]
    pub reserve_stake: UncheckedAccount<'info>,

    /// The fuSOL mint. `Account<Mint>` under `anchor_spl::token` enforces LEGACY SPL ownership
    /// (never Token-2022); decimals / freeze / authority / supply checks live in the handler.
    #[account(mut, address = config.fusol_mint @ ControllerError::AddressMismatch)]
    pub fusol_mint: Box<Account<'info, Mint>>,

    /// The maintenance vault — validated below as a fuSOL account owned by the maintenance
    /// PDA, then installed as the pool's manager fee account.
    #[account(mut, address = config.maintenance_vault @ ControllerError::AddressMismatch)]
    pub maintenance_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: `[b"maintenance"]` PDA — must be the vault's token authority.
    #[account(seeds = [MAINTENANCE_AUTHORITY_SEED], bump = config.maintenance_authority_bump)]
    pub maintenance_authority: UncheckedAccount<'info>,

    /// CHECK: the pinned stake-pool FORK program (never caller-supplied — a spoofed program
    /// would receive the pool-authority PDA's signature).
    #[account(address = FUSION_STAKE_POOL_PROGRAM_ID @ ControllerError::AddressMismatch)]
    pub stake_pool_program: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<InitializePool>) -> Result<()> {
    require!(!ctx.accounts.config.sealed, ControllerError::AlreadySealed);

    // fuSOL mint requirements (spec §12.2): 9 decimals, no freeze authority (irreversible
    // censorship-resistance), zero supply pre-pool, mint authority = the pool
    // withdraw-authority PDA (only the stake-pool program can ever mint).
    let mint = &ctx.accounts.fusol_mint;
    require!(mint.decimals == 9, ControllerError::InvalidFusolMint);
    require!(mint.freeze_authority.is_none(), ControllerError::InvalidFusolMint);
    require!(mint.supply == 0, ControllerError::InvalidFusolMint);
    require!(
        mint.mint_authority == COption::Some(ctx.accounts.pool_withdraw_authority.key()),
        ControllerError::InvalidFusolMint
    );

    // Maintenance vault: holds fuSOL, token authority = the maintenance PDA (the only signer
    // that can ever move its shares, and only through bounded crank rewards). No delegate or
    // close authority may exist — either could leak vault shares outside the reward path.
    let vault = &ctx.accounts.maintenance_vault;
    require!(vault.mint == ctx.accounts.fusol_mint.key(), ControllerError::InvalidMaintenanceVault);
    require!(
        vault.owner == ctx.accounts.maintenance_authority.key(),
        ControllerError::InvalidMaintenanceVault
    );
    require!(vault.delegate.is_none(), ControllerError::InvalidMaintenanceVault);
    require!(vault.close_authority.is_none(), ControllerError::InvalidMaintenanceVault);

    // The one-time Initialize CPI with the FIXED fee set (no setter exists after this).
    let ix = spl_cpi::initialize(
        &ctx.accounts.stake_pool.key(),
        &ctx.accounts.pool_authority.key(), // manager
        &ctx.accounts.pool_authority.key(), // staker (same narrow PDA)
        &ctx.accounts.pool_withdraw_authority.key(),
        &ctx.accounts.validator_list.key(),
        &ctx.accounts.reserve_stake.key(),
        &ctx.accounts.fusol_mint.key(),
        &ctx.accounts.maintenance_vault.key(), // manager fee account
        &ctx.accounts.token_program.key(),
        &ctx.accounts.deposit_authority.key(), // sets BOTH deposit authorities
        spl_cpi::Fee {
            denominator: EPOCH_MAINTENANCE_FEE_DENOMINATOR,
            numerator: EPOCH_MAINTENANCE_FEE_NUMERATOR,
        },
        // Withdrawal fee (applies to BOTH stake and SOL withdrawals at Initialize; the
        // per-kind split only exists through the absent SetFee path). 5 bps.
        spl_cpi::Fee { denominator: FEE_BPS_DENOMINATOR, numerator: SOL_WITHDRAW_FEE_BPS },
        // Deposit fee (same: one Initialize field covers stake + SOL deposits). 5 bps.
        spl_cpi::Fee { denominator: FEE_BPS_DENOMINATOR, numerator: SOL_DEPOSIT_FEE_BPS },
        REFERRAL_FEE_PERCENT,
        MAX_VALIDATORS,
    );
    invoke_signed(
        &ix,
        &[
            ctx.accounts.stake_pool.to_account_info(),
            ctx.accounts.pool_authority.to_account_info(), // covers manager + staker metas
            ctx.accounts.pool_withdraw_authority.to_account_info(),
            ctx.accounts.validator_list.to_account_info(),
            ctx.accounts.reserve_stake.to_account_info(),
            ctx.accounts.fusol_mint.to_account_info(),
            ctx.accounts.maintenance_vault.to_account_info(),
            ctx.accounts.token_program.to_account_info(),
            ctx.accounts.deposit_authority.to_account_info(),
            ctx.accounts.stake_pool_program.to_account_info(),
        ],
        &[
            &[POOL_AUTHORITY_SEED, &[ctx.accounts.config.pool_authority_bump]],
            &[DEPOSIT_AUTHORITY_SEED, &[ctx.accounts.config.deposit_authority_bump]],
        ],
    )?;

    // Seal forever: no second Initialize, and (structurally) no fee/authority mutation ever.
    ctx.accounts.config.sealed = true;

    emit_cpi!(crate::events::PoolInitialized {
        stake_pool: ctx.accounts.stake_pool.key(),
        fusol_mint: ctx.accounts.fusol_mint.key(),
        max_validators: MAX_VALIDATORS,
    });
    Ok(())
}
