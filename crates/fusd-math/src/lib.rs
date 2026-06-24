//! fUSD fixed-point money math.
//!
//! Dimensional discipline (fusion-docs.md):
//! - **WAD** = 1e18  — token quantities
//! - **RAY** = 1e27  — rates / prices / the interest accumulator
//! - **RAD** = 1e45  — internal balances (`art[wad] * rate[ray]`)
//!
//! Any product of two WAD/RAY-scale values overflows `u128` (e.g. `RAY*RAY` = 1e54),
//! so every multiply/divide goes through an exact **256-bit intermediate** ([`bnum`]),
//! never wrapping. Results that don't fit `u128` return `None` rather than truncating.
//!
//! Rounding policy (fusion-docs.md): round **against the protocol**. Debt-increasing
//! results round **up** (`*_up`); collateral credit / debt-decreasing results round
//! **down** (floor). Callers pick the variant for their direction; the convention is
//! documented per function.
//!
//! These are the primitives the accounting core is built from. The Reactor-Pool
//! product-sum (`P`/`S`) lives in [`reactor_pool`].

#![cfg_attr(not(test), no_std)]

use bnum::types::U256;

pub mod interest;
pub mod oracle_scale;
pub mod rate_bucket;
pub mod recovery;
pub mod redemption;
pub mod redistribution;
pub mod reactor_pool;

/// Kani formal-verification harnesses (bounded model checking). Compiled ONLY under `cargo kani`
/// (the `kani` cfg) — excluded from every normal/test/SBF build, so it can never affect production.
#[cfg(kani)]
mod kani_proofs;

/// 1e18
pub const WAD: u128 = 1_000_000_000_000_000_000;
/// 1e27
pub const RAY: u128 = 1_000_000_000_000_000_000_000_000_000;
pub const HALF_WAD: u128 = WAD / 2;
pub const HALF_RAY: u128 = RAY / 2;
/// Basis-point denominator.
pub const BPS_DENOMINATOR: u128 = 10_000;

// ---------------------------------------------------------------------------
// Core: exact `a * b / denom` over a 256-bit intermediate.
// ---------------------------------------------------------------------------

/// Exact `floor(a*b/denom)` and the "remainder is non-zero" flag, computed over a 256-bit
/// intermediate. `None` if `denom == 0` or the quotient exceeds `u128::MAX`. The single core both
/// [`mul_div_floor`] and [`mul_div_ceil`] are built from.
///
/// Two implementations selected at compile time, identical in result:
/// - **production** (`cfg(not(kani))`): `bnum` `U256` — exact, trusted, what ships.
/// - **`cfg(kani)`**: a CBMC-friendly hand `wide_mul` + binary long division ([`div_256_by_128`]).
///   `bnum`'s `basecase_div_rem` is a deep nested loop CBMC must fully unwind (minutes/harness); the
///   shim is a single flat loop (seconds). The proofs then verify THIS wrapper's floor/ceil/overflow
///   logic — `bnum`'s arithmetic is a trusted dependency — and the `shim_matches_bnum` differential
///   test (normal build) pins the shim ≡ `bnum`, so the shipped path stays validated. fusion-docs.md.
#[inline]
fn wide_mul_div(a: u128, b: u128, denom: u128) -> Option<(u128, bool)> {
    if denom == 0 {
        return None;
    }
    #[cfg(not(kani))]
    {
        let prod = U256::from(a) * U256::from(b);
        let d = U256::from(denom);
        let q = u128::try_from(prod / d).ok()?;
        Some((q, prod % d != U256::ZERO))
    }
    #[cfg(kani)]
    {
        let (hi, lo) = wide_mul(a, b);
        let (q, r) = div_256_by_128(hi, lo, denom)?;
        Some((q, r != 0))
    }
}

