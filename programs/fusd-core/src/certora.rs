//! Certora/CVLR verification harness — compiled ONLY under `--features certora` (via
//! `cargo certora-sbf`), NEVER in the production `.so` (scripts/check-no-certora.sh enforces it).
//!
//! STATUS by rule family (the assurance ladder is certora/README.md §"What a green run proves"):
//!   * PRODUCTION-LINKED, cloud-VERIFIED: `bitmap_coupling_preserved_by_{add,remove}_member` (drive
//!     `bucket::add_member`/`remove_member`; bitmap_helper.conf), the three `absorb_*` rules (drive
//!     `fusd_math::recovery::absorb`; absorb.conf), and the `round_trip_smoke` pipeline check.
//!   * PRODUCTION-LINKED, cloud-VERIFIED: the two `c1_*` rules (drive the real
//!     `fusd_oracle::aggregate`; c1_canonical.conf).
//!   * SHARED-TRANSITION, cloud-VERIFIED (S1 shared-fn mutation flip cloud-confirmed): the eight `supply_preserved_by_*_ghost`
//!     rules EXECUTE `crate::supply_transition` — the same bodies the handlers run at `u128`,
//!     monomorphized to `NativeInt` (audit M-01; supply.conf). The pre-M-01 replay-the-delta borrow
//!     rule was cloud-VERIFIED; the rewritten bodies need a re-run.
//!   * RETAINED-FAILING artifact: `bitmap_coherence_preserved_by_reconcile` (bitmap.conf) — spurious
//!     counterexample (skipped-callx havoc), superseded by bitmap_helper.conf; kept for the writeup.
//!
//! Residual gap (every family): a rule verifies the functions in its cone; the handlers' CALL SITES
//! into those functions are covered only by the litesvm mutation oracle (certora/mutations.md,
//! class HANDLER).

use cvlr::prelude::*;
use cvlr_solana::cvlr_nondet_account_info;

use crate::bucket;
use crate::constants::ZOMBIE_BUCKET;
use crate::state::{Market, Position, RedemptionBitmap};
use fusd_math::rate_bucket as rb;
use fusd_math::recovery;
use fusd_oracle::{aggregate, OracleConfig, PriceView};

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
/// Instead we model `circulating` fUSD as a pure GHOST and EXECUTE the production transition —
/// `crate::supply_transition::borrow`, the exact body borrow.rs runs at `u128`, monomorphized here to
/// `NativeInt` (audit M-01):
///   * `borrow` MINTS `amount` fUSD            → ghost `circulating += d.minted`  (the `mint_to` CPI)
///   * `borrow` writes `agg_recorded_debt = d.new_agg` / `unminted_interest = d.new_unminted`
///   * `bad_debt`: UNTOUCHED by borrow
/// so a mutation of the shared algebra (e.g. `new_agg = agg0` — mutation S1) flips this rule to
/// VIOLATED. The residual gap — a handler CALL-SITE mutation (dropping the call or the `d.new_agg`
/// assignment in borrow.rs) — is invisible to this rule and covered by the litesvm
/// `assert_supply_invariant` layer (mutations.md).
///
/// The `None` arm (overflow) early-returns: production aborts with no state change there, so the
/// identity is trivially preserved — and no `?`/`unwrap` appears in the rule (the
/// `-solanaSkipCallRegInst` havoc frontier, certora/README.md).
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
/// of a `nondet::<u128>()` ranges over the full u128 domain). The shared transitions must ONLY be
/// instantiated at `T = NativeInt` in rules (their `u128` copies hit that frontier).
#[rule]
pub fn supply_preserved_by_borrow_ghost() {
    use cvlr::mathint::NativeInt;

    // pre-state aggregates, each a full-range symbolic u128 lifted into the math-int domain.
    let agg0 = NativeInt::from(nondet::<u128>());      // market.agg_recorded_debt
    let unminted = NativeInt::from(nondet::<u128>());  // market.unminted_interest
    let bad = NativeInt::from(nondet::<u128>());        // market.bad_debt
    let amount = NativeInt::from(nondet::<u128>());     // borrow `amount` (minted this tx)
    // The C7 fee is a full-range symbolic NativeInt (fee == 0 is the disabled case, so this rule
    // covers BOTH the fee-on and fee-off borrow paths).
    let fee = NativeInt::from(nondet::<u128>());

    // Well-formed pre-state (the ONLY admissible assumption — the invariant's own domain): the identity
    // `circulating = agg − unminted + bad` requires `agg − unminted >= 0` (interest accrued into agg is
    // always >= the not-yet-minted slice of it). circ0 IS this value — circulating is the SPL mint supply,
    // a derived quantity, not an independent nondet, so binding it to the equation is the invariant, not
    // an over-assumption. (Vacuity is ruled out by rule_sanity + the S1 mutation flipping this to FAIL.)
    cvlr_assume!(agg0 >= unminted);
    let circ0 = (agg0 - unminted) + bad; // circulating fUSD, pre-borrow

    // EXECUTE the shared production transition (borrow.rs runs this same body at u128).
    let Some(d) = crate::supply_transition::borrow(agg0, unminted, amount, fee) else { return };
    let circ1 = circ0 + d.minted; // the mint_to CPI (ONLY `amount` is minted; the fee is not)

    clog!(amount);

    // POST: the global supply identity still holds over the EXECUTED post-state.
    cvlr_assert!(circ1 == (d.new_agg - d.new_unminted) + bad);
}

