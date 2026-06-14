use anchor_lang::prelude::*;
use anchor_spl::token::Mint;

use crate::constants::{REACTOR_DEPOSIT_SEED, REACTOR_POOL_SEED};
use crate::state::{ReactorDeposit, ReactorPool};

/// Open an (empty) Reactor-Pool deposit account for the signer in a market's pool.
#[derive(Accounts)]
pub struct OpenReactorDeposit<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(seeds = [REACTOR_POOL_SEED, collateral_mint.key().as_ref()], bump = reactor_pool.bump)]
    pub reactor_pool: Account<'info, ReactorPool>,

    #[account(
        init,
        payer = owner,
        space = ReactorDeposit::SPACE,
        seeds = [REACTOR_DEPOSIT_SEED, collateral_mint.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub reactor_deposit: Account<'info, ReactorDeposit>,

    pub system_program: Program<'info, System>,
}

pub fn handler(ctx: Context<OpenReactorDeposit>) -> Result<()> {
    let rp = &ctx.accounts.reactor_pool;
    let d = &mut ctx.accounts.reactor_deposit;
    d.owner = ctx.accounts.owner.key();
    d.reactor_pool = rp.key();
    d.deposited_fusd = 0;
    // Snapshot at the pool's current point; with deposited_fusd = 0 the gain math is a no-op
    // until the first `provide_to_reactor` sets a real snapshot.
    d.snapshot_p = rp.p;
    d.snapshot_s = 0;
    d.snapshot_scale = rp.scale;
    d.snapshot_epoch = rp.epoch;
    d.pending_collateral_gain = 0;
    d.bump = ctx.bumps.reactor_deposit;
    d._reserved = [0u8; 32];
    Ok(())
}
