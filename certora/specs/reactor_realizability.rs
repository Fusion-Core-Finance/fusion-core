//! Certora/CVLR spec — **Invariant #4: Reactor-Pool P/S realizability** (hardest; do last; timebox).
//!
//! Across any `provide_to_reactor` / `withdraw_from_reactor` / `liquidate`-offset interleaving, a
//! depositor can always realize their compounded deposit + collateral gain — no value created, no gain
//! silently lost across a scale bump or epoch roll. This is the cross-tx version of the B8
//! depositor-snapshot round-trip.
//!
//! Expect this to STRESS the prover: the P/S product-sum is nonlinear (`compounded = deposit · P_now /
//! P_snap`, with `scale`/`epoch` rescaling). Per the handoff: timebox it, and if the solver cannot close
//! the nonlinear form, FALL BACK to documenting the proof obligation + leaning on the Kani/proptest
//! pure-math proofs (`fusd_math::reactor_pool`: solvency `compounded ≤ deposits`, collateral
//! conservation, no-drift over 1000 offsets) PLUS the runnable cross-tx realizability test.
//!
//! Tractable decomposition (try these BEFORE the full nonlinear statement):
//!   R1. **Pool solvency is preserved**: `total_deposits ≥ Σ live compounded` is maintained by each of
//!       provide/withdraw/offset (this is the LINEAR conservation under the running P; the nonlinear
//!       per-depositor value cancels in the sum). This alone proves "no value created" + "withdraw never
//!       underflows `total_deposits`" — the core of realizability.
//!   R2. **A single depositor's compounded is monotone-non-increasing under offset and exact under
//!       provide/withdraw** (round-trip): provide(x) then withdraw(x) with no interleaved offset returns
//!       x (± documented dust); an offset only ever reduces it. Model ONE depositor + a nondet offset.
//!   R3. **No silent loss across an epoch roll**: a full-pool-drain offset (offset == total_deposits)
//!       sets epoch+1, P=DECIMAL_PRECISION, scale=0, total_deposits=0, and the seized collateral remains
//!       claimable (the depositor's gain up to the roll is realized into `pending_collateral_gain`).
//!
//! Runnable counterpart (the mutation oracle): `litesvm_reactor_realizability.rs` — multi-offset,
//! cross-epoch, full-drain, then every depositor claims + withdraws (no lock-out), Σwithdrawn ≤
//! Σprovided, vaults drain to ≤ P/S dust. See `mutations.md` rows R1/R3.
//!
//! STATUS: SPEC-ONLY PSEUDOCODE — not compiled, not run (see supply_invariant.rs header). No Certora
//! rule is implemented for invariant #4: it is DEFERRED from the Certora pass on purpose (the pool's
//! `bnum` U256 division is intractable for the SMT backend — certora/README.md). Coverage stays with
//! Kani + proptest + `litesvm_reactor_realizability` (mutations.md rows R1–R3).
#![cfg(feature = "certora")]
#![allow(unused)]

use cvlr::prelude::*; // confirmed (cvlr 0.6)

/// R1 — pool solvency preserved by `provide_to_reactor` (the easy direction: total_deposits += amount,
/// the provider's compounded += amount at the current snapshot; the inequality is maintained).
#[rule]
pub fn rp_solvency_preserved_by_provide() {
    let mut cx = provide_context_nondet();
    cvlr_assume!(account_valid(&cx));
    cvlr_assume!(pool_solvent(&cx));                  // total_deposits ≥ Σ compounded (the invariant)
    let amount: u64 = nondet();
    let _ = provide_to_reactor_handler(&mut cx, amount);
    cvlr_assert!(pool_solvent(&cx));
}

/// R1 — preserved by `withdraw_from_reactor` (withdraw is capped at the caller's compounded, and
/// total_deposits −= withdrawn; because withdrawn ≤ compounded ≤ total_deposits, the subtraction never
/// underflows AND solvency is preserved — the key "withdraw never bricks" fact).
#[rule]
pub fn rp_solvency_preserved_by_withdraw() {
    let mut cx = withdraw_reactor_context_nondet();
    cvlr_assume!(account_valid(&cx));
    cvlr_assume!(pool_solvent(&cx));
    let amount: u64 = nondet();
    cvlr_assume!(amount > 0);                         // the handler reverts on amount==0 (ZeroAmount) — that
                                                     // is a precondition, not a solvency violation, so exclude
                                                     // it (else `res.is_ok()` is a spurious counterexample).
    let res = withdraw_from_reactor_handler(&mut cx, amount);
    cvlr_assert!(res.is_ok());                        // never reverts on a solvent pool (no underflow)
    cvlr_assert!(pool_solvent(&cx));
}

/// R1 — preserved by an `offset` (liquidation RP burn): total_deposits −= offset, P scales down by
/// (1 − offset/total_deposits); Σ compounded scales by the same factor ⇒ solvency preserved. This is
/// where scale-bump / epoch-roll rescaling lives — the nonlinear stress point.
#[rule]
pub fn rp_solvency_preserved_by_offset() {
    let mut cx = liquidate_context_nondet();
    cvlr_assume!(account_valid(&cx));
    cvlr_assume!(pool_solvent(&cx));
    let _ = liquidate_handler(&mut cx);
    cvlr_assert!(pool_solvent(&cx));
}

/// R2 — single-depositor round-trip: provide(x) then withdraw(MAX) with NO interleaved offset returns
/// exactly x (the snapshot P/S is unchanged, so compounded == deposited). Catches a snapshot/realize
/// regression. CONFIRM the prover can carry the (P, S, scale, epoch) snapshot symbolically.
#[rule]
pub fn rp_provide_withdraw_round_trips_without_offset() {
    let mut cx = single_depositor_context_nondet();
    cvlr_assume!(account_valid(&cx));
    let x: u64 = nondet();
    cvlr_assume!(x > 0);
    let before = depositor_fusd_balance(&cx);
    let _ = provide_to_reactor_handler(&mut cx, x);
    // no offset between
    let _ = withdraw_from_reactor_handler(&mut cx, u64::MAX);
    cvlr_assert!(depositor_fusd_balance(&cx) == before); // round-trip exact (no offset ⇒ no loss)
}

/// R3 — full-drain epoch roll keeps the seized collateral realizable: after an offset == total_deposits,
/// the pool rolls (epoch+1, P reset, scale 0, total_deposits 0) and the depositor's collateral gain up
/// to the roll is realized into `pending_collateral_gain` (claimable), i.e. NOT lost by the reset.
#[rule]
pub fn rp_full_drain_preserves_claimable_collateral() {
    let mut cx = full_drain_context_nondet();         // offset sized == total_deposits (nondet within that)
    cvlr_assume!(account_valid(&cx));
    cvlr_assume!(pool_solvent(&cx));
    let coll_in_vault_pre = rp_coll_vault(&cx);
    let _ = liquidate_handler(&mut cx);               // the full-drain offset
    // epoch rolled, and every native unit of seized collateral that entered the RP vault is still there
    // awaiting claims (claim_reactor_gains realizes it) — none lost to the P reset.
    cvlr_assert!(cx.reactor_pool.total_deposits == 0);
    cvlr_assert!(rp_coll_vault(&cx) >= coll_in_vault_pre); // seizure added, nothing silently removed
}
