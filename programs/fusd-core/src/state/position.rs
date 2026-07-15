use anchor_lang::prelude::*;

/// A user's CDP (trove). PDA `[b"position", collateral_mint, owner]`.
///
/// Only the owner's transactions write it, so all distinct users' borrow/repay/adjust
/// operations run in parallel under Sealevel (fusion-docs.md).
#[account]
#[derive(Debug)]
pub struct Position {
    pub owner: Pubkey,
    pub collateral_mint: Pubkey,
    /// Locked collateral, native units.
    pub ink: u64,
    /// Recorded (present-value) debt in fUSD-native units, as of `last_debt_update` (BOLD
    /// `Troves[id].debt`). Interest accrued since then is folded in on the next touch
    /// (`accrual::realize`); this is the actual debt — no `art*rate` normalization.
    pub recorded_debt: u128,
    /// Borrower-chosen **annual interest rate** (bps), the real accrual rate (Liquity v2 user-set rates).
    /// Validated to `[MIN_USER_RATE_BPS, MAX_USER_RATE_BPS]` at borrow/`adjust_rate`. Also the redemption
    /// rate-bucket key — interest accrual never changes it, so redemption ordering is stable.
    pub user_rate_bps: u16,
    pub bump: u8,
    /// Unix timestamp of this position's last debt realization (BOLD `lastDebtUpdateTime`); the start of
    /// the interval over which `accrual::realize` charges `recorded_debt · user_rate_bps · dt` of interest.
    pub last_debt_update: i64,

    // --- liquidation tier-2 redistribution (fusion-docs.md) ---
    /// Collateral stake = `ink * total_stakes_snapshot / total_collateral_snapshot` (Liquity
    /// `_computeNewStake`). Weights this position's share of redistributed debt + collateral.
    pub stake: u128,
    /// Snapshots of the market's `l_coll`/`l_art` at this position's last touch (Liquity
    /// `rewardSnapshots`); pending gains accrue from here.
    pub redist_l_coll_snapshot: u128,
    pub redist_l_art_snapshot: u128,

    /// SOL liquidation bond actually posted (lamports), held on this account on top of rent. Fixed
    /// at open from the market's then-current `reserve_lamports`, so a later governance change can't
    /// retroactively alter it. Paid to the liquidator on liquidation (then 0); refunded on close.
    pub reserve_lamports: u64,

    /// Redemption rate-bucket this position is currently counted in — valid iff `recorded_debt > 0`
    /// (the position joins a bucket when it first takes on debt and leaves when debt hits 0). Stored
    /// explicitly (not re-derived) so a later `bucket_width_bps` change can't mis-target the
    /// decrement of the bucket it actually joined.
    pub bucket: u16,

    /// Collateral (native units) returned to this owner by the liquidation **bonus collar** — the
    /// surplus above `debt · (1 + liq_bonus_bps)` that a liquidation did NOT seize (fusion-docs.md).
    /// Held in the market collateral vault (NOT in `ink`, NOT staked — so a liquidated owner can't
    /// inherit redistributed debt on the leftover) and withdrawn via `claim_coll_surplus`. The market
    /// vault invariant is `vault == total_collateral + surplus_collateral + total_coll_surplus +
    /// protocol_collateral`, where `Market.total_coll_surplus == Σ Position.coll_surplus` aggregates
    /// this field.
    pub coll_surplus: u64,

    /// Unix timestamp of this position's last interest-rate change (set at `open_position`, updated on
    /// each `adjust_rate`). Drives the premature-rate-change cooldown/fee (BOLD anti-gaming):
    /// an `adjust_rate` within `Market.rate_adjust_cooldown_secs` of this time is charged an upfront fee.
    pub last_rate_adjust_ts: i64,

    /// Monotonic collateral-change nonce: increments whenever `ink` CHANGES for any reason
    /// (deposit, withdrawal, redemption, liquidation, realized redistribution fold). Carved from
    /// the HEAD of `_reserved` (zeroed bytes on pre-carve accounts decode as 0 = "never changed",
    /// the correct sentinel). Purely informational to fusd-core — no solvency/debt path reads it.
    /// Consumed read-only by the fuSOL stake-pool Allocation Controller's Preference sync to
    /// prevent fungible-share validator-direction reuse. Every `ink` mutation MUST go through
    /// [`Position::set_ink`] so the nonce can never silently miss a collateral change.
    pub ink_nonce: u64,

    /// Forward-compat reserve. WIDENED 6 → 32 bytes pre-launch; 32 → 24 on the `ink_nonce` carve.
    /// Keeps the carve-from-`_reserved` additive-upgrade path alive through the upgradeable
    /// Phases 1–3 (post-launch a Borsh account cannot grow without realloc). Carve from the HEAD;
    /// zeroed bytes on old accounts must decode as the new field's `0 = disabled/none` sentinel.
    pub _reserved: [u8; 24],
}

impl Position {
    pub const SPACE: usize = 8  // discriminator
        + 32 + 32               // owner, collateral_mint
        + 8 + 16                // ink, recorded_debt
        + 2 + 1                 // user_rate_bps, bump
        + 8                     // last_debt_update
        + 16 * 3                // stake, redist_l_coll_snapshot, redist_l_art_snapshot
        + 8                     // reserve_lamports
        + 2                     // bucket
        + 8                     // coll_surplus
        + 8                     // last_rate_adjust_ts
        + 8                     // ink_nonce (carved from _reserved)
        + 24; // reserved (widened pre-launch, minus the ink_nonce carve)

    /// The ONLY sanctioned way to change `ink`: writes the new value and bumps `ink_nonce` iff
    /// the value actually changed (a no-op write — e.g. re-zeroing an already-drained zombie in
    /// `liquidate` — is not a collateral change and must not bump). `saturating_add` keeps the
    /// documented invariant literal — the nonce is MONOTONICALLY INCREASING, never wraps back
    /// below a previously-observed value — while being behaviorally unreachable either way: 2^64
    /// increments are physically impossible, and even a hypothetically saturated nonce only fails
    /// toward "stale preference stays synced" in the direction layer — never funds or solvency.
    pub fn set_ink(&mut self, new_ink: u64) {
        if new_ink != self.ink {
            self.ink = new_ink;
            self.ink_nonce = self.ink_nonce.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `set_ink` bumps the nonce exactly on value CHANGES — a no-op write (re-zeroing an
    /// already-drained zombie in `liquidate`, or writing the current value back) is not a
    /// collateral change and must not bump (the nonce tracks "whenever ink CHANGES").
    #[test]
    fn set_ink_bumps_only_on_change() {
        // All-zero bit pattern is a valid Position (ints/Pubkeys/bool-free) — same technique as
        // the state/mod.rs layout tests.
        let mut p: Position = unsafe { std::mem::zeroed() };
        assert_eq!((p.ink, p.ink_nonce), (0, 0));
        p.set_ink(0); // no-op write on a fresh position
        assert_eq!(p.ink_nonce, 0);
        p.set_ink(5);
        assert_eq!((p.ink, p.ink_nonce), (5, 1));
        p.set_ink(5); // same value — not a collateral change
        assert_eq!(p.ink_nonce, 1);
        p.set_ink(0);
        assert_eq!((p.ink, p.ink_nonce), (0, 2));
        p.set_ink(0); // zombie re-zero
        assert_eq!(p.ink_nonce, 2);
    }
}
