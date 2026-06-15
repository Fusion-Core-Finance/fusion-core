//! Certora/CVLR spec — **Invariant #2: bitmap find-first-set preservation** (your B8 strength, lifted
//! to the instruction level — the second must-have for the Phase-1 FV exit).
//!
//! After every instruction that routes through `bucket::reconcile`, the per-market `RedemptionBitmap`
//! is COHERENT with the position set:
//!   (a) for all k:  `words` bit k set  ⟺  `counts[k] > 0`   (the find-first-set coupling)
//!   (b) `first_set(words)` is the true lowest occupied NORMAL bucket  (what `redeem` relies on)
//!   (c) the touched position's STORED `bucket` matches its debt/ink classification (`bucket::target`).
//!
//! FRAMING (document this explicitly — common reviewer question): the "two concurrent redemptions
//! never disagree on the lowest non-empty bucket" guarantee is provided by the **per-market `Market`
//! write-lock** (Sealevel serializes any two txs that write the same market), NOT by Certora modeling
//! parallelism. So the Certora property is the per-tx one: *each* instruction atomically preserves
//! coherence; serialization composes it across concurrent txs. The B8 stateful BTreeSet proptest proved
//! the *bit math* (`fusd_math::rate_bucket`); this proves the *handlers maintain the coupling*.
//!
//! Runnable counterpart (the mutation oracle): `assert_bitmap_coherent` (`integration-tests/src/lib.rs`),
//! asserted after every tx by `litesvm_invariants_fuzz.rs`. Mutation that must break BOTH this rule and
//! that suite: skip a `bucket::reconcile` call (verified at the runnable layer — `mutations.md` row B1).
//!
//! STATUS: spec scaffold — CVLR API confirmed (cvlr 0.6); remaining `// CONFIRM` = harness glue (see supply_invariant.rs header).
#![cfg(feature = "certora")]
#![allow(unused)]

use cvlr::prelude::*; // confirmed (cvlr 0.6)

use fusd_core::constants::{NUM_RATE_BUCKETS, ZOMBIE_BUCKET};

/// (a)+(b): the bitmap's `words`/`counts` are internally coherent. Quantified over all 256 buckets.
/// CONFIRM: whether the prover handles a 256-iteration `for` in a rule predicate directly, or whether
/// this must be expressed with a nondet witness bucket `k` (assert for an ARBITRARY k — usually the
/// tractable form on the Solana prover): `let k: usize = nondet(); assume k < NUM; assert bit(k) == (counts[k]>0)`.
fn bitmap_coherent(bm: &RedemptionBitmap) -> bool {
    let mut lowest_bit: Option<usize> = None;
    let mut k = 0;
    while k < NUM_RATE_BUCKETS {
        let bit = rb_is_set(&bm.words, k);            // CONFIRM: reuse fusd_math::rate_bucket::is_set
        if bit != (bm.counts[k] > 0) {
            return false;                              // (a) coupling broken
        }
        if bm.counts[k] > 0 && lowest_bit.is_none() {
            lowest_bit = Some(k);
        }
        k += 1;
    }
    rb_first_set(&bm.words) == lowest_bit              // (b) find-first-set == true lowest occupied
}

/// (c): the touched position's stored `bucket` equals what `bucket::target` would classify it as, given
/// its post-op `recorded_debt`/`ink`. (debt==0 ⇒ membership of none, stored bucket is don't-care.)
fn stored_bucket_matches(m: &Market, p: &Position) -> bool {
    if p.recorded_debt == 0 {
        return true;
    }
    let target = if p.ink == 0 || p.recorded_debt < m.min_debt as u128 {
        ZOMBIE_BUCKET
    } else {
        rb_bucket_of(p.user_rate_bps, m.bucket_width_bps, NUM_RATE_BUCKETS)
    };
    p.bucket as usize == target
}

fn bitmap_holds(bm: &RedemptionBitmap, m: &Market, p: &Position) -> bool {
    bitmap_coherent(bm) && stored_bucket_matches(m, p)
}

// One rule per instruction that calls `bucket::reconcile`. Inductive preservation: assume coherence in
// the pre-state, execute with nondet args, assert coherence in the post-state.
//
// NOTE the pre-state must ALSO assume the *aggregate* count invariant linking `counts[k]` to the full
// position set is consistent for the touched position — but keep this MINIMAL (only what reconcile
// relies on) to avoid vacuity. The runnable `assert_bitmap_coherent` reconstructs counts over ALL
// positions; the Certora rule, modeling one tx, asserts coherence is *preserved* given it held before.

