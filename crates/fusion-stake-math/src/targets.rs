//! Epoch target allocation: directed floors, the neutral pool, and the deterministic
//! equal capacity rounds that distribute it.
//!
//! Finalized inputs: `P` = productive lamports (total minus reserve target), `S` =
//! finalized fuSOL supply, `d_v` = eligible directed shares per validator, `D = Σ d_v`.
//! The plan MUST be rejected when `D > S`. Directed targets are per-validator floors,
//! clipped by the lifecycle cap; everything productive that direction does not claim —
//! undirected supply, cap clippings, stale/omitted preferences, rounding residue —
//! forms `neutral_total`, distributed EQUALLY across Active validators with remaining
//! capacity in deterministic capacity rounds.
//!
//! Conservation invariant (proved in tests and Kani): after planning,
//! `Σ final targets + capacity shortfall == P` — every productive lamport is either
//! assigned to a validator or recorded as aggregate-capacity shortfall.
//!
//! The round math is an INCREMENTAL FOLD ([`begin_round`] / [`step`]) so the on-chain
//! plan-neutral crank can process bounded validator slices per transaction with a
//! persisted cursor: the caller walks the round's unsaturated Active set in canonical
//! index order, and the epoch-derived rotation determines which ordinals receive the
//! integer remainder — so grants are independent of batch boundaries.

use crate::lifecycle::ValidatorStatus;

/// High fixed validator capacity (no dynamic account resizing).
pub const MAX_VALIDATORS: u64 = 1_024;
/// Hard upper bound on capacity rounds: each completed non-final round saturates at
/// least one Active validator, so rounds can never exceed the number of validators.
pub const MAX_NEUTRAL_ROUNDS: u64 = MAX_VALIDATORS;

/// Directed target floor for one validator:
/// `min(floor(productive * directed_shares / supply), lifecycle_cap)`.
///
/// The lifecycle cap encodes the status rules (0 for Registered/Draining/Removable, the
/// candidate cap for Candidate, the active cap for Active — see
/// [`crate::lifecycle::lifecycle_cap`]), so ineligible statuses land at 0 here. A zero
/// supply yields 0. The `u128` quotient is saturated into `u64` before the cap clamp;
/// it can only exceed `u64` when `directed_shares > supply`, which the plan-level guard
/// ([`neutral_total`]) rejects, and the cap clamp bounds it regardless.
#[inline]
pub fn directed_target(productive: u64, directed_shares: u64, supply: u64, lifecycle_cap: u64) -> u64 {
    if supply == 0 {
        return 0;
    }
    let raw = u128::from(productive) * u128::from(directed_shares) / u128::from(supply);
    u64::try_from(raw).unwrap_or(u64::MAX).min(lifecycle_cap)
}

/// The neutral pool, with the plan-level guard built in.
///
/// Returns `None` — REJECT THE PLAN — when total directed shares exceed the finalized
/// supply (`D > S`), and (belt-and-braces) if the summed directed targets somehow
/// exceed productive lamports, which is arithmetically impossible when `D <= S`
/// because `Σ floor(P·d_v/S) <= floor(P·D/S) <= P`.
///
/// The result includes the value backing undirected supply, cap-clipped direction,
/// temporarily ineligible / stale / omitted preferences, and integer-rounding residue.
#[inline]
pub fn neutral_total(
    productive: u64,
    sum_directed_targets: u64,
    total_directed_shares: u64,
    supply: u64,
) -> Option<u64> {
    if total_directed_shares > supply {
        return None;
    }
    productive.checked_sub(sum_directed_targets)
}

/// Per-validator NEUTRAL capacity: Active validators expose `cap - current_target`
/// (saturating); every other status — Candidate included — exposes 0. Candidate
/// validators NEVER receive neutral allocation.
#[inline]
pub fn neutral_capacity(status: ValidatorStatus, lifecycle_cap: u64, current_target: u64) -> u64 {
    match status {
        ValidatorStatus::Active => lifecycle_cap.saturating_sub(current_target),
        _ => 0,
    }
}