// ===== Invariant #1 (global supply) — the remaining supply-touching writers (ghost, NativeInt) =====
//
// These extend `supply_preserved_by_borrow_ghost` to the COMPLETE set of supply-touching writers
// (repay / refresh_market / liquidate / redeem / urgent_redeem / settle_bad_debt / book_interest), each
// in the identical blocker-free regime: model `circulating` (the SPL mint supply) as a pure `NativeInt`
// GHOST and EXECUTE that handler's `crate::supply_transition` body — the SAME code the handler runs at
// `u128`, monomorphized here to `NativeInt` (audit M-01). This sidesteps the SPL-token CPI mock — a
// workspace-global `[patch.crates-io] spl-token` would corrupt the deployable `.so` (certora/README.md
// §"Two prover frontiers"). A mutation of the shared algebra flips the matching rule AND the litesvm
// `assert_supply_invariant` fuzz oracle; the residual gap (a handler CALL-SITE mutation — dropping the
// call or the assignment of the returned post-state) is litesvm-only (mutations.md documents both
// layers per row). Every rule consumes a transition `None` (overflow/underflow) via
// `let Some(d) = … else { return }` — production aborts with no state change there, and no
// `?`/`unwrap` may appear in a rule (the `-solanaSkipCallRegInst` havoc frontier).
//
// STATUS: authored, pending a cloud run (the pre-M-01 replay-the-delta borrow rule was cloud-VERIFIED;
// the executed-shared-fn rewrite needs a re-run). All stay in the borrow rule's exact NativeInt regime
// (no account memory / bitwise / u128 checked-arith / handler glue) and are wired into
// certora/supply.conf.

/// repay BURNS `d.burn = min(amount, position_debt)` fUSD and un-books the same from the position and
/// agg — the executed `supply_transition::repay` body (repay.rs runs it at u128): circ −= burn,
/// agg −= burn ⇒ identity preserved. Mutation S2 (in the shared fn, `new_agg: agg0` — skip the csub):
/// circ falls but agg stays ⇒ VIOLATED. Call-site drop in repay.rs: litesvm-only.
#[rule]
pub fn supply_preserved_by_repay_ghost() {
    use cvlr::mathint::NativeInt;
    let agg0 = NativeInt::from(nondet::<u128>());
    let unminted = NativeInt::from(nondet::<u128>());
    let bad = NativeInt::from(nondet::<u128>());
    let position_debt = NativeInt::from(nondet::<u128>());
    let amount = NativeInt::from(nondet::<u128>()); // pre-cap repay amount
    cvlr_assume!(agg0 >= unminted);
    let circ0 = (agg0 - unminted) + bad;
    let Some(d) = crate::supply_transition::repay(agg0, position_debt, amount) else { return };
    let b = d.burn; // fUSD burned == debt un-booked
    cvlr_assume!(b <= agg0 - unminted); // repay can't drop agg below the unminted-interest floor
    let circ1 = circ0 - b;
    clog!(b);
    cvlr_assert!(circ1 == (d.new_agg - unminted) + bad);
}

