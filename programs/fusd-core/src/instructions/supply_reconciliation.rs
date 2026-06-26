//! Supply reconciliation (proof-of-reserves) — `init` (gov_authority-gated) + permissionless
//! `reconcile_supply`.
//!
//! Re-derives FUSD's sharded global supply invariant
//!     `mint.supply == Σ_market (agg_recorded_debt − unminted_interest + bad_debt)`
//! from the live `Market` accounts and compares it to the mint supply, stamping the result on the
//! `SupplyReconciliation` singleton + an event. Auditability only — never gates a user path. The sum
//! is computed underflow-safe (`Σ(agg + bad)` and `Σ unminted` separately) because a market freshly
//! past a terminal liquidation can carry `unminted > agg` while still satisfying `agg + bad ≥ unminted`.

use anchor_lang::prelude::*;
use anchor_spl::token::Mint;

use crate::constants::{CONFIG_SEED, FUSD_MINT_SEED, MAX_RECONCILE_MARKETS, SUPPLY_RECON_SEED};
use crate::errors::FusdError;
use crate::state::{Market, ProtocolConfig, SupplyReconciliation};

// ----------------------------------------- init -----------------------------------------

#[derive(Accounts)]
pub struct InitSupplyReconciliation<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,

    #[account(
        init,
        payer = authority,
        space = SupplyReconciliation::SPACE,
        seeds = [SUPPLY_RECON_SEED],
        bump,
    )]
    pub supply_recon: Box<Account<'info, SupplyReconciliation>>,

    pub system_program: Program<'info, System>,
}

/// One-time: create the global supply-reconciliation singleton. Gated on `config.gov_authority`.
pub fn init(ctx: Context<InitSupplyReconciliation>) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );
    let r = &mut ctx.accounts.supply_recon;
    r.last_ts = 0;
    r.last_market_count = 0;
    r.last_mint_supply = 0;
    r.last_backing = 0;
    r.last_residual = 0;
    r.bump = ctx.bumps.supply_recon;
    r._reserved = [0u8; 32];
    Ok(())
}

// --------------------------------- reconcile (permissionless) ------------------------------

#[derive(Accounts)]
pub struct ReconcileSupply<'info> {
    /// Permissionless caller (signs only to carry the tx). No authority check — the crank only READS
    /// markets + the mint and writes the observability singleton; it moves no funds and gates nothing.
    pub cranker: Signer<'info>,

    #[account(seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Box<Account<'info, Mint>>,

    #[account(mut, seeds = [SUPPLY_RECON_SEED], bump = supply_recon.bump)]
    pub supply_recon: Box<Account<'info, SupplyReconciliation>>,
    // remaining_accounts: the live `Market` accounts to sum (the off-chain monitor passes ALL of them).
}

/// Sum `agg_recorded_debt`/`unminted_interest`/`bad_debt` over the submitted `Market` accounts,
/// reconstruct the backed supply, and compare to the live mint supply. Stamps the singleton + emits.
/// Reverts only on malformed input (a non-`Market` account, a duplicate, or > `MAX_RECONCILE_MARKETS`)
/// — NEVER on a non-zero residual (the residual is the signal; reverting would blind the monitor when
/// drift exists). Each market is verified program-owned + deduped so the sum can't be inflated.
pub fn reconcile<'info>(ctx: Context<'_, '_, 'info, 'info, ReconcileSupply<'info>>) -> Result<()> {
    require!(
        ctx.remaining_accounts.len() <= MAX_RECONCILE_MARKETS,
        FusdError::TooManyRedemptionCandidates
    );

    let mut sum_agg_plus_bad: u128 = 0;
    let mut sum_unminted: u128 = 0;
    let mut seen: Vec<Pubkey> = Vec::with_capacity(ctx.remaining_accounts.len());

    for info in ctx.remaining_accounts.iter() {
        // Program-owned + the real Market discriminator (try_from checks both); a non-Market account
        // can't be slipped in to skew the sum.
        require_keys_eq!(*info.owner, crate::ID, FusdError::InvalidRecipient);
        let key = info.key();
        require!(!seen.contains(&key), FusdError::DuplicateRedemptionTarget);
        seen.push(key);
        let market = Account::<Market>::try_from(info)?;

        sum_agg_plus_bad = sum_agg_plus_bad
            .checked_add(market.agg_recorded_debt)
            .and_then(|s| s.checked_add(market.bad_debt))
            .ok_or(FusdError::MathOverflow)?;
        sum_unminted = sum_unminted
            .checked_add(market.unminted_interest)
            .ok_or(FusdError::MathOverflow)?;
    }

    // Reconstructed circulating: Σ(agg + bad) − Σ unminted. Non-negative in aggregate (each market's
    // circulating share `agg − unminted + bad ≥ 0`), so the subtraction can't underflow.
    let backing = sum_agg_plus_bad.saturating_sub(sum_unminted);
    let mint_supply = ctx.accounts.fusd_mint.supply as u128;
    // residual = backing − mint_supply (signed): 0 = reconciled; sign tells over/under or missing-market.
    let residual = (backing as i128).saturating_sub(mint_supply as i128);

    let now = Clock::get()?.unix_timestamp;
    let r = &mut ctx.accounts.supply_recon;
    r.last_ts = now;
    r.last_market_count = ctx.remaining_accounts.len() as u32;
    r.last_mint_supply = mint_supply;
    r.last_backing = backing;
    r.last_residual = residual;

    emit!(crate::events::SupplyReconciled {
        market_count: ctx.remaining_accounts.len() as u32,
        mint_supply,
        backing,
        residual,
    });
    Ok(())
}
