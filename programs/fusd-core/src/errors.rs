use anchor_lang::prelude::*;

/// Program error codes. Extended as flows land.
#[error_code]
pub enum FusdError {
    #[msg("Unauthorized: signer is not the configured governance authority")]
    Unauthorized,
    #[msg("Parameter is outside the compile-time clamp range")]
    ParamOutOfBounds,
    #[msg("Collateral mint has a freeze authority and cannot be onboarded")]
    CollateralHasFreezeAuthority,
    #[msg("Oracle price is stale, unset, or feeds disagree (mint frozen)")]
    OracleUnavailable,
    #[msg("Cached collateral price is too old to act on")]
    StalePrice,
    #[msg("Operation would leave the position below its minimum collateral ratio")]
    BelowMinCollateralRatio,
    #[msg("Market debt ceiling exceeded")]
    DebtCeilingExceeded,
    #[msg("Amount must be non-zero")]
    ZeroAmount,
    #[msg("Insufficient collateral in the position")]
    InsufficientCollateral,
    #[msg("Position is at or above its minimum collateral ratio — cannot liquidate")]
    PositionHealthy,
    #[msg("Reactor Pool deposits cannot fully absorb this liquidation (tier-2 redistribution not yet wired)")]
    ReactorPoolTooSmall,
    // Currently unconstructed (withdraw_from_reactor CLAMPS an over-withdraw to the deposit rather than
    // erroring). KEEP — removal would shift every later 6000+ordinal code (pinned by the litesvm `E_*`).
    #[msg("Insufficient Reactor Pool deposit balance")]
    InsufficientReactorDeposit,
    #[msg("Reactor Pool grid exhausted (scale/epoch out of range) — migration required")]
    ReactorGridExhausted,
    #[msg("Arithmetic overflow")]
    MathOverflow,
    #[msg("Liquidation cannot complete: the Reactor Pool is empty and no other position can absorb the redistribution")]
    NoRedistributionRecipients,
    #[msg("Redistribution accumulator overflow — migration required")]
    RedistributionAccumulatorOverflow,
    #[msg("Position still holds debt or collateral and cannot be closed")]
    PositionNotEmpty,
    #[msg("Nothing to redeem: no non-empty rate bucket or no valid candidate")]
    NothingToRedeem,
    // Currently unconstructed (redeem SKIPS a candidate outside the lowest bucket rather than erroring
    // per-candidate). KEEP — removal would shift every later 6000+ordinal code (pinned by litesvm `E_*`).
    #[msg("Redemption target is not in the lowest non-empty rate bucket")]
    WrongRedemptionBucket,
    #[msg("Duplicate redemption target in the candidate list")]
    DuplicateRedemptionTarget,
    #[msg("Governance timelock has not elapsed — the queued change cannot execute yet")]
    TimelockNotElapsed,
    #[msg("The market account does not match the queued change's target market")]
    TimelockMarketMismatch,
    #[msg("New mints are frozen: the oracle aggregate is degraded (stale/wide-conf/divergent)")]
    MintFrozen,
    #[msg("Pyth price update is invalid: wrong owner, unverified, wrong feed id, or non-positive price")]
    InvalidPriceUpdate,
    #[msg("Switchboard feed account is invalid: wrong owner, wrong key, or unparseable")]
    InvalidSwitchboardFeed,
    #[msg("CLMM pool account is invalid: wrong owner/program, discriminator, length, sqrt-price bounds, or mint pair")]
    InvalidClmmPool,
    #[msg("TWAP sample rejected: timestamp not strictly newer than the last, or closer than the minimum inter-sample interval (anti-flood)")]
    TwapSampleRejected,
    #[msg("New borrowing is paused by the guardian de-risk brake (auto-lifts; does not affect repay/liquidation/redemption)")]
    GuardianPaused,
    #[msg("Market is shut down (terminal): borrow and ordered redemption are closed; use urgent_redeem")]
    MarketShutdown,
    #[msg("Market is not shut down: urgent_redeem is only valid after shutdown")]
    MarketNotShutdown,
    #[msg("Shutdown condition not met: the market is neither below SCR nor in sustained oracle failure")]
    ShutdownConditionNotMet,
    #[msg("Net fUSD issuance would exceed the market's rate-limit cap for the current window")]
    RateLimitExceeded,
    #[msg("Operation restricted: it would leave the market below its critical collateral ratio (CCR band)")]
    CcrRestricted,
    #[msg("Liquidations are paused by the on-resume grace window after an oracle staleness halt — borrowers have a window to cure")]
    LiquidationGracePeriod,
    #[msg("Borrower interest rate is outside the allowed range [MIN_USER_RATE_BPS, MAX_USER_RATE_BPS]")]
    InterestRateOutOfBounds,
    #[msg("No liquidation collateral surplus to claim")]
    NoCollateralSurplus,
    #[msg("No pending inbound authority to accept (propose one first via migrate_inbound_authority)")]
    NoPendingAuthority,
    #[msg("Operation would leave the position below the market's minimum debt: repay fully or stay at/above min_debt")]
    DebtBelowMinimum,
    #[msg("Too many redemption candidate accounts (exceeds MAX_REDEMPTION_CANDIDATES)")]
    TooManyRedemptionCandidates,
    #[msg("Requested amount exceeds the available protocol-owned collateral (surplus or un-homed remainder)")]
    InsufficientProtocolCollateral,
    #[msg("Liquidation bonus collar is not fundable at the MCR boundary (requires 100% + liq_bonus <= MCR, or bonus 0)")]
    CollarExceedsMcr,
    #[msg("Parameter values are individually in range but jointly invalid for this market's config")]
    ParamCombinationInvalid,
    #[msg("Collateral vault holds less than the tracked ledger sum — accounting drift detected, reverting")]
    VaultReconciliationFailed,
    #[msg("Recipient must not be a protocol vault (a self-transfer would debit the counter while stranding the value)")]
    InvalidRecipient,
    // NOTE: append-only — new variants MUST go at the END so existing Anchor error codes (6000 + ordinal)
    // never shift (the litesvm `E_*` constants pin them). Consolidation order: backstop's variant (6044)
    // was already on master, so it keeps 6044; oracle-hardening's follows at 6045.
    #[msg("Requested amount exceeds the global backstop reserve's above-cap excess (the cap is the protective floor)")]
    InsufficientBackstopExcess,
    #[msg("Liquidations are paused: a fresh primary feed grossly disagrees with a present secondary (oracle divergence) — redemptions and repay stay open")]
    OracleDivergent,
    #[msg("C1 LST canonical-rate leg: the supplied SPL stake-pool account has the wrong owner or key (mis-built crank)")]
    InvalidStakePool,
}
