//! The bounded `GovernanceGate` + the fUSD-owned timelock (fusion-docs.md).
//!
//! Param changes are TWO-SPEED: the gate's migratable `inbound_authority` (a launch multisig →
//! the MetaDAO Squads vault PDA) QUEUES a clamped change; after the gate's
//! `timelock_secs` elapse, ANYONE may EXECUTE it. There is no un-timelocked immediate setter —
//! the queue→delay→execute path is the only way a market parameter moves (the planned de-risk
//! guardian is a separate, monotonic, emergency-only path). Squads runs `time_lock = 0`, which is
//! exactly why fUSD supplies its own delay.

use anchor_lang::prelude::*;

use crate::constants::{
    CONFIG_SEED, GOV_GATE_SEED, MAX_BAD_DEBT_PAYDOWN_BPS, MAX_BORROW_FEE_BPS, MAX_CCR_BPS,
    MAX_GOV_TIMELOCK_SECS, MAX_LIQ_BONUS_BPS, MAX_KEEPER_REWARD_BPS, MAX_LIQ_GAS_COMP_BPS,
    MAX_MCR_BPS, MAX_MIN_DEBT,
    MAX_RATE_ADJUST_COOLDOWN_SECS, MAX_REDEMPTION_FEE_BPS, MIN_CCR_BPS, MIN_GOV_TIMELOCK_SECS,
    MIN_MCR_BPS, TIMELOCK_SEED,
};
use crate::errors::FusdError;
use crate::state::{GovernanceGate, Market, ProtocolConfig, TimelockedParam};

// Re-exported so `lib.rs` (`use instructions::*`) sees `MarketParam` for the program signatures.
pub use crate::state::MarketParam;

// ----------------------------------------- shared clamps/apply -----------------------------------

/// Compile-time clamp check for a (param, value) pair. Run at QUEUE (fail fast) and again at
/// EXECUTE (defense-in-depth — the clamp constants can never have moved, but re-checking means a
/// stored op can never apply an out-of-bounds value).
fn validate_param(param: MarketParam, value: u64) -> Result<()> {
    match param {
        MarketParam::Mcr => {
            require!(
                value >= MIN_MCR_BPS as u64 && value <= MAX_MCR_BPS as u64,
                FusdError::ParamOutOfBounds
            );
        }
        MarketParam::DebtCeiling => {}
        MarketParam::RedemptionFee => {
            require!(value <= MAX_REDEMPTION_FEE_BPS as u64, FusdError::ParamOutOfBounds);
        }
        MarketParam::LiqGasComp => {
            require!(value <= MAX_LIQ_GAS_COMP_BPS as u64, FusdError::ParamOutOfBounds);
        }
        // No upper clamp: 0 disables the limiter, larger is more permissive (the loosen-path).
        MarketParam::RateLimitCap => {}
        // 0 disables the band; a non-zero CCR is clamped to a sane range.
        MarketParam::Ccr => {
            require!(
                value == 0
                    || (value >= MIN_CCR_BPS as u64 && value <= MAX_CCR_BPS as u64),
                FusdError::ParamOutOfBounds
            );
        }
        // 0 disables the collar (seize-all); otherwise clamped to the bonus ceiling.
        MarketParam::LiqBonus => {
            require!(value <= MAX_LIQ_BONUS_BPS as u64, FusdError::ParamOutOfBounds);
        }
        // 0 disables the dust floor; otherwise clamped to a sane maximum.
        MarketParam::MinDebt => {
            require!(value <= MAX_MIN_DEBT, FusdError::ParamOutOfBounds);
        }
        // 0 disables the premature-rate-change fee/cooldown; otherwise clamped (the value is seconds).
        MarketParam::RateAdjustCooldown => {
            require!(value <= MAX_RATE_ADJUST_COOLDOWN_SECS as u64, FusdError::ParamOutOfBounds);
        }
        // 0 disables the keeper reward; otherwise clamped to keep the buffer's share dominant.
        MarketParam::KeeperReward => {
            require!(value <= MAX_KEEPER_REWARD_BPS as u64, FusdError::ParamOutOfBounds);
        }
        // 0 disables the upfront borrowing fee; otherwise clamped to `<= MAX_BORROW_FEE_BPS` (C7).
        MarketParam::BorrowFee => {
            require!(value <= MAX_BORROW_FEE_BPS as u64, FusdError::ParamOutOfBounds);
        }
        // 0 disables the auto bad-debt paydown; otherwise clamped to `<= MAX_BAD_DEBT_PAYDOWN_BPS` (C16).
        MarketParam::BadDebtPaydown => {
            require!(value <= MAX_BAD_DEBT_PAYDOWN_BPS as u64, FusdError::ParamOutOfBounds);
        }
    }
    Ok(())
}

