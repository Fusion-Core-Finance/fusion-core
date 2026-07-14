//! Anyone: refresh a preference's observed `(ink, ink_nonce, owner)` after a collateral
//! change, delaying eligibility to the next epoch (`eligible_from_epoch = epoch + 1` — the
//! anti-reuse delay). Permissionless so indexers/keepers can keep users' directions current;
//! syncing can never harm the owner (it only restores direction the collateral change
//! suspended).
//!
//! **No-op guard:** when the live `(ink_nonce, owner)` already equals the recorded pair, the
//! call succeeds WITHOUT writing anything — in particular without touching
//! `eligible_from_epoch`. A permissionless sync that unconditionally re-delayed eligibility
//! would let a griefer keep any preference perpetually one epoch away from countability by
//! syncing it every epoch. The delay applies exactly when a real change is recorded, which is
//! the anti-reuse property the spec requires — nothing more.

use anchor_lang::prelude::*;

use crate::constants::{CONTROLLER_SEED, PREFERENCE_SEED};
use crate::errors::ControllerError;
use crate::state::{ControllerConfig, Preference};

#[event_cpi]
#[derive(Accounts)]
pub struct SyncPreference<'info> {
    #[account(seeds = [CONTROLLER_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ControllerConfig>>,

    /// CHECK: the fusd-core `Position` — owner-checked against `config.fusd_core_program` and
    /// byte-parsed in the handler; never written.
    pub fusion_position: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [PREFERENCE_SEED, fusion_position.key().as_ref()],
        bump = preference.bump,
    )]
    pub preference: Box<Account<'info, Preference>>,
}

pub fn handler(ctx: Context<SyncPreference>) -> Result<()> {
    let config = &ctx.accounts.config;

    require!(
        *ctx.accounts.fusion_position.owner == config.fusd_core_program,
        ControllerError::InvalidPositionAccount
    );
    let position = {
        let data = ctx.accounts.fusion_position.try_borrow_data()?;
        fusion_stake_view::position::parse(&data)
            .map_err(|_| error!(ControllerError::InvalidPositionAccount))?
    };
    require!(
        position.collateral_mint == config.fusol_collateral_mint.to_bytes(),
        ControllerError::InvalidPositionAccount
    );

    let preference = &mut ctx.accounts.preference;

    // No-op guard (see module doc): nothing changed, nothing written, delay untouched.
    if position.ink_nonce == preference.observed_ink_nonce
        && position.owner == preference.owner.to_bytes()
    {
        return Ok(());
    }

    let epoch = Clock::get()?.epoch;
    preference.owner = Pubkey::new_from_array(position.owner);
    preference.observed_ink_nonce = position.ink_nonce;
    preference.observed_ink = position.ink;
    preference.eligible_from_epoch = epoch.checked_add(1).ok_or(ControllerError::MathOverflow)?;
    // The selected vote account is NOT touched here — re-targeting is the owner-gated
    // `set_preference`.

    emit_cpi!(crate::events::PreferenceUpdated {
        fusion_position: ctx.accounts.fusion_position.key(),
        owner: preference.owner,
        vote_account: preference.vote_account,
        op: crate::events::PREF_OP_SYNCED,
        observed_ink: position.ink,
        observed_ink_nonce: position.ink_nonce,
        eligible_from_epoch: preference.eligible_from_epoch,
        epoch,
    });
    Ok(())
}
