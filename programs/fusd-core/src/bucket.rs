//! Redemption rate-bucket maintenance (fusion-docs.md): keeps the per-market
//! `RedemptionBitmap` (bitmap words + per-bucket member counts + the zombie-pen count) in sync as
//! positions join/leave/move buckets, bridging the on-chain accounts to the tested
//! `fusd_math::rate_bucket` math.
//!
//! Membership == "has debt", and a debt-bearing position is in exactly one of three states, captured
//! by `Position.bucket`:
//!   - a **normal rate bucket** `k ∈ [0, NUM_RATE_BUCKETS)` — healthy, redeemable, counted in
//!     `counts[k]` with bit `k` set;
//!   - the **zombie pen** (`Position.bucket == ZOMBIE_BUCKET`) — carries debt but is no longer a
//!     normal redemption target: collateral-exhausted (`ink == 0`, unredeemable) OR sub-`min_debt`
//!     dust. Counted in `zombie_count`, OUTSIDE the bitmap, so it can never wedge or clog the normal
//!     find-first-set ordering;
//!   - debt `== 0` ⇒ in no bucket at all.
//!
//! [`reconcile`] is the single source of truth: every position-touching instruction calls it once at
//! the end (after all `recorded_debt`/`ink` and any `user_rate_bps` mutations) with the `art` the
//! position had at the START of the instruction. It handles join (0→+), leave (+→0), rate moves
//! (`adjust_rate`), and the zombie transitions (healthy↔dormant) uniformly.

use anchor_lang::prelude::*;
use fusd_math::rate_bucket as rb;

use crate::constants::{NUM_RATE_BUCKETS, ZOMBIE_BUCKET};
use crate::errors::FusdError;
use crate::state::{Market, Position, RedemptionBitmap};

/// The normal rate bucket a position's current `user_rate` maps to under the market's width.
pub fn bucket_for(market: &Market, position: &Position) -> usize {
    rb::bucket_of(position.user_rate_bps, market.bucket_width_bps, NUM_RATE_BUCKETS)
}

/// The membership a position SHOULD hold given its CURRENT (post-op) `recorded_debt`/`ink`:
/// `None` (no debt) · `Some(ZOMBIE_BUCKET)` (collateral-exhausted or sub-`min_debt` dust) ·
/// `Some(k)` (healthy member of normal rate bucket `k`). The zombie test is price-INDEPENDENT
/// (`ink == 0`, not `coll_value == 0`) so it can't oscillate on an oracle move — redemption only ever
/// drives `ink` down, so the move into the pen is monotone until the borrower tops the position back up.
fn target(market: &Market, position: &Position) -> Option<usize> {
    if position.recorded_debt == 0 {
        None
    } else if position.ink == 0 || position.recorded_debt < market.min_debt as u128 {
        Some(ZOMBIE_BUCKET)
    } else {
        Some(bucket_for(market, position))
    }
}

/// Add one member to `bucket` (a normal index or `ZOMBIE_BUCKET`); set the bitmap bit on a normal
/// bucket's empty→non-empty transition.
fn add_member(bm: &mut RedemptionBitmap, bucket: usize) -> Result<()> {
    if bucket == ZOMBIE_BUCKET {
        bm.zombie_count = bm.zombie_count.checked_add(1).ok_or(FusdError::MathOverflow)?;
    } else {
        if bm.counts[bucket] == 0 {
            rb::set(&mut bm.words, bucket);
        }
        bm.counts[bucket] = bm.counts[bucket].checked_add(1).ok_or(FusdError::MathOverflow)?;
    }
    Ok(())
}

/// Remove one member from `bucket`; clear the bitmap bit on a normal bucket's non-empty→empty
/// transition.
fn remove_member(bm: &mut RedemptionBitmap, bucket: usize) -> Result<()> {
    if bucket == ZOMBIE_BUCKET {
        bm.zombie_count = bm.zombie_count.checked_sub(1).ok_or(FusdError::MathOverflow)?;
    } else {
        bm.counts[bucket] = bm.counts[bucket].checked_sub(1).ok_or(FusdError::MathOverflow)?;
        if bm.counts[bucket] == 0 {
            rb::clear(&mut bm.words, bucket);
        }
    }
    Ok(())
}

/// Reconcile a position's bucket membership after a touch, given the `art` it had at the START of the
/// instruction (`art_before > 0` ⇒ it was counted in `position.bucket`, which may be a normal bucket
/// OR `ZOMBIE_BUCKET`). The single membership entry point — handles join, leave, the `adjust_rate`
/// move, and every healthy↔zombie transition (the decrement always targets the STORED `position.bucket`,
/// so a `bucket_width_bps` change can never mis-target it). Call once, at the end, after all
/// `recorded_debt`/`ink` mutations and any `user_rate_bps` change.
pub fn reconcile(
    bm: &mut RedemptionBitmap,
    market: &Market,
    position: &mut Position,
    art_before: u128,
) -> Result<()> {
    let was_member = art_before > 0;
    let old = position.bucket as usize;
    match (was_member, target(market, position)) {
        (false, None) => {} // 0 → 0: never a member, still none.
        (false, Some(new)) => {
            // 0 → +: first debt (or redistribution realized debt into a debt-free position) — join.
            add_member(bm, new)?;
            position.bucket = new as u16;
        }
        (true, None) => {
            // + → 0: fully repaid / redeemed-to-zero / liquidated — leave whatever bucket it was in.
            remove_member(bm, old)?;
        }
        (true, Some(new)) => {
            // + → +: move iff the classification changed (rate move, or healthy↔zombie). When a
            // position is drained into the pen and it was the SOLE member of its normal bucket, this
            // clears that bit so redemption advances instead of wedging on an unredeemable stub.
            if old != new {
                remove_member(bm, old)?;
                add_member(bm, new)?;
                position.bucket = new as u16;
            }
        }
    }
    Ok(())
}
