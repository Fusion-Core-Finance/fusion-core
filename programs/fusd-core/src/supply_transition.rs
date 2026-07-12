//! Shared supply-identity transitions (audit M-01) — the SINGLE definition of every
//! `agg_recorded_debt` / `unminted_interest` / `bad_debt` accounting step, used by BOTH sides:
//!
//! * the handlers execute these functions monomorphized to `u128` (checked arithmetic, `None`
//!   mapped to `FusdError::MathOverflow` at the call site);
//! * the `certora.rs` supply rules execute the SAME bodies monomorphized to
//!   `cvlr::mathint::NativeInt` (unbounded math-int), so a mutation of the shared TRANSITION
//!   algebra flips the Certora proof AND the litesvm `assert_supply_invariant` oracle.
//!
//! Two residues escape the prover (both litesvm-covered; the ledger in certora/README.md and
//! certora/mutations.md classes them):
//! * a handler CALL-SITE mutation (dropping the call or the assignment of the returned post-state);
//! * a mutation inside `impl SupplyNum for u128` itself — the rules monomorphize the NativeInt
//!   impl, so the u128 trait methods are outside every rule's cone. Their guards are the unit
//!   tests below (each checked-op edge pinned) + the litesvm suites, NOT the prover.
//!
//! Constraints (prover frontiers — do not relax):
//! * Everything here returns `Option`, NEVER `Result`/`FusdError`, and contains no `?`/`unwrap`/
//!   `expect`: a `?`-to-`FusdError` Err arm lowers to an indirect `callx` that
//!   `-solanaSkipCallRegInst` stubs to an empty TAC block, havocing memory (certora/README.md
//!   §"Two prover frontiers"). Rules consume `None` via `let Some(d) = … else { return; }`.
//! * Rules must ONLY instantiate `T = NativeInt`. The `u128` monomorphizations also exist in the
//!   verification ELF (the whole crate compiles under the feature); a rule that calls a transition
//!   at `u128` hits the u128 checked-arith frontier and spuriously fails.
//! * The transition functions are `#[cfg_attr(feature = "certora", inline(always))]` (the
//!   `bucket.rs` precedent) so the prover sees their bodies; production codegen is unchanged.

// `let … else { return None }` is deliberate — `?` is banned in this module (see the module doc);
// clippy's rewrite suggestion would reintroduce it.
#![allow(clippy::question_mark)]

/// The minimal arithmetic surface the supply transitions need. `u128` (production) implements it
/// with checked ops; `NativeInt` (verification) implements it over the unbounded math-int domain.
pub trait SupplyNum: Copy + Sized + PartialOrd {
    /// Checked add.
    fn cadd(self, rhs: Self) -> Option<Self>;
    /// Checked (underflow-guarded) sub.
    fn csub(self, rhs: Self) -> Option<Self>;
    /// Two-value minimum.
    fn min2(self, rhs: Self) -> Self;
    /// `floor(self · bps / 10_000)` — the handlers' `checked_mul(bps)? / BPS_DENOMINATOR` idiom.
    fn bps_cut(self, bps: u16) -> Option<Self>;
    /// Lift a `u64` constant into the domain.
    fn from_u64(v: u64) -> Self;
}

impl SupplyNum for u128 {
    #[inline(always)]
    fn cadd(self, rhs: Self) -> Option<Self> {
        self.checked_add(rhs)
    }
    #[inline(always)]
    fn csub(self, rhs: Self) -> Option<Self> {
        self.checked_sub(rhs)
    }
    #[inline(always)]
    fn min2(self, rhs: Self) -> Self {
        self.min(rhs)
    }
    #[inline(always)]
    fn bps_cut(self, bps: u16) -> Option<Self> {
        let Some(scaled) = self.checked_mul(bps as u128) else { return None };
        Some(scaled / fusd_math::BPS_DENOMINATOR)
    }
    #[inline(always)]
    fn from_u64(v: u64) -> Self {
        v as u128
    }
}

