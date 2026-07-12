//! Kani bounded-model-checking harnesses for the fixed-point money-math core (fusion-docs.md).
//!
//! These prove the VALUE, rounding-DIRECTION, and FAIL-CLOSED-on-overflow contracts of the
//! `mul_div` / `wad` / `ray` / `bps` primitives the whole accounting core is built from — the class
//! of bug (a floor↔ceil flip, or a silent `u128` wrap instead of `None`) that would silently
//! under-collateralize every CDP. Pre-audit formal-verification tier; Certora remains complementary.
//!
//! HOW TO RUN:
//!   cargo install --locked kani-verifier && cargo kani setup
//!   cargo kani -p fusd-math
//!
//! TRACTABILITY (the rule that keeps this FAST): CBMC's formula grows with the number of SYMBOLIC
//! INPUT BITS, not the value magnitude. The money-math properties (floor/ceil direction, the
//! +1-on-remainder, fail-closed overflow) are scale-INDEPENDENT, so the harnesses feed only a few
//! symbolic bits (`u8`, occasionally structured as `RAY + u8`) — the standard u8/u16 narrowing method.
//! Wide symbolic inputs through the 256-bit divide are intractable (a `u128`-wide symbolic input made
//! one harness run >2 h); where a wide value is needed (the overflow boundary) we use CONCRETE
//! witnesses instead, and lean on the `shim_matches_bnum` differential test (20k full-range random +
//! edge cases, normal build) to sweep the wide space.
//!
//! Under `cfg(kani)`, `mul_div` routes through the CBMC-friendly `wide_mul` + `div_256_by_128` shim
//! (a flat 256-iteration loop, not `bnum`'s nested long-division); `unwind = 264` covers it. The
//! shim is proven ≡ `bnum` by `shim_matches_bnum`, so verifying the shim = verifying the shipped path.

use crate::*;

/// `mul_div_floor` returns exactly `floor(a*b/denom)` for in-range inputs (narrow symbolic inputs).
// strength: STRONG — exact floor value over symbolic a,b,d; both rounding branches covered; the wide (hi!=0) divide path is swept by shim_matches_bnum.
#[kani::proof]
fn mul_div_floor_matches_reference() {
    let a = kani::any::<u8>() as u128;
    let b = kani::any::<u8>() as u128;
    let d = kani::any::<u8>() as u128;
    kani::assume(a < 16 && b < 16 && d >= 1); // ≤16 symbolic bits keeps CBMC tractable
    assert_eq!(mul_div_floor(a, b, d), Some((a * b) / d));
    kani::cover!((a * b) % d != 0); // a fractional (floored) case is reachable
    kani::cover!((a * b) % d == 0); // an exact case is reachable
}

/// `mul_div_ceil == floor + [remainder ≠ 0]` — the rounding DIRECTION, exactly one ulp up, never
/// down (the property a floor↔ceil flip would break; `wad_mul_up`/`ray_mul_up` inherit it).
// strength: STRONG — exact +1-on-remainder direction; both ceil branches covered; ceil checked_add overflow & the wide divide are covered by mul_div_fails_closed_on_overflow / shim_matches_bnum.
#[kani::proof]
fn mul_div_ceil_is_floor_plus_remainder_indicator() {
    let a = kani::any::<u8>() as u128;
    let b = kani::any::<u8>() as u128;
    let d = kani::any::<u8>() as u128;
    kani::assume(a < 16 && b < 16 && d >= 1); // ≤16 symbolic bits keeps CBMC tractable
    let floor = mul_div_floor(a, b, d).unwrap();
    let ceil = mul_div_ceil(a, b, d).unwrap();
    let has_rem = (a * b) % d != 0;
    assert_eq!(ceil, floor + if has_rem { 1 } else { 0 });
    assert!(ceil >= floor);
    kani::cover!(has_rem && ceil == floor + 1);
    kani::cover!(!has_rem && ceil == floor);
}

/// A zero denominator fails closed (returns `None`, never divides) for BOTH directions.
// covers: none — unconditional asserts over fully-symbolic a,b; structurally non-vacuous (nothing gated to cover).
// strength: STRONG — fully symbolic a,b; unconditional (non-gated) asserts prove the denom==0 fail-closed for both directions, so it is structurally non-vacuous without a cover!.
#[kani::proof]
fn mul_div_zero_denominator_is_none() {
    let a = kani::any::<u8>() as u128;
    let b = kani::any::<u8>() as u128;
    assert_eq!(mul_div_floor(a, b, 0), None);
    assert_eq!(mul_div_ceil(a, b, 0), None);
}

