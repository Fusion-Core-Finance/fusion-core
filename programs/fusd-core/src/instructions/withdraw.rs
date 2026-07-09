use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use crate::accrual;
use crate::cdp;
use crate::constants::{
    MARKET_SEED, MAX_PRICE_STALENESS_SLOTS, POSITION_SEED, REDEMPTION_BITMAP_SEED,
};
use crate::errors::FusdError;
use crate::state::{Market, Position, RedemptionBitmap};

/// Withdraw collateral. If the position still has debt afterward, requires a fresh price
/// and that the position stays at/above its MCR.
#[event_cpi]
#[derive(Accounts)]
pub struct Withdraw<'info> {
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

    #[account(mut, seeds = [REDEMPTION_BITMAP_SEED, collateral_mint.key().as_ref()], bump)]
    pub redemption_bitmap: AccountLoader<'info, RedemptionBitmap>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
    require!(amount > 0, FusdError::ZeroAmount);
    // One Clock fetch reused for both the accrual timestamp and the two staleness-slot checks
    // below (the value is constant within an instruction).
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let slot = clock.slot;
    let art_before = ctx.accounts.position.recorded_debt;
    let old_weighted = accrual::weighted(&ctx.accounts.position)?;
    // Accrue the market, then realize this position's interest + any pending tier-2 redistribution
    // so the health check sees the current present debt.
    accrual::accrue(&mut ctx.accounts.market, now)?;
    accrual::realize(&ctx.accounts.market, &mut ctx.accounts.position, now)?;

    let ink = ctx.accounts.position.ink;
    require!(ink >= amount, FusdError::InsufficientCollateral);
    let new_ink = ink - amount;

    let recorded_debt = ctx.accounts.position.recorded_debt;
    if recorded_debt > 0 {
        let spot = ctx.accounts.market.spot;
        require!(spot > 0, FusdError::OracleUnavailable);
        require!(
            slot.saturating_sub(ctx.accounts.market.spot_updated_slot) <= MAX_PRICE_STALENESS_SLOTS,
            FusdError::StalePrice
        );
        require!(
            cdp::is_healthy(new_ink, recorded_debt, spot, ctx.accounts.market.mcr_bps),
            FusdError::BelowMinCollateralRatio
        );
    }

    // CCR borrow-restriction band: when enabled, block a withdrawal that would leave the
    // market's aggregate TCR below CCR (collateral fleeing a stressed market). Applies to every
    // position (a debt-free position's collateral is a tier-2 redistribution backstop). Fails OPEN
    // on a stale/absent price so a dead oracle can't grief-freeze withdrawals; never expands
    // liquidation. Repay/deposit (de-risking) are never blocked.
    //
    // SKIPPED in a shut-down market (found by the all-levers lever-matrix
    // suite): the band exists to stop new risk entering a LIVE stressed market, but a terminal
    // market's TCR can never recover (borrowing is dead), so an active band would PERMANENTLY
    // strand a fully-repaid borrower's collateral pending a governance Ccr=0 — failing the
    // "can this bit strand value?" litmus test. Shutdown's wind-down contract is that repay,
    // deposit, withdraw, and liquidation all stay open with no admin involvement.
    if ctx.accounts.market.ccr_bps > 0 && !ctx.accounts.market.shutdown {
        let m = &ctx.accounts.market;
        let fresh =
            m.spot > 0 && slot.saturating_sub(m.spot_updated_slot) <= MAX_PRICE_STALENESS_SLOTS;
        if fresh {
            let post_collateral =
                m.total_collateral.checked_sub(amount as u128).ok_or(FusdError::MathOverflow)?;
            require!(
                !cdp::tcr_below(post_collateral, m.agg_recorded_debt, m.spot, m.ccr_bps),
                FusdError::CcrRestricted
            );
        }
    }

    // Transfer collateral out of the escrow, signed by the market PDA.
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

    ctx.accounts.position.ink = new_ink;
    ctx.accounts.market.total_collateral = ctx
        .accounts
        .market
        .total_collateral
        .checked_sub(amount as u128)
        .ok_or(FusdError::MathOverflow)?;
    // Interest realized above may have grown `recorded_debt`; fold the weighted-sum delta, then the
    // stake (after the `ink` change).
    accrual::reweight(&mut ctx.accounts.market, &ctx.accounts.position, old_weighted)?;
    crate::redist::set_stake(&mut ctx.accounts.market, &mut ctx.accounts.position)?;

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
        op: crate::events::POSITION_OP_WITHDRAW,
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
