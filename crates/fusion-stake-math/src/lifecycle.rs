//! Validator lifecycle state machine and lifecycle-derived allocation caps.
//!
//! Liveness is simple and conservative: the controller checks only recent voting and
//! positive prior-epoch credits — it never ranks validators by yield. One failed epoch
//! freezes increases without forcing a decrease; two consecutive failed completed epochs
//! drain the validator, unless the global liveness guard is active (a systemic-outage
//! circuit breaker). A commission breach drains IMMEDIATELY and is NEVER suppressed by
//! the guard. Candidates promote to Active only after two consecutive healthy completed
//! epochs while carrying pool stake, and receive explicitly directed stake only — never
//! neutral allocation.

use crate::bps_of;

/// Consecutive failed completed epochs of liveness before a validator drains.
pub const LIVENESS_FAILURE_EPOCHS: u8 = 2;
/// Consecutive healthy completed epochs (while carrying pool stake) before a
/// Candidate promotes to Active.
pub const CANDIDATE_HEALTHY_EPOCHS: u8 = 2;

/// Validator lifecycle status. `repr(u8)` with fixed discriminants for on-chain storage;
/// round-trip via [`ValidatorStatus::as_u8`] / [`ValidatorStatus::from_u8`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ValidatorStatus {
    /// Record exists but the validator is not in the stake-pool validator list.
    /// Receives neither directed nor neutral stake. Admission to Candidate is an
    /// explicit program action (eligibility + minimum support + list capacity),
    /// not an epoch transition — [`advance_lifecycle`] never leaves this state.
    Registered = 0,
    /// Eligible and in probation. May receive only explicitly directed stake,
    /// bounded by the candidate cap. NEVER receives neutral allocation.
    Candidate = 1,
    /// Completed probation. Receives directed plus equal neutral allocation,
    /// bounded by the active cap.
    Active = 2,
    /// Commission breach, persistent liveness failure or removal condition.
    /// Excluded from neutral allocation; target is 0, so pool stake
    /// monotonically decreases toward 0.
    Draining = 3,
    /// No active/transient stake, no current target, removal delay completed.
    /// Terminal for this state machine (list-slot removal is a program action).
    Removable = 4,
}

impl ValidatorStatus {
    /// The stored on-chain byte.
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Parse the stored byte; `None` for anything outside the five valid states
    /// (fail closed on corrupt storage).
    #[inline]
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Registered),
            1 => Some(Self::Candidate),
            2 => Some(Self::Active),
            3 => Some(Self::Draining),
            4 => Some(Self::Removable),
            _ => None,
        }
    }
}

/// One completed epoch's observations for a validator, fed to [`advance_lifecycle`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleInput {
    /// Normalized inflation commission is at or below the fixed cap.
    pub commission_ok: bool,
    /// Passed BOTH liveness checks this completed epoch (recent vote landed within
    /// the freshness window AND positive prior-epoch credit growth).
    pub liveness_ok: bool,
    /// Consecutive failed completed epochs BEFORE counting this one.
    pub consecutive_failures: u8,
    /// Consecutive healthy-with-pool-stake completed epochs BEFORE counting this one.
    pub consecutive_healthy: u8,
    /// Validator carried pool stake through this completed epoch (probation only
    /// counts epochs spent actually carrying stake).
    pub has_pool_stake: bool,
    /// The global liveness guard is active (see [`global_liveness_guard`]);
    /// suppresses liveness-based draining — NEVER commission-based draining.
    pub guard_active: bool,
    /// Zero active AND zero transient stake AND a zero current target.
    pub zero_stake_and_target: bool,
    /// The fixed removal delay has elapsed since the validator began draining.
    pub removal_delay_elapsed: bool,
}

/// The epoch decision [`advance_lifecycle`] produces: the next status, the updated
/// streak counters to persist, and whether increases are frozen this epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LifecycleOutcome {
    pub status: ValidatorStatus,
    /// Persist as the next epoch's `consecutive_failures` input.
    pub consecutive_failures: u8,
    /// Persist as the next epoch's `consecutive_healthy` input.
    pub consecutive_healthy: u8,
    /// A single liveness failure stops increases but forces no decrease: the
    /// rebalance layer must not perform increase actions for this validator this
    /// epoch. (Draining/Removable/Registered are structurally increase-free anyway
    /// via a zero lifecycle cap; commission breach drains immediately.)
    pub increases_frozen: bool,
}