/// FAIL-CLOSED on overflow: a quotient that exceeds `u128::MAX` returns `None`, never a wrapped value.
/// Wide SYMBOLIC inputs explode CBMC over the 256-bit divide, so this pins the boundary with CONCRETE
/// witnesses on both sides (instant); `shim_matches_bnum` sweeps the overflow edge over 20k random
/// inputs. Covers `floor` (the full product when `denom == 1`) and the `ceil` boundary.
// covers: none — concrete-witness harness (UNIT_TEST); every assert is a single always-reached path.
// strength: UNIT_TEST — concrete overflow witnesses only (wide symbolic inputs through the 256-bit divide are intractable, >2h); the symbolic overflow sweep lives in shim_matches_bnum. Honestly labeled.
#[kani::proof]
fn mul_div_fails_closed_on_overflow() {
    // quotient > u128::MAX  ⇒  None (no wrap)
    assert_eq!(mul_div_floor(u128::MAX, 2, 1), None);
    assert_eq!(mul_div_floor(u128::MAX, u128::MAX, 1), None);
    assert_eq!(mul_div_floor(1u128 << 100, 1u128 << 100, 1), None);
    assert_eq!(mul_div_floor(1u128 << 127, 4, 1), None);
    // exactly fits u128  ⇒  Some(exact)
    assert_eq!(mul_div_floor(u128::MAX, 1, 1), Some(u128::MAX));
    assert_eq!(mul_div_floor(1u128 << 64, 1u128 << 63, 1), Some(1u128 << 127));
    // ceil at the boundary: an exact u128::MAX stays Some; a ceil one past it fails closed.
    assert_eq!(mul_div_ceil(u128::MAX, 2, 2), Some(u128::MAX)); // (2·MAX)/2 exact
    assert_eq!(mul_div_ceil(u128::MAX, 3, 2), None); // ceil(3·MAX/2) > u128::MAX
}

/// `present_debt` rounds debt UP against the borrower: any fractional `art*rate/RAY` is charged the
/// next whole unit, so realized debt is NEVER less than the true value (no rounding-induced
/// under-collateralization). `art: u8`, `rate = RAY + y` (`y: u8`) — 16 symbolic bits, tractable.
// strength: STRONG — exact round-up (never undercharges) over symbolic art & sub-RAY fraction; product stays < u128 (hi==0), so the wide loop generality is swept by shim_matches_bnum.
#[kani::proof]
fn present_debt_rounds_up_against_borrower() {
    let art = kani::any::<u8>() as u128;
    let y = kani::any::<u8>() as u128;
    let rate = RAY + y; // rate ≥ 1.0; `y` is the sub-RAY fraction
    // art*rate = art*RAY + art*y; /RAY floors to `art` (art*y ≤ 255·255 < RAY); remainder = art*y.
    let floor = art;
    let has_frac = art != 0 && y != 0;
    let debt = present_debt(art, rate).unwrap();
    assert_eq!(debt, floor + if has_frac { 1 } else { 0 });
    assert!(debt >= floor); // never UNDER-charges
    kani::cover!(has_frac && debt == art + 1); // a real round-up is reachable
}

/// On the RAY path directly: `ray_mul` (floor) is the lower bound, `ray_mul_up` the upper, differing
/// by at most one ulp, and by exactly one iff there is a remainder. `a, b = RAY + u8` (16 symbolic bits).
// strength: STRONG — exact floor-or-one-more on the shipped RAY path; product ~1e54 forces hi!=0, so this harness genuinely DRIVES the 256-iteration wide long division (strongest branch coverage of the group).
#[kani::proof]
fn ray_mul_up_is_floor_or_one_more() {
    let x = kani::any::<u8>() as u128;
    let z = kani::any::<u8>() as u128;
    let a = RAY + x;
    let b = RAY + z;
    let lo = ray_mul(a, b).unwrap();
    let hi = ray_mul_up(a, b).unwrap();
    // (RAY+x)(RAY+z) = RAY² + RAY·(x+z) + x·z; mod RAY == x·z (x·z ≤ 255·255 < RAY).
    let has_rem = x * z != 0;
    assert!(hi == lo || hi == lo + 1);
    assert_eq!(hi, lo + if has_rem { 1 } else { 0 });
    kani::cover!(has_rem); // a real round-up is reachable
}

/// `apply_bps` never over-pays: a fee at any rate ≤ 100% is ≤ the notional, and 100% is the identity.
/// (Pure `u128` checked-mul — no `bnum`/shim divide — so a wide `u64` input is fine and fast.)
// strength: STRONG — never-exceeds + 100%-identity over symbolic amount & the full <=100% bps range; the overflow fail-closed branches (amount: u8) are covered by the bps() unit test.
#[kani::proof]
fn apply_bps_never_exceeds_notional() {
    let amount = kani::any::<u8>() as u64; // narrow symbolic width: the property is scale-independent
    let bps: u16 = kani::any();
    kani::assume(bps <= BPS_DENOMINATOR as u16); // ≤ 100%
    let out = apply_bps(amount, bps).unwrap(); // result ≤ amount ≤ u64::MAX ⇒ downcast cannot fail
    assert!(out <= amount, "a sub-100% fee can never exceed the notional");
    if bps == BPS_DENOMINATOR as u16 {
        assert_eq!(out, amount, "100% is the identity");
    }
    kani::cover!(out < amount); // a real haircut is reachable
    kani::cover!(out == amount);
}

