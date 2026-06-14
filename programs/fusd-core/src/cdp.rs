//! Pure CDP math — thin, testable helpers the instruction handlers orchestrate.
//! Built on `fusd-math`. Rounding is conservative against the borrower (debt up,
//! collateral value down). fusion-docs.md.

use fusd_math::{mul_div_ceil, mul_div_floor, ray_mul, RAY};

pub const BPS: u128 = 10_000;

/// Collateral value in fUSD-native units: `ink * spot / RAY`, floored (conservative).
/// `spot` is RAY-scaled fUSD-native per 1 native collateral unit (see `Market.spot`).
pub fn collateral_value(ink: u64, spot: u128) -> Option<u128> {
    ray_mul(ink as u128, spot)
}

/// Max fUSD debt for a collateral value at `mcr_bps`: `value * 10_000 / mcr_bps`, floored.
pub fn max_debt(collateral_value: u128, mcr_bps: u16) -> Option<u128> {
    if mcr_bps == 0 {
        return None;
    }
    mul_div_floor(collateral_value, BPS, mcr_bps as u128)
}

/// Liquidation collateral **collar**: split a position's `ink` into the collateral a liquidation may
/// SEIZE and the SURPLUS returned to the borrower (fusion-docs.md). The SEIZED **quantity** is
/// `ceil(ceil(debt·(1+bonus))/spot)` capped at `ink` — the native-unit count that covers `debt·(1+bonus)`
/// of value; the rest is the borrower's claimable surplus. Both conversions round **UP** against the
/// borrower (so the surplus never rounds in their favor), which on a dust position can seize a count
/// whose value is a sub-unit above the cap — protocol-favoring, never the reverse.
///
/// - `bonus_bps == 0` ⇒ collar OFF: seize all of `ink`, return nothing (the legacy behavior).
/// - An underwater position (collateral value ≤ the capped seize value) seizes all of `ink` (surplus 0).
///
/// Returns `(seize_coll, surplus_coll)` with `seize_coll + surplus_coll == ink` exactly. `spot > 0`.
pub fn seize_collateral(ink: u64, debt: u128, spot: u128, bonus_bps: u16) -> Option<(u64, u64)> {
    if bonus_bps == 0 {
        return Some((ink, 0)); // collar disabled: seize the whole position
    }
    // The fUSD-native value the liquidation may take (debt + the bonus), rounded UP against the borrower.
    let seize_value = mul_div_ceil(debt, BPS + bonus_bps as u128, BPS)?;
    // Convert to native collateral (rounded UP), capped at the position's `ink`.
    let seize_coll = mul_div_ceil(seize_value, RAY, spot)?.min(ink as u128) as u64;
    Some((seize_coll, ink - seize_coll))
}

/// `recorded_debt <= max_debt(collateral_value(ink, spot), mcr_bps)` — i.e. the position is at or
/// above its minimum collateral ratio. `recorded_debt` is the position's realized present-value debt
/// (interest already folded in by `accrual::realize` — no `art*rate` realization here).
pub fn is_healthy(ink: u64, recorded_debt: u128, spot: u128, mcr_bps: u16) -> bool {
    let value = match collateral_value(ink, spot) {
        Some(v) => v,
        None => return false,
    };
    match max_debt(value, mcr_bps) {
        Some(m) => recorded_debt <= m,
        None => false,
    }
}

