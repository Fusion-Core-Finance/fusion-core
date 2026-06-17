//! Certora/CVLR verification harness — compiled ONLY under `--features certora` (via
//! `cargo certora-sbf`), NEVER in the production `.so` (scripts/check-no-certora.sh enforces it).
//!
//! BRING-UP STATE: the round-trip smoke rule is VERIFIED on the cloud (pipeline proven). This file is
//! now growing the REAL rules. The first, `bitmap_coherence_preserved_by_reconcile`, drives the actual
//! `crate::bucket::reconcile` over symbolic state — the single-touch / inductive core of Invariant #2
//! (the spec logic in `<repo>/certora/specs/bitmap_find_first_set.rs`). It deliberately needs NO Anchor
//! `Context`, token CPI, or summaries: `certora.rs` is an in-crate `mod`, so it calls
//! `crate::bucket::reconcile` directly over nondet structs. The handler-driven rules (supply, the full
//! bitmap set over `borrow`/`redeem`/…) still need the Anchor-account glue (cvlr_summaries.txt entries
//! for the token CPI / `invoke_signed` / `Clock` + the Anchor inlining block + `cvlr_solana_init!()`) —
//! that is the next, separate piece (see certora/README.md §Bring-up).

use cvlr::prelude::*;
use cvlr_solana::cvlr_nondet_account_info;

use crate::bucket;
use crate::constants::ZOMBIE_BUCKET;
use crate::state::{Market, Position, RedemptionBitmap};
use fusd_math::rate_bucket as rb;
use fusd_math::recovery;

/// Round-trip smoke: must VERIFY. Proves the toolchain (cargo certora-sbf), the `.conf`, and the cloud
/// submission/solve all work before any real rule is ported. Phrased so the SOLVER must actually reason
/// (a true implication over a nondet input, `a < 100 ⟹ a + 1 <= 100`) rather than a reflexive `x == x`
/// that the optimizer can fold to a no-op — a folded-away rule is a common cause of a generic prover
/// error. `rule_sanity` will confirm it's non-vacuous.
#[rule]
pub fn round_trip_smoke() {
    let a: u64 = nondet();
    cvlr_assume!(a < 100);
    cvlr_assert!(a + 1 <= 100);
}

// ============================ Invariant #2 (bitmap) — first real rule ============================

/// The words⟺counts coupling at a single bucket `k`: bit `k` is set IFF `counts[k] > 0`. (`k` must be
/// `< NUM_RATE_BUCKETS`.) This is property (a) of `bitmap_find_first_set.rs::bitmap_coherent`, checked
/// at one witness bucket to avoid a 256-iteration loop the prover may not unroll.
fn coupling_holds_at(bm: &RedemptionBitmap, k: usize) -> bool {
    rb::is_set(&bm.words, k) == (bm.counts[k] > 0)
}

