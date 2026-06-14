use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Mint, Token, TokenAccount};

use crate::accrual;
use crate::cdp;
use crate::constants::{
    FUSD_MINT_SEED, MARKET_SEED, POSITION_SEED, RATELIMIT_WINDOW_SECS, REDEMPTION_BITMAP_SEED,
};
use crate::errors::FusdError;
use crate::state::{Market, Position, RedemptionBitmap};

/// Burn fUSD to repay a position's debt. No price needed (repaying only reduces risk).
/// `amount` is capped at the position's current debt.
#[event_cpi]
#[derive(Accounts)]
pub struct Repay<'info> {
    pub owner: Signer<'info>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,

    #[account(
        mut,
        seeds = [POSITION_SEED, collateral_mint.key().as_ref(), owner.key().as_ref()],
        bump = position.bump,
        has_one = owner,
    )]
    pub position: Account<'info, Position>,

    #[account(mut, seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Account<'info, Mint>,

    #[account(mut, token::mint = fusd_mint, token::authority = owner)]
    pub owner_fusd_ata: Account<'info, TokenAccount>,

    #[account(mut, seeds = [REDEMPTION_BITMAP_SEED, collateral_mint.key().as_ref()], bump)]
    pub redemption_bitmap: AccountLoader<'info, RedemptionBitmap>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<Repay>, amount: u64) -> Result<()> {
    require!(amount > 0, FusdError::ZeroAmount);
    let now = Clock::get()?.unix_timestamp;
    let art_before = ctx.accounts.position.recorded_debt;
    let old_weighted = accrual::weighted(&ctx.accounts.position)?;
    // Accrue the market, then realize this position's interest + any pending tier-2 redistribution
    // so we repay against its current present debt.
    accrual::accrue(&mut ctx.accounts.market, now)?;
    accrual::realize(&ctx.accounts.market, &mut ctx.accounts.position, now)?;

    let current_debt = ctx.accounts.position.recorded_debt;
    if current_debt == 0 {
        // Nothing to repay; the realize above already brought the position current.
        crate::redist::set_stake(&mut ctx.accounts.market, &mut ctx.accounts.position)?;
        return Ok(());
    }

    // Recorded debt is in fUSD-native units; repaying burns exactly `repay_amount` of it. Repaying the
    // full debt zeroes `recorded_debt` (no dust — no `art*rate` normalization remains).
    let repay_amount = (amount as u128).min(current_debt) as u64;

    // Dust floor: a partial repay must not leave the position below `min_debt` — repay fully
    // (to 0) or stay at/above the floor. Checked before the burn, so a rejected repay burns nothing.
    let resulting_debt = current_debt - repay_amount as u128;
    let min_debt = ctx.accounts.market.min_debt;
    require!(
        min_debt == 0 || resulting_debt == 0 || resulting_debt >= min_debt as u128,
        FusdError::DebtBelowMinimum
    );

    token::burn(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Burn {
                mint: ctx.accounts.fusd_mint.to_account_info(),
                from: ctx.accounts.owner_fusd_ata.to_account_info(),
                authority: ctx.accounts.owner.to_account_info(),
            },
        ),
        repay_amount,
    )?;

    ctx.accounts.position.recorded_debt = current_debt - repay_amount as u128;
    ctx.accounts.market.agg_recorded_debt = ctx
        .accounts
        .market
        .agg_recorded_debt
        .checked_sub(repay_amount as u128)
        .ok_or(FusdError::MathOverflow)?;
    accrual::reweight(&mut ctx.accounts.market, &ctx.accounts.position, old_weighted)?;
    crate::redist::set_stake(&mut ctx.accounts.market, &mut ctx.accounts.position)?;

    // Net-outflow rate limiter: repaying restores bucket capacity by the burned amount (the
    // "net" in net-outflow). Disabled when `rl_cap == 0`. Never fails.
    if ctx.accounts.market.rl_cap > 0 {
        let m = &ctx.accounts.market;
        let new_accrued = cdp::ratelimit_restore(
            m.rl_accrued,
            m.rl_last_update,
            now,
            m.rl_cap,
            RATELIMIT_WINDOW_SECS,
            repay_amount,
        );
        ctx.accounts.market.rl_accrued = new_accrued;
        ctx.accounts.market.rl_last_update = now;
    }

    // Reconcile redemption rate-bucket membership (leaves on full repay).
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
        op: crate::events::POSITION_OP_REPAY,
        amount: repay_amount,
        ink: ctx.accounts.position.ink,
        recorded_debt: ctx.accounts.position.recorded_debt,
        user_rate_bps: ctx.accounts.position.user_rate_bps,
        bucket: ctx.accounts.position.bucket,
    });
    Ok(())
}
