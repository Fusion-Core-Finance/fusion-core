//! Anyone: advance to RECONCILE when the cluster epoch exceeds the controller epoch.
//!
//! **Preemption:** the reconcile-entry condition is EPOCH-based, not phase-based — this
//! instruction fires from ANY phase once `Clock::epoch > controller_epoch`. A cycle stranded
//! mid-phase across an epoch boundary can never finish (stale-plan CPIs fail the upstream
//! staleness gate, so its cursors can never complete) and its plan is for a stale finalized
//! NAV; the only sound recovery is to discard it and reconcile fresh. Unfinished physical
//! moves carry forward through native transient/reserve mechanics and are re-observed by the
//! new reconcile pass. In the normal path this is exactly IDLE → RECONCILE.
//!
//! Resets every cursor, plan aggregate, round mirror, churn/payout budget counter and the
//! liveness-guard aggregates. The finalized NAV snapshot fields deliberately SURVIVE: the
//! negative-NAV comparison at the next finalize and the provisional churn budget both read the
//! previous finalized values (the budget is refreshed from the new snapshot at FINALIZE).
//!
//! No reward — not a listed task class (a single trivial write).

use anchor_lang::prelude::*;

use crate::constants::{CONTROLLER_SEED, EPOCH_STATE_SEED, GLOBAL_CHURN_CAP_BPS};
use crate::errors::ControllerError;
use crate::logic::phase_transition_allowed;
use crate::state::{ControllerConfig, EpochState, PHASE_RECONCILE};
use fusion_stake_math::churn::global_churn_budget;

#[event_cpi]
#[derive(Accounts)]
pub struct StartEpoch<'info> {
    #[account(
        seeds = [CONTROLLER_SEED],
        bump = config.bump,
        constraint = config.sealed @ ControllerError::PoolNotInitialized,
    )]
    pub config: Box<Account<'info, ControllerConfig>>,

    #[account(mut, seeds = [EPOCH_STATE_SEED], bump)]
    pub epoch_state: AccountLoader<'info, EpochState>,
}

pub fn handler(ctx: Context<StartEpoch>) -> Result<()> {
    let clock = Clock::get()?;
    let mut es = ctx.accounts.epoch_state.load_mut()?;

    require!(clock.epoch > es.controller_epoch, ControllerError::EpochNotAdvanced);
    let from_phase = es.phase;
    // Structurally always true (`* -> RECONCILE`); kept as the single choke point every
    // transition goes through.
    require!(phase_transition_allowed(from_phase, PHASE_RECONCILE), ControllerError::WrongPhase);

    es.reset_for_new_epoch();
    es.controller_epoch = clock.epoch;
    // Provisional churn budget from the PREVIOUS finalized total (0 at genesis); FINALIZE
    // refreshes it from the new snapshot before REBALANCE can spend any of it.
    es.churn_budget_total = global_churn_budget(es.nav_total_lamports, GLOBAL_CHURN_CAP_BPS);
    es.phase = PHASE_RECONCILE;
    let epoch = es.controller_epoch;
    drop(es);

    emit_cpi!(crate::events::EpochPhaseChanged {
        epoch,
        from_phase,
        to_phase: PHASE_RECONCILE,
        slot: clock.slot,
    });
    Ok(())
}
