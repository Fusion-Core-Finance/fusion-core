use anchor_lang::prelude::*;

/// Global supply-reconciliation (proof-of-reserves) singleton. PDA `[b"supply_recon"]`.
///
/// FUSD's strong global invariant `mint.supply == Σ_market (agg_recorded_debt − unminted_interest +
/// bad_debt)` is **sharded** across the per-market `Market` accounts — it deliberately is NOT a single
/// hot counter (that would serialize every `borrow` behind one write-lock, the failure that killed
/// Maker's `Vat` on Solana). The permissionless `reconcile_supply` crank re-derives the total from the
/// markets and compares it to the live mint supply, stamping the result here for on-chain
/// proof-of-reserves. This is an **auditability layer, not a solvency dependency**: per-vault hard
/// solvency is always enforced inline; a non-zero `last_residual` flags an accounting drift (or a
/// market omitted from the crank's input) for off-chain alarms, and never gates any user path.
#[account]
#[derive(Debug)]
pub struct SupplyReconciliation {
    /// Unix-ts of the last reconciliation run.
    pub last_ts: i64,
    /// How many `Market` accounts the last run summed (the off-chain monitor passes ALL live markets;
    /// a count below the known market set is itself a signal the residual may be a missing market).
    pub last_market_count: u32,
    /// The live fUSD mint supply read at the last run.
    pub last_mint_supply: u128,
    /// The supply reconstructed from the summed markets: `Σ(agg + bad) − Σ unminted` (underflow-safe;
    /// each market's `agg + bad ≥ unminted` since its circulating share is non-negative).
    pub last_backing: u128,
    /// `last_backing − last_mint_supply` (signed). `0` = perfectly reconciled. Positive ⇒ markets
    /// account for MORE than the mint shows (a market double-counted, or an under-mint bug); negative
    /// ⇒ the mint shows MORE than the summed markets back (a market omitted, or an over-mint bug).
    pub last_residual: i128,
    pub bump: u8,
    pub _reserved: [u8; 32],
}

impl SupplyReconciliation {
    pub const SPACE: usize = 8      // discriminator
        + 8                         // last_ts
        + 4                         // last_market_count
        + 16 + 16 + 16              // last_mint_supply, last_backing, last_residual
        + 1                         // bump
        + 32; // reserved
}
