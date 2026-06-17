//! The Global Backstop Reserve — creation, funding, gov withdrawal of excess, and the TIMELOCKED
//! global-param flow (cut / reserve cap / the four draw-cap coefficients). The waterfall draw
//! (tier 3.5) lives in `liquidate.rs`; the funding cut lives in `refresh_market.rs`. Here: the
//! account lifecycle + the bounded governance surface.
//!
//! Gating mirrors the rest of the protocol: CREATION is `config.gov_authority` (like `init_market_oracle`);
//! the optional TOP-UP is permissionless (donating protocol-strengthening fUSD, like `fund_buffer`);
//! EXCESS WITHDRAWAL is the gate's `inbound_authority` (like `withdraw_surplus`); PARAM TUNING runs
//! through the QUEUE → timelock → EXECUTE flow (like `MarketParam`).

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer};

use crate::constants::{
    BACKSTOP_FUSD_VAULT_SEED, BACKSTOP_SEED, CONFIG_SEED, DEFAULT_BACKSTOP_CUT_BPS,
    DEFAULT_BACKSTOP_DRAW_BASE, DEFAULT_BACKSTOP_DRAW_CEILING_SHARE_BPS,
    DEFAULT_BACKSTOP_DRAW_DEBT_SHARE_BPS, DEFAULT_BACKSTOP_DRAW_K_BPS, DEFAULT_BACKSTOP_RESERVE_CAP,
    FUSD_MINT_SEED, GLOBAL_TIMELOCK_SEED, GOV_GATE_SEED, MAX_BACKSTOP_CUT_BPS,
    MAX_BACKSTOP_DRAW_CEILING_SHARE_BPS, MAX_BACKSTOP_DRAW_DEBT_SHARE_BPS, MAX_BACKSTOP_DRAW_K_BPS,
};
use crate::errors::FusdError;
use crate::state::{GlobalBackstopReserve, GovernanceGate, ProtocolConfig};

pub use crate::state::GlobalParam;

// ----------------------------------------- draw-cap helper ---------------------------------------

/// The fUSD a market may draw from the reserve for THIS liquidation — the hybrid per-market cap, then
/// floored by the live reserve balance. Pure; returns 0 when the backstop is unconfigured (every
/// coefficient 0) since each `min` arm is then 0. Read by `liquidate` (tier 3.5):
///
/// `min(base + draw_k·contributed, ceiling_share·reserve, debt_share·debt) − already_drawn`, then
/// `min(.., reserve_balance)`.
///
/// - base + contribution arm: a base allowance (useful to new gov-onboarded markets) plus
///   contribution-weighted access (fairness / anti-free-ride).
/// - ceiling-share arm: a single failure can take at most this fraction of the live reserve.
/// - debt-share arm: cumulative draws bounded vs the market's own debt.
/// - `− already_drawn`: enforces the cap across repeated draws.
pub fn draw_available(
    reserve: &GlobalBackstopReserve,
    market_global_contributed: u128,
    market_global_drawn: u128,
    market_debt: u128,
    reserve_balance: u128,
) -> u128 {
    let contribution_arm = (reserve.draw_base_allowance as u128).saturating_add(
        market_global_contributed.saturating_mul(reserve.draw_k_bps as u128) / 10_000,
    );
    let ceiling_arm = reserve_balance.saturating_mul(reserve.draw_ceiling_share_bps as u128) / 10_000;
    let debt_arm = market_debt.saturating_mul(reserve.draw_debt_share_bps as u128) / 10_000;
    let cap = contribution_arm.min(ceiling_arm).min(debt_arm);
    cap.saturating_sub(market_global_drawn).min(reserve_balance)
}

// ----------------------------------------- shared validate/apply ---------------------------------

/// Compile-time clamp check for a (GlobalParam, value). Run at QUEUE (fail fast) and again at EXECUTE
/// (a stored op can never apply out-of-bounds). `ReserveCap`/`DrawBase` are fUSD amounts with no upper
/// clamp (the protocol's own sizing); the bps coefficients are clamped.
fn validate_global_param(param: GlobalParam, value: u64) -> Result<()> {
    match param {
        GlobalParam::Cut => require!(value <= MAX_BACKSTOP_CUT_BPS as u64, FusdError::ParamOutOfBounds),
        GlobalParam::ReserveCap => {}
        GlobalParam::DrawBase => {}
        GlobalParam::DrawK => require!(value <= MAX_BACKSTOP_DRAW_K_BPS, FusdError::ParamOutOfBounds),
        GlobalParam::DrawCeilingShare => {
            require!(value <= MAX_BACKSTOP_DRAW_CEILING_SHARE_BPS as u64, FusdError::ParamOutOfBounds)
        }
        GlobalParam::DrawDebtShare => {
            require!(value <= MAX_BACKSTOP_DRAW_DEBT_SHARE_BPS as u64, FusdError::ParamOutOfBounds)
        }
    }
    Ok(())
}

