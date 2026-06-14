//! DEV/TEST ONLY (feature `dev-oracle`). Sets a market's cached collateral price so the
//! CDP flow is testable before the real oracle (fusd-oracle + Pyth/Switchboard) is wired.
//! NEVER compiled into mainnet builds; the real oracle crank replaces this.
#![cfg(feature = "dev-oracle")]

use anchor_lang::prelude::*;

use crate::constants::CONFIG_SEED;
use crate::errors::FusdError;
use crate::state::{Market, ProtocolConfig};

#[derive(Accounts)]
pub struct DevSetPrice<'info> {
    pub authority: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, ProtocolConfig>,

    #[account(mut)]
    pub market: Account<'info, Market>,
}

/// `spot` = RAY-scaled fUSD-native per 1 native collateral unit (see `Market.spot`).
pub fn handler(ctx: Context<DevSetPrice>, spot: u128) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );
    let m = &mut ctx.accounts.market;
    // Same freshness-clock writer as the production `update_price` crank, so the dev/test path arms
    // the on-resume liquidation grace window on a stall-then-resume exactly like production.
    // Dev mode models no confidence band, so the HIGH (debt/liquidation) price equals the LOW spot —
    // making the debt-price asymmetry a deliberate no-op for the existing dev-oracle liquidation suite (only
    // the real `update_price` crank and the dedicated asymmetry test exercise `debt_spot != spot`).
    m.commit_fresh_spot(spot, spot, Clock::get()?.slot);
    // Simulate a healthy oracle aggregate: clear the mint freeze so the dev/test CDP flow can
    // borrow (production markets start frozen and are only unfrozen by a real `update_price`).
    m.mint_frozen = false;
    Ok(())
}
