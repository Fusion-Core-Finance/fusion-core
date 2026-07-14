//! Position owner: select (or change) the validator direction for one fuSOL Fusion position.
//! Records the position's current `(ink, ink_nonce, owner)` and delays eligibility to the NEXT
//! epoch; at most one change per epoch (`Preference.change_epoch`).
//!
//! The Fusion `Position` is READ-only here (owner check + `fusion_stake_view::position` byte
//! parse) — this program never writes fusd-core state, and fusd-core debt paths never require
//! this account. Direction toward a Draining/Removable validator is rejected up front (its
//! lifecycle cap is 0, the direction could never take effect); any other status is accepted —
//! direction toward a merely-Registered validator is exactly how admission support accrues.
//!
//! Epoch frames: preference writes (`change_epoch`, `eligible_from_epoch`) use the CLUSTER
//! epoch; snapshot counting uses the controller epoch (`<=` cluster). The delay therefore
//! always spans at least one full controller cycle — the conservative direction.

use anchor_lang::prelude::*;

use crate::constants::{CONTROLLER_SEED, PREFERENCE_SEED, VALIDATOR_RECORD_SEED};
use crate::errors::ControllerError;
use crate::state::{ControllerConfig, Preference, ValidatorRecord};
use fusion_stake_math::lifecycle::ValidatorStatus;

#[event_cpi]
#[derive(Accounts)]
pub struct SetPreference<'info> {
    /// The Fusion position owner (verified against the parsed `Position.owner` in the
    /// handler); pays rent on first use.
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(seeds = [CONTROLLER_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ControllerConfig>>,

    /// CHECK: the fusd-core `Position` — owner-checked against `config.fusd_core_program` and
    /// byte-parsed (`fusion_stake_view::position`) in the handler; NEVER written (Fusion debt
    /// paths stay fully independent of this program).
    pub fusion_position: UncheckedAccount<'info>,

    /// CHECK: the selected validator's vote account (the record PDA below proves a
    /// registration exists for it).
    pub vote_account: UncheckedAccount<'info>,

    #[account(
        seeds = [VALIDATOR_RECORD_SEED, vote_account.key().as_ref()],
        bump = validator_record.bump,
    )]
    pub validator_record: Box<Account<'info, ValidatorRecord>>,

    /// Created on first set, reused on change (`init_if_needed` — the once-per-epoch change
    /// counter must survive re-targeting; close_preference additionally rejects a live-ink close in the change epoch, so close+recreate cannot reset the limit).
    #[account(
        init_if_needed,
        payer = owner,
        space = Preference::SPACE,
        seeds = [PREFERENCE_SEED, fusion_position.key().as_ref()],
        bump,
    )]
    pub preference: Box<Account<'info, Preference>>,

    pub system_program: Program<'info, System>,
}

pub fn handler(ctx: Context<SetPreference>) -> Result<()> {
    let config = &ctx.accounts.config;

    // The position must be a REAL fusd-core Position (runtime owner check first, then the
    // pinned byte parse), in the fuSOL collateral market, owned by the signer.
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
    require!(
        position.owner == ctx.accounts.owner.key().to_bytes(),
        ControllerError::PreferenceOwnerMismatch
    );

    // Directing a dead-end validator can never take effect (lifecycle cap 0) — reject up
    // front instead of silently recording a worthless direction.
    let status = ValidatorStatus::from_u8(ctx.accounts.validator_record.status)
        .ok_or(ControllerError::CorruptValidatorStatus)?;
    require!(
        !matches!(status, ValidatorStatus::Draining | ValidatorStatus::Removable),
        ControllerError::ValidatorNotEligibleForPreference
    );

    let epoch = Clock::get()?.epoch;
    let preference = &mut ctx.accounts.preference;
    let fresh = preference.version == 0; // init_if_needed zero-initializes new accounts
    if fresh {
        preference.version = 1;
        preference.fusion_position = ctx.accounts.fusion_position.key();
        preference.bump = ctx.bumps.preference;
        // last_counted_epoch starts 0 (never counted).
    } else {
        // At most one change per epoch — reduces churn and keeps epoch snapshots
        // deterministic. (A nonce refresh without re-targeting is the permissionless
        // `sync_preference`, which this limit does not apply to.)
        require!(preference.change_epoch != epoch, ControllerError::PreferenceChangeLimit);
    }

    preference.owner = ctx.accounts.owner.key();
    preference.vote_account = ctx.accounts.vote_account.key();
    preference.observed_ink_nonce = position.ink_nonce;
    preference.observed_ink = position.ink;
    preference.eligible_from_epoch = epoch.checked_add(1).ok_or(ControllerError::MathOverflow)?;
    preference.change_epoch = epoch;

    emit_cpi!(crate::events::PreferenceUpdated {
        fusion_position: ctx.accounts.fusion_position.key(),
        owner: ctx.accounts.owner.key(),
        vote_account: ctx.accounts.vote_account.key(),
        op: crate::events::PREF_OP_SET,
        observed_ink: position.ink,
        observed_ink_nonce: position.ink_nonce,
        eligible_from_epoch: preference.eligible_from_epoch,
        epoch,
    });
    Ok(())
}
