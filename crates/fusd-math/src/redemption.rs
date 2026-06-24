//! Dynamic redemption base-rate (Liquity-style decaying volume-spike fee; BOLD-sweep C9).
//!
//! Fusion's baseline redemption fee is a flat `redemption_fee_bps`. C9 layers a *dynamic* component
//! on top: a `base_rate` that SPIKES with redemption volume and DECAYS exponentially over time, so a
//! large redemption raises the fee for everyone briefly (deterring a redemption-arbitrage stampede)
//! and the fee relaxes back to the floor as volume subsides. This is the canonical Liquity peg
//! defense (`LiquityBase`/`TroveManager._updateBaseRateFromRedemption` + `_calcDecayedBaseRate`),
//! re-expressed in Fusion's integer RAY fixed-point.
//!
//! All three functions are pure, total, and host-tested — no account, no float. The on-chain
//! `redeem` instruction stores `base_rate` (RAY) + its last-update unix-ts on the `Market` and calls
//! these. The flat `redemption_fee_bps` becomes the FLOOR; a governable `redemption_base_rate_max_bps`
//! caps (and, at 0, disables) the dynamic add — so a market with the dynamic component off prices
//! redemptions byte-identically to the pre-C9 flat fee.

use crate::{mul_div_floor, ray_mul, ray_pow, BPS_DENOMINATOR, RAY};

/// Per-minute exponential decay factor (RAY): `0.5^(1/360)`, a **6-hour half-life**. Placeholder
/// pending calibration (the digest's ~6h; Liquity v1 uses 12h). `base_rate` loses half its value
/// every 360 minutes of no redemptions.
pub const BASE_RATE_DECAY_PER_MINUTE: u128 = 998_076_443_575_628_823_990_894_592;

/// Spike dampener (Liquity `BETA`): a redemption of `f` = (redeemed / market debt) raises the
/// base-rate by `f / BETA`. Higher BETA ⇒ a gentler spike. Placeholder pending calibration.
pub const BASE_RATE_BETA: u128 = 2;

/// Decay a stored `base_rate` (RAY) forward by `secs_elapsed` since its last update. Decays per
/// WHOLE MINUTE (Liquity's granularity): sub-minute elapsed rounds down, so a burst of redemptions
/// within one minute is not each charged a decay tick. Returns the input unchanged on a
/// zero/negative interval or a zero base-rate. Saturates to 0 on the (unreachable) overflow path.
pub fn decay_base_rate(base_rate: u128, secs_elapsed: i64) -> u128 {
    if base_rate == 0 || secs_elapsed <= 0 {
        return base_rate;
    }
    let minutes = (secs_elapsed as u64) / 60;
    if minutes == 0 {
        return base_rate;
    }
    let factor = ray_pow(BASE_RATE_DECAY_PER_MINUTE, minutes).unwrap_or(0);
    ray_mul(base_rate, factor).unwrap_or(0)
}

/// Bump the (already-decayed) `base_rate` after redeeming `redeemed` fUSD against a market carrying
/// `total_debt` fUSD: `base += (redeemed / total_debt) / BETA`. `total_debt == 0` or `redeemed == 0`
/// ⇒ no bump. The result saturates at `RAY` (100%); the effective fee is clamped to bps below, so a
/// saturated base-rate just means "max dynamic fee".
pub fn bump_base_rate(base_rate: u128, redeemed: u128, total_debt: u128) -> u128 {
    if total_debt == 0 || redeemed == 0 || BASE_RATE_BETA == 0 {
        return base_rate;
    }
    // redeemed / total_debt as a RAY fraction, dampened by BETA. `redeemed <= total_debt` in
    // practice (you can't redeem more than the market's debt), so frac <= RAY; mul_div_floor caps it
    // regardless via the saturating add + min below.
    let frac = mul_div_floor(redeemed, RAY, total_debt).unwrap_or(0) / BASE_RATE_BETA;
    base_rate.saturating_add(frac).min(RAY)
}

