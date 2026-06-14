//! Per-position interest — the Liquity-v2 / BOLD **weighted-debt-sum** accrual primitives.
//!
//! fUSD gives each borrower their own interest rate (`user_rate_bps`) while the market still accrues
//! interest in **O(1)** (no per-position loop — the Solana per-writable-account CU constraint). The
//! mechanism (fusion-docs.md):
//!
//! - **Aggregate (per market):** `agg_weighted_debt_sum = Σ recorded_debt_i · rate_bps_i`. Over an
//!   elapsed `dt` the whole market owes `agg_weighted_debt_sum · dt / (SECONDS_PER_YEAR · 10_000)` of
//!   new interest — one multiply, no loop. [`pending_aggregate_interest`].
//! - **Per position (realized on touch):** `recorded_debt · rate_bps · period / (SECONDS_PER_YEAR ·
//!   10_000)` is folded into the position's `recorded_debt`. [`accrued_interest`].
//!
//! **Linear** between touches (a single multiply by elapsed time — BOLD parity; per-second
//! compounding can't be O(1) with per-position rates); interest compounds **across** touches because
//! each realize capitalizes it into `recorded_debt`.
//!
//! **Rounding (load-bearing for solvency):** the aggregate rounds **UP** (ceil — against borrowers),
//! the per-position realize rounds **DOWN** (floor). Since both are the floor/ceil of the *same*
//! quantity `debt · rate · time / denom`, the aggregate is never short of the sum of per-position
//! realizations — the protocol can never mint less interest than it books as debt
//! ([`pending_aggregate_interest`] ≥ [`accrued_interest`] for one position; proven in Kani).
//!
//! **Scale:** debt is fUSD-native units (`u128`); rate is **bps** (`rate_bps`, ≤ `MAX_USER_RATE_BPS`).
//! At bps scale `agg_weighted_debt_sum = Σ debt·rate_bps` stays in `u128` with ~16 orders of magnitude
//! of headroom (Σdebt for a $1T market ≈ 1e18 native · 2550 ≈ 2.6e21 ≪ `u128::MAX`), so no U256 state
//! is needed; the only U256 work is inside [`mul_div_floor`]/[`mul_div_ceil`] (exact, fail-closed).

use crate::{mul_div_ceil, mul_div_floor};

/// Seconds in a financial year (365·86400) — BOLD's `ONE_YEAR`, **not** 365.25 (a financial constant).
pub const SECONDS_PER_YEAR: u128 = 31_536_000;
/// Basis-point denominator for the rate scale.
pub const INTEREST_RATE_DENOM: u128 = 10_000;
/// The combined interest denominator `SECONDS_PER_YEAR · 10_000` = 3.1536e11. A position's annual
/// interest is `recorded_debt · rate_bps / 10_000`; spread over a year that is `… · dt / SECONDS_PER_YEAR`,
/// i.e. `recorded_debt · rate_bps · dt / INTEREST_DENOM`. A compile-time constant, so the divide stays
/// cheap (divide-by-constant) for both the production `bnum` path and the `cfg(kani)` shim.
pub const INTEREST_DENOM: u128 = SECONDS_PER_YEAR * INTEREST_RATE_DENOM;

/// A position's contribution to `Market.agg_weighted_debt_sum`: `recorded_debt · rate_bps`. `None`
/// only on the absurd overflow `recorded_debt > u128::MAX / rate_bps` (a > ~1e34-native position) —
/// fail-closed, never a wrap. The exact value the aggregate add-then-subtract delta uses on every touch.
#[inline]
pub fn weighted_debt(recorded_debt: u128, rate_bps: u16) -> Option<u128> {
    recorded_debt.checked_mul(rate_bps as u128)
}

/// Aggregate pending interest over `dt` seconds for a market whose positions sum to `weighted_sum`
/// (`= Σ recorded_debt_i · rate_bps_i`): `ceil(weighted_sum · dt / INTEREST_DENOM)`.
///
/// **Rounds UP** (against borrowers) so the minted aggregate interest is never short of the sum of the
/// per-position [`accrued_interest`] realizations (the solvency direction; see the module rounding note).
/// `None` if the quotient exceeds `u128::MAX` (fail-closed — caller reverts).
#[inline]
pub fn pending_aggregate_interest(weighted_sum: u128, dt: u64) -> Option<u128> {
    mul_div_ceil(weighted_sum, dt as u128, INTEREST_DENOM)
}