/// Advance one validator's lifecycle by one completed epoch. Pure and deterministic.
///
/// Rule order (highest precedence first) for Candidate/Active:
/// 1. Commission breach → Draining IMMEDIATELY — never suppressed by the guard.
/// 2. Liveness failure streak reaching [`LIVENESS_FAILURE_EPOCHS`] → Draining,
///    UNLESS the global liveness guard is active (the streak still accumulates,
///    so a validator still failing when the guard lifts drains then).
/// 3. Candidate with a healthy streak reaching [`CANDIDATE_HEALTHY_EPOCHS`] → Active.
///
/// Streaks: a healthy epoch zeroes the failure streak; a failed epoch increments it
/// (saturating). The promotion streak increments only on a healthy epoch spent
/// carrying pool stake, and zeroes otherwise — "healthy while carrying pool stake"
/// must be consecutive.
///
/// Registered never transitions here (admission is an explicit program action, and a
/// Registered validator carries no pool stake, so its promotion streak stays 0).
/// Removable is terminal. Draining moves to Removable only at zero active+transient
/// stake, zero target, and an elapsed removal delay.
pub fn advance_lifecycle(current: ValidatorStatus, input: &LifecycleInput) -> LifecycleOutcome {
    let consecutive_failures = if input.liveness_ok {
        0
    } else {
        input.consecutive_failures.saturating_add(1)
    };
    let consecutive_healthy = if input.liveness_ok && input.has_pool_stake {
        input.consecutive_healthy.saturating_add(1)
    } else {
        0
    };

    let status = match current {
        ValidatorStatus::Registered => ValidatorStatus::Registered,
        ValidatorStatus::Removable => ValidatorStatus::Removable,
        ValidatorStatus::Draining => {
            if input.zero_stake_and_target && input.removal_delay_elapsed {
                ValidatorStatus::Removable
            } else {
                ValidatorStatus::Draining
            }
        }
        ValidatorStatus::Candidate | ValidatorStatus::Active => {
            if !input.commission_ok {
                // Commission breach drains immediately; the guard never suppresses it.
                ValidatorStatus::Draining
            } else if consecutive_failures >= LIVENESS_FAILURE_EPOCHS && !input.guard_active {
                ValidatorStatus::Draining
            } else if current == ValidatorStatus::Candidate
                && consecutive_healthy >= CANDIDATE_HEALTHY_EPOCHS
            {
                ValidatorStatus::Active
            } else {
                current
            }
        }
    };

    LifecycleOutcome {
        status,
        consecutive_failures,
        consecutive_healthy,
        increases_frozen: !input.liveness_ok,
    }
}

/// Global liveness guard: `true` (suppress liveness-based draining for this epoch)
/// when less than half of currently delegated pool stake passes the health check.
/// Prevents a cluster-wide outage or systemic data problem from triggering mass churn.
/// With zero delegated stake there is nothing to mass-drain, so the guard stays off.
#[inline]
pub fn global_liveness_guard(healthy_delegated_lamports: u64, total_delegated_lamports: u64) -> bool {
    // healthy < total/2 without a lossy division: healthy*2 < total, exact in u128.
    u128::from(healthy_delegated_lamports) * 2 < u128::from(total_delegated_lamports)
}