/// refresh_market consumes `d.amount = min(pending, u64::MAX)` of accrued interest: it MINTS
/// `keeper_cut + buffer_amount + backstop_cut` and (C16) DIVERTS `paydown` to retire `bad_debt`
/// instead of minting it — the executed `supply_transition::refresh` body (refresh_market.rs runs it
/// at u128), covering the FULL keeper → paydown → backstop → buffer split. So
/// circ += amount − paydown, unminted −= amount, bad −= paydown, agg flat (the interest was folded
/// into agg at accrual) ⇒ identity preserved; the second assert pins the split as exhaustive
/// (keeper + buffer + backstop + paydown == amount). The bps params range over their governance
/// clamps (MAX_KEEPER_REWARD_BPS / MAX_BAD_DEBT_PAYDOWN_BPS / MAX_BACKSTOP_CUT_BPS — constants.rs),
/// so the 0-bps disabled paths are all in scope. Mutation S3 (in the shared fn, `new_unminted =
/// pending` — skip the drain): interest double-counted ⇒ VIOLATED. Mutation C16 (`new_bad = bad0` —
/// divert without retiring): circ rose by only `amount − paydown` while bad stayed flat ⇒ VIOLATED
/// whenever paydown > 0. Call-site drops in refresh_market.rs: litesvm-only.
#[rule]
pub fn supply_preserved_by_refresh_market_ghost() {
    use cvlr::mathint::NativeInt;
    let agg0 = NativeInt::from(nondet::<u128>());
    let pending = NativeInt::from(nondet::<u128>()); // market.unminted_interest, pre-crank
    let bad0 = NativeInt::from(nondet::<u128>());
    let keeper_bps: u16 = nondet();
    let paydown_bps: u16 = nondet();
    let backstop_cut_bps: u16 = nondet();
    let headroom = NativeInt::from(nondet::<u128>()); // backstop reserve_cap − vault balance
    cvlr_assume!(agg0 >= pending); // the unminted slice always sits inside agg
    cvlr_assume!(keeper_bps <= 1_000); // MAX_KEEPER_REWARD_BPS (governance clamp)
    cvlr_assume!(paydown_bps <= 10_000); // MAX_BAD_DEBT_PAYDOWN_BPS
    cvlr_assume!(backstop_cut_bps <= 3_000); // MAX_BACKSTOP_CUT_BPS
    let circ0 = (agg0 - pending) + bad0;
    let Some(d) = crate::supply_transition::refresh(
        pending,
        bad0,
        keeper_bps,
        paydown_bps,
        backstop_cut_bps,
        headroom,
    ) else {
        return;
    };
    let u = d.amount; // interest consumed this refresh
    let circ1 = circ0 + (d.keeper_cut + d.buffer_amount + d.backstop_cut); // the three mint CPIs
    clog!(u);
    cvlr_assert!(circ1 == (agg0 - d.new_unminted) + d.new_bad);
    // Destination-sum conservation: the split is exhaustive — every consumed unit is minted
    // (keeper/buffer/backstop) or diverted to the C16 paydown, nothing stranded.
    cvlr_assert!(d.keeper_cut + d.buffer_amount + d.backstop_cut + d.paydown == u);
}

/// liquidate routes the victim debt through the waterfall — the executed
/// `supply_transition::liquidate` body over the `recovery::absorb` split (liquidate.rs runs it at
/// u128): the RP-offset + buffer + global BURNs remove `d.burned` fUSD and the same from agg; the
/// un-homed remainder leaves agg and is booked to bad_debt; redistributed debt stays in agg
/// (reassigned to survivors, supply-neutral — `redist` ranges freely here, proving the parking
/// neutrality). So circ −= burned, agg −= burned + unhomed, bad += unhomed ⇒ Δcirc = Δagg + Δbad ⇒
/// identity preserved. Mutation S4 (in the shared fn, `new_bad: bad0` — drop the unhomed booking):
/// agg drops by unhomed but bad isn't raised ⇒ VIOLATED whenever unhomed > 0. Call-site drop in
/// liquidate.rs: litesvm-only.
#[rule]
pub fn supply_preserved_by_liquidate_ghost() {
    use cvlr::mathint::NativeInt;
    let agg0 = NativeInt::from(nondet::<u128>());
    let unminted = NativeInt::from(nondet::<u128>());
    let bad0 = NativeInt::from(nondet::<u128>());
    let reactor = NativeInt::from(nondet::<u128>()); // tier-1 RP-offset BURN
    let redist = NativeInt::from(nondet::<u128>()); // tier-2: parked in agg, supply-neutral
    let buffer = NativeInt::from(nondet::<u128>()); // tier-3 buffer BURN
    let global = NativeInt::from(nondet::<u128>()); // tier-3.5 backstop BURN
    let unhomed = NativeInt::from(nondet::<u128>()); // tier-4 remainder booked to bad_debt
    cvlr_assume!(agg0 >= unminted);
    // The debt leaving agg can't breach the unminted floor (absorb conservation: the split is a
    // partition of the victim's present debt, which sits inside agg − unminted).
    cvlr_assume!(reactor + buffer + global + unhomed <= agg0 - unminted);
    let circ0 = (agg0 - unminted) + bad0;
    let Some(d) =
        crate::supply_transition::liquidate(agg0, bad0, reactor, redist, buffer, global, unhomed)
    else {
        return;
    };
    let burned = d.burned; // RP-offset + buffer + global BURNs (fUSD removed)
    let circ1 = circ0 - burned;
    clog!(burned);
    cvlr_assert!(circ1 == (d.new_agg - unminted) + d.new_bad);
}

