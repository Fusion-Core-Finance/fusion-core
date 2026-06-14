use anchor_lang::prelude::*;
use anchor_spl::token::Mint;

use crate::constants::POSITION_SEED;
use crate::errors::FusdError;
use crate::state::Position;

/// Close an empty position and reclaim its rent **plus** the SOL liquidation bond. Requires the
/// position hold no debt and no collateral. fusion-docs.md.
#[event_cpi]
#[derive(Accounts)]
pub struct ClosePosition<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(
        mut,
        seeds = [POSITION_SEED, collateral_mint.key().as_ref(), owner.key().as_ref()],
        bump = position.bump,
        has_one = owner,
        has_one = collateral_mint,
        close = owner,
    )]
    pub position: Account<'info, Position>,
}

pub fn handler(ctx: Context<ClosePosition>) -> Result<()> {
    // A position with ink == 0 has stake == 0, so there is no unrealized redistribution to lose.
    require!(ctx.accounts.position.recorded_debt == 0, FusdError::PositionNotEmpty);
    require!(ctx.accounts.position.ink == 0, FusdError::PositionNotEmpty);
    // Any unclaimed liquidation surplus must be claimed first (`claim_coll_surplus`), else closing the
    // account would strand that collateral in the vault with no owner record.
    require!(ctx.accounts.position.coll_surplus == 0, FusdError::PositionNotEmpty);
    // `close = owner` refunds all of the account's lamports — rent + any remaining reserve bond
    // (the bond is gone if the position was liquidated; still present if voluntarily wound down).
    emit_cpi!(crate::events::PositionUpdated {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        owner: ctx.accounts.position.owner,
        op: crate::events::POSITION_OP_CLOSE,
        amount: 0,
        ink: ctx.accounts.position.ink,
        recorded_debt: ctx.accounts.position.recorded_debt,
        user_rate_bps: ctx.accounts.position.user_rate_bps,
        bucket: ctx.accounts.position.bucket,
    });
    Ok(())
}
