//! `urgent_redeem` — the shutdown wind-down path (fusion-docs §4.x, §6.1).
//!
//! Valid ONLY when the market is `shutdown`. Unlike the ordered `redeem`, it is **unordered** (any
//! position is a valid target — the wind-down drains the whole market, so rate-bucket priority is
//! moot), **0-fee**, and pays **face value at the last price** with **no staleness gate** (that is
//! the point: a market shut down on oracle failure still redeems at its last good price). It never
//! creates bad debt — each redemption is capped at the position's `min(debt, collateral value)` —
//! and never over-pays (0% bonus), so no over-payment amount check or collateral-surplus path is needed.
//!
//! Mirrors `redeem`'s per-position mechanics (realize pending redistribution first, cap, reduce
//! `art`/`ink`/aggregates, recompute stake, leave the rate bucket on full redemption), minus the
//! bucket-ordering check, the CR sort, and the fee skim.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Mint, Token, TokenAccount, Transfer};
use fusd_math::{mul_div_floor, ray_mul, RAY};

use crate::accrual;
use crate::constants::{
    FUSD_MINT_SEED, MARKET_SEED, MAX_REDEMPTION_CANDIDATES, REDEMPTION_BITMAP_SEED,
};
use crate::errors::FusdError;
use crate::state::{Market, Position, RedemptionBitmap};

#[event_cpi]
#[derive(Accounts)]
pub struct UrgentRedeem<'info> {
    #[account(mut)]
    pub redeemer: Signer<'info>,

    pub collateral_mint: Box<Account<'info, Mint>>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,

    #[account(mut, seeds = [REDEMPTION_BITMAP_SEED, collateral_mint.key().as_ref()], bump)]
    pub redemption_bitmap: AccountLoader<'info, RedemptionBitmap>,

    #[account(mut, seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Box<Account<'info, Mint>>,

    #[account(mut, address = market.collateral_vault)]
    pub market_coll_vault: Box<Account<'info, TokenAccount>>,

    #[account(mut, token::mint = fusd_mint, token::authority = redeemer)]
    pub redeemer_fusd_ata: Box<Account<'info, TokenAccount>>,

    #[account(mut, token::mint = collateral_mint, token::authority = redeemer)]
    pub redeemer_collateral_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    // remaining_accounts: candidate Position accounts (writable), ANY bucket.
}