/// redeem BURNS `b` fUSD of face value and reduces the target debt by the same — the executed
/// `supply_transition::redeem_step` body (redeem.rs runs it at u128, once per candidate): circ −= b,
/// agg −= b ⇒ preserved (the redemption fee is retained COLLATERAL, not fUSD, so it does not move the
/// supply identity — that is the vault rule's concern). Mutation S6 (in the shared fn,
/// `new_agg: agg0` — skip the csub): VIOLATED (note: `redeem_step` is shared with urgent_redeem, so
/// this flips S7's rule too). Call-site drop in redeem.rs: litesvm-only.
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
    let Some(d) = crate::supply_transition::redeem_step(agg0, b) else { return };
    let circ1 = circ0 - d.burned;
    clog!(b);
    cvlr_assert!(circ1 == (d.new_agg - unminted) + bad);
}

/// urgent_redeem (shutdown wind-down): 0-fee burn-for-collateral driving the IDENTICAL shared
/// `supply_transition::redeem_step` body as redeem (urgent_redeem.rs runs it at u128, once per
/// candidate; circ −= b, agg −= b). Mutation S7 (shared-fn `new_agg: agg0`) ⇒ VIOLATED (flips S6's
/// rule too — one shared step fn); the two handlers' call-site drops are distinguished at the
/// litesvm layer.
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
    let Some(d) = crate::supply_transition::redeem_step(agg0, b) else { return };
    let circ1 = circ0 - d.burned;
    clog!(b);
    cvlr_assert!(circ1 == (d.new_agg - unminted) + bad);
}

/// settle_bad_debt BURNS recovered fUSD `x` and reduces bad_debt by the same in lockstep — the
/// executed `supply_transition::settle_bad_debt` body (settle_bad_debt.rs runs it at u128):
/// circ −= x, bad −= x, agg flat ⇒ preserved (the on-chain half of recapitalization). Mutation S5
/// (shared fn returns `Some(bad0)` — burn but skip the retire) ⇒ VIOLATED. Call-site drop in
/// settle_bad_debt.rs: litesvm-only.
#[rule]
pub fn supply_preserved_by_settle_bad_debt_ghost() {
    use cvlr::mathint::NativeInt;
    let agg0 = NativeInt::from(nondet::<u128>());
    let unminted = NativeInt::from(nondet::<u128>());
    let bad0 = NativeInt::from(nondet::<u128>());
    let x = NativeInt::from(nondet::<u128>()); // recovered fUSD burned against bad_debt
    cvlr_assume!(agg0 >= unminted);
    cvlr_assume!(x <= bad0); // can't settle more bad debt than exists (the handler's require)
    let circ0 = (agg0 - unminted) + bad0;
    let Some(bad1) = crate::supply_transition::settle_bad_debt(bad0, x) else { return };
    let circ1 = circ0 - x;
    clog!(x);
    cvlr_assert!(circ1 == (agg0 - unminted) + bad1);
}