/// Read a param's CURRENT value (the prev_value for the forensic event trail). Exhaustive + wildcard-
/// free so adding a `GlobalParam` variant without deciding its clamp/setter/reader is a compile error.
fn current_global_param(b: &GlobalBackstopReserve, param: GlobalParam) -> u64 {
    match param {
        GlobalParam::Cut => b.cut_bps as u64,
        GlobalParam::ReserveCap => b.reserve_cap,
        GlobalParam::DrawBase => b.draw_base_allowance,
        GlobalParam::DrawK => b.draw_k_bps,
        GlobalParam::DrawCeilingShare => b.draw_ceiling_share_bps as u64,
        GlobalParam::DrawDebtShare => b.draw_debt_share_bps as u64,
    }
}

/// Apply a validated (param, value). Caller MUST have run [`validate_global_param`] first.
fn apply_global_param(b: &mut GlobalBackstopReserve, param: GlobalParam, value: u64) {
    match param {
        GlobalParam::Cut => b.cut_bps = value as u16,
        GlobalParam::ReserveCap => b.reserve_cap = value,
        GlobalParam::DrawBase => b.draw_base_allowance = value,
        GlobalParam::DrawK => b.draw_k_bps = value,
        GlobalParam::DrawCeilingShare => b.draw_ceiling_share_bps = value as u16,
        GlobalParam::DrawDebtShare => b.draw_debt_share_bps = value as u16,
    }
}

// ----------------------------------------- init_global_backstop ----------------------------------

#[event_cpi]
#[derive(Accounts)]
pub struct InitGlobalBackstop<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,

    #[account(seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Box<Account<'info, Mint>>,

    #[account(
        init,
        payer = authority,
        space = GlobalBackstopReserve::SPACE,
        seeds = [BACKSTOP_SEED],
        bump,
    )]
    pub backstop: Box<Account<'info, GlobalBackstopReserve>>,

    #[account(
        init,
        payer = authority,
        seeds = [BACKSTOP_FUSD_VAULT_SEED],
        bump,
        token::mint = fusd_mint,
        token::authority = backstop,
    )]
    pub backstop_fusd_vault: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

/// One-time: create the global reserve + its fUSD vault. Gated on `config.gov_authority`. Ships fully
/// INERT — every param 0/off; governance enables calibrated values via the timelocked global-param flow.
pub fn init(ctx: Context<InitGlobalBackstop>) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );
    let b = &mut ctx.accounts.backstop;
    b.fusd_vault = ctx.accounts.backstop_fusd_vault.key();
    b.cut_bps = DEFAULT_BACKSTOP_CUT_BPS;
    b.reserve_cap = DEFAULT_BACKSTOP_RESERVE_CAP;
    b.draw_base_allowance = DEFAULT_BACKSTOP_DRAW_BASE;
    b.draw_k_bps = DEFAULT_BACKSTOP_DRAW_K_BPS;
    b.draw_ceiling_share_bps = DEFAULT_BACKSTOP_DRAW_CEILING_SHARE_BPS;
    b.draw_debt_share_bps = DEFAULT_BACKSTOP_DRAW_DEBT_SHARE_BPS;
    b.total_contributed = 0;
    b.total_absorbed = 0;
    b.total_withdrawn = 0;
    b.bump = ctx.bumps.backstop;
    b._reserved = [0u8; 64];

    emit_cpi!(crate::events::BackstopInitialized { fusd_vault: ctx.accounts.backstop_fusd_vault.key() });
    Ok(())
}

// ----------------------------------------- fund_backstop -----------------------------------------

#[event_cpi]
#[derive(Accounts)]
pub struct FundBackstop<'info> {
    pub funder: Signer<'info>,

    #[account(mut, seeds = [BACKSTOP_SEED], bump = backstop.bump)]
    pub backstop: Box<Account<'info, GlobalBackstopReserve>>,

    #[account(mut, address = backstop.fusd_vault)]
    pub backstop_fusd_vault: Box<Account<'info, TokenAccount>>,

    #[account(seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Box<Account<'info, Mint>>,

    #[account(mut, token::mint = fusd_mint, token::authority = funder)]
    pub funder_fusd_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
}