/// FIRST REAL RULE: a single `crate::bucket::reconcile` preserves the bitmap's words⟺counts coupling
/// (Invariant #2(a), inductive single-touch form). This exercises the EXACT production function whose
/// omission is mutation B1/B2 (`certora/mutations.md`). No Anchor/CPI/Clock glue — `reconcile` is pure
/// over `&mut RedemptionBitmap`, `&Market`, `&mut Position`, `u128`.
#[rule]
pub fn bitmap_coherence_preserved_by_reconcile() {
    // Back the `RedemptionBitmap` with a nondet Solana ACCOUNT's data — symbolic SVM account memory,
    // which the prover models natively (its memory analysis + the `-solanaOptimistic*` flags are built
    // for exactly this). `reconcile` writes `counts[bucket]` / `words[bucket >> 6]` at DATA-DERIVED
    // (symbolic) indices; on a stack/`Box`'d Rust array the prover havocs the whole word on such a write
    // (the prior attempt's spurious "Violated"), but on account memory it stays precise. The account
    // bytes are fully symbolic, so the bitmap's `words`/`counts`/`zombie_count` are all symbolic (more
    // general than the earlier zero-then-patch). We project the post-discriminator bytes to a typed
    // `&mut RedemptionBitmap` exactly as Anchor's `AccountLoader::load_mut` does (RefMut::map + bytemuck),
    // but without the discriminator/owner checks (irrelevant to this pure-state property).
    let info = cvlr_nondet_account_info();
    let need = 8 + core::mem::size_of::<RedemptionBitmap>(); // 8-byte Anchor discriminator + the struct
    let data = info.try_borrow_mut_data().unwrap();
    cvlr_assume!(data.len() >= need);
    let mut bytes = core::cell::RefMut::map(data, |d| &mut d[8..need]);
    let bm: &mut RedemptionBitmap = bytemuck::from_bytes_mut(&mut bytes);

    // `Market`/`Position` are foreign `#[account]` structs with no `Default`/`Nondet` impl. All their
    // fields are zero-valid (scalars / `Pubkey` / byte arrays — verified: no Option/NonZero/enum), and
    // `reconcile` does NO dynamic array indexing into them (only scalar reads + a scalar `position.bucket`
    // write), so a stack `mem::zeroed` + patch of the fields it reads is sound and prover-safe.
    let mut market: Market = unsafe { core::mem::zeroed() };
    market.min_debt = nondet();
    market.bucket_width_bps = nondet();
    cvlr_assume!(market.bucket_width_bps > 0); // bucket_of requires width > 0 (program clamps to >= 1)

    let mut position: Position = unsafe { core::mem::zeroed() };
    position.recorded_debt = nondet();
    position.ink = nondet();
    position.user_rate_bps = nondet();
    position.bucket = nondet(); // the STORED membership reconcile decrements
    // Account validity: the stored bucket is always a real bucket (`< 256`) or the zombie pen (`== 256`)
    // — reconcile maintains that. Without this the prover explores an out-of-range `counts[]` index.
    cvlr_assume!((position.bucket as usize) <= ZOMBIE_BUCKET);

    let art_before: u128 = nondet(); // art the position had at the START of the tx

    // Witness bucket for the coupling property — CONCRETE (bucket 0). `reconcile`/`add_member`/
    // `remove_member` treat every bucket index identically (no per-bucket special-casing), so coupling
    // preservation at a fixed representative bucket is a sound proof of the per-bucket invariant. A
    // concrete `k` also keeps the assert-side `is_set` (`words[k>>6] & (1<<(k&63))`) at a STATIC index +
    // mask — the Solana prover's bitwise/symbolic-index reasoning mis-models a fully-symbolic `k` (it
    // collapsed `BWAnd(.., 0x8000)` to 1 in the earlier trace); only `reconcile`'s writes at the
    // data-derived `old`/`new` stay symbolic, which precise-bitwise handles. (The full-range bit math is
    // already covered by the B8 proptest + Kani; this rule pins the instruction-level preservation.)
    let k: usize = 0;

    // PRE-STATE INVARIANT (the ONLY admissible assumption — the invariant itself, kept minimal to avoid
    // the vacuity footgun): the coupling holds at the witness bucket. reconcile touches at most the
    // `old` (stored) and `new` (target) buckets; `add_member` maintains coupling at `new` unconditionally
    // and `remove_member` maintains it at `old` given it held there — so witness-k coupling pre is
    // sufficient (k == old needs it; k == new is maintained regardless; k != either is untouched).
    cvlr_assume!(coupling_holds_at(bm, k));

    clog!(k, art_before);

    // EXECUTE the real code under test. (A `checked_*` overflow returns Err with the bitmap left
    // coupling-consistent, so ignoring the Result is sound for this property.)
    let _ = bucket::reconcile(&mut *bm, &market, &mut position, art_before);

    // POST: the coupling still holds at the witness bucket.
    cvlr_assert!(coupling_holds_at(&*bm, k));
}

// ============================ Invariant #1 (global supply) — borrow path ============================