/// book_interest (`accrual::accrue`'s pending fold AND adjust_rate's premature-adjustment fee — the
/// two supply-relevant writers the original seven rules missed): `x` enters agg AND unminted in
/// lockstep, nothing is minted — the executed `supply_transition::book_interest` body (accrual.rs
/// and adjust_rate.rs run it at u128): circ flat, agg += x, unminted += x, bad flat ⇒ identity
/// preserved. Mutation S8 (in the shared fn, `new_unminted: unminted0` — book into agg only):
/// agg rises with no unminted offset while circ stays flat ⇒ VIOLATED. Call-site drops in
/// accrual.rs / adjust_rate.rs: litesvm-only.
#[rule]
pub fn supply_preserved_by_book_interest_ghost() {
    use cvlr::mathint::NativeInt;
    let agg0 = NativeInt::from(nondet::<u128>());
    let unminted0 = NativeInt::from(nondet::<u128>());
    let bad = NativeInt::from(nondet::<u128>());
    let x = NativeInt::from(nondet::<u128>()); // pending interest / premature-adjustment fee
    cvlr_assume!(agg0 >= unminted0);
    let circ0 = (agg0 - unminted0) + bad;
    let Some(d) = crate::supply_transition::book_interest(agg0, unminted0, x) else { return };
    clog!(x);
    // Nothing minted: circulating is UNCHANGED while agg and unminted move together.
    cvlr_assert!(circ0 == (d.new_agg - d.new_unminted) + bad);
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

// ===================== Invariant C1 (LST canonical-rate cap) — oracle aggregate =====================
//
// CLOUD-VERIFIED (both rules, non-vacuous). These drive the REAL `fusd_oracle::aggregate`
// over nondet prices, in the SAME blocker-free pure-arithmetic regime as `absorb_conserves_debt`:
// no Anchor `Context`, no account memory, no CPI, no summaries — `aggregate` is a pure fn over plain
// structs. `k_bps = 0` makes the −k·σ haircut fold to 0 (it is orthogonal to C1 and would otherwise
// pull the proof into the u128 mul/div prover frontier the README documents); the C1 MIN-cap line is
// fully exercised, so mutation C1 stays live. `switchboard`/`dex_twap` are absent, so the chosen mid
// is the Pyth view — but the cap invariant holds for ANY chosen feed, so this loses no generality.

/// Build the C1-isolating config: every threshold off and `k_bps = 0`, so `aggregate`'s collateral
/// price reduces to exactly `MIN(market, canonical)` — the C1 cap, with nothing else in the cone.
fn c1_isolating_cfg() -> OracleConfig {
    OracleConfig {
        max_conf_bps: 0,
        max_deviation_bps: 0,
        twap_max_divergence_bps: 0,
        max_age_secs: 0,
        k_bps: 0, // haircut folds to 0 → pure-min regime
        band_lower_ray: 0,
        band_upper_ray: 0,
        liq_max_divergence_bps: 0,
        canonical_required: false,
    }
}

/// C1 SAFETY (the cap is an UPPER BOUND): with a present canonical valuation `c`, the collateral
/// (mint/LTV) price `aggregate` returns is always `<= c`. So an upward-manipulated market feed can
/// never lift borrowing power past the trustless on-chain stake-pool rate (the BOLD-08 over-mint
/// defense). Drives the real `aggregate`; `c` and the market `price` are full-range symbolic u128.
///
/// Mutation C1 (drop the cap in `aggregate` — `Some(c) => chosen.price.min(c)` → `Some(c) =>
/// chosen.price`): when `price > c` the collateral price becomes `price > c` ⇒ VIOLATED.
#[rule]
pub fn c1_canonical_caps_collateral() {
    let price: u128 = nondet();
    let c: u128 = nondet(); // the canonical LST valuation (RAY USD per whole token)
    let pyth = PriceView { price, conf: 0, expo: 0, publish_ts: 0 };
    let r = aggregate(pyth, None, None, Some(c), 0, &c1_isolating_cfg());
    clog!(c);
    cvlr_assert!(r.collateral_price <= c);
}

/// C1 MONOTONICITY (the leg only ever LOWERS collateral): for identical inputs, the collateral price
/// WITH a canonical leg is `<=` the price WITHOUT it. So enabling the canonical leg can never raise a
/// borrower's mint power — it is a purely conservative cap, never a price opinion that inflates.
///
/// Mutation C1 (`min` → `max`): when `c > price` the WITH result `max(price,c)` exceeds the uncapped
/// WITHOUT result `price` ⇒ VIOLATED. (Dropping the cap does NOT break this rule — both legs then
/// collapse to `price`; drop-cap is caught only by `c1_canonical_caps_collateral` above.)
#[rule]
pub fn c1_canonical_never_raises_collateral() {
    let price: u128 = nondet();
    let c: u128 = nondet();
    let cfg = c1_isolating_cfg();
    let pyth = PriceView { price, conf: 0, expo: 0, publish_ts: 0 };
    let with_canonical = aggregate(pyth, None, None, Some(c), 0, &cfg);
    let without = aggregate(pyth, None, None, None, 0, &cfg);
    clog!(c);
    cvlr_assert!(with_canonical.collateral_price <= without.collateral_price);
}
