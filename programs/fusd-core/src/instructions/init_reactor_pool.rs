use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};
use fusd_math::reactor_pool::DECIMAL_PRECISION;

use crate::constants::{
    CONFIG_SEED, ESS_SEED, FUSD_MINT_SEED, LIQ_INFRA_REACTOR_POOL, MARKET_SEED,
    REACTOR_COLL_VAULT_SEED, REACTOR_FUSD_VAULT_SEED, REACTOR_POOL_SEED,
};
use crate::errors::FusdError;
use crate::state::{EpochToScaleToSum, Market, ProtocolConfig, ReactorPool};

/// Governance: create a market's Reactor Pool (deposit + collateral vaults + the bounded
/// epoch→scale→sum grid). One per market. fusion-docs.md.
#[derive(Accounts)]
pub struct InitReactorPool<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,

    pub collateral_mint: Box<Account<'info, Mint>>,

    #[account(seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Box<Account<'info, Mint>>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,

    #[account(
        init,
        payer = authority,
        space = ReactorPool::SPACE,
        seeds = [REACTOR_POOL_SEED, collateral_mint.key().as_ref()],
        bump,
    )]
    pub reactor_pool: Box<Account<'info, ReactorPool>>,

    #[account(
        init,
        payer = authority,
        space = EpochToScaleToSum::SPACE,
        seeds = [ESS_SEED, collateral_mint.key().as_ref()],
        bump,
    )]
    pub epoch_to_scale_to_sum: AccountLoader<'info, EpochToScaleToSum>,

    #[account(
        init,
        payer = authority,
        seeds = [REACTOR_FUSD_VAULT_SEED, collateral_mint.key().as_ref()],
        bump,
        token::mint = fusd_mint,
        token::authority = reactor_pool,
    )]
    pub reactor_fusd_vault: Box<Account<'info, TokenAccount>>,

    #[account(
        init,
        payer = authority,
        seeds = [REACTOR_COLL_VAULT_SEED, collateral_mint.key().as_ref()],
        bump,
        token::mint = collateral_mint,
        token::authority = reactor_pool,
    )]
    pub reactor_coll_vault: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

pub fn handler(ctx: Context<InitReactorPool>) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );

    // L-02 borrow gate: record that the ReactorPool now exists. Unconditional OR — on a legacy
    // 0-flag market this starts converging it onto the gated encoding.
    ctx.accounts.market.liq_infra_flags |= LIQ_INFRA_REACTOR_POOL;

    // Zero-initialize the grid (all S sums start at 0) + set its discriminator.
    ctx.accounts.epoch_to_scale_to_sum.load_init()?;

    let rp = &mut ctx.accounts.reactor_pool;
    rp.collateral_mint = ctx.accounts.collateral_mint.key();
    rp.fusd_vault = ctx.accounts.reactor_fusd_vault.key();
    rp.coll_vault = ctx.accounts.reactor_coll_vault.key();
    rp.epoch_to_scale_to_sum = ctx.accounts.epoch_to_scale_to_sum.key();
    rp.p = DECIMAL_PRECISION;
    rp.epoch = 0;
    rp.scale = 0;
    rp.total_deposits = 0;
    rp.last_coll_error = 0;
    rp.last_loss_error = 0;
    rp.bump = ctx.bumps.reactor_pool;
    rp._reserved = [0u8; 64];
    Ok(())
}