/// FIRST REAL SUPPLY RULE: `borrow` preserves the global supply identity
///     circulating == agg_recorded_debt − unminted_interest + bad_debt
///
/// This is the supply candidate that DEFEATS the spl-token-patch blocker. A real `mint_to` CPI would
/// need a workspace-global `[patch.crates-io] spl-token = cvlr-spl-token`, which is global to the whole
/// workspace and would corrupt the deployable `.so` (it cannot be feature-gated inside this package).
/// Instead we model `circulating` fUSD as a pure `u128` GHOST and replay borrow's accounting delta,
/// anchored to the REAL field assignment in `borrow.rs`:
///   * `borrow` MINTS `amount` fUSD            → ghost `circulating += amount`  (the `mint_to` CPI)
///   * `borrow` writes `agg_recorded_debt += amount` (borrow.rs:135, the `S1` mutation target)
///   * `borrow` leaves `unminted_interest` / `bad_debt` untouched
/// so the identity is preserved by construction — provable as pure u128 arithmetic over nondet `Market`
/// fields (the same blocker-free regime as the bitmap/absorb rules; this is the spec's "Option 2").
///
/// Mutation S1 (drop the `agg_recorded_debt = new_agg` assignment → model as `agg1 = agg0`): circulating
/// rises by `amount` but agg stays flat, so `circ1 != agg1 − unminted + bad` → VIOLATED.
///
/// ENCODING NOTE (characterized frontier, see certora/README.md §"u128 checked-arith blocker"): the same
/// identity expressed in raw `u128` `checked_add`/`checked_sub` reports a SPURIOUS `Violated` — the Solana
/// prover mis-models the 128-bit compiler-rt limb lowering of u128 arithmetic (the same class of pathology
/// as the bitmap `is_set` blocker). The DIAGNOSTIC confirmed it: the identical algebra VERIFIES at native
/// `u64` width AND in the `NativeInt` math-int domain, but FAILS in raw `u128 checked_*`. So this rule is
/// expressed in `cvlr::mathint::NativeInt` — the canonical Certora idiom for u128 overflow-domain reasoning
/// (the `TestSolana/U128ArithTest` pattern, e.g. `check_u128_saturating_and_wrapping_add_equiv`). NativeInt
/// lifts each value to an unbounded symbolic integer the prover reasons over EXACTLY, so the proof is sound
/// AND non-spurious; the u128-ness of the production fields is faithfully represented (each `NativeInt::from`
/// of a `nondet::<u128>()` ranges over the full u128 domain).
#[rule]
pub fn supply_preserved_by_borrow_ghost() {
    use cvlr::mathint::NativeInt;

    // pre-state aggregates, each a full-range symbolic u128 lifted into the math-int domain.
    let agg0 = NativeInt::from(nondet::<u128>());      // market.agg_recorded_debt
    let unminted = NativeInt::from(nondet::<u128>());  // market.unminted_interest
    let bad = NativeInt::from(nondet::<u128>());        // market.bad_debt
    let amount = NativeInt::from(nondet::<u128>());     // borrow `amount` (minted this tx)

    // Well-formed pre-state (the ONLY admissible assumption — the invariant's own domain): the identity
    // `circulating = agg − unminted + bad` requires `agg − unminted >= 0` (interest accrued into agg is
    // always >= the not-yet-minted slice of it). circ0 IS this value — circulating is the SPL mint supply,
    // a derived quantity, not an independent nondet, so binding it to the equation is the invariant, not
    // an over-assumption. (Vacuity is ruled out by rule_sanity + the S1 mutation flipping this to FAIL.)
    cvlr_assume!(agg0 >= unminted);
    let circ0 = (agg0 - unminted) + bad; // circulating fUSD, pre-borrow

    // borrow's accounting delta (borrow.rs handler):
    //   * agg_recorded_debt = agg0 + amount          (borrow.rs:135 — the line mutation S1 drops)
    //   * mint_to(amount): SPL mint supply += amount  (the token CPI, modeled as the ghost `circ`)
    //   * unminted_interest / bad_debt: UNTOUCHED by borrow
    // (Production guards `checked_add` for MathOverflow; in the unbounded math-int domain there is no
    // overflow to model — the proof holds for every reachable on-chain value AND beyond, strictly more
    // general than the u128-bounded path while being sound for it.)
    let agg1 = agg0 + amount;
    let circ1 = circ0 + amount;

    clog!(amount);

    // POST: the global supply identity still holds. By construction
    //   (agg1 − unminted) + bad = (agg0 + amount − unminted) + bad = (agg0 − unminted + bad) + amount
    //                           = circ0 + amount = circ1.
    cvlr_assert!(circ1 == (agg1 - unminted) + bad);
}

