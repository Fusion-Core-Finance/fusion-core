use anchor_lang::prelude::*;
use anchor_spl::token::Mint;

use crate::constants::{
    CONFIG_SEED, DEX_TWAP_SEED, MARKET_ORACLE_SEED, MARKET_SEED, MAX_ORACLE_CONF_BPS,
    MAX_ORACLE_DEVIATION_BPS, MAX_ORACLE_K_BPS, MAX_ORACLE_MAX_AGE_SECS, MAX_TWAP_DIVERGENCE_BPS,
    MAX_LIQ_DIVERGENCE_BPS, MAX_TWAP_STALENESS_SECS, MAX_TWAP_WINDOW_SECS, MIN_ORACLE_K_BPS,
    MIN_PRICE_BAND_RATIO, MIN_TWAP_MIN_SAMPLES, MIN_TWAP_WINDOW_SECS,
};
use crate::errors::FusdError;
use crate::state::{DexTwap, Market, MarketOracle, ProtocolConfig};

/// Inputs to `init_market_oracle`. Thresholds are clamped at the compile-time bounds in
/// `constants.rs` (defaults there are placeholders pending the backtesting pass).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct InitMarketOracleArgs {
    /// Pyth price-feed id (NOT an account address).
    pub pyth_feed_id: [u8; 32],
    /// Switchboard on-demand feed account.
    pub switchboard_feed: Pubkey,
    /// Orca Whirlpool to sample (`Pubkey::default()` = none). At least one pool required.
    /// Validated structurally (owner / discriminator / mint pair) on EVERY `sample_twap`,
    /// not here — a misconfigured pool yields no samples, never a bad price.
    pub orca_pool: Pubkey,
    /// Raydium CLMM pool to sample (`Pubkey::default()` = none).
    pub raydium_pool: Pubkey,
    pub max_conf_bps: u16,
    pub max_deviation_bps: u16,
    pub twap_max_divergence_bps: u16,
    pub max_age_secs: i64,
    pub k_bps: u16,
    pub twap_window_secs: i64,
    pub twap_min_samples: u32,
    pub twap_max_staleness_secs: i64,
    /// Plausibility band, RAY-scaled USD per native collateral unit. Each bound
    /// `0` = disabled; when both are set they must be `>= MIN_PRICE_BAND_RATIO` apart (a coarse rail,
    /// not a tight opinion). Default `(0, 0)` = off — byte-identical to the pre-band oracle behavior.
    pub price_band_lower_ray: u128,
    pub price_band_upper_ray: u128,
    /// Liquidation-divergence threshold (bps). `0` = disabled; otherwise clamped
    /// `<= MAX_LIQ_DIVERGENCE_BPS` and intended LOOSER than the mint deviation thresholds.
    pub liq_max_divergence_bps: u16,
}

#[derive(Accounts)]
pub struct InitMarketOracle<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,

    pub collateral_mint: Box<Account<'info, Mint>>,

    /// The expected quote (USD-stable) leg of the sampled CLMM pool. Read once here to bind its
    /// key + decimals into `MarketOracle` so `sample_twap` needs no Mint account at crank time.
    pub quote_mint: Box<Account<'info, Mint>>,

    /// The market must already exist (init_market first).
    #[account(seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,

    #[account(
        init,
        payer = authority,
        space = MarketOracle::SPACE,
        seeds = [MARKET_ORACLE_SEED, collateral_mint.key().as_ref()],
        bump,
    )]
    pub market_oracle: Box<Account<'info, MarketOracle>>,

    #[account(
        init,
        payer = authority,
        space = DexTwap::SPACE,
        seeds = [DEX_TWAP_SEED, collateral_mint.key().as_ref()],
        bump,
    )]
    pub dex_twap: AccountLoader<'info, DexTwap>,

    pub system_program: Program<'info, System>,
}

