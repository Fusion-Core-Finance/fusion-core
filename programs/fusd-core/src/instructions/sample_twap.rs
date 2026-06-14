//! `sample_twap` — the permissionless DEX-TWAP sampler crank.
//!
//! Solana CLMMs expose no on-chain TWAP accumulator, so fUSD samples a pool's spot `sqrt_price`
//! and appends a `usd_ray` observation to the per-market `DexTwap` ring. The ring's
//! time-weighting (a window-spanning average that a few-block pump cannot move — the Mango
//! lesson) is what makes the TWAP a sound manipulation-resistance corridor for `update_price`.
//! The TWAP is NEVER a primary price.
//!
//! Full guard set (`docs/clmm-pool-layouts.md`): the pool must be one of the configured
//! pools AND owned by the matching venue program; its discriminator, length, and `sqrt_price`
//! bounds are checked; and its two mints must equal {collateral, quote}. A misconfigured pool
//! yields no sample, never a bad price.

use anchor_lang::prelude::*;
use anchor_spl::token::Mint;
use fusd_math::oracle_scale::sqrt_price_q64_to_ray;
use fusd_math::{ray_div, RAY};

use crate::clmm::{self, Venue};
use crate::constants::{
    DEX_TWAP_SEED, MARKET_ORACLE_SEED, ORCA_WHIRLPOOL_PROGRAM_ID, RAYDIUM_CLMM_PROGRAM_ID,
    TWAP_RING_CAPACITY,
};
use crate::errors::FusdError;
use crate::state::{DexTwap, MarketOracle};

#[event_cpi]
#[derive(Accounts)]
pub struct SampleTwap<'info> {
    /// Permissionless caller. No authority check.
    pub cranker: Signer<'info>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(
        seeds = [MARKET_ORACLE_SEED, collateral_mint.key().as_ref()],
        bump = market_oracle.bump,
    )]
    pub market_oracle: Account<'info, MarketOracle>,

    #[account(mut, seeds = [DEX_TWAP_SEED, collateral_mint.key().as_ref()], bump)]
    pub dex_twap: AccountLoader<'info, DexTwap>,

    /// CHECK: a raw Orca Whirlpool / Raydium CLMM pool account. Fully validated in the handler
    /// (must be a configured pool, owned by the matching venue program; discriminator / length /
    /// sqrt-price bounds / mint-pair checked via `crate::clmm`). Never trusted by address alone.
    pub clmm_pool: UncheckedAccount<'info>,
}

pub fn handler(ctx: Context<SampleTwap>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;

    // Snapshot config into locals (no lingering borrow across the ring mutation).
    let mo = &ctx.accounts.market_oracle;
    let orca_pool = mo.orca_pool;
    let raydium_pool = mo.raydium_pool;
    let coll_mint = mo.collateral_mint;
    let quote_mint = mo.quote_mint;
    let coll_decimals = mo.collateral_decimals;
    let quote_decimals = mo.quote_decimals;
    let twap_window_secs = mo.twap_window_secs;

    // 1. Venue selection: the pool must be one of the configured pools.
    let pool_key = ctx.accounts.clmm_pool.key();
    let (venue, venue_program) = if orca_pool != Pubkey::default() && pool_key == orca_pool {
        (Venue::Orca, ORCA_WHIRLPOOL_PROGRAM_ID)
    } else if raydium_pool != Pubkey::default() && pool_key == raydium_pool {
        (Venue::Raydium, RAYDIUM_CLMM_PROGRAM_ID)
    } else {
        return err!(FusdError::InvalidClmmPool);
    };

    // 2. Runtime owner must be the matching venue program (never trust by address).
    require!(ctx.accounts.clmm_pool.owner == &venue_program, FusdError::InvalidClmmPool);

    // 3. Parse the pool bytes (discriminator / length / sqrt-price bounds, checked inside).
    let sample = {
        let data = ctx
            .accounts
            .clmm_pool
            .try_borrow_data()
            .map_err(|_| FusdError::InvalidClmmPool)?;
        clmm::parse(venue, &data).map_err(|_| FusdError::InvalidClmmPool)?
    };

    // 4. Bind the pool to the right asset pair: its two mints must be {collateral, quote}.
    let pair_ok = (sample.mint_a == coll_mint && sample.mint_b == quote_mint)
        || (sample.mint_a == quote_mint && sample.mint_b == coll_mint);
    require!(pair_ok, FusdError::InvalidClmmPool);
    let collateral_is_a = sample.mint_a == coll_mint;

    // Decimals for token_a / token_b (sqrt_price = sqrt(token_b-native / token_a-native)).
    // base = token_a, quote = token_b. Raydium carries decimals in-account → cross-check against
    // the configured mints (defense-in-depth); Whirlpool stores none → use the configured values.
    let cfg_a = if collateral_is_a { coll_decimals } else { quote_decimals };
    let cfg_b = if collateral_is_a { quote_decimals } else { coll_decimals };
    let (dec_a, dec_b) = match (sample.dec_a, sample.dec_b) {
        (Some(da), Some(db)) => {
            require!(da == cfg_a && db == cfg_b, FusdError::InvalidClmmPool);
            (da, db)
        }
        _ => (cfg_a, cfg_b),
    };

    // 5. quote-per-base (token_b per token_a), RAY-scaled. Invert when the collateral is the
    //    quote side, so the observation is always USD(quote)-per-collateral.
    let price_ba =
        sqrt_price_q64_to_ray(sample.sqrt_price, dec_a, dec_b).ok_or(FusdError::MathOverflow)?;
    let usd_ray = if collateral_is_a {
        price_ba
    } else {
        ray_div(RAY, price_ba).ok_or(FusdError::MathOverflow)?
    };
    // A zero observation is meaningless and would skew the TWAP corridor toward freezing mints; it
    // only arises from a pathological decimal spread flooring `price_ba` to 0. Reject defensively.
    require!(usd_ray > 0, FusdError::InvalidClmmPool);

    // 6. Anti-flood spacing gate. The ring holds only `TWAP_RING_CAPACITY` samples and
    //    `twap()` refuses to extrapolate, so without a minimum spacing anyone could spam fresh
    //    samples ~1/sec and evict all window-spanning history → `twap() == None` → mints frozen
    //    indefinitely for the cost of base fees. Require consecutive samples to be at least
    //    `ceil(window / (N-1))` apart: then a FULL ring of N samples always spans ≥ `window` (its
    //    N-1 gaps each ≥ that), so the corridor's coverage holds even under a sustained flood, and
    //    an attacker cannot push faster than an honest keeper. The first sample (empty ring) is
    //    always accepted; the strictly-increasing-ts ring invariant is preserved underneath.
    let mut twap = ctx.accounts.dex_twap.load_mut()?;
    if let Some(last) = twap.ring().last() {
        let gaps = (TWAP_RING_CAPACITY as i64 - 1).max(1);
        // window is clamped > 0 at init, so this ceil-div is well-defined and ≥ 1.
        let min_interval = (twap_window_secs + gaps - 1) / gaps;
        require!(now - last.ts >= min_interval, FusdError::TwapSampleRejected);
    }
    twap.ring_mut().push(usd_ray, now).map_err(|_| FusdError::TwapSampleRejected)?;

    // The TWAP-liveness heartbeat (the corridor needs window-spanning samples; monitors alarm on gaps).
    emit_cpi!(crate::events::TwapSampled {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        usd_ray,
        ts: now,
    });
    Ok(())
}