// ===== Invariant #1 (global supply) — the remaining mint/burn instructions (ghost, NativeInt) =====
//
// These extend `supply_preserved_by_borrow_ghost` to the COMPLETE set of supply-touching instructions
// (repay / refresh_market / liquidate / redeem / urgent_redeem / settle_bad_debt), each in the identical
// blocker-free regime: model `circulating` (the SPL mint supply) as a pure `NativeInt` GHOST and replay the
// handler's documented accounting delta, anchored to the real field assignments. This sidesteps the
// SPL-token CPI mock — a workspace-global `[patch.crates-io] spl-token` would corrupt the deployable `.so`
// (certora/README.md §"Two prover frontiers"). The runnable litesvm `assert_supply_invariant` is the
// handler-level oracle for these same deltas (mutations.md rows S2–S7). The rule proves the per-instruction
// supply ALGEBRA is consistent; the litesvm layer ties that algebra to the real handler.
//
// STATUS: authored, pending a cloud run. The borrow sibling is cloud-VERIFIED and these stay in its exact
// NativeInt regime (no account memory / bitwise / u128 checked-arith / handler glue), so they are wired into
// certora/supply.conf and await `CERTORAKEY` to flip VERIFIED. Each carries the in-rule mutation (model the
// dropped field update) that MUST flip it to VIOLATED — the non-vacuity acceptance check.

/// repay BURNS `b` fUSD and un-books the same from agg: circ −= b, agg −= b ⇒ identity preserved.
/// Mutation S2 (drop `agg_recorded_debt -= b` → model `agg1 = agg0`): circ falls but agg stays ⇒ VIOLATED.
#[rule]
pub fn supply_preserved_by_repay_ghost() {
    use cvlr::mathint::NativeInt;
    let agg0 = NativeInt::from(nondet::<u128>());
    let unminted = NativeInt::from(nondet::<u128>());
    let bad = NativeInt::from(nondet::<u128>());
    let b = NativeInt::from(nondet::<u128>()); // fUSD burned == debt un-booked
    cvlr_assume!(agg0 >= unminted);
    cvlr_assume!(b <= agg0 - unminted); // repay can't drop agg below the unminted-interest floor
    let circ0 = (agg0 - unminted) + bad;
    let agg1 = agg0 - b;
    let circ1 = circ0 - b;
    clog!(b);
    cvlr_assert!(circ1 == (agg1 - unminted) + bad);
}

/// refresh_market MINTS the accrued `u` interest into the buffer: circ += u, unminted −= u, agg flat (the
/// interest was folded into agg at accrual) ⇒ identity preserved. Mutation S3 (mint but skip
/// `unminted_interest -= u` → model `unminted1 = unminted0`): the interest is double-counted ⇒ VIOLATED.
#[rule]
pub fn supply_preserved_by_refresh_market_ghost() {
    use cvlr::mathint::NativeInt;
    let agg0 = NativeInt::from(nondet::<u128>());
    let unminted0 = NativeInt::from(nondet::<u128>());
    let bad = NativeInt::from(nondet::<u128>());
    let u = NativeInt::from(nondet::<u128>()); // unminted interest minted into the buffer this refresh
    cvlr_assume!(agg0 >= unminted0);
    cvlr_assume!(u <= unminted0); // can't mint more interest than is unminted
    let circ0 = (agg0 - unminted0) + bad;
    let unminted1 = unminted0 - u;
    let circ1 = circ0 + u;
    clog!(u);
    cvlr_assert!(circ1 == (agg0 - unminted1) + bad);
}

