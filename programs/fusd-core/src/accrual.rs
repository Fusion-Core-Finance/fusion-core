//! Per-position interest accrual glue — bridges the on-chain `Market`/`Position` to the proven
//! `fusd_math::interest` weighted-debt-sum math.
//!
//! The canonical sequence every position-touching instruction follows:
//! 1. capture `old_w = weighted(position)` (the position's current contribution to the agg sum),
//! 2. [`accrue`] the market (aggregate interest → `agg_recorded_debt` + `unminted_interest`),
//! 3. [`realize`] the position (its own interest + pending redistribution → `recorded_debt`/`ink`),
//! 4. apply the instruction's own debt/collateral/rate change,
//! 5. [`reweight`] (`agg_weighted_debt_sum += new_w − old_w`) and `redist::set_stake` (after any `ink` change).
//!
//! Why the split: `accrue` charges the WHOLE market's interest in O(1) off `agg_weighted_debt_sum`;
//! `realize` moves an individual position's debt up so its NEXT interval weights correctly — neither
//! the per-position `accrued` nor the realized redistribution gain is re-added to `agg_recorded_debt`
//! (the former is already inside `accrue`'s aggregate `pending`; the latter has been in
//! `agg_recorded_debt` since the liquidation that parked it). Only the instruction's own principal
//! delta (borrow `+amount`, repay `−r`, …) moves `agg_recorded_debt` in the handler.

use anchor_lang::prelude::*;

use crate::errors::FusdError;
use crate::redist;
use crate::state::{Market, Position};

/// Advance the market's aggregate interest to `now` (O(1)): fold the pending interest into
/// `agg_recorded_debt` and the `unminted_interest` counter, then advance the clock. The interest is
/// **not minted here** — `refresh_market` mints `unminted_interest` into the insurance buffer lazily,
/// off the hot path. Interest **stops at shutdown** (BOLD): a shut-down market accrues nothing and its
/// clock freezes. Call once at the top of every position-touching instruction.
pub fn accrue(market: &mut Market, now: i64) -> Result<()> {
    let dt = now.saturating_sub(market.last_update_ts);
    if dt <= 0 || market.shutdown {
        return Ok(());
    }
    let pending =
        fusd_math::interest::pending_aggregate_interest(market.agg_weighted_debt_sum, dt as u64)
            .ok_or(FusdError::MathOverflow)?;
    // Book the pending interest through the shared supply transition (agg + unminted move in
    // lockstep — the body certora.rs proves).
    let d = crate::supply_transition::book_interest(
        market.agg_recorded_debt,
        market.unminted_interest,
        pending,
    )
    .ok_or(FusdError::MathOverflow)?;
    market.agg_recorded_debt = d.new_agg;
    market.unminted_interest = d.new_unminted;
    market.last_update_ts = now;
    Ok(())
}

/// Bring a position current before it's read or changed: fold its accrued interest (at its OWN
/// `user_rate_bps`, over the gap since `last_debt_update`) AND any pending tier-2 redistribution into
/// `recorded_debt`/`ink`, roll the redistribution snapshot, and advance `last_debt_update`.
///
/// Does NOT touch `agg_recorded_debt` (see the module note), nor `agg_weighted_debt_sum`/`stake` — the
/// caller applies the single weighted-sum delta via [`reweight`] after its own change, then
/// `redist::set_stake` after any `ink` change. Call AFTER [`accrue`].
///
/// **Shutdown:** interest stops at the wind-down (BOLD). The aggregate stops accruing in [`accrue`]
/// (the clock `last_update_ts` freezes at the final pre-shutdown accrue, which `shutdown`/`liquidate`
/// force to the shutdown moment), so the per-position period is capped at `last_update_ts` too — else
/// per-position interest would outrun the frozen aggregate and break `Σ recorded_debt ≤ agg_recorded_debt`.
pub fn realize(market: &Market, position: &mut Position, now: i64) -> Result<()> {
    // Cap the accrual window at the shutdown moment (= the frozen aggregate clock) once shut down.
    let cap = if market.shutdown { market.last_update_ts } else { now };
    let period = cap.saturating_sub(position.last_debt_update).max(0) as u64;
    let accrued = fusd_math::interest::accrued_interest(
        position.recorded_debt,
        position.user_rate_bps,
        period,
    )
    .ok_or(FusdError::MathOverflow)?;

    let (pending_coll, pending_debt) = redist::pending(market, position)?;
    if pending_coll > 0 {
        let add = u64::try_from(pending_coll).map_err(|_| FusdError::MathOverflow)?;
        let new_ink = position.ink.checked_add(add).ok_or(FusdError::MathOverflow)?;
        // Realized redistribution IS a collateral change — the fold must bump `ink_nonce`
        // (the fuSOL stake-pool design names it explicitly), so debt-only touches (borrow/repay/adjust_rate)
        // that fold pending collateral still invalidate a stake-direction preference.
        position.set_ink(new_ink);
    }
    position.recorded_debt = position
        .recorded_debt
        .checked_add(accrued)
        .ok_or(FusdError::MathOverflow)?
        .checked_add(pending_debt)
        .ok_or(FusdError::MathOverflow)?;
    // Advance to `cap`, not `now`: in steady state they are equal; under shutdown `cap` is the frozen
    // moment, so a later frozen touch sees `period = (cap − cap).max(0) = 0` and charges no further
    // interest (obviously-correct vs. relying on the `.max(0)` clamp against a `now > cap` gap).
    position.last_debt_update = cap;
    redist::roll_snapshot(market, position);
    Ok(())
}

/// A position's current contribution to `Market.agg_weighted_debt_sum` (`recorded_debt · rate_bps`).
/// Capture this BEFORE [`realize`] + the instruction's change to get the `old_weighted` for [`reweight`].
pub fn weighted(position: &Position) -> Result<u128> {
    Ok(
        fusd_math::interest::weighted_debt(position.recorded_debt, position.user_rate_bps)
            .ok_or(FusdError::MathOverflow)?,
    )
}

/// Apply the single weighted-sum delta for a position whose `recorded_debt` and/or `rate` changed
/// this instruction: `agg_weighted_debt_sum += new_weighted − old_weighted` (add THEN subtract,
/// checked — no intermediate underflow on a net-decrease). `old_weighted` is the contribution captured
/// before [`realize`] + the change; the new contribution is read here from the now-current position.
pub fn reweight(market: &mut Market, position: &Position, old_weighted: u128) -> Result<()> {
    let new_weighted = weighted(position)?;
    market.agg_weighted_debt_sum = market
        .agg_weighted_debt_sum
        .checked_add(new_weighted)
        .ok_or(FusdError::MathOverflow)?
        .checked_sub(old_weighted)
        .ok_or(FusdError::MathOverflow)?;
    Ok(())
}
