//! Certora/CVLR spec — **Invariant #3: liquidation always terminates** (every liquidation routes the
//! full realized debt into exactly one partition of the waterfall; it never reverts mid-waterfall
//! leaving debt unaccounted, and the un-homed branch always trips `shutdown`).
//!
//! The pure conservation `sp + redist + buffer + global + unhomed == debt` (5-tier, with the global
//! backstop) is **Kani-proven** over fully-symbolic u128 in `fusd_math::recovery::absorb`
//! (`recovery.rs`; PROOF_STRENGTH.md). The Certora job here is the INSTRUCTION-level claim that the
//! `liquidate` handler always REACHES `absorb` with a consistent split and commits it — i.e. there is
//! no early-return / overflow / account path that drops realized debt on the floor.
//!
//! Two complementary obligations:
//!   T1. liquidate either reverts cleanly (no state change) OR fully partitions the realized victim
//!       debt: `sp_offset + redistributed + buffer_burn + global_draw + unhomed == realized_debt`.
//!   T2. `unhomed > 0  ⟹  market.shutdown == true` after the tx (un-homed debt is never silently
//!       dropped; it always trips the terminal breaker).
//!
//! Note this is largely SUBSUMED by `supply_invariant::supply_preserved_by_liquidate` (unaccounted debt
//! immediately breaks the supply identity) — but stated separately because it pins the *partition* and
//! the *shutdown coupling* directly, which is the property a reviewer reads off the liquidation engine.
//!
//! Runnable counterpart: every `liquidate` in `litesvm_invariants_fuzz.rs` (supply + vault asserted
//! after each) + the per-tier split asserts in `litesvm_liquidation.rs` / `_redistribution.rs` /
//! `_buffer.rs` / `_backstop.rs`. Mutation oracle: drop the `bad_debt += unhomed` book OR the
//! `shutdown = true` set in the un-homed branch (`mutations.md` rows L1/L2).
//!
//! STATUS: spec scaffold — CVLR API confirmed (cvlr 0.6); remaining `// CONFIRM` = harness glue (see supply_invariant.rs header).
#![cfg(feature = "certora")]
#![allow(unused)]

use cvlr::prelude::*; // confirmed (cvlr 0.6)

/// The waterfall split a `liquidate` tx produced, as observed from state deltas / the emitted
/// `LiquidationEvent` (which carries the full breakdown — events.rs). CONFIRM how the prover reads an
/// emit_cpi! payload, or reconstruct from pre/post deltas of the RP, buffer, backstop, and bad_debt.
struct Waterfall {
    realized_debt: u128, // the victim's debt brought current (interest + parked redistribution folded)
    sp_offset: u128,     // burned from the Reactor Pool
    redistributed: u128, // moved to l_art (parked, non-interest-bearing) for recipients
    buffer_burn: u128,   // burned from the local insurance buffer
    global_draw: u128,   // drawn from the global backstop reserve (tier 3.5)
    unhomed: u128,       // booked into market.bad_debt
}

/// T1 — conservation: the realized debt is fully partitioned (no debt unaccounted).
#[rule]
pub fn liquidate_partitions_the_full_debt() {
    let mut cx = liquidate_context_nondet();
    cvlr_assume!(account_valid(&cx));
    let pre = snapshot(&cx);                          // CONFIRM: pre-state capture
    let res = liquidate_handler(&mut cx);
    if res.is_err() {
        // A clean revert: liquidate is atomic, so no partial state change — termination is trivial.
        cvlr_assert!(state_unchanged(&cx, &pre));     // CONFIRM: Solana reverts are atomic; this should hold by construction
        return;
    }
    let w = waterfall_of(&cx, &pre);
    cvlr_assert!(
        w.sp_offset + w.redistributed + w.buffer_burn + w.global_draw + w.unhomed == w.realized_debt
    );
}

/// T2 — the un-homed branch always trips the terminal breaker (never silently drops debt).
#[rule]
pub fn unhomed_debt_always_trips_shutdown() {
    let mut cx = liquidate_context_nondet();
    cvlr_assume!(account_valid(&cx));
    cvlr_assume!(!cx.market.shutdown);                // start from a live market
    let pre = snapshot(&cx);
    let res = liquidate_handler(&mut cx);
    if res.is_ok() {
        let w = waterfall_of(&cx, &pre);
        // booked bad debt ⟹ the market is now shut down.
        cvlr_assert!(w.unhomed == 0 || cx.market.shutdown);
    }
}

/// T3 (ordering, optional but cheap) — the tiers fill strictly in order: no buffer burn while the RP
/// could have absorbed more, no un-homed while the buffer/backstop had capacity. Mirrors the Kani
/// `absorb_is_fail_closed_and_ordered` at the instruction level.
#[rule]
pub fn liquidate_fills_tiers_in_order() {
    let mut cx = liquidate_context_nondet();
    cvlr_assume!(account_valid(&cx));
    let pre = snapshot(&cx);
    if liquidate_handler(&mut cx).is_ok() {
        let w = waterfall_of(&cx, &pre);
        // unhomed > 0 only when the RP was short AND no redistribution recipient existed AND the buffer
        // and backstop were exhausted. CONFIRM the exact capacity reads against the handler.
        cvlr_assert!(w.unhomed == 0 || (rp_was_short(&pre) && buffer_was_drained(&pre)));
    }
}
