use anchor_lang::prelude::*;

/// The system-wide **Global Backstop Reserve** — bounded shared SECOND-LOSS capital (one per protocol).
/// PDA `[b"backstop"]`.
///
/// A protocol-owned **fUSD reserve** funded by a minority cut of every market's realized interest
/// (`refresh_market`), drawn automatically in the liquidation waterfall as tier 3.5 — AFTER a market's
/// own local insurance buffer (first-loss) is exhausted, BEFORE un-homed bad debt — up to a per-market
/// hybrid draw cap. It mutualizes only the narrow tail where a contained local failure would otherwise
/// surface as a system-wide backing shortfall (confidence contagion), WITHOUT coupling normal market
/// operations: it never moves user collateral between pools, and draws are rule-based (never a
/// discretionary governance allocation).
///
/// Its fUSD balance is protocol-owned and excluded from any position/market backing or health
/// computation — consumed only via the balanced loss-absorption flow in `liquidate`. Param tuning runs
/// through the TIMELOCKED `GlobalParam` flow; every param ships 0/off.
#[account]
#[derive(Debug)]
pub struct GlobalBackstopReserve {
    /// The fUSD reserve vault (authority = this PDA). Its token balance IS the reserve balance.
    pub fusd_vault: Pubkey,

    // --- governable params (timelocked `GlobalParam`; all clamped; 0 = off) ---
    /// Cut of post-keeper realized interest routed here from each market (bps). 0 = unfunded.
    pub cut_bps: u16,
    /// Reserve-level cap (v1 ABSOLUTE fUSD). Above it, the funding cut reverts to the local buffer.
    pub reserve_cap: u64,
    /// Per-market draw base allowance (fUSD) — access independent of contribution.
    pub draw_base_allowance: u64,
    /// Per-market contribution multiplier (bps): + `draw_k_bps/10_000 · Market.global_contributed`.
    pub draw_k_bps: u64,
    /// A single draw may take at most this fraction (bps) of the LIVE reserve balance.
    pub draw_ceiling_share_bps: u16,
    /// Cumulative draws for a market may not exceed this fraction (bps) of its own debt.
    pub draw_debt_share_bps: u16,

    // --- cumulative accounting (observability + the reserve-solvency invariant) ---
    /// Σ inflows: interest cuts (across all markets) + gov top-ups. `vault == total_contributed −
    /// total_absorbed − total_withdrawn` (the reserve-solvency invariant).
    pub total_contributed: u128,
    /// Σ debt the reserve has absorbed (burned its fUSD against) across tier-3.5 draws.
    pub total_absorbed: u128,
    /// Σ above-cap excess withdrawn by governance.
    pub total_withdrawn: u128,

    pub bump: u8,
    /// Forward-compat reserve (e.g. the debt-relative-cap snapshot pointer fast-follow). Carve new
    /// fields from the HEAD; old accounts' zeroed bytes decode as the `0 = off` sentinel.
    pub _reserved: [u8; 64],
}

impl GlobalBackstopReserve {
    pub const SPACE: usize = 8      // discriminator
        + 32                        // fusd_vault
        + 2 + 8 + 8 + 8 + 2 + 2     // cut_bps, reserve_cap, draw_base, draw_k_bps, ceiling_share, debt_share
        + 16 + 16 + 16              // total_contributed, total_absorbed, total_withdrawn
        + 1                         // bump
        + 64; // reserved
}
