//! Permissionless: create a `ValidatorRecord` for a vote account. Registration is NOT
//! admission — the record starts `Registered` (max target 0, no list slot); admission to
//! Candidate is a later plan outcome requiring objective eligibility, minimum directed support
//! (`MIN_ACTIVATION_TARGET_LAMPORTS`), and free list capacity.
//!
//! Objective admission gate here (spec §6.1 "account type" row): the account must be OWNED by
//! the canonical Vote program AND parse under a supported `VoteState` version
//! (`fusion-stake-view` fails closed on unknown versions). Anything else rejects — never trust
//! by parse alone, never by address alone.

use anchor_lang::prelude::*;

use crate::constants::{
    CONTROLLER_SEED, VALIDATOR_LIST_INDEX_UNSET, VALIDATOR_RECORD_SEED, VOTE_PROGRAM_ID,
};
use crate::errors::ControllerError;
use crate::state::{ControllerConfig, ValidatorRecord};
use fusion_stake_math::lifecycle::ValidatorStatus;

#[event_cpi]
#[derive(Accounts)]
pub struct RegisterValidator<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(seeds = [CONTROLLER_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ControllerConfig>>,

    /// CHECK: owner-checked against the canonical Vote program below, then byte-parsed by
    /// `fusion_stake_view::vote_state` (fails closed on unsupported versions).
    pub vote_account: UncheckedAccount<'info>,

    #[account(
        init,
        payer = payer,
        space = ValidatorRecord::SPACE,
        seeds = [VALIDATOR_RECORD_SEED, vote_account.key().as_ref()],
        bump,
    )]
    pub validator_record: Box<Account<'info, ValidatorRecord>>,

    pub system_program: Program<'info, System>,
}

pub fn handler(ctx: Context<RegisterValidator>) -> Result<()> {
    // Runtime owner check FIRST (the parser sees raw bytes only, by design).
    require!(
        *ctx.accounts.vote_account.owner == VOTE_PROGRAM_ID,
        ControllerError::InvalidVoteAccount
    );
    // Must parse under a supported VoteState version. The prior completed epoch is the
    // eligibility question the parser answers; registration only requires parse success —
    // liveness/commission gates run at every plan pass, not here.
    let prior_epoch = Clock::get()?.epoch.saturating_sub(1);
    let data = ctx.accounts.vote_account.try_borrow_data()?;
    fusion_stake_view::vote_state::parse(&data, prior_epoch)
        .map_err(|_| error!(ControllerError::InvalidVoteAccount))?;
    drop(data);

    let record = &mut ctx.accounts.validator_record;
    record.version = 1;
    record.vote_account = ctx.accounts.vote_account.key();
    record.validator_list_index = VALIDATOR_LIST_INDEX_UNSET;
    record.status = ValidatorStatus::Registered.as_u8();
    record.bump = ctx.bumps.validator_record;
    // Every counter/balance/target field starts 0 (Anchor zero-initializes new accounts);
    // left explicit-free on purpose — 0 is the correct genesis for all of them.

    emit_cpi!(crate::events::ValidatorRegistered {
        vote_account: ctx.accounts.vote_account.key(),
        payer: ctx.accounts.payer.key(),
    });
    Ok(())
}