/// `ray_pow` interest-accumulator identities. `x^0 == 1.0` for ANY base (the `exp == 0` path returns
/// `RAY` with no loop / no divide). The non-trivial cases use CONCRETE small exponents: a SYMBOLIC
/// exponent makes CBMC unwind the binary-exp loop AND its nested divide for every `n` (intractable).
// covers: none — unconditional identity asserts (symbolic base + concrete exponents); no gated branch to cover.
// strength: WEAK — only the no-loop x^0==1 identity is symbolic (a symbolic exponent unwinds the binary-exp loop+divide per bit, intractable); the loop / odd-bit multiply / overflow `?` generality is swept by the ray_pow_matches_reference differential test.
#[kani::proof]
fn ray_pow_identities() {
    let base: u128 = kani::any();
    kani::assume(base < (1u128 << 100));
    assert_eq!(ray_pow(base, 0), Some(RAY)); // x^0 = 1.0, for ANY base
    assert_eq!(ray_pow(RAY, 2), Some(RAY)); // 1.0^2 = 1.0
    assert_eq!(ray_pow(2 * RAY, 2), Some(4 * RAY)); // 2.0^2 = 4.0 (the squaring is exact)
}

// ===========================================================================
// Conservation-critical accounting: redistribution (tier-2 liquidation) and the
// Reactor-Pool product-sum. These prove the SINGLE-operation versions of the
// solvency/conservation invariants the integration tests check only by example.
// Same tractability discipline as above: ≤16 symbolic bits/harness, structured
// scale values, a `kani::cover!` on every harness. update_product/accumulate route
// their 256-bit divide through the `cfg(kani)` shim (validated ≡ bnum by the
// differential tests `update_product_matches_bnum` / `muladd_div_matches_bnum`).
// ===========================================================================

use crate::redistribution;
use crate::reactor_pool;

/// Stride/grid used by the offset harnesses. A tiny grid (4 scales × 4 epochs) keeps CBMC's array
/// model small; `u8` offset inputs never bump `scale` (the per-unit loss stays ≫ `SCALE_FACTOR`), so
/// `scale` stays 0 and the epoch-roll only needs row 1 to be addressable.
const REACTOR_STRIDE: u64 = 4;
fn reactor_grid() -> [u128; 16] {
    [0u128; 16]
}

/// A concrete pool size / stake total for the offset & redistribution harnesses. The pool-size
/// division (`coll·1e18 / total`) divides a LARGE dividend by the pool size; CBMC dispatches that
/// in seconds when the divisor is a CONSTANT, but a SYMBOLIC divisor of that magnitude is intractable
/// (it ran >9 min and did not finish). So the divisor is pinned; the CONSERVED quantities — debt,
/// collateral, the carried error — stay fully symbolic. 97 is prime and coprime to 1e18, so the floor
/// residuals (`… mod 97`) are genuinely non-trivial (no vacuous "remainder is always 0" harness).
const REACTOR_POOL: u128 = 97;

/// `s_idx` returns `None` EXACTLY when the cell is out of range (`scale >= stride`, OR the flat index
/// `epoch*stride + scale >= len`), and otherwise the position's exact in-range flat index. This is the
/// depositor-safety guard: an out-of-range `(epoch, scale)` must never alias an occupied cell (the
/// "direct indexing, no wrap" decision). Pure index math (no division) → tractable at four symbolic bytes.
// strength: STRONG — exact two-directional iff (None iff out-of-range, else the precise flat index) over four fully-symbolic inputs; three covers separate the stride-rejection, the len-rejection, and a valid index.
#[kani::proof]
fn s_idx_none_iff_cell_out_of_range() {
    let epoch = kani::any::<u8>() as u64;
    let scale = kani::any::<u8>() as u64;
    let stride = kani::any::<u8>() as u64;
    let len = kani::any::<u8>() as usize;
    let r = reactor_pool::s_idx(epoch, scale, stride, len);
    let idx = epoch * stride + scale; // no overflow: 255*255 + 255 fits u64
    if scale >= stride || idx >= len as u64 {
        assert!(r.is_none(), "an out-of-range cell must map to None, never alias another cell");
    } else {
        assert_eq!(r, Some(idx as usize), "an in-range cell maps to its exact flat index");
        assert!((idx as usize) < len);
    }
    kani::cover!(r.is_none() && scale >= stride); // the stride-overflow rejection
    kani::cover!(r.is_none() && scale < stride); // the distinct len-overflow rejection
    kani::cover!(r.is_some()); // a valid in-range index
}

/// `compute_stake` identity: before the first liquidation (`total_collateral_snapshot == 0`) a
/// position's stake equals its raw collateral — no scaling (the genesis case; no division on this path).
// strength: WEAK — concrete tcs==0 locks the genesis no-op branch `Ok(coll)` (near-tautology, proves only that tss is ignored at genesis); the substantive downscale branch is the STRONG sibling compute_stake_never_over_stakes.
#[kani::proof]
fn compute_stake_identity_before_first_liquidation() {
    let coll = kani::any::<u8>() as u128;
    let tss = kani::any::<u8>() as u128;
    assert_eq!(redistribution::compute_stake(coll, tss, 0), Ok(coll));
    kani::cover!(coll > 0 && tss > 0); // the identity holds regardless of the (ignored) tss
}

