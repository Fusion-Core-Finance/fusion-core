use anchor_lang::prelude::*;

/// Per-market **insurance buffer** — protocol first-loss capital, the third liquidation
/// loss-absorption tier (RP → redistribution → **buffer** → un-homed; fusion-docs.md). It is a
/// pure **fUSD reserve**: on a liquidation the RP and other positions can't fully absorb, the buffer
/// **burns its own fUSD** to extinguish the remaining debt (the matching seized collateral stays in the
/// market vault as protocol-owned backing). Funded from realized fees only (no treasury seed);
/// it may run empty, in which case the waterfall falls straight to the terminal `unhomed` → `shutdown`.
///
/// The buffer's fUSD balance is **protocol-owned and excluded from any position/market backing or
/// health computation** — it is consumed only via the balanced loss-absorption flow in `liquidate`
/// (stock-reconciliation discipline). One per market; authority of its own fUSD vault.
#[account]
pub struct InsuranceBuffer {
    /// The market (collateral mint) this buffer backs.
    pub collateral_mint: Pubkey,
    /// The fUSD reserve vault (authority = this buffer PDA). Its token balance IS the buffer balance.
    pub fusd_vault: Pubkey,
    /// Cumulative fUSD funded into the buffer (observability / proof-of-reserves).
    pub total_funded: u128,
    /// Cumulative debt (fUSD) the buffer has absorbed (burned) across liquidations (observability).
    pub total_absorbed: u128,
    pub bump: u8,
    pub _reserved: [u8; 64],
}

impl InsuranceBuffer {
    pub const SPACE: usize = 8 + 32 + 32 + 16 + 16 + 1 + 64;
}