/// `floor(a * b / denom)`, computed exactly in 256 bits. `None` if `denom == 0` or the
/// quotient exceeds `u128::MAX`. The product `a * b` never overflows (both fit `u128`,
/// so the product fits 256 bits).
#[inline]
pub fn mul_div_floor(a: u128, b: u128, denom: u128) -> Option<u128> {
    wide_mul_div(a, b, denom).map(|(q, _)| q)
}

/// `ceil(a * b / denom)`. Same overflow/zero rules as [`mul_div_floor`] (a ceil that exceeds
/// `u128::MAX` returns `None`).
#[inline]
pub fn mul_div_ceil(a: u128, b: u128, denom: u128) -> Option<u128> {
    let (q, has_rem) = wide_mul_div(a, b, denom)?;
    if has_rem {
        q.checked_add(1)
    } else {
        Some(q)
    }
}

/// Full 256-bit product of two `u128`s as `(hi, lo)` — schoolbook over 64-bit limbs, no loop, so
/// CBMC handles it in one step. Used only by the `cfg(kani)` shim paths and the differential
/// tests (never the production path). `a*b = hi·2^128 + lo`.
#[cfg(any(kani, test))]
pub(crate) fn wide_mul(a: u128, b: u128) -> (u128, u128) {
    const M: u128 = u64::MAX as u128; // low-64 mask
    let (a0, a1) = (a & M, a >> 64);
    let (b0, b1) = (b & M, b >> 64);
    let lo_lo = a0 * b0;
    let mid1 = a1 * b0;
    let mid2 = a0 * b1;
    let hi_hi = a1 * b1;
    // Sum the 2^64-weighted middle terms with the carry out of the low 64 bits.
    let cross = (lo_lo >> 64) + (mid1 & M) + (mid2 & M);
    let lo = (lo_lo & M) | (cross << 64);
    let hi = hi_hi + (mid1 >> 64) + (mid2 >> 64) + (cross >> 64);
    (hi, lo)
}

/// `(hi·2^128 + lo) / denom` and its remainder, as `(quotient, remainder)`; `None` when the quotient
/// would exceed `u128::MAX` (exactly when `hi >= denom`). Restoring BINARY long division — one flat
/// 256-iteration loop with a carry-aware shift (the shifted remainder can momentarily reach `< 2·denom`,
/// which would overflow `u128`, so the lost top bit is carried in `carry`). Deliberately simple and
/// obviously-correct (CBMC-friendly); the `shim_matches_bnum` test validates it against `bnum`.
/// Caller guarantees `denom != 0`.
#[cfg(any(kani, test))]
pub(crate) fn div_256_by_128(hi: u128, lo: u128, denom: u128) -> Option<(u128, u128)> {
    if hi >= denom {
        return None; // quotient >= 2^128 — fail closed, exactly as the bnum try_from would
    }
    if hi == 0 {
        // Product fits u128 (the common case): one native division instead of the 256-bit loop —
        // far cheaper for CBMC. `shim_matches_bnum` validates this path too.
        return Some((lo / denom, lo % denom));
    }
    let mut rem: u128 = 0;
    let mut quo: u128 = 0;
    let mut i: u32 = 256;
    while i > 0 {
        i -= 1;
        let bit = if i >= 128 { (hi >> (i - 128)) & 1 } else { (lo >> i) & 1 };
        let carry = rem >> 127; // top bit that the shift below would drop
        rem = (rem << 1) | bit; // wraps; the true remainder is carry·2^128 + rem, and < 2·denom
        quo <<= 1;
        if carry == 1 || rem >= denom {
            rem = rem.wrapping_sub(denom); // true_rem - denom ∈ [0, denom); wrapping handles carry==1
            quo |= 1;
        }
    }
    Some((quo, rem))
}