/// Epoch-derived rotation start index over `n` entries (`epoch % n`; 0 when `n == 0`).
/// Used for integer-remainder assignment in capacity rounds and for deterministic
/// equal-deficit/surplus tie-breaks in the rebalance ordering.
#[inline]
pub fn rotation_start(epoch: u64, n: u64) -> u64 {
    if n == 0 {
        0
    } else {
        epoch % n
    }
}

/// One capacity round's persisted state — small and flat so the on-chain crank can
/// store it in the epoch-state account and resume the fold across transactions.
///
/// Within a round every grant is order-independent: the base for the validator at
/// unsaturated-ordinal `o` is `tranche + 1` iff `o` falls in the rotated remainder
/// window, else `tranche`, then clipped to that validator's remaining capacity. The
/// caller (cursor) walks the round's unsaturated Active set in canonical index order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NeutralRound {
    /// Neutral lamports remaining at round START.
    pub remaining: u64,
    /// Unsaturated Active validators at round start; every one must be stepped
    /// exactly once, in canonical index order.
    pub n_unsaturated: u64,
    /// `remaining / n_unsaturated`.
    pub tranche: u64,
    /// `remaining % n_unsaturated` — the count of `tranche + 1` grants.
    pub remainder: u64,
    /// Rotated start ordinal for the remainder window (`epoch % n_unsaturated`).
    pub start: u64,
    /// Validators stepped so far this round (doubles as the next ordinal).
    pub steps: u64,
    /// Remainder slots consumed so far (`<= remainder`).
    pub remainder_used: u64,
    /// Per-round grant accumulator (`<= remaining`, exactly — see [`step`]).
    pub granted: u64,
    /// Validators that hit their remaining capacity this round (they leave the
    /// unsaturated set for the next round).
    pub saturated: u64,
}

impl NeutralRound {
    /// Every unsaturated validator has been stepped.
    #[inline]
    pub fn is_complete(&self) -> bool {
        self.steps >= self.n_unsaturated
    }

    /// Neutral lamports remaining after this round's grants. Exact: `granted` can
    /// never exceed `remaining` because per-step bases sum to exactly `remaining`
    /// over a full round and each grant is `<=` its base (saturation is unreachable,
    /// kept only to honor the no-unchecked-arithmetic rule).
    #[inline]
    pub fn remaining_after(&self) -> u64 {
        self.remaining.saturating_sub(self.granted)
    }
}

/// Begin a capacity round. `None` when there is nothing to distribute or no
/// unsaturated Active validator remains — in the latter case the caller records
/// `remaining` as the plan's aggregate capacity shortfall.
#[inline]
pub fn begin_round(remaining: u64, n_unsaturated: u64, epoch: u64) -> Option<NeutralRound> {
    if remaining == 0 || n_unsaturated == 0 {
        return None;
    }
    Some(NeutralRound {
        remaining,
        n_unsaturated,
        tranche: remaining / n_unsaturated,
        remainder: remaining % n_unsaturated,
        start: rotation_start(epoch, n_unsaturated),
        steps: 0,
        remainder_used: 0,
        granted: 0,
        saturated: 0,
    })
}

