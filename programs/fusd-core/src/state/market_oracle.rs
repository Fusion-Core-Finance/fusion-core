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
    //     USD per 1 native collateral unit; each bound `0` = disabled. Set 10×–100× wide at init and
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

    pub bump: u8,
    /// Forward-compat reserve. WIDENED 32 → 64 bytes pre-launch (layout-freeze checklist): a Borsh
    /// account cannot grow without realloc post-launch, so carve-from-`_reserved` headroom is free
    /// now and impossible later. Carve new fields from the HEAD; old accounts' zeroed bytes must
    /// decode as the new field's `0 = disabled/none` sentinel. Holds feed-rebind and future
    /// oracle refinements (e.g. a second Pyth feed for LST redemption-rate pricing).
    pub _reserved: [u8; 62],
}

impl MarketOracle {
    pub const SPACE: usize = 8      // discriminator
        + 32 + 32 + 32 + 32 + 32    // collateral_mint, pyth_feed_id, switchboard_feed, orca_pool, raydium_pool
        + 1 + 32 + 1                // collateral_decimals, quote_mint, quote_decimals
        + 2 + 2 + 2 + 8 + 2         // conf, deviation, twap divergence, max_age, k
        + 8 + 4 + 8                 // twap window, min_samples, staleness
        + 16 + 16                   // price_band_lower_ray, price_band_upper_ray
        + 2                         // liq_max_divergence_bps
        + 1                         // bump
        + 62; // reserved (64 → 62 for liq_max_divergence_bps; widened 32 → 64 for freeze headroom)
}
