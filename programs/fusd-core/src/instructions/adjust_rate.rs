use anchor_lang::prelude::*;
use anchor_spl::token::Mint;

use crate::accrual;
use crate::cdp;
use crate::constants::{
    MARKET_SEED, MAX_PRICE_STALENESS_SLOTS, MAX_USER_RATE_BPS, MIN_USER_RATE_BPS, POSITION_SEED,
    REDEMPTION_BITMAP_SEED,
};
use crate::errors::FusdError;
use crate::state::{Market, Position, RedemptionBitmap};

/// Change a position's borrower-set interest rate, moving it to the matching redemption bucket
/// (fusion-docs.md `adjust_rate`). Interest is realized at the OLD rate first, then the
/// rate switches and the weighted-debt sum is re-weighted at the new rate. When the market enables
/// it (`rate_adjust_cooldown_secs > 0`), a rate change within that cooldown of the position's last
/// change/open is charged a BOLD **upfront fee** = `cooldown`-seconds of interest at the new rate
/// — so reactive rate-jumping to dodge the redemption queue costs ~that much each time.
/// When (and ONLY when) that fee is charged, a fresh-price ICR≥MCR re-check runs afterward so the
/// fee can't push the borrower's own position below MCR (BOLD-sweep); the common no-fee path
/// reads no oracle.
#[event_cpi]
#[derive(Accounts)]
pub struct AdjustRate<'info> {
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

    #[account(mut, seeds = [REDEMPTION_BITMAP_SEED, collateral_mint.key().as_ref()], bump)]
    pub redemption_bitmap: AccountLoader<'info, RedemptionBitmap>,
}

