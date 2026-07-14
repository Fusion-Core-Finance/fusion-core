//! Preference countability: the predicate that decides whether one position's
//! validator direction counts in the current epoch.
//!
//! The nonce clause is what prevents fungible-share vote reuse: any collateral change
//! (deposit, withdrawal, redemption, liquidation, realized redistribution) bumps the
//! position's ink nonce, desynchronizing the preference until the owner resyncs it —
//! and a resync only becomes eligible NEXT epoch, so the same shares can never direct
//! stake twice in one epoch through different positions. The last-counted clause
//! enforces one count per epoch per preference. Losing countability affects direction
//! only — never funds, solvency operations or fuSOL rewards.

/// A 32-byte on-chain address (mint, owner). Kept as raw bytes so this crate stays
/// dependency-free; the controller compares Pubkeys byte-wise through it.
pub type Address = [u8; 32];

/// The controller-owned preference record for one Fusion position (one position,
/// one validator; the PDA seed includes the position address, so duplicates cannot
/// exist on-chain).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreferenceView {
    /// Position owner at the last (re)sync.
    pub owner: Address,
    /// `Position.ink_nonce` observed at the last (re)sync.
    pub observed_ink_nonce: u64,
    /// First epoch this preference may count (set to sync-epoch + 1 on every
    /// nonce or validator change).
    pub eligible_from_epoch: u64,
    /// Last epoch this preference was counted (one count per epoch).
    pub last_counted_epoch: u64,
}

/// The Fusion position fields the countability check reads. `ink` is the collateral
/// amount the snapshot adds to the chosen validator's directed shares when countable
/// (the predicate itself does not depend on it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionView {
    pub owner: Address,
    pub collateral_mint: Address,
    /// Recorded collateral (fuSOL shares) — the weight a countable snapshot adds.
    pub ink: u64,
    /// Monotonic counter bumped on every recorded-collateral change.
    pub ink_nonce: u64,
}

/// Is this preference countable for `current_epoch`? All five clauses must hold:
///
/// 1. the position's collateral is fuSOL,
/// 2. the position owner still matches the preference owner,
/// 3. the collateral ink nonce is unchanged since the last sync,
/// 4. the eligibility delay has passed (`current_epoch >= eligible_from_epoch`),
/// 5. the preference was not already counted this epoch.
#[inline]
pub fn countable(
    pref: &PreferenceView,
    pos: &PositionView,
    fusol_mint: &Address,
    current_epoch: u64,
) -> bool {
    pos.collateral_mint == *fusol_mint
        && pos.owner == pref.owner
        && pos.ink_nonce == pref.observed_ink_nonce
        && current_epoch >= pref.eligible_from_epoch
        && pref.last_counted_epoch != current_epoch
}

#[cfg(test)]
mod tests {
    use super::*;

    const FUSOL: Address = [7u8; 32];
    const OWNER: Address = [1u8; 32];

    fn base_pref() -> PreferenceView {
        PreferenceView {
            owner: OWNER,
            observed_ink_nonce: 42,
            eligible_from_epoch: 100,
            last_counted_epoch: 99,
        }
    }

    fn base_pos() -> PositionView {
        PositionView { owner: OWNER, collateral_mint: FUSOL, ink: 1_000, ink_nonce: 42 }
    }

    #[test]
    fn countable_when_all_five_clauses_hold() {
        assert!(countable(&base_pref(), &base_pos(), &FUSOL, 100));
    }

    #[test]
    fn wrong_mint_not_countable() {
        let pos = PositionView { collateral_mint: [8u8; 32], ..base_pos() };
        assert!(!countable(&base_pref(), &pos, &FUSOL, 100));
    }

    #[test]
    fn owner_mismatch_not_countable() {
        let pos = PositionView { owner: [2u8; 32], ..base_pos() };
        assert!(!countable(&base_pref(), &pos, &FUSOL, 100));
    }

    #[test]
    fn nonce_mismatch_gives_zero_influence() {
        // Any collateral change (deposit/withdraw/redeem/liquidate/redistribute)
        // bumps the nonce and invalidates the preference until resync.
        let pos = PositionView { ink_nonce: 43, ..base_pos() };
        assert!(!countable(&base_pref(), &pos, &FUSOL, 100));
    }

    #[test]
    fn changes_delayed_one_epoch() {
        // eligible_from_epoch = 100: not countable at 99, countable from 100 on.
        assert!(!countable(&base_pref(), &base_pos(), &FUSOL, 99));
        assert!(countable(&base_pref(), &base_pos(), &FUSOL, 100));
        assert!(countable(&base_pref(), &base_pos(), &FUSOL, 101));
    }

    #[test]
    fn one_count_per_epoch() {
        // Counting at epoch 100 records last_counted_epoch = 100: a second snapshot
        // in the same epoch is not countable; the next epoch it is again.
        let counted = PreferenceView { last_counted_epoch: 100, ..base_pref() };
        assert!(!countable(&counted, &base_pos(), &FUSOL, 100));
        assert!(countable(&counted, &base_pos(), &FUSOL, 101));
    }

    /// Exhaustive over all 2^5 clause combinations: countable iff ALL five hold.
    #[test]
    fn exactly_the_conjunction_of_five_clauses() {
        for bits in 0u8..32 {
            let mint_ok = bits & 1 != 0;
            let owner_ok = bits & 2 != 0;
            let nonce_ok = bits & 4 != 0;
            let delay_ok = bits & 8 != 0;
            let uncounted = bits & 16 != 0;
            let pref = PreferenceView {
                owner: OWNER,
                observed_ink_nonce: 42,
                eligible_from_epoch: if delay_ok { 100 } else { 101 },
                last_counted_epoch: if uncounted { 99 } else { 100 },
            };
            let pos = PositionView {
                owner: if owner_ok { OWNER } else { [2u8; 32] },
                collateral_mint: if mint_ok { FUSOL } else { [8u8; 32] },
                ink: 1_000,
                ink_nonce: if nonce_ok { 42 } else { 43 },
            };
            let expect = mint_ok && owner_ok && nonce_ok && delay_ok && uncounted;
            assert_eq!(countable(&pref, &pos, &FUSOL, 100), expect, "bits={bits:05b}");
        }
    }
}