/// Relational (cross-parameter) config validation — the klend-style "config integrity gauntlet".
/// Per-field clamps catch out-of-range values; this catches values that are
/// individually in range but JOINTLY lethal for the market. Run at QUEUE (fail fast against the
/// live config) and again at EXECUTE (the load-bearing check: the sibling param may have changed
/// between queue and execute, so re-checking against the LIVE market makes two jointly-conflicting
/// queued ops order-independent — the second execute is rejected; governance re-queues in the safe
/// order). `init_market` asserts the same predicates over its args, so a violating combo can never
/// exist at birth either.
///
/// CONSTITUTIONAL SHAPE: the predicates are pure functions of CONFIG fields only — never of live
/// market conditions (TCR, price, positions). Conditioning config validity on market health would
/// be recovery-mode-like reflexivity. And the relational
/// branch runs ONLY for the three liquidation-economics params, so a hypothetical market whose
/// standing combo predates a bound can never have UNRELATED param changes blocked (no bricking).
pub fn validate_market_config(
    mcr_bps: u64,
    scr_bps: u64,
    liq_bonus_bps: u64,
    liq_gas_comp_bps: u64,
) -> Result<()> {
    // (i) Collar fundability: a liquidation at the MCR boundary must be able to pay the FULL
    // advertised bonus collar — otherwise `seize_collateral`'s `.min(ink)` silently truncates it
    // near MCR and the documented bonus/coll_surplus becomes a dead letter (the klend
    // threshold·(1+max_bonus) <= 100% solvency-product analog). Collar off (0) is exempt.
    // This is a config CONSISTENCY bound (advertised-penalty honesty), not an RP-loss guarantee.
    require!(
        liq_bonus_bps == 0 || 10_000 + liq_bonus_bps <= mcr_bps,
        FusdError::CollarExceedsMcr
    );
    // (ii) RP-solvency product: at the MCR boundary the Reactor Pool must receive at least the
    // debt it burns AFTER the liquidator gas-comp skim. `seizable` is the collateral value (bps of
    // debt) a boundary liquidation can seize: the collar cap when on, all of ink (= MCR) when off.
    let seizable_bps = if liq_bonus_bps == 0 { mcr_bps } else { (10_000 + liq_bonus_bps).min(mcr_bps) };
    require!(
        seizable_bps * 10_000u64.saturating_sub(liq_gas_comp_bps) >= 100_000_000,
        FusdError::ParamCombinationInvalid
    );
    // (iii) Shutdown ordering: MCR >= SCR, else a market whose positions all sit healthily in
    // [MCR, SCR) has TCR < SCR and ANYONE can trigger the irreversible terminal `shutdown`.
    // `>=` not `>`: MCR == SCR is safe (TCR < SCR then implies at least one position is already
    // liquidatable; Liquity v2 WETH ships MCR = SCR = 110%).
    require!(mcr_bps >= scr_bps, FusdError::ParamCombinationInvalid);
    Ok(())
}

/// Read a param's CURRENT value off a market — the mirror of [`apply_param`], used to capture
/// `prev_value` for the forensic Prv/New event trail. Exhaustive and
/// wildcard-free on purpose: adding a `MarketParam` variant without deciding its clamp
/// (`validate_param`), its setter (`apply_param`), AND its reader (here) is a compile error —
/// the klend triple-coverage property.
fn current_param(market: &Market, param: MarketParam) -> u64 {
    match param {
        MarketParam::Mcr => market.mcr_bps as u64,
        MarketParam::DebtCeiling => market.debt_ceiling,
        MarketParam::RedemptionFee => market.redemption_fee_bps as u64,
        MarketParam::LiqGasComp => market.liq_gas_comp_bps as u64,
        MarketParam::RateLimitCap => market.rl_cap,
        MarketParam::Ccr => market.ccr_bps as u64,
        MarketParam::LiqBonus => market.liq_bonus_bps as u64,
        MarketParam::MinDebt => market.min_debt,
        MarketParam::RateAdjustCooldown => market.rate_adjust_cooldown_secs as u64,
        MarketParam::KeeperReward => market.keeper_reward_bps as u64,
        MarketParam::BorrowFee => market.borrow_fee_bps as u64,
        MarketParam::BadDebtPaydown => market.bad_debt_paydown_bps as u64,
    }
}

