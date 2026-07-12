//! Liquidation **tier-2 redistribution** accounting — the Liquity stake-based reward-per-unit
//! algorithm, adapted to fUSD's normalized (`art`) debt. When the Reactor Pool can't fully
//! absorb a liquidation (fusion-docs.md tier 1), the uncovered debt **and** its collateral are
//! spread across the market's remaining positions in **O(1)** — two market-level accumulators per
//! liquidation, applied **lazily** to each position the next time it's touched. Liquidations never
//! stall on pool size. fusion-docs.md; precedent: Liquity `TroveManager` (`L_ETH` /
//! `L_LUSDDebt`, `rewardSnapshots`, stake/`totalStakes` machinery).
//!
//! ## Model
//! - Two cumulative **reward-per-unit-staked** accumulators, `l_coll` and `l_art` (1e18-scaled):
//!   each redistribution adds `redistributed * 1e18 / total_stakes` to them, carrying the
//!   floor-division residual in `last_coll_error` / `last_art_error` (Liquity's
//!   `lastETHError_Redistribution` / `lastLUSDDebtError_Redistribution`) so repeated small
//!   liquidations don't drift.
//! - A position holds a **`stake`** and a `{l_coll, l_art}` snapshot. Its pending gains are
//!   `stake * (L_now - L_snapshot) / 1e18`. On touch they fold into the recorded `ink`/`art` and
//!   the snapshot rolls forward.
//! - **`stake = coll * total_stakes_snapshot / total_collateral_snapshot`** (snapshots captured
//!   after each liquidation; `stake = coll` before the first). This keeps `Σ stake == total_stakes`
//!   exact as redistribution grows positions' collateral, so reward-per-unit never over- or
//!   under-distributes (Liquity `_computeNewStake`). `art` is redistributed normalized (consistent
//!   with the single per-market `rate` accumulator), weighted by the collateral `stake`.
//!
//! ## Overflow / migration (BOLD-sweep C4)
//! `l_coll`/`l_art` grow monotonically; like the Reactor-Pool grid they **revert on `u128`
//! overflow** (`AccumulatorOverflow`) rather than wrap (a migration trigger, astronomically unlikely
//! for realistic sizes) — never a silent loss of a position's unrealized gain. The per-unit numerator
//! `amount · 1e18 + last_error` is computed in **256-bit** (`U256` / the `wide_muladd_div` shim under
//! Kani), so the `· 1e18` never overflows; the only `u128` edges are the per-unit quotient itself
//! (`AccumulatorOverflow` when `amount/total_stakes` exceeds ~`u128::MAX / 1e18 ≈ 3.40e20`) and the
//! cumulative `l += rpu` add. Redistributed `coll`/`art` are bounded by the liquidated position's own
//! collateral/debt (≤ market supply), so for any plausible per-market supply both edges sit ≥4 orders of
//! magnitude below the cap (see `reactor_pool`'s envelope note). Pinned by
//! `redistribute_reverts_not_wraps_beyond_envelope`.

use crate::mul_div_floor;
// `bnum` backs the production `accumulate` divide; under `cfg(kani)` that path is swapped for the
// `wide_muladd_div` shim, so the import would be unused there.
#[cfg(not(kani))]
use bnum::types::U256;

/// 1e18 reward-per-unit precision (matches the Reactor Pool's `DECIMAL_PRECISION`).
pub const PRECISION: u128 = 1_000_000_000_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedistError {
    /// Redistribution attempted with no staked positions to receive it (caller must ensure at
    /// least one other position exists, or route to the surplus buffer).
    NoStakes,
    /// An accumulator would exceed `u128` — a migration trigger, never a wrap.
    AccumulatorOverflow,
    /// Arithmetic overflow (should not occur within realistic sizes).
    Math,
}

/// Market-level redistribution accumulators (mirror Liquity `L_ETH`/`L_LUSDDebt` + the error
/// feedback terms). `Default` is the genesis state (all zero).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RedistState {
    pub l_coll: u128,
    pub l_art: u128,
    pub last_coll_error: u128,
    pub last_art_error: u128,
}

/// A position's snapshot of the accumulators at its last touch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RedistSnapshot {
    pub l_coll: u128,
    pub l_art: u128,
}

impl RedistState {
    pub fn new() -> Self {
        Self::default()
    }

    /// The current snapshot point (what a freshly-touched position records).
    pub fn snapshot(&self) -> RedistSnapshot {
        RedistSnapshot { l_coll: self.l_coll, l_art: self.l_art }
    }
}

