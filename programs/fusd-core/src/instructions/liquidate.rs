use anchor_lang::prelude::*;
use anchor_spl::token::{self, Burn, Mint, Token, TokenAccount, Transfer};
use fusd_math::reactor_pool as rpm;
use fusd_math::{mul_div_floor, recovery};

use crate::accrual;
use crate::cdp;
use crate::constants::{
    BUFFER_SEED, FUSD_MINT_SEED, MARKET_SEED, MAX_PRICE_STALENESS_SLOTS, REDEMPTION_BITMAP_SEED,
    SHUTDOWN_REASON_UNHOMED_BAD_DEBT, REACTOR_MAX_SCALES, REACTOR_POOL_SEED,
};
use crate::errors::FusdError;
use crate::reactor;
use crate::state::{
    EpochToScaleToSum, InsuranceBuffer, Market, Position, RedemptionBitmap, ReactorPool,
};

/// Permissionless liquidation of an under-MCR position, via the loss-absorption waterfall
/// ([`recovery::absorb`]): **Reactor Pool** offset → **redistribution** to other positions →
/// **insurance buffer** (burns its fUSD) → **un-homed** (trips `shutdown`). The four tiers account
/// for the full debt, so a liquidation can NEVER stall — it either fully absorbs, or trips the
/// terminal wind-down (replacing the old `NoRedistributionRecipients` revert). fusion-docs.md.
#[event_cpi]
#[derive(Accounts)]
pub struct Liquidate<'info> {
    /// Permissionless caller; receives the SOL reserve bond + the collateral gas-comp.
    ///
    /// Data-bearing accounts below are boxed: 13 inline `Account` payloads overflow the
    /// 4 KB BPF stack frame in `try_accounts` (measured 5504 B — same class as the
    /// `InitReactorPool` fix).
    #[account(mut)]
    pub liquidator: Signer<'info>,

    pub collateral_mint: Box<Account<'info, Mint>>,

    #[account(mut, seeds = [MARKET_SEED, collateral_mint.key().as_ref()], bump = market.bump)]
    pub market: Box<Account<'info, Market>>,

    #[account(mut, has_one = collateral_mint)]
    pub position: Box<Account<'info, Position>>,

    #[account(mut, seeds = [REACTOR_POOL_SEED, collateral_mint.key().as_ref()], bump = reactor_pool.bump)]
    pub reactor_pool: Box<Account<'info, ReactorPool>>,

    #[account(mut, address = reactor_pool.epoch_to_scale_to_sum)]
    pub epoch_to_scale_to_sum: AccountLoader<'info, EpochToScaleToSum>,

    #[account(mut, address = market.collateral_vault)]
    pub market_coll_vault: Box<Account<'info, TokenAccount>>,

    #[account(mut, address = reactor_pool.fusd_vault)]
    pub reactor_fusd_vault: Box<Account<'info, TokenAccount>>,

    #[account(mut, address = reactor_pool.coll_vault)]
    pub reactor_coll_vault: Box<Account<'info, TokenAccount>>,

    #[account(mut, seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Box<Account<'info, Mint>>,

    /// The liquidator's collateral ATA — receives the gas-comp skim (no-op when the market sets
    /// `liq_gas_comp_bps == 0`, but always required). `token::authority = liquidator` pins it to the
    /// caller, so it can't alias a program vault (which would make the skim a no-op self-transfer
    /// and desync `total_collateral` from the vault balance).
    #[account(mut, token::mint = collateral_mint, token::authority = liquidator)]
    pub liquidator_collateral_ata: Box<Account<'info, TokenAccount>>,

    #[account(mut, seeds = [REDEMPTION_BITMAP_SEED, collateral_mint.key().as_ref()], bump)]
    pub redemption_bitmap: AccountLoader<'info, RedemptionBitmap>,

    /// Tier 3: the per-market insurance buffer (protocol first-loss fUSD reserve). Required on every
    /// liquidation so the waterfall always has its third tier; if empty it simply contributes 0 and
    /// the residual falls through to the terminal `unhomed` → `shutdown`.
    #[account(mut, seeds = [BUFFER_SEED, collateral_mint.key().as_ref()], bump = insurance_buffer.bump)]
    pub insurance_buffer: Box<Account<'info, InsuranceBuffer>>,

    #[account(mut, address = insurance_buffer.fusd_vault)]
    pub buffer_fusd_vault: Box<Account<'info, TokenAccount>>,

    /// Tier 3.5: the Global Backstop Reserve + its vault (OPTIONAL).
    /// When BOTH are supplied, a liquidation whose loss spills past the local buffer may draw from the
    /// reserve up to the per-market hybrid cap, before booking un-homed bad debt. When omitted (no
    /// backstop, or a market that doesn't use it), `global_available == 0` and the waterfall is the
    /// pre-backstop 4-tier. Boxed (liquidate is already stack-tight).
    #[account(mut, seeds = [crate::constants::BACKSTOP_SEED], bump = backstop.bump)]
    pub backstop: Option<Box<Account<'info, crate::state::GlobalBackstopReserve>>>,

    #[account(mut)]
    pub backstop_fusd_vault: Option<Box<Account<'info, TokenAccount>>>,

    pub token_program: Program<'info, Token>,
}

