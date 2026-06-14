use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use crate::accrual;
use crate::constants::{MARKET_SEED, POSITION_SEED, REDEMPTION_BITMAP_SEED};
use crate::errors::FusdError;
use crate::state::{Market, Position, RedemptionBitmap};

/// Deposit collateral into a position. No price needed (adding collateral only reduces risk).
#[event_cpi]
#[derive(Accounts)]
pub struct Deposit<'info> {
    /// `mut`: the under-bond top-up below debits lamports from the owner via a system transfer,
    /// which requires the account writable even when the owner is not the tx fee payer.
    #[account(mut)]
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

    #[account(mut, token::mint = collateral_mint, token::authority = owner)]
    pub owner_collateral_ata: Account<'info, TokenAccount>,

    #[account(mut)]
    pub collateral_vault: Account<'info, TokenAccount>,

    #[account(mut, seeds = [REDEMPTION_BITMAP_SEED, collateral_mint.key().as_ref()], bump)]
    pub redemption_bitmap: AccountLoader<'info, RedemptionBitmap>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

pub fn handler(ctx: Context<Deposit>, amount: u64) -> Result<()> {
    require!(amount > 0, FusdError::ZeroAmount);
    let now = Clock::get()?.unix_timestamp;
    let art_before = ctx.accounts.position.recorded_debt;
    let old_weighted = accrual::weighted(&ctx.accounts.position)?;

    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.owner_collateral_ata.to_account_info(),
                to: ctx.accounts.collateral_vault.to_account_info(),
                authority: ctx.accounts.owner.to_account_info(),
            },
        ),
        amount,
    )?;

    // A deposit is a position touch: accrue the market and realize this position's interest + any
    // pending tier-2 redistribution before adjusting it. `realize` (not a combined touch)
    // since we recompute the stake exactly once below, after the `ink` change.
    accrual::accrue(&mut ctx.accounts.market, now)?;
    accrual::realize(&ctx.accounts.market, &mut ctx.accounts.position, now)?;

    ctx.accounts.position.ink = ctx
        .accounts
        .position
        .ink
        .checked_add(amount)
        .ok_or(FusdError::MathOverflow)?;
    ctx.accounts.market.total_collateral = ctx
        .accounts
        .market
        .total_collateral
        .checked_add(amount as u128)
        .ok_or(FusdError::MathOverflow)?;
    // Interest realized above may have grown `recorded_debt`; fold the weighted-sum delta and the
    // stake (after the `ink` change).
    accrual::reweight(&mut ctx.accounts.market, &ctx.accounts.position, old_weighted)?;
    crate::redist::set_stake(&mut ctx.accounts.market, &mut ctx.accounts.position)?;

    // Re-post the SOL liquidation bond if the position is under-bonded — e.g. a position reused
    // after a liquidation that consumed its bond, or one opened before the market set a reserve.
    // Tops up to the market's CURRENT bond; a position already at/above it is untouched (so a
    // governance change never silently lowers an existing bond, and only an active deposit — an
    // "adjustment" — can raise it). Without this, a reused position would borrow bond-free.
    let market_reserve = ctx.accounts.market.reserve_lamports;
    let posted = ctx.accounts.position.reserve_lamports;
    if market_reserve > posted {
        anchor_lang::system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                anchor_lang::system_program::Transfer {
                    from: ctx.accounts.owner.to_account_info(),
                    to: ctx.accounts.position.to_account_info(),
                },
            ),
            market_reserve - posted,
        )?;
        ctx.accounts.position.reserve_lamports = market_reserve;
    }

    // Reconcile redemption rate-bucket membership (a `realize` may have taken art 0→+).
    {
        let mut bm = ctx.accounts.redemption_bitmap.load_mut()?;
        crate::bucket::reconcile(
            &mut bm,
            &ctx.accounts.market,
            &mut ctx.accounts.position,
            art_before,
        )?;
    }

    emit_cpi!(crate::events::PositionUpdated {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        owner: ctx.accounts.position.owner,
        op: crate::events::POSITION_OP_DEPOSIT,
        amount: amount,
        ink: ctx.accounts.position.ink,
        recorded_debt: ctx.accounts.position.recorded_debt,
        user_rate_bps: ctx.accounts.position.user_rate_bps,
        bucket: ctx.accounts.position.bucket,
    });
    crate::reconcile::assert_collateral_vault_sufficiency(
        &mut ctx.accounts.collateral_vault,
        &ctx.accounts.market,
    )?;
    Ok(())
}
