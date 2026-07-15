use anchor_lang::prelude::*;

/// Per-market oracle configuration: feed bindings + validation thresholds (fusion-docs.md).
/// PDA `[b"oracle", collateral_mint]`. Read-only on the hot path; written at init and
/// (later) by governance setters within the compile-time clamps in `constants.rs`.
///
/// The thresholds mirror `fusd_oracle::OracleConfig` — `update_price` converts and calls
/// `aggregate`. Stored as the smallest sufficient ints (bps fit u16; the clamps guarantee it).
#[account]
#[derive(Debug)]
pub struct MarketOracle {
    pub collateral_mint: Pubkey,
    /// Pyth price-feed id (32 bytes, NOT an account address — bound via
    /// `get_price_no_older_than(.., feed_id)`).
    pub pyth_feed_id: [u8; 32],
    /// Switchboard on-demand feed this market accepts quotes for.
    pub switchboard_feed: Pubkey,
    /// Orca Whirlpool pool sampled by `sample_twap`. `Pubkey::default()` = not configured.
    pub orca_pool: Pubkey,
    /// Raydium CLMM pool sampled by `sample_twap`. `Pubkey::default()` = not configured.
    pub raydium_pool: Pubkey,

    // --- CLMM sampling: the quote (USD) leg + decimals, so `sample_twap` needs no Market/Mint
    //     accounts (decimals are immutable; bound to the real mints at init). fusion-docs.md. ---
    /// The collateral mint's decimals (copied from the mint at init; mirrors `Market.collateral_decimals`).
    pub collateral_decimals: u8,
    /// The expected quote (USD-stable) mint of the sampled pool — the OTHER leg of the collateral/USD
    /// pair. `sample_twap` requires the pool's two mints == {collateral_mint, quote_mint}.
    pub quote_mint: Pubkey,
    /// The quote mint's decimals (bound to the real mint at init). Used to whole-token-adjust the
    /// CLMM `sqrt_price` for Whirlpool (which stores no decimals); cross-checked against Raydium's
    /// in-account decimals.
    pub quote_decimals: u8,

    // --- fusd_oracle::OracleConfig thresholds (clamped; defaults pending backtesting) ---
    /// Freeze mints if Pyth conf/price exceeds this (bps).
    pub max_conf_bps: u16,
    /// Pyth ↔ Switchboard agreement band (bps).
    pub max_deviation_bps: u16,
    /// DEX-TWAP divergence corridor (bps).
    pub twap_max_divergence_bps: u16,
    /// Feed staleness cutoff (seconds).
    pub max_age_secs: i64,
    /// Asymmetry: collateral `price − k·σ`, debt `price + k·σ` (bps of σ).
    pub k_bps: u16,

    // --- TWAP guards (fusd_oracle::TwapConfig + the window) ---
    pub twap_window_secs: i64,
    pub twap_min_samples: u32,
    pub twap_max_staleness_secs: i64,

    // --- Plausibility band — a coarse 10^k-scale / absolute-nonsense rail on the
    //     committed spot, mirroring `fusd_oracle::OracleConfig::band_{lower,upper}_ray`. RAY-scaled
    //     USD per whole collateral TOKEN (the `usd_ray` scale the band is compared against in
    //     `update_price` — NOT per native unit; they differ by 10^decimals); each bound `0` = disabled. Set 10×–100× wide at init and
    //     INIT-ONLY in v1 (no governance setter — a `MarketParam::PriceBand` would have to add a
    //     placement sanity check on top of the width clamp). The guardian gets NO band power. ---
    pub price_band_lower_ray: u128,
    pub price_band_upper_ray: u128,

    /// Liquidation-divergence threshold (bps), mirroring
    /// `fusd_oracle::OracleConfig::liq_max_divergence_bps`. `0` = disabled. When set, a fresh primary
    /// disagreeing with a present secondary by more than this arms `Market.liq_divergence_until`,
    /// pausing LIQUIDATIONS only (never redemptions/repay). Set LOOSER than the mint
    /// deviation thresholds. INIT-ONLY in v1 (clamped `[0, MAX_LIQ_DIVERGENCE_BPS]`). Carved from
    /// `_reserved`.
    pub liq_max_divergence_bps: u16,

    /// C1 LST canonical-rate leg: the SPL Stake Pool `StakePool` account for this collateral, when
    /// it is a liquid-staking token. `update_price` reads its `total_lamports / pool_token_supply`
    /// (SOL per LST) and serves the collateral price at `MIN(market, sol_usd · rate)` so an upward-
    /// manipulated market feed can't inflate borrowing power past the trustless stake-pool rate
    /// (BOLD-08). `Pubkey::default()` (zero) = NOT an LST market: the leg is disabled and minting is
    /// unaffected. When set, minting REQUIRES a fresh, valid stake-pool rate + SOL/USD feed. The
    /// SOL/USD underlying feed is the shared `constants::PYTH_SOL_USD_FEED_ID`. INIT-ONLY in v1.
    /// Carved from `_reserved` (the doc's anticipated "LST redemption-rate" refinement).
    pub lst_stake_pool: Pubkey,