macro_rules! bucket_preservation_rule {
    ($name:ident, $ctor:ident, $handler:ident $(, $arg:ident : $ty:ty)*) => {
        #[rule]
        pub fn $name() {
            let mut cx = $ctor();
            cvlr_assume!(bitmap_holds(&cx.redemption_bitmap, &cx.market, &cx.position));
            cvlr_assume!(account_valid(&cx));
            $( let $arg: $ty = nondet(); )*
            let _ = $handler(&mut cx $(, $arg)*);
            cvlr_assert!(bitmap_holds(&cx.redemption_bitmap, &cx.market, &cx.position));
        }
    };
}

bucket_preservation_rule!(bitmap_preserved_by_borrow,      borrow_context_nondet,      borrow_handler,      amount: u64);
bucket_preservation_rule!(bitmap_preserved_by_repay,       repay_context_nondet,       repay_handler,       amount: u64);
bucket_preservation_rule!(bitmap_preserved_by_deposit,     deposit_context_nondet,     deposit_handler,     amount: u64);
bucket_preservation_rule!(bitmap_preserved_by_withdraw,    withdraw_context_nondet,    withdraw_handler,    amount: u64);
bucket_preservation_rule!(bitmap_preserved_by_adjust_rate, adjust_rate_context_nondet, adjust_rate_handler, new_rate: u16);

// `liquidate` and `redeem` also reconcile, but touch MULTIPLE positions (redeem) or zero a victim
// (liquidate); model each with the appropriate touched-position set. The find-first-set property is the
// load-bearing one for `redeem` (it must START at the lowest non-empty bucket).
bucket_preservation_rule!(bitmap_preserved_by_liquidate,   liquidate_context_nondet,   liquidate_handler);

/// `urgent_redeem` (shutdown wind-down) ALSO calls `bucket::reconcile` (it drains positions in a
/// shut-down market, any bucket, unordered) — so it is a reconcile caller and must preserve coherence.
/// Multi-position touch like `redeem`; model the candidate set via remaining_accounts. (Targeting is
/// NOT asserted here: urgent_redeem is deliberately unordered, so only coherence is the obligation.)
bucket_preservation_rule!(bitmap_preserved_by_urgent_redeem, urgent_redeem_context_nondet, urgent_redeem_handler, amount: u64);

/// `redeem` MUST drain starting at the lowest non-empty bucket (it cannot skip it). Beyond coherence,
/// assert the targeting precondition: every position the drain touched carried `bucket == first_set` of
/// the PRE-state bitmap (the strict lower-bucket guarantee — skip-not-revert means a dodged candidate
/// is skipped, never a lower bucket targeted). Model the candidate set via remaining_accounts.
///
/// ⚠ The TARGETING assertion below (not just coherence) is the LOAD-BEARING part of this rule and the
/// thing mutation B3 must break — it requires wiring a ghost variable / observed-targets channel in the
/// handler model (flagged CONFIRM). Until that is wired, this rule only proves coherence, NOT targeting;
/// `certora/mutations.md` row B3 is marked accordingly. Do not tick B3 until the targeting assert is live.
#[rule]
pub fn redeem_targets_lowest_bucket_and_preserves_coherence() {
    let mut cx = redeem_context_nondet();
    cvlr_assume!(bitmap_coherent(&cx.redemption_bitmap));
    cvlr_assume!(account_valid(&cx));
    let pre_lowest = rb_first_set(&cx.redemption_bitmap.words); // CONFIRM read of pre-state words
    let amount: u64 = nondet();
    let _ = redeem_handler(&mut cx, amount);
    // Coherence is preserved (the part proven today).
    cvlr_assert!(bitmap_coherent(&cx.redemption_bitmap));
    // TARGETING (CONFIRM — the B3-breaking assertion): assert every touched position carried
    // `bucket == pre_lowest`. Express via a ghost variable updated in the handler model / observed
    // targets. Without this, B3 is NOT covered.
    let _ = pre_lowest; // becomes: cvlr_assert!(all_touched_targets_eq(&cx, pre_lowest));
}
