//! Anyone, during the preference window: count one valid position's current ink into the
//! selected validator's epoch directed weight. One count per epoch per preference; a counted
//! snapshot stays in the epoch's target even if the owner later exits (the destination
//! position's nonce change + eligibility delay prevent double-direction).
//!
//! `EpochState` is READ-ONLY here — snapshots write only the Preference and the selected
//! validator's record, so snapshots for distinct validators parallelize under Sealevel and can
//! never contend with the crank write lane. The epoch-level `D ≤ S` guard is enforced where
//! the aggregate is built: PLAN-DIRECTED checked-sums every record's epoch-stamped
//! `directed_shares` into `EpochState.total_directed_shares` (the plan-obligations placement;
//! the per-record adds here are checked too, so no intermediate can wrap).
//!
//! Epoch frame: countability, the once-per-epoch stamp and the shares stamp all use the
//! CONTROLLER epoch (the cycle whose plan this window feeds), not the live cluster epoch.
//!
//! The full countable predicate (`fusion_stake_math::preference::countable`) re-reads the LIVE
//! position bytes: mint, owner, nonce, eligibility delay, and not-already-counted must ALL
//! hold. Draining/Removable validators reject (their lifecycle cap is 0 — the shares stay
//! neutral, which is where a stale direction belongs); Registered/Candidate/Active count
//! (directed support toward a Registered validator is the admission signal).

use anchor_lang::prelude::*;

use crate::constants::{CONTROLLER_SEED, EPOCH_STATE_SEED, PREFERENCE_SEED, VALIDATOR_RECORD_SEED};
use crate::errors::ControllerError;
use crate::state::{ControllerConfig, EpochState, Preference, ValidatorRecord, PHASE_PREFERENCES};
use fusion_stake_math::lifecycle::ValidatorStatus;
use fusion_stake_math::preference::{countable, PositionView, PreferenceView};

#[event_cpi]
#[derive(Accounts)]
pub struct SnapshotPreference<'info> {
    #[account(seeds = [CONTROLLER_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ControllerConfig>>,

    /// Read-only: phase + window-deadline checks (snapshots never advance the crank machine,
    /// keeping them parallel across validators).
    #[account(seeds = [EPOCH_STATE_SEED], bump)]
    pub epoch_state: AccountLoader<'info, EpochState>,

    /// CHECK: the fusd-core `Position` — owner-checked against `config.fusd_core_program` and
    /// byte-parsed in the handler; never written.
    pub fusion_position: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [PREFERENCE_SEED, fusion_position.key().as_ref()],
        bump = preference.bump,
    )]
    pub preference: Box<Account<'info, Preference>>,

    /// The record of the preference's SELECTED validator (seeded by the stored vote account,
    /// so a snapshot can never credit a different validator).
    #[account(
        mut,
        seeds = [VALIDATOR_RECORD_SEED, preference.vote_account.as_ref()],
        bump = validator_record.bump,
    )]
    pub validator_record: Box<Account<'info, ValidatorRecord>>,
}

pub fn handler(ctx: Context<SnapshotPreference>) -> Result<()> {
    let config = &ctx.accounts.config;

    // Window gate: only while the preference phase is open AND before the deadline slot
    // (strictly before — `close_preference_window` owns the deadline slot itself).
    let (controller_epoch, window_close_slot) = {
        let es = ctx.accounts.epoch_state.load()?;
        require!(es.phase == PHASE_PREFERENCES, ControllerError::PreferenceWindowClosed);
        (es.controller_epoch, es.preference_window_close_slot)
    };
    require!(Clock::get()?.slot < window_close_slot, ControllerError::PreferenceWindowClosed);

    // Live position read (owner check first, then the pinned byte parse).
    require!(
        *ctx.accounts.fusion_position.owner == config.fusd_core_program,
        ControllerError::InvalidPositionAccount
    );
    let position = {
        let data = ctx.accounts.fusion_position.try_borrow_data()?;
        fusion_stake_view::position::parse(&data)
            .map_err(|_| error!(ControllerError::InvalidPositionAccount))?
    };

    // The FULL countable predicate over the live bytes (mint, owner, nonce, delay, once).
    let preference = &ctx.accounts.preference;
    let pref_view = PreferenceView {
        owner: preference.owner.to_bytes(),
        observed_ink_nonce: preference.observed_ink_nonce,
        eligible_from_epoch: preference.eligible_from_epoch,
        last_counted_epoch: preference.last_counted_epoch,
    };
    let pos_view = PositionView {
        owner: position.owner,
        collateral_mint: position.collateral_mint,
        ink: position.ink,
        ink_nonce: position.ink_nonce,
    };
    require!(
        countable(&pref_view, &pos_view, &config.fusol_collateral_mint.to_bytes(), controller_epoch),
        ControllerError::PreferenceNotCountable
    );

    // Lifecycle gate: shares directed at a Draining/Removable validator stay neutral.
    let record = &mut ctx.accounts.validator_record;
    let status =
        ValidatorStatus::from_u8(record.status).ok_or(ControllerError::CorruptValidatorStatus)?;
    require!(
        !matches!(status, ValidatorStatus::Draining | ValidatorStatus::Removable),
        ControllerError::ValidatorNotEligibleForPreference
    );

    // Epoch-stamped accumulation: a stale stamp self-clears (no epoch-start clearing pass
    // over all records is ever needed).
    if record.directed_shares_epoch != controller_epoch {
        record.directed_shares_epoch = controller_epoch;
        record.directed_shares = 0;
    }
    record.directed_shares = record
        .directed_shares
        .checked_add(position.ink)
        .ok_or(ControllerError::MathOverflow)?;

    let preference = &mut ctx.accounts.preference;
    preference.last_counted_epoch = controller_epoch;

    emit_cpi!(crate::events::PreferenceUpdated {
        fusion_position: ctx.accounts.fusion_position.key(),
        owner: preference.owner,
        vote_account: preference.vote_account,
        op: crate::events::PREF_OP_COUNTED,
        observed_ink: position.ink,
        observed_ink_nonce: position.ink_nonce,
        eligible_from_epoch: preference.eligible_from_epoch,
        epoch: controller_epoch,
    });
    Ok(())
}