/// Per-field + relational validation for a (param, value) against a live market. The relational
/// predicates run only when the change touches the liquidation-economics tuple.
fn validate_param_for_market(market: &Market, param: MarketParam, value: u64) -> Result<()> {
    validate_param(param, value)?;
    match param {
        MarketParam::Mcr | MarketParam::LiqBonus | MarketParam::LiqGasComp => {
            // Overlay the candidate value on the live tuple and re-assert joint validity.
            let mut mcr = market.mcr_bps as u64;
            let mut bonus = market.liq_bonus_bps as u64;
            let mut gas = market.liq_gas_comp_bps as u64;
            match param {
                MarketParam::Mcr => mcr = value,
                MarketParam::LiqBonus => bonus = value,
                MarketParam::LiqGasComp => gas = value,
                _ => unreachable!(),
            }
            validate_market_config(mcr, market.scr_bps as u64, bonus, gas)
        }
        _ => Ok(()),
    }
}

/// Apply a validated (param, value) to a market. Caller MUST have run [`validate_param`] first.
fn apply_param(market: &mut Market, param: MarketParam, value: u64) {
    match param {
        MarketParam::Mcr => market.mcr_bps = value as u16,
        MarketParam::DebtCeiling => market.debt_ceiling = value,
        MarketParam::RedemptionFee => market.redemption_fee_bps = value as u16,
        MarketParam::LiqGasComp => market.liq_gas_comp_bps = value as u16,
        MarketParam::RateLimitCap => {
            market.rl_cap = value;
            // Reconcile the bucket to the new cap so `rl_accrued <= rl_cap` always holds: a
            // cap-lower clamps stored pressure down (it then drains at the new rate); disabling
            // (value 0) zeroes it, so a later re-enable starts empty; a raise leaves accrued
            // unchanged (the loosen-path doesn't wipe legitimate pressure → no bypass).
            market.rl_accrued = market.rl_accrued.min(value);
        }
        MarketParam::Ccr => market.ccr_bps = value as u16,
        MarketParam::LiqBonus => market.liq_bonus_bps = value as u16,
        MarketParam::MinDebt => market.min_debt = value,
        MarketParam::RateAdjustCooldown => market.rate_adjust_cooldown_secs = value as i64,
        MarketParam::KeeperReward => market.keeper_reward_bps = value as u16,
        MarketParam::BorrowFee => market.borrow_fee_bps = value as u16,
        MarketParam::BadDebtPaydown => market.bad_debt_paydown_bps = value as u16,
    }
}

// ----------------------------------------- init_governance_gate ----------------------------------

#[derive(Accounts)]
pub struct InitGovernanceGate<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,

    #[account(
        init,
        payer = authority,
        space = GovernanceGate::SPACE,
        seeds = [GOV_GATE_SEED],
        bump,
    )]
    pub gov_gate: Box<Account<'info, GovernanceGate>>,

    pub system_program: Program<'info, System>,
}

/// One-time: create the gate, gated by `config.gov_authority` (the deployer / launch admin
/// bootstraps it). `inbound_authority` is the initial param-tuning authority (migratable later).
pub fn init_gate(
    ctx: Context<InitGovernanceGate>,
    inbound_authority: Pubkey,
    timelock_secs: i64,
) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );
    require!(inbound_authority != Pubkey::default(), FusdError::ParamOutOfBounds);
    require!(
        timelock_secs >= MIN_GOV_TIMELOCK_SECS && timelock_secs <= MAX_GOV_TIMELOCK_SECS,
        FusdError::ParamOutOfBounds
    );
    let g = &mut ctx.accounts.gov_gate;
    g.inbound_authority = inbound_authority;
    g.pending_inbound_authority = Pubkey::default(); // no handoff in flight at genesis
    g.timelock_secs = timelock_secs;
    g.queue_nonce = 0;
    g.bump = ctx.bumps.gov_gate;
    g._reserved = [0u8; 32];
    Ok(())
}

