//! Liquidation **loss-absorption waterfall** — the terminal-recovery accounting (fusion-docs.md).
//!
//! A liquidation's present debt is extinguished across tiers in **strict order**:
//!   1. **Reactor Pool** offset (burn pool deposits, up to the pool's size),
//!   2. **redistribution** to the other active positions (Liquity tier 2; all-or-nothing on whether
//!      any recipient exists, since a single reward-per-unit bump spreads the whole remainder),
//!   3. the per-market **local insurance buffer** (this market's FIRST-LOSS capital, fail-closed haircut),
//!      3.5 the **global backstop reserve** (shared SECOND-LOSS capital, fail-closed haircut — the caller
//!      passes the already-CAPPED amount available to THIS draw: `min(reserve balance, per-market draw
//!      cap)`, so this tier needs no knowledge of the cap logic),
//!   4. **un-homed** — the residual no tier could extinguish *now*, which is the terminal signal to
//!      shut the market down (0-fee `urgent_redeem` wind-down).
//!
//! This is the fix for the old `NoRedistributionRecipients` hard-revert (`liquidate.rs`): with the
//! buffer tiers and the un-homed terminal signal, the outputs **always account for the full `debt`**,
//! so a liquidation can never get stuck and strand un-liquidatable bad debt. The single conservation
//! identity `reactor + redist + buffer + global + unhomed == debt` is the load-bearing invariant (proven in
//! Kani — `recovery.rs` harnesses); `liquidate.rs` calls [`absorb`] for the split and treats a non-zero
//! `unhomed` as the shutdown trigger.
//!
//! Both buffers are **fUSD-denominated** (each burns its own fUSD to extinguish debt and takes the
//! matching seized collateral); their balances are protocol-owned and do NOT count toward any
//! position/market backing until consumed via this balanced flow. The LOCAL buffer is first-loss (each
//! market eats its own normal losses); the GLOBAL reserve is bounded second-loss capital that catches
//! only the narrow tail past a market's drained local buffer, up to a per-market draw cap (computed by
//! the caller, never here). Funding is realized-fees-only (no treasury seed); either buffer may run
//! empty, in which case its tier contributes 0 and the waterfall falls toward the terminal `unhomed` →
//! shutdown (safe because launch posture bounds exposure: small ceilings, RP-coverage requirements,
//! SCR shutdown, the net-issuance limiter, conservative collateral params).

/// How a liquidation's present `debt` is extinguished across the tiers. The five fields sum to the
/// input `debt` **exactly** (conservation), in the strict order RP → redistribution → local buffer →
/// global backstop → un-homed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Absorption {
    /// Offset by the Reactor Pool (burned against pool deposits).
    pub reactor: u128,
    /// Redistributed to the other active positions (Liquity tier 2).
    pub redist: u128,
    /// Absorbed by the per-market LOCAL insurance buffer (first-loss capital).
    pub buffer: u128,
    /// Absorbed by the GLOBAL backstop reserve (second-loss capital; the caller pre-caps the amount
    /// available to this draw). Tier 3.5 — between the local buffer and un-homed.
    pub global: u128,
    /// Residual no tier could extinguish now — non-zero ⇒ trigger the per-market shutdown wind-down.
    pub unhomed: u128,
}

