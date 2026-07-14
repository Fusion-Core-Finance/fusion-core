//! Anyone, at/after the deadline slot: freeze the epoch's direction totals and enter
//! PLAN-DIRECTED.
//!
//! The direction totals live epoch-stamped on the ValidatorRecords (snapshots never write
//! `EpochState`); "freezing" is therefore structural — snapshots require
//! `phase == PREFERENCES && slot < close_slot`, so from this transition on no further count
//! can land for this epoch. The undirected supply is derived where the directed aggregate is
//! built: PLAN-DIRECTED checked-sums `D` from the records and `U = S − D` emerges in
//! `neutral_total` (the plan-obligations placement of the `D ≤ S` guard).
//!
//! Zeroes every plan accumulator/cursor/round field for the coming plan passes. No reward —
//! a trivial transition.

use anchor_lang::prelude::*;

use crate::constants::{CONTROLLER_SEED, EPOCH_STATE_SEED};
use crate::errors::ControllerError;
use crate::logic::phase_transition_allowed;
use crate::state::{ControllerConfig, EpochState, PHASE_PLAN_DIRECTED, PHASE_PREFERENCES};

#[event_cpi]
#[derive(Accounts)]
pub struct ClosePreferenceWindow<'info> {
    #[account(
        seeds = [CONTROLLER_SEED],
        bump = config.bump,
        constraint = config.sealed @ ControllerError::PoolNotInitialized,
    )]
    pub config: Box<Account<'info, ControllerConfig>>,

    #[account(mut, seeds = [EPOCH_STATE_SEED], bump)]
    pub epoch_state: AccountLoader<'info, EpochState>,
}

pub fn handler(ctx: Context<ClosePreferenceWindow>) -> Result<()> {
    let clock = Clock::get()?;
    let mut es = ctx.accounts.epoch_state.load_mut()?;
    require!(es.phase == PHASE_PREFERENCES, ControllerError::WrongPhase);
    require!(
        clock.slot >= es.preference_window_close_slot,
        ControllerError::PreferenceWindowStillOpen
    );
    require!(
        phase_transition_allowed(es.phase, PHASE_PLAN_DIRECTED),
        ControllerError::WrongPhase
    );

    es.reset_plan_state();
    es.phase = PHASE_PLAN_DIRECTED;
    let epoch = es.controller_epoch;
    drop(es);

    emit_cpi!(crate::events::EpochPhaseChanged {
        epoch,
        from_phase: PHASE_PREFERENCES,
        to_phase: PHASE_PLAN_DIRECTED,
        slot: clock.slot,
    });
    Ok(())
}
