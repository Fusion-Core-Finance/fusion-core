//! Liquidation tier-2 redistribution glue: bridges the on-chain `Market`/`Position` to the tested
//! `fusd_math::redistribution` math. See fusion-docs.md.
//!
//! Invariant maintained by the callers: every instruction that touches a position's `ink`/`art`
//! calls [`realize`] first (to fold in any pending redistribution), then adjusts state, then calls
//! [`set_stake`]. `Market.total_collateral` and `Market.agg_art` already account for redistributed
//! amounts (counted at redistribution time), so [`realize`] never moves them — it only shifts a
//! position's pending gains from "implied by the accumulators" to "recorded", and `total_stakes`
//! tracks the stored stakes.
//!
//! Floor-dust direction: a position realizes a **floored** share of each redistribution
//! ([`fusd_math::redistribution::pending`]), and the residual stays in the aggregates. So
//! `Market.total_collateral >= Σ position.ink` and `Market.agg_art >= Σ position.art` — the dust is
//! **protocol-favoring** (extra over-collateralization / debt counted, never a shortfall). What
//! stays EXACT is `Market.total_collateral == collateral-vault balance`: the dust is real tokens in
//! the vault, simply not yet owned by any position.

use anchor_lang::prelude::*;
use fusd_math::redistribution::{self as rd, RedistSnapshot, RedistState};

use crate::errors::FusdError;
use crate::state::{Market, Position};

/// Build the math accumulator state from the market.
pub fn state(m: &Market) -> RedistState {
    RedistState {
        l_coll: m.l_coll,
        l_art: m.l_art,
        last_coll_error: m.last_coll_redist_error,
        last_art_error: m.last_art_redist_error,
    }
}

/// Write a mutated accumulator state back to the market.
pub fn write_state(m: &mut Market, st: &RedistState) {
    m.l_coll = st.l_coll;
    m.l_art = st.l_art;
    m.last_coll_redist_error = st.last_coll_error;
    m.last_art_redist_error = st.last_art_error;
}

pub fn snapshot_of(p: &Position) -> RedistSnapshot {
    RedistSnapshot { l_coll: p.redist_l_coll_snapshot, l_art: p.redist_l_art_snapshot }
}

/// A position's pending (unrealized) tier-2 redistribution gains: `(collateral, recorded-debt)`,
/// `stake · (l − snapshot)` floored. The "debt" is recorded present-value debt (BOLD `L_boldDebt`).
pub fn pending(m: &Market, p: &Position) -> Result<(u128, u128)> {
    Ok(rd::pending(p.stake, &state(m), &snapshot_of(p)).map_err(map_err)?)
}

/// Roll a position's redistribution snapshot forward to the market's current accumulators (after its
/// pending gains have been realized into `ink`/`recorded_debt`). Liquity `rewardSnapshots` update.
pub fn roll_snapshot(m: &Market, p: &mut Position) {
    p.redist_l_coll_snapshot = m.l_coll;
    p.redist_l_art_snapshot = m.l_art;
}

pub fn map_err(e: rd::RedistError) -> FusdError {
    match e {
        rd::RedistError::NoStakes => FusdError::NoRedistributionRecipients,
        rd::RedistError::AccumulatorOverflow => FusdError::RedistributionAccumulatorOverflow,
        rd::RedistError::Math => FusdError::MathOverflow,
    }
}

/// Recompute a position's stake from its current `ink` and the market's post-liquidation system
/// snapshot, folding the delta into `market.total_stakes`. Liquity `_updateStakeAndTotalStakes`.
/// Call AFTER any `ink` change (and after [`realize`]).
pub fn set_stake(m: &mut Market, p: &mut Position) -> Result<()> {
    let new_stake =
        rd::compute_stake(p.ink as u128, m.total_stakes_snapshot, m.total_collateral_snapshot)
            .map_err(map_err)?;
    m.total_stakes = m
        .total_stakes
        .checked_sub(p.stake)
        .and_then(|t| t.checked_add(new_stake))
        .ok_or(FusdError::MathOverflow)?;
    p.stake = new_stake;
    Ok(())
}