/// Split `debt` across the loss-absorption tiers in STRICT order (RP → redistribution → local buffer →
/// global backstop → un-homed). Each tier takes up to its capacity; the buffers haircut **fail-closed**
/// (`min(remaining, balance)`); redistribution is **all-or-nothing** on `has_redist_recipients` (a single
/// reward-per-unit bump spreads the entire remainder, so it absorbs everything left or nothing).
///
/// `global_available` is the amount the GLOBAL backstop may contribute to THIS liquidation — the caller
/// (`liquidate.rs`) passes `min(reserve fUSD balance, the market's remaining hybrid draw cap)`, so all
/// cap/eligibility logic stays out of this pure function; here it is just another fail-closed haircut
/// AFTER the local buffer (second-loss) and BEFORE un-homed.
///
/// The result ALWAYS accounts for the full `debt` (`reactor + redist + buffer + global + unhomed == debt`),
/// so liquidation can never revert/stall: a non-zero `unhomed` is the terminal signal to shut the market
/// down, and it occurs EXACTLY when no tier can cover (RP short of `debt`, no redistribution recipient,
/// and BOTH buffers drained/capped).
#[inline]
pub fn absorb(
    debt: u128,
    reactor_capacity: u128,
    has_redist_recipients: bool,
    buffer_balance: u128,
    global_available: u128,
) -> Absorption {
    let reactor = debt.min(reactor_capacity); // tier 1: up to the pool size
    let rem = debt - reactor; // reactor <= debt, no underflow
    let redist = if has_redist_recipients { rem } else { 0 }; // tier 2: all-or-nothing
    let rem = rem - redist; // redist in {0, rem}, no underflow
    let buffer = rem.min(buffer_balance); // tier 3: local buffer, fail-closed haircut
    let rem = rem - buffer; // buffer <= rem, no underflow
    let global = rem.min(global_available); // tier 3.5: global backstop, fail-closed (pre-capped)
    let unhomed = rem - global; // global <= rem, no underflow
    Absorption { reactor, redist, buffer, global, unhomed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn reactor_fully_covers() {
        // Pool larger than the debt: the RP absorbs all of it.
        assert_eq!(absorb(100, 250, true, 999, 999), Absorption { reactor: 100, redist: 0, buffer: 0, global: 0, unhomed: 0 });
        assert_eq!(absorb(100, 100, false, 0, 0), Absorption { reactor: 100, redist: 0, buffer: 0, global: 0, unhomed: 0 });
    }

    #[test]
    fn redistribution_takes_the_remainder() {
        // RP covers 40, the other 60 redistributes (recipients exist); buffers untouched.
        assert_eq!(absorb(100, 40, true, 999, 999), Absorption { reactor: 40, redist: 60, buffer: 0, global: 0, unhomed: 0 });
    }

    #[test]
    fn local_buffer_covers_when_no_recipients() {
        // RP covers 40, no recipients, local buffer (>= 60) absorbs the rest; global untouched.
        assert_eq!(absorb(100, 40, false, 60, 999), Absorption { reactor: 40, redist: 0, buffer: 60, global: 0, unhomed: 0 });
        assert_eq!(absorb(100, 40, false, 1000, 0), Absorption { reactor: 40, redist: 0, buffer: 60, global: 0, unhomed: 0 });
    }

    #[test]
    fn global_covers_after_local_buffer() {
        // RP 40, no recipients, local buffer only 25 -> global (>= 35) catches the rest BEFORE un-homed.
        assert_eq!(absorb(100, 40, false, 25, 35), Absorption { reactor: 40, redist: 0, buffer: 25, global: 35, unhomed: 0 });
        assert_eq!(absorb(100, 40, false, 25, 1000), Absorption { reactor: 40, redist: 0, buffer: 25, global: 35, unhomed: 0 });
        // Local buffer empty: the global tier carries the whole post-RP remainder.
        assert_eq!(absorb(100, 40, false, 0, 60), Absorption { reactor: 40, redist: 0, buffer: 0, global: 60, unhomed: 0 });
    }

    #[test]
    fn global_haircuts_fail_closed_then_unhomed() {
        // RP 40, no recipients, local buffer 25, global only 20 -> 25 + 20 absorbed, 15 un-homed.
        assert_eq!(absorb(100, 40, false, 25, 20), Absorption { reactor: 40, redist: 0, buffer: 25, global: 20, unhomed: 15 });
        // Both buffers empty: the whole post-RP remainder is un-homed (the 4-tier behavior, recovered).
        assert_eq!(absorb(100, 40, false, 0, 0), Absorption { reactor: 40, redist: 0, buffer: 0, global: 0, unhomed: 60 });
    }

    #[test]
    fn global_available_zero_is_byte_identical_to_four_tier() {
        // With the backstop off/capped to 0, every split matches the pre-backstop 4-tier waterfall.
        assert_eq!(absorb(100, 40, false, 25, 0), Absorption { reactor: 40, redist: 0, buffer: 25, global: 0, unhomed: 35 });
        assert_eq!(absorb(100, 40, true, 25, 0), Absorption { reactor: 40, redist: 60, buffer: 0, global: 0, unhomed: 0 });
    }

    #[test]
    fn conservation_holds_everywhere() {
        for &debt in &[0u128, 1, 50, 100] {
            for &cap in &[0u128, 30, 100, 200] {
                for &rec in &[true, false] {
                    for &bal in &[0u128, 25, 1000] {
                        for &glob in &[0u128, 20, 1000] {
                            let a = absorb(debt, cap, rec, bal, glob);
                            assert_eq!(a.reactor + a.redist + a.buffer + a.global + a.unhomed, debt, "conservation");
                            assert!(a.reactor <= cap);
                            assert!(a.buffer <= bal);
                            assert!(a.global <= glob);
                        }
                    }
                }
            }
        }
    }

    // --- proptest fuzz (B8): the loss-absorption waterfall over FULLY WIDE random u128 inputs,
    // asserting the SAME properties the Kani harnesses prove — conservation, strict tier order,
    // fail-closed haircuts, and the terminal `unhomed` firing exactly when no tier can cover. Pure
    // min/sub/add, so no precondition is needed: every u128 input is in-contract.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        // CONSERVATION: the five tiers sum to debt EXACTLY (the load-bearing identity), and each tier
        // is itself bounded by debt.
        #[test]
        fn absorb_conserves_debt(
            debt in any::<u128>(),
            reactor_capacity in any::<u128>(),
            has_redist in any::<bool>(),
            buffer_balance in any::<u128>(),
            global_available in any::<u128>(),
        ) {
            let a = absorb(debt, reactor_capacity, has_redist, buffer_balance, global_available);
            prop_assert_eq!(a.reactor + a.redist + a.buffer + a.global + a.unhomed, debt, "tiers must sum to debt");
            prop_assert!(a.reactor <= debt && a.redist <= debt && a.buffer <= debt && a.global <= debt && a.unhomed <= debt);
        }

        // FAIL-CLOSED + STRICT ORDER: no tier takes more than its capacity; redistribution is
        // all-or-nothing on the post-RP remainder; the local buffer is used only AFTER redistribution
        // can't; the global tier only AFTER the local buffer; and `unhomed > 0` happens EXACTLY when no
        // tier could cover (RP drained, no recipients, BOTH buffers drained, and the total genuinely short).
        #[test]
        fn absorb_fail_closed_and_ordered(
            debt in any::<u128>(),
            reactor_capacity in any::<u128>(),
            has_redist in any::<bool>(),
            buffer_balance in any::<u128>(),
            global_available in any::<u128>(),
        ) {
            let a = absorb(debt, reactor_capacity, has_redist, buffer_balance, global_available);
            prop_assert!(a.reactor <= reactor_capacity, "RP over its capacity");
            prop_assert!(a.buffer <= buffer_balance, "local buffer over its balance (not fail-closed)");
            prop_assert!(a.global <= global_available, "global over its available (not fail-closed)");
            prop_assert!(a.redist == 0 || a.redist == debt - a.reactor, "redist not all-or-nothing");
            if a.buffer > 0 {
                prop_assert!(!has_redist, "local buffer used before redistribution (order violated)");
            }
            if a.global > 0 {
                prop_assert!(!has_redist, "global used before redistribution (order violated)");
                prop_assert_eq!(a.buffer, buffer_balance, "global used before the local buffer was drained");
            }
            if a.unhomed > 0 {
                prop_assert!(!has_redist, "un-homed with a redistribution recipient available");
                prop_assert_eq!(a.reactor, reactor_capacity, "un-homed without draining the RP");
                prop_assert_eq!(a.buffer, buffer_balance, "un-homed without draining the local buffer");
                prop_assert_eq!(a.global, global_available, "un-homed without draining the global tier");
                // RP + both buffers genuinely cannot cover (guard the additive overflow with checked_add).
                prop_assert!(
                    reactor_capacity
                        .checked_add(buffer_balance)
                        .and_then(|t| t.checked_add(global_available))
                        .is_some_and(|t| t < debt),
                    "un-homed but RP + buffers could have covered"
                );
            }
        }

        // INDEPENDENT REFERENCE: recompute the waterfall with a naive saturating model and require an
        // exact match (NOT a re-run of the production body — a separate min/sub formulation).
        #[test]
        fn absorb_matches_naive_reference(
            debt in any::<u128>(),
            reactor_capacity in any::<u128>(),
            has_redist in any::<bool>(),
            buffer_balance in any::<u128>(),
            global_available in any::<u128>(),
        ) {
            let a = absorb(debt, reactor_capacity, has_redist, buffer_balance, global_available);

            let ref_sp = core::cmp::min(debt, reactor_capacity);
            let mut left = debt - ref_sp;
            let ref_redist = if has_redist { left } else { 0 };
            left -= ref_redist;
            let ref_buffer = core::cmp::min(left, buffer_balance);
            left -= ref_buffer;
            let ref_global = core::cmp::min(left, global_available);
            let ref_unhomed = left - ref_global;

            prop_assert_eq!(a.reactor, ref_sp);
            prop_assert_eq!(a.redist, ref_redist);
            prop_assert_eq!(a.buffer, ref_buffer);
            prop_assert_eq!(a.global, ref_global);
            prop_assert_eq!(a.unhomed, ref_unhomed);
        }
    }
}