/// `(a*b + add) / denom` as `(quotient, remainder)`, exact over a 256-bit intermediate — the
/// CBMC-friendly shim twin of the `bnum` `(a*b + add)/denom` computation in
/// [`redistribution::accumulate`](crate::redistribution). `None` if `denom == 0` or the quotient
/// exceeds `u128::MAX` (the remainder always fits, being `< denom`). Available under `kani`/`test`;
/// `accumulate` uses it under `cfg(kani)`, and `muladd_div_matches_bnum` pins it ≡ `bnum`.
#[cfg(any(kani, test))]
pub(crate) fn wide_muladd_div(a: u128, b: u128, add: u128, denom: u128) -> Option<(u128, u128)> {
    if denom == 0 {
        return None;
    }
    let (mut hi, lo) = wide_mul(a, b);
    let (lo, carry) = lo.overflowing_add(add);
    if carry {
        hi += 1; // `a*b` has hi ≤ 2^128-2, so the at-most-+1 carry can never overflow hi
    }
    div_256_by_128(hi, lo, denom)
}

// ---------------------------------------------------------------------------
// WAD (1e18) and RAY (1e27) fixed-point multiply / divide.
// ---------------------------------------------------------------------------

/// `floor(a * b / WAD)`.
#[inline]
pub fn wad_mul(a: u128, b: u128) -> Option<u128> {
    mul_div_floor(a, b, WAD)
}
/// `ceil(a * b / WAD)` — use when the result increases debt.
#[inline]
pub fn wad_mul_up(a: u128, b: u128) -> Option<u128> {
    mul_div_ceil(a, b, WAD)
}
/// `floor(a * WAD / b)`.
#[inline]
pub fn wad_div(a: u128, b: u128) -> Option<u128> {
    mul_div_floor(a, WAD, b)
}

/// `floor(a * b / RAY)`.
#[inline]
pub fn ray_mul(a: u128, b: u128) -> Option<u128> {
    mul_div_floor(a, b, RAY)
}
/// `ceil(a * b / RAY)` — use when the result increases debt.
#[inline]
pub fn ray_mul_up(a: u128, b: u128) -> Option<u128> {
    mul_div_ceil(a, b, RAY)
}
/// `floor(a * RAY / b)`.
#[inline]
pub fn ray_div(a: u128, b: u128) -> Option<u128> {
    mul_div_floor(a, RAY, b)
}

/// Present-value debt of a position: `art[wad] * rate[ray] / RAY` → `wad`, rounded **up**
/// (against the borrower). This is the `art * rate` realization from fusion-docs.md.
#[inline]
pub fn present_debt(art: u128, rate: u128) -> Option<u128> {
    ray_mul_up(art, rate)
}

/// `base^exp` in RAY fixed-point (Maker-style `rpow`), via binary exponentiation. `base`
/// and the result are RAY-scaled; `base^0 == RAY`. Each squaring/multiply floors (so the
/// result is a lower bound on the true power — conservative for an interest accumulator).
/// `None` on intermediate overflow of `u128`.
pub fn ray_pow(base: u128, mut exp: u64) -> Option<u128> {
    let mut z: u128 = if exp & 1 == 1 { base } else { RAY };
    let mut b = base;
    exp >>= 1;
    while exp > 0 {
        b = ray_mul(b, b)?;
        if exp & 1 == 1 {
            z = ray_mul(z, b)?;
        }
        exp >>= 1;
    }
    Some(z)
}

