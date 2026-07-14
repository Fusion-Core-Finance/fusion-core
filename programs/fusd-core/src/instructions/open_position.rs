use anchor_lang::prelude::*;
use anchor_spl::token::Mint;

use crate::constants::{MARKET_SEED, MAX_USER_RATE_BPS, MIN_USER_RATE_BPS, POSITION_SEED};
use crate::errors::FusdError;
use crate::state::{Market, Position};

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct OpenPositionArgs {
    /// Borrower-chosen **annual interest rate** (bps) — the real accrual rate AND the redemption
    /// rate-bucket key. Validated to `[MIN_USER_RATE_BPS, MAX_USER_RATE_BPS]`.
    pub user_rate_bps: u16,
}

#[event_cpi]
#[derive(Accounts)]
pub struct OpenPosition<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,

    #[account(
        init,
        payer = owner,
        space = Position::SPACE,
        seeds = [POSITION_SEED, collateral_mint.key().as_ref(), owner.key().as_ref()],
        bump,
    )]
    pub position: Account<'info, Position>,

    pub system_program: Program<'info, System>,
}

pub fn handler(ctx: Context<OpenPosition>, args: OpenPositionArgs) -> Result<()> {
    require!(
        args.user_rate_bps >= MIN_USER_RATE_BPS && args.user_rate_bps <= MAX_USER_RATE_BPS,
        FusdError::InterestRateOutOfBounds
    );
    let reserve = ctx.accounts.market.reserve_lamports;
    let now = Clock::get()?.unix_timestamp;
    {
        let p = &mut ctx.accounts.position;
        p.owner = ctx.accounts.owner.key();
        p.collateral_mint = ctx.accounts.collateral_mint.key();
        p.ink = 0;
        p.recorded_debt = 0;
        p.user_rate_bps = args.user_rate_bps;
        p.bump = ctx.bumps.position;
        // Interest clock starts now; with no debt the first realize accrues 0 regardless.
        p.last_debt_update = now;
        // Fresh position: no stake yet, and snapshot the market's redistribution accumulators NOW so
        // it only earns redistributions that happen after it opens.
        p.stake = 0;
        p.redist_l_coll_snapshot = ctx.accounts.market.l_coll;
        p.redist_l_art_snapshot = ctx.accounts.market.l_art;
        // Bond fixed at the market's CURRENT value (a later governance change can't alter it).
        p.reserve_lamports = reserve;
        // No debt yet, so not in any rate bucket (bucket index is valid only when art > 0).
        p.bucket = 0;
        p.coll_surplus = 0;
        // Rate-change cooldown clock starts at open (BOLD `lastInterestRateAdjTime`), so a rate
        // adjustment within the market's cooldown of opening is also charged the upfront fee.
        p.last_rate_adjust_ts = now;
        // Collateral-change nonce starts at 0 ("never changed"); bumped by `set_ink` on every
        // real `ink` change. NOTE a close+reopen at the same PDA restarts it at 0 — safe for the
        // stake controller because per-epoch double-count is blocked by `last_counted_epoch`,
        // not by nonce uniqueness (fuSOL stake-pool design).
        p.ink_nonce = 0;
        p._reserved = [0u8; 24];
    }

    // Post the SOL liquidation bond on top of rent (paid to the liquidator on liq, refunded on
    // close). Skipped when the market sets no reserve.
    if reserve > 0 {
        anchor_lang::system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.to_account_info(),
                anchor_lang::system_program::Transfer {
                    from: ctx.accounts.owner.to_account_info(),
                    to: ctx.accounts.position.to_account_info(),
                },
            ),
            reserve,
        )?;
    }

    emit_cpi!(crate::events::PositionUpdated {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        owner: ctx.accounts.owner.key(),
        op: crate::events::POSITION_OP_OPEN,
        amount: 0,
        ink: 0,
        recorded_debt: 0,
        user_rate_bps: args.user_rate_bps,
        bucket: 0,
    });
    Ok(())
}