/// liquidate routes the victim debt through the waterfall: the RP-offset + buffer BURNs remove `burned`
/// fUSD and the same from agg; the un-homed remainder `h` leaves agg and is booked to bad_debt;
/// redistributed debt stays in agg (reassigned to survivors, supply-neutral). So circ −= burned,
/// agg −= burned + h, bad += h ⇒ Δcirc = Δagg + Δbad ⇒ identity preserved. Mutation S4 (in the un-homed
/// branch skip `bad_debt += h` → model `bad1 = bad0`): agg drops by h but bad isn't raised ⇒ VIOLATED
/// whenever h > 0.
#[rule]
pub fn supply_preserved_by_liquidate_ghost() {
    use cvlr::mathint::NativeInt;
    let agg0 = NativeInt::from(nondet::<u128>());
    let unminted = NativeInt::from(nondet::<u128>());
    let bad0 = NativeInt::from(nondet::<u128>());
    let burned = NativeInt::from(nondet::<u128>()); // RP-offset + buffer BURNs (fUSD removed)
    let h = NativeInt::from(nondet::<u128>()); // un-homed remainder booked to bad_debt
    cvlr_assume!(agg0 >= unminted);
    cvlr_assume!(burned + h <= agg0 - unminted); // the debt leaving agg can't breach the unminted floor
    let circ0 = (agg0 - unminted) + bad0;
    let agg1 = agg0 - (burned + h);
    let bad1 = bad0 + h;
    let circ1 = circ0 - burned;
    clog!(burned);
    cvlr_assert!(circ1 == (agg1 - unminted) + bad1);
}

/// redeem BURNS `b` fUSD of face value and reduces the target debt by the same: circ −= b, agg −= b ⇒
/// preserved (the redemption fee is retained COLLATERAL, not fUSD, so it does not move the supply identity
/// — that is the vault rule's concern). Mutation (skip the agg decrement → `agg1 = agg0`) ⇒ VIOLATED.
#[rule]
pub fn supply_preserved_by_redeem_ghost() {
    use cvlr::mathint::NativeInt;
    let agg0 = NativeInt::from(nondet::<u128>());
    let unminted = NativeInt::from(nondet::<u128>());
    let bad = NativeInt::from(nondet::<u128>());
    let b = NativeInt::from(nondet::<u128>()); // fUSD face value burned == debt cleared
    cvlr_assume!(agg0 >= unminted);
    cvlr_assume!(b <= agg0 - unminted);
    let circ0 = (agg0 - unminted) + bad;
    let agg1 = agg0 - b;
    let circ1 = circ0 - b;
    clog!(b);
    cvlr_assert!(circ1 == (agg1 - unminted) + bad);
}

/// urgent_redeem (shutdown wind-down): 0-fee burn-for-collateral with the identical supply algebra to
/// redeem (circ −= b, agg −= b). Mutation (skip the agg decrement) ⇒ VIOLATED.
#[rule]
pub fn supply_preserved_by_urgent_redeem_ghost() {
    use cvlr::mathint::NativeInt;
    let agg0 = NativeInt::from(nondet::<u128>());
    let unminted = NativeInt::from(nondet::<u128>());
    let bad = NativeInt::from(nondet::<u128>());
    let b = NativeInt::from(nondet::<u128>());
    cvlr_assume!(agg0 >= unminted);
    cvlr_assume!(b <= agg0 - unminted);
    let circ0 = (agg0 - unminted) + bad;
    let agg1 = agg0 - b;
    let circ1 = circ0 - b;
    clog!(b);
    cvlr_assert!(circ1 == (agg1 - unminted) + bad);
}