/// `floor(amount * bps / 10_000)`. `None` if the `u64` result overflows.
#[inline]
pub fn apply_bps(amount: u64, bps: u16) -> Option<u64> {
    let scaled = (amount as u128).checked_mul(bps as u128)? / BPS_DENOMINATOR;
    u64::try_from(scaled).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants() {
        assert_eq!(WAD, 10u128.pow(18));
        assert_eq!(RAY, 10u128.pow(27));
    }

    #[test]
    fn mul_div_floor_exact_and_floor() {
        assert_eq!(mul_div_floor(6, 7, 2), Some(21));
        assert_eq!(mul_div_floor(7, 1, 2), Some(3)); // floors
        assert_eq!(mul_div_floor(1, 1, 0), None); // div by zero
        // The cases the u128 scaffold could not do — now exact via 256-bit:
        assert_eq!(mul_div_floor(RAY, RAY, RAY), Some(RAY));
        assert_eq!(mul_div_floor(u128::MAX, u128::MAX, u128::MAX), Some(u128::MAX));
        // Quotient overflows u128 -> None (not a wrap).
        assert_eq!(mul_div_floor(u128::MAX, u128::MAX, 1), None);
    }

    #[test]
    fn mul_div_ceil_rounds_up() {
        assert_eq!(mul_div_ceil(7, 1, 2), Some(4)); // 3.5 -> 4
        assert_eq!(mul_div_ceil(6, 2, 3), Some(4)); // exact -> equals floor
        assert_eq!(mul_div_ceil(0, 5, 3), Some(0));
        // ceil >= floor, differ by exactly 1 when there's a remainder
        assert_eq!(mul_div_ceil(7, 3, 2).unwrap(), mul_div_floor(7, 3, 2).unwrap() + 1);
    }

    #[test]
    fn wad_ray_ops() {
        assert_eq!(wad_mul(2 * WAD, 3 * WAD), Some(6 * WAD));
        assert_eq!(ray_mul(RAY, RAY), Some(RAY)); // identity
        assert_eq!(ray_mul(2 * RAY, 2 * RAY), Some(4 * RAY));
        assert_eq!(wad_div(WAD, 2 * WAD), Some(WAD / 2));
        assert_eq!(ray_div(RAY, RAY), Some(RAY));
        // up vs floor differ by 1 on a non-zero remainder
        assert_eq!(ray_mul(RAY + 1, RAY + 1), Some(RAY + 2)); // floor((RAY+1)^2/RAY)
        assert_eq!(ray_mul_up(RAY + 1, RAY + 1), Some(RAY + 3));
        // `wad_mul_up` (the WAD ceil twin): exact multiple has no rounding; a non-zero remainder
        // rounds up against the debtor, one above the floor.
        assert_eq!(wad_mul_up(2 * WAD, 3 * WAD), Some(6 * WAD)); // exact
        assert_eq!(wad_mul(WAD + 1, WAD + 1), Some(WAD + 2)); // floor((WAD+1)^2/WAD)
        assert_eq!(wad_mul_up(WAD + 1, WAD + 1), Some(WAD + 3));
    }

    #[test]
    fn present_debt_rounds_up() {
        // Any fractional debt rounds UP against the borrower: 1 * (RAY+1)/RAY = 1 + 1/RAY -> 2.
        assert_eq!(present_debt(1, RAY + 1), Some(2));
        // No rounding when exact.
        assert_eq!(present_debt(5 * WAD, RAY), Some(5 * WAD));
        assert_eq!(present_debt(WAD, RAY), Some(WAD)); // rate 1.0 -> debt == art
        assert_eq!(present_debt(WAD, 2 * RAY), Some(2 * WAD)); // rate 2.0
    }

    #[test]
    fn ray_pow_works() {
        assert_eq!(ray_pow(123 * RAY, 0), Some(RAY)); // x^0 = 1.0
        assert_eq!(ray_pow(123, 1), Some(123)); // x^1 = x
        assert_eq!(ray_pow(RAY, 5), Some(RAY)); // 1.0^n = 1.0
        assert_eq!(ray_pow(2 * RAY, 3), Some(8 * RAY)); // 2^3 = 8
        assert_eq!(ray_pow(2 * RAY, 10), Some(1024 * RAY)); // 2^10
        // 1.1^2 = 1.21 (RAY-scaled), exact at this scale
        let one_point_one = RAY + RAY / 10;
        assert_eq!(ray_pow(one_point_one, 2), Some(RAY + RAY / 10 + RAY / 10 + RAY / 100));
    }

    #[test]
    fn bps() {
        assert_eq!(apply_bps(10_000, 500), Some(500)); // 5%
        assert_eq!(apply_bps(1_000_000, 11_000), Some(1_100_000)); // 110%
        assert_eq!(apply_bps(0, 9_999), Some(0));
        assert_eq!(apply_bps(u64::MAX, 65_535), None); // overflow on downcast
    }

    /// Deterministic differential check of `mul_div_*` against a `u128` reference, over
    /// inputs whose product fits `u128` (so the reference is valid). Broad-range and
    /// proptest-based fuzzing is a later milestone; this is a fast always-on guard.
    #[test]
    fn mul_div_matches_u128_reference() {
        let mut s: u128 = 0x2545_F491_4F6C_DD1D;
        let mut next = || {
            s = s
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            s
        };
        let mask: u128 = (1u128 << 60) - 1; // products < 2^120, always fit u128
        for _ in 0..5_000 {
            let a = next() & mask;
            let b = next() & mask;
            let d = (next() % (u64::MAX as u128)) + 1; // non-zero
            let prod = a * b; // fits by construction
            assert_eq!(mul_div_floor(a, b, d), Some(prod / d));
            let expect_ceil = prod / d + if prod % d != 0 { 1 } else { 0 };
            assert_eq!(mul_div_ceil(a, b, d), Some(expect_ceil));
        }
    }

    /// Differential validation that the `cfg(kani)` shim (`wide_mul` + `div_256_by_128`) computes
    /// EXACTLY what the production `bnum` path does — full-u128-range random inputs + edge cases.
    /// This is the link that keeps the shipped `bnum` path validated even though Kani verifies the
    /// shim instead of `bnum` (which Kani can't unwind tractably). `wide_mul`/`div_256_by_128` are
    /// available here via `cfg(any(kani, test))`.
    #[test]
    fn shim_matches_bnum() {
        fn check(a: u128, b: u128, d: u128) {
            // `wide_mul` reconstructs the exact 256-bit product.
            let (hi, lo) = wide_mul(a, b);
            let prod = U256::from(a) * U256::from(b);
            assert_eq!((U256::from(hi) << 128) + U256::from(lo), prod, "wide_mul wrong: {a}*{b}");
            if d == 0 {
                return; // div_256_by_128's caller guarantees denom != 0
            }
            let dd = U256::from(d);
            let q_ref = prod / dd;
            let r_ref = prod % dd;
            let fits = q_ref <= U256::from(u128::MAX);
            match div_256_by_128(hi, lo, d) {
                Some((q, r)) => {
                    assert!(fits, "shim Some but bnum quotient overflows u128: {a}*{b}/{d}");
                    assert_eq!(U256::from(q), q_ref, "quotient mismatch: {a}*{b}/{d}");
                    assert_eq!(U256::from(r), r_ref, "remainder mismatch: {a}*{b}/{d}");
                }
                None => assert!(!fits, "shim None but bnum quotient fits u128: {a}*{b}/{d}"),
            }
        }
        // Edge cases (incl. the overflow boundary and the WAD/RAY denominators fUSD actually uses).
        let edges = [
            0u128, 1, 2, WAD, RAY, u64::MAX as u128, (u64::MAX as u128) + 1, u128::MAX, u128::MAX - 1,
        ];
        for &a in &edges {
            for &b in &edges {
                for &d in &edges {
                    check(a, b, d);
                }
            }
        }
        // Full-range random (no 2^60 cap — the bnum reference is exact at 256 bits).
        let mut s: u128 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            s
        };
        for _ in 0..20_000 {
            let (a, b, d) = (next(), next(), next() | 1); // d != 0
            check(a, b, d);
        }
    }

    /// Differential validation that the `wide_muladd_div` shim (used by `redistribution::accumulate`
    /// under `cfg(kani)`) computes EXACTLY `(a*b + add)/denom` and its remainder, as the production
    /// `bnum` path does — full-u128-range random inputs + edge cases incl. the carry into `hi` and the
    /// `PRECISION` multiplier `accumulate` actually uses. Keeps the shipped `bnum` accumulate validated.
    #[test]
    fn muladd_div_matches_bnum() {
        fn check(a: u128, b: u128, add: u128, d: u128) {
            if d == 0 {
                assert_eq!(wide_muladd_div(a, b, add, d), None);
                return;
            }
            let num = U256::from(a) * U256::from(b) + U256::from(add);
            let dd = U256::from(d);
            let q_ref = num / dd;
            let r_ref = num % dd;
            let fits = q_ref <= U256::from(u128::MAX);
            match wide_muladd_div(a, b, add, d) {
                Some((q, r)) => {
                    assert!(fits, "shim Some but bnum quotient overflows u128: ({a}*{b}+{add})/{d}");
                    assert_eq!(U256::from(q), q_ref, "quotient mismatch: ({a}*{b}+{add})/{d}");
                    assert_eq!(U256::from(r), r_ref, "remainder mismatch: ({a}*{b}+{add})/{d}");
                }
                None => assert!(!fits, "shim None but bnum quotient fits u128: ({a}*{b}+{add})/{d}"),
            }
        }
        // The redistribution::PRECISION multiplier + small/edge stakes & errors.
        let edges = [0u128, 1, 2, 255, WAD, RAY, u64::MAX as u128, (u64::MAX as u128) + 1, u128::MAX];
        for &a in &edges {
            for &add in &edges {
                for &d in &edges {
                    check(a, crate::redistribution::PRECISION, add, d); // the accumulate shape
                    check(a, 2, add, d.max(1)); // a generic shape too
                }
            }
        }
        // Full-range random.
        let mut s: u128 = 0xDEAD_BEEF_CAFE_F00D;
        let mut next = || {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            s
        };
        for _ in 0..20_000 {
            check(next(), next(), next(), next() | 1);
        }
    }

    /// Differential validation of `ray_pow`'s binary-exponentiation LOOP (the squaring + odd-bit
    /// multiply) and its fail-closed overflow `?`, against an independent integer reference. Using
    /// integer-multiple-of-RAY bases (`k*RAY`) makes every intermediate `ray_mul` EXACT (no flooring),
    /// so `ray_pow(k*RAY, e) == k^e * RAY` when that fits `u128`, and `None` when `k^e` (or `*RAY`)
    /// overflows. This is the sweep the WEAK `ray_pow_identities` Kani harness cannot provide (a symbolic
    /// exponent unwinds the loop+divide per bit — intractable), so it is the primary guarantee for the
    /// `ray_pow` iteration the same way `shim_matches_bnum` is for `mul_div`.
    #[test]
    fn ray_pow_matches_reference() {
        fn checked_int_pow(k: u128, e: u32) -> Option<u128> {
            let mut acc: u128 = 1;
            for _ in 0..e {
                acc = acc.checked_mul(k)?;
            }
            Some(acc)
        }
        let mut s: u128 = 0xA5A5_5A5A_1234_9876;
        let mut next = || {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            s
        };
        for _ in 0..20_000 {
            let k = next() % 2000; // base factor 0..1999 — k*RAY <= 1999e27 < u128::MAX always fits
            let e = (next() % 40) as u64; // exponent 0..39 — a healthy mix of in-range and overflow
            // Intermediates never exceed the final (powers grow), so ray_pow returns None EXACTLY when
            // k^e * RAY overflows u128 — which `checked_int_pow(..).checked_mul(RAY)` predicts exactly.
            let expected = checked_int_pow(k, e as u32).and_then(|p| p.checked_mul(RAY));
            assert_eq!(ray_pow(k * RAY, e), expected, "k={k} e={e}");
        }
        // Explicit edges: x^0 == 1.0 for a non-trivial base; a squaring that overflows fails closed.
        assert_eq!(ray_pow(123 * RAY, 0), Some(RAY));
        assert_eq!(ray_pow(u128::MAX, 2), None);
    }
}
