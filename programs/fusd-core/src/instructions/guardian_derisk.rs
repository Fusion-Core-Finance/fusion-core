//! `guardian_derisk` — the independent emergency brake (fusion-docs §7.2).
//!
//! A guardian role that is **independent of the governance authority** (so a frozen or
//! compromised governance system cannot also freeze fUSD's emergency response) can pause NEW
//! borrowing on a market without the
//! governance timelock. This is the protocol's single discretionary emergency lever, and its
//! envelope is constitutional and deliberately tiny:
//!
//! - It pauses ONLY `borrow` (new debt). Repay, withdraw, liquidation, and redemption ignore it —
//!   the guardian can never touch existing positions, user funds, or the peg floor.
//! - It **auto-lifts**: the pause is a `now + pause_secs` timestamp, clamped to at most
//!   `GUARDIAN_MAX_PAUSE_SECS`. A captured guardian can re-assert it, but re-pausing NEW borrowing
//!   is the least-harmful power imaginable — it can never ratchet the protocol permanently shut.
//! - `pause_secs = 0` lifts an active pause early (the guardian relaxing its own measure).
//!
//! It cannot seize, freeze funds, mint, raise MCR, or impede redemption — none of those code paths
//! exist here. Lowering ceilings / raising fees stay with the bounded `GovernanceGate`.

use anchor_lang::prelude::*;
use anchor_spl::token::Mint;

use crate::constants::{CONFIG_SEED, GUARDIAN_MAX_PAUSE_SECS, MARKET_SEED};
use crate::errors::FusdError;
use crate::state::{Market, ProtocolConfig};

#[event_cpi]
#[derive(Accounts)]
pub struct GuardianDerisk<'info> {
    /// The de-risk guardian (must equal `ProtocolConfig.guardian`). Independent of gov_authority.
    pub guardian: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, ProtocolConfig>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,
}

/// Pause new borrowing on the market for `pause_secs` (clamped to `GUARDIAN_MAX_PAUSE_SECS`).
/// `pause_secs = 0` lifts an active pause. Only the configured guardian may call it.
pub fn handler(ctx: Context<GuardianDerisk>, pause_secs: i64) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.guardian.key(),
        ctx.accounts.config.guardian,
        FusdError::Unauthorized
    );
    require!(
        (0..=GUARDIAN_MAX_PAUSE_SECS).contains(&pause_secs),
        FusdError::ParamOutOfBounds
    );

    let now = Clock::get()?.unix_timestamp;
    // Absolute deadline (auto-lifts when `now` passes it). Each call sets a fresh window from now,
    // so `pause_secs = 0` ⇒ `now` ⇒ not paused (an early lift). `saturating_add` cannot overflow
    // for any in-range `pause_secs`, but is used for total safety.
    ctx.accounts.market.guardian_paused_until = now.saturating_add(pause_secs);

    emit_cpi!(crate::events::GuardianDerisked {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        guardian: ctx.accounts.guardian.key(),
        paused_until: ctx.accounts.market.guardian_paused_until,
    });
    Ok(())
}