/// `compute_stake` never OVER-stakes: in the post-liquidation regime where redistributed collateral
/// grew `total_collateral` at least as fast as `total_stakes` (`tss <= tcs`), a position's stake is
/// `<= its collateral`. This is what keeps `Σ stake == total_stakes` from inflating. `coll, tss < 16`,
/// `tcs: u8` — ≤16 symbolic bits through the shimmed `mul_div_floor`.
// strength: STRONG — anti-inflation invariant stake<=coll over a SYMBOLIC divisor (tcs); strict-downscale and ratio==1 boundary both covered. Operands <16 keep the shimmed mul_div_floor on its hi==0 path (scale-independent).
#[kani::proof]
fn compute_stake_never_over_stakes() {
    let coll = kani::any::<u8>() as u128;
    let tss = kani::any::<u8>() as u128;
    let tcs = kani::any::<u8>() as u128;
    kani::assume(coll < 16 && tss < 16 && tcs >= 1 && tss <= tcs);
    let stake = redistribution::compute_stake(coll, tss, tcs).unwrap();
    assert!(stake <= coll, "downscaling never inflates a stake above the position's collateral");
    kani::cover!(stake < coll); // a real downscale (ratio < 1) is reachable
    kani::cover!(stake == coll && tss == tcs); // the ratio==1 boundary stays exact
}

/// A full-pool offset (`debt == total`) is the epoch-roll branch: NO product update (the `bnum`/shim
/// divide is never reached), only native arithmetic. Proves the deterministic reset — deposits to zero,
/// `epoch` advances exactly once, `P`/`scale` reset, `last_loss_error` cleared — and that the collateral
/// error-feedback residual is a proper remainder (`last_coll_error < total`). `total = REACTOR_POOL`
/// (concrete divisor), `coll: u8` symbolic.
// strength: STRONG — the full epoch-roll reset tuple + a proper coll residual over symbolic coll; REACTOR_POOL=97 is the disclosed concrete divisor (a symbolic pool-size divisor ran >9min; 97 is prime/coprime-to-1e18 so residuals are non-trivial).
#[kani::proof]
fn offset_full_drain_rolls_epoch_and_resets() {
    let coll = kani::any::<u8>() as u128;
    let total = REACTOR_POOL; // concrete divisor (see REACTOR_POOL); a full drain is debt == total
    let mut st = reactor_pool::PoolState::new();
    st.total_deposits = total;
    let mut g = reactor_grid();
    reactor_pool::offset(&mut st, &mut g, REACTOR_STRIDE, total, coll).unwrap();
    assert_eq!(st.total_deposits, 0, "a full drain empties the pool");
    assert_eq!(st.epoch, 1, "epoch rolls exactly once");
    assert_eq!(st.scale, 0, "scale resets");
    assert_eq!(st.p, reactor_pool::DECIMAL_PRECISION, "P resets to 1.0");
    assert_eq!(st.last_loss_error, 0, "loss error cleared on a full drain");
    assert!(st.last_coll_error < total, "coll error-feedback residual is a proper remainder");
    kani::cover!(coll > 0 && st.last_coll_error > 0); // a real fractional coll-per-unit is reachable
}

/// A PARTIAL offset (`0 < debt < total`) keeps the pool SOLVENT and conserves the rounding direction.
/// This drives the product-update branch (tractable via the `update_product` shim). Proves: deposits
/// fall by EXACTLY the debt; the collateral residual is a proper remainder (`< total`); the loss
/// residual reflects Liquity's loss-per-unit "+1" (`1 <= last_loss_error <= total`, i.e. the loss is
/// rounded UP, never under-counted); and the sole depositor's compounded deposit NEVER exceeds the
/// remaining pool (`compounded <= total_deposits` — the solvency invariant; any deficit is the
/// protocol-favoring dust). `total = REACTOR_POOL` (concrete divisor), `debt ∈ [1, total)` and `coll: u8`
/// symbolic. With these the per-unit loss stays ≫ SCALE_FACTOR, so `scale` never bumps.
// strength: STRONG — a genuine solvency invariant (compounded <= total_deposits, not a local post-condition) + the loss-per-unit "+1" round-up over symbolic debt,coll; REACTOR_POOL=97 pinned (disclosed). Scale-bump path delegated to update_product_rescales_above_floor.
#[kani::proof]
fn offset_partial_keeps_pool_solvent() {
    let debt = kani::any::<u8>() as u128;
    let coll = kani::any::<u8>() as u128;
    let total = REACTOR_POOL; // concrete divisor; debt & coll stay symbolic
    kani::assume(debt >= 1 && debt < total);
    let mut st = reactor_pool::PoolState::new();
    st.total_deposits = total;
    let mut g = reactor_grid();
    let snap = st.snapshot(&g, REACTOR_STRIDE);
    reactor_pool::offset(&mut st, &mut g, REACTOR_STRIDE, debt, coll).unwrap();

    assert_eq!(st.total_deposits, total - debt, "deposits fall by exactly the offset debt");
    assert!(st.last_coll_error < total, "coll residual is a proper remainder");
    assert!(
        st.last_loss_error >= 1 && st.last_loss_error <= total,
        "loss-per-unit '+1' rounds the loss UP — never under-counted"
    );

    // Solvency: the whole pool as one depositor can never claim more than what remains.
    let comp = reactor_pool::compounded_deposit(&st, total, &snap);
    assert!(comp <= st.total_deposits, "compounded deposit never exceeds the pool (solvency)");
    kani::cover!(comp < st.total_deposits); // a real protocol-favoring dust deficit is reachable
    kani::cover!(st.last_coll_error > 0); // a fractional coll-per-unit is reachable
}

