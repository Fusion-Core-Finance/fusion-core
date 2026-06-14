use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use crate::constants::{ESS_SEED, REACTOR_DEPOSIT_SEED, REACTOR_POOL_SEED};
use crate::errors::FusdError;
use crate::reactor;
use crate::state::{EpochToScaleToSum, ReactorDeposit, ReactorPool};

/// Claim a depositor's realized (seized-collateral) gains. Realizes the latest gain first,
/// then pays out the full pending balance.
#[event_cpi]
#[derive(Accounts)]
pub struct ClaimReactorGains<'info> {
    pub owner: Signer<'info>,
    pub collateral_mint: Account<'info, Mint>,

    #[account(seeds = [REACTOR_POOL_SEED, collateral_mint.key().as_ref()], bump = reactor_pool.bump)]
    pub reactor_pool: Account<'info, ReactorPool>,

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

    #[account(mut, address = reactor_pool.coll_vault)]
    pub reactor_coll_vault: Account<'info, TokenAccount>,

    #[account(mut, token::mint = collateral_mint, token::authority = owner)]
    pub owner_collateral_ata: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<ClaimReactorGains>) -> Result<()> {
    let ps = reactor::pool_state(&ctx.accounts.reactor_pool);

    // Fold the latest accrued gain into `pending`, then re-snapshot (deposit unchanged in fUSD
    // terms — only collateral is claimed).
    let compounded = {
        let grid = ctx.accounts.epoch_to_scale_to_sum.load()?;
        let c = reactor::realize(&ps, &mut ctx.accounts.reactor_deposit, &grid.data)?;
        reactor::set_snapshot(&mut ctx.accounts.reactor_deposit, &ps, &grid.data);
        c
    };
    ctx.accounts.reactor_deposit.deposited_fusd =
        u64::try_from(compounded).map_err(|_| FusdError::MathOverflow)?;

    let amount = ctx.accounts.reactor_deposit.pending_collateral_gain;
    if amount > 0 {
        let coll_key = ctx.accounts.collateral_mint.key();
        let bump = ctx.accounts.reactor_pool.bump;
        let signer: &[&[&[u8]]] = &[&[REACTOR_POOL_SEED, coll_key.as_ref(), &[bump]]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.reactor_coll_vault.to_account_info(),
                    to: ctx.accounts.owner_collateral_ata.to_account_info(),
                    authority: ctx.accounts.reactor_pool.to_account_info(),
                },
                signer,
            ),
            amount,
        )?;
        ctx.accounts.reactor_deposit.pending_collateral_gain = 0;
    }

    emit_cpi!(crate::events::ReactorDepositUpdated {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        owner: ctx.accounts.owner.key(),
        op: crate::events::REACTOR_OP_CLAIM,
        fusd_amount: 0,
        collateral_paid: amount,
        deposited_fusd: ctx.accounts.reactor_deposit.deposited_fusd,
    });
    Ok(())
}