/// Spread `coll` collateral (native units) and `art` normalized-debt across all positions holding
/// stake, by bumping the per-unit-staked accumulators with error feedback. `total_stakes` is the
/// sum of the receiving positions' stakes and MUST be `> 0` (the liquidated position's stake is
/// excluded by the caller before calling this). Mirrors Liquity `_redistributeDebtAndColl`.
pub fn redistribute(
    st: &mut RedistState,
    total_stakes: u128,
    coll: u128,
    art: u128,
) -> Result<(), RedistError> {
    if total_stakes == 0 {
        return Err(RedistError::NoStakes);
    }
    st.l_coll = accumulate(st.l_coll, &mut st.last_coll_error, total_stakes, coll)?;
    st.l_art = accumulate(st.l_art, &mut st.last_art_error, total_stakes, art)?;
    Ok(())
}

/// `l += (amount * 1e18 + carried_error) / total_stakes`, carrying the new residual. 256-bit
/// intermediate so the `* 1e18` never overflows.
fn accumulate(
    l: u128,
    last_error: &mut u128,
    total_stakes: u128,
    amount: u128,
) -> Result<u128, RedistError> {
    // `(reward_per_unit, remainder)` of `(amount*PRECISION + *last_error) / total_stakes`. Two
    // implementations, identical in result (pinned by `muladd_div_matches_bnum`):
    //   production (`cfg(not(kani))`): exact `bnum` U256 — what ships.
    //   `cfg(kani)`: the `wide_muladd_div` shim — `bnum`'s 256-bit long division is intractable for
    //   CBMC even at a few symbolic bits; the shim's flat loop is not. `total_stakes > 0` is the
    //   caller (`redistribute`) invariant, so the only reachable `None`/overflow is `rpu > u128::MAX`.
    #[cfg(not(kani))]
    let (rpu, rem) = {
        let numerator = U256::from(amount)
            .checked_mul(U256::from(PRECISION))
            .and_then(|x| x.checked_add(U256::from(*last_error)))
            .ok_or(RedistError::Math)?;
        let ts = U256::from(total_stakes);
        let reward_per_unit = numerator / ts;
        let rem = u128::try_from(numerator - reward_per_unit * ts).map_err(|_| RedistError::Math)?;
        let rpu = u128::try_from(reward_per_unit).map_err(|_| RedistError::AccumulatorOverflow)?;
        (rpu, rem)
    };
    #[cfg(kani)]
    let (rpu, rem) = crate::wide_muladd_div(amount, PRECISION, *last_error, total_stakes)
        .ok_or(RedistError::AccumulatorOverflow)?;

    *last_error = rem;
    l.checked_add(rpu).ok_or(RedistError::AccumulatorOverflow)
}

/// A position's pending redistribution gains since its snapshot: `(collateral, normalized_debt)`,
/// each `stake * (L_now - L_snapshot) / 1e18`, floored. Mirrors Liquity `getPendingETHReward` /
/// `getPendingLUSDDebtReward`.
pub fn pending(
    stake: u128,
    st: &RedistState,
    snap: &RedistSnapshot,
) -> Result<(u128, u128), RedistError> {
    Ok((
        pending_one(stake, st.l_coll, snap.l_coll)?,
        pending_one(stake, st.l_art, snap.l_art)?,
    ))
}

fn pending_one(stake: u128, l_now: u128, l_snap: u128) -> Result<u128, RedistError> {
    let delta = l_now.saturating_sub(l_snap);
    if stake == 0 || delta == 0 {
        return Ok(0);
    }
    mul_div_floor(stake, delta, PRECISION).ok_or(RedistError::Math)
}