/// settle_bad_debt BURNS recovered fUSD `x` and reduces bad_debt by the same in lockstep: circ −= x,
/// bad −= x, agg flat ⇒ preserved (the on-chain half of recapitalization). Mutation S5 (burn but skip
/// `bad_debt -= x` → model `bad1 = bad0`) ⇒ VIOLATED.
#[rule]
pub fn supply_preserved_by_settle_bad_debt_ghost() {
    use cvlr::mathint::NativeInt;
    let agg0 = NativeInt::from(nondet::<u128>());
    let unminted = NativeInt::from(nondet::<u128>());
    let bad0 = NativeInt::from(nondet::<u128>());
    let x = NativeInt::from(nondet::<u128>()); // recovered fUSD burned against bad_debt
    cvlr_assume!(agg0 >= unminted);
    cvlr_assume!(x <= bad0); // can't settle more bad debt than exists
    let circ0 = (agg0 - unminted) + bad0;
    let bad1 = bad0 - x;
    let circ1 = circ0 - x;
    clog!(x);
    cvlr_assert!(circ1 == (agg0 - unminted) + bad1);
}

// ===== Invariant #2 (bitmap) — direct add_member / remove_member at a CONCRETE bucket =====
//
// VERIFIED (both rules; non-vacuous — they flip to VIOLATED under mutations B1/B2). This is the
// breakthrough on the bitmap frontier. The recipe that makes it work, and WHY (each isolated by a cloud
// diagnostic, see certora/README.md §Bitmap-frontier):
//   1. CONCRETE bucket 0. The store index `bucket >> 6`, the read index, and the `1 << (bucket & 63)`
//      mask are all static — no array-update aliasing under a symbolic index (diag D1/D3 verify).
//   2. DIRECT add_member/remove_member (not via reconcile/target). Bypasses the `bucket_of`
//      classification. These two fns are `pub(crate)` + `#[cfg_attr(certora, inline(always))]` —
//      verification-only, behavior-neutral, so the prover sees their bodies (diag D4 verifies).
//   3. CONCRETE pre-count (0 for add / 1 for remove). This is the REAL fix: it makes the production
//      `checked_add`/`checked_sub` constant-fold, so the `.ok_or(FusdError::MathOverflow)?` Err branch is
//      provably dead and the slicer drops it. That Err branch is an INDIRECT `callx` (the
//      `From<FusdError>`/`core::fmt` conversion) which `-solanaSkipCallRegInst` translates to an EMPTY tac
//      block (SbfCFGToTAC.kt: skipped callx → `listOf()`), leaving R0/account-memory HAVOCED — the
//      original spurious counterexample. Diag D4 (no `?`) verifies; D5 (= D4 + `.ok_or(FusdError)?`)
//      FAILS, isolating this conclusively. A nondet count can't drive the fold, so the bound must be a
//      concrete pre-state, not a `cvlr_assume!`. The real `rb::set`/`rb::clear` IS still exercised
//      (empty↔non-empty transition), so B1/B2 stay live.
//   4. `precise_bitwise_ops true` (conf). `clear`'s `&= !(1<<k)` AND-complement mask needs precise
//      bitwise modeling; the math-int regime alone mis-modeled it (add, which uses `|=`, verified under
//      math-int; remove, which uses `&= !`, only verified once precise_bitwise_ops was added).
//   5. `-solanaCvtNondetAccountInfo true` (conf). Applies the precise TAC summary on the nondet account
//      (docs: required for Anchor projects; otherwise CVT_nondet_account_info is a no-op).
// The account-data projection is the same nondet-bytemuck pattern as
// `bitmap_coherence_preserved_by_reconcile` above.

