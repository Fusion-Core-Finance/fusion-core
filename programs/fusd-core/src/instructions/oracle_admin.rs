//! Oracle infrastructure admin: the bounded-updatable oracle PROGRAM IDs and the
//! per-market feed REBIND path, both gated on `ProtocolConfig.gov_authority` (the deliberative
//! futarchy/Squads-controlled authority — the same gate as `init_market_oracle`).
//!
//! Motivation: Pyth's core program migration (~2026-07-31) changes the receiver program ID, and
//! Fusion's endgame is an IMMUTABLE program. Hard-coded oracle program IDs would be a time bomb — a
//! binary pinned to the old program reads permanently-stale accounts (→ mint freeze → oracle-failure
//! shutdown). Moving the IDs into `ProtocolConfig` (with a gov-gated setter) and adding a feed-rebind
//! path lets the migration be absorbed by a transaction, not a redeploy. These are feed-INFRASTRUCTURE
//! bindings, not risk params — they cannot mint/move/freeze/seize and they never touch the per-market
//! liquidation/redemption math, so they sit on the `gov_authority` admin lane, not the timelocked
//! `MarketParam` lane.

use anchor_lang::prelude::*;
use anchor_spl::token::Mint;

use crate::constants::{CONFIG_SEED, MARKET_ORACLE_SEED};
use crate::errors::FusdError;
use crate::state::{MarketOracle, ProtocolConfig};

// ----------------------------------------- set_oracle_program_ids --------------------------------

#[event_cpi]
#[derive(Accounts)]
pub struct SetOracleProgramIds<'info> {
    /// MUST equal `config.gov_authority`.
    pub authority: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,
}

/// Update the bounded-updatable oracle program IDs. `None` leaves a field unchanged. The
/// primary Pyth receiver and the Switchboard program must be real IDs (never `Pubkey::default()`,
/// which would brick the crank); the **alt** Pyth receiver MAY be set to `Pubkey::default()` —
/// that DISABLES the second accepted receiver (e.g. the post-cutover defense-in-depth cleanup, after
/// promoting the upgraded receiver to primary).
pub fn set_program_ids(
    ctx: Context<SetOracleProgramIds>,
    new_pyth_receiver: Option<Pubkey>,
    new_pyth_receiver_alt: Option<Pubkey>,
    new_switchboard: Option<Pubkey>,
) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );
    let config = &mut ctx.accounts.config;
    let old_pyth = config.pyth_receiver_program_id;
    let old_pyth_alt = config.pyth_receiver_program_id_alt;
    let old_switchboard = config.switchboard_program_id;

    if let Some(p) = new_pyth_receiver {
        require!(p != Pubkey::default(), FusdError::ParamOutOfBounds);
        config.pyth_receiver_program_id = p;
    }
    if let Some(a) = new_pyth_receiver_alt {
        // default IS permitted here — it disables the second accepted receiver.
        config.pyth_receiver_program_id_alt = a;
    }
    if let Some(s) = new_switchboard {
        require!(s != Pubkey::default(), FusdError::ParamOutOfBounds);
        config.switchboard_program_id = s;
    }

    emit_cpi!(crate::events::OracleProgramIdsUpdated {
        old_pyth,
        new_pyth: config.pyth_receiver_program_id,
        old_pyth_alt,
        new_pyth_alt: config.pyth_receiver_program_id_alt,
        old_switchboard,
        new_switchboard: config.switchboard_program_id,
    });
    Ok(())
}

// ----------------------------------------- rebind_market_oracle_feeds ----------------------------

/// Inputs to `rebind_market_oracle_feeds`. The full feed-binding set is re-supplied (the caller
/// passes the current value for any binding it is not changing) and re-validated exactly as
/// `init_market_oracle` validates them at birth. Thresholds, the plausibility band, the asymmetry
/// factor, and the TWAP guards are NOT touched — only the feed sources move.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct RebindOracleFeedsArgs {
    pub pyth_feed_id: [u8; 32],
    pub switchboard_feed: Pubkey,
    pub orca_pool: Pubkey,
    pub raydium_pool: Pubkey,
}

#[event_cpi]
#[derive(Accounts)]
pub struct RebindMarketOracleFeeds<'info> {
    /// MUST equal `config.gov_authority`.
    pub authority: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,

    pub collateral_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        seeds = [MARKET_ORACLE_SEED, collateral_mint.key().as_ref()],
        bump = market_oracle.bump,
    )]
    pub market_oracle: Box<Account<'info, MarketOracle>>,
}

/// Rebind a market's oracle feed SOURCES — the Pyth feed id, the Switchboard feed account,
/// and the DEX-TWAP pool accounts — for the Pyth core migration or a feed-account format change. The
/// DEX-TWAP ring is left intact: a rebind targets the SAME collateral asset, so existing samples (USD
/// prices) stay valid. Re-runs the init-time binding validation so a rebind can never land an
/// unverifiable feed.
pub fn rebind_feeds(ctx: Context<RebindMarketOracleFeeds>, args: RebindOracleFeedsArgs) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );

    // Same binding validation as `init_market_oracle`: real feed bindings + at least one (distinct) pool.
    require!(args.pyth_feed_id != [0u8; 32], FusdError::ParamOutOfBounds);
    require!(args.switchboard_feed != Pubkey::default(), FusdError::ParamOutOfBounds);
    require!(
        args.orca_pool != Pubkey::default() || args.raydium_pool != Pubkey::default(),
        FusdError::ParamOutOfBounds
    );
    require!(
        args.orca_pool == Pubkey::default()
            || args.raydium_pool == Pubkey::default()
            || args.orca_pool != args.raydium_pool,
        FusdError::ParamOutOfBounds
    );

    let o = &mut ctx.accounts.market_oracle;
    o.pyth_feed_id = args.pyth_feed_id;
    o.switchboard_feed = args.switchboard_feed;
    o.orca_pool = args.orca_pool;
    o.raydium_pool = args.raydium_pool;

    emit_cpi!(crate::events::OracleFeedsRebound {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        pyth_feed_id: args.pyth_feed_id,
        switchboard_feed: args.switchboard_feed,
        orca_pool: args.orca_pool,
        raydium_pool: args.raydium_pool,
    });
    Ok(())
}