/// Permissionless top-up: deposit protocol-strengthening fUSD into the reserve (like `fund_buffer`).
/// Counts toward `total_contributed` (the reserve-solvency invariant) but NOT any market's
/// `global_contributed` — a donation grants no market draw access.
pub fn fund(ctx: Context<FundBackstop>, amount: u64) -> Result<()> {
    require!(amount > 0, FusdError::ZeroAmount);
    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.funder_fusd_ata.to_account_info(),
                to: ctx.accounts.backstop_fusd_vault.to_account_info(),
                authority: ctx.accounts.funder.to_account_info(),
            },
        ),
        amount,
    )?;
    ctx.accounts.backstop.total_contributed = ctx
        .accounts
        .backstop
        .total_contributed
        .checked_add(amount as u128)
        .ok_or(FusdError::MathOverflow)?;

    emit_cpi!(crate::events::BackstopFunded {
        funder: ctx.accounts.funder.key(),
        amount,
        total_contributed: ctx.accounts.backstop.total_contributed,
    });
    Ok(())
}

// ----------------------------------------- withdraw_backstop_excess ------------------------------

#[event_cpi]
#[derive(Accounts)]
pub struct WithdrawBackstopExcess<'info> {
    /// MUST equal `gov_gate.inbound_authority` (the governance fund-movers gating pattern).
    pub authority: Signer<'info>,

    #[account(seeds = [GOV_GATE_SEED], bump = gov_gate.bump)]
    pub gov_gate: Box<Account<'info, GovernanceGate>>,

    #[account(mut, seeds = [BACKSTOP_SEED], bump = backstop.bump)]
    pub backstop: Box<Account<'info, GlobalBackstopReserve>>,

    #[account(mut, address = backstop.fusd_vault)]
    pub backstop_fusd_vault: Box<Account<'info, TokenAccount>>,

    #[account(seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Box<Account<'info, Mint>>,

    /// The recipient fUSD account — must NOT be the reserve vault itself (a self-transfer would debit
    /// the counter while stranding the value).
    #[account(mut, token::mint = fusd_mint)]
    pub recipient_fusd_ata: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
}

/// Governance recovers ABOVE-CAP excess from the reserve (never below `reserve_cap` — the cap is the
/// protective floor). Only protocol-owned fUSD, amount-capped to the above-cap excess; unlike
/// `withdraw_surplus`/`sweep` (which debit their counter before transferring), here the CPI transfer
/// IS the debit and `total_withdrawn` is bumped after. Gated on `gov_gate.inbound_authority`.
pub fn withdraw_excess(ctx: Context<WithdrawBackstopExcess>, amount: u64) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.gov_gate.inbound_authority,
        FusdError::Unauthorized
    );
    require!(amount > 0, FusdError::ZeroAmount);
    require_keys_neq!(
        ctx.accounts.recipient_fusd_ata.key(),
        ctx.accounts.backstop_fusd_vault.key(),
        FusdError::InvalidRecipient
    );
    // Only the portion above the cap is withdrawable; the cap stays as the protective reserve floor.
    let balance = ctx.accounts.backstop_fusd_vault.amount;
    let excess = balance.saturating_sub(ctx.accounts.backstop.reserve_cap);
    require!(amount <= excess, FusdError::InsufficientBackstopExcess);

    let signer: &[&[&[u8]]] = &[&[BACKSTOP_SEED, &[ctx.accounts.backstop.bump]]];
    token::transfer(
        CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.backstop_fusd_vault.to_account_info(),
                to: ctx.accounts.recipient_fusd_ata.to_account_info(),
                authority: ctx.accounts.backstop.to_account_info(),
            },
            signer,
        ),
        amount,
    )?;
    ctx.accounts.backstop.total_withdrawn = ctx
        .accounts
        .backstop
        .total_withdrawn
        .checked_add(amount as u128)
        .ok_or(FusdError::MathOverflow)?;

    emit_cpi!(crate::events::BackstopWithdrawn {
        recipient: ctx.accounts.recipient_fusd_ata.key(),
        amount,
        total_withdrawn: ctx.accounts.backstop.total_withdrawn,
    });
    Ok(())
}