// ----------------------------------------- migrate_inbound_authority -----------------------------
// TWO-STEP handoff (propose → accept). Step 1 (`migrate_authority`): the CURRENT authority records a
// PENDING successor. Step 2 (`accept_authority`): the successor itself signs to take the seat. The
// live `inbound_authority` only moves on the accept, so a propose to a typo'd / unheld key can never
// brick param governance — it just can never be accepted, and the current authority re-proposes.

#[event_cpi]
#[derive(Accounts)]
pub struct MigrateInboundAuthority<'info> {
    /// MUST equal the CURRENT `gov_gate.inbound_authority` — governance proposes its own successor
    /// (e.g. launch multisig → MetaDAO Squads vault).
    pub authority: Signer<'info>,

    #[account(mut, seeds = [GOV_GATE_SEED], bump = gov_gate.bump)]
    pub gov_gate: Box<Account<'info, GovernanceGate>>,
}

/// Step 1 — propose. `new_authority == Pubkey::default()` clears a pending handoff (cancel); any
/// other value records a pending successor that does NOT take effect until it accepts.
pub fn migrate_authority(ctx: Context<MigrateInboundAuthority>, new_authority: Pubkey) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.gov_gate.inbound_authority,
        FusdError::Unauthorized
    );
    ctx.accounts.gov_gate.pending_inbound_authority = new_authority;

    emit_cpi!(crate::events::InboundAuthorityProposed {
        current: ctx.accounts.gov_gate.inbound_authority,
        pending: new_authority,
    });
    Ok(())
}

// ----------------------------------------- accept_inbound_authority ------------------------------

#[event_cpi]
#[derive(Accounts)]
pub struct AcceptInboundAuthority<'info> {
    /// MUST equal `gov_gate.pending_inbound_authority` — the proposed successor proves control by
    /// signing. This is what makes the handoff two-step (the incoming key can't be a typo).
    pub new_authority: Signer<'info>,

    #[account(mut, seeds = [GOV_GATE_SEED], bump = gov_gate.bump)]
    pub gov_gate: Box<Account<'info, GovernanceGate>>,
}

/// Step 2 — accept. The pending successor signs; only then does the live authority move.
pub fn accept_authority(ctx: Context<AcceptInboundAuthority>) -> Result<()> {
    let g = &mut ctx.accounts.gov_gate;
    require!(
        g.pending_inbound_authority != Pubkey::default(),
        FusdError::NoPendingAuthority
    );
    require_keys_eq!(
        ctx.accounts.new_authority.key(),
        g.pending_inbound_authority,
        FusdError::Unauthorized
    );
    let previous = g.inbound_authority;
    g.inbound_authority = g.pending_inbound_authority;
    g.pending_inbound_authority = Pubkey::default();

    emit_cpi!(crate::events::InboundAuthorityMigrated {
        previous,
        new_authority: g.inbound_authority,
    });
    Ok(())
}

// ----------------------------------------- queue_param_change ------------------------------------

#[event_cpi]
#[derive(Accounts)]
pub struct QueueParamChange<'info> {
    /// MUST equal `gov_gate.inbound_authority`. Pays the queued op's rent.
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(mut, seeds = [GOV_GATE_SEED], bump = gov_gate.bump)]
    pub gov_gate: Box<Account<'info, GovernanceGate>>,

    /// The target market (real, program-owned `Market`); recorded into the op and re-checked at execute.
    pub market: Box<Account<'info, Market>>,

    #[account(
        init,
        payer = authority,
        space = TimelockedParam::SPACE,
        seeds = [TIMELOCK_SEED, gov_gate.queue_nonce.to_le_bytes().as_ref()],
        bump,
    )]
    pub timelocked_param: Box<Account<'info, TimelockedParam>>,

    pub system_program: Program<'info, System>,
}

