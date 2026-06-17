use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Mint, Token, TokenAccount, Transfer};
use fusd_math::rate_bucket as rb;
use fusd_math::{mul_div_floor, ray_mul, RAY};

use crate::accrual;
use crate::constants::{
    FUSD_MINT_SEED, MARKET_SEED, MAX_PRICE_STALENESS_SLOTS, MAX_REDEMPTION_CANDIDATES,
    REDEMPTION_BITMAP_SEED, ZOMBIE_BUCKET,
};
use crate::errors::FusdError;
use crate::state::{Market, Position, RedemptionBitmap};

/// Redeem fUSD for **face-value** collateral, draining the lowest non-empty rate bucket first
/// (fusion-docs.md). The redeemer passes the candidate positions as
/// `remaining_accounts`; the program verifies each is in the lowest bucket — so it **cannot skip a
/// lower-rate bucket** (the strict, sound guarantee) — and redeems them lowest-collateral-ratio-
/// first **among the submitted candidates**. In-bucket targeting is therefore the disclosed
/// bucket-level fairness compromise, not a strict per-position guarantee: a redeemer chooses
/// which in-bucket members to submit. Burns the redeemer's fUSD and pays out collateral minus the
/// flat redemption fee (retained as market surplus). Profits the redeemer only when fUSD < `(1-fee)·$1`.
#[event_cpi]
#[derive(Accounts)]
pub struct Redeem<'info> {
    #[account(mut)]
    pub redeemer: Signer<'info>,

    pub collateral_mint: Account<'info, Mint>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Account<'info, Market>,

    #[account(mut, seeds = [REDEMPTION_BITMAP_SEED, collateral_mint.key().as_ref()], bump)]
    pub redemption_bitmap: AccountLoader<'info, RedemptionBitmap>,

    #[account(mut, seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Account<'info, Mint>,

    #[account(mut, address = market.collateral_vault)]
    pub market_coll_vault: Account<'info, TokenAccount>,

    #[account(mut, token::mint = fusd_mint, token::authority = redeemer)]
    pub redeemer_fusd_ata: Account<'info, TokenAccount>,

    #[account(mut, token::mint = collateral_mint, token::authority = redeemer)]
    pub redeemer_collateral_ata: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    // remaining_accounts: the candidate Position accounts (writable) in the lowest non-empty bucket.
}

