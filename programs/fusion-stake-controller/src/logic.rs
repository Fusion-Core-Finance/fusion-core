//! Pure decision glue between the crank handlers and `fusion-stake-math`: the phase-transition
//! legality table, the deterministic rebalance walk, the rebalance action decision, the
//! negative-NAV comparison, and the vote-observation policy. No accounts, no syscalls — every
//! function is a deterministic map over integers, unit-tested below (the heavier end-to-end
//! scenarios live in the litesvm phase).

use fusion_stake_math::churn::{action_amount, exceeds_hysteresis};
use fusion_stake_math::lifecycle::ValidatorStatus;
use fusion_stake_math::targets::rotation_start;
use fusion_stake_view::vote_state::VoteSample;

use crate::state::{
    PHASE_FINALIZE, PHASE_IDLE, PHASE_PLAN_DIRECTED, PHASE_PLAN_FINALIZE, PHASE_PLAN_NEUTRAL,
    PHASE_PREFERENCES, PHASE_REBALANCE, PHASE_RECONCILE,
};

// --- Phase machine ---------------------------------------------------------------------------

/// The legal phase-transition relation of the crank state machine.
///
/// The forward edges are exactly the spec's cycle
/// (IDLE → RECONCILE → FINALIZE → PREFERENCES → PLAN-DIRECTED → PLAN-NEUTRAL → PLAN-FINALIZE →
/// REBALANCE → IDLE). Additionally `* → RECONCILE` is legal — `start_epoch` PREEMPTS any phase
/// when the cluster epoch has advanced past the controller epoch (the reconcile-entry condition
/// is epoch-based, not phase-based): a cycle stranded mid-phase across an epoch boundary would
/// otherwise wedge forever (stale-plan CPIs fail upstream on the staleness gate and no cursor
/// could ever complete), and a stale plan must be discarded, not resumed. Unfinished physical
/// moves carry forward through native transient/reserve mechanics.
pub fn phase_transition_allowed(from: u8, to: u8) -> bool {
    matches!(
        (from, to),
        (_, PHASE_RECONCILE)
            | (PHASE_RECONCILE, PHASE_FINALIZE)
            | (PHASE_FINALIZE, PHASE_PREFERENCES)
            | (PHASE_PREFERENCES, PHASE_PLAN_DIRECTED)
            | (PHASE_PLAN_DIRECTED, PHASE_PLAN_NEUTRAL)
            | (PHASE_PLAN_NEUTRAL, PHASE_PLAN_FINALIZE)
            | (PHASE_PLAN_FINALIZE, PHASE_REBALANCE)
            | (PHASE_REBALANCE, PHASE_IDLE)
    )
}

// --- NAV -------------------------------------------------------------------------------------

/// `true` iff the new finalized exchange rate is STRICTLY lower than the previous one:
/// `prev_total / prev_supply > new_total / new_supply`, compared exactly by cross-
/// multiplication in `u128` (no division, no rounding). Degenerate supplies (genesis, empty
/// pool) never signal a decrease.
pub fn nav_rate_decreased(
    prev_total: u64,
    prev_supply: u64,
    new_total: u64,
    new_supply: u64,
) -> bool {
    if prev_supply == 0 || new_supply == 0 {
        return false;
    }
    u128::from(prev_total) * u128::from(new_supply)
        > u128::from(new_total) * u128::from(prev_supply)
}

// --- Vote observation policy -----------------------------------------------------------------

/// One validator's reconcile-time health observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoteObservation {
    pub commission_ok: bool,
    pub liveness_ok: bool,
}

