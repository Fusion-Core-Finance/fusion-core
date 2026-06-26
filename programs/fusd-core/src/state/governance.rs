use anchor_lang::prelude::*;

/// Which bounded market parameter a governance change targets — the live, clamped set of
/// post-init tunables, and the payload a timelocked op carries.
///
/// Deliberately excludes the non-retroactivity-sensitive params (the per-position SOL reserve
/// bond and the redemption `bucket_width`): bonds are fixed at open and `Position.bucket` is
/// stored, so those must not be retroactively re-applied to existing positions — they get
/// dedicated, explicitly non-retroactive handling with the registry work.
///
/// **APPEND-ONLY.** The borsh tag is the variant's declaration ordinal, and it
/// is PERSISTED — inside queued `TimelockedParam` accounts and in every emitted
/// `ParamChangeQueued/Executed` event. Removing or reordering a variant would silently RE-KEY
/// every in-flight queued op (a queued `LiqBonus` change executing as `Ccr`) and corrupt
/// historical event decoding. Rules: new variants are appended LAST; a variant is deprecated by
/// leaving it in place as a tombstone whose `validate_param` arm returns
/// `FusdError::ParamOutOfBounds` (so stored ops carrying it can never execute); a tag byte is
/// never reused. The `market_param_tags_are_pinned` test below breaks CI on any violation.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarketParam {
    /// `mcr_bps` — minimum collateral ratio (bps). Clamp: `[MIN_MCR_BPS, MAX_MCR_BPS]` (100%–300%).
    /// The upper bound is constitutional: MCR is read live by `liquidate`, so an unbounded raise
    /// could retroactively expand the liquidatable set (the Recovery-Mode death spiral FUSD's
    /// no-Recovery-Mode rule rejects).
    ///
    /// THREE layers prevent a retroactive liquidation-set expansion: (1) the
    /// compile-time clamp above; (2) the queue→execute timelock (a user exit window — but
    /// `MIN_GOV_TIMELOCK_SECS = 0` is permitted for guarded launch); (3) an executed RAISE arms
    /// `Market.liq_grace_until = max(current, now + MCR_RAISE_GRACE_SLOTS)` so liquidation waits
    /// ~1h regardless of timelock config (the machine-enforced invariant-4 exit window; emits
    /// `McrRaiseGraceArmed`). DISCLOSED trade-off: clamp-legal raise cycling (±1bp with pre-queued
    /// ops) can keep the grace re-armed — a governance-triggered liquidation-suppression channel,
    /// bounded by the grace-free `shutdown`/`urgent_redeem` backstop and smaller than the existing
    /// MCR-floor-lowering power; monitor via the event. Also relationally bound: MCR ≥ SCR and the
    /// collar-fundability/RP-solvency products (`validate_market_config`).
    Mcr,
    /// `debt_ceiling` — fUSD-native debt cap. A ceiling (no upper clamp); 0 pauses new debt.
    DebtCeiling,
    /// `redemption_fee_bps` — flat redemption fee (bps). Clamp: `<= MAX_REDEMPTION_FEE_BPS`.
    RedemptionFee,
    /// `liq_gas_comp_bps` — liquidator collateral gas-comp (bps). Clamp: `<= MAX_LIQ_GAS_COMP_BPS`.
    LiqGasComp,
    /// `rl_cap` — net-outflow rate-limit cap (fUSD-native per window). 0 disables the limiter; a
    /// higher value is the fast loosen-path. No upper clamp (larger ⇒ more permissive).
    RateLimitCap,
    /// `ccr_bps` — CCR borrow-restriction band threshold. 0 disables the band; otherwise clamped to
    /// `[MIN_CCR_BPS, MAX_CCR_BPS]`. Blocks only risk-increasing ops below this aggregate TCR.
    Ccr,
    /// `liq_bonus_bps` — liquidation bonus collar (bps). 0 = collar OFF (seize the whole position);
    /// otherwise clamped to `<= MAX_LIQ_BONUS_BPS`. Applies only to FUTURE liquidations (never
    /// retroactive — it changes how much collateral a liquidation may seize, not any existing
    /// position's stored state), so it is governance-tunable.
    LiqBonus,
    /// `min_debt` — minimum position debt (fUSD-native), the dust floor. 0 disables; otherwise clamped
    /// to `<= MAX_MIN_DEBT`. Non-retroactive (gates only new borrow/repay, never force-closes).
    MinDebt,
    /// `rate_adjust_cooldown_secs` — the premature rate-change cooldown (secs). 0 disables; otherwise
    /// clamped to `<= MAX_RATE_ADJUST_COOLDOWN_SECS`. The BOLD anti-gaming fee window.
    RateAdjustCooldown,
    /// `keeper_reward_bps` — the cut of `refresh_market`'s minted interest paid to the cranker. 0
    /// disables; otherwise clamped to `<= MAX_KEEPER_REWARD_BPS`. The self-funding keeper incentive.
    KeeperReward,
    /// `borrow_fee_bps` — the upfront borrowing fee (BOLD-sweep C7). 0 disables; otherwise clamped to
    /// `<= MAX_BORROW_FEE_BPS`. Appended last (the discriminant is serialized — order is frozen).
    BorrowFee,
    /// `bad_debt_paydown_bps` — the auto bad-debt paydown rate (BOLD-sweep C16). 0 disables; otherwise
    /// clamped to `<= MAX_BAD_DEBT_PAYDOWN_BPS`. Appended last (the discriminant is serialized).
    BadDebtPaydown,
    /// `redemption_base_rate_max_bps` — the dynamic redemption base-rate cap/enable (BOLD-sweep C9). 0
    /// DISABLES the dynamic component (flat-fee-only); otherwise clamped to
    /// `<= MAX_REDEMPTION_BASE_RATE_BPS`. Appended last (the discriminant is serialized).
    RedemptionBaseRateMax,

    // --- RiskParamRegistry: the broader tunable set on `MarketOracle` + `Market.scr_bps`. These
    //     REQUIRE the optional `market_oracle` account in the queue/execute context (Scr writes the
    //     Market). All bounded by the same compile-time clamps `init_market_oracle` enforces. ---
    /// `MarketOracle.max_conf_bps` — Pyth conf/price freeze band. `(0, MAX_ORACLE_CONF_BPS]`.
    OracleMaxConf,
    /// `MarketOracle.max_deviation_bps` — Pyth↔Switchboard agreement band. `(0, MAX_ORACLE_DEVIATION_BPS]`.
    OracleMaxDeviation,
    /// `MarketOracle.twap_max_divergence_bps` — DEX-TWAP mint corridor. `(0, MAX_TWAP_DIVERGENCE_BPS]`;
    /// must stay `<= liq_max_divergence_bps` (relational).
    OracleTwapDivergence,
    /// `MarketOracle.liq_max_divergence_bps` — liquidation-pause divergence gate. `[0, MAX_LIQ_DIVERGENCE_BPS]`;
    /// must stay `>= twap_max_divergence_bps` (relational).
    OracleLiqDivergence,
    /// `MarketOracle.max_age_secs` — feed staleness cutoff. `(0, MAX_ORACLE_MAX_AGE_SECS]`.
    OracleMaxAge,
    /// `MarketOracle.k_bps` — asymmetric `price ∓ k·σ`. `[MIN_ORACLE_K_BPS, MAX_ORACLE_K_BPS]`.
    OracleK,
    /// `MarketOracle.twap_max_staleness_secs` — TWAP sample staleness. `(0, MAX_TWAP_STALENESS_SECS]`.
    OracleTwapStaleness,
    /// `Market.scr_bps` — shutdown collateral ratio. `[MIN_SCR_BPS, MAX_SCR_BPS]`; must stay `<= mcr_bps`
    /// (relational, via `validate_market_config`).
    Scr,
}

