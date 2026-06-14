//! `update_price` — the permissionless oracle crank that feeds `Market.spot`.
//!
//! Reads Pyth (primary) + optional Switchboard (secondary) + the self-maintained DEX-TWAP
//! corridor, normalizes EVERYTHING to `usd_ray` (RAY-scaled USD per whole collateral token) so
//! the cross-feed comparison and the output share one scale, runs `fusd_oracle::aggregate`, then
//! writes the conservative collateral price into `Market.spot` and the aggregate's mint-mode into
//! `Market.mint_frozen`. Anyone may call it (it carries a fresh Pyth post-update in the same tx).
//!
//! A degraded aggregate freezes NEW MINTS only — `spot` still gets a conservative price,
//! so repay / liquidation / redemption keep working. Only `borrow` consults `mint_frozen`.

use anchor_lang::prelude::*;
use anchor_spl::token::Mint;
use fusd_math::oracle_scale::{px_to_ray, usd_ray_to_spot};
use fusd_oracle::{aggregate, OracleConfig, OracleMode, PriceView, TwapConfig};
use pyth_solana_receiver_sdk::price_update::{PriceUpdateV2, VerificationLevel};
use switchboard_on_demand::PullFeedAccountData;

use crate::constants::{CONFIG_SEED, DEX_TWAP_SEED, MARKET_ORACLE_SEED, MARKET_SEED};
use crate::errors::FusdError;
use crate::instructions::init_protocol::FUSD_DECIMALS;
use crate::state::{DexTwap, Market, MarketOracle, ProtocolConfig};

#[event_cpi]
#[derive(Accounts)]
pub struct UpdatePrice<'info> {
    /// Permissionless caller (signs only to carry the tx / pay fees). No authority check.
    pub cranker: Signer<'info>,

    /// Global config (read-only) — carries the bounded-updatable oracle program IDs the parsers
    /// verify the feed accounts' owners against (so a Pyth core migration doesn't force a redeploy).
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, ProtocolConfig>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,

    #[account(
        seeds = [MARKET_ORACLE_SEED, collateral_mint.key().as_ref()],
        bump = market_oracle.bump,
    )]
    pub market_oracle: Account<'info, MarketOracle>,

    /// CHECK: a Pyth `PriceUpdateV2`. Verified in the handler: runtime owner ==
    /// `PYTH_RECEIVER_PROGRAM_ID`, anchor discriminator, `VerificationLevel::Full`, and bound to
    /// `market_oracle.pyth_feed_id`. Never trusted by address.
    pub pyth_price_update: UncheckedAccount<'info>,

    /// CHECK: an optional Switchboard `PullFeedAccountData`. When present, verified in the handler
    /// (owner == `SWITCHBOARD_ON_DEMAND_PROGRAM_ID`, key == `market_oracle.switchboard_feed`).
    /// Absent (or a degraded value) ⇒ `aggregate` freezes mints but still prices off Pyth.
    pub switchboard_feed: Option<UncheckedAccount<'info>>,

    #[account(seeds = [DEX_TWAP_SEED, collateral_mint.key().as_ref()], bump)]
    pub dex_twap: AccountLoader<'info, DexTwap>,
}