pub fn queue(ctx: Context<QueueParamChange>, param: MarketParam, value: u64) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.gov_gate.inbound_authority,
        FusdError::Unauthorized
    );
    // Fail fast — never queue an out-of-bounds value OR a combination jointly invalid against the
    // CURRENT config (the execute-time re-check below remains the load-bearing one).
    validate_param_for_market(&ctx.accounts.market, param, value)?;

    let now = Clock::get()?.unix_timestamp;
    let nonce = ctx.accounts.gov_gate.queue_nonce;

    let op = &mut ctx.accounts.timelocked_param;
    op.nonce = nonce;
    op.eta = now.saturating_add(ctx.accounts.gov_gate.timelock_secs);
    op.market = ctx.accounts.market.key();
    op.param = param;
    op.value = value;
    op.bump = ctx.bumps.timelocked_param;
    op._reserved = [0u8; 16];

    ctx.accounts.gov_gate.queue_nonce =
        ctx.accounts.gov_gate.queue_nonce.checked_add(1).ok_or(FusdError::MathOverflow)?;

    emit_cpi!(crate::events::ParamChangeQueued {
        market: ctx.accounts.market.key(),
        nonce,
        param,
        value,
        eta: ctx.accounts.timelocked_param.eta,
    });
    Ok(())
}

// ----------------------------------------- execute_param_change ----------------------------------

#[event_cpi]
#[derive(Accounts)]
pub struct ExecuteParamChange<'info> {
    /// Anyone — execution is permissionless once the timelock elapses. Receives the op's rent.
    #[account(mut)]
    pub executor: Signer<'info>,

    // No `gov_gate`: execution is permissionless and the timelock is baked into `op.eta` at queue
    // time, so `execute` never reads the gate (matching the `ExecuteGlobalParamChange` twin).
    #[account(mut)]
    pub market: Box<Account<'info, Market>>,

    #[account(
        mut,
        close = executor,
        seeds = [TIMELOCK_SEED, timelocked_param.nonce.to_le_bytes().as_ref()],
        bump = timelocked_param.bump,
    )]
    pub timelocked_param: Box<Account<'info, TimelockedParam>>,
}

pub fn execute(ctx: Context<ExecuteParamChange>) -> Result<()> {
    let op = &ctx.accounts.timelocked_param;
    require_keys_eq!(ctx.accounts.market.key(), op.market, FusdError::TimelockMarketMismatch);
    let now = Clock::get()?.unix_timestamp;
    require!(now >= op.eta, FusdError::TimelockNotElapsed);
    // Re-validate before applying a stored value — per-field (the clamps can't have moved, but a
    // stored op must never apply out-of-bounds) AND relational against the LIVE market (the
    // sibling param may have changed since queue; rejecting here makes jointly-conflicting queued
    // ops order-independent — governance cancels and re-queues in the safe order).
    validate_param_for_market(&ctx.accounts.market, op.param, op.value)?;
    let (param, value, nonce) = (op.param, op.value, op.nonce);

    // An MCR RAISE instantly expands the liquidatable set over live positions — the retroactive-
    // worsening vector the protocol forbids — so arm the liquidation grace window before applying
    // (machine-enforces the "user exit window" even at timelock 0). Checked
    // BEFORE apply_param so the comparison is against the pre-change mcr_bps. Monotone `max`:
    // never shortens an active oracle-resume grace. Raises only — a LOWERING is pure de-risk for
    // borrowers (positions already below the unchanged-or-higher old threshold stay liquidatable).
    // `liquidate` is the ONLY reader of `liq_grace_until`; redeem/shutdown/urgent_redeem never
    // gate on it. Known, accepted trade-off (documented on `MarketParam::Mcr`): clamp-legal raise
    // cycling can re-arm the window repeatedly — bounded by the grace-free shutdown/urgent_redeem
    // backstop, and monitored via the event below.
    if param == MarketParam::Mcr && (value as u16) > ctx.accounts.market.mcr_bps {
        let now_slot = Clock::get()?.slot;
        let armed_until = ctx
            .accounts
            .market
            .liq_grace_until
            .max(now_slot.saturating_add(crate::constants::MCR_RAISE_GRACE_SLOTS));
        ctx.accounts.market.liq_grace_until = armed_until;
        emit_cpi!(crate::events::McrRaiseGraceArmed {
            collateral_mint: ctx.accounts.market.collateral_mint,
            old_mcr_bps: ctx.accounts.market.mcr_bps,
            new_mcr_bps: value as u16,
            grace_until_slot: armed_until,
        });
    }

    // Capture the pre-change value for the forensic Prv/New trail, immediately before apply.
    let prev_value = current_param(&ctx.accounts.market, param);
    apply_param(&mut ctx.accounts.market, param, value);
    // `timelocked_param` is closed to `executor` by the `close` constraint.

    emit_cpi!(crate::events::ParamChangeExecuted {
        market: ctx.accounts.market.key(),
        nonce,
        param,
        prev_value,
        value,
    });
    Ok(())
}

