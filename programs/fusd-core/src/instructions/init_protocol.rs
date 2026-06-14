use anchor_lang::prelude::*;
use anchor_lang::ProgramData;
use anchor_spl::token::{Mint, Token};

use crate::constants::{CONFIG_SEED, FUSD_MINT_SEED, MINT_AUTHORITY_SEED};
use crate::errors::FusdError;
use crate::state::ProtocolConfig;

/// fUSD has 6 decimals (USDC/USDH convention).
pub const FUSD_DECIMALS: u8 = 6;

/// Inputs to `init_protocol`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct InitProtocolArgs {
    /// Migratable inbound governance authority (e.g. the MetaDAO Squads vault PDA).
    pub gov_authority: Pubkey,
    /// Guardian — de-risk only, independent of futarchy/Squads.
    pub guardian: Pubkey,
}

#[event_cpi]
#[derive(Accounts)]
pub struct InitProtocol<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    /// This program's `ProgramData` account (BPF upgradeable loader). Derived as the canonical
    /// programdata PDA of THIS program, so it cannot be spoofed; `Account<ProgramData>` checks the
    /// owner (= the upgradeable loader) and deserializes it. The constraint gates the one-time
    /// bootstrap to the program's **upgrade authority** — only whoever can upgrade the program may
    /// initialize it. This closes the initialize-front-run governance-capture vector: the global
    /// `[b"config"]` PDA is deterministic, so without this gate anyone could land `init_protocol`
    /// first and set `gov_authority`/`guardian` to themselves.
    #[account(
        seeds = [crate::ID.as_ref()],
        bump,
        seeds::program = anchor_lang::solana_program::bpf_loader_upgradeable::ID,
        constraint = program_data.upgrade_authority_address == Some(payer.key())
            @ FusdError::Unauthorized,
    )]
    pub program_data: Account<'info, ProgramData>,

    #[account(
        init,
        payer = payer,
        space = ProtocolConfig::SPACE,
        seeds = [CONFIG_SEED],
        bump,
    )]
    pub config: Account<'info, ProtocolConfig>,

    /// CHECK: PDA that holds the fUSD mint authority. Never a keypair — minting is only
    /// possible via `invoke_signed` from inside borrow under the rules.
    #[account(seeds = [MINT_AUTHORITY_SEED], bump)]
    pub mint_authority: UncheckedAccount<'info>,

    /// The fUSD mint. Created with mint authority = the PDA above and **NO freeze
    /// authority** (omitted ⇒ `None`, irreversible) — the core censorship-resistance
    /// guarantee. Legacy SPL Token program (not Token-2022).
    #[account(
        init,
        payer = payer,
        seeds = [FUSD_MINT_SEED],
        bump,
        mint::decimals = FUSD_DECIMALS,
        mint::authority = mint_authority,
    )]
    pub fusd_mint: Account<'info, Mint>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

pub fn handler(ctx: Context<InitProtocol>, args: InitProtocolArgs) -> Result<()> {
    let config = &mut ctx.accounts.config;
    config.gov_authority = args.gov_authority;
    config.guardian = args.guardian;
    config.deployer = ctx.accounts.payer.key();
    config.fusd_mint = ctx.accounts.fusd_mint.key();
    config.bump = ctx.bumps.config;
    config.pending_gov_authority = Pubkey::default(); // no handoff in flight at genesis
    // Seed the bounded-updatable oracle program IDs from the compile-time genesis defaults.
    // `set_oracle_program_ids` (gov-gated) updates them for the Pyth core migration (~2026-07-31).
    config.pyth_receiver_program_id = crate::constants::PYTH_RECEIVER_PROGRAM_ID;
    // Pre-seed the UPGRADED receiver as the second accepted owner so `update_price` honors price
    // updates from EITHER receiver through the migration's dual-running window — a zero-downtime,
    // zero-gov-action cutover (the on-chain analog of Pyth's dual-fetch guidance).
    config.pyth_receiver_program_id_alt = crate::constants::PYTH_RECEIVER_PROGRAM_ID_UPGRADED;
    config.switchboard_program_id = crate::constants::SWITCHBOARD_ON_DEMAND_PROGRAM_ID;
    config._reserved = [0u8; 32];

    emit_cpi!(crate::events::ProtocolInitialized {
        gov_authority: args.gov_authority,
        guardian: args.guardian,
        fusd_mint: ctx.accounts.fusd_mint.key(),
    });
    Ok(())
}