pub fn handler(ctx: Context<InitMarketOracle>, args: InitMarketOracleArgs) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );

    // Feed bindings must be real (an all-zero feed id / default pubkey can never verify).
    require!(args.pyth_feed_id != [0u8; 32], FusdError::ParamOutOfBounds);
    require!(args.switchboard_feed != Pubkey::default(), FusdError::ParamOutOfBounds);
    // The TWAP corridor is load-bearing for mint-mode (aggregate requires it): at least one pool.
    require!(
        args.orca_pool != Pubkey::default() || args.raydium_pool != Pubkey::default(),
        FusdError::ParamOutOfBounds
    );
    // If both venues are configured they must be distinct pools — else `sample_twap`'s
    // address-based venue selection would shadow one venue with the other's program owner.
    require!(
        args.orca_pool == Pubkey::default()
            || args.raydium_pool == Pubkey::default()
            || args.orca_pool != args.raydium_pool,
        FusdError::ParamOutOfBounds
    );
    // The quote leg must differ from the collateral (a pool's two mints are distinct).
    require_keys_neq!(
        ctx.accounts.quote_mint.key(),
        ctx.accounts.collateral_mint.key(),
        FusdError::ParamOutOfBounds
    );

    // Compile-time clamps: all thresholds positive and bounded.
    require!(
        args.max_conf_bps > 0 && args.max_conf_bps <= MAX_ORACLE_CONF_BPS,
        FusdError::ParamOutOfBounds
    );
    require!(
        args.max_deviation_bps > 0 && args.max_deviation_bps <= MAX_ORACLE_DEVIATION_BPS,
        FusdError::ParamOutOfBounds
    );
    require!(
        args.twap_max_divergence_bps > 0
            && args.twap_max_divergence_bps <= MAX_TWAP_DIVERGENCE_BPS,
        FusdError::ParamOutOfBounds
    );
    require!(
        args.max_age_secs > 0 && args.max_age_secs <= MAX_ORACLE_MAX_AGE_SECS,
        FusdError::ParamOutOfBounds
    );
    require!(
        args.k_bps >= MIN_ORACLE_K_BPS && args.k_bps <= MAX_ORACLE_K_BPS,
        FusdError::ParamOutOfBounds
    );
    require!(
        args.twap_window_secs >= MIN_TWAP_WINDOW_SECS
            && args.twap_window_secs <= MAX_TWAP_WINDOW_SECS,
        FusdError::ParamOutOfBounds
    );
    require!(args.twap_min_samples >= MIN_TWAP_MIN_SAMPLES, FusdError::ParamOutOfBounds);
    require!(
        args.twap_max_staleness_secs > 0
            && args.twap_max_staleness_secs <= MAX_TWAP_STALENESS_SECS,
        FusdError::ParamOutOfBounds
    );
    // Plausibility band: each bound is independently disable-able (0). When BOTH are set, require
    // a minimum width (`upper >= lower · MIN_PRICE_BAND_RATIO`) so the band can only ever be a coarse
    // 10^k-scale rail, never a tight price opinion a captured governance could weaponize into a
    // synthetic oracle outage. A reversed/degenerate band (lower >= upper) is rejected by the same
    // check. Default (0, 0) = off.
    if args.price_band_lower_ray != 0 && args.price_band_upper_ray != 0 {
        let min_upper = args
            .price_band_lower_ray
            .checked_mul(MIN_PRICE_BAND_RATIO)
            .ok_or(FusdError::MathOverflow)?;
        require!(args.price_band_upper_ray >= min_upper, FusdError::ParamOutOfBounds);
    }
    // Liquidation-divergence threshold: 0 = disabled; otherwise clamped (set looser than the mint
    // deviation thresholds — enforced by deployer choice, not relationally, matching the other oracle
    // thresholds' init-only clamp pattern).
    require!(args.liq_max_divergence_bps <= MAX_LIQ_DIVERGENCE_BPS, FusdError::ParamOutOfBounds);

    // Zero-initialize the observation ring (empty; all-zero IS the valid empty state).
    ctx.accounts.dex_twap.load_init()?;

    let o = &mut ctx.accounts.market_oracle;
    o.collateral_mint = ctx.accounts.collateral_mint.key();
    o.pyth_feed_id = args.pyth_feed_id;
    o.switchboard_feed = args.switchboard_feed;
    o.orca_pool = args.orca_pool;
    o.raydium_pool = args.raydium_pool;
    o.collateral_decimals = ctx.accounts.collateral_mint.decimals;
    o.quote_mint = ctx.accounts.quote_mint.key();
    o.quote_decimals = ctx.accounts.quote_mint.decimals;
    o.max_conf_bps = args.max_conf_bps;
    o.max_deviation_bps = args.max_deviation_bps;
    o.twap_max_divergence_bps = args.twap_max_divergence_bps;
    o.max_age_secs = args.max_age_secs;
    o.k_bps = args.k_bps;
    o.twap_window_secs = args.twap_window_secs;
    o.twap_min_samples = args.twap_min_samples;
    o.twap_max_staleness_secs = args.twap_max_staleness_secs;
    o.price_band_lower_ray = args.price_band_lower_ray;
    o.price_band_upper_ray = args.price_band_upper_ray;
    o.liq_max_divergence_bps = args.liq_max_divergence_bps;
    o.bump = ctx.bumps.market_oracle;
    o._reserved = [0u8; 62];
    Ok(())
}