// ----------------------------------------- cancel_param_change -----------------------------------

#[event_cpi]
#[derive(Accounts)]
pub struct CancelParamChange<'info> {
    /// MUST equal `gov_gate.inbound_authority`. Reclaims the op's rent.
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(seeds = [GOV_GATE_SEED], bump = gov_gate.bump)]
    pub gov_gate: Box<Account<'info, GovernanceGate>>,

    #[account(
        mut,
        close = authority,
        seeds = [TIMELOCK_SEED, timelocked_param.nonce.to_le_bytes().as_ref()],
        bump = timelocked_param.bump,
    )]
    pub timelocked_param: Box<Account<'info, TimelockedParam>>,
}

/// Governance withdraws a queued change before it executes (closes the op, applies nothing).
pub fn cancel(ctx: Context<CancelParamChange>) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.gov_gate.inbound_authority,
        FusdError::Unauthorized
    );
    emit_cpi!(crate::events::ParamChangeCanceled { nonce: ctx.accounts.timelocked_param.nonce });
    Ok(())
}

// ----------------------------------------- tests -------------------------------------------------

#[cfg(test)]
mod tests {
    use super::validate_market_config;

    fn ok(mcr: u64, scr: u64, bonus: u64, gas: u64) -> bool {
        validate_market_config(mcr, scr, bonus, gas).is_ok()
    }

    #[test]
    fn collar_fundability_bound() {
        // bonus == 0 ⇒ exempt regardless of MCR (collar off / seize-all).
        assert!(ok(11_000, 10_500, 0, 0));
        // Exact boundary: 100% + bonus == MCR is fundable.
        assert!(ok(11_000, 10_500, 1_000, 0));
        // One bp short truncates the collar near MCR ⇒ rejected.
        assert!(!ok(10_999, 10_500, 1_000, 0));
        assert!(!ok(10_000, 10_000, 1, 0));
        // Comfortable config passes.
        assert!(ok(15_000, 11_000, 1_000, 0));
    }

    #[test]
    fn reactor_solvency_product_bound() {
        // Equality boundary: MCR 100%, no bonus, no gas comp ⇒ product exactly 1e8.
        assert!(ok(10_000, 10_000, 0, 0));
        // MCR 100% with ANY gas comp guarantees an RP loss at the boundary ⇒ rejected.
        assert!(!ok(10_000, 10_000, 0, 1));
        // The klend-analog killer combo: small bonus + big gas comp (both individually in-clamp).
        assert!(!ok(15_000, 11_000, 100, 1_000)); // 10_100 * 9_000 = 90.9M < 1e8
        assert!(!ok(15_000, 11_000, 1_000, 1_000)); // 11_000 * 9_000 = 99M < 1e8
        // Current production defaults pass with margin.
        assert!(ok(15_000, 11_000, 1_000, 50)); // 11_000 * 9_950 = 109.45M
        // Collar off: seize-all means MCR itself is the seizable value.
        assert!(ok(15_000, 11_000, 0, 1_000)); // 15_000 * 9_000 = 135M
    }

    #[test]
    fn shutdown_ordering_bound() {
        // MCR == SCR is safe (TCR < SCR then implies a liquidatable position exists;
        // Liquity v2 WETH ships MCR = SCR).
        assert!(ok(11_000, 11_000, 0, 0));
        // MCR < SCR: healthy positions in [MCR, SCR) would make the market terminally
        // shutdown-able while operating as configured ⇒ rejected.
        assert!(!ok(10_999, 11_000, 0, 0));
        assert!(!ok(10_500, 11_000, 0, 0));
    }
}
