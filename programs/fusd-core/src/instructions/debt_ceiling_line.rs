//! Debt-ceiling **auto-line** (Maker DC-IAM analog) — `init` / `set` (gov_authority-gated) +
//! permissionless `bump`.
//!
//! The auto-line lets a market's effective `Market.debt_ceiling` track utilization: a permissionless
//! crank raises it toward `MIN(line, agg_recorded_debt + gap)` in `gap`-sized steps, no more often
//! than every `ttl` seconds, never above the gov-set hard `line`. The hot `borrow` path is unchanged
//! — it always reads `Market.debt_ceiling`; the auto-line only *moves* that value within `[debt, line]`.
//! Opt-in: a market without a `DebtCeilingLine` account behaves exactly as before (static ceiling, set
//! only via the gate's `DebtCeiling` param).
//!
//! Security: `init`/`set` are gov_authority-gated (raising the cap is governance's call); the
//! permissionless `bump` can never push the ceiling past `line`, so opening the crank to anyone grants
//! no authority over the cap — only the throttled, bounded follow-the-utilization motion.

use anchor_lang::prelude::*;
use anchor_spl::token::Mint;

use crate::accrual;
use crate::constants::{CONFIG_SEED, MARKET_SEED, RATE_LIMITER_SEED};
use crate::errors::FusdError;
use crate::state::{DebtCeilingLine, Market, ProtocolConfig};

/// Apply the auto-line's target to the market's live ceiling and emit. Shared by `init` / `set`
/// (immediate, gov-driven) and the permissionless `bump` (throttled). Scalar params (not the account)
/// so the caller's `&mut DebtCeilingLine` borrow is released before the `&mut Market` write.
fn apply_and_emit(line: u64, gap: u64, ttl: i64, market: &mut Market, collateral_mint: Pubkey) {
    let target = line.min(
        u64::try_from(market.agg_recorded_debt)
            .unwrap_or(u64::MAX)
            .saturating_add(gap),
    );
    market.debt_ceiling = target;
    // Plain `emit!` (not the CPI transport): a single small observability struct; indexers can also
    // read the account directly. No funds move here.
    emit!(crate::events::DebtCeilingLineBumped {
        collateral_mint,
        debt_ceiling: target,
        line,
        gap,
        ttl,
        agg_recorded_debt: market.agg_recorded_debt,
    });
}

// ----------------------------------------- init -----------------------------------------

#[derive(Accounts)]
pub struct InitDebtCeilingLine<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,

    pub collateral_mint: Box<Account<'info, Mint>>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,

    #[account(
        init,
        payer = authority,
        space = DebtCeilingLine::SPACE,
        seeds = [RATE_LIMITER_SEED, collateral_mint.key().as_ref()],
        bump,
    )]
    pub debt_ceiling_line: Box<Account<'info, DebtCeilingLine>>,

    pub system_program: Program<'info, System>,
}

/// Create a market's auto-line and apply its initial ceiling immediately. Gated on
/// `config.gov_authority`. `ttl < 0` is rejected; `line`/`gap` are unclamped (a ceiling, like
/// `Market.debt_ceiling`).
pub fn init(ctx: Context<InitDebtCeilingLine>, line: u64, gap: u64, ttl: i64) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );
    require!(ttl >= 0, FusdError::ParamOutOfBounds);
    let now = Clock::get()?.unix_timestamp;
    // Accrue first so the initial ceiling is computed against current utilization.
    accrual::accrue(&mut ctx.accounts.market, now)?;

    let dcl = &mut ctx.accounts.debt_ceiling_line;
    dcl.collateral_mint = ctx.accounts.collateral_mint.key();
    dcl.line = line;
    dcl.gap = gap;
    dcl.ttl = ttl;
    dcl.last_bump_ts = now;
    dcl.bump = ctx.bumps.debt_ceiling_line;
    dcl._reserved = [0u8; 32];

    apply_and_emit(line, gap, ttl, &mut ctx.accounts.market, ctx.accounts.collateral_mint.key());
    Ok(())
}

// ----------------------------------------- set -----------------------------------------

#[derive(Accounts)]
pub struct SetDebtCeilingLine<'info> {
    pub authority: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,

    pub collateral_mint: Box<Account<'info, Mint>>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,

    #[account(
        mut,
        seeds = [RATE_LIMITER_SEED, collateral_mint.key().as_ref()],
        bump = debt_ceiling_line.bump,
    )]
    pub debt_ceiling_line: Box<Account<'info, DebtCeilingLine>>,
}

/// Governance updates `line`/`gap`/`ttl` and applies the new ceiling IMMEDIATELY (governance is
/// trusted within clamps; a tightening should bind at once). Gated on `config.gov_authority`.
pub fn set(ctx: Context<SetDebtCeilingLine>, line: u64, gap: u64, ttl: i64) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );
    require!(ttl >= 0, FusdError::ParamOutOfBounds);
    let now = Clock::get()?.unix_timestamp;
    accrual::accrue(&mut ctx.accounts.market, now)?;

    let dcl = &mut ctx.accounts.debt_ceiling_line;
    dcl.line = line;
    dcl.gap = gap;
    dcl.ttl = ttl;
    dcl.last_bump_ts = now;

    apply_and_emit(line, gap, ttl, &mut ctx.accounts.market, ctx.accounts.collateral_mint.key());
    Ok(())
}

// ----------------------------------------- bump (permissionless) -----------------------------------

#[derive(Accounts)]
pub struct BumpDebtCeiling<'info> {
    /// Permissionless caller (signs only to carry the tx). No authority check — the bump can never
    /// raise the ceiling past the gov-set `line`.
    pub cranker: Signer<'info>,

    pub collateral_mint: Box<Account<'info, Mint>>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,

    #[account(
        mut,
        seeds = [RATE_LIMITER_SEED, collateral_mint.key().as_ref()],
        bump = debt_ceiling_line.bump,
    )]
    pub debt_ceiling_line: Box<Account<'info, DebtCeilingLine>>,
}

/// Permissionless: move `Market.debt_ceiling` toward `MIN(line, agg_recorded_debt + gap)`, throttled
/// to once per `ttl`. Reverts `RateLimitExceeded` if called before `last_bump_ts + ttl` (the DC-IAM
/// throttle — reuses the nearest existing error code; this is the auto-line's rate limit).
pub fn bump(ctx: Context<BumpDebtCeiling>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let dcl_ttl = ctx.accounts.debt_ceiling_line.ttl;
    let last = ctx.accounts.debt_ceiling_line.last_bump_ts;
    require!(now >= last.saturating_add(dcl_ttl), FusdError::RateLimitExceeded);

    // Bring utilization current so the follow-the-debt target is accurate.
    accrual::accrue(&mut ctx.accounts.market, now)?;
    let dcl = &mut ctx.accounts.debt_ceiling_line;
    dcl.last_bump_ts = now;
    let (line, gap, ttl) = (dcl.line, dcl.gap, dcl.ttl);

    apply_and_emit(line, gap, ttl, &mut ctx.accounts.market, ctx.accounts.collateral_mint.key());
    Ok(())
}
