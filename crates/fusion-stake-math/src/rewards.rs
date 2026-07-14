//! Crank reward payouts from the maintenance vault.
//!
//! Rewards are fixed fuSOL share amounts by task class, bounded by both the epoch
//! payout budget and the vault balance. A zero payout is a normal outcome — cranks
//! remain executable unpaid on an empty vault (permissionless liveness never depends
//! on the reward). No-op, duplicate, stale-cursor and failed transactions earn zero
//! upstream of this function (they never reach a payout).

/// The shares actually paid for a successful, previously incomplete crank task:
/// `min(task_reward, epoch_budget_remaining, vault_balance)`. Never exceeds any of
/// the three bounds, so the epoch budget and the vault are conserved by construction.
#[inline]
pub fn payout(task_reward: u64, epoch_budget_remaining: u64, vault_balance: u64) -> u64 {
    task_reward.min(epoch_budget_remaining).min(vault_balance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn payout_hand_vectors() {
        assert_eq!(payout(100, 1_000, 1_000), 100); // task reward binds
        assert_eq!(payout(100, 60, 1_000), 60); // epoch budget binds
        assert_eq!(payout(100, 1_000, 30), 30); // vault binds
        assert_eq!(payout(100, 1_000, 0), 0); // empty vault: crank runs unpaid
        assert_eq!(payout(0, 1_000, 1_000), 0); // zero-reward task class
        assert_eq!(payout(100, 0, 1_000), 0); // budget exhausted
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        /// The payout is exactly the minimum of the three bounds — never exceeds any,
        /// and always equals one of them (budget/vault conservation by construction).
        #[test]
        fn payout_is_min_of_three(
            r in 0u64..=u64::MAX,
            b in 0u64..=u64::MAX,
            v in 0u64..=u64::MAX,
        ) {
            let p = payout(r, b, v);
            prop_assert!(p <= r && p <= b && p <= v);
            prop_assert!(p == r || p == b || p == v);
        }
    }
}