/// Fold one validator into the round and return its grant.
///
/// The caller must step each round-start-unsaturated validator exactly once, in
/// canonical index order, with `validator_remaining_capacity > 0` (its cap minus its
/// current target). The grant is `min(base, capacity)` where `base` is `tranche + 1`
/// for ordinals in the rotated remainder window and `tranche` otherwise. Steps past
/// round completion are inert and return 0, so a crank replay cannot double-grant.
///
/// Invariants maintained (proved in tests and Kani):
/// - Σ grants over a completed round `== remaining - remaining_after()` and `<= remaining`;
/// - a completed round with `remaining_after() > 0` saturated at least one validator
///   (otherwise every grant equaled its base and the round consumed everything);
/// - therefore repeated rounds terminate within the number of validators
///   ([`MAX_NEUTRAL_ROUNDS`]).
pub fn step(state: &mut NeutralRound, validator_remaining_capacity: u64) -> u64 {
    debug_assert!(validator_remaining_capacity > 0, "saturated validators leave the set");
    if state.is_complete() {
        return 0;
    }
    // Rotated position of this ordinal: (ordinal - start) mod n, computed in u128 so the
    // intermediate `ordinal + n` cannot wrap for any u64 `n`.
    let n = u128::from(state.n_unsaturated);
    let rotated = (u128::from(state.steps) + n - u128::from(state.start)) % n;
    let in_window = rotated < u128::from(state.remainder);
    // Exact, never saturates: n == 1 forces remainder == 0 (no +1); n >= 2 bounds
    // tranche <= u64::MAX / 2.
    let base = state.tranche.saturating_add(u64::from(in_window));
    let grant = base.min(validator_remaining_capacity);

    state.steps = state.steps.saturating_add(1);
    if in_window {
        state.remainder_used = state.remainder_used.saturating_add(1);
    }
    // Exact: Σ base over a full round = tranche·n + remainder = remaining <= u64::MAX.
    state.granted = state.granted.saturating_add(grant);
    if grant == validator_remaining_capacity {
        state.saturated = state.saturated.saturating_add(1);
    }
    grant
}

/// Outcome of a full neutral distribution ([`distribute_neutral`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NeutralOutcome {
    /// Capacity rounds executed (`<=` the number of validators with capacity).
    pub rounds: u64,
    /// Neutral lamports no Active capacity could absorb — recorded by the plan as
    /// the aggregate capacity shortfall (they stay in reserve temporarily).
    pub shortfall: u64,
}