pub fn handler<'info>(ctx: Context<'_, '_, 'info, 'info, Redeem<'info>>, amount: u64) -> Result<()> {
    require!(amount > 0, FusdError::ZeroAmount);
    // Bound the candidate count so the per-tx account/CU budget can't be blown (the Jupiter-Lend
    // >64-account DoS). Each candidate costs a realize + reweight + set_stake + dup scan.
    require!(
        ctx.remaining_accounts.len() <= MAX_REDEMPTION_CANDIDATES,
        FusdError::TooManyRedemptionCandidates
    );
    // A shut-down market winds down through `urgent_redeem` (unordered, 0-fee) instead.
    require!(!ctx.accounts.market.shutdown, FusdError::MarketShutdown);
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;
    accrual::accrue(&mut ctx.accounts.market, now)?;
    let spot = ctx.accounts.market.spot;
    let fee_bps = ctx.accounts.market.redemption_fee_bps as u128;

    // Redemption pays face value against a fresh oracle.
    let slot = clock.slot;
    require!(spot > 0, FusdError::OracleUnavailable);
    require!(
        slot.saturating_sub(ctx.accounts.market.spot_updated_slot) <= MAX_PRICE_STALENESS_SLOTS,
        FusdError::StalePrice
    );

    // Redemption's strict target is the lowest non-empty NORMAL bucket (find-first-set — can't skip a
    // lower one). The zombie pen (collateral-exhausted / sub-min_debt positions) sits OUTSIDE this
    // ordering: a pen member can never wedge or clog the normal buckets, and a redeemer may drain pen
    // members out-of-band (an unredeemable `ink == 0` stub is simply skipped below, so it can't block
    // the floor). Proceed if there is anything to redeem in either set.
    let (lowest_normal, has_zombies) = {
        let bm = ctx.accounts.redemption_bitmap.load()?;
        (rb::first_set(&bm.words), bm.zombie_count > 0)
    };
    require!(lowest_normal.is_some() || has_zombies, FusdError::NothingToRedeem);

    let coll_mint = ctx.accounts.collateral_mint.key();

    // Validate candidates (each a Position in the lowest bucket) and collect (index, ink, art) for
    // the CR sort; reject duplicates so a position can't be redeemed twice in one call.
    let mut order: Vec<(usize, u64, u128)> = Vec::with_capacity(ctx.remaining_accounts.len());
    let mut seen: Vec<Pubkey> = Vec::with_capacity(ctx.remaining_accounts.len());
    for (i, info) in ctx.remaining_accounts.iter().enumerate() {
        // CLOSED-candidate skip: a candidate repaid + `close_position`d between tx build and
        // execution is no longer a program-owned account, and `Account::try_from` would revert
        // the WHOLE batch — a
        // borrower-controllable grief (rent refunded), worst during a depeg when the floor
        // matters most. Skip it like any other no-longer-valid candidate. The hard reverts
        // below stay: a PRESENT-but-wrong account (program-owned non-Position → discriminator
        // error; wrong-market Position → Unauthorized) is a redeemer error, not borrower grief.
        // NB: a closed key passed twice is skipped twice (this guard runs before the dedup) —
        // benign under skip semantics, pinned by the regression suite.
        if info.owner != &crate::ID || info.data_is_empty() {
            continue;
        }
        let mut pos = Account::<Position>::try_from(info)?;
        require_keys_eq!(pos.collateral_mint, coll_mint, FusdError::Unauthorized);
        let key = info.key();
        require!(!seen.contains(&key), FusdError::DuplicateRedemptionTarget);
        seen.push(key);
        // Accept a candidate iff it carries debt AND is either a member of the lowest NORMAL bucket
        // (the strict can't-skip-a-lower-bucket guarantee) or a zombie-pen member (drainable out-of-
        // band). Anything else — a higher normal bucket, a borrower who reactively re-bucketed to
        // dodge, a position another redeemer already cleared — is SKIPPED (not a whole-batch revert):
        // reverting on one stale candidate would let a single dodger grief the tx; skipping keeps the
        // floor live. The strict guarantee holds: `lowest_normal` is still find-first-set and
        // only its genuine members (plus out-of-ordering zombies) are redeemed.
        let b = pos.bucket as usize;
        if !(pos.recorded_debt > 0 && (b == ZOMBIE_BUCKET || Some(b) == lowest_normal)) {
            continue;
        }
        // Bring each candidate fully current FIRST (redeem is a position touch like every other):
        // realize its interest + any pending tier-2 redistribution into `recorded_debt`, and fold the
        // weighted-sum delta — so the CR sort, the underwater cap, the redeemed amount, and the
        // bucket-leave all use TRUE debt/collateral. Otherwise a position redeemed-to-zero on its
        // stale recorded debt would resurrect pending debt on its next touch, out of its bucket and
        // untargetable, with agg_recorded_debt carrying debt for which no fUSD was burned.
        let old_weighted = accrual::weighted(&pos)?;
        accrual::realize(&ctx.accounts.market, &mut pos, now)?;
        accrual::reweight(&mut ctx.accounts.market, &pos, old_weighted)?;
        // Recompute the stake here too: `realize` may have grown `ink` (folded-in redistributed
        // collateral). A candidate validated in this pass but NOT reached in the redeem pass below
        // (the `remaining == 0` break, or a `redeem_amt == 0` skip) would otherwise persist grown ink
        // with a stale stake until its next touch — mirror `urgent_redeem`'s per-candidate set_stake.
        crate::redist::set_stake(&mut ctx.accounts.market, &mut pos)?;
        // NB: bucket membership is deliberately NOT reconciled here — it is deferred to the redeem
        // loop below — so this validation pass keeps a CONSISTENT `lowest_normal` snapshot (mutating
        // the bitmap mid-validation could move a candidate's bucket and invalidate the snapshot used to
        // accept the others). Consequence: if `realize` restores a dust zombie's health (ink ↑ via
        // redistribution) but the redeem pass then skips it, it keeps its stale `ZOMBIE_BUCKET` label
        // until its next touch — benign and self-healing (it stays counted once, never blocks the floor,
        // is still drainable out-of-band, and reconciles on any later touch).
        order.push((i, pos.ink, pos.recorded_debt));
        pos.exit(&crate::ID)?;
    }
    require!(!order.is_empty(), FusdError::NothingToRedeem);

    // Lowest collateral-ratio first among the submitted candidates (the disclosed bucket-level
    // fairness; the program ignores the submitted order).
    order.sort_by(|a, b| rb::cmp_collateral_ratio(a.1, a.2, b.1, b.2));

    let mut remaining = amount as u128;
    let mut redeemer_coll_total: u128 = 0;
    let mut fee_coll_total: u128 = 0;

    for (idx, _, _) in order.iter() {
        if remaining == 0 {
            break;
        }
        let info = &ctx.remaining_accounts[*idx];
        let mut pos = Account::<Position>::try_from(info)?;

        let debt = pos.recorded_debt; // already realized in the validation pass above
        let coll_value = ray_mul(pos.ink as u128, spot).ok_or(FusdError::MathOverflow)?;
        // Cap at the position's debt AND its collateral value, so redemption never creates bad
        // debt on an under-water position.
        let redeem_amt = remaining.min(debt).min(coll_value);
        if redeem_amt == 0 {
            continue;
        }
        // Collateral removed at face value (floor against the redeemer), capped at `ink`.
        let coll_total = mul_div_floor(redeem_amt, RAY, spot)
            .ok_or(FusdError::MathOverflow)?
            .min(pos.ink as u128);
        // Recorded debt is in fUSD-native units: redeeming `redeem_amt` reduces it by exactly that
        // (full redemption zeroes it). `redeem_amt <= debt` by the cap above.
        let fee_coll = coll_total.checked_mul(fee_bps).ok_or(FusdError::MathOverflow)? / 10_000;
        let redeemer_coll = coll_total - fee_coll;
        let old_weighted = accrual::weighted(&pos)?;

        // Apply to the position + market aggregates.
        pos.recorded_debt -= redeem_amt;
        pos.ink -= coll_total as u64;
        ctx.accounts.market.agg_recorded_debt = ctx
            .accounts
            .market
            .agg_recorded_debt
            .checked_sub(redeem_amt)
            .ok_or(FusdError::MathOverflow)?;
        ctx.accounts.market.total_collateral = ctx
            .accounts
            .market
            .total_collateral
            .checked_sub(coll_total)
            .ok_or(FusdError::MathOverflow)?;
        ctx.accounts.market.surplus_collateral = ctx
            .accounts
            .market
            .surplus_collateral
            .checked_add(fee_coll as u64)
            .ok_or(FusdError::MathOverflow)?;

        // Drop the redeemed debt from the weighted sum and recompute the stake (ink reduced).
        accrual::reweight(&mut ctx.accounts.market, &pos, old_weighted)?;
        crate::redist::set_stake(&mut ctx.accounts.market, &mut pos)?;
        // Reconcile membership on the post-redeem state (`debt` is the pre-redeem recorded debt > 0, so
        // this is a +→{0|zombie|healthy} transition): a fully-redeemed position leaves; one drained to
        // `ink == 0` (or below `min_debt`) MOVES to the zombie pen — and if it was the sole member of
        // the lowest normal bucket, that clears the bit so the next redeem advances to the next bucket
        // instead of wedging on the now-unredeemable stub. A still-healthy position stays put.
        {
            let mut bm = ctx.accounts.redemption_bitmap.load_mut()?;
            crate::bucket::reconcile(&mut bm, &ctx.accounts.market, &mut pos, debt)?;
        }
        // Persist the (manually-loaded) position back to its account.
        pos.exit(&crate::ID)?;

        redeemer_coll_total = redeemer_coll_total
            .checked_add(redeemer_coll)
            .ok_or(FusdError::MathOverflow)?;
        fee_coll_total = fee_coll_total.checked_add(fee_coll).ok_or(FusdError::MathOverflow)?;
        remaining -= redeem_amt;
    }

    let redeemed_fusd = (amount as u128) - remaining;
    require!(redeemed_fusd > 0, FusdError::NothingToRedeem);

    // Burn the redeemed fUSD from the redeemer.
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

    // Pay out the redeemed collateral (minus fees) from the vault, signed by the market PDA.
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

    emit_cpi!(crate::events::RedemptionEvent {
        collateral_mint: coll_mint,
        redeemer: ctx.accounts.redeemer.key(),
        fusd_burned: redeemed_fusd as u64,
        collateral_paid: redeemer_coll_total as u64,
        fee_collateral: fee_coll_total as u64,
        // The lowest normal bucket targeted, or the ZOMBIE_BUCKET sentinel for a pen-only drain.
        bucket: lowest_normal.unwrap_or(ZOMBIE_BUCKET) as u16,
        candidates: order.len() as u8, // <= MAX_REDEMPTION_CANDIDATES (20), fits u8
    });
    crate::reconcile::assert_collateral_vault_sufficiency(
        &mut ctx.accounts.market_coll_vault,
        &ctx.accounts.market,
    )?;
    Ok(())
}
