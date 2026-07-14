use anchor_lang::prelude::*;
use fusion_stake_math::targets::NeutralRound;

// --- Crank phases (the spec's IDLE → RECONCILE → … → REBALANCE → IDLE machine) --------------
pub const PHASE_IDLE: u8 = 0;
pub const PHASE_RECONCILE: u8 = 1;
pub const PHASE_FINALIZE: u8 = 2;
pub const PHASE_PREFERENCES: u8 = 3;
pub const PHASE_PLAN_DIRECTED: u8 = 4;
pub const PHASE_PLAN_NEUTRAL: u8 = 5;
pub const PHASE_PLAN_FINALIZE: u8 = 6;
pub const PHASE_REBALANCE: u8 = 7;

/// The single crank write lane (zero-copy). PDA `[b"epoch_state"]`.
///
/// Competing permissionless callers may race, but only one transaction can advance a given
/// cursor value; successful transitions are idempotent at the task level. Fusion position
/// operations never write this account, so debt paths stay independent of the crank.
///
/// All plan aggregates are FINALIZED-snapshot values: the controller never estimates stake
/// balances — it snapshots the canonical stake-pool totals at FINALIZE and plans from those.
#[account(zero_copy)]
#[repr(C)]
#[derive(Debug)]
pub struct EpochState {
    /// The epoch this controller cycle is processing (advanced by `start_epoch` when the
    /// cluster epoch exceeds it).
    pub controller_epoch: u64,

    // --- cursors (all monotonic within their phase) --------------------------------------
    /// Next validator-list index RECONCILE will update (u64 for field alignment; the upstream
    /// list index is u32).
    pub reconcile_cursor: u64,
    /// Next canonical validator ordinal PLAN-DIRECTED will process.
    pub plan_directed_cursor: u64,
    /// Rebalance actions executed this epoch.
    pub rebalance_actions_done: u64,

    /// Slot at which the preference window closes (set by FINALIZE; 0 = window not open).
    pub preference_window_close_slot: u64,

    // --- finalized NAV snapshot -----------------------------------------------------------
    /// Canonical `StakePool.total_lamports` at FINALIZE.
    pub nav_total_lamports: u64,
    /// Canonical fuSOL supply at FINALIZE.
    pub nav_fusol_supply: u64,
    /// `total - reserve_target` — the lamports the plan MUST fully assign (or record as
    /// capacity shortfall).
    pub productive_lamports: u64,
    /// The epoch's operational reserve target.
    pub reserve_target: u64,

    // --- plan aggregates --------------------------------------------------------------------
    /// Σ eligible directed shares over all validators, CHECKED-summed in PLAN-DIRECTED; the
    /// plan is rejected when it exceeds `nav_fusol_supply` (`D > S`).
    pub total_directed_shares: u64,
    /// The neutral pool remaining to distribute (decremented as rounds grant).
    pub neutral_total: u64,
    /// Unsaturated Active validators entering the CURRENT capacity round.
    pub unsaturated_active_count: u64,
    /// Neutral lamports no Active capacity could absorb (recorded at PLAN-FINALIZE; they stay
    /// in reserve temporarily).
    pub capacity_shortfall: u64,

    // --- churn ------------------------------------------------------------------------------
    /// This epoch's global churn budget (`bps_of(total, GLOBAL_CHURN_CAP_BPS)`).
    pub churn_budget_total: u64,
    /// Budget consumed by executed actions (`<= churn_budget_total`).
    pub churn_budget_used: u64,

    // --- global liveness guard aggregates (accumulated during PLAN-DIRECTED) ----------------
    /// Delegated pool lamports whose validator passed the health check this epoch.
    pub healthy_delegated_lamports: u64,
    /// Total DELEGATED pool lamports (excludes the reserve — the guard denominator).
    pub total_delegated_lamports: u64,

    // --- maintenance rewards -----------------------------------------------------------------
    /// Crank payouts made this epoch (`<= CRANK_EPOCH_PAYOUT_BUDGET`).
    pub epoch_payout_budget_used: u64,

    // --- neutral capacity-round state (mirrors `fusion_stake_math::targets::NeutralRound`
    //     field-for-field so the fold resumes across transactions; see `neutral_round()` /
    //     `set_neutral_round()`) ------------------------------------------------------------
    pub round_remaining: u64,
    pub round_n_unsaturated: u64,
    pub round_tranche: u64,
    pub round_remainder: u64,
    pub round_start: u64,
    pub round_steps: u64,
    pub round_remainder_used: u64,
    pub round_granted: u64,
    pub round_saturated: u64,
    /// Completed capacity rounds this epoch (terminates within `MAX_NEUTRAL_ROUNDS`).
    /// `0` doubles as "no round opened yet this phase".
    pub neutral_round_number: u64,
    /// In-round validator cursor: next canonical validator ordinal to fold into the round.
    pub neutral_cursor: u64,