    pub bump: u8,

    /// Canonical-primary oracle mode (fuSOL): `1` = this market's PRICE IS the composed
    /// `sol_usd × stake_pool_rate` — the bound Pyth/Switchboard feeds are the SOL/USD legs
    /// (init-enforced `pyth_feed_id == PYTH_SOL_USD_FEED_ID`) and `update_price` scales both
    /// parsed views by the bound pool's `total_lamports / pool_token_supply` before aggregation,
    /// so `spot` AND `debt_spot` both track pool NAV (a negative-NAV finalization propagates to
    /// the liquidation path on the next crank). Requires `lst_stake_pool` set (owner = the
    /// FUSION stake-pool fork, not `SPoo1…`), no DEX pools bound (no venue exists pre-listing;
    /// the TWAP corridor is optional in this mode and deferred until a fuSOL venue design lands),
    /// and `liquidity_haircut_bps > 0`. `0` (zeroed on every pre-carve account) = the market-feed
    /// oracle, byte-identical to prior behavior. INIT-ONLY. Carved from `_reserved`.
    pub canonical_primary: u8,
    /// Liquidity haircut (bps) applied to the COLLATERAL (mint/LTV) price in canonical-primary
    /// mode — the conservative stand-in for a market corridor while no fuSOL DEX venue exists
    /// (`spot = composed_nav × (10_000 − haircut) / 10_000`, after the −k·σ confidence haircut).
    /// The debt/liquidation price is NOT haircut (it wants the conservative HIGH side). Clamped
    /// `[1, MAX_LIQUIDITY_HAIRCUT_BPS]` in mode 1; MUST be 0 in mode 0 (unused). INIT-ONLY.
    /// Carved from `_reserved`.
    pub liquidity_haircut_bps: u16,

    /// Last COMMITTED canonical-primary pool rate: `total_lamports / pool_token_supply`, RAY-scaled
    /// (SOL per whole fuSOL — both 9-decimal, so the ratio is the whole-token rate directly).
    /// Written by `update_price` on every mode-1 crank that commits a fresh price. A NEW rate
    /// strictly BELOW this one is a pool NAV decrease (slashing / negative finalization) — the
    /// crank commits the lower price immediately AND arms the standard on-resume liquidation grace
    /// (`Market.liq_grace_until`, monotone `max`), so a loss borrowers could not front-run gets the
    /// same cure window as a staleness resume. Deliberately keyed on the POOL RATE, not `debt_spot`:
    /// a plain SOL/USD move is normal market risk and must never arm the grace. `0` = never
    /// committed (the first crank only seeds it, never arms). Mode-0 markets never write it.
    /// Carved from `_reserved`.
    pub last_canonical_rate_ray: u128,

    /// Forward-compat reserve. WIDENED 32 → 64 bytes pre-launch (layout-freeze checklist): a Borsh
    /// account cannot grow without realloc post-launch, so carve-from-`_reserved` headroom is free
    /// now and impossible later. Carve new fields from the HEAD; old accounts' zeroed bytes must
    /// decode as the new field's `0 = disabled/none` sentinel. Holds feed-rebind and future
    /// oracle refinements. (`lst_stake_pool` above carved 32 bytes for the C1 LST leg: was 62;
    /// `canonical_primary` + `liquidity_haircut_bps` carved 3 more: was 30;
    /// `last_canonical_rate_ray` carved 16 more: was 27.)
    pub _reserved: [u8; 11],
}

impl MarketOracle {
    pub const SPACE: usize = 8      // discriminator
        + 32 + 32 + 32 + 32 + 32    // collateral_mint, pyth_feed_id, switchboard_feed, orca_pool, raydium_pool
        + 1 + 32 + 1                // collateral_decimals, quote_mint, quote_decimals
        + 2 + 2 + 2 + 8 + 2         // conf, deviation, twap divergence, max_age, k
        + 8 + 4 + 8                 // twap window, min_samples, staleness
        + 16 + 16                   // price_band_lower_ray, price_band_upper_ray
        + 2                         // liq_max_divergence_bps
        + 32                        // lst_stake_pool (C1 LST canonical-rate leg)
        + 1                         // bump
        + 1 + 2                     // canonical_primary, liquidity_haircut_bps (fuSOL mode)
        + 16                        // last_canonical_rate_ray (fuSOL NAV-decrease grace)
        + 11; // reserved (62 → 30 for lst_stake_pool → 27 for the fuSOL mode fields
              //           → 11 for last_canonical_rate_ray)
}