pub fn handler<'info>(
    ctx: Context<'_, '_, 'info, 'info, UrgentRedeem<'info>>,
    amount: u64,
) -> Result<()> {
    require!(amount > 0, FusdError::ZeroAmount);
    require!(ctx.accounts.market.shutdown, FusdError::MarketNotShutdown);
    // Bound the candidate count (per-tx account/CU budget) — same cap as ordered redeem.
    require!(
        ctx.remaining_accounts.len() <= MAX_REDEMPTION_CANDIDATES,
        FusdError::TooManyRedemptionCandidates
    );
    let now = Clock::get()?.unix_timestamp;
    // Shut down ⇒ `accrue` is a no-op (interest frozen at the wind-down); the clock stays at the
    // shutdown moment, capping per-position interest in `realize`.
    accrual::accrue(&mut ctx.accounts.market, now)?;
    let spot = ctx.accounts.market.spot;
    let debt_spot = ctx.accounts.market.debt_spot;
    // Face value at the LAST price — no staleness gate (a wind-down must proceed even on an oracle
    // outage). Pay at the MID (`(spot + debt_spot)/2`), same as ordered `redeem`, so the wind-down
    // never over-pays at the conservative LOW `spot`. A price must exist (a never-priced market has no
    // debt to wind down anyway).
    require!(spot > 0 && debt_spot > 0, FusdError::OracleUnavailable);
    let mid = spot.checked_add(debt_spot).ok_or(FusdError::MathOverflow)? / 2;

    let coll_mint = ctx.accounts.collateral_mint.key();
    let mut remaining = amount as u128;
    let mut redeemer_coll_total: u128 = 0;
    let mut seen: Vec<Pubkey> = Vec::with_capacity(ctx.remaining_accounts.len());

    for info in ctx.remaining_accounts.iter() {
        if remaining == 0 {
            break;
        }
        // CLOSED-candidate skip: a candidate repaid + closed between tx build
        // and execution must not revert the whole wind-down batch (see redeem.rs for the full
        // rationale — it matters at least as much here, mid-shutdown). Present-but-wrong
        // accounts still hard-revert below.
        if info.owner != &crate::ID || info.data_is_empty() {
            continue;
        }
        let key = info.key();
        require!(!seen.contains(&key), FusdError::DuplicateRedemptionTarget);
        seen.push(key);

        let mut pos = Account::<Position>::try_from(info)?;
        require_keys_eq!(pos.collateral_mint, coll_mint, FusdError::Unauthorized);
        // Bring the candidate current (a position touch): realize interest (frozen at shutdown) + any
        // pending tier-2 redistribution, so debt/collateral and the bucket-leave use TRUE values and
        // the snapshot is rolled. `old_weighted` captured before, applied after — covers the realize
        // AND any redemption reduction in ONE weighted-sum delta on every path.
        let old_weighted = accrual::weighted(&pos)?;
        // Capture the bucket-membership state (has-debt) BEFORE realize: `reconcile` keys off the
        // 0↔+ debt transition over the whole touch. A debt-free-in-storage position (never borrowed,
        // or fully repaid) is NOT a bitmap member; if `realize` folds in parked tier-2 redistribution
        // debt it goes 0→+ here, so it must JOIN — never blindly `leave` (which would underflow the
        // member count for bucket 0 / over-decrement a stale bucket). Same pattern as `deposit`.
        let art_before = pos.recorded_debt;
        accrual::realize(&ctx.accounts.market, &mut pos, now)?;

        let debt = pos.recorded_debt;
        let coll_value = ray_mul(pos.ink as u128, mid).ok_or(FusdError::MathOverflow)?;
        // Cap at the position's debt AND collateral value ⇒ never creates bad debt, never over-draws.
        let redeem_amt = remaining.min(debt).min(coll_value);
        if redeem_amt == 0 {
            // Nothing to redeem here, but the realize may have grown debt/ink; reconcile fully —
            // including bucket membership (a realize that took art 0→+ on a debt-free-in-storage
            // position must JOIN, not stay un-counted).
            accrual::reweight(&mut ctx.accounts.market, &pos, old_weighted)?;
            crate::redist::set_stake(&mut ctx.accounts.market, &mut pos)?;
            {
                let mut bm = ctx.accounts.redemption_bitmap.load_mut()?;
                crate::bucket::reconcile(&mut bm, &ctx.accounts.market, &mut pos, art_before)?;
            }
            pos.exit(&crate::ID)?;
            continue;
        }
        let coll_total = mul_div_floor(redeem_amt, RAY, mid)
            .ok_or(FusdError::MathOverflow)?
            .min(pos.ink as u128);

        // 0-fee: the redeemer receives the full `coll_total`; nothing is skimmed to surplus. Recorded
        // debt is native, so it reduces by exactly `redeem_amt` (`redeem_amt <= debt` by the cap).
        // The burn/agg accounting is the shared `supply_transition::redeem_step` body certora.rs
        // proves (one step per candidate, same as ordered `redeem`).
        let d = crate::supply_transition::redeem_step(
            ctx.accounts.market.agg_recorded_debt,
            redeem_amt,
        )
        .ok_or(FusdError::MathOverflow)?;
        pos.recorded_debt -= redeem_amt;
        pos.ink -= coll_total as u64;
        ctx.accounts.market.agg_recorded_debt = d.new_agg;
        ctx.accounts.market.total_collateral = ctx
            .accounts
            .market
            .total_collateral
            .checked_sub(coll_total)
            .ok_or(FusdError::MathOverflow)?;

        accrual::reweight(&mut ctx.accounts.market, &pos, old_weighted)?;
        crate::redist::set_stake(&mut ctx.accounts.market, &mut pos)?;
        // Reconcile bucket membership over the WHOLE touch (`art_before` → current debt): handles a
        // realize that joined (0→+) AND a full redemption that left (+→0) in one delta, so a position
        // that was never a member is never spuriously `leave`d. (Ordered `redeem` can use a bare
        // `leave` because it pre-validates `recorded_debt > 0` membership; urgent_redeem accepts ANY
        // position, so it must reconcile off the captured `art_before`.)
        {
            let mut bm = ctx.accounts.redemption_bitmap.load_mut()?;
            crate::bucket::reconcile(&mut bm, &ctx.accounts.market, &mut pos, art_before)?;
        }
        pos.exit(&crate::ID)?;

        redeemer_coll_total = redeemer_coll_total
            .checked_add(coll_total)
            .ok_or(FusdError::MathOverflow)?;
        remaining -= d.burned;
    }

    let redeemed_fusd = (amount as u128) - remaining;
    require!(redeemed_fusd > 0, FusdError::NothingToRedeem);

    token::burn(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Burn {
                mint: ctx.accounts.fusd_mint.to_account_info(),
                from: ctx.accounts.redeemer_fusd_ata.to_account_info(),
                authority: ctx.accounts.redeemer.to_account_info(),
            },
        ),
        redeemed_fusd as u64,
    )?;

    if redeemer_coll_total > 0 {
        let m_bump = ctx.accounts.market.bump;
        let signer: &[&[&[u8]]] = &[&[MARKET_SEED, coll_mint.as_ref(), &[m_bump]]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.market_coll_vault.to_account_info(),
                    to: ctx.accounts.redeemer_collateral_ata.to_account_info(),
                    authority: ctx.accounts.market.to_account_info(),
                },
                signer,
            ),
            redeemer_coll_total as u64,
        )?;
    }

    emit_cpi!(crate::events::UrgentRedemptionEvent {
        collateral_mint: coll_mint,
        redeemer: ctx.accounts.redeemer.key(),
        fusd_burned: redeemed_fusd as u64,
        collateral_paid: redeemer_coll_total as u64,
    });
    crate::reconcile::assert_collateral_vault_sufficiency(
        &mut ctx.accounts.market_coll_vault,
        &ctx.accounts.market,
    )?;
    Ok(())
}