// The math-int domain is unbounded: `cadd` is total (its `None` arm is statically dead after
// monomorphization, so no havoc-able branch reaches the prover) and `csub` guards underflow with a
// plain symbolic branch (a direct CVT_nativeint extern comparison — no `callx`). Do NOT use
// `NativeInt::checked_sub`: in cvlr-mathint 0.6.1 it is a PANICKING alias for `sub`.
#[cfg(feature = "certora")]
impl SupplyNum for cvlr::mathint::NativeInt {
    #[inline(always)]
    fn cadd(self, rhs: Self) -> Option<Self> {
        Some(self + rhs)
    }
    #[inline(always)]
    fn csub(self, rhs: Self) -> Option<Self> {
        if self >= rhs {
            Some(self - rhs)
        } else {
            None
        }
    }
    #[inline(always)]
    fn min2(self, rhs: Self) -> Self {
        Ord::min(self, rhs)
    }
    #[inline(always)]
    fn bps_cut(self, bps: u16) -> Option<Self> {
        Some(self * Self::from(bps as u64) / Self::from(10_000u64))
    }
    #[inline(always)]
    fn from_u64(v: u64) -> Self {
        Self::from(v)
    }
}

// ============================== per-instruction transitions ==============================

/// `borrow`'s supply delta: debt grows by `amount + fee`, only `amount` is minted to the borrower,
/// the fee is booked as unminted interest (C7).
pub struct BorrowDelta<T> {
    /// `amount + fee` — the position-level debt growth (the position add stays handler-side).
    pub debt_delta: T,
    pub new_agg: T,
    /// `unminted + fee` (== the pre-state when `fee == 0`).
    pub new_unminted: T,
    /// The fUSD minted to the borrower (== `amount`; the fee is NOT minted).
    pub minted: T,
}

#[cfg_attr(feature = "certora", inline(always))]
pub fn borrow<T: SupplyNum>(agg0: T, unminted0: T, amount: T, fee: T) -> Option<BorrowDelta<T>> {
    let Some(debt_delta) = amount.cadd(fee) else { return None };
    let Some(new_agg) = agg0.cadd(debt_delta) else { return None };
    let Some(new_unminted) = unminted0.cadd(fee) else { return None };
    Some(BorrowDelta { debt_delta, new_agg, new_unminted, minted: amount })
}

/// `repay`'s supply delta: burn `min(amount, position_debt)` and un-book the same from the
/// position and the aggregate.
pub struct RepayDelta<T> {
    /// The fUSD burned (== the debt un-booked; `<= amount`).
    pub burn: T,
    pub new_position_debt: T,
    pub new_agg: T,
}

#[cfg_attr(feature = "certora", inline(always))]
pub fn repay<T: SupplyNum>(agg0: T, position_debt: T, amount: T) -> Option<RepayDelta<T>> {
    let burn = amount.min2(position_debt);
    let Some(new_position_debt) = position_debt.csub(burn) else { return None };
    let Some(new_agg) = agg0.csub(burn) else { return None };
    Some(RepayDelta { burn, new_position_debt, new_agg })
}

/// `refresh_market`'s supply delta: consume `amount = min(pending, u64::MAX)` of unminted
/// interest, split it keeper → C16 bad-debt paydown → backstop → buffer (in that order; the
/// paydown slice is NOT minted — it retires `bad_debt` instead), leaving
/// `keeper_cut + paydown + backstop_cut + buffer_amount == amount` exactly.
///
/// The two optional-account gates collapse into the params: the caller passes `keeper_bps = 0`
/// when no cranker ATA is supplied, and `backstop_cut_bps = 0` / `backstop_headroom = 0` when the
/// backstop pair is absent — value-identical to the in-handler gating.
pub struct RefreshDelta<T> {
    /// The interest consumed this crank (`min(pending, u64::MAX)`).
    pub amount: T,
    pub keeper_cut: T,
    pub paydown: T,
    pub backstop_cut: T,
    pub buffer_amount: T,
    pub new_unminted: T,
    pub new_bad: T,
}