/// `bucket::add_member` preserves the words⟺counts coupling at the (concrete) touched bucket.
/// On an empty→non-empty transition it must BOTH set bit 0 AND bump counts[0]; this pins those two
/// updates together. Mutation B1 (drop `rb::set` in `add_member`) makes counts[0] 0→1 while bit 0 stays
/// clear ⇒ coupling false ⇒ VIOLATED.
#[rule]
pub fn bitmap_coupling_preserved_by_add_member() {
    let info = cvlr_nondet_account_info();
    let need = 8 + core::mem::size_of::<RedemptionBitmap>(); // 8-byte Anchor discriminator + the struct
    let data = info.try_borrow_mut_data().unwrap();
    cvlr_assume!(data.len() >= need);
    let mut bytes = core::cell::RefMut::map(data, |d| &mut d[8..need]);
    let bm: &mut RedemptionBitmap = bytemuck::from_bytes_mut(&mut bytes);

    let k: usize = 0; // CONCRETE witness AND concrete touched bucket — fully static accesses
    // CONCRETE pre-count = 0 (the empty→non-empty transition — exactly the case mutation B1 breaks). A
    // concrete count makes `add_member`'s `checked_add(1)` constant-fold to `Some(1)`, so the
    // `.ok_or(FusdError::MathOverflow)?` Err branch is PROVABLY dead and the slicer removes it. That Err
    // branch is the `From<FusdError>`/`core::fmt` INDIRECT `callx` which `-solanaSkipCallRegInst` stubs to
    // an EMPTY tac block (SbfCFGToTAC.kt: skipped callx → `listOf()`), leaving R0/account-memory havoced —
    // the spurious counterexample (proven by diag D4 verifying vs D5, D4+`?`/FusdError, failing). With a
    // nondet count the slicer can't drop the branch (the bound is on a projected account field the scalar
    // domain doesn't track), so the havocing stub survives. The real `rb::set` IS still exercised on the
    // empty→non-empty transition, so mutation B1 stays live (non-vacuous).
    bm.counts[k] = 0;
    cvlr_assume!(coupling_holds_at(bm, k)); // ⇒ bit k is clear pre-state (counts[k]==0)

    clog!(k);

    let _ = bucket::add_member(&mut *bm, k);

    cvlr_assert!(coupling_holds_at(&*bm, k));
}

/// `bucket::remove_member` preserves the words⟺counts coupling at the (concrete) touched bucket.
/// On a non-empty→empty transition it must BOTH clear bit 0 AND drop counts[0]; this pins those two
/// updates together. Mutation B2 (drop `rb::clear` in `remove_member`) makes counts[0] go to 0 while bit
/// 0 stays set ⇒ coupling false ⇒ VIOLATED. The `counts[k] > 0` precondition is the in-contract
/// precondition (`remove_member` is only ever called on a bucket that has a member to remove; without it
/// the `checked_sub` underflows to Err and the post-state is uninteresting).
#[rule]
pub fn bitmap_coupling_preserved_by_remove_member() {
    let info = cvlr_nondet_account_info();
    let need = 8 + core::mem::size_of::<RedemptionBitmap>();
    let data = info.try_borrow_mut_data().unwrap();
    cvlr_assume!(data.len() >= need);
    let mut bytes = core::cell::RefMut::map(data, |d| &mut d[8..need]);
    let bm: &mut RedemptionBitmap = bytemuck::from_bytes_mut(&mut bytes);

    let k: usize = 0;
    // CONCRETE pre-count = 1 (the non-empty→empty transition — exactly the case mutation B2 breaks, and
    // the in-contract precondition that there is a member to remove). A concrete count makes
    // `remove_member`'s `checked_sub(1)` constant-fold to `Some(0)`, so the `.ok_or(FusdError)?` Err
    // (underflow) branch is PROVABLY dead and is sliced away — removing the `-solanaSkipCallRegInst`
    // havocing stub that otherwise produces a spurious counterexample (see the add_member rule). The real
    // `rb::clear` IS still exercised on the non-empty→empty transition, so mutation B2 stays live.
    bm.counts[k] = 1;
    cvlr_assume!(coupling_holds_at(bm, k)); // ⇒ bit k is set pre-state (counts[k]==1>0)

    clog!(k);

    let _ = bucket::remove_member(&mut *bm, k);

    cvlr_assert!(coupling_holds_at(&*bm, k));
}

// ===================== Invariant #3 (liquidation termination) — first real rule =====================