/// Derive the lifecycle observation from a (possibly failed) vote-account parse.
///
/// `sample = None` (unreadable: wrong owner, truncated, or an unsupported VoteState version —
/// including a future cluster-wide format migration) observes `commission_ok = true,
/// liveness_ok = false`: liveness failure is the guard-suppressible channel the spec routes
/// systemic vote-state problems through, while a commission verdict from unreadable data would
/// mass-drain THROUGH the guard (commission draining is deliberately never suppressed). Fail
/// closed on liveness, never fail open into commission draining.
pub fn observe_vote(
    sample: Option<VoteSample>,
    commission_cap_percent: u8,
    current_slot: u64,
    freshness_window_slots: u64,
) -> VoteObservation {
    match sample {
        None => VoteObservation { commission_ok: true, liveness_ok: false },
        Some(s) => VoteObservation {
            commission_ok: s.commission <= commission_cap_percent,
            liveness_ok: s.prior_epoch_credit_growth
                && current_slot.saturating_sub(s.freshness_slot) <= freshness_window_slots,
        },
    }
}

// --- Rebalance walk --------------------------------------------------------------------------

/// One slot of the deterministic rebalance walk: `pass` 0 processes ONLY Draining
/// decreases/removals, pass 1 the ordinary deficit/surplus moves; `index` is the validator-list
/// ordinal (as planned) the caller MUST supply accounts for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RebalanceSlot {
    pub pass: u8,
    pub index: u64,
}

/// Map the monotonic `rebalance_cursor` to its slot: two full passes over the planned
/// validator ordinals, each starting from the epoch-rotating index
/// (`fusion_stake_math::targets::rotation_start`) and wrapping — the spec's epoch-rotation
/// fairness applied to walk order. `None` once the walk is complete (or nothing was planned).
pub fn rebalance_slot(cursor: u64, planned_len: u64, epoch: u64) -> Option<RebalanceSlot> {
    if planned_len == 0 || cursor >= planned_len.saturating_mul(2) {
        return None;
    }
    let pass = (cursor / planned_len) as u8;
    let ordinal = cursor % planned_len;
    let start = rotation_start(epoch, planned_len);
    // (start + ordinal) < 2 * planned_len <= 2 * u64::MAX/2 — but be exact anyway.
    let index = (start % planned_len).wrapping_add(ordinal) % planned_len;
    Some(RebalanceSlot { pass, index })
}

// --- Rebalance action decision ---------------------------------------------------------------

/// The deterministic action for one rebalance-walk visit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// No valid move for this validator at this slot (cursor advances, zero reward).
    Skip,
    /// `RemoveValidatorFromPool` — the drained residual exits via whole-account deactivation.
    Remove,
    /// `DecreaseValidatorStakeWithReserve` by the contained lamports.
    Decrease(u64),
    /// `IncreaseValidatorStake` by the contained lamports.
    Increase(u64),
}

/// Everything [`decide_action`] reads. All balances are LIVE canonical values (validator-list
/// entry + reserve account) — the plan supplies the targets, the chain supplies the physics.
#[derive(Clone, Copy, Debug)]
pub struct ActionInputs {
    /// Walk pass (0 = Draining, 1 = ordinary).
    pub pass: u8,
    /// The record's lifecycle status (as advanced by this epoch's plan).
    pub status: ValidatorStatus,
    /// Live `active_stake_lamports` of the validator's list entry.
    pub active_lamports: u64,
    /// Live `transient_stake_lamports` — nonzero blocks ANY move (one transient per
    /// validator; upstream would fail `TransientAccountInUse` anyway, we skip instead so the
    /// cursor keeps advancing).
    pub transient_lamports: u64,
    /// The plan's final target for this validator.
    pub final_target: u64,
    /// `stake_rent + minimum_delegation` — the irreducible balance a validator stake account
    /// retains (upstream decrease floor).
    pub drained_floor: u64,
    /// `max(HYSTERESIS_MIN, bps_of(total))` (`fusion_stake_math::churn::hysteresis`).
    pub hysteresis_threshold: u64,
    /// `churn_budget_total − churn_budget_used`.
    pub budget_remaining: u64,
    /// Per-validator per-epoch move cap.
    pub validator_move_cap: u64,
    /// Live reserve lamports available for increases: reserve balance minus the operational
    /// reserve target minus the stake-rent rider the increase split carries.
    pub reserve_available_for_increase: u64,
    /// Raw live reserve balance (a decrease pre-funds the transient's rent from it).
    pub reserve_lamports: u64,
    /// Rent-exempt minimum of a stake account.
    pub stake_rent: u64,
    /// The EFFECTIVE upstream minimum delegation (minimum action size for increase/decrease),
    /// derived at runtime by the handler (`spl_cpi::effective_minimum_delegation`) — never the
    /// bare `UPSTREAM_MINIMUM_DELEGATION` floor.
    pub min_delegation: u64,
    /// Reconcile observed the validator healthy this epoch (a single liveness failure freezes
    /// increases without forcing a decrease) AND the observation is current-epoch.
    pub increases_allowed: bool,
    /// An increase or decrease already executed for this validator this epoch.
    pub acted_this_epoch: bool,
}