/// The lifecycle-derived allocation cap (the maximum FINAL target, directed + neutral):
/// 0 for Registered/Draining/Removable, `floor(total * candidate_cap_bps / 10_000)` for
/// Candidate (which may fill it with DIRECTED stake only), and
/// `floor(total * active_cap_bps / 10_000)` for Active.
#[inline]
pub fn lifecycle_cap(
    status: ValidatorStatus,
    total_pool_lamports: u64,
    candidate_cap_bps: u64,
    active_cap_bps: u64,
) -> u64 {
    match status {
        ValidatorStatus::Registered | ValidatorStatus::Draining | ValidatorStatus::Removable => 0,
        ValidatorStatus::Candidate => bps_of(total_pool_lamports, candidate_cap_bps),
        ValidatorStatus::Active => bps_of(total_pool_lamports, active_cap_bps),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A healthy, staked, guard-off, no-removal baseline input.
    fn healthy() -> LifecycleInput {
        LifecycleInput {
            commission_ok: true,
            liveness_ok: true,
            consecutive_failures: 0,
            consecutive_healthy: 0,
            has_pool_stake: true,
            guard_active: false,
            zero_stake_and_target: false,
            removal_delay_elapsed: false,
        }
    }

    #[test]
    fn status_byte_round_trips() {
        for s in [
            ValidatorStatus::Registered,
            ValidatorStatus::Candidate,
            ValidatorStatus::Active,
            ValidatorStatus::Draining,
            ValidatorStatus::Removable,
        ] {
            assert_eq!(ValidatorStatus::from_u8(s.as_u8()), Some(s));
        }
        for b in 5u8..=255 {
            assert_eq!(ValidatorStatus::from_u8(b), None);
        }
    }

    #[test]
    fn commission_breach_drains_immediately_even_under_guard() {
        let input = LifecycleInput { commission_ok: false, guard_active: true, ..healthy() };
        for from in [ValidatorStatus::Candidate, ValidatorStatus::Active] {
            let out = advance_lifecycle(from, &input);
            assert_eq!(out.status, ValidatorStatus::Draining);
        }
    }

    #[test]
    fn two_consecutive_liveness_failures_drain_unless_guard() {
        let failing = LifecycleInput { liveness_ok: false, consecutive_failures: 1, ..healthy() };
        for from in [ValidatorStatus::Candidate, ValidatorStatus::Active] {
            let out = advance_lifecycle(from, &failing);
            assert_eq!(out.status, ValidatorStatus::Draining);
            assert_eq!(out.consecutive_failures, 2);
        }
        // Guard active: draining suppressed, but the streak keeps accumulating.
        let guarded = LifecycleInput { guard_active: true, ..failing };
        let out = advance_lifecycle(ValidatorStatus::Active, &guarded);
        assert_eq!(out.status, ValidatorStatus::Active);
        assert_eq!(out.consecutive_failures, 2);
        // Guard lifts, validator still failing: drains now.
        let after_guard = LifecycleInput { liveness_ok: false, consecutive_failures: 2, ..healthy() };
        let out = advance_lifecycle(ValidatorStatus::Active, &after_guard);
        assert_eq!(out.status, ValidatorStatus::Draining);
        // Guard lifts, validator recovered: failure streak resets, stays Active.
        let recovered = LifecycleInput { consecutive_failures: 2, ..healthy() };
        let out = advance_lifecycle(ValidatorStatus::Active, &recovered);
        assert_eq!(out.status, ValidatorStatus::Active);
        assert_eq!(out.consecutive_failures, 0);
    }

    #[test]
    fn single_failure_freezes_increases_without_status_change() {
        let one_fail = LifecycleInput { liveness_ok: false, ..healthy() };
        let out = advance_lifecycle(ValidatorStatus::Active, &one_fail);
        assert_eq!(out.status, ValidatorStatus::Active); // no forced decrease
        assert_eq!(out.consecutive_failures, 1);
        assert!(out.increases_frozen); // but increases stop
        let out = advance_lifecycle(ValidatorStatus::Active, &healthy());
        assert!(!out.increases_frozen);
    }

    #[test]
    fn candidate_promotes_after_two_healthy_staked_epochs() {
        // First healthy epoch with stake: streak 1, still Candidate.
        let out = advance_lifecycle(ValidatorStatus::Candidate, &healthy());
        assert_eq!(out.status, ValidatorStatus::Candidate);
        assert_eq!(out.consecutive_healthy, 1);
        // Second consecutive: promotes.
        let second = LifecycleInput { consecutive_healthy: 1, ..healthy() };
        let out = advance_lifecycle(ValidatorStatus::Candidate, &second);
        assert_eq!(out.status, ValidatorStatus::Active);
        assert_eq!(out.consecutive_healthy, 2);
    }

    #[test]
    fn healthy_epoch_without_pool_stake_resets_promotion_streak() {
        let unstaked = LifecycleInput { has_pool_stake: false, consecutive_healthy: 1, ..healthy() };
        let out = advance_lifecycle(ValidatorStatus::Candidate, &unstaked);
        assert_eq!(out.status, ValidatorStatus::Candidate);
        assert_eq!(out.consecutive_healthy, 0); // "while carrying pool stake" must be consecutive
    }

    #[test]
    fn registered_never_advances_and_never_accrues_promotion_streak() {
        // Even a (bogus) healthy+staked input leaves Registered in place; a real
        // Registered validator has no pool stake, so the streak stays 0.
        let out = advance_lifecycle(
            ValidatorStatus::Registered,
            &LifecycleInput { has_pool_stake: false, consecutive_healthy: 7, ..healthy() },
        );
        assert_eq!(out.status, ValidatorStatus::Registered);
        assert_eq!(out.consecutive_healthy, 0);
    }

    #[test]
    fn draining_to_removable_requires_all_three_conditions() {
        let both = LifecycleInput { zero_stake_and_target: true, removal_delay_elapsed: true, ..healthy() };
        assert_eq!(advance_lifecycle(ValidatorStatus::Draining, &both).status, ValidatorStatus::Removable);
        let no_delay = LifecycleInput { zero_stake_and_target: true, ..healthy() };
        assert_eq!(advance_lifecycle(ValidatorStatus::Draining, &no_delay).status, ValidatorStatus::Draining);
        let still_staked = LifecycleInput { removal_delay_elapsed: true, ..healthy() };
        assert_eq!(advance_lifecycle(ValidatorStatus::Draining, &still_staked).status, ValidatorStatus::Draining);
    }

    #[test]
    fn removable_is_terminal() {
        let out = advance_lifecycle(ValidatorStatus::Removable, &healthy());
        assert_eq!(out.status, ValidatorStatus::Removable);
    }

    #[test]
    fn guard_hand_vectors() {
        assert!(global_liveness_guard(49, 100)); // < 50% healthy: suppress
        assert!(!global_liveness_guard(50, 100)); // exactly half: no suppression
        assert!(!global_liveness_guard(0, 0)); // empty pool: nothing to mass-drain
        assert!(global_liveness_guard(0, 1));
        assert!(!global_liveness_guard(u64::MAX, u64::MAX)); // no u64 overflow (u128 product)
        assert!(global_liveness_guard(u64::MAX / 2, u64::MAX));
    }

    #[test]
    fn lifecycle_cap_by_status() {
        let total = 1_000_000u64;
        // Draft defaults: candidate 25 bps, active 200 bps.
        assert_eq!(lifecycle_cap(ValidatorStatus::Registered, total, 25, 200), 0);
        assert_eq!(lifecycle_cap(ValidatorStatus::Draining, total, 25, 200), 0);
        assert_eq!(lifecycle_cap(ValidatorStatus::Removable, total, 25, 200), 0);
        assert_eq!(lifecycle_cap(ValidatorStatus::Candidate, total, 25, 200), 2_500);
        assert_eq!(lifecycle_cap(ValidatorStatus::Active, total, 25, 200), 20_000);
    }

    /// Exhaustive over the whole (status × 6 booleans × small streaks) input space:
    /// the machine only ever produces legal transitions.
    #[test]
    fn transition_relation_is_exactly_the_spec() {
        use ValidatorStatus::*;
        for status in [Registered, Candidate, Active, Draining, Removable] {
            for bits in 0u8..64 {
                for failures in 0u8..4 {
                    for healthy_streak in 0u8..4 {
                        let input = LifecycleInput {
                            commission_ok: bits & 1 != 0,
                            liveness_ok: bits & 2 != 0,
                            consecutive_failures: failures,
                            consecutive_healthy: healthy_streak,
                            has_pool_stake: bits & 4 != 0,
                            guard_active: bits & 8 != 0,
                            zero_stake_and_target: bits & 16 != 0,
                            removal_delay_elapsed: bits & 32 != 0,
                        };
                        let out = advance_lifecycle(status, &input);
                        let legal: &[ValidatorStatus] = match status {
                            Registered => &[Registered],
                            Candidate => &[Candidate, Active, Draining],
                            Active => &[Active, Draining],
                            Draining => &[Draining, Removable],
                            Removable => &[Removable],
                        };
                        assert!(legal.contains(&out.status), "{status:?} -> {:?}", out.status);
                        // Commission breach is never suppressed.
                        if matches!(status, Candidate | Active) && !input.commission_ok {
                            assert_eq!(out.status, Draining);
                        }
                        // The guard suppresses liveness draining (commission ok).
                        if matches!(status, Candidate | Active)
                            && input.commission_ok
                            && input.guard_active
                        {
                            assert_ne!(out.status, Draining);
                        }
                        // Streak bookkeeping.
                        if input.liveness_ok {
                            assert_eq!(out.consecutive_failures, 0);
                        } else {
                            assert_eq!(out.consecutive_failures, failures + 1);
                            assert!(out.increases_frozen);
                        }
                    }
                }
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        /// Guard is exactly `healthy*2 < total` — never active at or above half.
        #[test]
        fn guard_matches_reference(healthy in 0u64..=u64::MAX, total in 0u64..=u64::MAX) {
            let expect = (healthy as u128) * 2 < total as u128;
            prop_assert_eq!(global_liveness_guard(healthy, total), expect);
        }
    }
}
