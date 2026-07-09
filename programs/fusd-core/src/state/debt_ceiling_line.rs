use anchor_lang::prelude::*;

/// Per-market **debt-ceiling auto-line** (Maker DC-IAM analog). PDA `[b"ratelimit", collateral_mint]`.
/// OPT-IN and default-absent: a market with no `DebtCeilingLine` account uses its static
/// `Market.debt_ceiling` exactly as before (the hot `borrow` path is unchanged — it always reads
/// `Market.debt_ceiling`, never this account).
///
/// When present, the permissionless `bump_debt_ceiling` crank moves `Market.debt_ceiling` toward
/// `MIN(line, agg_recorded_debt + gap)` — the effective ceiling "follows" utilization up by `gap`
/// steps (capped at the hard `line`), no more often than every `ttl` seconds. Governance sets
/// `line`/`gap`/`ttl` (gov_authority-gated); the permissionless crank can only ever move the live
/// ceiling WITHIN `[debt, line]`, never past the gov-set `line` — so opening the crank to anyone
/// adds no authority over the cap.
///
/// INTERACTION WITH THE `DebtCeiling` PARAM (audit #10): on a market that HAS an auto-line, govern the
/// ceiling via `set_debt_ceiling_line` (the hard `line`), NOT the timelocked `DebtCeiling`
/// `MarketParam`. Both write `Market.debt_ceiling`, but the next permissionless `bump` re-derives it
/// from `MIN(line, debt + gap)` — so a `DebtCeiling` param change is transient (overridden by the next
/// bump). It is BOUNDED, not an escalation: a bump can never raise the ceiling above the gov-set
/// `line`. Pinned by `litesvm_debt_ceiling_line::auto_line_bump_overrides_a_debt_ceiling_param_change`.
#[account]
#[derive(Debug)]
pub struct DebtCeilingLine {
    pub collateral_mint: Pubkey,
    /// The HARD maximum debt ceiling (fUSD-native) the auto-line may ever raise `Market.debt_ceiling`
    /// to. Gov-set; the permissionless crank never exceeds it. (Unclamped, like `Market.debt_ceiling`.)
    pub line: u64,
    /// The maximum step (fUSD-native) a single bump may raise the effective ceiling above current
    /// utilization: `new = min(line, agg_recorded_debt + gap)`. `0` freezes growth at current debt.
    pub gap: u64,
    /// Minimum seconds between bumps (the DC-IAM throttle). `0` = bump anytime.
    pub ttl: i64,
    /// Unix-ts of the last bump (the throttle anchor).
    pub last_bump_ts: i64,
    pub bump: u8,
    /// Forward-compat reserve. Carve new fields from the HEAD; zeroed bytes decode as the field's
    /// `0 = disabled/none` sentinel.
    pub _reserved: [u8; 32],
}

impl DebtCeilingLine {
    pub const SPACE: usize = 8      // discriminator
        + 32                        // collateral_mint
        + 8 + 8                     // line, gap
        + 8 + 8                     // ttl, last_bump_ts
        + 1                         // bump
        + 32; // reserved
}