/// A single position's interest accrued over `period` seconds at its own `rate_bps`:
/// `floor(recorded_debt · rate_bps · period / INTEREST_DENOM)`.
///
/// **Rounds DOWN** (BOLD per-trove `_calcInterest`); the aggregate [`pending_aggregate_interest`] ceil
/// covers the protocol's margin. Computed as `floor((recorded_debt · rate_bps) · period / denom)` —
/// the weighted term is formed exactly first (no precision loss), then one exact 256-bit `mul_div`.
/// `None` on the absurd `recorded_debt · rate_bps` overflow or a quotient past `u128::MAX` (fail-closed).
#[inline]
pub fn accrued_interest(recorded_debt: u128, rate_bps: u16, period: u64) -> Option<u128> {
    let weighted = weighted_debt(recorded_debt, rate_bps)?;
    mul_div_floor(weighted, period as u128, INTEREST_DENOM)
}

/// Upfront fee for a **premature** interest-rate adjustment (BOLD anti-gaming):
/// one `period_secs` of interest at `rate_bps` on `recorded_debt`, rounded **UP** against the borrower:
/// `ceil(recorded_debt · rate_bps · period / INTEREST_DENOM)`.
///
/// Charged when a borrower changes their rate within the cooldown of their last change, so reactive
/// rate-jumping to dodge the redemption queue costs ~`period` of interest each time (the dodge becomes
/// self-defeating). Same shape as [`accrued_interest`] but ceil (a one-time charge, not linear accrual).
/// `None` on the absurd `recorded_debt · rate_bps` overflow or a quotient past `u128::MAX` (fail-closed).
#[inline]
pub fn premature_adjustment_fee(recorded_debt: u128, rate_bps: u16, period_secs: u64) -> Option<u128> {
    let weighted = weighted_debt(recorded_debt, rate_bps)?;
    mul_div_ceil(weighted, period_secs as u128, INTEREST_DENOM)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bnum::types::U256;
    use proptest::prelude::*;

    // fUSD-native debt: $1 = 1e6 (6 decimals). $1000 = 1e9 native.
    const D_1K: u128 = 1_000_000_000;

    #[test]
    fn one_year_at_5pct_is_50_dollars() {
        // 5% (500 bps) on $1000 for a full year = $50 = 50e6 native.
        assert_eq!(accrued_interest(D_1K, 500, SECONDS_PER_YEAR as u64), Some(50_000_000));
        // Half a year = $25 (linear in time).
        assert_eq!(accrued_interest(D_1K, 500, (SECONDS_PER_YEAR / 2) as u64), Some(25_000_000));
        // Min rate 0.5% (50 bps) on $1000 for a year = $5.
        assert_eq!(accrued_interest(D_1K, 50, SECONDS_PER_YEAR as u64), Some(5_000_000));
        // Max rate 25.5% (2550 bps) on $1000 for a year = $255.
        assert_eq!(accrued_interest(D_1K, 2_550, SECONDS_PER_YEAR as u64), Some(255_000_000));
    }

    #[test]
    fn premature_fee_is_ceil_of_period_interest() {
        // 7 days of 5% interest on $1000 = 50e6 * 7/365 = 958_904.10..; ceil ⇒ 958_905.
        let week = 7 * 86_400;
        assert_eq!(premature_adjustment_fee(D_1K, 500, week), Some(958_905));
        // A full year equals the full annual interest (matches accrued_interest at the year boundary).
        assert_eq!(
            premature_adjustment_fee(D_1K, 500, SECONDS_PER_YEAR as u64),
            Some(50_000_000)
        );
        // Cooldown 0 ⇒ no fee; zero debt ⇒ no fee.
        assert_eq!(premature_adjustment_fee(D_1K, 500, 0), Some(0));
        assert_eq!(premature_adjustment_fee(0, 500, week), Some(0));
        // Ceil vs the floor of accrued_interest: the fee is never short of the linear accrual.
        let fee = premature_adjustment_fee(D_1K + 1, 1_337, week).unwrap();
        let floor = accrued_interest(D_1K + 1, 1_337, week).unwrap();
        assert!(fee >= floor && fee - floor <= 1);
        // Fail-closed on the absurd weighted overflow.
        assert_eq!(premature_adjustment_fee(u128::MAX, 2, 1), None);
    }

    #[test]
    fn accrued_zero_at_boundaries() {
        assert_eq!(accrued_interest(D_1K, 500, 0), Some(0)); // no time
        assert_eq!(accrued_interest(D_1K, 0, SECONDS_PER_YEAR as u64), Some(0)); // no rate
        assert_eq!(accrued_interest(0, 500, SECONDS_PER_YEAR as u64), Some(0)); // no debt
        // Sub-resolution: a tiny debt over a short period floors to 0 (the floor direction).
        assert_eq!(accrued_interest(1, 50, 1), Some(0));
    }

    #[test]
    fn accrued_fails_closed_on_overflow() {
        // recorded_debt · rate_bps overflows u128 -> None (no wrap).
        assert_eq!(accrued_interest(u128::MAX, 2, 1), None);
        assert_eq!(accrued_interest(u128::MAX / 100, 2_550, 1), None);
        // A weighted term that fits u128 stays Some (no spurious None): huge debt, rate 1 bps, 1s.
        assert!(accrued_interest(u128::MAX / 100_000, 1, 1).is_some());
    }

    #[test]
    fn pending_aggregate_rounds_up_and_matches_sum() {
        // One position: weighted = $1000 · 500 bps. Over a year, aggregate == per-position here.
        let weighted = weighted_debt(D_1K, 500).unwrap();
        assert_eq!(
            pending_aggregate_interest(weighted, SECONDS_PER_YEAR as u64),
            Some(50_000_000)
        );
        // A remainder rounds UP (ceil), unlike the per-position floor.
        // weighted+1 over a year = (500e9+1)/10_000 = 50_000_000.0001 -> ceil 50_000_001.
        assert_eq!(
            pending_aggregate_interest(weighted + 1, SECONDS_PER_YEAR as u64),
            Some(50_000_001)
        );
        // The same numerator floored (per-position) stays 50_000_000.
        assert_eq!(mul_div_floor(weighted + 1, SECONDS_PER_YEAR, INTEREST_DENOM), Some(50_000_000));
    }

    #[test]
    fn aggregate_never_short_of_position() {
        // The solvency direction: for any single position, the aggregate ceil >= the per-position floor.
        for &(d, r, t) in &[
            (D_1K, 500u16, SECONDS_PER_YEAR as u64),
            (1, 50, 1),
            (7, 13, 99),
            (D_1K + 1, 2_549, 1234),
            (123_456_789, 777, 86_400),
        ] {
            let agg = pending_aggregate_interest(weighted_debt(d, r).unwrap(), t).unwrap();
            let pos = accrued_interest(d, r, t).unwrap();
            assert!(agg >= pos, "aggregate {agg} < position {pos} for ({d},{r},{t})");
            // They differ by at most 1 (ceil vs floor of the same quantity).
            assert!(agg - pos <= 1);
        }
    }

    /// Differential test: `accrued_interest`/`pending_aggregate_interest` vs an independent U256
    /// reference over a wide deterministic input sweep. The always-on correctness guarantee.
    #[test]
    fn matches_u256_reference() {
        let denom = U256::from(INTEREST_DENOM);
        // A small LCG over (debt, rate, period) — wide but bounded so the weighted term fits u128.
        let mut s: u128 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            s
        };
        for _ in 0..20_000 {
            // debt up to ~1e15 native ($1B), rate up to 2550 bps, period up to ~20 years.
            let debt = (next() % 1_000_000_000_000_000) as u128;
            let rate = (next() % 2_551) as u16;
            let period = (next() % 631_152_000) as u64; // 0..~20y

            let w = U256::from(debt) * U256::from(rate as u128);
            let num = w * U256::from(period as u128);
            let ref_floor = u128::try_from(num / denom).ok();
            let ref_ceil = {
                let q = num / denom;
                let c = if num % denom != U256::ZERO { q + U256::ONE } else { q };
                u128::try_from(c).ok()
            };

            assert_eq!(accrued_interest(debt, rate, period), ref_floor, "floor ({debt},{rate},{period})");
            let weighted = weighted_debt(debt, rate).unwrap();
            assert_eq!(
                pending_aggregate_interest(weighted, period),
                ref_ceil,
                "ceil ({debt},{rate},{period})"
            );
            // The conservation direction holds on every sample.
            if let (Some(a), Some(p)) = (ref_ceil, ref_floor) {
                assert!(a >= p);
            }
        }
    }

    // --- proptest fuzz (B8): the always-on differential & rounding-direction guarantees over WIDE
    // random inputs, asserting the SAME properties the Kani harnesses prove on tiny domains.
    //
    // Generators keep the weighted term `debt·rate` inside u128 (the function's documented precondition
    // for a non-None result): debt up to ~1e30 native against a ≤2550-bps rate stays ≪ u128::MAX, so
    // these never spuriously hit the fail-closed path — that path is fuzzed separately below.

    // Independent U256 reference for floor/ceil of `debt·rate·period / INTEREST_DENOM`. NOT a re-run of
    // the production `bnum`/mul_div path — a direct big-integer recompute.
    fn ref_floor_ceil(debt: u128, rate: u16, period: u64) -> (Option<u128>, Option<u128>) {
        let denom = U256::from(INTEREST_DENOM);
        let num = U256::from(debt) * U256::from(rate as u128) * U256::from(period as u128);
        let q = num / denom;
        let floor = u128::try_from(q).ok();
        let ceil = if num % denom != U256::ZERO {
            u128::try_from(q + U256::ONE).ok()
        } else {
            floor
        };
        (floor, ceil)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        // accrued_interest == independent U256 FLOOR of debt·rate·period/denom (formula + floor direction).
        #[test]
        fn accrued_matches_u256_floor(
            debt in 0u128..=1_000_000_000_000_000_000_000_000_000_000, // up to ~1e30 native
            rate in 0u16..=2_550,
            period in 0u64..=631_152_000,                              // 0..~20 years
        ) {
            let (floor, _) = ref_floor_ceil(debt, rate, period);
            prop_assert_eq!(accrued_interest(debt, rate, period), floor);
        }

        // premature_adjustment_fee == independent U256 CEIL (round-UP against the borrower).
        #[test]
        fn premature_fee_matches_u256_ceil(
            debt in 0u128..=1_000_000_000_000_000_000_000_000_000_000,
            rate in 0u16..=2_550,
            period in 0u64..=631_152_000,
        ) {
            let (_, ceil) = ref_floor_ceil(debt, rate, period);
            prop_assert_eq!(premature_adjustment_fee(debt, rate, period), ceil);
        }

        // pending_aggregate_interest(weighted, dt) == independent U256 CEIL of weighted·dt/denom
        // (the aggregate rounds UP — the solvency margin). `weighted` generated directly, wide.
        #[test]
        fn aggregate_matches_u256_ceil(
            weighted in 0u128..=1_000_000_000_000_000_000_000_000_000_000,
            dt in 0u64..=631_152_000,
        ) {
            let denom = U256::from(INTEREST_DENOM);
            let num = U256::from(weighted) * U256::from(dt as u128);
            let q = num / denom;
            let expect = if num % denom != U256::ZERO {
                u128::try_from(q + U256::ONE).ok()
            } else {
                u128::try_from(q).ok()
            };
            prop_assert_eq!(pending_aggregate_interest(weighted, dt), expect);
        }

        // SOLVENCY/no-drift direction: for one position the aggregate ceil is never short of the
        // per-position floor, and the two differ by at most 1 (ceil vs floor of the same quantity).
        #[test]
        fn aggregate_never_short_of_position_fuzz(
            debt in 0u128..=1_000_000_000_000_000_000_000_000_000_000,
            rate in 0u16..=2_550,
            period in 0u64..=631_152_000,
        ) {
            let weighted = weighted_debt(debt, rate).unwrap(); // precondition: debt·rate fits u128
            let agg = pending_aggregate_interest(weighted, period).unwrap();
            let pos = accrued_interest(debt, rate, period).unwrap();
            prop_assert!(agg >= pos, "aggregate {} < position {}", agg, pos);
            prop_assert!(agg - pos <= 1);
        }

        // The premature fee (ceil) is never short of the linear accrual (floor), differing by ≤1.
        #[test]
        fn premature_fee_never_below_accrued(
            debt in 0u128..=1_000_000_000_000_000_000_000_000_000_000,
            rate in 0u16..=2_550,
            period in 0u64..=631_152_000,
        ) {
            let fee = premature_adjustment_fee(debt, rate, period).unwrap();
            let accrued = accrued_interest(debt, rate, period).unwrap();
            prop_assert!(fee >= accrued);
            prop_assert!(fee - accrued <= 1);
        }

        // FAIL-CLOSED: when debt·rate overflows u128, every weighted-term function returns None (never
        // wraps). Generator forces the overflow: debt > u128::MAX/rate with rate >= 2.
        #[test]
        fn weighted_overflow_fails_closed(
            rate in 2u16..=u16::MAX,
            extra in 1u128..=1_000_000,
        ) {
            let debt = (u128::MAX / rate as u128).saturating_add(extra); // debt·rate > u128::MAX
            prop_assert_eq!(weighted_debt(debt, rate), None);
            prop_assert_eq!(accrued_interest(debt, rate, 1), None);
            prop_assert_eq!(premature_adjustment_fee(debt, rate, 1), None);
        }
    }
}
