//! `withdraw_surplus` — governance withdraws accrued redemption-fee surplus collateral.
//!
//! `Market.surplus_collateral` accumulates the flat redemption fee skimmed on every `redeem` (held in
//! the collateral vault, backing no position — protocol revenue). This lets the governance authority
//! (the gate's `inbound_authority`, the launch multisig) move it to a recipient token account.
//!
//! **Bounded by construction:** it can only ever move `surplus_collateral` — never position-backing
//! `total_collateral`, never the borrower-owed `total_coll_surplus`, never the un-homed
//! `protocol_collateral` — so the vault invariant
//! `vault == total_collateral + surplus_collateral + total_coll_surplus + protocol_collateral`
//! is preserved (both the vault balance and `surplus_collateral` drop by exactly `amount`). Not
//! shutdown-gated: redemption fees are revenue and recoverable on a live market.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use crate::constants::{GOV_GATE_SEED, MARKET_SEED};
use crate::errors::FusdError;
use crate::state::{GovernanceGate, Market};

#[event_cpi]
#[derive(Accounts)]
pub struct WithdrawSurplus<'info> {
    /// MUST equal `gov_gate.inbound_authority`.
    pub authority: Signer<'info>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,

    #[account(seeds = [GOV_GATE_SEED], bump = gov_gate.bump)]
    pub gov_gate: Account<'info, GovernanceGate>,

    #[account(mut, address = market.collateral_vault)]
    pub market_coll_vault: Account<'info, TokenAccount>,

    /// Where the surplus collateral is sent (governance-chosen treasury). Must not alias the
    /// market vault: a governance fat-finger passing the vault itself would make the transfer a
    /// no-op self-transfer while the counter is debited — silent value-stranding.
    #[account(
        mut,
        token::mint = collateral_mint,
        constraint = recipient.key() != market.collateral_vault @ FusdError::InvalidRecipient,
    )]
    pub recipient: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<WithdrawSurplus>, amount: u64) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.gov_gate.inbound_authority,
        FusdError::Unauthorized
    );
    require!(amount > 0, FusdError::ZeroAmount);
    require!(
        amount <= ctx.accounts.market.surplus_collateral,
        FusdError::InsufficientProtocolCollateral
    );

    // Debit the tracked surplus FIRST (checked), then move exactly that out of the vault.
    ctx.accounts.market.surplus_collateral -= amount;

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

    emit_cpi!(crate::events::SurplusWithdrawn {
        collateral_mint: coll_mint,
        recipient: ctx.accounts.recipient.key(),
        amount,
        surplus_remaining: ctx.accounts.market.surplus_collateral,
    });
    crate::reconcile::assert_collateral_vault_sufficiency(
        &mut ctx.accounts.market_coll_vault,
        &ctx.accounts.market,
    )?;
    Ok(())
}
