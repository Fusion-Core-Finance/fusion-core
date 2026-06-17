//! `shutdown` — the terminal per-market circuit breaker (fusion-docs §4.x).
//!
//! Permissionless: anyone may wind a market down once it is genuinely failing — its aggregate
//! collateral ratio has fallen below `scr_bps` (assessed on a fresh price), OR its oracle has been
//! dead for a sustained period. There is **no discretionary trigger** (no guardian/gov key): the
//! one lever that can permanently close a market must be condition-gated only, so it can't become a
//! coercible kill switch. The flag is **irreversible** — the only remedy is migration.
//!
//! On shutdown, `borrow` and the ordered `redeem` close, and `urgent_redeem` (unordered, 0-fee,
//! face value at the last price) opens — so the peg floor stays open during the wind-down.
//! Per-market only: a shutdown of one collateral never touches another market.

use anchor_lang::prelude::*;
use anchor_spl::token::Mint;

use crate::cdp;
use crate::constants::{
    MARKET_SEED, MAX_PRICE_STALENESS_SLOTS, SHUTDOWN_ORACLE_STALENESS_SLOTS,
    SHUTDOWN_REASON_ORACLE_FAILURE, SHUTDOWN_REASON_SCR,
};
use crate::accrual;
use crate::errors::FusdError;
use crate::state::Market;

#[event_cpi]
#[derive(Accounts)]
pub struct Shutdown<'info> {
    /// Permissionless caller (signs only to carry the tx). No authority check.
    pub cranker: Signer<'info>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,
}

pub fn handler(ctx: Context<Shutdown>) -> Result<()> {
    require!(!ctx.accounts.market.shutdown, FusdError::MarketShutdown);
    // Fold aggregate interest so the TCR check sees the current present debt.
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    accrual::accrue(&mut ctx.accounts.market, now)?;

    let m = &ctx.accounts.market;
    let slot = clock.slot;
    let staleness = slot.saturating_sub(m.spot_updated_slot);

    // Oracle failure: a market that WAS priced (`spot > 0`) has gone stale past the outage
    // threshold. A never-priced market (`spot == 0`) is pre-launch, not failed — so a fresh market
    // can't be griefed into a terminal shutdown.
    let oracle_failed = m.spot > 0 && staleness > SHUTDOWN_ORACLE_STALENESS_SLOTS;

    // Aggregate undercollateralization — assessed ONLY on a fresh price (a stale price can't
    // soundly assert TCR < SCR; that window is covered by the oracle-failure trigger instead).
    let tcr_breach = m.spot > 0
        && staleness <= MAX_PRICE_STALENESS_SLOTS
        && cdp::tcr_below(m.total_collateral, m.agg_recorded_debt, m.spot, m.scr_bps);

    require!(oracle_failed || tcr_breach, FusdError::ShutdownConditionNotMet);

    // Record the named reason alongside the flag (oracle failure takes precedence when both hold).
    let reason = if oracle_failed { SHUTDOWN_REASON_ORACLE_FAILURE } else { SHUTDOWN_REASON_SCR };
    ctx.accounts.market.shutdown = true;
    ctx.accounts.market.shutdown_reason = reason;

    emit_cpi!(crate::events::ShutdownEvent {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        reason,
    });
    Ok(())
}
