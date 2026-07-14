//! Close a preference account and refund its rent: by the position owner at any time, or by
//! ANYONE when the preference is dead weight — its Fusion position no longer exists at that
//! address, or the position parses with zero ink (zero-weight direction is worthless; the
//! owner can always `set_preference` again later).
//!
//! Rent ALWAYS refunds to the recorded `preference.owner` (the `close =` target is
//! handler-pinned to it), so a permissionless close can never redirect value — it can only do
//! the owner the favor of reclaiming their rent.

use anchor_lang::prelude::*;

use crate::constants::{CONTROLLER_SEED, PREFERENCE_SEED};
use crate::errors::ControllerError;
use crate::state::{ControllerConfig, Preference};

#[event_cpi]
#[derive(Accounts)]
pub struct ClosePreference<'info> {
    /// The position owner, or anyone when the underlying position no longer exists (or holds
    /// zero ink).
    pub closer: Signer<'info>,

    #[account(seeds = [CONTROLLER_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ControllerConfig>>,

    /// CHECK: the (possibly already-closed) fusd-core `Position` this preference refers to;
    /// the handler decides the authorization rule from its live state.
    pub fusion_position: UncheckedAccount<'info>,

    #[account(
        mut,
        close = rent_recipient,
        seeds = [PREFERENCE_SEED, fusion_position.key().as_ref()],
        bump = preference.bump,
    )]
    pub preference: Box<Account<'info, Preference>>,

    /// CHECK: rent destination — handler-pinned to `preference.owner` so a permissionless
    /// close can never redirect the refund.
    #[account(mut)]
    pub rent_recipient: UncheckedAccount<'info>,
}

pub fn handler(ctx: Context<ClosePreference>) -> Result<()> {
    // Rent goes to the recorded owner, whoever signs.
    require!(
        ctx.accounts.rent_recipient.key() == ctx.accounts.preference.owner,
        ControllerError::InvalidRentRecipient
    );

    // Does a live, non-empty Position still exist at that address? (Closed accounts are
    // system-owned/zero-data; a reused address that no longer parses as a Position is equally
    // "gone" for this purpose — fail toward permissionless closability of dead weight.)
    let live_ink = if *ctx.accounts.fusion_position.owner == ctx.accounts.config.fusd_core_program
    {
        let data = ctx.accounts.fusion_position.try_borrow_data()?;
        fusion_stake_view::position::parse(&data).map(|p| p.ink).unwrap_or(0)
    } else {
        0
    };
    if live_ink > 0 {
        // The position is alive and weighted: only its recorded owner may drop the direction.
        require!(
            ctx.accounts.closer.key() == ctx.accounts.preference.owner,
            ControllerError::PositionStillOpen
        );
        // Close+recreate must not bypass the once-per-epoch change limit: a live-ink preference
        // changed THIS cluster epoch stays put until the next one (set_preference's fresh-init
        // path has no change_epoch to check, so the limit is enforced at the exit instead).
        // Direction safety never depended on this (eligible_from_epoch already blocks same-epoch
        // counting of any recreate) — this pins the documented anti-churn rule itself.
        require!(
            ctx.accounts.preference.change_epoch != Clock::get()?.epoch,
            ControllerError::PreferenceChangeLimit2
        );
    }

    let preference = &ctx.accounts.preference;
    emit_cpi!(crate::events::PreferenceUpdated {
        fusion_position: ctx.accounts.fusion_position.key(),
        owner: preference.owner,
        vote_account: preference.vote_account,
        op: crate::events::PREF_OP_CLOSED,
        observed_ink: preference.observed_ink,
        observed_ink_nonce: preference.observed_ink_nonce,
        eligible_from_epoch: preference.eligible_from_epoch,
        epoch: Clock::get()?.epoch,
    });
    Ok(()) // `close = rent_recipient` performs the lamport refund + account wipe
}
