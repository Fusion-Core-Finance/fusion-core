use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, MintTo, Token, TokenAccount};

use crate::accrual;
use crate::cdp;
use crate::constants::{
    FUSD_MINT_SEED, MARKET_SEED, MAX_PRICE_STALENESS_SLOTS, MINT_AUTHORITY_BUMP, MINT_AUTHORITY_SEED,
    POSITION_SEED,
    RATELIMIT_WINDOW_SECS, REDEMPTION_BITMAP_SEED,
};
use crate::errors::FusdError;
use crate::state::{Market, Position, RedemptionBitmap};

/// Mint `amount` fUSD against a position, up to its MCR and the market debt ceiling.
#[event_cpi]
#[derive(Accounts)]
pub struct Borrow<'info> {
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

    /// CHECK: the fUSD mint-authority PDA; only signs minting from inside this rule.
    #[account(seeds = [MINT_AUTHORITY_SEED], bump = MINT_AUTHORITY_BUMP)]
    pub mint_authority: UncheckedAccount<'info>,

    #[account(mut, token::mint = fusd_mint, token::authority = owner)]
    pub owner_fusd_ata: Account<'info, TokenAccount>,

    #[account(mut, seeds = [REDEMPTION_BITMAP_SEED, collateral_mint.key().as_ref()], bump)]
    pub redemption_bitmap: AccountLoader<'info, RedemptionBitmap>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<Borrow>, amount: u64) -> Result<()> {
    require!(amount > 0, FusdError::ZeroAmount);
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let slot = clock.slot;

    let art_before = ctx.accounts.position.recorded_debt;
    let old_weighted = accrual::weighted(&ctx.accounts.position)?;
    // Accrue the market's aggregate interest, then realize this position's own interest + any pending
    // tier-2 redistribution, so the MCR / ceiling / CCR checks see the current present debt.
    accrual::accrue(&mut ctx.accounts.market, now)?;
    accrual::realize(&ctx.accounts.market, &mut ctx.accounts.position, now)?;

    let spot = ctx.accounts.market.spot;
    let mcr = ctx.accounts.market.mcr_bps;

    // Taking on debt requires a fresh, non-degraded, non-paused market. `spot > 0` and staleness
    // guard the cache; `mint_frozen` is the oracle aggregate's mode (set by `update_price`);
    // `guardian_paused_until` is the independent guardian de-risk brake. All three freeze NEW MINTS
    // only — repay, withdraw, liquidation, and redemption ignore them and keep using `spot`.
    require!(!ctx.accounts.market.shutdown, FusdError::MarketShutdown);
    require!(spot > 0, FusdError::OracleUnavailable);
    require!(!ctx.accounts.market.mint_frozen, FusdError::MintFrozen);
    require!(now >= ctx.accounts.market.guardian_paused_until, FusdError::GuardianPaused);
    require!(
        slot.saturating_sub(ctx.accounts.market.spot_updated_slot) <= MAX_PRICE_STALENESS_SLOTS,
        FusdError::StalePrice
    );

    // C7 upfront borrowing fee: a one-time charge ADDED TO THE DEBT (`borrow_fee_bps`; 0 = disabled),
    // rounded UP against the borrower. The fee is NOT minted to the borrower — debt grows by
    // `amount + fee`, only `amount` is minted below, and `fee` is booked into `unminted_interest` so
    // `refresh_market` mints it to the insurance buffer (funds first-loss capital exactly like accrued
    // interest). The supply identity `circulating == agg_recorded_debt − unminted_interest + bad_debt`
    // is preserved: circulating += amount, agg += amount + fee, unminted += fee. The MCR / ceiling /
    // CCR checks below all see the POST-fee debt (the borrower must collateralize what they owe).
    // TWIN: adjust_rate.rs's premature-adjustment fee folds the same debt+agg+unminted triple (there
    // contiguously, post-realization, via `supply_transition::book_interest`). This site's algebra is
    // `supply_transition::borrow` — the shared body certora.rs's `supply_preserved_by_borrow_ghost`
    // executes at NativeInt.
    let fee = if ctx.accounts.market.borrow_fee_bps > 0 {
        fusd_math::mul_div_ceil(
            amount as u128,
            ctx.accounts.market.borrow_fee_bps as u128,
            fusd_math::BPS_DENOMINATOR,
        )
        .ok_or(FusdError::MathOverflow)?
    } else {
        0
    };
    // The supply-identity transition (debt_delta / new_agg / new_unminted) — the shared body
    // certora.rs proves; the checks below consume its outputs, the commit block assigns them.
    let d = crate::supply_transition::borrow(
        ctx.accounts.market.agg_recorded_debt,
        ctx.accounts.market.unminted_interest,
        amount as u128,
        fee,
    )
    .ok_or(FusdError::MathOverflow)?;

    // `recorded_debt` is now realized present-value debt; the borrow adds `amount + fee` to it.
    let new_debt = ctx
        .accounts
        .position
        .recorded_debt
        .checked_add(d.debt_delta)
        .ok_or(FusdError::MathOverflow)?;
    let ink = ctx.accounts.position.ink;
    require!(cdp::is_healthy(ink, new_debt, spot, mcr), FusdError::BelowMinCollateralRatio);

    // Dust floor: when enabled, a borrow must leave the position at or above `min_debt`,
    // so the lowest rate bucket can't be stuffed with sub-floor positions to throttle redemptions.
    let min_debt = ctx.accounts.market.min_debt;
    require!(min_debt == 0 || new_debt >= min_debt as u128, FusdError::DebtBelowMinimum);

    require!(
        d.new_agg <= ctx.accounts.market.debt_ceiling as u128,
        FusdError::DebtCeilingExceeded
    );

    // CCR borrow-restriction band: when enabled, a borrow may not leave the market's aggregate
    // TCR below CCR — block new debt into a stressed market. The price is already fresh here (the
    // staleness gate above fail-closes), and this NEVER expands the liquidatable set.
    if ctx.accounts.market.ccr_bps > 0 {
        let m = &ctx.accounts.market;
        require!(
            !cdp::tcr_below(m.total_collateral, d.new_agg, spot, m.ccr_bps),
            FusdError::CcrRestricted
        );
    }

    // Net-outflow rate limiter: borrowing this `amount` consumes bucket capacity. Disabled
    // when `rl_cap == 0`. Checked before any state change so an over-cap borrow mints nothing.
    if ctx.accounts.market.rl_cap > 0 {
        let m = &ctx.accounts.market;
        let new_accrued = cdp::ratelimit_consume(
            m.rl_accrued,
            m.rl_last_update,
            now,
            m.rl_cap,
            RATELIMIT_WINDOW_SECS,
            amount,
        )
        .ok_or(FusdError::RateLimitExceeded)?;
        ctx.accounts.market.rl_accrued = new_accrued;
        ctx.accounts.market.rl_last_update = now;
    }

    // Commit the debt, update the two aggregates (the weighted sum by the realize + borrow delta),
    // recompute the stake (realize may have grown ink via redistribution), then mint.
    ctx.accounts.position.recorded_debt = new_debt;
    ctx.accounts.market.agg_recorded_debt = d.new_agg;
    // C7: book the upfront fee as unminted interest — `refresh_market` mints it to the buffer. This
    // is the +fee half of `agg += amount + fee` that is NOT minted to the borrower (supply identity).
    // `d.new_unminted == unminted + fee` (unchanged when `fee == 0`).
    ctx.accounts.market.unminted_interest = d.new_unminted;
    accrual::reweight(&mut ctx.accounts.market, &ctx.accounts.position, old_weighted)?;
    crate::redist::set_stake(&mut ctx.accounts.market, &mut ctx.accounts.position)?;

    let bump = MINT_AUTHORITY_BUMP;
    let signer: &[&[&[u8]]] = &[&[MINT_AUTHORITY_SEED, &[bump]]];
    token::mint_to(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            MintTo {
                mint: ctx.accounts.fusd_mint.to_account_info(),
                to: ctx.accounts.owner_fusd_ata.to_account_info(),
                authority: ctx.accounts.mint_authority.to_account_info(),
            },
            signer,
        ),
        amount,
    )?;

    // Reconcile redemption rate-bucket membership (joins on the first debt, incl. any realized
    // redistributed debt).
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
        op: crate::events::POSITION_OP_BORROW,
        amount: amount,
        ink: ctx.accounts.position.ink,
        recorded_debt: ctx.accounts.position.recorded_debt,
        user_rate_bps: ctx.accounts.position.user_rate_bps,
        bucket: ctx.accounts.position.bucket,
    });
    Ok(())
}
