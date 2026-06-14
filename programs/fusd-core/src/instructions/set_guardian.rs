//! `set_guardian` — governance rotates/revokes the de-risk guardian (see fusion-docs.md).
//!
//! The guardian (the independent emergency brake, see `guardian_derisk`) is set at
//! `init_protocol`; this lets `gov_authority` rotate it — to recover from a compromised or lost
//! guardian key, or to hand it to a different role. Gated on `config.gov_authority` and applied
//! **immediately** (no timelock): the guardian's only power is the bounded, harmless borrow pause
//! (it can never touch funds), so fast revocation of a captured key is the right tradeoff, and a
//! timelock would only delay revoking a bad actor. Setting it to `Pubkey::default()` revokes the
//! guardian entirely (no key can then pause; any active pause still auto-lifts).
//!
//! This does NOT weaken the guardian's independence-from-a-frozen-DAO property: a frozen
//! `gov_authority` cannot rotate the guardian either, so the brake keeps working when governance
//! cannot act — which is exactly what the independence is for.

use anchor_lang::prelude::*;

use crate::constants::CONFIG_SEED;
use crate::errors::FusdError;
use crate::state::ProtocolConfig;

#[event_cpi]
#[derive(Accounts)]
pub struct SetGuardian<'info> {
    /// Must equal `config.gov_authority` (the bootstrap/admin authority).
    pub authority: Signer<'info>,

    #[account(mut, seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, ProtocolConfig>,
}

pub fn handler(ctx: Context<SetGuardian>, new_guardian: Pubkey) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );
    let previous = ctx.accounts.config.guardian;
    ctx.accounts.config.guardian = new_guardian;

    emit_cpi!(crate::events::GuardianRotated { previous, new_guardian });
    Ok(())
}