pub fn handler(ctx: Context<UpdatePrice>) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    let slot = clock.slot;

    // Snapshot the oracle config (read-only) into locals so no borrow lingers across the mutation.
    let mo = &ctx.accounts.market_oracle;
    let pyth_feed_id = mo.pyth_feed_id;
    let sb_feed_key = mo.switchboard_feed;
    let twap_window_secs = mo.twap_window_secs;
    let twap_cfg = TwapConfig {
        min_samples: mo.twap_min_samples,
        max_staleness: mo.twap_max_staleness_secs,
    };
    let cfg = OracleConfig {
        max_conf_bps: mo.max_conf_bps as u128,
        max_deviation_bps: mo.max_deviation_bps as u128,
        twap_max_divergence_bps: mo.twap_max_divergence_bps as u128,
        max_age_secs: mo.max_age_secs,
        k_bps: mo.k_bps as u128,
        // Plausibility band (RAY-scaled, same scale as the usd_ray price views below). 0 = off.
        band_lower_ray: mo.price_band_lower_ray,
        band_upper_ray: mo.price_band_upper_ray,
        // Liquidation-divergence threshold (bps). 0 = off.
        liq_max_divergence_bps: mo.liq_max_divergence_bps as u128,
    };
    let coll_decimals = ctx.accounts.market.collateral_decimals;
    // The bounded-updatable oracle program IDs (genesis = the compile-time constants; updatable by
    // gov via `set_oracle_program_ids` to absorb the Pyth core migration without a redeploy). The
    // Pyth update is accepted if owned by EITHER configured receiver (primary OR the alt) — the
    // dual-running window cutover (alt seeded to the upgraded receiver at genesis; default = disabled).
    let pyth_program_id = ctx.accounts.config.pyth_receiver_program_id;
    let pyth_program_id_alt = ctx.accounts.config.pyth_receiver_program_id_alt;
    let sb_program_id = ctx.accounts.config.switchboard_program_id;

    // --- 1. Pyth (primary, mandatory) → usd_ray PriceView -----------------------------------
    let pyth_pv =
        parse_pyth(&ctx.accounts.pyth_price_update, &pyth_feed_id, &pyth_program_id, &pyth_program_id_alt)?;

    // --- 2. Switchboard (secondary, optional) → usd_ray PriceView ---------------------------
    let sb_pv = match ctx.accounts.switchboard_feed.as_ref() {
        Some(sb) => parse_switchboard(sb, &sb_feed_key, &sb_program_id)?,
        None => None,
    };

    // --- 3. DEX-TWAP corridor (already usd_ray in the ring) ---------------------------------
    let twap = ctx
        .accounts
        .dex_twap
        .load()?
        .ring()
        .twap(now, twap_window_secs, &twap_cfg);

    // --- 4. Aggregate + write spot / mode ---------------------------------------------------
    let result = aggregate(pyth_pv, sb_pv, twap, now, &cfg);
    let market = &mut ctx.accounts.market;

    // The mode always reflects the latest aggregate (mints freeze the instant feeds degrade).
    market.mint_frozen = result.mode != OracleMode::Ok;

    // Arm/extend the liquidation-divergence pause when a FRESH primary grossly disagrees with a
    // PRESENT secondary. Monotone `max` (re-armed each divergent crank); on re-convergence the pause
    // self-clears `LIQ_DIVERGENCE_GRACE_SLOTS` after the LAST divergent observation, so a snap-back
    // can't instantly cascade. Liquidation-ONLY — redemption/urgent_redeem/repay never read it.
    // Default off when `liq_max_divergence_bps == 0` (`liq_divergent` is always false).
    if result.liq_divergent {
        market.liq_divergence_until = market
            .liq_divergence_until
            .max(slot.saturating_add(crate::constants::LIQ_DIVERGENCE_GRACE_SLOTS));
    }

    // Refresh the cached price ONLY when a fresh feed backed it AND the conservative valuation is
    // usable (nonzero). Two safety properties (both flagged by the oracle-wiring review):
    //   - Freshness (staleness breaker): liquidation/redemption/withdraw ignore
    //     `mint_frozen` and gate solely on `slot - spot_updated_slot <= MAX_PRICE_STALENESS_SLOTS`.
    //     If we advanced the slot off a STALE fallback price (Pyth uses get_price_unchecked, so an
    //     old-but-signed update still parses), a keeper could re-post it every <100s to keep the
    //     cache "fresh" and liquidate at a stale valuation. So a stale aggregate (`!result.fresh`)
    //     leaves the cache to age out — the staleness gate then pauses those paths too.
    //   - Nonzero: under catastrophic confidence (`k·σ >= price`) the conservative `collateral_price`
    //     saturates to 0; writing `spot = 0` would brick liquidation/redemption (they require
    //     `spot > 0`). We keep the last good price instead (it ages out via the staleness gate).
    //   - Plausible: an implausible fresh price (outside the configured 10^k-scale band — the
    //     Sept-2021 Pyth mis-scale class, or a wrong feed rebind during the Pyth core migration) is
    //     WITHHELD exactly like a stale one. The same `spot == 0` precedent: don't commit nonsense as
    //     the liquidation/redemption price; let the cache age into the staleness machinery
    //     (repay/deposit stay open; a sustained breach → shutdown → urgent_redeem). Default band
    //     `(0, 0)` ⇒ `plausible == true` always ⇒ byte-identical to the prior behavior.
    if result.fresh && result.plausible {
        let spot = usd_ray_to_spot(result.collateral_price, coll_decimals, FUSD_DECIMALS)
            .ok_or(FusdError::MathOverflow)?;
        // The asymmetric HIGH (debt/liquidation) price = `price + k·σ`. Cached alongside `spot`
        // (the LOW price) so liquidation prices off the optimistic valuation and a wide confidence
        // band can't drive a destructive liquidation on noise. `debt_price >= collateral_price`, so
        // `debt_spot >= spot` and `spot > 0 ⇒ debt_spot > 0`.
        let debt_spot = usd_ray_to_spot(result.debt_price, coll_decimals, FUSD_DECIMALS)
            .ok_or(FusdError::MathOverflow)?;
        if spot > 0 {
            // Advances the staleness clock and arms the on-resume liquidation grace window if this
            // fresh price recovers from a staleness halt.
            market.commit_fresh_spot(spot, debt_spot, slot);
        }
    }

    // The oracle heartbeat (staleness monitors alarm when these stop): the post-aggregate state.
    emit_cpi!(crate::events::PriceCommitted {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        spot: market.spot,
        slot,
        mint_frozen: market.mint_frozen,
        fresh: result.fresh,
        plausible: result.plausible,
    });
    Ok(())
}

