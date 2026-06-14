//! Reactor-Pool glue: bridges the on-chain accounts to the tested
//! `fusd_math::reactor_pool` math. See fusion-docs.md.

use anchor_lang::prelude::*;
use fusd_math::reactor_pool::{self as rpm, PoolState, Snapshot};

use crate::constants::REACTOR_MAX_SCALES;
use crate::errors::FusdError;
use crate::state::{ReactorDeposit, ReactorPool};

/// Build the math `PoolState` from the on-chain account.
pub fn pool_state(reactor: &ReactorPool) -> PoolState {
    PoolState {
        p: reactor.p,
        epoch: reactor.epoch,
        scale: reactor.scale,
        total_deposits: reactor.total_deposits,
        last_coll_error: reactor.last_coll_error,
        last_loss_error: reactor.last_loss_error,
    }
}

/// Write a mutated `PoolState` back to the on-chain account.
pub fn write_back(reactor: &mut ReactorPool, ps: &PoolState) {
    reactor.p = ps.p;
    reactor.epoch = ps.epoch;
    reactor.scale = ps.scale;
    reactor.total_deposits = ps.total_deposits;
    reactor.last_coll_error = ps.last_coll_error;
    reactor.last_loss_error = ps.last_loss_error;
}

fn snapshot_of(dep: &ReactorDeposit) -> Snapshot {
    Snapshot { p: dep.snapshot_p, s: dep.snapshot_s, scale: dep.snapshot_scale, epoch: dep.snapshot_epoch }
}

/// Realize the depositor's accrued collateral gain into `pending_collateral_gain` and return
/// their current compounded fUSD deposit. Does NOT reset the snapshot — the caller does that
/// (via [`set_snapshot`]) after adjusting `deposited_fusd`.
pub fn realize(ps: &PoolState, dep: &mut ReactorDeposit, grid: &[u128]) -> Result<u128> {
    let snap = snapshot_of(dep);
    let initial = dep.deposited_fusd as u128;
    let compounded = rpm::compounded_deposit(ps, initial, &snap);
    let gain = rpm::collateral_gain(grid, REACTOR_MAX_SCALES, initial, &snap).map_err(map_err)?;
    let gain_u64 = u64::try_from(gain).map_err(|_| FusdError::MathOverflow)?;
    dep.pending_collateral_gain = dep
        .pending_collateral_gain
        .checked_add(gain_u64)
        .ok_or(FusdError::MathOverflow)?;
    Ok(compounded)
}

/// Reset the depositor's snapshot to the pool's current `(p, S[epoch,scale], scale, epoch)`.
pub fn set_snapshot(dep: &mut ReactorDeposit, ps: &PoolState, grid: &[u128]) {
    let snap = ps.snapshot(grid, REACTOR_MAX_SCALES);
    dep.snapshot_p = snap.p;
    dep.snapshot_s = snap.s;
    dep.snapshot_scale = snap.scale;
    dep.snapshot_epoch = snap.epoch;
}

pub fn map_err(e: rpm::ReactorError) -> FusdError {
    match e {
        rpm::ReactorError::NoDeposits | rpm::ReactorError::DebtExceedsDeposits => FusdError::ReactorPoolTooSmall,
        rpm::ReactorError::ScaleOverflow | rpm::ReactorError::EpochOverflow => FusdError::ReactorGridExhausted,
        rpm::ReactorError::Math => FusdError::MathOverflow,
    }
}
