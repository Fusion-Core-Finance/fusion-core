//! Pure price-scaling for the oracle wiring (fusion-docs.md). Converts feed prices into
//! the RAY-scaled units `Market.spot` uses, and CLMM `sqrt_price` (Q64.64) into a RAY-scaled
//! quote-per-base price for the DEX-TWAP corridor. All intermediates go through 256-bit
//! ([`bnum`]) — `sqrt_price²` alone is ~256 bits, and `price · RAY` overflows `u128`.
//!
//! Scale conventions:
//! - **`*_ray` price** = RAY-scaled "USD (quote whole-token) per whole base token". Pyth/Switchboard
//!   and the DEX TWAP all normalize to this before [`crate`]-external `aggregate`, so the corridor
//!   compares like with like.
//! - **`Market.spot`** = RAY-scaled fUSD-native per *native* collateral unit. `usd_ray_to_spot`
//!   bridges the two: `spot = usd_ray · 10^fusd_decimals / 10^coll_decimals`.

use bnum::types::U256;

use crate::RAY;

/// `10^e` as U256, guarded (e ≤ 60 keeps it well under 2^256). None if `e` is too large.
fn ten_pow(e: u32) -> Option<U256> {
    if e > 60 {
        return None;
    }
    Some(U256::from(10u128).pow(e))
}

/// RAY-scaled value of `price · 10^expo` — e.g. a Pyth/Switchboard `(price, expo)` → RAY-scaled
/// USD per whole token. `expo` is typically negative. None on overflow.
pub fn px_to_ray(price: u128, expo: i32) -> Option<u128> {
    let base = U256::from(price).checked_mul(U256::from(RAY))?;
    let scaled = if expo < 0 {
        base / ten_pow(expo.unsigned_abs())?
    } else {
        base.checked_mul(ten_pow(expo as u32)?)?
    };
    u128::try_from(scaled).ok()
}

/// `Market.spot` (RAY-scaled fUSD-native per native collateral unit) from a RAY-scaled USD price
/// per *whole* collateral token: `spot = usd_ray · 10^fusd_decimals / 10^coll_decimals`. None on
/// overflow. (Matches the test harness's `spot_for_usd`.)
pub fn usd_ray_to_spot(usd_ray: u128, coll_decimals: u8, fusd_decimals: u8) -> Option<u128> {
    let num = U256::from(usd_ray).checked_mul(ten_pow(fusd_decimals as u32)?)?;
    u128::try_from(num / ten_pow(coll_decimals as u32)?).ok()
}