/// Verify + normalize the Pyth `PriceUpdateV2` to a `usd_ray` `PriceView`. Hard-errors (rather
/// than degrading) on an untrusted / wrong-feed / non-positive update — a bad crank input simply
/// reverts and leaves `spot` for the next valid post; staleness is left to `aggregate`.
fn parse_pyth(
    acc: &UncheckedAccount,
    feed_id: &[u8; 32],
    pyth_program_id: &Pubkey,
    pyth_program_id_alt: &Pubkey,
) -> Result<PriceView> {
    // Accept either configured receiver (the dual-running cutover). The alt is disabled when default.
    let owner = acc.owner;
    let owner_ok = owner == pyth_program_id
        || (*pyth_program_id_alt != Pubkey::default() && owner == pyth_program_id_alt);
    require!(owner_ok, FusdError::InvalidPriceUpdate);

    let pu = {
        let data = acc.try_borrow_data().map_err(|_| FusdError::InvalidPriceUpdate)?;
        PriceUpdateV2::try_deserialize(&mut &data[..])
            .map_err(|_| FusdError::InvalidPriceUpdate)?
    };

    // Require full Wormhole-guardian verification and feed-id binding.
    require!(
        pu.verification_level.gte(VerificationLevel::Full),
        FusdError::InvalidPriceUpdate
    );
    let price = pu
        .get_price_unchecked(feed_id)
        .map_err(|_| FusdError::InvalidPriceUpdate)?;
    require!(price.price > 0, FusdError::InvalidPriceUpdate);

    let price_ray = px_to_ray(price.price as u128, price.exponent).ok_or(FusdError::MathOverflow)?;
    let conf_ray = px_to_ray(price.conf as u128, price.exponent).ok_or(FusdError::MathOverflow)?;
    Ok(PriceView { price: price_ray, conf: conf_ray, expo: 0, publish_ts: price.publish_time })
}

/// Verify + normalize a Switchboard `PullFeedAccountData` to a `usd_ray` `PriceView`. The account
/// (owner + key) is validated hard; the VALUE degrades gracefully — an empty/non-positive result
/// returns `None` (so `aggregate` freezes mints but still prices off Pyth), never a revert.
fn parse_switchboard(
    acc: &UncheckedAccount,
    expected_key: &Pubkey,
    sb_program_id: &Pubkey,
) -> Result<Option<PriceView>> {
    require!(acc.owner == sb_program_id, FusdError::InvalidSwitchboardFeed);
    require_keys_eq!(acc.key(), *expected_key, FusdError::InvalidSwitchboardFeed);

    // Read the median result (i128, 1e18-scaled), its σ, slot, the feed's last update time, and the
    // quorum fields (how many oracle responses backed this result vs. the feed's required minimum).
    let (value, std_dev, sb_slot, ts, num_samples, min_responses) = {
        let data = acc.try_borrow_data().map_err(|_| FusdError::InvalidSwitchboardFeed)?;
        let feed = PullFeedAccountData::parse(data).map_err(|_| FusdError::InvalidSwitchboardFeed)?;
        (
            feed.result.value,
            feed.result.std_dev,
            feed.result.slot,
            feed.last_update_timestamp,
            feed.result.num_samples,
            feed.min_responses,
        )
    };

    // Degraded (uninitialized result or non-positive median) ⇒ treat as absent.
    if sb_slot == 0 || value <= 0 {
        return Ok(None);
    }
    // Quorum gate: the median must be backed by at least the feed's required number of oracle
    // responses (`min_responses`, floored at 1 so a misconfigured 0-quorum feed can't pass trivially).
    // A sub-quorum / single-oracle submission is a degraded result — treat it as absent so it can
    // only FREEZE mints, never silently drive the served price.
    if (num_samples as u32) < (min_responses).max(1) {
        return Ok(None);
    }
    // Switchboard scales by 1e18 (PRECISION = 18); normalize to RAY via expo -18. If either the
    // value or σ overflows the conversion, drop the whole SB view (degrade to None) rather than
    // fabricate a `u128::MAX` confidence that could saturate the conservative price to 0.
    let (price_ray, conf_ray) =
        match (px_to_ray(value as u128, -18), px_to_ray(std_dev.max(0) as u128, -18)) {
            (Some(p), Some(c)) => (p, c),
            _ => return Ok(None),
        };
    Ok(Some(PriceView { price: price_ray, conf: conf_ray, expo: 0, publish_ts: ts }))
}