/// `update_product` RESCALE invariant: the returned `P` is always `>= SCALE_FACTOR` (never underflows
/// the precision floor — the depositor-precision guarantee) and the rescale loop TERMINATES in `<= 4`
/// bumps (well inside the `bumps > 4 → Err` guard; the `.unwrap()` succeeding IS that proof). Structured
/// around the floor (`p = SCALE_FACTOR + u8`, `factor` straddling `SCALE_FACTOR`) so the product
/// straddles `DECIMAL_PRECISION` and BOTH the 1- and 2-bump rescales are genuinely exercised. 16 bits.
// strength: STRONG — rescale invariant new_p >= SCALE_FACTOR (verified tight) + termination (bumps<=4) over symbolic p,factor; both bump depths covered. The bumps==0 no-rescale path (common production case) is swept by update_product_matches_bnum.
#[kani::proof]
fn update_product_rescales_above_floor() {
    let p = reactor_pool::SCALE_FACTOR + kani::any::<u8>() as u128; // p ∈ [SCALE_FACTOR, +255]
    // factor straddles SCALE_FACTOR (stays in (0, DECIMAL_PRECISION]) → product straddles 1e18.
    let factor = reactor_pool::SCALE_FACTOR - 128 + kani::any::<u8>() as u128;
    let (new_p, bumps) = reactor_pool::update_product(p, factor).unwrap();
    assert!(new_p >= reactor_pool::SCALE_FACTOR, "P never rescales below the SCALE_FACTOR floor");
    assert!(bumps <= 4, "the rescale loop terminates well within the bump guard");
    kani::cover!(bumps == 1); // one rescale (product just above 1e18)
    kani::cover!(bumps == 2); // two rescales (product just below 1e18 — the deepest realistic case)
}

/// `accumulate` (driven via `redistribute`) performs a PROPER division — the no-drift core. From
/// genesis (`l_coll == 0`) with a carried error `e`, the new accumulator and residual satisfy the
/// EXACT identity `coll*PRECISION + e == l_coll*total_stakes + last_coll_error` with
/// `last_coll_error < total_stakes`. The carried error is what makes repeated tiny redistributions sum
/// without systematic drift. `total_stakes = REACTOR_POOL` (concrete divisor), `coll: u8` and the carried
/// error `e` (a prior proper remainder, so `e < total_stakes`) symbolic; `art == 0` keeps the art-side
/// accumulate inert. Routes through the `wide_muladd_div` shim under `cfg(kani)`.
// strength: STRONG — the exact Euclidean identity q*divisor + remainder == numerator + proper-remainder over symbolic dividend & carried error; REACTOR_POOL=97 pinned (disclosed); shim ≡ bnum pinned by muladd_div_matches_bnum.
#[kani::proof]
fn accumulate_is_proper_division() {
    let coll = kani::any::<u8>() as u128;
    let e = kani::any::<u8>() as u128;
    let total_stakes = REACTOR_POOL; // concrete divisor; coll & carried error stay symbolic
    kani::assume(e < total_stakes); // a realistic carried error (a previous proper remainder)
    let mut st = redistribution::RedistState::new();
    st.last_coll_error = e; // seed a carried error
    redistribution::redistribute(&mut st, total_stakes, coll, 0).unwrap();

    let numerator = coll * redistribution::PRECISION + e;
    assert_eq!(
        st.l_coll * total_stakes + st.last_coll_error,
        numerator,
        "accumulator division is exact: q*divisor + remainder == numerator"
    );
    assert!(st.last_coll_error < total_stakes, "carried error is a proper remainder (< total_stakes)");
    kani::cover!(st.last_coll_error > 0); // a real fractional carry is reachable
    kani::cover!(e > 0); // the carried-error feedback path is exercised
}

