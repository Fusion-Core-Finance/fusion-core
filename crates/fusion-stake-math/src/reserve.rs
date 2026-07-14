//! Operational reserve policy.
//!
//! The reserve is a fixed percentage of total pool lamports, subject to a fixed absolute
//! minimum, and never more than the pool itself. It funds liquid-SOL withdrawals, new
//! validator stake accounts and crank-related account creation. Everything above the
//! reserve target is **productive** and MUST be represented in the epoch target plan —
//! reserve surplus is a standing increase task, not a discretionary holding.

use crate::bps_of;

/// Reserve target: `min(total, max(reserve_min, floor(total * bps / 10_000)))`.
///
/// Invariant: the result is always `<= total_lamports` (the outer `min`), so
/// [`productive_lamports`] can never underflow.
#[inline]
pub fn reserve_target(total_lamports: u64, reserve_min_lamports: u64, reserve_target_bps: u64) -> u64 {
    total_lamports.min(reserve_min_lamports.max(bps_of(total_lamports, reserve_target_bps)))
}

/// Productive (allocatable) lamports: `total - reserve`.
///
/// `reserve` must come from [`reserve_target`] over the same `total_lamports`, which
/// guarantees `reserve <= total`; the saturating form makes a violated precondition
/// fail toward "nothing to allocate" instead of wrapping.
#[inline]
pub fn productive_lamports(total_lamports: u64, reserve: u64) -> u64 {
    total_lamports.saturating_sub(reserve)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const SOL: u64 = 1_000_000_000;

    #[test]
    fn reserve_hand_vectors() {
        // Percentage dominates: 2% of 1000 SOL = 20 SOL > 10 SOL minimum.
        assert_eq!(reserve_target(1_000 * SOL, 10 * SOL, 200), 20 * SOL);
        // Minimum dominates: 2% of 100 SOL = 2 SOL < 10 SOL minimum.
        assert_eq!(reserve_target(100 * SOL, 10 * SOL, 200), 10 * SOL);
        // Total dominates: a 5-SOL pool cannot hold a 10-SOL reserve.
        assert_eq!(reserve_target(5 * SOL, 10 * SOL, 200), 5 * SOL);
        // Zero pool.
        assert_eq!(reserve_target(0, 10 * SOL, 200), 0);
        // Zero minimum and zero bps: everything is productive.
        assert_eq!(reserve_target(1_000 * SOL, 0, 0), 0);
    }

    #[test]
    fn productive_hand_vectors() {
        let total = 1_000 * SOL;
        let r = reserve_target(total, 10 * SOL, 200);
        assert_eq!(productive_lamports(total, r), 980 * SOL);
        // Reserve swallows the whole pool.
        assert_eq!(productive_lamports(5 * SOL, 5 * SOL), 0);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        /// Reserve never exceeds the pool; reserve + productive == total exactly;
        /// the reserve is at least the (total-clamped) minimum.
        #[test]
        fn reserve_split_conserves(
            total in 0u64..=u64::MAX,
            min in 0u64..=u64::MAX,
            bps in 0u64..=10_000,
        ) {
            let r = reserve_target(total, min, bps);
            prop_assert!(r <= total);
            prop_assert!(r >= min.min(total));
            let p = productive_lamports(total, r);
            prop_assert_eq!(r + p, total); // exact conservation (r <= total, so no saturation)
        }
    }
}