// ----------------------------------------- queue_global_param ------------------------------------
// TIMELOCKED global-param flow — mirrors the per-market `queue/execute/cancel_param_change`, sharing
// the `GovernanceGate` (inbound authority + timelock + nonce) but a DISTINCT op account
// (`TimelockedGlobalParam`) + PDA prefix (`GLOBAL_TIMELOCK_SEED`).

#[event_cpi]
#[derive(Accounts)]
pub struct QueueGlobalParamChange<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(mut, seeds = [GOV_GATE_SEED], bump = gov_gate.bump)]
    pub gov_gate: Box<Account<'info, GovernanceGate>>,

    #[account(
        init,
        payer = authority,
        space = crate::state::TimelockedGlobalParam::SPACE,
        seeds = [GLOBAL_TIMELOCK_SEED, gov_gate.queue_nonce.to_le_bytes().as_ref()],
        bump,
    )]
    pub timelocked_param: Box<Account<'info, crate::state::TimelockedGlobalParam>>,

    pub system_program: Program<'info, System>,
}

pub fn queue(ctx: Context<QueueGlobalParamChange>, param: GlobalParam, value: u64) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.gov_gate.inbound_authority,
        FusdError::Unauthorized
    );
    validate_global_param(param, value)?;

    let now = Clock::get()?.unix_timestamp;
    let nonce = ctx.accounts.gov_gate.queue_nonce;

    let op = &mut ctx.accounts.timelocked_param;
    op.nonce = nonce;
    op.eta = now.saturating_add(ctx.accounts.gov_gate.timelock_secs);
    op.param = param;
    op.value = value;
    op.bump = ctx.bumps.timelocked_param;
    op._reserved = [0u8; 16];

    ctx.accounts.gov_gate.queue_nonce =
        ctx.accounts.gov_gate.queue_nonce.checked_add(1).ok_or(FusdError::MathOverflow)?;

    emit_cpi!(crate::events::GlobalParamChangeQueued {
        nonce,
        param,
        value,
        eta: ctx.accounts.timelocked_param.eta,
    });
    Ok(())
}

// ----------------------------------------- execute_global_param ----------------------------------

#[event_cpi]
#[derive(Accounts)]
pub struct ExecuteGlobalParamChange<'info> {
    #[account(mut)]
    pub executor: Signer<'info>,

    #[account(mut, seeds = [BACKSTOP_SEED], bump = backstop.bump)]
    pub backstop: Box<Account<'info, GlobalBackstopReserve>>,

    #[account(
        mut,
        close = executor,
        seeds = [GLOBAL_TIMELOCK_SEED, timelocked_param.nonce.to_le_bytes().as_ref()],
        bump = timelocked_param.bump,
    )]
    pub timelocked_param: Box<Account<'info, crate::state::TimelockedGlobalParam>>,
}

pub fn execute(ctx: Context<ExecuteGlobalParamChange>) -> Result<()> {
    let op = &ctx.accounts.timelocked_param;
    let now = Clock::get()?.unix_timestamp;
    require!(now >= op.eta, FusdError::TimelockNotElapsed);
    validate_global_param(op.param, op.value)?; // defense-in-depth before applying a stored value
    let (param, value, nonce) = (op.param, op.value, op.nonce);

    let prev_value = current_global_param(&ctx.accounts.backstop, param);
    apply_global_param(&mut ctx.accounts.backstop, param, value);
    // `timelocked_param` is closed to `executor` by the `close` constraint.

    emit_cpi!(crate::events::GlobalParamChangeExecuted { nonce, param, prev_value, value });
    Ok(())
}

// ----------------------------------------- cancel_global_param -----------------------------------

#[event_cpi]
#[derive(Accounts)]
pub struct CancelGlobalParamChange<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(seeds = [GOV_GATE_SEED], bump = gov_gate.bump)]
    pub gov_gate: Box<Account<'info, GovernanceGate>>,

    #[account(
        mut,
        close = authority,
        seeds = [GLOBAL_TIMELOCK_SEED, timelocked_param.nonce.to_le_bytes().as_ref()],
        bump = timelocked_param.bump,
    )]
    pub timelocked_param: Box<Account<'info, crate::state::TimelockedGlobalParam>>,
}

pub fn cancel(ctx: Context<CancelGlobalParamChange>) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.gov_gate.inbound_authority,
        FusdError::Unauthorized
    );
    emit_cpi!(crate::events::GlobalParamChangeCanceled { nonce: ctx.accounts.timelocked_param.nonce });
    Ok(())
}
