use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};

use crate::constants::{
    BUFFER_FUSD_VAULT_SEED, BUFFER_SEED, CONFIG_SEED, FUSD_MINT_SEED, LIQ_INFRA_INSURANCE_BUFFER,
    MARKET_SEED,
};
use crate::errors::FusdError;
use crate::state::{InsuranceBuffer, Market, ProtocolConfig};

/// Governance: create a market's **insurance buffer** — an fUSD reserve vault that is the third
/// liquidation loss-absorption tier (RP → redistribution → buffer → un-homed; fusion-docs.md).
/// One per market. The buffer starts EMPTY (funded only via `fund_buffer` from realized fees);
/// an empty buffer is safe because the launch posture bounds exposure (small ceilings, RP-coverage
/// requirements, SCR shutdown, the net-issuance limiter, conservative params).
#[derive(Accounts)]
pub struct InitInsuranceBuffer<'info> {
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
        space = InsuranceBuffer::SPACE,
        seeds = [BUFFER_SEED, collateral_mint.key().as_ref()],
        bump,
    )]
    pub insurance_buffer: Box<Account<'info, InsuranceBuffer>>,

    #[account(
        init,
        payer = authority,
        seeds = [BUFFER_FUSD_VAULT_SEED, collateral_mint.key().as_ref()],
        bump,
        token::mint = fusd_mint,
        token::authority = insurance_buffer,
    )]
    pub buffer_fusd_vault: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

pub fn handler(ctx: Context<InitInsuranceBuffer>) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );

    // L-02 borrow gate: record that the insurance buffer now exists. Unconditional OR — on a
    // legacy 0-flag market this starts converging it onto the gated encoding.
    ctx.accounts.market.liq_infra_flags |= LIQ_INFRA_INSURANCE_BUFFER;

    let b = &mut ctx.accounts.insurance_buffer;
    b.collateral_mint = ctx.accounts.collateral_mint.key();
    b.fusd_vault = ctx.accounts.buffer_fusd_vault.key();
    b.total_funded = 0;
    b.total_absorbed = 0;
    b.bump = ctx.bumps.insurance_buffer;
    b._reserved = [0u8; 64];
    Ok(())
}