pub fn handler(ctx: Context<AdjustRate>, new_rate_bps: u16) -> Result<()> {
    // Rejected in a shut-down market: the rate buckets it maintains only order live redemptions,
    // but a terminal market winds down via unordered 0-fee `urgent_redeem`, so a rate change does
    // nothing useful — and would still charge the premature-adjustment fee. Matches borrow/redeem.
    require!(!ctx.accounts.market.shutdown, FusdError::MarketShutdown);
    require!(
        new_rate_bps >= MIN_USER_RATE_BPS && new_rate_bps <= MAX_USER_RATE_BPS,
        FusdError::InterestRateOutOfBounds
    );
    let now = Clock::get()?.unix_timestamp;
    let art_before = ctx.accounts.position.recorded_debt;
    // Weight captured at the OLD rate; `realize` accrues interest at the OLD rate before the switch.
    let old_weighted = accrual::weighted(&ctx.accounts.position)?;
    accrual::accrue(&mut ctx.accounts.market, now)?;
    accrual::realize(&ctx.accounts.market, &mut ctx.accounts.position, now)?;

    // Premature-rate-change upfront fee (BOLD anti-gaming). When the market enables it and
    // this change lands within `cooldown` of the position's last change/open, charge `cooldown`-secs of
    // interest at the NEW rate (rounded up, against the borrower) on the realized debt. The fee is added
    // to the position's `recorded_debt` AND the market aggregate AND `unminted_interest` — exactly like
    // accrued interest, so `refresh_market` mints it into the insurance buffer and the supply invariant
    // (`circulating == agg_recorded_debt − unminted_interest + bad_debt`) is preserved. Always update the
    // cooldown clock so the next change is measured from now.
    // TWIN: borrow.rs's C7 upfront fee folds the same debt+agg+unminted triple (there fused into the
    // borrow delta BEFORE the health checks). A change to either fee's accounting must be mirrored in
    // the other and re-modeled in certora.rs's supply rules.
    let cooldown = ctx.accounts.market.rate_adjust_cooldown_secs;
    let last_adjust = ctx.accounts.position.last_rate_adjust_ts;
    let within_cooldown = cooldown > 0 && now < last_adjust.saturating_add(cooldown);
    if within_cooldown && ctx.accounts.position.recorded_debt > 0 {
        let fee = fusd_math::interest::premature_adjustment_fee(
            ctx.accounts.position.recorded_debt,
            new_rate_bps,
            cooldown as u64,
        )
        .ok_or(FusdError::MathOverflow)?;
        ctx.accounts.position.recorded_debt = ctx
            .accounts
            .position
            .recorded_debt
            .checked_add(fee)
            .ok_or(FusdError::MathOverflow)?;
        ctx.accounts.market.agg_recorded_debt = ctx
            .accounts
            .market
            .agg_recorded_debt
            .checked_add(fee)
            .ok_or(FusdError::MathOverflow)?;
        ctx.accounts.market.unminted_interest = ctx
            .accounts
            .market
            .unminted_interest
            .checked_add(fee)
            .ok_or(FusdError::MathOverflow)?;

        // BOLD-sweep (`_applyUpfrontFee` re-checks ICR≥MCR / TCR≥CCR): the fee just GREW
        // `recorded_debt` and the aggregate, so a fee charged on a near-MCR position could push it
        // below MCR — a self-inflicted liquidation hole the moment a non-zero cooldown is enabled.
        // Re-check health AFTER folding the fee and revert if it would make the position liquidatable.
        //
        // This is the ONLY `adjust_rate` branch that reads the oracle: the common no-fee path stays
        // price-free (the NONE tier — see constants.rs). FAIL-CLOSED — a stale/absent price rejects the
        // fee-bearing adjust (acting on an untrusted price is worse than making the borrower wait out
        // the cooldown or repay first). This NEVER expands the liquidatable set: it blocks
        // only the borrower's OWN fee-bearing op; existing liquidation eligibility is untouched.
        let spot = ctx.accounts.market.spot;
        let slot = Clock::get()?.slot;
        require!(spot > 0, FusdError::OracleUnavailable);
        require!(
            slot.saturating_sub(ctx.accounts.market.spot_updated_slot) <= MAX_PRICE_STALENESS_SLOTS,
            FusdError::StalePrice
        );
        require!(
            cdp::is_healthy(
                ctx.accounts.position.ink,
                ctx.accounts.position.recorded_debt,
                spot,
                ctx.accounts.market.mcr_bps,
            ),
            FusdError::BelowMinCollateralRatio
        );

        // CCR borrow-restriction band: the fee added to the aggregate is a risk-increasing change,
        // so when the band is enabled in a LIVE market it must not push the market's TCR below CCR
        // (mirroring borrow/withdraw). Gated on `ccr_bps > 0` and SKIPPED in shutdown — a terminal
        // market's TCR can never recover, so an active band would needlessly strand the borrower's
        // ability to re-rate (the same reasoning as withdraw.rs). The price is already fresh here
        // (fail-closed above), so no fail-open dance is needed; this never expands the liquidatable set.
        if ctx.accounts.market.ccr_bps > 0 && !ctx.accounts.market.shutdown {
            let m = &ctx.accounts.market;
            require!(
                !cdp::tcr_below(m.total_collateral, m.agg_recorded_debt, spot, m.ccr_bps),
                FusdError::CcrRestricted
            );
        }
    }
    ctx.accounts.position.last_rate_adjust_ts = now;

    // Switch the rate, then re-weight the position into the aggregate at the NEW rate (the delta moves
    // its whole `recorded_debt` — including any upfront fee just added — from the old rate's weight to
    // the new rate's).
    ctx.accounts.position.user_rate_bps = new_rate_bps;
    accrual::reweight(&mut ctx.accounts.market, &ctx.accounts.position, old_weighted)?;
    crate::redist::set_stake(&mut ctx.accounts.market, &mut ctx.accounts.position)?;

    // Reconcile membership on the post-switch state (single source of truth): an already-bucketed
    // position moves to the new rate's bucket; one that `realize` just brought debt into joins it; and
    // a zombie (collateral-exhausted / sub-min_debt) stays in the pen — a rate change never restores
    // health, but its new rate is recorded so it lands in the right bucket when it later un-zombies.
    {
        let mut bm = ctx.accounts.redemption_bitmap.load_mut()?;
        crate::bucket::reconcile(&mut bm, &ctx.accounts.market, &mut ctx.accounts.position, art_before)?;
    }

    emit_cpi!(crate::events::PositionUpdated {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        owner: ctx.accounts.position.owner,
        op: crate::events::POSITION_OP_ADJUST_RATE,
        amount: new_rate_bps as u64,
        ink: ctx.accounts.position.ink,
        recorded_debt: ctx.accounts.position.recorded_debt,
        user_rate_bps: ctx.accounts.position.user_rate_bps,
        bucket: ctx.accounts.position.bucket,
    });
    Ok(())
}