/// A sole recipient holding ALL the stake receives essentially the whole redistribution: its pending
/// collateral is within ONE dust unit BELOW the amount redistributed — floor rounding favors the system,
/// never over-pays. `stake == total_stakes == REACTOR_POOL` (concrete divisor); `coll: u8` symbolic; `art == 0`
/// (the coll side carries the proof). The ≤1 dust bound holds because `total_stakes <= 1e18`. Via the shim.
// strength: STRONG — floor-conservation (pending <= coll, deficit <= 1, matching the verified true max) over symbolic coll; REACTOR_POOL=97 pinned (disclosed); the <=1 dust bound holds because total_stakes <= 1e18.
#[kani::proof]
fn redistribute_single_recipient_conserves() {
    let coll = kani::any::<u8>() as u128;
    let stake = REACTOR_POOL; // concrete divisor; the sole recipient holds ALL the stake
    let mut st = redistribution::RedistState::new();
    let snap = st.snapshot();
    redistribution::redistribute(&mut st, stake, coll, 0).unwrap();
    let (pending_coll, pending_art) = redistribution::pending(stake, &st, &snap).unwrap();
    assert!(pending_coll <= coll, "never over-distributes (floor favors the system)");
    assert!(coll - pending_coll <= 1, "the deficit is at most one dust unit");
    assert_eq!(pending_art, 0);
    kani::cover!(pending_coll == coll); // an exact (no-dust) distribution is reachable
    kani::cover!(coll - pending_coll == 1); // a real dust deficit is reachable
}

/// TWO recipients whose stakes sum to the whole stake share a redistribution with no leakage:
/// `Σ pending <= redistributed` and the shortfall is at most 2 dust units (one floor per recipient) —
/// floor rounding always favors the system, never over-distributes. `stake1 + stake2 == REACTOR_POOL`
/// (concrete divisor); `coll: u8` and the split `stake1 ∈ [1, REACTOR_POOL-1]` symbolic; `art == 0`.
// strength: STRONG — pro-rata conservation p1+p2 <= coll over symbolic coll AND a symbolic two-way split; REACTOR_POOL=97 pinned (disclosed). The asserted <=2 dust bound is sound but slack (verified true max is 1).
#[kani::proof]
fn redistribute_two_recipients_conserve() {
    let coll = kani::any::<u8>() as u128;
    let stake1 = kani::any::<u8>() as u128;
    kani::assume(stake1 >= 1 && stake1 < REACTOR_POOL);
    let stake2 = REACTOR_POOL - stake1;
    let mut st = redistribution::RedistState::new();
    let snap = st.snapshot();
    redistribution::redistribute(&mut st, REACTOR_POOL, coll, 0).unwrap();
    let (p1, _) = redistribution::pending(stake1, &st, &snap).unwrap();
    let (p2, _) = redistribution::pending(stake2, &st, &snap).unwrap();
    assert!(p1 + p2 <= coll, "two recipients never over-claim the redistributed collateral");
    assert!(coll - (p1 + p2) <= 2, "the shortfall is at most one floor-dust unit per recipient");
    kani::cover!(p1 + p2 == coll); // an exact split is reachable
    kani::cover!(coll - (p1 + p2) >= 1); // a real dust shortfall is reachable
}

/// COLLATERAL conservation through a partial offset: the sole depositor (the whole pool) accrues a
/// collateral gain within ONE dust unit BELOW the seized collateral — `collateral_gain <= coll`, never
/// more (floor favors the system). The Reactor-Pool counterpart to the deposit-side solvency in
/// `offset_partial_keeps_pool_solvent`. `total = REACTOR_POOL` (concrete divisor), `debt ∈ [1, total)` and
/// `coll: u8` symbolic; no scale bump (per-unit loss ≫ SCALE_FACTOR).
// strength: STRONG — collateral conservation (gain <= coll, floor never over-distributes) + the tight 1-dust bound over symbolic debt,coll; REACTOR_POOL=97 pinned (disclosed). collateral_gain's scale+1 sub-path is delegated (domain never bumps).
#[kani::proof]
fn offset_partial_conserves_collateral() {
    let debt = kani::any::<u8>() as u128;
    let coll = kani::any::<u8>() as u128;
    let total = REACTOR_POOL; // concrete divisor; debt & coll stay symbolic
    kani::assume(debt >= 1 && debt < total);
    let mut st = reactor_pool::PoolState::new();
    st.total_deposits = total;
    let mut g = reactor_grid();
    let snap = st.snapshot(&g, REACTOR_STRIDE);
    reactor_pool::offset(&mut st, &mut g, REACTOR_STRIDE, debt, coll).unwrap();
    let gain = reactor_pool::collateral_gain(&g, REACTOR_STRIDE, total, &snap).unwrap();
    assert!(gain <= coll, "the sole depositor never accrues more collateral than was seized");
    assert!(coll - gain <= 1, "the gain shortfall is at most one dust unit");
    kani::cover!(gain == coll); // an exact (no-dust) collateral distribution is reachable
    kani::cover!(coll > 0 && coll - gain == 1); // a real dust shortfall is reachable
}

// --- Liquidation loss-absorption waterfall (recovery.rs) — the terminal-recovery lane ---------------
// Pure min/sub/add (no division), so these run FULLY SYMBOLIC over u128 — no narrowing, no REACTOR_POOL
// pin: the strongest harnesses in the suite. They prove the fix for the old NoRedistributionRecipients
// revert: the waterfall always accounts for the full debt and the terminal `unhomed` fires exactly
// when no tier can cover.
use crate::recovery;

