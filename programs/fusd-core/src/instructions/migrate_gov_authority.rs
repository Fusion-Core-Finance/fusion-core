//! Two-step propose/accept rotation for `ProtocolConfig.gov_authority` (the bootstrap/admin
//! authority gating `init_market`, `init_market_oracle`, `init_reactor_pool`,
//! `init_insurance_buffer`, `init_governance_gate`, and `set_guardian`).
//!
//! Mirrors the `GovernanceGate` inbound-authority handshake (governance.rs): the live key moves
//! ONLY when the proposed successor itself signs the accept, so a handoff to a dead/mistyped key
//! can never strand the role — the roadmap's governance-minimization path requires handing this
//! authority to a successor signer or PDA, and before this instruction existed there was NO
//! transfer path at all (a lost admin key would have permanently frozen market onboarding and
//! guardian rotation).
//!
//! Deliberately NOT two-step: `set_guardian` (fast revocation of a compromised guardian is
//! load-bearing, and a typo'd guardian is benign — equivalent to revoke, recoverable).

use anchor_lang::prelude::*;

use crate::constants::CONFIG_SEED;
use crate::errors::FusdError;
use crate::state::ProtocolConfig;

// ----------------------------------------- migrate_gov_authority ---------------------------------

#[event_cpi]
#[derive(Accounts)]
pub struct MigrateGovAuthority<'info> {
    /// MUST equal the CURRENT `config.gov_authority` — the admin proposes its own successor.
    pub authority: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,
}

/// Step 1 — propose. `new_authority == Pubkey::default()` clears a pending handoff (cancel); any
/// other value records a pending successor that does NOT take effect until it accepts. Never
/// touches the live `gov_authority`.
pub fn migrate(ctx: Context<MigrateGovAuthority>, new_authority: Pubkey) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );
    ctx.accounts.config.pending_gov_authority = new_authority;

    emit_cpi!(crate::events::GovAuthorityProposed {
        current: ctx.accounts.config.gov_authority,
        pending: new_authority,
    });
    Ok(())
}

// ----------------------------------------- accept_gov_authority ----------------------------------

#[event_cpi]
#[derive(Accounts)]
pub struct AcceptGovAuthority<'info> {
    /// MUST equal `config.pending_gov_authority` — the proposed successor proves control by
    /// signing. This is what makes the handoff two-step (the incoming key can't be a typo).
    pub new_authority: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,
}

/// Step 2 — accept. The pending successor signs; only then does the live authority move. The
/// live `gov_authority` can never become `Pubkey::default()` via this path (the pending-nonzero
/// require below).
pub fn accept(ctx: Context<AcceptGovAuthority>) -> Result<()> {
    let config = &mut ctx.accounts.config;
    require!(
        config.pending_gov_authority != Pubkey::default(),
        FusdError::NoPendingAuthority
    );
    require_keys_eq!(
        ctx.accounts.new_authority.key(),
        config.pending_gov_authority,
        FusdError::Unauthorized
    );
    let previous = config.gov_authority;
    config.gov_authority = config.pending_gov_authority;
    config.pending_gov_authority = Pubkey::default();

    emit_cpi!(crate::events::GovAuthorityMigrated {
        previous,
        new_authority: config.gov_authority,
    });
    Ok(())
}