/// The bounded governance gate. PDA `[b"gov_gate"]`. The sole authorizer of timelocked param
/// changes (`queue_param_change`). `inbound_authority` is MIGRATABLE — repoint from a guarded-
/// launch multisig to the MetaDAO DAO's Squads vault PDA later (an earlier PoC proved that path).
/// fusion-docs.md.
#[account]
#[derive(Debug)]
pub struct GovernanceGate {
    /// The authority allowed to QUEUE param changes — a launch multisig at first, the MetaDAO
    /// Squads vault PDA in production. Migratable via the TWO-STEP handshake
    /// (`migrate_inbound_authority` proposes → `accept_inbound_authority` the new key signs).
    pub inbound_authority: Pubkey,
    /// The proposed next inbound authority, pending its own acceptance. `Pubkey::default()` ⇒ no
    /// handoff in flight. The live `inbound_authority` only changes when the holder of THIS key
    /// signs `accept_inbound_authority`, so a typo'd / unheld proposal can never brick governance
    /// (it simply can never be accepted, and the current authority can re-propose).
    pub pending_inbound_authority: Pubkey,
    /// The fUSD-owned timelock delay (seconds) between queue and execute. Bounded by
    /// `[MIN_GOV_TIMELOCK_SECS, MAX_GOV_TIMELOCK_SECS]`; Squads itself runs `time_lock = 0`,
    /// which is exactly why fUSD supplies its own.
    pub timelock_secs: i64,
    /// Monotonic counter assigning each queued op its own `TimelockedParam` PDA.
    pub queue_nonce: u64,
    pub bump: u8,
    pub _reserved: [u8; 32],
}

