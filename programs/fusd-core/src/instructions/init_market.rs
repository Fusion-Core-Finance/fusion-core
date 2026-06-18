use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};

use crate::constants::{
    COLLATERAL_VAULT_SEED, CONFIG_SEED, DEFAULT_SCR_BPS, MARKET_SEED, MAX_BUCKET_WIDTH_BPS,
    MAX_LIQ_BONUS_BPS, MAX_LIQ_GAS_COMP_BPS, MAX_MCR_BPS, MAX_REDEMPTION_FEE_BPS,
    MAX_RESERVE_LAMPORTS, MAX_USER_RATE_BPS, MIN_BUCKET_WIDTH_BPS, MIN_MCR_BPS, NUM_RATE_BUCKETS,
    REDEMPTION_BITMAP_SEED,
};
use crate::errors::FusdError;
use crate::state::{Market, ProtocolConfig, RedemptionBitmap};

/// Inputs to `init_market`. (Real compile-time clamps for these land with the params
/// milestone; here we enforce only basic sanity.)
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct InitMarketArgs {
    /// Minimum collateral ratio, bps. Must be >= 100%.
    pub mcr_bps: u16,
    /// Debt ceiling, fUSD-native units.
    pub debt_ceiling: u64,
    /// Per-position SOL liquidation bond (lamports). Bounded by `MAX_RESERVE_LAMPORTS`; 0 disables.
    pub reserve_lamports: u64,
    /// Liquidator collateral gas-comp (bps). Bounded by `MAX_LIQ_GAS_COMP_BPS`; 0 disables.
    pub liq_gas_comp_bps: u16,
    /// Liquidation bonus collar (bps). Bounded by `MAX_LIQ_BONUS_BPS`; **0 = collar OFF** (seize-all).
    /// Deploy scripts pass `DEFAULT_LIQ_BONUS_BPS`. Governance-tunable later via `MarketParam::LiqBonus`.
    pub liq_bonus_bps: u16,
    /// Redemption rate-bucket width (bps). Bounded by `MIN/MAX_BUCKET_WIDTH_BPS`.
    pub bucket_width_bps: u16,
    /// Flat redemption fee (bps). Bounded by `MAX_REDEMPTION_FEE_BPS`; 0 disables.
    pub redemption_fee_bps: u16,
}

#[event_cpi]
#[derive(Accounts)]
pub struct InitMarket<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,

    /// The collateral asset. The onboarding gate is COMPLETE for the locked legacy-SPL-only
    /// stance: `Account<token::Mint>` + `Program<Token>` reject any
    /// Token-2022 mint at validation (AccountOwnedByWrongProgram) — legacy mints physically
    /// cannot carry fee/hook/pausable/delegate/state extensions — and the handler rejects the
    /// one hazard legacy CAN carry (a freeze authority). Pinned by the litesvm T22 regression.
    pub collateral_mint: Box<Account<'info, Mint>>,

    #[account(
        init,
        payer = authority,
        space = Market::SPACE,
        seeds = [MARKET_SEED, collateral_mint.key().as_ref()],
        bump,
    )]
    pub market: Box<Account<'info, Market>>,

    #[account(
        init,
        payer = authority,
        seeds = [COLLATERAL_VAULT_SEED, collateral_mint.key().as_ref()],
        bump,
        token::mint = collateral_mint,
        token::authority = market,
    )]
    pub collateral_vault: Box<Account<'info, TokenAccount>>,

    #[account(
        init,
        payer = authority,
        space = RedemptionBitmap::SPACE,
        seeds = [REDEMPTION_BITMAP_SEED, collateral_mint.key().as_ref()],
        bump,
    )]
    pub redemption_bitmap: AccountLoader<'info, RedemptionBitmap>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