/// Reference driver over the [`begin_round`]/[`step`] fold: distribute `neutral`
/// across validators until it is exhausted or no capacity remains. This is the exact
/// loop the on-chain plan-neutral crank runs incrementally; hosts (keeper simulation,
/// tests) run it in one call.
///
/// `remaining_capacity[v]` is each validator's neutral capacity (from
/// [`neutral_capacity`] — 0 for anything non-Active, which keeps it out of every
/// round), decremented in place. `targets[v]` accumulates grants in place; entries
/// stay `<=` the validator's cap, so the saturating add is exact when capacities were
/// derived as `cap - target`. Slices must be the same length.
pub fn distribute_neutral(
    neutral: u64,
    epoch: u64,
    remaining_capacity: &mut [u64],
    targets: &mut [u64],
) -> NeutralOutcome {
    assert_eq!(remaining_capacity.len(), targets.len(), "parallel per-validator slices");
    let mut remaining = neutral;
    let mut rounds = 0u64;
    loop {
        let n_unsaturated = remaining_capacity.iter().filter(|&&c| c > 0).count() as u64;
        let Some(mut round) = begin_round(remaining, n_unsaturated, epoch) else {
            return NeutralOutcome { rounds, shortfall: remaining };
        };
        // One canonical pass: each slot is visited once, so a capacity read here is the
        // round-start value — membership cannot shift mid-round.
        for (capacity, target) in remaining_capacity.iter_mut().zip(targets.iter_mut()) {
            if *capacity == 0 {
                continue;
            }
            let grant = step(&mut round, *capacity);
            *target = target.saturating_add(grant); // exact: target + capacity == cap
            *capacity = capacity.saturating_sub(grant); // exact: grant <= capacity
        }
        debug_assert!(round.is_complete());
        remaining = round.remaining_after();
        rounds = rounds.saturating_add(1); // bounded by the saturation argument
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::lifecycle_cap;
    use crate::reserve::{productive_lamports, reserve_target};
    use proptest::prelude::*;

    // -- directed targets -------------------------------------------------------

    #[test]
    fn directed_target_hand_vectors() {
        // floor(9_800 * 300 / 1_000) = 2_940, under the cap.
        assert_eq!(directed_target(9_800, 300, 1_000, 5_000), 2_940);
        // Cap clips.
        assert_eq!(directed_target(9_800, 300, 1_000, 100), 100);
        // Zero-cap statuses (Registered/Draining/Removable) land at 0.
        assert_eq!(directed_target(9_800, 300, 1_000, 0), 0);
        // Zero supply.
        assert_eq!(directed_target(9_800, 300, 0, 5_000), 0);
        // Floor: 10 * 1 / 3 = 3.33… -> 3.
        assert_eq!(directed_target(10, 1, 3, u64::MAX), 3);
    }

    #[test]
    fn plan_guard_rejects_directed_over_supply() {
        assert_eq!(neutral_total(9_800, 0, 1_001, 1_000), None); // D > S: reject
        assert_eq!(neutral_total(9_800, 3_040, 1_000, 1_000), Some(6_760)); // D == S: fine
        assert_eq!(neutral_total(9_800, 9_801, 0, 1_000), None); // caller bug: sum > P
    }

    #[test]
    fn candidate_never_gets_neutral_capacity() {
        for status in [
            ValidatorStatus::Registered,
            ValidatorStatus::Candidate,
            ValidatorStatus::Draining,
            ValidatorStatus::Removable,
        ] {
            assert_eq!(neutral_capacity(status, u64::MAX, 0), 0, "{status:?}");
        }
        assert_eq!(neutral_capacity(ValidatorStatus::Active, 5_000, 2_940), 2_060);
        // Already at/over cap: no capacity, no underflow.
        assert_eq!(neutral_capacity(ValidatorStatus::Active, 5_000, 5_000), 0);
        assert_eq!(neutral_capacity(ValidatorStatus::Active, 5_000, 6_000), 0);
    }

    // -- capacity rounds --------------------------------------------------------

    #[test]
    fn single_round_exact_split() {
        // 100 across 4 with ample capacity: one round, 25 each.
        let mut caps = [1_000u64; 4];
        let mut targets = [0u64; 4];
        let out = distribute_neutral(100, 0, &mut caps, &mut targets);
        assert_eq!(out, NeutralOutcome { rounds: 1, shortfall: 0 });
        assert_eq!(targets, [25, 25, 25, 25]);
    }

    #[test]
    fn remainder_follows_epoch_rotation() {
        // 10 across 3: tranche 3, remainder 1. Epoch 1 rotates the +1 to index 1.
        let mut caps = [100u64; 3];
        let mut targets = [0u64; 3];
        let out = distribute_neutral(10, 1, &mut caps, &mut targets);
        assert_eq!(out, NeutralOutcome { rounds: 1, shortfall: 0 });
        assert_eq!(targets, [3, 4, 3]);
        // Epoch 0: the +1 lands on index 0.
        let mut caps = [100u64; 3];
        let mut targets = [0u64; 3];
        distribute_neutral(10, 0, &mut caps, &mut targets);
        assert_eq!(targets, [4, 3, 3]);
    }

    #[test]
    fn multi_round_saturation_hand_vector() {
        // 100 across caps [10, 100, 100], epoch 0:
        // round 1: tranche 33, remainder 1 at start 0 -> grants [10 (sat), 33, 33], 24 left;
        // round 2: 2 unsaturated, tranche 12 -> grants [12, 12], 0 left.
        let mut caps = [10u64, 100, 100];
        let mut targets = [0u64; 3];
        let out = distribute_neutral(100, 0, &mut caps, &mut targets);
        assert_eq!(out, NeutralOutcome { rounds: 2, shortfall: 0 });
        assert_eq!(targets, [10, 45, 45]);
        assert_eq!(caps, [0, 55, 55]);
    }

    #[test]
    fn shortfall_recorded_when_capacity_exhausted() {
        let mut caps = [10u64, 20];
        let mut targets = [0u64; 2];
        let out = distribute_neutral(100, 0, &mut caps, &mut targets);
        assert_eq!(out, NeutralOutcome { rounds: 1, shortfall: 70 });
        assert_eq!(targets, [10, 20]); // everything absorbable was absorbed
        // No Active capacity at all: full shortfall, zero rounds.
        let out = distribute_neutral(100, 0, &mut [], &mut []);
        assert_eq!(out, NeutralOutcome { rounds: 0, shortfall: 100 });
        let mut caps = [0u64, 0];
        let mut targets = [0u64; 2];
        let out = distribute_neutral(100, 0, &mut caps, &mut targets);
        assert_eq!(out, NeutralOutcome { rounds: 0, shortfall: 100 });
        assert_eq!(targets, [0, 0]);
    }

    #[test]
    fn zero_neutral_is_a_no_op() {
        let mut caps = [10u64, 20];
        let mut targets = [7u64, 9];
        let out = distribute_neutral(0, 5, &mut caps, &mut targets);
        assert_eq!(out, NeutralOutcome { rounds: 0, shortfall: 0 });
        assert_eq!(targets, [7, 9]);
    }

    #[test]
    fn step_is_inert_after_round_completion() {
        let mut round = begin_round(10, 2, 0).unwrap();
        let a = step(&mut round, 100);
        let b = step(&mut round, 100);
        assert_eq!(a + b, 10);
        assert!(round.is_complete());
        assert_eq!(step(&mut round, 100), 0); // replay-safe: no double grant
        assert_eq!(round.granted, 10);
    }

    #[test]
    fn tiny_remaining_smaller_than_set() {
        // remaining < n: tranche 0, remainder = remaining; only the rotated window gets 1.
        let mut caps = [5u64; 5];
        let mut targets = [0u64; 5];
        let out = distribute_neutral(2, 3, &mut caps, &mut targets);
        assert_eq!(out, NeutralOutcome { rounds: 1, shortfall: 0 });
        // start = 3 % 5 = 3: ordinals 3 and 4 get the two units.
        assert_eq!(targets, [0, 0, 0, 1, 1]);
    }

    // -- full plan pipeline -----------------------------------------------------

    /// End-to-end hand vector exercising: reserve split, directed floors, a cap clip
    /// re-entering neutral, a stale (Draining) direction re-entering neutral,
    /// candidate exclusion from neutral, multi-round saturation, and conservation.
    #[test]
    fn full_plan_hand_vector() {
        use ValidatorStatus::*;
        let total = 10_000u64;
        let reserve = reserve_target(total, 10, 200); // max(10, 200) = 200
        assert_eq!(reserve, 200);
        let productive = productive_lamports(total, reserve);
        assert_eq!(productive, 9_800);

        let supply = 1_000u64;
        let statuses = [Active, Candidate, Active, Draining];
        let shares = [300u64, 200, 0, 100]; // D = 600 <= S = 1_000
        let caps: Vec<u64> =
            statuses.iter().map(|&s| lifecycle_cap(s, total, 100, 5_000)).collect();
        assert_eq!(caps, [5_000, 100, 5_000, 0]);

        let directed: Vec<u64> = (0..4)
            .map(|v| directed_target(productive, shares[v], supply, caps[v]))
            .collect();
        // B's floor(9_800*200/1_000) = 1_960 clips to its 100 cap; D's direction is
        // stale (Draining -> cap 0). Both excesses re-enter the neutral pool.
        assert_eq!(directed, [2_940, 100, 0, 0]);

        let d_total: u64 = shares.iter().sum();
        let sum_directed: u64 = directed.iter().sum();
        let neutral = neutral_total(productive, sum_directed, d_total, supply).unwrap();
        assert_eq!(neutral, 6_760); // includes the 1_860 clip and the 980 stale direction

        let mut capacity: Vec<u64> = (0..4)
            .map(|v| neutral_capacity(statuses[v], caps[v], directed[v]))
            .collect();
        assert_eq!(capacity, [2_060, 0, 5_000, 0]); // Candidate B: zero neutral capacity

        let mut finals = directed.clone();
        let out = distribute_neutral(neutral, 0, &mut capacity, &mut finals);
        // round 1: tranche 3_380 -> A clips at 2_060 (saturates), C takes 3_380;
        // round 2: C alone takes the remaining 1_320.
        assert_eq!(out, NeutralOutcome { rounds: 2, shortfall: 0 });
        assert_eq!(finals, [5_000, 100, 4_700, 0]);
        assert_eq!(finals[0], caps[0]); // A pinned at its cap

        // Conservation: every productive lamport is a target (no shortfall here).
        assert_eq!(finals.iter().sum::<u64>() + out.shortfall, productive);
    }

    // -- properties -------------------------------------------------------------

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        /// Directed targets never exceed the cap, and never exceed productive when
        /// shares are within supply.
        #[test]
        fn directed_at_most_cap_and_productive(
            p in 0u64..=u64::MAX,
            d in 0u64..=u64::MAX,
            s in 0u64..=u64::MAX,
            cap in 0u64..=u64::MAX,
        ) {
            let t = directed_target(p, d, s, cap);
            prop_assert!(t <= cap);
            if d <= s {
                prop_assert!(t <= p);
            }
        }

        /// The plan-level guard: rejected iff D > S (for in-range directed sums).
        #[test]
        fn directed_over_supply_always_rejected(
            p in 0u64..=u64::MAX,
            sum in 0u64..=u64::MAX,
            d in 0u64..=u64::MAX,
            s in 0u64..=u64::MAX,
        ) {
            let r = neutral_total(p, sum, d, s);
            if d > s {
                prop_assert_eq!(r, None);
            } else {
                prop_assert_eq!(r.is_some(), sum <= p);
            }
        }

        /// All-undirected pool: one round, an equal split modulo the rotated
        /// remainder — max spread 1, exactly `remainder` validators get the +1,
        /// placed by the epoch rotation.
        #[test]
        fn all_undirected_equal_split(
            neutral in 0u64..1_000_000_000,
            n in 1usize..48,
            epoch in 0u64..10_000,
        ) {
            let mut caps = vec![u64::MAX; n];
            let mut targets = vec![0u64; n];
            let out = distribute_neutral(neutral, epoch, &mut caps, &mut targets);
            prop_assert_eq!(out.shortfall, 0);
            prop_assert_eq!(out.rounds, u64::from(neutral > 0));
            prop_assert_eq!(targets.iter().sum::<u64>(), neutral);
            let (tranche, rem) = (neutral / n as u64, neutral % n as u64);
            let start = rotation_start(epoch, n as u64);
            for (i, &t) in targets.iter().enumerate() {
                let rotated = (i as u64 + n as u64 - start) % n as u64;
                prop_assert_eq!(t, tranche + u64::from(rotated < rem));
            }
        }

        /// Conservation, termination and saturation over random capacities:
        /// grants + shortfall == neutral; grants never exceed capacity; rounds are
        /// bounded by the initially-unsaturated count (the MAX_NEUTRAL_ROUNDS bound).
        #[test]
        fn conservation_and_round_bound(
            neutral in 0u64..=2_000_000,
            caps in proptest::collection::vec(0u64..=100_000, 0..40),
            epoch in 0u64..10_000,
        ) {
            let mut capacity = caps.clone();
            let mut targets = vec![0u64; caps.len()];
            let unsaturated0 = caps.iter().filter(|&&c| c > 0).count() as u64;
            let out = distribute_neutral(neutral, epoch, &mut capacity, &mut targets);
            // Conservation is EXACT.
            prop_assert_eq!(targets.iter().sum::<u64>() + out.shortfall, neutral);
            // Grants never exceed per-validator capacity.
            for (t, c) in targets.iter().zip(caps.iter()) {
                prop_assert!(t <= c);
            }
            // Positive shortfall only with every capacity exhausted.
            if out.shortfall > 0 {
                prop_assert!(capacity.iter().all(|&c| c == 0));
            }
            // Termination within the number of participating validators.
            prop_assert!(out.rounds <= unsaturated0.max(u64::from(neutral > 0)));
            prop_assert!(out.rounds <= MAX_NEUTRAL_ROUNDS);
        }

        /// Fold-level invariants, driving begin_round/step by hand: per-round grants
        /// sum to exactly (old_remaining - new_remaining), and every completed
        /// NON-final round saturates at least one validator.
        #[test]
        fn rounds_conserve_and_saturate(
            neutral in 1u64..=1_000_000,
            caps in proptest::collection::vec(1u64..=50_000, 1..24),
            epoch in 0u64..10_000,
        ) {
            let mut capacity = caps.clone();
            let mut remaining = neutral;
            let mut rounds = 0u64;
            loop {
                let n = capacity.iter().filter(|&&c| c > 0).count() as u64;
                let Some(mut round) = begin_round(remaining, n, epoch) else { break };
                let mut grant_sum = 0u64;
                for c in capacity.iter_mut().filter(|c| **c > 0) {
                    let g = step(&mut round, *c);
                    prop_assert!(g <= *c);
                    *c -= g;
                    grant_sum += g;
                }
                prop_assert!(round.is_complete());
                prop_assert_eq!(grant_sum, round.granted);
                prop_assert_eq!(round.remainder_used, round.remainder);
                let after = round.remaining_after();
                // Grants sum to exactly the amount the round consumed.
                prop_assert_eq!(grant_sum, remaining - after);
                // A completed non-final round saturated someone (else it consumed all).
                if after > 0 {
                    prop_assert!(round.saturated >= 1);
                }
                remaining = after;
                rounds += 1;
                prop_assert!(rounds <= caps.len() as u64);
            }
        }

        /// Plan-level conservation over a fully random validator set: directed floors
        /// plus neutral rounds plus recorded shortfall exactly cover productive, with
        /// candidates receiving directed stake only.
        #[test]
        fn random_plan_conserves_productive(
            total in 0u64..=1_000_000_000_000,
            reserve_min in 0u64..=1_000_000_000,
            status_bytes in proptest::collection::vec(0u8..5, 1..24),
            share_units in proptest::collection::vec(0u64..=1_000, 1..24),
            slack in 0u64..=100_000,
            candidate_bps in 0u64..=500,
            active_bps in 0u64..=10_000,
            epoch in 0u64..10_000,
        ) {
            let n = status_bytes.len().min(share_units.len());
            let statuses: Vec<ValidatorStatus> =
                status_bytes[..n].iter().map(|&b| ValidatorStatus::from_u8(b).unwrap()).collect();
            let shares = &share_units[..n];
            let d_total: u64 = shares.iter().sum();
            let supply = d_total + slack; // guarantees D <= S
            let reserve = reserve_target(total, reserve_min, 200);
            let productive = productive_lamports(total, reserve);

            let caps: Vec<u64> = statuses.iter()
                .map(|&s| lifecycle_cap(s, total, candidate_bps, active_bps))
                .collect();
            let directed: Vec<u64> = (0..n)
                .map(|v| directed_target(productive, shares[v], supply, caps[v]))
                .collect();
            let sum_directed: u64 = directed.iter().sum();
            let neutral = neutral_total(productive, sum_directed, d_total, supply);
            prop_assert!(neutral.is_some()); // D <= S by construction: never rejected
            let neutral = neutral.unwrap();

            let mut capacity: Vec<u64> = (0..n)
                .map(|v| neutral_capacity(statuses[v], caps[v], directed[v]))
                .collect();
            let mut finals = directed.clone();
            let out = distribute_neutral(neutral, epoch, &mut capacity, &mut finals);

            // THE conservation invariant: targets + shortfall == productive, exactly.
            prop_assert_eq!(finals.iter().sum::<u64>() + out.shortfall, productive);
            for v in 0..n {
                // Final targets never exceed the lifecycle cap.
                prop_assert!(finals[v] <= caps[v]);
                // Non-Active validators received no neutral grants.
                if statuses[v] != ValidatorStatus::Active {
                    prop_assert_eq!(finals[v], directed[v]);
                }
                // Registered/Draining/Removable land at zero (cap 0).
                if matches!(statuses[v], ValidatorStatus::Registered
                    | ValidatorStatus::Draining | ValidatorStatus::Removable) {
                    prop_assert_eq!(finals[v], 0);
                }
            }
            prop_assert!(out.rounds <= n as u64);
        }
    }
}