/// A position's stake from its collateral and the system snapshot captured at the last
/// liquidation: `coll * total_stakes_snapshot / total_collateral_snapshot`, or `coll` before the
/// first liquidation (`total_collateral_snapshot == 0`). Mirrors Liquity `_computeNewStake`.
pub fn compute_stake(
    coll: u128,
    total_stakes_snapshot: u128,
    total_collateral_snapshot: u128,
) -> Result<u128, RedistError> {
    if total_collateral_snapshot == 0 {
        Ok(coll)
    } else {
        mul_div_floor(coll, total_stakes_snapshot, total_collateral_snapshot)
            .ok_or(RedistError::Math)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn fresh_state_is_zero() {
        let st = RedistState::new();
        assert_eq!(st, RedistState::default());
        assert_eq!(st.snapshot(), RedistSnapshot::default());
    }

    #[test]
    fn single_recipient_gets_all() {
        // One receiving position with stake 100; redistribute 10 coll + 40 art.
        let mut st = RedistState::new();
        let snap = st.snapshot();
        redistribute(&mut st, 100, 10, 40).unwrap();
        let (coll, art) = pending(100, &st, &snap).unwrap();
        assert_eq!(coll, 10);
        assert_eq!(art, 40);
    }

    #[test]
    fn two_recipients_share_pro_rata_by_stake() {
        // Stakes 60 + 40 = 100; redistribute 10 coll + 50 art.
        let mut st = RedistState::new();
        let snap = st.snapshot();
        redistribute(&mut st, 100, 10, 50).unwrap();

        let (ca, aa) = pending(60, &st, &snap).unwrap();
        let (cb, ab) = pending(40, &st, &snap).unwrap();
        assert_eq!((ca, aa), (6, 30));
        assert_eq!((cb, ab), (4, 20));
        // conservation (exact here — 100 divides evenly)
        assert_eq!(ca + cb, 10);
        assert_eq!(aa + ab, 50);
    }

    #[test]
    fn rejects_redistribution_with_no_stakes() {
        let mut st = RedistState::new();
        assert_eq!(redistribute(&mut st, 0, 1, 1), Err(RedistError::NoStakes));
    }

    /// BOLD-sweep C4: redistribution beyond the u128 envelope REVERTS (`AccumulatorOverflow`), never
    /// wraps — both the per-unit quotient edge and the cumulative-accumulator add.
    #[test]
    fn redistribute_reverts_not_wraps_beyond_envelope() {
        // `amount · 1e18 / total_stakes` exceeding u128 (huge amount, tiny stakes) -> AccumulatorOverflow.
        let mut st = RedistState::new();
        assert_eq!(redistribute(&mut st, 1, u128::MAX, 0), Err(RedistError::AccumulatorOverflow));
        // The cumulative accumulator overflowing on the `l += rpu` add -> AccumulatorOverflow.
        let mut st = RedistState::new();
        st.l_coll = u128::MAX - 5;
        assert_eq!(redistribute(&mut st, 1, 10, 0), Err(RedistError::AccumulatorOverflow));
    }

    #[test]
    fn snapshot_only_counts_gains_after_it() {
        // A position that snapshots AFTER a redistribution earns nothing from it.
        let mut st = RedistState::new();
        redistribute(&mut st, 100, 10, 40).unwrap();
        let late_snap = st.snapshot();
        let (coll, art) = pending(100, &st, &late_snap).unwrap();
        assert_eq!((coll, art), (0, 0));
        // ...but a further redistribution does reach it.
        redistribute(&mut st, 100, 5, 5).unwrap();
        let (coll, art) = pending(100, &st, &late_snap).unwrap();
        assert_eq!((coll, art), (5, 5));
    }

    #[test]
    fn error_feedback_no_drift_over_many_redistributions() {
        // 1000 tiny redistributions of (3 coll, 1 art) over stake 1e6 should reach the sole
        // recipient with no systematic drift, thanks to the carried error.
        let mut st = RedistState::new();
        let snap = st.snapshot();
        let stake = 1_000_000u128;
        for _ in 0..1000 {
            redistribute(&mut st, stake, 3, 1).unwrap();
        }
        let (coll, art) = pending(stake, &st, &snap).unwrap();
        assert!((3000 - 2..=3000).contains(&coll), "coll no drift: {coll}");
        assert!((1000 - 2..=1000).contains(&art), "art no drift: {art}");
    }

    #[test]
    fn compute_stake_identity_before_first_liquidation() {
        // total_collateral_snapshot == 0 -> stake == coll.
        assert_eq!(compute_stake(123, 0, 0).unwrap(), 123);
        assert_eq!(compute_stake(123, 999, 0).unwrap(), 123);
    }

    #[test]
    fn compute_stake_downscales_after_redistribution_grew_collateral() {
        // After a liquidation, total_collateral grew (redistributed coll) while total_stakes did
        // not, so the snapshot ratio < 1: a new deposit's stake is below its raw collateral, which
        // is what keeps Σ stake == total_stakes exact.
        let total_stakes_snapshot = 100;
        let total_collateral_snapshot = 110; // 10 units of redistributed coll added on top
        let stake = compute_stake(110, total_stakes_snapshot, total_collateral_snapshot).unwrap();
        assert_eq!(stake, 100); // 110 * 100 / 110
        let smaller = compute_stake(11, total_stakes_snapshot, total_collateral_snapshot).unwrap();
        assert_eq!(smaller, 10); // 11 * 100 / 110
    }

    /// End-to-end conservation through the stake machinery: two positions, a redistribution, lazy
    /// application, then a second redistribution — the receiving positions' applied collateral and
    /// debt always sum (minus floor dust) to what was redistributed, and `Σ stake == total_stakes`.
    #[test]
    fn end_to_end_two_positions_conserve_and_keep_total_stakes() {
        let mut st = RedistState::new();

        // Genesis: A has 60 coll, B has 40 coll. No prior liquidation -> stake == coll.
        let mut a_coll = 60u128;
        let mut b_coll = 40u128;
        let mut a_stake = compute_stake(a_coll, 0, 0).unwrap();
        let mut b_stake = compute_stake(b_coll, 0, 0).unwrap();
        let mut total_stakes = a_stake + b_stake;
        let mut total_collateral = a_coll + b_coll;
        assert_eq!(total_stakes, 100);
        let a_snap = st.snapshot();
        let b_snap = st.snapshot();

        // A liquidated position contributes 30 coll + 90 art to be redistributed across A+B.
        redistribute(&mut st, total_stakes, 30, 90).unwrap();
        total_collateral += 30; // redistributed coll now backs A+B

        // Apply to A (lazy touch): fold pending, roll snapshot, recompute stake from the post-liq
        // system snapshot (total_stakes/total_collateral captured at the liquidation).
        let tss = total_stakes; // snapshot taken right after the liquidation
        let tcs = total_collateral;
        let (a_pc, a_pa) = pending(a_stake, &st, &a_snap).unwrap();
        let mut a_art = 0u128;
        a_coll += a_pc;
        a_art += a_pa;
        // (snapshot would roll to st.snapshot() here; the second redistribution below re-snapshots.)
        let new_a_stake = compute_stake(a_coll, tss, tcs).unwrap();
        total_stakes = total_stakes - a_stake + new_a_stake;
        a_stake = new_a_stake;
        assert_eq!((a_pc, a_pa), (18, 54)); // 60% of (30, 90)

        // Apply to B.
        let (b_pc, b_pa) = pending(b_stake, &st, &b_snap).unwrap();
        let mut b_art = 0u128;
        b_coll += b_pc;
        b_art += b_pa;
        let new_b_stake = compute_stake(b_coll, tss, tcs).unwrap();
        total_stakes = total_stakes - b_stake + new_b_stake;
        b_stake = new_b_stake;
        assert_eq!((b_pc, b_pa), (12, 36)); // 40% of (30, 90)

        // Conservation after full application.
        assert_eq!(a_coll + b_coll, total_collateral, "all collateral accounted");
        assert_eq!(a_art + b_art, 90, "all redistributed debt accounted");
        // Σ stake == total_stakes (the invariant the stake formula preserves).
        assert_eq!(a_stake + b_stake, total_stakes);

        // A second redistribution still splits by the (now updated) stakes without drift.
        let before = st.snapshot();
        redistribute(&mut st, total_stakes, total_stakes /* 1 coll per unit */, 0).unwrap();
        let (a_pc2, _) = pending(a_stake, &st, &before).unwrap();
        let (b_pc2, _) = pending(b_stake, &st, &before).unwrap();
        assert!(a_pc2 + b_pc2 <= total_stakes && total_stakes - (a_pc2 + b_pc2) <= 2);
    }

    // --- proptest fuzz (B8): conservation (floor never over-distributes), no-drift error feedback,
    // and compute_stake bounds, over WIDE random inputs — the SAME properties the Kani harnesses
    // (redistribute_*_conserves, accumulate_is_proper_division, compute_stake_never_over_stakes) prove
    // on tiny pinned domains. Amounts/stakes are bounded so each `amount·1e18` per-unit fits a u128
    // accumulator (the documented realistic-size regime; out-of-contract sizes revert, not wrap).

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        // CONSERVATION over N recipients whose stakes sum to total_stakes: Σ pending <= amount
        // (floor rounding always favors the system, never over-distributes) and the shortfall is at
        // most one dust unit per recipient. Reference is a DIRECT sum of independent per-stake floors.
        #[test]
        fn redistribute_conserves_over_recipients(
            stakes in prop::collection::vec(1u128..=1_000_000_000_000, 1..=8),
            coll in 0u128..=1_000_000_000_000_000_000,
            art in 0u128..=1_000_000_000_000_000_000,
        ) {
            let total_stakes: u128 = stakes.iter().sum();
            let mut st = RedistState::new();
            let snap = st.snapshot();
            redistribute(&mut st, total_stakes, coll, art).unwrap();

            let mut sum_coll = 0u128;
            let mut sum_art = 0u128;
            for &s in &stakes {
                let (c, a) = pending(s, &st, &snap).unwrap();
                sum_coll += c;
                sum_art += a;
            }
            let n = stakes.len() as u128;
            // Never over-distribute (floor favors the system).
            prop_assert!(sum_coll <= coll, "coll over-distributed: {} > {}", sum_coll, coll);
            prop_assert!(sum_art <= art, "art over-distributed: {} > {}", sum_art, art);
            // The shortfall (dust) is bounded by one floor per recipient.
            prop_assert!(coll - sum_coll <= n, "coll dust {} > {}", coll - sum_coll, n);
            prop_assert!(art - sum_art <= n, "art dust {} > {}", art - sum_art, n);
        }

        // NoStakes precondition: redistributing with total_stakes == 0 is rejected (never wraps/panics).
        #[test]
        fn redistribute_zero_stakes_rejected(coll in any::<u128>(), art in any::<u128>()) {
            let mut st = RedistState::new();
            prop_assert_eq!(redistribute(&mut st, 0, coll, art), Err(RedistError::NoStakes));
        }

        // ERROR-FEEDBACK NO-DRIFT: many tiny redistributions to a sole recipient still sum to within
        // one dust unit of the cumulative redistributed amount — the carried error prevents systematic
        // loss. Independent reference: the exact running total of what was fed in.
        #[test]
        fn error_feedback_no_drift(
            // Discriminating regime: large, NON-round stakes in [1e16, 1e18] (stake == total_stakes,
            // sole recipient) make each round's floor-division residue ~O(stake/1e18) units. WITHOUT the
            // carried-error feedback these residues accumulate to tens/hundreds of units over the rounds
            // (the assertion below would fail); WITH it they stay <= 1. The cap stays at 1e18 because the
            // <= 1 dust bound is only proven for total_stakes <= 1e18 (see kani_proofs.rs ~L343).
            stake in 10_000_000_000_000_003u128..=999_999_999_999_999_989,
            amounts in prop::collection::vec(0u128..=999_999_937, 1..=400),
        ) {
            let mut st = RedistState::new();
            let snap = st.snapshot();
            let total: u128 = amounts.iter().sum();
            for &amt in &amounts {
                redistribute(&mut st, stake, amt, 0).unwrap();
            }
            let (coll, _) = pending(stake, &st, &snap).unwrap();
            // The sole recipient holds ALL the stake: it receives the whole cumulative amount minus at
            // most one dust unit (a single final floor), regardless of how many bumps it took.
            prop_assert!(coll <= total, "drifted ABOVE the fed total: {} > {}", coll, total);
            prop_assert!(total - coll <= 1, "drift {} exceeds one dust unit", total - coll);
        }

        // compute_stake never inflates a stake above the position's collateral when the post-liquidation
        // snapshot ratio is <= 1 (total_stakes_snapshot <= total_collateral_snapshot) — the invariant
        // that keeps Σ stake == total_stakes from growing. Reference: stake <= coll (the bound itself).
        #[test]
        fn compute_stake_never_over_stakes_fuzz(
            coll in 0u128..=1_000_000_000_000_000_000,
            tcs in 1u128..=1_000_000_000_000_000_000,
            ratio_num in 0u128..=1_000_000_000_000_000_000,
        ) {
            let tss = ratio_num.min(tcs); // ensure tss <= tcs (ratio <= 1)
            let stake = compute_stake(coll, tss, tcs).unwrap();
            prop_assert!(stake <= coll, "downscaling inflated stake {} above coll {}", stake, coll);
        }

        // compute_stake genesis identity: before the first liquidation (tcs == 0) stake == coll
        // regardless of the (ignored) total_stakes_snapshot.
        #[test]
        fn compute_stake_genesis_identity(coll in any::<u128>(), tss in any::<u128>()) {
            prop_assert_eq!(compute_stake(coll, tss, 0), Ok(coll));
        }

        // pending is MONOTONIC in stake at a fixed accumulator delta: a larger stake never receives
        // less. (Independent of the production division — a structural ordering property.)
        #[test]
        fn pending_monotonic_in_stake(
            total_stakes in 1u128..=1_000_000_000_000,
            s1 in 1u128..=1_000_000_000_000,
            s2 in 1u128..=1_000_000_000_000,
            coll in 0u128..=1_000_000_000_000_000_000,
        ) {
            let mut st = RedistState::new();
            let snap = st.snapshot();
            redistribute(&mut st, total_stakes, coll, 0).unwrap();
            let (lo, hi) = if s1 <= s2 { (s1, s2) } else { (s2, s1) };
            let (plo, _) = pending(lo, &st, &snap).unwrap();
            let (phi, _) = pending(hi, &st, &snap).unwrap();
            prop_assert!(plo <= phi, "pending not monotonic: stake {} -> {}, stake {} -> {}", lo, plo, hi, phi);
        }
    }
}