/// The effective redemption fee (bps) for a redemption: the flat `floor_bps`
/// (`Market.redemption_fee_bps`) PLUS the dynamic base-rate expressed in bps, the dynamic add itself
/// capped by `dynamic_max_bps`, and the whole thing clamped to `[floor_bps, cap_bps]`
/// (`cap_bps == MAX_REDEMPTION_FEE_BPS`).
///
/// `dynamic_max_bps == 0` DISABLES the dynamic component ⇒ returns `min(floor_bps, cap_bps)`, i.e.
/// exactly the pre-C9 flat fee. The base-rate→bps conversion floors (RAY / 10_000 per bp).
pub fn effective_fee_bps(base_rate: u128, floor_bps: u16, dynamic_max_bps: u16, cap_bps: u16) -> u16 {
    let dyn_bps = base_rate / (RAY / BPS_DENOMINATOR); // RAY/10_000 == 1e23 per bp
    let dyn_capped = dyn_bps.min(dynamic_max_bps as u128);
    let eff = (floor_bps as u128 + dyn_capped).min(cap_bps as u128);
    eff as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decay_halves_over_the_half_life() {
        // Start at 100% (RAY); after 360 minutes (6h) it should be ~50%.
        let half = decay_base_rate(RAY, 360 * 60);
        let expected = RAY / 2;
        // within 0.1% of RAY/2 (the constant is floored to 27 digits).
        assert!(half.abs_diff(expected) < RAY / 1000, "got {half}, want ~{expected}");
        // Two half-lives ⇒ ~25%.
        let quarter = decay_base_rate(RAY, 720 * 60);
        assert!(quarter.abs_diff(RAY / 4) < RAY / 1000, "got {quarter}");
    }

    #[test]
    fn decay_is_monotone_and_bounded() {
        let r = RAY / 3;
        assert_eq!(decay_base_rate(r, 0), r, "no time ⇒ unchanged");
        assert_eq!(decay_base_rate(r, -5), r, "negative interval ⇒ unchanged");
        assert_eq!(decay_base_rate(r, 59), r, "sub-minute ⇒ unchanged (whole-minute granularity)");
        assert!(decay_base_rate(r, 60) < r, "one minute ⇒ strictly decayed");
        assert_eq!(decay_base_rate(0, 10_000), 0, "zero stays zero");
        // Decays toward 0, never below.
        let yr = decay_base_rate(RAY, 365 * 24 * 3600);
        assert!(yr < RAY / 1_000_000, "a year ⇒ ~0, got {yr}");
    }

    #[test]
    fn bump_scales_with_redeemed_fraction() {
        // Redeem 10% of a market's debt ⇒ base rises by 10%/BETA = 5% (RAY/20).
        let b = bump_base_rate(0, 100, 1_000);
        assert!(b.abs_diff(RAY / 20) < RAY / 100_000, "10%/2 = 5%, got {b}");
        // Redeem 100% ⇒ +50% (RAY/2).
        let full = bump_base_rate(0, 1_000, 1_000);
        assert!(full.abs_diff(RAY / 2) < RAY / 100_000, "100%/2 = 50%, got {full}");
        // Accumulates on an existing base-rate.
        let stacked = bump_base_rate(RAY / 20, 100, 1_000);
        assert!(stacked.abs_diff(RAY / 10) < RAY / 100_000, "5% + 5% = 10%, got {stacked}");
    }

    #[test]
    fn bump_edge_cases() {
        assert_eq!(bump_base_rate(RAY / 4, 0, 1_000), RAY / 4, "zero redeemed ⇒ no bump");
        assert_eq!(bump_base_rate(RAY / 4, 100, 0), RAY / 4, "zero debt ⇒ no bump");
        // Saturates at RAY (can't exceed 100%).
        assert_eq!(bump_base_rate(RAY, 1_000, 1_000), RAY, "saturates at RAY");
    }

    #[test]
    fn effective_fee_disabled_is_flat_floor() {
        // dynamic_max_bps == 0 ⇒ exactly the flat fee, regardless of base_rate.
        assert_eq!(effective_fee_bps(0, 50, 0, 500), 50);
        assert_eq!(effective_fee_bps(RAY, 50, 0, 500), 50, "huge base-rate ignored when disabled");
        assert_eq!(effective_fee_bps(RAY / 2, 0, 0, 500), 0, "floor 0 + disabled ⇒ 0");
    }

    #[test]
    fn effective_fee_adds_base_rate_over_the_floor() {
        // base_rate 1% (RAY/100) ⇒ 100 bps dynamic; floor 50 ⇒ 150 bps, under the 500 cap.
        assert_eq!(effective_fee_bps(RAY / 100, 50, 500, 500), 150);
        // Dynamic add capped by dynamic_max_bps (200): base 3% would be 300 bps but caps to 200 ⇒ 250.
        assert_eq!(effective_fee_bps(3 * RAY / 100, 50, 200, 500), 250);
        // Overall cap: floor 50 + dynamic 500 would be 550 but clamps to the 500 cap.
        assert_eq!(effective_fee_bps(5 * RAY / 100, 50, 500, 500), 500);
    }

    #[test]
    fn effective_fee_is_monotone_in_base_rate() {
        let mut prev = 0u16;
        for k in 0..=50u128 {
            let f = effective_fee_bps(k * RAY / 1000, 50, 5000, 5000);
            assert!(f >= prev, "fee must be non-decreasing in base_rate");
            prev = f;
        }
    }
}