pub fn handler(ctx: Context<InitMarket>, args: InitMarketArgs) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );
    require!(
        ctx.accounts.collateral_mint.freeze_authority.is_none(),
        FusdError::CollateralHasFreezeAuthority
    );
    require!(
        args.mcr_bps >= MIN_MCR_BPS && args.mcr_bps <= MAX_MCR_BPS,
        FusdError::ParamOutOfBounds
    );
    require!(args.reserve_lamports <= MAX_RESERVE_LAMPORTS, FusdError::ParamOutOfBounds);
    require!(args.liq_gas_comp_bps <= MAX_LIQ_GAS_COMP_BPS, FusdError::ParamOutOfBounds);
    require!(args.liq_bonus_bps <= MAX_LIQ_BONUS_BPS, FusdError::ParamOutOfBounds);
    // Relational bounds: the same predicates the governance queue/execute
    // path asserts, so a jointly-lethal combo (collar unfundable at MCR, RP-negative gas comp,
    // MCR <= SCR shutdown inversion) can never exist at birth either. The market is created with
    // `scr_bps = DEFAULT_SCR_BPS`, so that is the value the ordering bound binds against.
    crate::instructions::governance::validate_market_config(
        args.mcr_bps as u64,
        DEFAULT_SCR_BPS as u64,
        args.liq_bonus_bps as u64,
        args.liq_gas_comp_bps as u64,
    )?;
    require!(
        args.bucket_width_bps >= MIN_BUCKET_WIDTH_BPS && args.bucket_width_bps <= MAX_BUCKET_WIDTH_BPS,
        FusdError::ParamOutOfBounds
    );
    // The bitmap must address the WHOLE valid rate range: `width · NUM_RATE_BUCKETS >= MAX_USER_RATE_BPS`.
    // Otherwise `rate_bucket::bucket_of` clamps every rate above `width · 256` into the top bucket,
    // collapsing the per-rate redemption ordering for that range (the bitmap is fUSD's ordering
    // primitive). With the default width 10 the span is 256 × 10 = 2560 ≥ MAX_USER_RATE_BPS = 2550
    // (one bucket of headroom); this check makes any other chosen width honor the same coverage invariant.
    require!(
        (args.bucket_width_bps as usize)
            .checked_mul(NUM_RATE_BUCKETS)
            .is_some_and(|span| span >= MAX_USER_RATE_BPS as usize),
        FusdError::ParamOutOfBounds
    );
    require!(args.redemption_fee_bps <= MAX_REDEMPTION_FEE_BPS, FusdError::ParamOutOfBounds);

    // Zero-initialize the redemption bitmap (all buckets empty).
    ctx.accounts.redemption_bitmap.load_init()?;

    let m = &mut ctx.accounts.market;
    m.collateral_mint = ctx.accounts.collateral_mint.key();
    m.collateral_vault = ctx.accounts.collateral_vault.key();
    // Weighted-debt-sum interest accounting (BOLD). All zero at genesis; interest accrues off the
    // per-position `user_rate_bps` once positions borrow.
    m.agg_recorded_debt = 0;
    m.agg_weighted_debt_sum = 0;
    m.unminted_interest = 0;
    m.last_update_ts = Clock::get()?.unix_timestamp;
    m.spot = 0;
    m.debt_spot = 0; // HIGH (debt/liquidation) price; 0 until the first crank, like `spot`
    m.spot_updated_slot = 0;
    m.mcr_bps = args.mcr_bps;
    m.debt_ceiling = args.debt_ceiling;
    m.collateral_decimals = ctx.accounts.collateral_mint.decimals;
    m.bump = ctx.bumps.market;
    m.vault_bump = ctx.bumps.collateral_vault;
    // Redistribution accumulators start at the genesis (all-zero) state.
    m.l_coll = 0;
    m.l_art = 0;
    m.last_coll_redist_error = 0;
    m.last_art_redist_error = 0;
    m.total_stakes = 0;
    m.total_collateral = 0;
    m.total_stakes_snapshot = 0;
    m.total_collateral_snapshot = 0;
    m.reserve_lamports = args.reserve_lamports;
    m.liq_gas_comp_bps = args.liq_gas_comp_bps;
    m.liq_bonus_bps = args.liq_bonus_bps;
    m.total_coll_surplus = 0;
    m.bucket_width_bps = args.bucket_width_bps;
    m.redemption_fee_bps = args.redemption_fee_bps;
    m.surplus_collateral = 0;
    // Mints start FROZEN: a market cannot borrow until the oracle crank (`update_price`) writes a
    // fresh, non-degraded aggregate. Matches `spot == 0` already blocking borrow; explicit here so
    // the gate is set even before the first crank.
    m.mint_frozen = true;
    // No guardian pause at open (0 ⇒ `now >= 0` always true ⇒ not paused).
    m.guardian_paused_until = 0;
    // Markets open live; shutdown is set only by the permissionless `shutdown` on a failure trigger.
    m.shutdown = false;
    m.scr_bps = DEFAULT_SCR_BPS;
    // Rate limiter starts DISABLED (cap 0); governance enables a calibrated cap post-launch.
    m.rl_cap = 0;
    m.rl_accrued = 0;
    m.rl_last_update = 0;
    // CCR borrow-restriction band starts DISABLED (0); governance enables a calibrated CCR.
    m.ccr_bps = 0;
    // No on-resume liquidation grace active at open; armed only by `commit_fresh_spot` on a stall→resume.
    m.liq_grace_until = 0;
    // No oracle-divergence pause at open; armed only by `update_price` on a fresh-vs-secondary
    // divergence (and only when liq_max_divergence_bps is enabled).
    m.liq_divergence_until = 0;
    m.bad_debt = 0;
    m.shutdown_reason = crate::constants::SHUTDOWN_REASON_NONE;
    // Dust floor + premature-rate-change cooldown + keeper reward start DISABLED (0); governance enables
    // calibrated values post-launch, the same default-off pattern as the rate limiter / CCR.
    m.min_debt = 0;
    m.rate_adjust_cooldown_secs = 0;
    m.keeper_reward_bps = 0;
    // Upfront borrowing fee starts DISABLED (0); governance enables a calibrated value (C7).
    m.borrow_fee_bps = 0;
    // Un-homed retained collateral (the `bad_debt` offset) + global-backstop per-market counters
    // all start at 0 (no liquidations/contributions/draws yet). Explicit so every Market field's
    // genesis value is readable from this handler rather than left to Anchor's implicit zero-fill.
    m.protocol_collateral = 0;
    m.global_contributed = 0;
    m.global_drawn = 0;
    m._reserved = [0u8; 38];

    emit_cpi!(crate::events::MarketInitialized {
        collateral_mint: m.collateral_mint,
        mcr_bps: args.mcr_bps,
        debt_ceiling: args.debt_ceiling,
        bucket_width_bps: args.bucket_width_bps,
        liq_bonus_bps: args.liq_bonus_bps,
    });
    Ok(())
}