/// Whether the market's aggregate collateral ratio (TCR) is **below** a `ratio_bps` threshold:
/// `agg_recorded_debt > max_debt(collateral_value, ratio_bps)` — the market-level analog of
/// `!is_healthy`, over the `u128` aggregates `total_collateral`/`agg_recorded_debt`. Callers
/// `accrual::accrue` first, so `agg_recorded_debt` includes interest up to now. Used for BOTH the
/// shutdown SCR and the CCR borrow-restriction band. Returns `false` when there is no debt (a market
/// with no debt is never "below" any ratio) and, conservatively, on any arithmetic edge (so a math
/// overflow can never spuriously trigger a shutdown or freeze borrowing).
pub fn tcr_below(
    total_collateral: u128,
    agg_recorded_debt: u128,
    spot: u128,
    ratio_bps: u16,
) -> bool {
    if agg_recorded_debt == 0 {
        return false;
    }
    let value = match ray_mul(total_collateral, spot) {
        Some(v) => v,
        None => return false,
    };
    match max_debt(value, ratio_bps) {
        Some(m) => agg_recorded_debt > m,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Net-outflow rate limiter — a leaky bucket (token bucket). fusion-docs.md.
// ---------------------------------------------------------------------------

/// The bucket's current pressure after time-decay: it refills at the rate of the **current** `cap`
/// per `window_secs`, restoring `cap * elapsed / window_secs` of pressure since `last_update`. No
/// fixed-window boundary burst. `window_secs <= 0` ⇒ no decay (treated as inert). The cap is a
/// mutable gov param; `apply_param` clamps `rl_accrued <= rl_cap` on every change, so stored
/// pressure never exceeds the live cap (a cap-lower then drains at the new, slower rate).
fn ratelimit_decayed(accrued: u64, last_update: i64, now: i64, cap: u64, window_secs: i64) -> u64 {
    if window_secs <= 0 {
        return accrued;
    }
    let elapsed = now.saturating_sub(last_update).max(0) as u128;
    let restored = (cap as u128).saturating_mul(elapsed) / window_secs as u128;
    accrued.saturating_sub(u64::try_from(restored.min(u64::MAX as u128)).unwrap_or(u64::MAX))
}

/// Consume `outflow` of capacity (an outflow op, e.g. `borrow`). Returns the new pressure to store,
/// or `None` if it would exceed `cap` (the caller reverts). Callers skip this entirely when
/// `cap == 0` (disabled). The result is always `<= cap`.
pub fn ratelimit_consume(
    accrued: u64,
    last_update: i64,
    now: i64,
    cap: u64,
    window_secs: i64,
    outflow: u64,
) -> Option<u64> {
    let next = ratelimit_decayed(accrued, last_update, now, cap, window_secs).checked_add(outflow)?;
    if next > cap {
        return None;
    }
    Some(next)
}

/// Restore `inflow` of capacity (an inflow op, e.g. `repay` — the "net" in net-outflow). Always
/// succeeds; floors at 0. Returns the new pressure to store.
pub fn ratelimit_restore(
    accrued: u64,
    last_update: i64,
    now: i64,
    cap: u64,
    window_secs: i64,
    inflow: u64,
) -> u64 {
    ratelimit_decayed(accrued, last_update, now, cap, window_secs).saturating_sub(inflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fusd_math::RAY;

    // A clean price: 1 collateral unit = 150 fUSD-native, RAY-scaled.
    const SPOT_150: u128 = 150 * RAY;

    #[test]
    fn value_and_max_debt() {
        assert_eq!(collateral_value(2, SPOT_150), Some(300)); // 2 * 150
        assert_eq!(max_debt(300, 12_000), Some(250)); // 300 / 1.2
        assert_eq!(max_debt(300, 10_000), Some(300)); // 100% MCR
        assert_eq!(max_debt(300, 0), None);
    }

    #[test]
    fn health_boundary() {
        // ink=2 @ 150 => value 300; MCR 120% => max debt 250. `recorded_debt` IS the present debt.
        assert!(is_healthy(2, 250, SPOT_150, 12_000)); // exactly at the limit: healthy
        assert!(!is_healthy(2, 251, SPOT_150, 12_000)); // one over: unhealthy
        assert!(!is_healthy(0, 1, SPOT_150, 12_000)); // no collateral, some debt
        assert!(is_healthy(2, 0, SPOT_150, 12_000)); // no debt: always healthy
        // unset price (spot 0) => value 0 => any debt unhealthy (can't borrow w/o a price)
        assert!(!is_healthy(2, 1, 0, 12_000));
    }

    #[test]
    fn liquidation_collar() {
        // spot = RAY: 1 native collateral unit == 1 fUSD-native.
        // Collar off (bonus 0): seize everything, no surplus.
        assert_eq!(seize_collateral(200, 100, RAY, 0), Some((200, 0)));
        // 10% bonus: seize collateral worth debt·1.10 = 110; return the rest of the 200.
        assert_eq!(seize_collateral(200, 100, RAY, 1_000), Some((110, 90)));
        // Underwater (collateral value 100 <= seize value 110): seize all, no surplus.
        assert_eq!(seize_collateral(100, 100, RAY, 1_000), Some((100, 0)));
        // Exactly at the cap: seize all, no surplus.
        assert_eq!(seize_collateral(110, 100, RAY, 1_000), Some((110, 0)));
        // Rounds UP against the borrower: a 0.01% bonus on 100 -> ceil(100.01) = 101 seized.
        assert_eq!(seize_collateral(200, 100, RAY, 1), Some((101, 99)));
        // Super-unit price (1 token = 2 fUSD-native): seize value 110 -> 55 tokens.
        assert_eq!(seize_collateral(200, 100, 2 * RAY, 1_000), Some((55, 145)));
        // Sub-unit price (1 token = 0.5 fUSD-native): seize value 110 -> 220 tokens.
        assert_eq!(seize_collateral(300, 100, RAY / 2, 1_000), Some((220, 80)));
        // Fail-closed: a zero price (would divide by zero) returns None, never a wrap.
        assert_eq!(seize_collateral(100, 100, 0, 1_000), None);
        // Conservation: seize + surplus == ink for every case.
        for &(ink, debt, bonus) in &[(0u64, 0u128, 0u16), (1000, 500, 1000), (7, 13, 2000), (10, 10, 500)] {
            let (s, r) = seize_collateral(ink, debt, RAY, bonus).unwrap();
            assert_eq!(s + r, ink);
        }
    }

    #[test]
    fn tcr_shutdown_boundary() {
        // 10 collateral @ $100 = $1000 value; SCR 110% => shutdown when debt > 1000/1.1 = 909.
        let spot_100: u128 = 100 * RAY;
        assert!(!tcr_below(10, 909, spot_100, 11_000)); // exactly at the limit: not below
        assert!(tcr_below(10, 910, spot_100, 11_000)); // one over: below SCR
        // No debt => never below SCR (nothing to wind down), even at a zero price.
        assert!(!tcr_below(10, 0, spot_100, 11_000));
        assert!(!tcr_below(0, 0, 0, 11_000));
        // Debt but zero price => value 0 < any positive debt => below SCR.
        assert!(tcr_below(10, 1, 0, 11_000));
    }

    #[test]
    fn ratelimit_leaky_bucket() {
        const CAP: u64 = 1_000;
        const W: i64 = 100; // window seconds
        // Consume up to the cap.
        let a = ratelimit_consume(0, 0, 0, CAP, W, 600).unwrap();
        assert_eq!(a, 600);
        let a = ratelimit_consume(a, 0, 0, CAP, W, 400).unwrap();
        assert_eq!(a, 1000);
        // One more unit at the same instant exceeds the cap.
        assert_eq!(ratelimit_consume(a, 0, 0, CAP, W, 1), None);

        // Time refills linearly: half a window restores half the cap.
        let a = ratelimit_decayed(1000, 0, W / 2, CAP, W);
        assert_eq!(a, 500);
        // After a full window the bucket is empty regardless of prior pressure.
        assert_eq!(ratelimit_decayed(1000, 0, W, CAP, W), 0);
        assert_eq!(ratelimit_decayed(1000, 0, 10 * W, CAP, W), 0);

        // Restore (an inflow) reduces pressure immediately, floored at 0.
        assert_eq!(ratelimit_restore(800, 0, 0, CAP, W, 300), 500);
        assert_eq!(ratelimit_restore(800, 0, 0, CAP, W, 9999), 0);

        // After a full window, a fresh full-cap consume succeeds (no boundary burst beyond cap).
        let a = ratelimit_consume(1000, 0, W, CAP, W, 1000).unwrap();
        assert_eq!(a, 1000);
        assert_eq!(ratelimit_consume(1000, 0, W, CAP, W, 1001), None);

        // No overflow on extreme decay inputs.
        assert_eq!(ratelimit_decayed(u64::MAX, 0, i64::MAX, u64::MAX, 1), 0);
    }
}