    // --- plan verification accumulators -----------------------------------------------------
    /// Σ per-validator directed targets, CHECKED-summed in PLAN-DIRECTED (list records AND
    /// admission extras) — one leg of the PLAN-FINALIZE conservation proof.
    pub sum_directed_targets: u64,
    /// Σ neutral grants applied, CHECKED-summed grant-by-grant in PLAN-NEUTRAL — the second
    /// leg of the conservation proof (independent of the round mirror's own accounting).
    pub neutral_granted_total: u64,

    // --- rebalance walk ----------------------------------------------------------------------
    /// Deterministic rebalance visit counter over `[0, 2 × planned validators)`: pass 0
    /// (Draining decreases/removals) then pass 1 (ordinary deficit/surplus moves), each pass
    /// walking list ordinals from the epoch-rotating start index. See `logic::rebalance_slot`.
    pub rebalance_cursor: u64,

    // --- preferred-withdraw fold -------------------------------------------------------------
    /// Greatest positive `observed active − final target` seen in the latest full plan walk
    /// (reset at each capacity-round start; PLAN-DIRECTED seeds it for the zero-round case).
    pub preferred_withdraw_surplus: u64,
    /// The vote account holding that greatest surplus (`[0; 32]` when none). PLAN-FINALIZE
    /// sets it (or None) as the pool's preferred WITHDRAW validator — the deterministic
    /// drain-first source.
    pub preferred_withdraw_vote: [u8; 32],

    /// Current phase (`PHASE_*`).
    pub phase: u8,
    /// Explicit padding to the 8-byte boundary (zero-copy layouts carry no implicit padding).
    pub _padding: [u8; 7],
    /// Forward-compat reserve (carve from the HEAD on any later addition).
    pub _reserved: [u8; 64],
}

// House zero-copy discipline: size/alignment + field-offset pins, so a size-neutral swap of two
// equal-width fields is a compile error, not a silent byte remap.
// `% 8 == 0` (not `.is_multiple_of(8)`): the SBF toolchain (platform-tools v1.48, cargo 1.84)
// predates `is_multiple_of`, and clippy 1.93's suggestion would break the on-chain build.
#[allow(clippy::manual_is_multiple_of)]
const _: () = assert!(core::mem::size_of::<EpochState>() % 8 == 0);
const _: () = assert!(core::mem::size_of::<EpochState>() == 368);
const _: () = assert!(core::mem::offset_of!(EpochState, controller_epoch) == 0);
const _: () = assert!(core::mem::offset_of!(EpochState, reconcile_cursor) == 8);
const _: () = assert!(core::mem::offset_of!(EpochState, plan_directed_cursor) == 16);
const _: () = assert!(core::mem::offset_of!(EpochState, rebalance_actions_done) == 24);
const _: () = assert!(core::mem::offset_of!(EpochState, preference_window_close_slot) == 32);
const _: () = assert!(core::mem::offset_of!(EpochState, nav_total_lamports) == 40);
const _: () = assert!(core::mem::offset_of!(EpochState, nav_fusol_supply) == 48);
const _: () = assert!(core::mem::offset_of!(EpochState, productive_lamports) == 56);
const _: () = assert!(core::mem::offset_of!(EpochState, reserve_target) == 64);
const _: () = assert!(core::mem::offset_of!(EpochState, total_directed_shares) == 72);
const _: () = assert!(core::mem::offset_of!(EpochState, neutral_total) == 80);
const _: () = assert!(core::mem::offset_of!(EpochState, unsaturated_active_count) == 88);
const _: () = assert!(core::mem::offset_of!(EpochState, capacity_shortfall) == 96);
const _: () = assert!(core::mem::offset_of!(EpochState, churn_budget_total) == 104);
const _: () = assert!(core::mem::offset_of!(EpochState, churn_budget_used) == 112);
const _: () = assert!(core::mem::offset_of!(EpochState, healthy_delegated_lamports) == 120);
const _: () = assert!(core::mem::offset_of!(EpochState, total_delegated_lamports) == 128);
const _: () = assert!(core::mem::offset_of!(EpochState, epoch_payout_budget_used) == 136);
const _: () = assert!(core::mem::offset_of!(EpochState, round_remaining) == 144);
const _: () = assert!(core::mem::offset_of!(EpochState, round_n_unsaturated) == 152);
const _: () = assert!(core::mem::offset_of!(EpochState, round_tranche) == 160);
const _: () = assert!(core::mem::offset_of!(EpochState, round_remainder) == 168);
const _: () = assert!(core::mem::offset_of!(EpochState, round_start) == 176);
const _: () = assert!(core::mem::offset_of!(EpochState, round_steps) == 184);
const _: () = assert!(core::mem::offset_of!(EpochState, round_remainder_used) == 192);
const _: () = assert!(core::mem::offset_of!(EpochState, round_granted) == 200);
const _: () = assert!(core::mem::offset_of!(EpochState, round_saturated) == 208);
const _: () = assert!(core::mem::offset_of!(EpochState, neutral_round_number) == 216);
const _: () = assert!(core::mem::offset_of!(EpochState, neutral_cursor) == 224);
const _: () = assert!(core::mem::offset_of!(EpochState, sum_directed_targets) == 232);
const _: () = assert!(core::mem::offset_of!(EpochState, neutral_granted_total) == 240);
const _: () = assert!(core::mem::offset_of!(EpochState, rebalance_cursor) == 248);
const _: () = assert!(core::mem::offset_of!(EpochState, preferred_withdraw_surplus) == 256);
const _: () = assert!(core::mem::offset_of!(EpochState, preferred_withdraw_vote) == 264);
const _: () = assert!(core::mem::offset_of!(EpochState, phase) == 296);
const _: () = assert!(core::mem::offset_of!(EpochState, _padding) == 297);
const _: () = assert!(core::mem::offset_of!(EpochState, _reserved) == 304);