#[cfg_attr(feature = "certora", inline(always))]
pub fn refresh<T: SupplyNum>(
    pending: T,
    bad0: T,
    keeper_bps: u16,
    paydown_bps: u16,
    backstop_cut_bps: u16,
    backstop_headroom: T,
) -> Option<RefreshDelta<T>> {
    let zero = T::from_u64(0);
    let amount = pending.min2(T::from_u64(u64::MAX));
    let keeper_cut = if keeper_bps > 0 {
        let Some(cut) = amount.bps_cut(keeper_bps) else { return None };
        cut
    } else {
        zero
    };
    let Some(post_keeper) = amount.csub(keeper_cut) else { return None };
    let paydown = if paydown_bps > 0 && bad0 > zero {
        let Some(want) = post_keeper.bps_cut(paydown_bps) else { return None };
        want.min2(bad0)
    } else {
        zero
    };
    let Some(post_paydown) = post_keeper.csub(paydown) else { return None };
    let backstop_cut = if backstop_cut_bps > 0 {
        let Some(want) = post_paydown.bps_cut(backstop_cut_bps) else { return None };
        want.min2(backstop_headroom)
    } else {
        zero
    };
    let Some(buffer_amount) = post_paydown.csub(backstop_cut) else { return None };
    let Some(new_unminted) = pending.csub(amount) else { return None };
    let Some(new_bad) = bad0.csub(paydown) else { return None };
    Some(RefreshDelta {
        amount,
        keeper_cut,
        paydown,
        backstop_cut,
        buffer_amount,
        new_unminted,
        new_bad,
    })
}

/// `liquidate`'s supply delta over the waterfall split: the reactor / buffer / global tiers BURN
/// fUSD and extinguish the same debt from the aggregate; the un-homed remainder leaves the
/// aggregate and is booked to `bad_debt`. `_redist` is deliberately taken and NOT subtracted —
/// redistributed debt stays parked in `agg_recorded_debt` (reassigned to survivors,
/// supply-neutral), and the parameter pins that in the executed transition.
pub struct LiquidateDelta<T> {
    /// Total fUSD burned by the tiers (`reactor + buffer + global`).
    pub burned: T,
    pub new_agg: T,
    pub new_bad: T,
}

#[cfg_attr(feature = "certora", inline(always))]
pub fn liquidate<T: SupplyNum>(
    agg0: T,
    bad0: T,
    reactor: T,
    _redist: T,
    buffer: T,
    global: T,
    unhomed: T,
) -> Option<LiquidateDelta<T>> {
    let Some(a1) = agg0.csub(reactor) else { return None };
    let Some(a2) = a1.csub(buffer) else { return None };
    let Some(a3) = a2.csub(global) else { return None };
    let Some(new_agg) = a3.csub(unhomed) else { return None };
    let Some(b1) = reactor.cadd(buffer) else { return None };
    let Some(burned) = b1.cadd(global) else { return None };
    let Some(new_bad) = bad0.cadd(unhomed) else { return None };
    Some(LiquidateDelta { burned, new_agg, new_bad })
}

/// One redemption candidate's supply delta (shared by `redeem` AND `urgent_redeem` — their
/// transitions are identical): burn `redeem_amt` fUSD of face value and un-book the same debt.
pub struct RedeemStepDelta<T> {
    /// The fUSD face value this candidate contributes to the batch burn (== `redeem_amt`).
    pub burned: T,
    pub new_agg: T,
}

#[cfg_attr(feature = "certora", inline(always))]
pub fn redeem_step<T: SupplyNum>(agg0: T, redeem_amt: T) -> Option<RedeemStepDelta<T>> {
    let Some(new_agg) = agg0.csub(redeem_amt) else { return None };
    Some(RedeemStepDelta { burned: redeem_amt, new_agg })
}

/// `settle_bad_debt`'s supply delta: `burned` fUSD leaves circulation and retires the same
/// `bad_debt`. Returns the new `bad_debt`.
#[cfg_attr(feature = "certora", inline(always))]
pub fn settle_bad_debt<T: SupplyNum>(bad0: T, burned: T) -> Option<T> {
    bad0.csub(burned)
}

