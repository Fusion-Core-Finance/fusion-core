//! `settle_bad_debt` — governance burns fUSD to retire realized un-homed bad debt (the
//! recapitalization settlement — the on-chain half of the recovery loop).
//!
//! After `sweep_protocol_collateral` hands the un-homed collateral to governance (sold off-chain to buy
//! back the circulating-unbacked fUSD), this burns that fUSD and reduces `Market.bad_debt` by the same
//! amount. Both sides of the supply invariant
//! (`circulating == agg_recorded_debt − unminted_interest + bad_debt`) drop by `amount`, so it stays
//! exact. It can never reduce `bad_debt` below 0 (checked), and burns from the authority's own fUSD —
//! it cannot touch anyone else's balance.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Mint, Token, TokenAccount};

use crate::constants::{FUSD_MINT_SEED, GOV_GATE_SEED, MARKET_SEED};
use crate::errors::FusdError;
use crate::state::{GovernanceGate, Market};

#[event_cpi]
#[derive(Accounts)]
pub struct SettleBadDebt<'info> {
    /// MUST equal `gov_gate.inbound_authority`. The fUSD is burned from this signer's ATA.
    pub authority: Signer<'info>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,

    #[account(seeds = [GOV_GATE_SEED], bump = gov_gate.bump)]
    pub gov_gate: Account<'info, GovernanceGate>,

    #[account(mut, seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Account<'info, Mint>,

    #[account(mut, token::mint = fusd_mint, token::authority = authority)]
    pub authority_fusd_ata: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<SettleBadDebt>, amount: u64) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.gov_gate.inbound_authority,
        FusdError::Unauthorized
    );
    require!(amount > 0, FusdError::ZeroAmount);
    require!(
        (amount as u128) <= ctx.accounts.market.bad_debt,
        FusdError::InsufficientProtocolCollateral
    );

    // Burn the recovered fUSD from the authority, then retire that much realized bad debt. Burn FIRST
    // (fails closed if the authority lacks the balance) so `bad_debt` only drops against fUSD that has
    // actually left circulation.
    token::burn(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Burn {
                mint: ctx.accounts.fusd_mint.to_account_info(),
                from: ctx.accounts.authority_fusd_ata.to_account_info(),
                authority: ctx.accounts.authority.to_account_info(),
            },
        ),
        amount,
    )?;
    ctx.accounts.market.bad_debt -= amount as u128;

    emit_cpi!(crate::events::BadDebtSettled {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        amount,
        bad_debt_remaining: ctx.accounts.market.bad_debt,
    });
    Ok(())
}