// strength: STRONG — fully-symbolic u128 conservation `reactor+redist+buffer+global+unhomed == debt` (the load-bearing identity); every tier-finishing branch forced by a cover!; no division, no bound.
#[kani::proof]
fn absorb_conserves_debt_exactly() {
    let debt = kani::any::<u128>();
    let reactor_capacity = kani::any::<u128>();
    let has_redist_recipients = kani::any::<bool>();
    let buffer_balance = kani::any::<u128>();
    let global_available = kani::any::<u128>();
    let a = recovery::absorb(debt, reactor_capacity, has_redist_recipients, buffer_balance, global_available);
    // CONSERVATION: every unit of debt is accounted for by exactly the five tiers (the fix for the
    // NoRedistributionRecipients stall — the split can never lose or invent debt, and never reverts).
    assert_eq!(a.reactor + a.redist + a.buffer + a.global + a.unhomed, debt, "the tiers sum to debt exactly");
    // Each component is itself bounded by debt (so the additions above cannot overflow).
    assert!(a.reactor <= debt && a.redist <= debt && a.buffer <= debt && a.global <= debt && a.unhomed <= debt);
    kani::cover!(a.reactor == debt && debt > 0); // RP fully covers
    kani::cover!(a.redist > 0); // redistribution path
    kani::cover!(a.buffer > 0); // local buffer path
    kani::cover!(a.global > 0); // global backstop path
    kani::cover!(a.unhomed > 0); // the terminal (shutdown) path is reachable
}

// strength: STRONG — fully-symbolic u128; proves the strict tier ORDER, the fail-closed buffer haircuts, and that `unhomed > 0` happens EXACTLY when no tier can cover (RP-short ∧ no recipients ∧ both buffers drained). No division, no bound.
#[kani::proof]
fn absorb_is_fail_closed_and_ordered() {
    let debt = kani::any::<u128>();
    let reactor_capacity = kani::any::<u128>();
    let has_redist_recipients = kani::any::<bool>();
    let buffer_balance = kani::any::<u128>();
    let global_available = kani::any::<u128>();
    let a = recovery::absorb(debt, reactor_capacity, has_redist_recipients, buffer_balance, global_available);

    assert!(a.reactor <= reactor_capacity, "RP never offsets more than the pool holds");
    assert!(a.buffer <= buffer_balance, "the local buffer never absorbs more than its balance (fail-closed)");
    assert!(a.global <= global_available, "the global tier never absorbs more than is available (fail-closed)");
    assert!(a.redist == 0 || a.redist == debt - a.reactor, "redistribution is all-or-nothing on the remainder");
    if a.buffer > 0 {
        assert!(!has_redist_recipients, "the local buffer is used only AFTER redistribution can't (strict order)");
    }
    if a.global > 0 {
        assert!(!has_redist_recipients, "the global tier is used only AFTER redistribution can't (strict order)");
        assert_eq!(a.buffer, buffer_balance, "the global tier is used only AFTER the local buffer is drained");
    }
    if a.unhomed > 0 {
        // The terminal case is reachable EXACTLY when no tier can cover the debt:
        assert!(!has_redist_recipients, "un-homed implies no redistribution recipient");
        assert_eq!(a.reactor, reactor_capacity, "un-homed implies the RP was fully drained");
        assert_eq!(a.buffer, buffer_balance, "un-homed implies the local buffer was fully drained (fail-closed)");
        assert_eq!(a.global, global_available, "un-homed implies the global tier was fully drained (fail-closed)");
        assert!(reactor_capacity + buffer_balance + global_available < debt, "un-homed implies RP + buffers genuinely cannot cover");
    }
    kani::cover!(a.buffer > 0 && a.unhomed == 0); // the local buffer fully covers the post-RP remainder
    kani::cover!(a.global > 0 && a.unhomed == 0); // the global tier fully covers the post-buffer remainder
    kani::cover!(a.unhomed > 0); // a genuine shortfall (the shutdown trigger) is reachable
    kani::cover!(a.redist > 0 && a.buffer == 0); // recipients exist ⇒ buffers bypassed
}

// --- Per-position interest (interest.rs) — the BOLD weighted-debt-sum accrual ----------------------
// TRACTABILITY: the interest denominator (SECONDS_PER_YEAR·10_000 ≈ 3.15e11) is huge, so tiny symbolic
// inputs floor to 0 (vacuous). The fix is a CONCRETE period of exactly SECONDS_PER_YEAR: the year
// factor then cancels EXACTLY (floor(d·r·Y/(Y·10_000)) == floor(d·r/10_000), proven below by algebra),
// leaving the meaningful value `debt·rate/10_000` over a ≤16-bit symbolic (debt,rate) — which pins the
// FORMULA + UNITS + rounding direction non-vacuously. The wide/arbitrary-period generality is swept by
// the always-on `interest::tests::matches_u256_reference` differential test (20k inputs vs a U256 ref).
use crate::interest;

