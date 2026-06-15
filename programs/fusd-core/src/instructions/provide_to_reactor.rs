use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use crate::constants::{ESS_SEED, FUSD_MINT_SEED, REACTOR_DEPOSIT_SEED, REACTOR_POOL_SEED};
use crate::errors::FusdError;
use crate::reactor;
use crate::state::{EpochToScaleToSum, ReactorDeposit, ReactorPool};

/// Deposit fUSD into a market's Reactor Pool. Realizes any accrued collateral gain first.
#[event_cpi]
#[derive(Accounts)]
pub struct ProvideToReactor<'info> {
    pub owner: Signer<'info>,
    pub collateral_mint: Account<'info, Mint>,

    #[account(seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Account<'info, Mint>,

    #[account(mut, seeds = [REACTOR_POOL_SEED, collateral_mint.key().as_ref()], bump = reactor_pool.bump)]
    pub reactor_pool: Account<'info, ReactorPool>,

    // Read-only here (only `.load()` — the grid is written exclusively by `liquidate`), so NOT `mut`,
    // matching the sibling read-only consumer `claim_reactor_gains`; avoids an unneeded write lock.
    #[account(seeds = [ESS_SEED, collateral_mint.key().as_ref()], bump,
        address = reactor_pool.epoch_to_scale_to_sum)]
    pub epoch_to_scale_to_sum: AccountLoader<'info, EpochToScaleToSum>,

    #[account(
        mut,
        seeds = [REACTOR_DEPOSIT_SEED, collateral_mint.key().as_ref(), owner.key().as_ref()],
        bump = reactor_deposit.bump,
        has_one = owner,
    )]
    pub reactor_deposit: Account<'info, ReactorDeposit>,

    #[account(mut, token::mint = fusd_mint, token::authority = owner)]
    pub owner_fusd_ata: Account<'info, TokenAccount>,

    #[account(mut, address = reactor_pool.fusd_vault)]
    pub reactor_fusd_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<ProvideToReactor>, amount: u64) -> Result<()> {
    require!(amount > 0, FusdError::ZeroAmount);
    let ps = reactor::pool_state(&ctx.accounts.reactor_pool);

    // Realize accrued gain + compute the depositor's current compounded deposit, then
    // re-snapshot. (Provide doesn't change P/scale/epoch, so the snapshot is at the current point.)
    let compounded = {
        let grid = ctx.accounts.epoch_to_scale_to_sum.load()?;
        let c = reactor::realize(&ps, &mut ctx.accounts.reactor_deposit, &grid.data)?;
        reactor::set_snapshot(&mut ctx.accounts.reactor_deposit, &ps, &grid.data);
        c
    };
    let new_deposited = u64::try_from(
        compounded.checked_add(amount as u128).ok_or(FusdError::MathOverflow)?,
    )
    .map_err(|_| FusdError::MathOverflow)?;

    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.owner_fusd_ata.to_account_info(),
                to: ctx.accounts.reactor_fusd_vault.to_account_info(),
                authority: ctx.accounts.owner.to_account_info(),
            },
        ),
        amount,
    )?;

    ctx.accounts.reactor_pool.total_deposits = ctx
        .accounts
        .reactor_pool
        .total_deposits
        .checked_add(amount as u128)
        .ok_or(FusdError::MathOverflow)?;
    ctx.accounts.reactor_deposit.deposited_fusd = new_deposited;

    emit_cpi!(crate::events::ReactorDepositUpdated {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        owner: ctx.accounts.owner.key(),
        op: crate::events::REACTOR_OP_PROVIDE,
        fusd_amount: amount,
        collateral_paid: 0,
        deposited_fusd: new_deposited,
    });
    Ok(())
}