/// Interest booking (`accrual::accrue`'s pending fold AND `adjust_rate`'s premature-adjustment
/// fee — the C7 twin): `x` enters `agg_recorded_debt` and `unminted_interest` in lockstep, so the
/// supply identity is untouched until `refresh_market` mints it.
pub struct InterestDelta<T> {
    pub new_agg: T,
    pub new_unminted: T,
}

#[cfg_attr(feature = "certora", inline(always))]
pub fn book_interest<T: SupplyNum>(agg0: T, unminted0: T, x: T) -> Option<InterestDelta<T>> {
    let Some(new_agg) = agg0.cadd(x) else { return None };
    let Some(new_unminted) = unminted0.cadd(x) else { return None };
    Some(InterestDelta { new_agg, new_unminted })
}

// ============================== host unit tests (T = u128) ==============================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrow_with_fee() {
        let d = borrow(1_000u128, 40, 500, 5).unwrap();
        assert_eq!(d.debt_delta, 505);
        assert_eq!(d.new_agg, 1_505);
        assert_eq!(d.new_unminted, 45);
        assert_eq!(d.minted, 500);
    }

    #[test]
    fn borrow_zero_fee_leaves_unminted() {
        let d = borrow(1_000u128, 40, 500, 0).unwrap();
        assert_eq!(d.debt_delta, 500);
        assert_eq!(d.new_agg, 1_500);
        assert_eq!(d.new_unminted, 40);
    }

    #[test]
    fn borrow_overflow_is_none() {
        assert!(borrow(u128::MAX, 0, 1, 0).is_none()); // agg overflow
        assert!(borrow(0u128, 0, u128::MAX, 1).is_none()); // debt_delta overflow
        assert!(borrow(0u128, u128::MAX, 0, 1).is_none()); // unminted overflow
    }

    #[test]
    fn repay_partial_and_capped() {
        // Partial: burn == amount.
        let d = repay(1_000u128, 600, 200).unwrap();
        assert_eq!(d.burn, 200);
        assert_eq!(d.new_position_debt, 400);
        assert_eq!(d.new_agg, 800);
        // Over-repay: burn capped at the position debt (full repay zeroes it).
        let d = repay(1_000u128, 600, 900).unwrap();
        assert_eq!(d.burn, 600);
        assert_eq!(d.new_position_debt, 0);
        assert_eq!(d.new_agg, 400);
    }

    #[test]
    fn repay_agg_underflow_is_none() {
        assert!(repay(100u128, 600, 600).is_none());
    }

    fn assert_refresh_conserves(d: &RefreshDelta<u128>) {
        assert_eq!(d.keeper_cut + d.paydown + d.backstop_cut + d.buffer_amount, d.amount);
    }

    #[test]
    fn refresh_all_to_buffer() {
        let d = refresh(10_000u128, 0, 0, 0, 0, 0).unwrap();
        assert_eq!(d.amount, 10_000);
        assert_eq!(d.keeper_cut, 0);
        assert_eq!(d.paydown, 0);
        assert_eq!(d.backstop_cut, 0);
        assert_eq!(d.buffer_amount, 10_000);
        assert_eq!(d.new_unminted, 0);
        assert_eq!(d.new_bad, 0);
        assert_refresh_conserves(&d);
    }

    #[test]
    fn refresh_keeper_cut_floors() {
        // 10% of 10_005 floors to 1_000 (buffer-favoring).
        let d = refresh(10_005u128, 0, 1_000, 0, 0, 0).unwrap();
        assert_eq!(d.keeper_cut, 1_000);
        assert_eq!(d.buffer_amount, 9_005);
        assert_refresh_conserves(&d);
    }

    #[test]
    fn refresh_paydown_capped_by_bad_debt() {
        // want = 10_000 * 50% = 5_000, capped at bad0 = 300.
        let d = refresh(10_000u128, 300, 0, 5_000, 0, 0).unwrap();
        assert_eq!(d.paydown, 300);
        assert_eq!(d.new_bad, 0);
        assert_eq!(d.buffer_amount, 9_700);
        assert_refresh_conserves(&d);
    }

    #[test]
    fn refresh_paydown_gate_needs_bad_debt() {
        // paydown_bps > 0 but bad0 == 0 → no paydown (the handler gate).
        let d = refresh(10_000u128, 0, 0, 5_000, 0, 0).unwrap();
        assert_eq!(d.paydown, 0);
        assert_eq!(d.buffer_amount, 10_000);
        assert_refresh_conserves(&d);
    }

    #[test]
    fn refresh_backstop_cut_capped_by_headroom() {
        // want = 10_000 * 30% = 3_000, capped at headroom = 700.
        let d = refresh(10_000u128, 0, 0, 0, 3_000, 700).unwrap();
        assert_eq!(d.backstop_cut, 700);
        assert_eq!(d.buffer_amount, 9_300);
        assert_refresh_conserves(&d);
    }

    #[test]
    fn refresh_full_split() {
        // pending 20_000, bad 400: keeper 10% = 2_000; paydown 50% of 18_000 = 9_000 → capped 400;
        // backstop 30% of 17_600 = 5_280 → headroom 5_000; buffer = 12_600.
        let d = refresh(20_000u128, 400, 1_000, 5_000, 3_000, 5_000).unwrap();
        assert_eq!(d.keeper_cut, 2_000);
        assert_eq!(d.paydown, 400);
        assert_eq!(d.backstop_cut, 5_000);
        assert_eq!(d.buffer_amount, 12_600);
        assert_eq!(d.new_unminted, 0);
        assert_eq!(d.new_bad, 0);
        assert_refresh_conserves(&d);
    }

    #[test]
    fn refresh_amount_capped_at_u64_max() {
        let pending = u64::MAX as u128 + 5;
        let d = refresh(pending, 0, 0, 0, 0, 0).unwrap();
        assert_eq!(d.amount, u64::MAX as u128);
        assert_eq!(d.new_unminted, 5);
        assert_refresh_conserves(&d);
    }

    #[test]
    fn liquidate_full_waterfall() {
        let d = liquidate(10_000u128, 50, 3_000, 2_000, 1_000, 500, 250).unwrap();
        assert_eq!(d.burned, 4_500); // reactor + buffer + global
        // redist (2_000) stays parked in agg.
        assert_eq!(d.new_agg, 10_000 - 3_000 - 1_000 - 500 - 250);
        assert_eq!(d.new_bad, 300);
    }

    #[test]
    fn liquidate_redist_is_supply_neutral() {
        // A redistribution-only liquidation leaves agg AND bad untouched.
        let d = liquidate(10_000u128, 50, 0, 4_000, 0, 0, 0).unwrap();
        assert_eq!(d.burned, 0);
        assert_eq!(d.new_agg, 10_000);
        assert_eq!(d.new_bad, 50);
    }

    #[test]
    fn liquidate_underflow_is_none() {
        assert!(liquidate(1_000u128, 0, 600, 0, 600, 0, 0).is_none());
    }

    #[test]
    fn redeem_step_unbooks_the_burn() {
        let d = redeem_step(1_000u128, 400).unwrap();
        assert_eq!(d.burned, 400);
        assert_eq!(d.new_agg, 600);
        assert!(redeem_step(100u128, 400).is_none());
    }

    #[test]
    fn settle_bad_debt_retires() {
        assert_eq!(settle_bad_debt(500u128, 200), Some(300));
        assert_eq!(settle_bad_debt(500u128, 500), Some(0));
        assert_eq!(settle_bad_debt(100u128, 200), None);
    }

    #[test]
    fn book_interest_moves_in_lockstep() {
        let d = book_interest(1_000u128, 40, 7).unwrap();
        assert_eq!(d.new_agg, 1_007);
        assert_eq!(d.new_unminted, 47);
        assert!(book_interest(u128::MAX, 0, 1u128).is_none());
        assert!(book_interest(0u128, u128::MAX, 1u128).is_none());
    }
}