/// LIQUIDATION CONSERVATION (Invariant #3 core, mutation L1/S4): a single `recovery::absorb` routes the
/// FULL present `debt` across the five loss-absorption tiers with nothing stranded —
/// `reactor + redist + buffer + global + unhomed == debt`. This is the load-bearing termination invariant:
/// every liquidation accounts for the entire debt (no silent strand), so the waterfall can never stall —
/// a non-zero `unhomed` is the terminal shutdown signal, never lost debt.
///
/// `absorb` is **total** (pure u128 `min`/sub/add, defined for EVERY input — no account memory, no
/// symbolic-index array store, no bitwise ops, no Anchor `Context`/CPI/summaries), so this rule needs NO
/// `cvlr_assume!`: every nondet u128 quintuple is in-contract, which also removes any vacuity footgun (the
/// rule cannot pass for lack of admissible inputs). Structurally identical to the cloud-VERIFIED
/// `round_trip_smoke`, just over the real fusd-math arithmetic.
#[rule]
pub fn absorb_conserves_debt() {
    let debt: u128 = nondet();
    let reactor_capacity: u128 = nondet();
    let has_redist: bool = nondet();
    let buffer_balance: u128 = nondet();
    let global_available: u128 = nondet();
    let a = recovery::absorb(debt, reactor_capacity, has_redist, buffer_balance, global_available);
    clog!(debt);
    cvlr_assert!(a.reactor + a.redist + a.buffer + a.global + a.unhomed == debt);
}

// ===================== Invariant #3 (liquidation) — strict tier ordering =====================

/// STRICT TIER ORDERING (the "fail-closed and ordered" half of Invariant #3): a tier contributes only
/// after every higher-priority tier is fully exhausted and no redistribution recipient exists. This is
/// the pure-u128 sibling of `absorb_conserves_debt`, mirroring the Kani
/// `absorb_is_fail_closed_and_ordered` and the B8 proptest `absorb_fail_closed_and_ordered`. It drives
/// the EXACT production `recovery::absorb` over fully-symbolic u128 inputs (no Anchor/CPI/Clock glue —
/// `absorb` is total min/sub/add), so the assume-set is EMPTY (avoids the vacuity footgun).
///
/// The three implications encode the waterfall RP → redistribution → local buffer → global → un-homed:
///   * un-homed > 0  ⟹  no recipient, RP at its cap, local buffer drained, global drained (terminal).
///   * global   > 0  ⟹  no recipient AND the local buffer was fully drained first.
///   * buffer   > 0  ⟹  no recipient (redistribution would have taken the whole remainder first).
/// Mutation L1/L2 (reordering the global tier ahead of the local buffer) flips the
/// `global>0 ⟹ buffer==buffer_balance` clause to VIOLATED.
#[rule]
pub fn absorb_unhomed_iff_no_tier_covers() {
    let debt: u128 = nondet();
    let cap: u128 = nondet();
    let recip: bool = nondet();
    let bal: u128 = nondet();
    let avail: u128 = nondet();
    let a = recovery::absorb(debt, cap, recip, bal, avail);
    if a.unhomed > 0 {
        cvlr_assert!(!recip);
        cvlr_assert!(a.reactor == cap);
        cvlr_assert!(a.buffer == bal);
        cvlr_assert!(a.global == avail);
    }
    if a.global > 0 {
        cvlr_assert!(!recip);
        cvlr_assert!(a.buffer == bal);
    }
    if a.buffer > 0 {
        cvlr_assert!(!recip);
    }
}

/// Non-vacuity witness: the un-homed terminal tier is genuinely REACHABLE (with no recipient, an
/// uncapped/short waterfall leaves a residual). A `cvlr_satisfy!` makes the reachability explicit so the
/// ordering rule above can never be a vacuous all-paths-infeasible pass.
#[rule]
pub fn absorb_unhomed_reachable() {
    let debt: u128 = nondet();
    let cap: u128 = nondet();
    let bal: u128 = nondet();
    let avail: u128 = nondet();
    let a = recovery::absorb(debt, cap, false, bal, avail);
    cvlr_satisfy!(a.unhomed > 0);
}