impl GovernanceGate {
    // 8 disc + 32 inbound + 32 pending + 8 timelock + 8 nonce + 1 bump + 32 reserved.
    pub const SPACE: usize = 8 + 32 + 32 + 8 + 8 + 1 + 32;
}

/// A single queued, timelocked parameter change. PDA `[b"timelock", nonce_le]`. Created by
/// `queue_param_change`; applied + closed by `execute_param_change` once `now >= eta` (anyone
/// may execute), or closed unexecuted by `cancel_param_change`. fusion-docs.md.
#[account]
#[derive(Debug)]
pub struct TimelockedParam {
    /// The op's nonce (the `GovernanceGate.queue_nonce` value at queue time; also the PDA seed).
    pub nonce: u64,
    /// Earliest unix timestamp at which `execute_param_change` may apply this op.
    pub eta: i64,
    /// The market this op targets (re-checked at execute).
    pub market: Pubkey,
    /// The parameter and its new value (re-clamped at execute).
    pub param: MarketParam,
    pub value: u64,
    pub bump: u8,
    pub _reserved: [u8; 16],
}

impl TimelockedParam {
    // 8 disc + 8 nonce + 8 eta + 32 market + 1 enum-tag (param) + 8 value + 1 bump + 16 reserved.
    pub const SPACE: usize = 8 + 8 + 8 + 32 + 1 + 8 + 1 + 16;
}

/// Which bounded GLOBAL (Global Backstop Reserve) parameter a governance change targets. Mirrors
/// [`MarketParam`] but for the system-wide backstop (no per-market target). **APPEND-ONLY** for the
/// same persisted-borsh-tag reason — see [`MarketParam`]; pinned by `global_param_tags_are_pinned`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobalParam {
    /// `cut_bps` — funding cut of post-keeper interest routed to the reserve. Clamp `[0, MAX_BACKSTOP_CUT_BPS]`.
    Cut,
    /// `reserve_cap` — absolute fUSD reserve-level cap. No upper clamp (protocol sizing); 0 = no accrual.
    ReserveCap,
    /// `draw_base_allowance` — per-market base draw access (fUSD). No upper clamp.
    DrawBase,
    /// `draw_k_bps` — contribution multiplier (bps). Clamp `[0, MAX_BACKSTOP_DRAW_K_BPS]`.
    DrawK,
    /// `draw_ceiling_share_bps` — max fraction (bps) of the reserve one draw may take. Clamp `[0, 10_000]`.
    DrawCeilingShare,
    /// `draw_debt_share_bps` — max cumulative draw (bps) vs a market's own debt. Clamp `[0, 10_000]`.
    DrawDebtShare,
}