impl EpochState {
    pub const SPACE: usize = 8 + core::mem::size_of::<EpochState>(); // 8 + 368 = 376

    /// Rehydrate the persisted capacity-round fold state for `fusion_stake_math::targets::step`.
    pub fn neutral_round(&self) -> NeutralRound {
        NeutralRound {
            remaining: self.round_remaining,
            n_unsaturated: self.round_n_unsaturated,
            tranche: self.round_tranche,
            remainder: self.round_remainder,
            start: self.round_start,
            steps: self.round_steps,
            remainder_used: self.round_remainder_used,
            granted: self.round_granted,
            saturated: self.round_saturated,
        }
    }

    /// Persist the fold state back after a batch of `step` calls.
    pub fn set_neutral_round(&mut self, round: &NeutralRound) {
        self.round_remaining = round.remaining;
        self.round_n_unsaturated = round.n_unsaturated;
        self.round_tranche = round.tranche;
        self.round_remainder = round.remainder;
        self.round_start = round.start;
        self.round_steps = round.steps;
        self.round_remainder_used = round.remainder_used;
        self.round_granted = round.granted;
        self.round_saturated = round.saturated;
    }

    /// Zero every plan accumulator, cursor and round-mirror field the plan phases build up.
    /// Called at PLAN-DIRECTED entry (`close_preference_window`) and folded into
    /// [`reset_for_new_epoch`]. NEVER touches the finalized NAV snapshot.
    pub fn reset_plan_state(&mut self) {
        self.plan_directed_cursor = 0;
        self.total_directed_shares = 0;
        self.sum_directed_targets = 0;
        self.neutral_total = 0;
        self.neutral_granted_total = 0;
        self.unsaturated_active_count = 0;
        self.capacity_shortfall = 0;
        self.set_neutral_round(&NeutralRound {
            remaining: 0,
            n_unsaturated: 0,
            tranche: 0,
            remainder: 0,
            start: 0,
            steps: 0,
            remainder_used: 0,
            granted: 0,
            saturated: 0,
        });
        self.neutral_round_number = 0;
        self.neutral_cursor = 0;
        self.rebalance_cursor = 0;
        self.rebalance_actions_done = 0;
        self.preferred_withdraw_surplus = 0;
        self.preferred_withdraw_vote = [0u8; 32];
    }

    /// Full per-epoch reset (`start_epoch`): everything above plus the reconcile cursor, the
    /// preference window, churn/payout budgets and the liveness-guard aggregates. The NAV
    /// snapshot fields deliberately SURVIVE — the negative-NAV comparison and the provisional
    /// churn budget both read the PREVIOUS finalized values.
    pub fn reset_for_new_epoch(&mut self) {
        self.reset_plan_state();
        self.reconcile_cursor = 0;
        self.preference_window_close_slot = 0;
        self.churn_budget_used = 0;
        self.healthy_delegated_lamports = 0;
        self.total_delegated_lamports = 0;
        self.epoch_payout_budget_used = 0;
    }

    /// Fold one validator into the preferred-withdraw argmax (strictly greater wins, so ties
    /// resolve to the first candidate in canonical walk order — deterministic).
    pub fn fold_preferred_withdraw(&mut self, vote: &[u8; 32], surplus: u64) {
        if surplus > self.preferred_withdraw_surplus {
            self.preferred_withdraw_surplus = surplus;
            self.preferred_withdraw_vote = *vote;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fusion_stake_math::targets::{begin_round, step};

    /// The persisted mirror round-trips through the pure-crate fold: stepping a rehydrated
    /// round is byte-identical to stepping the original (the incremental crank's soundness).
    #[test]
    fn neutral_round_mirror_round_trips() {
        let mut es: EpochState = unsafe { core::mem::zeroed() };
        let mut reference = begin_round(100, 3, 7).unwrap();
        es.set_neutral_round(&reference);
        assert_eq!(es.neutral_round(), reference);

        // Step the reference once, persist, rehydrate, and step both once more.
        let g1 = step(&mut reference, 50);
        es.set_neutral_round(&reference);
        let mut rehydrated = es.neutral_round();
        assert_eq!(rehydrated, reference);
        let g2_ref = step(&mut reference, 10);
        let g2_re = step(&mut rehydrated, 10);
        assert_eq!(g2_ref, g2_re);
        assert_eq!(rehydrated, reference);
        assert!(g1 > 0);
    }
}