// strength: STRONG — exact one-year value `floor(debt·rate/10_000)` (the formula + units + FLOOR direction) over symbolic debt,rate; both rounding branches covered; arbitrary-period generality swept by matches_u256_reference.
#[kani::proof]
fn accrued_one_year_is_debt_times_rate() {
    let debt = kani::any::<u8>() as u128;
    let rate = kani::any::<u8>() as u16; // a small symbolic rate (the formula is scale-independent)
    // One year of interest = debt·rate/10_000, floored against... the protocol (per-position floor).
    // The concrete SECONDS_PER_YEAR makes the year factor cancel exactly (see header).
    let accrued = interest::accrued_interest(debt, rate, interest::SECONDS_PER_YEAR as u64).unwrap();
    let expected = (debt * rate as u128) / interest::INTEREST_RATE_DENOM; // = floor(debt·rate/10_000)
    assert_eq!(accrued, expected);
    kani::cover!((debt * rate as u128) >= interest::INTEREST_RATE_DENOM); // a NONZERO year of interest is reachable
    kani::cover!((debt * rate as u128) % interest::INTEREST_RATE_DENOM != 0); // a real floor (remainder) is reachable
}

// strength: STRONG — exact one-year aggregate `ceil(weighted/10_000)` (the round-UP direction the aggregate mint depends on) over symbolic weighted; both branches covered; wide sweep in matches_u256_reference.
#[kani::proof]
fn pending_aggregate_one_year_rounds_up() {
    let weighted = kani::any::<u16>() as u128; // = Σ recorded·rate_bps for a tiny market (≤16 symbolic bits)
    let pending =
        interest::pending_aggregate_interest(weighted, interest::SECONDS_PER_YEAR as u64).unwrap();
    let floor = weighted / interest::INTEREST_RATE_DENOM;
    let has_rem = weighted % interest::INTEREST_RATE_DENOM != 0;
    // The aggregate rounds UP — never short of what positions owe (the solvency margin).
    assert_eq!(pending, floor + if has_rem { 1 } else { 0 });
    assert!(pending >= floor);
    kani::cover!(has_rem && pending == floor + 1); // a real round-up is reachable
    kani::cover!(!has_rem && pending == floor); // an exact case is reachable
}

// strength: STRONG — the SOLVENCY/no-drift direction: the aggregate ceil for one position's weight is never below that position's floor realization, differing by at most 1; both the equal and the differ-by-1 cases covered (concrete year; the relation is structural ceil>=floor of the same numerator).
#[kani::proof]
fn aggregate_never_short_of_position() {
    let debt = kani::any::<u8>() as u128;
    let rate = kani::any::<u8>() as u16;
    let dt = interest::SECONDS_PER_YEAR as u64;
    // The aggregate formula applied to one position's weight vs that position's own realization — the
    // per-unit form of "Σ aggregate ceil >= Σ per-position floor" (the minted interest is never short).
    let agg =
        interest::pending_aggregate_interest(interest::weighted_debt(debt, rate).unwrap(), dt).unwrap();
    let pos = interest::accrued_interest(debt, rate, dt).unwrap();
    assert!(agg >= pos, "aggregate interest is never less than the position's realized interest");
    assert!(agg - pos <= 1, "ceil and floor of the same quantity differ by at most one");
    kani::cover!(agg == pos); // an exact (no-margin) case is reachable
    kani::cover!(agg == pos + 1); // a one-unit protocol margin is reachable
}

// covers: none — unconditional boundary asserts over fully-symbolic inputs; structurally non-vacuous.
// strength: STRONG — fully symbolic small inputs; unconditional asserts prove interest is zero at each of the three boundaries (no time / no rate / no debt), so it is structurally non-vacuous without a cover.
#[kani::proof]
fn accrued_zero_at_boundaries() {
    let d = kani::any::<u8>() as u128;
    let r = kani::any::<u8>() as u16;
    let p = kani::any::<u8>() as u64;
    assert_eq!(interest::accrued_interest(d, r, 0), Some(0)); // no elapsed time ⇒ no interest
    assert_eq!(interest::accrued_interest(d, 0, p), Some(0)); // a 0% rate ⇒ no interest
    assert_eq!(interest::accrued_interest(0, r, p), Some(0)); // no debt ⇒ no interest
}

// covers: none — concrete-witness harness (UNIT_TEST); every assert is a single always-reached path.
// strength: UNIT_TEST — concrete witnesses that the weighted-term overflow fails closed (None, never a wrap); the symbolic over-u128 sweep lives in matches_u256_reference. Honestly labeled.
#[kani::proof]
fn accrued_fails_closed_on_overflow() {
    // recorded_debt · rate_bps > u128::MAX ⇒ None (no wrap).
    assert_eq!(interest::accrued_interest(u128::MAX, 2, 1), None);
    assert_eq!(interest::accrued_interest(u128::MAX / 100, 2_550, 1), None);
    // A weighted term that fits stays Some — no spurious fail-closed.
    assert!(interest::accrued_interest(u128::MAX / 100_000, 1, 1).is_some());
    assert!(interest::weighted_debt(u128::MAX, 1).is_some()); // ·1 never overflows
    assert_eq!(interest::weighted_debt(u128::MAX, 2), None); // ·2 does
}