/// A single queued, timelocked GLOBAL-param change. PDA `[b"gtimelock", nonce_le]` (distinct prefix
/// from the per-market `TimelockedParam`). Shares the `GovernanceGate` (inbound authority + timelock +
/// nonce); applied to the `GlobalBackstopReserve` by `execute_global_param` once `now >= eta`.
#[account]
#[derive(Debug)]
pub struct TimelockedGlobalParam {
    pub nonce: u64,
    pub eta: i64,
    pub param: GlobalParam,
    pub value: u64,
    pub bump: u8,
    pub _reserved: [u8; 16],
}

impl TimelockedGlobalParam {
    // 8 disc + 8 nonce + 8 eta + 1 enum-tag (param) + 8 value + 1 bump + 16 reserved.
    pub const SPACE: usize = 8 + 8 + 8 + 1 + 8 + 1 + 16;
}

#[cfg(test)]
mod tests {
    use super::{GlobalParam, MarketParam};
    use anchor_lang::AnchorSerialize;

    /// Pins every `GlobalParam` variant's borsh tag (same append-only discipline as MarketParam).
    #[test]
    fn global_param_tags_are_pinned() {
        let pinned: &[(GlobalParam, u8)] = &[
            (GlobalParam::Cut, 0),
            (GlobalParam::ReserveCap, 1),
            (GlobalParam::DrawBase, 2),
            (GlobalParam::DrawK, 3),
            (GlobalParam::DrawCeilingShare, 4),
            (GlobalParam::DrawDebtShare, 5),
        ];
        for (variant, tag) in pinned {
            assert_eq!(
                variant.try_to_vec().unwrap(),
                vec![*tag],
                "{variant:?} must serialize to the pinned tag {tag} — GlobalParam is append-only"
            );
        }
    }

    /// Pins every `MarketParam` variant's borsh tag byte. The tag is persisted
    /// in `TimelockedParam` accounts and emitted events, so any reorder/removal re-keys queued
    /// ops and corrupts historical decoding — this test makes that a CI failure instead. New
    /// variants must be APPENDED (extending this list), never inserted or removed.
    #[test]
    fn market_param_tags_are_pinned() {
        let pinned: &[(MarketParam, u8)] = &[
            (MarketParam::Mcr, 0),
            (MarketParam::DebtCeiling, 1),
            (MarketParam::RedemptionFee, 2),
            (MarketParam::LiqGasComp, 3),
            (MarketParam::RateLimitCap, 4),
            (MarketParam::Ccr, 5),
            (MarketParam::LiqBonus, 6),
            (MarketParam::MinDebt, 7),
            (MarketParam::RateAdjustCooldown, 8),
            (MarketParam::KeeperReward, 9),
            (MarketParam::BorrowFee, 10),
            (MarketParam::BadDebtPaydown, 11),
            (MarketParam::RedemptionBaseRateMax, 12),
            (MarketParam::OracleMaxConf, 13),
            (MarketParam::OracleMaxDeviation, 14),
            (MarketParam::OracleTwapDivergence, 15),
            (MarketParam::OracleLiqDivergence, 16),
            (MarketParam::OracleMaxAge, 17),
            (MarketParam::OracleK, 18),
            (MarketParam::OracleTwapStaleness, 19),
            (MarketParam::Scr, 20),
        ];
        for (variant, tag) in pinned {
            let bytes = variant.try_to_vec().unwrap();
            assert_eq!(
                bytes,
                vec![*tag],
                "{variant:?} must serialize to the pinned tag {tag} — MarketParam is append-only"
            );
        }
    }
}
