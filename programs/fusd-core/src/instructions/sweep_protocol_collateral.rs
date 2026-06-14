//! `sweep_protocol_collateral` — governance recovers the retained protocol-owned (un-homed) collateral
//! (the recapitalization sweep).
//!
//! `Market.protocol_collateral` is the post-RP remainder of tier-3/4 liquidations that had no
//! redistribution recipient: collateral the protocol owns outright, sitting in the vault backing NO
//! position (the offsetting asset for realized `bad_debt`, or — on a buffer-only absorb — recovered
//! revenue). This lets the governance authority move it to a recipient so it can be deployed against
//! the loss (sold off-chain → buy back & burn the circulating-unbacked fUSD).
//!
//! **Bounded by construction:** it can only move `protocol_collateral` — never position-backing
//! `total_collateral`, the borrower-owed `total_coll_surplus`, or the redemption-fee
//! `surplus_collateral` — so the 4-term vault invariant is preserved. It touches NO live position and
//! reads no oracle, so it needs no shutdown/staleness gate; `protocol_collateral` is already-owned
//! protocol property. `bad_debt` is left intact as the on-chain record of the loss (proof-of-reserves);
//! the recovery is an off-chain governance action this sweep enables.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use crate::constants::{GOV_GATE_SEED, MARKET_SEED};
use crate::errors::FusdError;
use crate::state::{GovernanceGate, Market};

#[event_cpi]
#[derive(Accounts)]
pub struct SweepProtocolCollateral<'info> {
    /// MUST equal `gov_gate.inbound_authority`.
    pub authority: Signer<'info>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,

    #[account(seeds = [GOV_GATE_SEED], bump = gov_gate.bump)]
    pub gov_gate: Account<'info, GovernanceGate>,

    #[account(mut, address = market.collateral_vault)]
    pub market_coll_vault: Account<'info, TokenAccount>,

    /// Where the recovered collateral is sent (governance recapitalization account). Must not
    /// alias the market vault: a governance fat-finger passing the vault itself would make the
    /// transfer a no-op self-transfer while the counter is debited — silent value-stranding.
    #[account(
        mut,
        token::mint = collateral_mint,
        constraint = recipient.key() != market.collateral_vault @ FusdError::InvalidRecipient,
    )]
    pub recipient: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<SweepProtocolCollateral>, amount: u64) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.gov_gate.inbound_authority,
        FusdError::Unauthorized
    );
    require!(amount > 0, FusdError::ZeroAmount);
    require!(
        amount <= ctx.accounts.market.protocol_collateral,
        FusdError::InsufficientProtocolCollateral
    );

    // Debit the tracked protocol collateral FIRST (checked), then move exactly that out of the vault.
    ctx.accounts.market.protocol_collateral -= amount;

    let coll_mint = ctx.accounts.collateral_mint.key();
    let m_bump = ctx.accounts.market.bump;
    let signer: &[&[&[u8]]] = &[&[MARKET_SEED, coll_mint.as_ref(), &[m_bump]]];
    token::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.market_coll_vault.to_account_info(),
                to: ctx.accounts.recipient.to_account_info(),
                authority: ctx.accounts.market.to_account_info(),
            },
            signer,
        ),
        amount,
    )?;

    emit_cpi!(crate::events::ProtocolCollateralSwept {
        collateral_mint: coll_mint,
        recipient: ctx.accounts.recipient.key(),
        amount,
        protocol_collateral_remaining: ctx.accounts.market.protocol_collateral,
        bad_debt: ctx.accounts.market.bad_debt,
    });
    crate::reconcile::assert_collateral_vault_sufficiency(
        &mut ctx.accounts.market_coll_vault,
        &ctx.accounts.market,
    )?;
    Ok(())
}
