use anchor_lang::prelude::*;

use crate::constants::REACTOR_GRID_LEN;

/// Per-market Reactor Pool: fUSD depositors who absorb liquidated debt and earn the seized
/// collateral at a discount, in O(1) per liquidation. PDA `[b"reactor", collateral_mint]`.
/// Scalar fields mirror `fusd_math::reactor_pool::PoolState`. fusion-docs.md.
#[account]
#[derive(Debug)]
pub struct ReactorPool {
    pub collateral_mint: Pubkey,
    /// fUSD deposit vault (authority = this RP PDA).
    pub fusd_vault: Pubkey,
    /// Seized-collateral vault awaiting depositor claims (authority = this RP PDA).
    pub coll_vault: Pubkey,
    /// The bounded epoch→scale→sum grid account (zero-copy).
    pub epoch_to_scale_to_sum: Pubkey,
    /// Running product `P` (1e18-scaled); starts at `DECIMAL_PRECISION`.
    pub p: u128,
    pub epoch: u64,
    pub scale: u64,
    /// Current total fUSD deposited.
    pub total_deposits: u128,
    pub last_coll_error: u128,
    pub last_loss_error: u128,
    pub bump: u8,
    pub _reserved: [u8; 64],
}

impl ReactorPool {
    pub const SPACE: usize = 8 + (32 * 4) + 16 + 8 + 8 + 16 + 16 + 16 + 1 + 64;
}

/// The bounded epoch×scale collateral-gain-per-unit grid — one `u128` per cell (single
/// collateral per isolated market). Direct indexing `epoch * REACTOR_MAX_SCALES + scale`; the
/// `offset` math reverts if exhausted (never wraps → no silent gain loss). Zero-copy, following
/// Hubble/USDH's `[u128; N]` precedent. PDA `[b"ess", collateral_mint]`.
#[account(zero_copy)]
#[repr(C)]
pub struct EpochToScaleToSum {
    pub data: [u128; REACTOR_GRID_LEN],
}

impl EpochToScaleToSum {
    pub const SPACE: usize = 8 + REACTOR_GRID_LEN * 16;
}

// Layout pins: SPACE-vs-layout drift and tail padding become compile errors.
const _: () = assert!(EpochToScaleToSum::SPACE == 8 + core::mem::size_of::<EpochToScaleToSum>());
const _: () = assert!(core::mem::size_of::<EpochToScaleToSum>() % 8 == 0);

/// A depositor's stake in a Reactor Pool. PDA `[b"reactor_dep", collateral_mint, owner]`.
/// Realize-on-interaction: provide/withdraw fold the accrued collateral gain into
/// `pending_collateral_gain` and reset the snapshot.
#[account]
#[derive(Debug)]
pub struct ReactorDeposit {
    pub owner: Pubkey,
    pub reactor_pool: Pubkey,
    /// Recorded deposit at last interaction (Liquity's `initialDeposit`).
    pub deposited_fusd: u64,
    // snapshot {P, S, scale, epoch}
    pub snapshot_p: u128,
    pub snapshot_s: u128,
    pub snapshot_scale: u64,
    pub snapshot_epoch: u64,
    /// Realized-but-unclaimed seized collateral (native units).
    pub pending_collateral_gain: u64,
    pub bump: u8,
    pub _reserved: [u8; 32],
}

impl ReactorDeposit {
    pub const SPACE: usize = 8 + 32 + 32 + 8 + 16 + 16 + 8 + 8 + 8 + 1 + 32;
}