/// RAY-scaled price = **quote whole-tokens per base whole-token** from a Q64.64 `sqrt_price`
/// (= `sqrt(quote-native / base-native)`, as Orca Whirlpool / Raydium CLMM store it). The raw
/// native price is `sqrt_price² / 2^128`; this then RAY-scales and whole-token-adjusts:
/// `(sqrt_price² / 2^128) · RAY · 10^base_decimals / 10^quote_decimals`.
///
/// Staged so no intermediate exceeds 256 bits (`sqrt_price²` ≈ 2^192; multiplying by RAY directly
/// would overflow, so the `/2^128` is split around the RAY multiply). None on zero/overflow.
/// If the collateral is the *quote* side of the pool, the caller inverts the result.
pub fn sqrt_price_q64_to_ray(
    sqrt_price: u128,
    base_decimals: u8,
    quote_decimals: u8,
) -> Option<u128> {
    if sqrt_price == 0 {
        return None;
    }
    let sp = U256::from(sqrt_price);
    let sq = sp.checked_mul(sp)?; // sqrt_price^2, Q128.128 (≤ ~2^192)
    // price_raw_ray = (sq >> 128) · RAY, computed as ((sq >> 64) · RAY) >> 64 to stay < 2^256.
    let q = sq >> 64u32; // Q128.64, ≤ ~2^128
    let price_raw_ray = q.checked_mul(U256::from(RAY))? >> 64u32; // = price_raw · RAY
    let num = price_raw_ray.checked_mul(ten_pow(base_decimals as u32)?)?;
    u128::try_from(num / ten_pow(quote_decimals as u32)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ── Property/fuzz tests ─────────────────────────────────────────────────────────────────────
    // These hammer the oracle price-scaling / Q64.64-decode functions over wide random inputs. The
    // Q64.64 decode is LOSSY (two `>>64` truncations), so the value checks assert membership in a
    // BOUNDED error band against an INDEPENDENT f64 recompute — never exact equality. The exact
    // integer paths (`px_to_ray`, `usd_ray_to_spot`) assert exact, against a 128-bit recompute.
    //
    // RAY = 1e27 has 90 bits; `f64` has 53 bits of mantissa, so an f64 reference cannot represent a
    // RAY-scaled value to the unit. The value checks therefore use a RELATIVE tolerance (a few ulp
    // of f64 plus the documented Q64.64 truncation), which is the honest bound for this decode.

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        // ── px_to_ray ───────────────────────────────────────────────────────────────────────────

        // NEVER PANICS: any (price, expo) returns Some/None, never wraps or crashes. Wide price
        // (including 0 and u128::MAX) and the full i32 expo range.
        #[test]
        fn px_to_ray_never_panics(price in any::<u128>(), expo in any::<i32>()) {
            let _ = px_to_ray(price, expo);
        }

        // MONOTONIC in price at a fixed expo: a larger price yields a larger-or-equal output (no
        // inversions). Compares two outputs from two ordered inputs — no reference needed. Only
        // asserted when BOTH are Some (an out-of-contract overflow → None is correct behavior).
        #[test]
        fn px_to_ray_monotonic_in_price(
            a in any::<u128>(),
            b in any::<u128>(),
            expo in -40i32..=20,
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            if let (Some(rlo), Some(rhi)) = (px_to_ray(lo, expo), px_to_ray(hi, expo)) {
                prop_assert!(rlo <= rhi, "px_to_ray inverted: {rlo} > {rhi} (lo {lo}, hi {hi}, expo {expo})");
            }
        }

        // EXACT value against an INDEPENDENT 128-bit-domain reference computed in a WIDER type
        // (i128 headroom via u256-free split is awkward here, so the reference uses the same RAY but
        // a banded compute), over the FULL price range (0..=u128::MAX) at NEGATIVE expo. The
        // reference divides EARLY (price/10^|expo| first, then × RAY) when that loses no precision
        // (price a clean multiple of 10^|expo|), and otherwise builds the exact value with bnum so
        // there is no overflow guard hiding the assertion. Every case where production returns Some
        // is checked exactly; every None is justified against the same wide reference.
        #[test]
        fn px_to_ray_matches_reference_neg_expo(
            price in any::<u128>(),
            expo in -60i32..=0,
        ) {
            use bnum::types::U256;
            let got = px_to_ray(price, expo);
            // Independent wide reference: (price · RAY) / 10^|expo|, all in U256 (never overflows for
            // these inputs since price·RAY ≤ 2^218 and 10^60 ≤ 2^200). Production must agree exactly
            // whenever the result fits u128, and return None exactly when it does not.
            let base = U256::from(price) * U256::from(RAY);
            let ten = U256::from(10u128).pow(expo.unsigned_abs());
            let reference = base / ten;
            match u128::try_from(reference) {
                Ok(want) => prop_assert_eq!(got, Some(want),
                    "px_to_ray exact mismatch (price {}, expo {})", price, expo),
                Err(_) => prop_assert_eq!(got, None,
                    "px_to_ray should fail closed when the true value exceeds u128 (price {}, expo {})",
                    price, expo),
            }
        }

        // EXACT value at NON-NEGATIVE expo against an independent U256 reference (price · RAY · 10^expo).
        // Covers the positive-exponent multiply branch the negative-expo property does not exercise.
        // The price domain is sized so the result ALWAYS fits u128 over the whole expo range
        // (price ≤ ~2^33, RAY ≈ 2^90, 10^2 ≈ 2^7 → product ≤ ~2^130... bounded below u128 by the
        // explicit cap), so the exact `Some(want)` assertion fires on EVERY case — not vacuously. The
        // separate `px_to_ray_overflow_fails_closed` property pins the None/overflow side.
        #[test]
        fn px_to_ray_matches_reference_pos_expo(
            price in 0u128..=3_000_000_000u128, // real Pyth-magnitude prices (e.g. $30 @ 1e8 feed)
            expo in 0i32..=2,
        ) {
            use bnum::types::U256;
            // price · RAY · 10^expo ≤ 3e9 · 1e27 · 100 = 3e38 < u128::MAX (3.4e38), so try_from succeeds.
            let reference = U256::from(price) * U256::from(RAY) * U256::from(10u128).pow(expo as u32);
            let want = u128::try_from(reference).expect("reference fits u128 by construction");
            prop_assert_eq!(px_to_ray(price, expo), Some(want),
                "px_to_ray exact mismatch (price {}, expo {})", price, expo);
        }

        // ── usd_ray_to_spot ─────────────────────────────────────────────────────────────────────

        // NEVER PANICS over wide usd_ray and the full u8 decimal domains.
        #[test]
        fn usd_ray_to_spot_never_panics(
            usd_ray in any::<u128>(),
            coll_dec in any::<u8>(),
            fusd_dec in any::<u8>(),
        ) {
            let _ = usd_ray_to_spot(usd_ray, coll_dec, fusd_dec);
        }

        // MONOTONIC in usd_ray at fixed decimals: a larger USD price yields a larger-or-equal spot.
        #[test]
        fn usd_ray_to_spot_monotonic(
            a in any::<u128>(),
            b in any::<u128>(),
            coll_dec in 0u8..=30,
            fusd_dec in 0u8..=30,
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            if let (Some(slo), Some(shi)) =
                (usd_ray_to_spot(lo, coll_dec, fusd_dec), usd_ray_to_spot(hi, coll_dec, fusd_dec))
            {
                prop_assert!(slo <= shi, "usd_ray_to_spot inverted: {slo} > {shi}");
            }
        }

        // EXACT value against an independent recompute on the domain where `usd_ray · 10^fusd_dec`
        // fits u128. (decimals ≤ 18 covers every real SPL mint; 0..=18 keeps the reference in u128.)
        #[test]
        fn usd_ray_to_spot_matches_reference(
            usd_ray in 0u128..=(u128::MAX / 1_000_000_000_000_000_000),
            coll_dec in 0u8..=18,
            fusd_dec in 0u8..=18,
        ) {
            let num = usd_ray.checked_mul(10u128.pow(fusd_dec as u32));
            let got = usd_ray_to_spot(usd_ray, coll_dec, fusd_dec);
            match num {
                Some(n) => prop_assert_eq!(got, Some(n / 10u128.pow(coll_dec as u32))),
                None => { let _ = got; } // production uses 256-bit; it may legitimately succeed where u128 overflows
            }
        }

        // ── sqrt_price_q64_to_ray ───────────────────────────────────────────────────────────────

        // NEVER PANICS over wide sqrt_price (0, 1, u128::MAX, and a max real Q64.64) and decimals.
        #[test]
        fn sqrt_price_never_panics(
            sqrt_price in any::<u128>(),
            base_dec in any::<u8>(),
            quote_dec in any::<u8>(),
        ) {
            let _ = sqrt_price_q64_to_ray(sqrt_price, base_dec, quote_dec);
        }

        // FAIL-CLOSED: sqrt_price == 0 is always None, for any decimals (documented guard).
        #[test]
        fn sqrt_price_zero_is_none(base_dec in any::<u8>(), quote_dec in any::<u8>()) {
            prop_assert_eq!(sqrt_price_q64_to_ray(0, base_dec, quote_dec), None);
        }

        // MONOTONIC in sqrt_price at fixed decimals: a larger sqrt_price yields a larger-or-equal
        // RAY price (the decode floors, so non-decreasing). Compares two ordered inputs directly.
        // sqrt_price kept ≤ 2^96 (a valid CLMM Q64.64 ≈ price 2^64; this is far above any real pool)
        // so sqrt_price² stays well inside 256 bits and both sides are Some.
        #[test]
        fn sqrt_price_monotonic(
            a in 1u128..=(1u128 << 96),
            b in 1u128..=(1u128 << 96),
            base_dec in 0u8..=12,
            quote_dec in 0u8..=12,
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            if let (Some(rlo), Some(rhi)) =
                (sqrt_price_q64_to_ray(lo, base_dec, quote_dec),
                 sqrt_price_q64_to_ray(hi, base_dec, quote_dec))
            {
                prop_assert!(rlo <= rhi, "sqrt_price decode inverted: {rlo} > {rhi} (lo {lo}, hi {hi})");
            }
        }

        // ROUND-TRIP against an INDEPENDENT f64 recompute `(sqrt_price/2^64)² · RAY · 10^base/10^quote`
        // to within a small RELATIVE tolerance. Error basis: the f64 reference dominates — `sqrt_price
        // as f64` loses ~2^-53 relative (sqrt_price here is up to 2^72 > the 53-bit mantissa), squaring
        // roughly doubles that to ~2^-52, and `RAY as f64` / `ray as f64` add a couple more ulp; the
        // two production `>>64` floors contribute only ~2^-64 relative and are negligible by comparison.
        // The achievable bound is therefore a few f64 ulp (~1e-15); 1e-12 is the tightest band that is
        // stable across this whole domain (three orders of headroom over the f64 ulp floor), and it is
        // ~3 orders TIGHTER than the prior 1e-9. Domain: real CLMM sqrt_price range, modest decimals,
        // non-trivial magnitude so the relative tolerance is meaningful.
        #[test]
        fn sqrt_price_decode_within_band(
            sqrt_price in (1u128 << 56)..=(1u128 << 72),
            base_dec in 0u8..=9,
            quote_dec in 0u8..=9,
        ) {
            if let Some(ray) = sqrt_price_q64_to_ray(sqrt_price, base_dec, quote_dec) {
                let sp = sqrt_price as f64 / 2f64.powi(64);
                let price_raw = sp * sp; // native quote/base
                let dec_adj = 10f64.powi(base_dec as i32 - quote_dec as i32);
                let expected = price_raw * (RAY as f64) * dec_adj;
                // Skip cases the f64 reference cannot meaningfully represent (overflow / underflow to 0).
                if expected.is_finite() && expected > 1.0 && expected < (u128::MAX as f64) {
                    let got = ray as f64;
                    let rel = (got - expected).abs() / expected;
                    prop_assert!(rel < 1e-12,
                        "decode outside band: got {got}, expected {expected}, rel {rel} (sqrt_price {sqrt_price})");
                }
            }
        }

        // ── fail-closed (None) on overflow ────────────────────────────────────────────────────────

        // FAIL-CLOSED: every function returns None (never Some(garbage)/a wrap) on CONSTRUCTED
        // overflow inputs. Inputs are built so the true value provably exceeds u128 (mirrors the
        // constructive pattern in interest.rs::weighted_overflow_fails_closed), so a wrapping
        // implementation returning Some(_) would fail here. Random generators almost never hit these
        // magnitudes, so this is the discriminating coverage.

        // px_to_ray, NEGATIVE-expo try_from path: u128::MAX · RAY / 10^|expo| = u128::MAX · 10^(27-|expo|)
        // (RAY = 10^27). For |expo| ≤ 26 the factor 10^(27-|expo|) ≥ 10 keeps the true value > u128::MAX,
        // so the final try_from must fail → None. (At |expo| = 27 it equals u128::MAX exactly — no
        // overflow — which is why the range stops at -26.)
        #[test]
        fn px_to_ray_overflow_fails_closed(expo in -26i32..=-1) {
            prop_assert_eq!(px_to_ray(u128::MAX, expo), None,
                "px_to_ray must fail closed on overflow (price u128::MAX, expo {})", expo);
        }

        // usd_ray_to_spot: u128::MAX · 10^fusd_dec / 10^coll_dec with fusd_dec > coll_dec forces the
        // numerator (and quotient) past u128 → None.
        #[test]
        fn usd_ray_to_spot_overflow_fails_closed(
            fusd_dec in 1u8..=38,
            coll_dec in 0u8..=0,
        ) {
            prop_assert_eq!(usd_ray_to_spot(u128::MAX, coll_dec, fusd_dec), None,
                "usd_ray_to_spot must fail closed on overflow (usd_ray u128::MAX, coll {}, fusd {})",
                coll_dec, fusd_dec);
        }

        // sqrt_price_q64_to_ray: a near-max sqrt_price makes (sqrt_price²>>128)·RAY·10^base exceed
        // u128 (and the staged 256-bit checked_mul or final try_from must catch it) → None. base_dec
        // ≥ quote_dec keeps the decimal adjustment from shrinking it back under u128.
        #[test]
        fn sqrt_price_overflow_fails_closed(
            base_dec in 0u8..=10,
            shift in 0u8..=10,
        ) {
            let quote_dec = base_dec.saturating_sub(shift);
            prop_assert_eq!(sqrt_price_q64_to_ray(u128::MAX, base_dec, quote_dec), None,
                "sqrt_price_q64_to_ray must fail closed on overflow (sqrt_price u128::MAX, base {}, quote {})",
                base_dec, quote_dec);
        }
    }

    #[test]
    fn px_to_ray_negative_expo() {
        // Pyth SOL $69.33 = price 6_933_000_000, expo -8 → 69.33 · RAY.
        assert_eq!(px_to_ray(6_933_000_000, -8), Some(RAY / 100 * 6933));
        // $1.00 with 8-dp feed.
        assert_eq!(px_to_ray(100_000_000, -8), Some(RAY));
    }

    #[test]
    fn px_to_ray_positive_expo() {
        assert_eq!(px_to_ray(5, 2), Some(RAY * 500)); // 5 · 10^2 = 500
    }

    #[test]
    fn usd_ray_to_spot_matches_harness() {
        // $100/token, 9-dec collateral, 6-dec fUSD → RAY/10 (the harness's documented example).
        assert_eq!(usd_ray_to_spot(RAY * 100, 9, 6), Some(RAY / 10));
        // $69/token → 69 · RAY/1000.
        assert_eq!(usd_ray_to_spot(RAY * 69, 9, 6), Some(RAY / 1000 * 69));
    }

    #[test]
    fn pyth_to_spot_roundtrip() {
        // Full Pyth→spot path: $100, 8-dp feed, 9-dec collateral.
        let usd_ray = px_to_ray(100_000_000_000, -9).unwrap(); // price 1e11, expo -9 = $100
        assert_eq!(usd_ray, RAY * 100);
        assert_eq!(usd_ray_to_spot(usd_ray, 9, 6), Some(RAY / 10));
    }

    #[test]
    fn sqrt_price_decode_proof() {
        // Verified Whirlpool WSOL/USDC sample: sqrt_price 4857170867873581308 → 69.33 USDC/SOL.
        // base = SOL (token_a, 9 dec), quote = USDC (token_b, 6 dec).
        let ray = sqrt_price_q64_to_ray(4_857_170_867_873_581_308, 9, 6).unwrap();
        // RAY-scaled ≈ 69.33 · RAY. Allow a small rounding band (Q64.64 truncation).
        let expected = RAY / 100 * 6933;
        let diff = ray.abs_diff(expected);
        assert!(diff < RAY / 100, "got {ray}, expected ~{expected} (diff {diff})");
    }

    #[test]
    fn sqrt_price_unit_price() {
        // sqrt_price = 2^64 → price_raw = 1.0 (native); equal decimals → 1.0 · RAY.
        let ray = sqrt_price_q64_to_ray(1u128 << 64, 6, 6).unwrap();
        assert_eq!(ray, RAY);
    }

    #[test]
    fn zero_and_overflow_guards() {
        assert_eq!(sqrt_price_q64_to_ray(0, 9, 6), None);
        assert_eq!(px_to_ray(u128::MAX, 30), None); // 10^30 · u128::MAX overflows
        assert_eq!(usd_ray_to_spot(u128::MAX, 0, 60), None);
    }
}
