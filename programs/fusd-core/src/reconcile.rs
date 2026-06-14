//! On-chain vault-vs-ledger reconciliation (adapting klend's
//! `lending_checks` pre/post balance verification to Fusion's account model).
//!
//! The 4-term vault invariant
//! `vault == total_collateral + surplus_collateral + total_coll_surplus + protocol_collateral`
//! is specified in fusion-docs.md and exactly (`==`) asserted across the litesvm suite —
//! but until this module no handler enforced it at RUNTIME, so a future bookkeeping bug (most
//! plausibly in `liquidate`'s four-counter multi-branch waterfall, which has several legs with
//! accounting but no token move) would be silent production drift, caught only if a test covers
//! that exact path. For a protocol whose end-state is no-admin-recourse immutability, the only
//! acceptable failure mode is a same-transaction revert.
//!
//! DIRECTION IS LOAD-BEARING: the on-chain check is `vault >= tracked` (sufficiency /
//! proof-of-reserves), NEVER exact equality. The dangerous direction (vault under-funds the
//! ledger — theft, double-pay, over-credit) hard-reverts in the same tx; the protocol-favoring
//! direction (a permissionless donation, an under-credit) passes on-chain and stays caught by
//! the exact `==` litesvm helpers. An absolute on-chain `==` would let a 1-lamport donation to
//! the vault permanently brick liquidation and redemption — the two lifelines the protocol's hard
//! invariants protect — with no admin recourse by design.
//!
//! Deliberately NOT adopted from klend (redundant given Anchor constraint pinning, legacy-SPL-
//! only tokens, and a no-external-CPI program): pre/post three-axis delta snapshots, the
//! LendingAction enum, signed shared-vault netting, post-CPI re-runs, and a named transfer-
//! wrapper refactor. The RP fUSD vault gets no sufficiency assert yet — out of scope until the
//! P/S compounded-deposit rounding direction is proven protocol-favoring.

use anchor_lang::prelude::*;
use anchor_spl::token::TokenAccount;

use crate::errors::FusdError;
use crate::state::Market;

/// Assert the market collateral vault holds at least the 4-term tracked sum. Call as the LAST
/// statement of every handler that moves vault collateral or mutates any of the four counters.
/// `vault.reload()` first — Anchor caches token-account state across the handler's CPIs, so the
/// comparison must read the post-transfer balance.
pub fn assert_collateral_vault_sufficiency(
    vault: &mut Account<TokenAccount>,
    market: &Market,
) -> Result<()> {
    vault.reload()?;
    check_sufficiency(
        vault.amount,
        market.total_collateral,
        market.surplus_collateral,
        market.total_coll_surplus,
        market.protocol_collateral,
    )
}

/// The pure comparison (unit-testable without an SVM): `vault >= Σ tracked`, checked sum.
fn check_sufficiency(
    vault_amount: u64,
    total_collateral: u128,
    surplus_collateral: u64,
    total_coll_surplus: u64,
    protocol_collateral: u64,
) -> Result<()> {
    let tracked = total_collateral
        .checked_add(surplus_collateral as u128)
        .and_then(|t| t.checked_add(total_coll_surplus as u128))
        .and_then(|t| t.checked_add(protocol_collateral as u128))
        .ok_or(FusdError::MathOverflow)?;
    require!((vault_amount as u128) >= tracked, FusdError::VaultReconciliationFailed);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::check_sufficiency;

    #[test]
    fn sufficiency_boundaries() {
        // Exact equality passes (the healthy steady state).
        assert!(check_sufficiency(100, 60, 20, 15, 5).is_ok());
        // Surplus vault balance (a donation) passes — `>=` is the load-bearing direction:
        // a 1-unit donation must never brick the market's flows.
        assert!(check_sufficiency(101, 60, 20, 15, 5).is_ok());
        // The dangerous direction — vault under-funds the ledger — reverts.
        assert!(check_sufficiency(99, 60, 20, 15, 5).is_err());
        // Each counter participates.
        assert!(check_sufficiency(99, 100, 0, 0, 0).is_err());
        assert!(check_sufficiency(99, 0, 100, 0, 0).is_err());
        assert!(check_sufficiency(99, 0, 0, 100, 0).is_err());
        assert!(check_sufficiency(99, 0, 0, 0, 100).is_err());
        // Checked sum: an overflowing tracked total fails closed, never wraps to "sufficient".
        assert!(check_sufficiency(u64::MAX, u128::MAX, u64::MAX, 0, 0).is_err());
    }
}
