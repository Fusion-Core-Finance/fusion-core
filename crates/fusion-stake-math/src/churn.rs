//! Hysteresis and churn limits for the rebalance phase.
//!
//! The controller never rebalances for small deviations: a move is valid only when the
//! absolute target deviation exceeds both a fixed SOL minimum and a pool-relative bps
//! threshold. Total principal moved per epoch is capped globally, and each validator
//! has a (lower) per-epoch move cap. Every executed action shrinks the remaining
//! global budget, so caps hold over ARBITRARY folded action sequences by construction.

use crate::bps_of;

/// Hysteresis threshold: `max(min_abs_lamports, floor(total * bps / 10_000))`
/// (the draft default is `max(50 SOL, 5 bps of pool)`).
#[inline]
pub fn hysteresis(total_lamports: u64, min_abs_lamports: u64, bps: u64) -> u64 {
    min_abs_lamports.max(bps_of(total_lamports, bps))
}

/// A move is valid only when the absolute deviation STRICTLY exceeds the threshold.
#[inline]
pub fn exceeds_hysteresis(deviation_abs: u64, threshold: u64) -> bool {
    deviation_abs > threshold
}

/// Per-epoch global churn budget: `floor(total * global_churn_cap_bps / 10_000)`.
#[inline]
pub fn global_churn_budget(total_lamports: u64, global_churn_cap_bps: u64) -> u64 {
    bps_of(total_lamports, global_churn_cap_bps)
}

/// Per-epoch per-validator move cap: `floor(total * validator_move_cap_bps / 10_000)`.
#[inline]
pub fn validator_move_cap(total_lamports: u64, validator_move_cap_bps: u64) -> u64 {
    bps_of(total_lamports, validator_move_cap_bps)
}

/// The lamports one rebalance action may move:
/// `min(deviation, remaining_global_budget, per_validator_move_cap, source_capacity)`,
/// where `source_capacity` is whatever bounds the source side (available reserve for an
/// increase, transient capacity for a decrease).
///
/// `min_action` is the minimum-action floor (the stake-pool minimum delegation): a
/// clamped amount below it returns 0 — not worth a stake account — UNLESS the deviation
/// is a full drain (`deviation_is_full_drain`, i.e. the move empties the validator
/// toward a zero target), which must always be able to finish regardless of size.
#[inline]
pub fn action_amount(
    deviation: u64,
    remaining_global_budget: u64,
    per_validator_move_cap: u64,
    source_capacity: u64,
    min_action: u64,
    deviation_is_full_drain: bool,
) -> u64 {
    let amount = deviation
        .min(remaining_global_budget)
        .min(per_validator_move_cap)
        .min(source_capacity);
    if amount < min_action && !deviation_is_full_drain {
        0
    } else {
        amount
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const SOL: u64 = 1_000_000_000;

    #[test]
    fn hysteresis_hand_vectors() {
        // Draft default max(50 SOL, 5 bps): large pool -> bps dominates.
        assert_eq!(hysteresis(2_000_000 * SOL, 50 * SOL, 5), 1_000 * SOL);
        // Small pool -> absolute minimum dominates (5 bps of 10_000 SOL = 5 SOL).
        assert_eq!(hysteresis(10_000 * SOL, 50 * SOL, 5), 50 * SOL);
        assert_eq!(hysteresis(0, 0, 5), 0);
    }

    #[test]
    fn hysteresis_is_strict() {
        assert!(!exceeds_hysteresis(50, 50)); // equal is NOT enough
        assert!(exceeds_hysteresis(51, 50));
        assert!(!exceeds_hysteresis(0, 0));
    }

    #[test]
    fn budget_and_cap_hand_vectors() {
        // Draft defaults: 300 bps global, 50 bps per validator.
        assert_eq!(global_churn_budget(1_000_000 * SOL, 300), 30_000 * SOL);
        assert_eq!(validator_move_cap(1_000_000 * SOL, 50), 5_000 * SOL);
    }

    #[test]
    fn action_amount_hand_vectors() {
        // Budget binds.
        assert_eq!(action_amount(100, 60, 80, 90, 10, false), 60);
        // Deviation binds.
        assert_eq!(action_amount(30, 60, 80, 90, 10, false), 30);
        // Per-validator cap binds.
        assert_eq!(action_amount(100, 60, 40, 90, 10, false), 40);
        // Source capacity binds.
        assert_eq!(action_amount(100, 60, 80, 20, 10, false), 20);
        // Below the minimum-action floor and not a full drain: 0.
        assert_eq!(action_amount(100, 60, 80, 90, 70, false), 0);
        // Same clamp but the deviation IS a full drain: it proceeds.
        assert_eq!(action_amount(100, 60, 80, 90, 70, true), 60);
        // Exactly at the floor proceeds.
        assert_eq!(action_amount(100, 60, 80, 90, 60, false), 60);
        // Exhausted budget always yields 0 (full drain included, at zero size).
        assert_eq!(action_amount(100, 0, 80, 90, 10, false), 0);
        assert_eq!(action_amount(100, 0, 80, 90, 10, true), 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        /// One action never exceeds any of its four bounds, and a nonzero action
        /// meets the minimum floor unless it is a full drain.
        #[test]
        fn action_bounded_by_all_inputs(
            dev in 0u64..=u64::MAX,
            budget in 0u64..=u64::MAX,
            cap in 0u64..=u64::MAX,
            src in 0u64..=u64::MAX,
            min_action in 0u64..=u64::MAX,
            full_drain: bool,
        ) {
            let a = action_amount(dev, budget, cap, src, min_action, full_drain);
            prop_assert!(a <= dev && a <= budget && a <= cap && a <= src);
            prop_assert!(a == 0 || a >= min_action || full_drain);
        }

        /// Churn caps hold over ARBITRARY folded action sequences: total moved never
        /// exceeds the initial global budget, the budget never underflows, and every
        /// single action respects the per-validator cap.
        #[test]
        fn caps_hold_over_folded_sequences(
            total in 0u64..=u64::MAX / 2,
            global_bps in 0u64..=10_000,
            validator_bps in 0u64..=10_000,
            actions in proptest::collection::vec(
                (0u64..=u64::MAX, 0u64..=u64::MAX, 0u64..=1_000_000, any::<bool>()),
                0..64,
            ),
            min_action in 0u64..=1_000,
        ) {
            let initial = global_churn_budget(total, global_bps);
            let vcap = validator_move_cap(total, validator_bps);
            let mut budget = initial;
            let mut moved: u128 = 0;
            for (dev, src, _, full_drain) in actions {
                let a = action_amount(dev, budget, vcap, src, min_action, full_drain);
                prop_assert!(a <= vcap); // per-validator cap holds on every action
                prop_assert!(a <= budget); // never overdraws (so the fold can't underflow)
                budget -= a;
                moved += u128::from(a);
            }
            prop_assert_eq!(moved, u128::from(initial - budget));
            prop_assert!(moved <= u128::from(initial)); // global cap holds over the fold
        }
    }
}
