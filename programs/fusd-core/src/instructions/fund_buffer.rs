use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use crate::constants::{BUFFER_SEED, FUSD_MINT_SEED};
use crate::errors::FusdError;
use crate::state::InsuranceBuffer;

/// Permissionless: deposit `amount` fUSD into a market's insurance buffer — the funding hook (realized
/// fees / treasury / a keeper-run fee-router). Anyone may add: the buffer
/// is protocol-owned first-loss capital, so funding it only strengthens solvency. fUSD moves from the
/// funder's ATA into the buffer's fUSD reserve vault; `total_funded` is tracked for proof-of-reserves.
#[event_cpi]
#[derive(Accounts)]
pub struct FundBuffer<'info> {
    pub funder: Signer<'info>,

    pub collateral_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        seeds = [BUFFER_SEED, collateral_mint.key().as_ref()],
        bump = insurance_buffer.bump,
    )]
    pub insurance_buffer: Box<Account<'info, InsuranceBuffer>>,

    #[account(mut, address = insurance_buffer.fusd_vault)]
    pub buffer_fusd_vault: Box<Account<'info, TokenAccount>>,

    #[account(seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Box<Account<'info, Mint>>,

    #[account(mut, token::mint = fusd_mint, token::authority = funder)]
    pub funder_fusd_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<FundBuffer>, amount: u64) -> Result<()> {
    require!(amount > 0, FusdError::ZeroAmount);

    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.funder_fusd_ata.to_account_info(),
                to: ctx.accounts.buffer_fusd_vault.to_account_info(),
                authority: ctx.accounts.funder.to_account_info(),
            },
        ),
        amount,
    )?;

    let b = &mut ctx.accounts.insurance_buffer;
    b.total_funded = b.total_funded.checked_add(amount as u128).ok_or(FusdError::MathOverflow)?;

    emit_cpi!(crate::events::BufferFunded {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        funder: ctx.accounts.funder.key(),
        amount,
        total_funded: ctx.accounts.insurance_buffer.total_funded,
    });
    Ok(())
}
