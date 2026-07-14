use anchor_lang::prelude::*;

/// The controller's immutable address book. PDA `[b"controller"]`, created once by
/// `initialize_controller` (which only RECORDS predeclared addresses — the stake-pool-side
/// accounts need not exist yet) and frozen by `initialize_pool` (`sealed = true`).
///
/// No authority is stored anywhere in this account: the payer funds creation and keeps
/// nothing. Every recorded address is compared (`address =` constraints) on each later
/// instruction, so a mis-built transaction can never smuggle a foreign account into a CPI.
#[account]
#[derive(Debug)]
pub struct ControllerConfig {
    /// Layout version byte (1).
    pub version: u8,
    /// `false` between `initialize_controller` and `initialize_pool`; `true` forever after.
    /// While unsealed, deposits are rejected and the one-time pool CPI is still available.
    pub sealed: bool,

    /// The pinned stake-pool FORK program (== `constants::FUSION_STAKE_POOL_PROGRAM_ID`;
    /// recorded so the audit config manifest reads it from one on-chain account).
    pub stake_pool_program: Pubkey,
    /// The `StakePool` state account (pre-created, program-owned, initialized by our CPI).
    pub stake_pool: Pubkey,
    /// The pool's `ValidatorList` account (pre-created at exactly `MAX_VALIDATORS` capacity).
    pub validator_list: Pubkey,
    /// The pool's reserve stake account.
    pub reserve_stake: Pubkey,
    /// The fuSOL mint (legacy SPL, 9 decimals, freeze authority None, mint authority = the
    /// pool withdraw-authority PDA).
    pub fusol_mint: Pubkey,
    /// The stake-pool program's withdraw-authority PDA
    /// (`[stake_pool, b"withdraw"]` under the FORK program id) — derived and recorded at init.
    pub pool_withdraw_authority: Pubkey,
    /// The maintenance vault: a fuSOL token account, authority = the `[b"maintenance"]` PDA.
    /// Doubles as the stake pool's manager fee account, so every pool fee lands here.
    pub maintenance_vault: Pubkey,
    /// fusd-core — owner of the `Position` accounts the Preference layer reads.
    pub fusd_core_program: Pubkey,
    /// The collateral mint a Fusion `Position` must hold for its preference to count. Recorded
    /// as its own named field (the Preference path never derives it); at genesis it MUST equal
    /// `fusol_mint` — the fuSOL market's collateral mint IS the pool mint — and
    /// `initialize_controller` enforces that equality.
    pub fusol_collateral_mint: Pubkey,

    /// Canonical bump of this `[b"controller"]` PDA.
    pub bump: u8,
    /// Canonical bump of the `[b"pool_authority"]` PDA (manager + staker signer).
    pub pool_authority_bump: u8,
    /// Canonical bump of the `[b"deposit_authority"]` PDA (SOL + stake deposit signer).
    pub deposit_authority_bump: u8,
    /// Canonical bump of the `[b"maintenance"]` PDA (maintenance vault token authority).
    pub maintenance_authority_bump: u8,

    /// Forward-compat reserve. Carve from the HEAD; zeroed bytes on old accounts must decode
    /// as the new field's `0 = disabled/none` sentinel (fusd-core discipline).
    pub _reserved: [u8; 64],
}

impl ControllerConfig {
    pub const SPACE: usize = 8 // discriminator
        + 1 + 1                // version, sealed
        + 32 * 9               // the nine recorded addresses
        + 4                    // bump, pool_authority_bump, deposit_authority_bump, maintenance_authority_bump
        + 64; // _reserved
}