/// THE deterministic action for one walk visit. The caller supplies accounts; it chooses
/// nothing — a visit either yields exactly one action or a skip.
///
/// Rules (this is CURSOR-ORDER execution, not the spec's global greatest-deficit-first
/// priority — the documented deviation and its rationale live in `execute_next_action`'s
/// module doc):
/// - A live transient or an already-executed move this epoch skips (one move per validator per
///   epoch — upstream's own transient discipline).
/// - Pass 0, Removable: `Remove` (the whole-account deactivation is the only exit for the
///   sub-minimum residual — this is how a clipped sub-minimum "full drain" reconciles with the
///   upstream minimum-delegation rules; the residual can never leave via a decrease).
/// - Pass 0, Draining: decrease toward the floor. EXEMPT from hysteresis (draining is a
///   lifecycle exit, not an optimization — hysteresis would strand up to the threshold on a
///   dead validator forever), bounded by budget/per-validator cap, and floored at the upstream
///   minimum delegation (a residual below it waits for Removable).
/// - Pass 1, Candidate/Active: greatest-deviation move for THIS validator — increase on a
///   deficit (only if increases are allowed), decrease on a surplus — gated by hysteresis and
///   bounded by budget, per-validator cap, reserve/source capacity and the minimum action.
/// - Everything else skips.
pub fn decide_action(i: &ActionInputs) -> Action {
    if i.transient_lamports > 0 || i.acted_this_epoch {
        return Action::Skip;
    }
    match (i.pass, i.status) {
        (0, ValidatorStatus::Removable) => Action::Remove,
        (0, ValidatorStatus::Draining) => {
            // A decrease pre-funds the transient's rent exemption from the reserve.
            if i.reserve_lamports < i.stake_rent {
                return Action::Skip;
            }
            let deviation = i.active_lamports.saturating_sub(i.drained_floor);
            let amount = action_amount(
                deviation,
                i.budget_remaining,
                i.validator_move_cap,
                deviation, // source side IS the deviation (retention floor already subtracted)
                i.min_delegation,
                false, // upstream minimum overrides the full-drain exemption for decreases
            );
            if amount == 0 {
                Action::Skip
            } else {
                Action::Decrease(amount)
            }
        }
        (1, ValidatorStatus::Candidate | ValidatorStatus::Active) => {
            if i.final_target > i.active_lamports {
                // Deficit → increase.
                let deviation = i.final_target - i.active_lamports;
                if !exceeds_hysteresis(deviation, i.hysteresis_threshold) || !i.increases_allowed
                {
                    return Action::Skip;
                }
                let amount = action_amount(
                    deviation,
                    i.budget_remaining,
                    i.validator_move_cap,
                    i.reserve_available_for_increase,
                    i.min_delegation,
                    false,
                );
                if amount == 0 {
                    Action::Skip
                } else {
                    Action::Increase(amount)
                }
            } else {
                // Surplus → decrease.
                let deviation = i.active_lamports - i.final_target;
                if !exceeds_hysteresis(deviation, i.hysteresis_threshold) {
                    return Action::Skip;
                }
                if i.reserve_lamports < i.stake_rent {
                    return Action::Skip;
                }
                let source = i.active_lamports.saturating_sub(i.drained_floor);
                let amount = action_amount(
                    deviation,
                    i.budget_remaining,
                    i.validator_move_cap,
                    source,
                    i.min_delegation,
                    false,
                );
                if amount == 0 {
                    Action::Skip
                } else {
                    Action::Decrease(amount)
                }
            }
        }
        _ => Action::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::*;

    const ALL_PHASES: [u8; 8] = [
        PHASE_IDLE,
        PHASE_RECONCILE,
        PHASE_FINALIZE,
        PHASE_PREFERENCES,
        PHASE_PLAN_DIRECTED,
        PHASE_PLAN_NEUTRAL,
        PHASE_PLAN_FINALIZE,
        PHASE_REBALANCE,
    ];

    /// The exhaustive legality table: the forward cycle edges, plus `* → RECONCILE`
    /// (the epoch-preemption edge), and NOTHING else.
    #[test]
    fn phase_transition_table_is_exactly_the_spec_cycle_plus_preemption() {
        let forward = [
            (PHASE_IDLE, PHASE_RECONCILE),
            (PHASE_RECONCILE, PHASE_FINALIZE),
            (PHASE_FINALIZE, PHASE_PREFERENCES),
            (PHASE_PREFERENCES, PHASE_PLAN_DIRECTED),
            (PHASE_PLAN_DIRECTED, PHASE_PLAN_NEUTRAL),
            (PHASE_PLAN_NEUTRAL, PHASE_PLAN_FINALIZE),
            (PHASE_PLAN_FINALIZE, PHASE_REBALANCE),
            (PHASE_REBALANCE, PHASE_IDLE),
        ];
        for from in ALL_PHASES {
            for to in ALL_PHASES {
                let expected = forward.contains(&(from, to)) || to == PHASE_RECONCILE;
                assert_eq!(
                    phase_transition_allowed(from, to),
                    expected,
                    "transition {from} -> {to}"
                );
            }
        }
        // Corrupt `from` bytes may only recover through the preemption edge.
        assert!(phase_transition_allowed(0xFF, PHASE_RECONCILE));
        assert!(!phase_transition_allowed(0xFF, PHASE_FINALIZE));
    }

    #[test]
    fn nav_decrease_is_exact_cross_multiplication() {
        // Rate 1.5 -> 1.4: decreased.
        assert!(nav_rate_decreased(150, 100, 140, 100));
        // Rate 1.5 -> 1.5 (different scale): NOT decreased (strict).
        assert!(!nav_rate_decreased(150, 100, 300, 200));
        // Rate up: not decreased.
        assert!(!nav_rate_decreased(150, 100, 151, 100));
        // One-lamport precision at u64 scale (no float, no rounding).
        assert!(nav_rate_decreased(u64::MAX, u64::MAX, u64::MAX - 1, u64::MAX));
        // Degenerate supplies never signal.
        assert!(!nav_rate_decreased(150, 0, 140, 100));
        assert!(!nav_rate_decreased(150, 100, 0, 0));
    }

    #[test]
    fn vote_observation_policy() {
        let sample = |commission, growth, slot| VoteSample {
            commission,
            prior_epoch_credit_growth: growth,
            freshness_slot: slot,
        };
        // Healthy: fresh vote + credit growth + commission at cap.
        let o = observe_vote(Some(sample(10, true, 990)), 10, 1_000, 100);
        assert_eq!(o, VoteObservation { commission_ok: true, liveness_ok: true });
        // Commission breach observed.
        assert!(!observe_vote(Some(sample(11, true, 990)), 10, 1_000, 100).commission_ok);
        // Stale vote (out of the freshness window) fails liveness.
        assert!(!observe_vote(Some(sample(0, true, 800)), 10, 1_000, 100).liveness_ok);
        // Boundary: exactly at the window edge is still fresh.
        assert!(observe_vote(Some(sample(0, true, 900)), 10, 1_000, 100).liveness_ok);
        // No prior-epoch credit growth fails liveness.
        assert!(!observe_vote(Some(sample(0, false, 990)), 10, 1_000, 100).liveness_ok);
        // UNREADABLE: liveness fails (guard-suppressible), commission does NOT breach.
        let o = observe_vote(None, 10, 1_000, 100);
        assert_eq!(o, VoteObservation { commission_ok: true, liveness_ok: false });
        // Future freshness slot (garbage) saturates to "fresh" — credits still gate.
        assert!(observe_vote(Some(sample(0, true, 2_000)), 10, 1_000, 100).liveness_ok);
    }

    /// The rebalance walk: monotonic cursor, two full passes, epoch-rotated start, and
    /// termination — the structural half of the determinism guard (the caller cannot choose or
    /// omit a validator; the slot names exactly one index per cursor value).
    #[test]
    fn rebalance_walk_is_two_rotated_passes() {
        let len = 5u64;
        let epoch = 7u64; // rotation start = 7 % 5 = 2
        let mut seen: Vec<(u8, u64)> = Vec::new();
        let mut cursor = 0u64;
        while let Some(slot) = rebalance_slot(cursor, len, epoch) {
            seen.push((slot.pass, slot.index));
            cursor += 1;
        }
        assert_eq!(cursor, 10); // exactly 2 passes
        // Pass 0 then pass 1, each a full rotation starting at index 2.
        let expected_order = [2u64, 3, 4, 0, 1];
        for (i, &(pass, index)) in seen.iter().enumerate() {
            assert_eq!(pass, (i / 5) as u8);
            assert_eq!(index, expected_order[i % 5]);
        }
        // Every index visited exactly once per pass — omission is impossible.
        for pass in 0..2u8 {
            let mut visits: Vec<u64> =
                seen.iter().filter(|(p, _)| *p == pass).map(|(_, i)| *i).collect();
            visits.sort_unstable();
            assert_eq!(visits, [0, 1, 2, 3, 4]);
        }
        // Termination and the empty plan.
        assert_eq!(rebalance_slot(10, len, epoch), None);
        assert_eq!(rebalance_slot(u64::MAX, len, epoch), None);
        assert_eq!(rebalance_slot(0, 0, epoch), None);
    }

    /// The determinism guard from the handler's side: for a given cursor there is exactly ONE
    /// acceptable index — any other passed validator mismatches and must be rejected without
    /// advancing (the handler compares `record.validator_list_index` against `slot.index`).
    #[test]
    fn rebalance_slot_names_exactly_one_index_per_cursor() {
        for epoch in [0u64, 1, 3, 1_000_003] {
            for len in [1u64, 2, 7, 1_024] {
                for cursor in 0..(2 * len).min(64) {
                    let slot = rebalance_slot(cursor, len, epoch).unwrap();
                    assert!(slot.index < len);
                    for wrong in 0..len.min(16) {
                        if wrong != slot.index {
                            // The handler's `require!(record_index == slot.index)` rejects.
                            assert_ne!(wrong, slot.index);
                        }
                    }
                    // Same cursor, same epoch, same len => same slot (pure determinism).
                    assert_eq!(rebalance_slot(cursor, len, epoch).unwrap(), slot);
                }
            }
        }
    }

    fn base_inputs() -> ActionInputs {
        ActionInputs {
            pass: 1,
            status: ValidatorStatus::Active,
            active_lamports: 1_000_000_000_000, // 1000 SOL
            transient_lamports: 0,
            final_target: 1_000_000_000_000,
            drained_floor: 3_000_000, // ~rent + min delegation
            hysteresis_threshold: 50_000_000_000, // 50 SOL
            budget_remaining: 30_000_000_000_000,
            validator_move_cap: 5_000_000_000_000,
            reserve_available_for_increase: 10_000_000_000_000,
            reserve_lamports: 20_000_000_000_000,
            stake_rent: 2_282_880,
            min_delegation: 1_000_000,
            increases_allowed: true,
            acted_this_epoch: false,
        }
    }

    #[test]
    fn action_decision_table() {
        // Balanced: within hysteresis → skip.
        assert_eq!(decide_action(&base_inputs()), Action::Skip);

        // Deficit beyond hysteresis → increase by the deviation.
        let deficit = ActionInputs { final_target: 1_100_000_000_000, ..base_inputs() };
        assert_eq!(decide_action(&deficit), Action::Increase(100_000_000_000));

        // Same deficit, increases frozen (single liveness failure) → skip.
        let frozen = ActionInputs { increases_allowed: false, ..deficit };
        assert_eq!(decide_action(&frozen), Action::Skip);

        // Deficit clipped by reserve availability.
        let poor = ActionInputs { reserve_available_for_increase: 60_000_000_000, ..deficit };
        assert_eq!(decide_action(&poor), Action::Increase(60_000_000_000));

        // Surplus beyond hysteresis → decrease by the deviation.
        let surplus = ActionInputs { final_target: 900_000_000_000, ..base_inputs() };
        assert_eq!(decide_action(&surplus), Action::Decrease(100_000_000_000));

        // Live transient blocks everything.
        let busy = ActionInputs { transient_lamports: 1, ..surplus };
        assert_eq!(decide_action(&busy), Action::Skip);
        // One move per epoch.
        let acted = ActionInputs { acted_this_epoch: true, ..surplus };
        assert_eq!(decide_action(&acted), Action::Skip);

        // Draining ignores hysteresis (deviation below the 50-SOL threshold still drains).
        let draining = ActionInputs {
            pass: 0,
            status: ValidatorStatus::Draining,
            active_lamports: 10_000_000_000, // 10 SOL, target 0
            final_target: 0,
            ..base_inputs()
        };
        assert_eq!(decide_action(&draining), Action::Decrease(10_000_000_000 - 3_000_000));

        // Draining residual below the upstream minimum: no decrease possible → skip
        // (the residual exits via Remove once Removable).
        let residual = ActionInputs {
            active_lamports: 3_500_000, // floor + 0.5 × min_delegation
            ..draining
        };
        assert_eq!(decide_action(&residual), Action::Skip);

        // Removable in pass 0 → remove; and only in pass 0.
        let removable =
            ActionInputs { status: ValidatorStatus::Removable, ..draining };
        assert_eq!(decide_action(&removable), Action::Remove);
        let removable_p1 = ActionInputs { pass: 1, ..removable };
        assert_eq!(decide_action(&removable_p1), Action::Skip);

        // Draining in the ordinary pass → skip (handled in pass 0 only).
        let draining_p1 = ActionInputs { pass: 1, ..draining };
        assert_eq!(decide_action(&draining_p1), Action::Skip);
        // Non-draining in pass 0 → skip.
        let active_p0 = ActionInputs { pass: 0, ..surplus };
        assert_eq!(decide_action(&active_p0), Action::Skip);

        // Budget exhaustion downs every move.
        let broke = ActionInputs { budget_remaining: 0, ..surplus };
        assert_eq!(decide_action(&broke), Action::Skip);
        let broke_drain = ActionInputs { budget_remaining: 0, ..draining };
        assert_eq!(decide_action(&broke_drain), Action::Skip);

        // Decrease with a rent-empty reserve skips (the transient rent pre-funding would fail).
        let dry = ActionInputs { reserve_lamports: 0, ..surplus };
        assert_eq!(decide_action(&dry), Action::Skip);

        // Registered never acts.
        let registered = ActionInputs { status: ValidatorStatus::Registered, ..surplus };
        assert_eq!(decide_action(&registered), Action::Skip);
        let registered_p0 = ActionInputs { pass: 0, ..registered };
        assert_eq!(decide_action(&registered_p0), Action::Skip);
    }

    /// Every action respects the budget/cap bounds (the crate property, re-checked through
    /// this layer's plumbing).
    #[test]
    fn actions_respect_caps() {
        let tight = ActionInputs {
            final_target: 900_000_000_000,
            budget_remaining: 70_000_000_000,
            validator_move_cap: 80_000_000_000,
            ..base_inputs()
        };
        assert_eq!(decide_action(&tight), Action::Decrease(70_000_000_000));
        let tighter = ActionInputs { validator_move_cap: 60_000_000_000, ..tight };
        assert_eq!(decide_action(&tighter), Action::Decrease(60_000_000_000));
    }
}