pub fn handler(ctx: Context<Liquidate>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    accrual::accrue(&mut ctx.accounts.market, now)?;
    // Liquidation eligibility AND the seize conversion price off the HIGH (debt) price
    // `Market.debt_spot` (= price + k·σ), never the LOW `spot`. Under price uncertainty a position is
    // liquidated only when underwater at the OPTIMISTIC valuation, so a wide confidence band cannot
    // drive a destructive, irreversible liquidation on noise (cf. borrow/withdraw/redeem/CCR/SCR,
    // which keep the conservative LOW `spot` — pessimism protects *extending*/*winding-down* risk,
    // optimism protects *destroying* a position). `spot`/`spot_updated_slot` stay the shared freshness
    // clock; `debt_spot >= spot` by construction, so `spot > 0 ⇒ debt_spot > 0` (the guard is defensive
    // against a market priced before this field existed, which reads 0 and is fail-closed un-liquidatable).
    let liq_spot = ctx.accounts.market.debt_spot;
    let mcr = ctx.accounts.market.mcr_bps;

    // Liquidation must price against a fresh oracle.
    let slot = Clock::get()?.slot;
    require!(ctx.accounts.market.spot > 0 && liq_spot > 0, FusdError::OracleUnavailable);
    require!(
        slot.saturating_sub(ctx.accounts.market.spot_updated_slot) <= MAX_PRICE_STALENESS_SLOTS,
        FusdError::StalePrice
    );
    // On-resume grace: after the price recovers from a staleness halt, liquidations stay paused until
    // `liq_grace_until` so borrowers who couldn't act during the outage get a window to cure before a
    // stale-then-fresh price can cascade. 0 when no halt occurred — a no-op in steady state.
    require!(
        slot >= ctx.accounts.market.liq_grace_until,
        FusdError::LiquidationGracePeriod
    );
    // Divergence gate: pause liquidations while a FRESH primary grossly disagrees with a PRESENT
    // secondary (the verdict cached by `update_price`, with a post-convergence grace). A manipulated
    // or briefly-bad primary the protocol's own secondaries visibly reject cannot drive a liquidation
    // cascade. Redemption, urgent_redeem, and repay NEVER gate on this — the peg floor must
    // clear under divergence. 0 ⇒ no active pause (gate disabled, or feeds converged + grace elapsed).
    require!(
        slot >= ctx.accounts.market.liq_divergence_until,
        FusdError::OracleDivergent
    );

    let art_before = ctx.accounts.position.recorded_debt;
    // The victim's weight to strip from the aggregate (captured before realize; applied after zeroing).
    let old_weighted = accrual::weighted(&ctx.accounts.position)?;
    // Bring the victim current — accrue its interest + any pending redistribution that pushed it under MCR.
    accrual::realize(&ctx.accounts.market, &mut ctx.accounts.position, now)?;

    let debt = ctx.accounts.position.recorded_debt; // realized present-value debt (fUSD-native)
    let ink = ctx.accounts.position.ink;
    require!(debt > 0, FusdError::PositionHealthy);
    // Only liquidatable strictly below MCR.
    require!(
        !cdp::is_healthy(ink, debt, liq_spot, mcr),
        FusdError::PositionHealthy
    );

    // Remove the victim from the system aggregates BEFORE distributing, so redistribution targets
    // only the OTHER positions.
    ctx.accounts.market.total_stakes = ctx
        .accounts
        .market
        .total_stakes
        .checked_sub(ctx.accounts.position.stake)
        .ok_or(FusdError::MathOverflow)?;
    ctx.accounts.market.total_collateral = ctx
        .accounts
        .market
        .total_collateral
        .checked_sub(ink as u128)
        .ok_or(FusdError::MathOverflow)?;

    // Bonus collar: a liquidation seizes collateral worth at most `debt · (1 + liq_bonus_bps)`;
    // the surplus above that is returned to the borrower as `Position.coll_surplus` (held in the vault,
    // claimable via `claim_coll_surplus`, kept OUT of `ink`/stake so the liquidated owner can't inherit
    // redistributed debt on the leftover). `liq_bonus_bps == 0` ⇒ seize all (collar off). The DEBT
    // absorption below is unchanged — only the collateral distribution is capped. The surplus is the
    // borrower's OWN over-cap collateral, so it is returned EVEN when this same liquidation books
    // un-homed bad debt: the retained `seize_coll` (worth ≥ the full debt) backs that loss, never the surplus.
    let (seize_coll, surplus_coll) =
        cdp::seize_collateral(ink, debt, liq_spot, ctx.accounts.market.liq_bonus_bps)
            .ok_or(FusdError::MathOverflow)?;
    if surplus_coll > 0 {
        // The surplus stays in the vault (no token move); it leaves `total_collateral` (removed with
        // `ink` above) and is tracked separately as owed-to-owner.
        ctx.accounts.position.coll_surplus = ctx
            .accounts
            .position
            .coll_surplus
            .checked_add(surplus_coll)
            .ok_or(FusdError::MathOverflow)?;
        ctx.accounts.market.total_coll_surplus = ctx
            .accounts
            .market
            .total_coll_surplus
            .checked_add(surplus_coll)
            .ok_or(FusdError::MathOverflow)?;
    }

    // Liquidator collateral gas-comp, skimmed off the SEIZED collateral before the RP/redistribution
    // split (`liq_gas_comp_bps` is clamped <= MAX_LIQ_GAS_COMP_BPS = 1_000, so gas_comp <= seize_coll/10
    // and `distributable > 0` when `seize_coll > 0` — the coll_sp/coll_r split relies on that).
    let gas_comp = (seize_coll as u128)
        .checked_mul(ctx.accounts.market.liq_gas_comp_bps as u128)
        .ok_or(FusdError::MathOverflow)?
        / 10_000;
    let distributable = (seize_coll as u128) - gas_comp;

    // Loss-absorption waterfall (`recovery::absorb`, proven in fusd-math): split the present debt
    // across RP → redistribution → insurance buffer → un-homed. The debt split is over the full
    // present debt; the collateral split is over `distributable` (post gas-comp).
    let reactor_deposits = ctx.accounts.reactor_pool.total_deposits;
    let has_recipients = ctx.accounts.market.total_stakes > 0; // the victim is already excluded above
    let buffer_balance = ctx.accounts.buffer_fusd_vault.amount as u128;
    // Tier 3.5 (global backstop): the fUSD this market may draw from the shared reserve for THIS
    // liquidation — the per-market hybrid cap floored by the live reserve balance. 0 when the backstop
    // accounts are omitted / unconfigured, so the waterfall is the pre-backstop 4-tier. `agg_recorded_debt`
    // (post-accrue, victim still included) is the market-size input to the debt-share cap arm.
    let global_available: u128 = match (
        ctx.accounts.backstop.as_ref(),
        ctx.accounts.backstop_fusd_vault.as_ref(),
    ) {
        (Some(bs), Some(vault)) => {
            require_keys_eq!(vault.key(), bs.fusd_vault, FusdError::InvalidRecipient);
            crate::instructions::global_backstop::draw_available(
                bs,
                ctx.accounts.market.global_contributed,
                ctx.accounts.market.global_drawn,
                ctx.accounts.market.agg_recorded_debt,
                vault.amount as u128,
            )
        }
        _ => 0,
    };
    let split = recovery::absorb(debt, reactor_deposits, has_recipients, buffer_balance, global_available);

    // The waterfall's supply-identity transition (the shared body certora.rs proves), applied in ONE
    // step: the reactor/buffer/global tiers extinguish their debt from `agg_recorded_debt`, the
    // un-homed remainder moves agg → `bad_debt`, and `split.redist` deliberately stays parked in agg
    // (reassigned to survivors — supply-neutral). The tier branches below move the tokens and their
    // own counters but no longer touch these two fields.
    let sd = crate::supply_transition::liquidate(
        ctx.accounts.market.agg_recorded_debt,
        ctx.accounts.market.bad_debt,
        split.reactor,
        split.redist,
        split.buffer,
        split.global,
        split.unhomed,
    )
    .ok_or(FusdError::MathOverflow)?;
    ctx.accounts.market.agg_recorded_debt = sd.new_agg;
    ctx.accounts.market.bad_debt = sd.new_bad;

    let offset_present = split.reactor;
    let coll_sp = mul_div_floor(distributable, offset_present, debt).ok_or(FusdError::MathOverflow)?;
    // Collateral backing the post-RP remainder (exact remainder so `coll_sp + coll_r == distributable`):
    // redistributed to others (tier 2), or left protocol-owned in the market vault (tiers 3/4).
    let coll_r = distributable - coll_sp;
    // Recorded debt is fUSD-native, so the waterfall splits ARE the debt amounts directly (no
    // `art*rate` conversion): `split.reactor + split.redist + split.buffer + split.unhomed == debt` exactly
    // (the `recovery::absorb` conservation, proven in Kani). `offset_present == split.reactor` is the RP's
    // share; the post-RP remainder is `debt − offset_present`.

    let coll_key = ctx.accounts.collateral_mint.key();

    // ---- Liquidator gas-comp: collateral escrow -> liquidator ATA (signed by the market PDA) ----
    if gas_comp > 0 {
        let gas_comp_u64 = u64::try_from(gas_comp).map_err(|_| FusdError::MathOverflow)?;
        let m_bump = ctx.accounts.market.bump;
        let signer: &[&[&[u8]]] = &[&[MARKET_SEED, coll_key.as_ref(), &[m_bump]]];
        token::transfer(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.market_coll_vault.to_account_info(),
                    to: ctx.accounts.liquidator_collateral_ata.to_account_info(),
                    authority: ctx.accounts.market.to_account_info(),
                },
                signer,
            ),
            gas_comp_u64,
        )?;
    }

    // ---- Tier 1: Reactor Pool offset (only the portion the pool can cover) ----
    if offset_present > 0 {
        let mut ps = reactor::pool_state(&ctx.accounts.reactor_pool);
        {
            let mut grid = ctx.accounts.epoch_to_scale_to_sum.load_mut()?;
            rpm::offset(&mut ps, &mut grid.data, REACTOR_MAX_SCALES, offset_present, coll_sp)
                .map_err(reactor::map_err)?;
        }
        reactor::write_back(&mut ctx.accounts.reactor_pool, &ps);

        // Burn the offset debt from the pool's fUSD vault (signed by the RP PDA).
        let offset_u64 = u64::try_from(offset_present).map_err(|_| FusdError::MathOverflow)?;
        {
            let reactor_bump = ctx.accounts.reactor_pool.bump;
            let signer: &[&[&[u8]]] = &[&[REACTOR_POOL_SEED, coll_key.as_ref(), &[reactor_bump]]];
            token::burn(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Burn {
                        mint: ctx.accounts.fusd_mint.to_account_info(),
                        from: ctx.accounts.reactor_fusd_vault.to_account_info(),
                        authority: ctx.accounts.reactor_pool.to_account_info(),
                    },
                    signer,
                ),
                offset_u64,
            )?;
        }

        // Move the RP's share of the seized collateral market-escrow -> RP collateral vault (signed
        // by the market PDA). Depositors claim it via `claim_reactor_gains`.
        if coll_sp > 0 {
            let coll_sp_u64 = u64::try_from(coll_sp).map_err(|_| FusdError::MathOverflow)?;
            let m_bump = ctx.accounts.market.bump;
            let signer: &[&[&[u8]]] = &[&[MARKET_SEED, coll_key.as_ref(), &[m_bump]]];
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.market_coll_vault.to_account_info(),
                        to: ctx.accounts.reactor_coll_vault.to_account_info(),
                        authority: ctx.accounts.market.to_account_info(),
                    },
                    signer,
                ),
                coll_sp_u64,
            )?;
        }
        // The offset debt is extinguished (recorded debt is native, so the RP's share is
        // `split.reactor`) — already dropped from `agg_recorded_debt` by the consolidated supply
        // transition above.
    }

    // ---- Tier 2: redistribute the remainder to the other active positions ----
    if split.redist > 0 {
        let mut rs = crate::redist::state(&ctx.accounts.market);
        fusd_math::redistribution::redistribute(
            &mut rs,
            ctx.accounts.market.total_stakes,
            coll_r,
            split.redist,
        )
        .map_err(crate::redist::map_err)?;
        crate::redist::write_state(&mut ctx.accounts.market, &rs);

        // The redistributed collateral stays in the market vault, now backing the other positions (no
        // token move); `split.redist` stays in `agg_recorded_debt` (now owed by them — NOT
        // extinguished). It is **parked out of `agg_weighted_debt_sum`** (the victim's whole weight was
        // already captured in `old_weighted`); recipients re-weight it at their own rate when they next
        // touch (BOLD lazy redistribution).
        ctx.accounts.market.total_collateral = ctx
            .accounts
            .market
            .total_collateral
            .checked_add(coll_r)
            .ok_or(FusdError::MathOverflow)?;
    } else if split.buffer > 0 || split.global > 0 || split.unhomed > 0 {
        // ---- Tier 3 (local buffer) + Tier 3.5 (global backstop) + Tier 4 (un-homed → shutdown):
        //      no redistribution recipient ----
        // The post-RP collateral has nowhere to redistribute, so it stays PROTOCOL-OWNED in the market
        // vault (backing the wind-down). No token move — only the accounting changes. It is booked into
        // `protocol_collateral` (NOT `total_collateral`, which only backs live positions): it backs no
        // position, and tracking it separately keeps the vault invariant exact and makes it recoverable
        // in O(1) via `sweep_protocol_collateral` (deployed against `bad_debt`).
        let coll_r_u64 = u64::try_from(coll_r).map_err(|_| FusdError::MathOverflow)?;
        ctx.accounts.market.protocol_collateral = ctx
            .accounts
            .market
            .protocol_collateral
            .checked_add(coll_r_u64)
            .ok_or(FusdError::MathOverflow)?;

        // Tier 3: the buffer burns its OWN fUSD to extinguish `split.buffer` of debt (the matching
        // collateral already stays in the market vault above — the buffer is an fUSD-only reserve).
        if split.buffer > 0 {
            let buf_u64 = u64::try_from(split.buffer).map_err(|_| FusdError::MathOverflow)?;
            {
                let buf_bump = ctx.accounts.insurance_buffer.bump;
                let signer: &[&[&[u8]]] = &[&[BUFFER_SEED, coll_key.as_ref(), &[buf_bump]]];
                token::burn(
                    CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        Burn {
                            mint: ctx.accounts.fusd_mint.to_account_info(),
                            from: ctx.accounts.buffer_fusd_vault.to_account_info(),
                            authority: ctx.accounts.insurance_buffer.to_account_info(),
                        },
                        signer,
                    ),
                    buf_u64,
                )?;
            }
            // (`split.buffer` already left `agg_recorded_debt` via the consolidated transition.)
            ctx.accounts.insurance_buffer.total_absorbed = ctx
                .accounts
                .insurance_buffer
                .total_absorbed
                .checked_add(split.buffer)
                .ok_or(FusdError::MathOverflow)?;
        }

        // Tier 3.5: the GLOBAL backstop reserve burns its OWN fUSD to extinguish `split.global` of debt
        // (bounded by the per-market draw cap that produced `global_available`). The matching collateral
        // already stays protocol-owned in the market vault above (the reserve is an fUSD-only second-loss
        // pool). `split.global > 0` ⇒ both backstop accounts are present (the cap was computed from them).
        if split.global > 0 {
            let glob_u64 = u64::try_from(split.global).map_err(|_| FusdError::MathOverflow)?;
            let bs_ai = ctx.accounts.backstop.as_ref().unwrap().to_account_info();
            let vault_ai = ctx.accounts.backstop_fusd_vault.as_ref().unwrap().to_account_info();
            let bs_bump = ctx.accounts.backstop.as_ref().unwrap().bump;
            {
                let signer: &[&[&[u8]]] = &[&[crate::constants::BACKSTOP_SEED, &[bs_bump]]];
                token::burn(
                    CpiContext::new_with_signer(
                        ctx.accounts.token_program.to_account_info(),
                        Burn {
                            mint: ctx.accounts.fusd_mint.to_account_info(),
                            from: vault_ai,
                            authority: bs_ai,
                        },
                        signer,
                    ),
                    glob_u64,
                )?;
            }
            // (`split.global` already left `agg_recorded_debt` via the consolidated transition.)
            // Per-market cumulative draw (enforces the cap across repeated draws — LOCAL write).
            ctx.accounts.market.global_drawn = ctx
                .accounts
                .market
                .global_drawn
                .checked_add(split.global)
                .ok_or(FusdError::MathOverflow)?;
            let bs = ctx.accounts.backstop.as_mut().unwrap();
            bs.total_absorbed =
                bs.total_absorbed.checked_add(split.global).ok_or(FusdError::MathOverflow)?;
            emit_cpi!(crate::events::BackstopDrawn {
                collateral_mint: coll_key,
                amount: split.global,
                total_absorbed: bs.total_absorbed,
            });
        }

        // Tier 4: any residual is UN-HOMED bad debt — fUSD that stays in circulation with no backing
        // debt. The loss is already realized by the consolidated transition above (dropped from
        // `agg_recorded_debt`, booked to `bad_debt`); here trip the terminal shutdown so
        // `urgent_redeem` winds the market down (the position is zeroed below, so it can never be
        // re-liquidated into a stall). This replaces the old NoRedistributionRecipients revert.
        if split.unhomed > 0 {
            // First-set-wins: never clobber a reason already set by `shutdown` (SCR / oracle failure).
            if ctx.accounts.market.shutdown_reason == crate::constants::SHUTDOWN_REASON_NONE {
                ctx.accounts.market.shutdown_reason = SHUTDOWN_REASON_UNHOMED_BAD_DEBT;
            }
            // ShutdownEvent fires exactly once per market (the false→true transition); a later tier-4
            // liquidation in an already-shut market re-books bad debt without re-announcing shutdown.
            if !ctx.accounts.market.shutdown {
                ctx.accounts.market.shutdown = true;
                emit_cpi!(crate::events::ShutdownEvent {
                    collateral_mint: coll_key,
                    reason: ctx.accounts.market.shutdown_reason,
                });
            }
            emit_cpi!(crate::events::BadDebtEvent {
                collateral_mint: coll_key,
                position: ctx.accounts.position.key(),
                amount: split.unhomed,
                total_bad_debt: ctx.accounts.market.bad_debt,
            });
        }
    }

    // System snapshots for the stake formula, captured after removing the victim and adding back the
    // redistributed collateral. Liquity `_updateSystemSnapshots_excludeCollRemainder`: when no stake
    // remains we snapshot ZERO collateral — otherwise `compute_stake` (which only self-heals at `tcs ==
    // 0`) would return stake 0 for every future position, pinning `total_stakes` at 0 and permanently
    // bricking tier-2 redistribution for the market. With zero snapshot, the next depositor takes the
    // genesis identity (stake == coll) and the redistribution machinery re-arms. (The un-homed remainder
    // now lands in `protocol_collateral`, not `total_collateral`, so the snapshot already excludes it;
    // the zero-when-no-stake rule still guards the residual redistribution floor dust.)
    ctx.accounts.market.total_stakes_snapshot = ctx.accounts.market.total_stakes;
    ctx.accounts.market.total_collateral_snapshot = if ctx.accounts.market.total_stakes == 0 {
        0
    } else {
        ctx.accounts.market.total_collateral
    };

    // Pay the SOL liquidation bond to the liquidator (direct lamport accounting on the
    // program-owned position account; the bond sits on top of rent, so this never under-funds rent).
    let bond = ctx.accounts.position.reserve_lamports;
    if bond > 0 {
        // Checked lamport moves (revert cleanly on any invariant violation rather than panic). The
        // bond sits on top of rent, so this never under-funds the position's rent-exemption.
        ctx.accounts.position.to_account_info().sub_lamports(bond)?;
        ctx.accounts.liquidator.to_account_info().add_lamports(bond)?;
    }

    // Close out the victim (bond consumed; the owner can `close_position` to reclaim rent).
    ctx.accounts.position.recorded_debt = 0;
    ctx.accounts.position.ink = 0;
    ctx.accounts.position.stake = 0;
    ctx.accounts.position.reserve_lamports = 0;
    ctx.accounts.position.redist_l_coll_snapshot = ctx.accounts.market.l_coll;
    ctx.accounts.position.redist_l_art_snapshot = ctx.accounts.market.l_art;

    // Strip the victim's ENTIRE weight from `agg_weighted_debt_sum` (new weight = 0 after zeroing).
    // The redistributed portion's weight is re-added lazily at the recipients' own rates when they
    // next touch — never here (BOLD lazy redistribution).
    accrual::reweight(&mut ctx.accounts.market, &ctx.accounts.position, old_weighted)?;

    // Reconcile redemption rate-bucket membership (the victim leaves its bucket).
    {
        let mut bm = ctx.accounts.redemption_bitmap.load_mut()?;
        crate::bucket::reconcile(
            &mut bm,
            &ctx.accounts.market,
            &mut ctx.accounts.position,
            art_before,
        )?;
    }

    emit_cpi!(crate::events::LiquidationEvent {
        collateral_mint: coll_key,
        position: ctx.accounts.position.key(),
        owner: ctx.accounts.position.owner,
        liquidator: ctx.accounts.liquidator.key(),
        debt,
        seized_collateral: seize_coll,
        gas_comp: gas_comp as u64, // <= seize_coll/10, fits u64
        coll_surplus: surplus_coll,
        reactor_offset: split.reactor,
        redistributed: split.redist,
        buffer_absorbed: split.buffer,
        backstop_absorbed: split.global,
        unhomed: split.unhomed,
        spot: liq_spot, // the HIGH (debt) price liquidation actually priced at
    });
    // Highest-value reconcile site: the 4-tier waterfall moves value across all four counters
    // with several accounting-only legs (no token move) — exactly where silent drift would hide.
    crate::reconcile::assert_collateral_vault_sufficiency(
        &mut ctx.accounts.market_coll_vault,
        &ctx.accounts.market,
    )?;
    Ok(())
}
