use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use crate::constants::{MARKET_SEED, POSITION_SEED};
use crate::errors::FusdError;
use crate::state::{Market, Position};

/// Withdraw a position's liquidation **collateral surplus** — the collateral a collared liquidation
/// returned to the borrower (`Position.coll_surplus`; fusion-docs.md). Owner-only. The surplus has
/// been sitting in the market collateral vault (counted in `Market.total_coll_surplus`, NOT in
/// `total_collateral` and NOT backing any position); this moves it to the owner and zeroes the claim.
///
/// Always safe — it touches no debt and removes collateral that was never backing anything, so it
/// needs no price/health check and is allowed even in shutdown (it's the owner's collateral).
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimCollSurplus<'info> {
    pub owner: Signer<'info>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(
        mut,
        seeds = [MARKET_SEED, collateral_mint.key().as_ref()],
        bump = market.bump,
        has_one = collateral_vault,
    )]
    pub market: Account<'info, Market>,

    #[account(
        mut,
        seeds = [POSITION_SEED, collateral_mint.key().as_ref(), owner.key().as_ref()],
        bump = position.bump,
        has_one = owner,
    )]
    pub position: Account<'info, Position>,

    #[account(mut)]
    pub collateral_vault: Account<'info, TokenAccount>,

    #[account(mut, token::mint = collateral_mint, token::authority = owner)]
    pub owner_collateral_ata: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<ClaimCollSurplus>) -> Result<()> {
    let amount = ctx.accounts.position.coll_surplus;
    require!(amount > 0, FusdError::NoCollateralSurplus);

    // Transfer the surplus out of the escrow, signed by the market PDA.
    let coll_key = ctx.accounts.collateral_mint.key();
    let mbump = ctx.accounts.market.bump;
    let signer: &[&[&[u8]]] = &[&[MARKET_SEED, coll_key.as_ref(), &[mbump]]];
    token::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.collateral_vault.to_account_info(),
                to: ctx.accounts.owner_collateral_ata.to_account_info(),
                authority: ctx.accounts.market.to_account_info(),
            },
            signer,
        ),
        amount,
    )?;

    ctx.accounts.position.coll_surplus = 0;
    ctx.accounts.market.total_coll_surplus = ctx
        .accounts
        .market
        .total_coll_surplus
        .checked_sub(amount)
        .ok_or(FusdError::MathOverflow)?;

    emit_cpi!(crate::events::CollSurplusClaimed {
        collateral_mint: coll_key,
        owner: ctx.accounts.owner.key(),
        amount,
    });
    crate::reconcile::assert_collateral_vault_sufficiency(
        &mut ctx.accounts.collateral_vault,
        &ctx.accounts.market,
    )?;
    Ok(())
}
