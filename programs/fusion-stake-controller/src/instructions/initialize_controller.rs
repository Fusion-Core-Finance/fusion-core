//! One-time genesis: create `ControllerConfig` + `EpochState` and RECORD the predeclared
//! address set. The stake-pool-side accounts (pool, list, reserve, mint, vault) need not exist
//! yet — this instruction stores addresses only; `initialize_pool` later validates the live
//! accounts against them and performs the one-time stake-pool `Initialize` CPI.
//!
//! No authority is stored: the payer funds account creation and keeps nothing. The payer must
//! however hold this program's UPGRADE authority (the fusd-core `init_protocol` gate): the
//! `[b"controller"]` PDA is deterministic, so without the gate anyone could front-run genesis
//! with hostile addresses and permanently brick the deployment (the config has no setters, by
//! design). Once the program's upgrade authority is burned at launch sealing, this instruction
//! becomes uncallable — which is correct, it has already run.

use anchor_lang::prelude::*;
use anchor_lang::ProgramData;

use crate::constants::{
    CONTROLLER_SEED, DEPOSIT_AUTHORITY_SEED, EPOCH_STATE_SEED, FUSD_CORE_PROGRAM_ID,
    FUSION_STAKE_POOL_PROGRAM_ID, MAINTENANCE_AUTHORITY_SEED, POOL_AUTHORITY_SEED,
};
use crate::errors::ControllerError;
use crate::state::{ControllerConfig, EpochState};

/// The predeclared stake-pool-side addresses (spec §"initialization without a persistent
/// bootstrap key": every account is created at a predetermined address, then verified).
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct InitializeControllerArgs {
    /// The pre-created (program-owned, uninitialized) `StakePool` account.
    pub stake_pool: Pubkey,
    /// The pre-created `ValidatorList` account (sized to exactly `MAX_VALIDATORS`).
    pub validator_list: Pubkey,
    /// The pool reserve stake account.
    pub reserve_stake: Pubkey,
    /// The fuSOL mint.
    pub fusol_mint: Pubkey,
    /// The maintenance vault (fuSOL token account, authority = the `[b"maintenance"]` PDA);
    /// doubles as the stake pool's manager fee account.
    pub maintenance_vault: Pubkey,
}

#[event_cpi]
#[derive(Accounts)]
pub struct InitializeController<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    /// This program's `ProgramData` account (BPF upgradeable loader), derived as the canonical
    /// programdata PDA so it cannot be spoofed. Gates genesis to the program's UPGRADE
    /// authority — front-run protection only; nothing about the payer is recorded.
    #[account(
        seeds = [crate::ID.as_ref()],
        bump,
        seeds::program = anchor_lang::solana_program::bpf_loader_upgradeable::ID,
        constraint = program_data.upgrade_authority_address == Some(payer.key())
            @ ControllerError::InvalidConfigAddress,
    )]
    pub program_data: Account<'info, ProgramData>,

    #[account(
        init,
        payer = payer,
        space = ControllerConfig::SPACE,
        seeds = [CONTROLLER_SEED],
        bump,
    )]
    pub config: Box<Account<'info, ControllerConfig>>,

    #[account(
        init,
        payer = payer,
        space = EpochState::SPACE,
        seeds = [EPOCH_STATE_SEED],
        bump,
    )]
    pub epoch_state: AccountLoader<'info, EpochState>,

    /// CHECK: the `[b"pool_authority"]` PDA — the stake pool's manager + staker. Included only
    /// so its canonical bump is derived and recorded; it holds no data and never will.
    #[account(seeds = [POOL_AUTHORITY_SEED], bump)]
    pub pool_authority: UncheckedAccount<'info>,

    /// CHECK: the `[b"deposit_authority"]` PDA — the pool's SOL + stake deposit authority.
    #[account(seeds = [DEPOSIT_AUTHORITY_SEED], bump)]
    pub deposit_authority: UncheckedAccount<'info>,

    /// CHECK: the `[b"maintenance"]` PDA — the maintenance vault's token authority.
    #[account(seeds = [MAINTENANCE_AUTHORITY_SEED], bump)]
    pub maintenance_authority: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

pub fn handler(ctx: Context<InitializeController>, args: InitializeControllerArgs) -> Result<()> {
    // Reject default pubkeys: a zeroed address here would permanently wedge `initialize_pool`
    // (nothing can ever exist at the default address with the required owners/authorities).
    for addr in [
        &args.stake_pool,
        &args.validator_list,
        &args.reserve_stake,
        &args.fusol_mint,
        &args.maintenance_vault,
    ] {
        require!(*addr != Pubkey::default(), ControllerError::InvalidConfigAddress);
    }

    // The stake-pool program derives its withdraw authority as [pool, b"withdraw"] under its
    // own (the FORK's) program id; derive and record it once so every later instruction pins
    // it with a plain `address =` compare.
    let (pool_withdraw_authority, _) = Pubkey::find_program_address(
        &[args.stake_pool.as_ref(), b"withdraw"],
        &FUSION_STAKE_POOL_PROGRAM_ID,
    );

    let config = &mut ctx.accounts.config;
    config.version = 1;
    config.sealed = false;
    config.stake_pool_program = FUSION_STAKE_POOL_PROGRAM_ID;
    config.stake_pool = args.stake_pool;
    config.validator_list = args.validator_list;
    config.reserve_stake = args.reserve_stake;
    config.fusol_mint = args.fusol_mint;
    config.pool_withdraw_authority = pool_withdraw_authority;
    config.maintenance_vault = args.maintenance_vault;
    config.fusd_core_program = FUSD_CORE_PROGRAM_ID;
    // The fuSOL market's collateral mint IS the pool mint (one asset); recorded under its own
    // name so the Preference layer reads an explicit field, never a derivation.
    config.fusol_collateral_mint = args.fusol_mint;
    config.bump = ctx.bumps.config;
    config.pool_authority_bump = ctx.bumps.pool_authority;
    config.deposit_authority_bump = ctx.bumps.deposit_authority;
    config.maintenance_authority_bump = ctx.bumps.maintenance_authority;
    config._reserved = [0u8; 64];

    // Zero-initialize the crank state machine (all-zero == epoch 0, PHASE_IDLE — the valid
    // pre-genesis state; `start_epoch` takes it from here once the pool exists).
    ctx.accounts.epoch_state.load_init()?;

    emit_cpi!(crate::events::ControllerInitialized {
        stake_pool: args.stake_pool,
        validator_list: args.validator_list,
        reserve_stake: args.reserve_stake,
        fusol_mint: args.fusol_mint,
        pool_withdraw_authority,
        maintenance_vault: args.maintenance_vault,
        fusd_core_program: FUSD_CORE_PROGRAM_ID,
    });
    Ok(())
}
