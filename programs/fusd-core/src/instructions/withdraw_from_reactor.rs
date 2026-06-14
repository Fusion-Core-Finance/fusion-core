use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use crate::constants::{ESS_SEED, FUSD_MINT_SEED, REACTOR_DEPOSIT_SEED, REACTOR_POOL_SEED};
use crate::errors::FusdError;
use crate::reactor;
use crate::state::{EpochToScaleToSum, ReactorDeposit, ReactorPool};

/// Withdraw fUSD from a Reactor Pool (capped at the compounded deposit). Realizes gain first.
#[event_cpi]
#[derive(Accounts)]
pub struct WithdrawFromReactor<'info> {
    pub owner: Signer<'info>,
    pub collateral_mint: Account<'info, Mint>,

    #[account(seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Account<'info, Mint>,

    #[account(mut, seeds = [REACTOR_POOL_SEED, collateral_mint.key().as_ref()], bump = reactor_pool.bump)]
    pub reactor_pool: Account<'info, ReactorPool>,

    #[account(mut, seeds = [ESS_SEED, collateral_mint.key().as_ref()], bump,
        address = reactor_pool.epoch_to_scale_to_sum)]
    pub epoch_to_scale_to_sum: AccountLoader<'info, EpochToScaleToSum>,

    #[account(
        mut,
        seeds = [REACTOR_DEPOSIT_SEED, collateral_mint.key().as_ref(), owner.key().as_ref()],
        bump = reactor_deposit.bump,
        has_one = owner,
    )]
    pub reactor_deposit: Account<'info, ReactorDeposit>,

    #[account(mut, address = reactor_pool.fusd_vault)]
    pub reactor_fusd_vault: Account<'info, TokenAccount>,

    #[account(mut, token::mint = fusd_mint, token::authority = owner)]
    pub owner_fusd_ata: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<WithdrawFromReactor>, amount: u64) -> Result<()> {
    require!(amount > 0, FusdError::ZeroAmount);
    let ps = reactor::pool_state(&ctx.accounts.reactor_pool);

    let compounded = {
        let grid = ctx.accounts.epoch_to_scale_to_sum.load()?;
        let c = reactor::realize(&ps, &mut ctx.accounts.reactor_deposit, &grid.data)?;
        reactor::set_snapshot(&mut ctx.accounts.reactor_deposit, &ps, &grid.data);
        c
    };

    let withdraw_amt = (amount as u128).min(compounded) as u64;
    if withdraw_amt > 0 {
        let coll_key = ctx.accounts.collateral_mint.key();
        let bump = ctx.accounts.reactor_pool.bump;
        let signer: &[&[&[u8]]] = &[&[REACTOR_POOL_SEED, coll_key.as_ref(), &[bump]]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.reactor_fusd_vault.to_account_info(),
                    to: ctx.accounts.owner_fusd_ata.to_account_info(),
                    authority: ctx.accounts.reactor_pool.to_account_info(),
                },
                signer,
            ),
            withdraw_amt,
        )?;
    }

    ctx.accounts.reactor_pool.total_deposits = ctx
        .accounts
        .reactor_pool
        .total_deposits
        .checked_sub(withdraw_amt as u128)
        .ok_or(FusdError::MathOverflow)?;
    let remaining = u64::try_from(compounded - withdraw_amt as u128)
        .map_err(|_| FusdError::MathOverflow)?;
    ctx.accounts.reactor_deposit.deposited_fusd = remaining;

    emit_cpi!(crate::events::ReactorDepositUpdated {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        owner: ctx.accounts.owner.key(),
        op: crate::events::REACTOR_OP_WITHDRAW,
        fusd_amount: withdraw_amt,
        collateral_paid: 0,
        deposited_fusd: remaining,
    });
    Ok(())
}
