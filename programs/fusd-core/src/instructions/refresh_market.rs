use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, MintTo, Token, TokenAccount};

use crate::accrual;
use crate::constants::{
    BUFFER_SEED, FUSD_MINT_SEED, MARKET_SEED, MINT_AUTHORITY_BUMP, MINT_AUTHORITY_SEED,
};
use crate::errors::FusdError;
use crate::state::{InsuranceBuffer, Market};

/// Permissionless: advance the market's aggregate interest to now AND mint the accumulated interest
/// into the per-market insurance buffer (the lazy mint seam; fusion-docs.md). Anyone may call
/// it; keepers run it to keep the shared `Market` interest current and to capitalize the buffer from
/// the realized-interest fee stream. The mint is kept here (off the hot path) so borrow/repay/etc carry
/// no buffer vault — they only `accrual::accrue` into `Market.unminted_interest`, which this drains.
#[event_cpi]
#[derive(Accounts)]
pub struct RefreshMarket<'info> {
    pub collateral_mint: Box<Account<'info, Mint>>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,

    #[account(mut, seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Box<Account<'info, Mint>>,

    /// CHECK: the fUSD mint-authority PDA; only signs minting from inside the protocol rules.
    #[account(seeds = [MINT_AUTHORITY_SEED], bump = MINT_AUTHORITY_BUMP)]
    pub mint_authority: UncheckedAccount<'info>,

    #[account(mut, seeds = [BUFFER_SEED, collateral_mint.key().as_ref()], bump = insurance_buffer.bump)]
    pub insurance_buffer: Box<Account<'info, InsuranceBuffer>>,

    #[account(mut, address = insurance_buffer.fusd_vault)]
    pub buffer_fusd_vault: Box<Account<'info, TokenAccount>>,

    /// OPTIONAL: an fUSD token account to receive the keeper reward — a cut (`Market.keeper_reward_bps`)
    /// of the interest this crank mints; the cranker directs it to themselves. Constrained
    /// to the fUSD mint only (the caller picks any fUSD account, typically their own ATA). When omitted,
    /// or when `keeper_reward_bps == 0`, the WHOLE interest mints to the buffer (no reward). Permissionless:
    /// whoever does the crank work earns the cut.
    #[account(mut, token::mint = fusd_mint)]
    pub cranker_fusd_ata: Option<Box<Account<'info, TokenAccount>>>,

    /// OPTIONAL: the Global Backstop Reserve + its vault. When BOTH are supplied (and the reserve's
    /// `cut_bps > 0` and it is below its cap), `backstop_cut_bps` of the post-keeper interest routes
    /// here as second-loss capital; the rest stays in the LOCAL buffer (the majority). When omitted,
    /// the whole post-keeper interest funds the local buffer (byte-identical to pre-backstop behavior).
    /// The hot user paths never touch these — only this periodic crank.
    #[account(mut, seeds = [crate::constants::BACKSTOP_SEED], bump = backstop.bump)]
    pub backstop: Option<Box<Account<'info, crate::state::GlobalBackstopReserve>>>,

    #[account(mut)]
    pub backstop_fusd_vault: Option<Box<Account<'info, TokenAccount>>>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<RefreshMarket>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    accrual::accrue(&mut ctx.accounts.market, now)?;

    // Mint the accumulated interest into the buffer's reserve vault and zero the counter — the
    // realized-interest fee stream that funds the insurance buffer (the funding loop). One-for-one
    // with the `agg_recorded_debt` growth that already booked it, so the supply invariant holds:
    // `circulating == agg_recorded_debt − unminted_interest + bad_debt`.
    let pending = ctx.accounts.market.unminted_interest;
    if pending == 0 {
        return Ok(());
    }
    // Keeper reward: pay the cranker a `keeper_reward_bps` cut of the interest minted this
    // call, ONLY when the reward is enabled AND the caller supplied an fUSD account (collapsed here
    // to an effective 0 bps when the account is absent). The cut floors (buffer-favoring), so the
    // buffer always gets `amount - keeper_cut`. This is a SPLIT of interest the protocol already
    // mints (booked in `agg_recorded_debt`), not a fresh mint — the supply invariant and credible
    // neutrality hold. Spam-proof: a second immediate crank has `pending == 0`.
    let keeper_bps_eff: u16 = if ctx.accounts.cranker_fusd_ata.is_some() {
        ctx.accounts.market.keeper_reward_bps
    } else {
        0
    };

    // Backstop cut (global second-loss capital): when the reserve +
    // its vault are supplied AND the reserve's `cut_bps > 0` AND it is below its cap, route a MINORITY
    // of the post-keeper interest there; the LOCAL buffer keeps the rest (majority + the floor
    // remainder). Capped so the reserve never exceeds `reserve_cap` (excess reverts to local). When the
    // accounts are omitted (or cut disabled / at cap), the whole post-keeper funds the local buffer —
    // byte-identical to pre-backstop behavior. Parallelism: the reserve PROGRAM account is read for
    // params (+ a cumulative bump); the cut mints into the SHARED reserve vault, so funded refresh
    // cranks serialize among THEMSELVES on that vault — never the hot user paths.
    // The vault-identity check stays INSIDE the match: it fires whenever the account pair is
    // supplied, even when `cut_bps == 0`.
    let (backstop_cut_bps_eff, backstop_headroom): (u16, u128) =
        match (ctx.accounts.backstop.as_ref(), ctx.accounts.backstop_fusd_vault.as_ref()) {
            (Some(bs), Some(vault)) => {
                require_keys_eq!(vault.key(), bs.fusd_vault, FusdError::InvalidRecipient);
                (bs.cut_bps, (bs.reserve_cap as u128).saturating_sub(vault.amount as u128))
            }
            _ => (0, 0),
        };

    // The supply-identity transition (the shared body certora.rs proves): consume `amount =
    // min(pending, u64::MAX)` (capping — instead of `try_from`-reverting — lets a >u64 backlog drain
    // over multiple cranks; the surplus stays booked in `agg_recorded_debt`), split it keeper →
    // C16 bad-debt paydown → backstop → buffer. C16: when the market carries realized un-homed
    // `bad_debt`, a governable fraction of the post-keeper interest RETIRES it instead of minting —
    // loss recovery before buffer/backstop growth, capped at the outstanding `bad_debt`.
    // Supply-preserving: the diverted slice is simply NOT minted (`circulating` rises by
    // `amount − paydown`) while `bad_debt` drops by `paydown`.
    let d = crate::supply_transition::refresh(
        pending,
        ctx.accounts.market.bad_debt,
        keeper_bps_eff,
        ctx.accounts.market.bad_debt_paydown_bps,
        backstop_cut_bps_eff,
        backstop_headroom,
    )
    .ok_or(FusdError::MathOverflow)?;

    let bump = MINT_AUTHORITY_BUMP;
    let signer: &[&[&[u8]]] = &[&[MINT_AUTHORITY_SEED, &[bump]]];
    if d.buffer_amount > 0 {
        token::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                MintTo {
                    mint: ctx.accounts.fusd_mint.to_account_info(),
                    to: ctx.accounts.buffer_fusd_vault.to_account_info(),
                    authority: ctx.accounts.mint_authority.to_account_info(),
                },
                signer,
            ),
            d.buffer_amount as u64,
        )?;
    }
    if d.keeper_cut > 0 {
        // `cranker_fusd_ata` is Some here (keeper_cut > 0 requires it).
        let cranker_ata = ctx.accounts.cranker_fusd_ata.as_ref().unwrap();
        token::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                MintTo {
                    mint: ctx.accounts.fusd_mint.to_account_info(),
                    to: cranker_ata.to_account_info(),
                    authority: ctx.accounts.mint_authority.to_account_info(),
                },
                signer,
            ),
            d.keeper_cut as u64,
        )?;
    }
    if d.backstop_cut > 0 {
        // Mint the cut into the global reserve vault (Some here — backstop_cut > 0 requires both accounts).
        let vault = ctx.accounts.backstop_fusd_vault.as_ref().unwrap();
        token::mint_to(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                MintTo {
                    mint: ctx.accounts.fusd_mint.to_account_info(),
                    to: vault.to_account_info(),
                    authority: ctx.accounts.mint_authority.to_account_info(),
                },
                signer,
            ),
            d.backstop_cut as u64,
        )?;
        // Per-market cumulative contribution (LOCAL write — feeds the contribution-weighted draw cap).
        ctx.accounts.market.global_contributed = ctx
            .accounts
            .market
            .global_contributed
            .checked_add(d.backstop_cut)
            .ok_or(FusdError::MathOverflow)?;
        // Reserve cumulative (the reserve-solvency invariant). `as_mut` is Some here.
        let bs = ctx.accounts.backstop.as_mut().unwrap();
        bs.total_contributed =
            bs.total_contributed.checked_add(d.backstop_cut).ok_or(FusdError::MathOverflow)?;
    }

    // C16: retire the diverted slice of `bad_debt`. The `paydown` interest was consumed from
    // `unminted_interest` (below) and NOT minted, so dropping `bad_debt` by the same amount keeps
    // `circulating == agg_recorded_debt − unminted_interest + bad_debt` exact.
    // `d.new_bad == bad_debt − paydown` (unchanged when `paydown == 0`).
    ctx.accounts.market.bad_debt = d.new_bad;

    // Subtract what was minted OR diverted to paydown (drains a hypothetical >u64 backlog over multiple
    // cranks; the common case consumes the whole `pending` and leaves 0). Both the minted `amount −
    // paydown` and the diverted `paydown` are interest realized out of `unminted_interest` this crank.
    ctx.accounts.market.unminted_interest = d.new_unminted;
    // `total_funded` tracks the fUSD that entered the BUFFER (organic interest minus the keeper cut, the
    // backstop cut, and the C16 paydown diversion + external top-ups) for proof-of-reserves.
    ctx.accounts.insurance_buffer.total_funded = ctx
        .accounts
        .insurance_buffer
        .total_funded
        .checked_add(d.buffer_amount)
        .ok_or(FusdError::MathOverflow)?;

    emit_cpi!(crate::events::InterestMinted {
        collateral_mint: ctx.accounts.collateral_mint.key(),
        amount: d.amount as u64,
        to_buffer: d.buffer_amount as u64,
        to_backstop: d.backstop_cut as u64,
        to_bad_debt_paydown: d.paydown as u64,
        keeper_cut: d.keeper_cut as u64,
        unminted_remaining: ctx.accounts.market.unminted_interest,
    });
    Ok(())
}
