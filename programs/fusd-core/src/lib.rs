//! fUSD core — the CDP engine.
//!
//! A trustless, permissionless, Solana-native overcollateralized CDP stablecoin
//! (Liquity/Maker-inspired), governed by MetaDAO futarchy within hard bounds.
//! Shared math/oracle logic lives in `crates/fusd-math` and `crates/fusd-oracle`.
//! See `docs/fusion-docs.md` for the full technical reference.
//!
//! This program implements the core CDP flow (open / deposit / withdraw / borrow /
//! repay), per-position interest accrual (`refresh_market`), permissionless liquidation
//! plus the Reactor Pool and insurance buffer, the rate-bucket redemption bitmap, the
//! oracle stack, and the bounded GovernanceGate. Borrow/withdraw read a cached
//! `Market.spot`; until the oracle crank populates it, the dev-only `dev_set_price`
//! (feature `dev-oracle`) does, so mainnet builds cannot borrow until the oracle is wired.

use anchor_lang::prelude::*;

pub mod accrual;
pub mod bucket;
pub mod cdp;
pub mod clmm;
pub mod constants;
pub mod errors;
pub mod stake_pool;
pub mod events;
pub mod instructions;
pub mod reconcile;
pub mod redist;
pub mod reactor;
pub mod state;
pub mod supply_transition;

// Certora/CVLR verification harness — compiled ONLY under `--features certora` (the Certora cloud
// build), NEVER in the production `.so` (scripts/check-no-certora.sh enforces it).
#[cfg(feature = "certora")]
mod certora;

use instructions::*;

declare_id!("FuSiontgYvCc2N2Cinvo5gxSuxt2UfGxKMcbzkB67kud");

// On-chain disclosure contact (Neodyme `security.txt`). Embedded in the deployable program only
// (`not(no-entrypoint)`), so CPI/lib consumers don't carry it. The disclosure CHANNEL is GitHub's
// private vulnerability reporting + the policy in `SECURITY.md`; no email/key is invented here.
// Branding: Fusion = protocol, Fusion Dollar (FUSD) = stablecoin, FUSION = ownership token. The
// `security.txt` is frozen forever once the program is made immutable.
#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Fusion (FUSD)",
    project_url: "https://github.com/Fusion-Core-Finance/fusion-core",
    contacts: "link:https://github.com/Fusion-Core-Finance/fusion-core/security/advisories/new",
    policy: "https://github.com/Fusion-Core-Finance/fusion-core/blob/master/SECURITY.md",
    preferred_languages: "en",
    source_code: "https://github.com/Fusion-Core-Finance/fusion-core",
    auditors: "None yet — pre-audit"
}

#[program]
pub mod fusd_core {
    use super::*;

    /// One-time: initialize `ProtocolConfig` and the fUSD mint.
    pub fn init_protocol(ctx: Context<InitProtocol>, args: InitProtocolArgs) -> Result<()> {
        instructions::init_protocol::handler(ctx, args)
    }

    /// Governance: onboard a collateral as an isolated market + escrow vault.
    pub fn init_market(ctx: Context<InitMarket>, args: InitMarketArgs) -> Result<()> {
        instructions::init_market::handler(ctx, args)
    }

    /// Governance: bind a market's oracle feeds + validation thresholds and create its
    /// DEX-TWAP observation ring.
    pub fn init_market_oracle(
        ctx: Context<InitMarketOracle>,
        args: InitMarketOracleArgs,
    ) -> Result<()> {
        instructions::init_market_oracle::handler(ctx, args)
    }

    /// Permissionless: advance a market's interest accumulator.
    pub fn refresh_market(ctx: Context<RefreshMarket>) -> Result<()> {
        instructions::refresh_market::handler(ctx)
    }

    /// Permissionless: sample a configured Orca/Raydium CLMM pool's spot price into the market's
    /// DEX-TWAP observation ring (the manipulation-resistance corridor).
    pub fn sample_twap(ctx: Context<SampleTwap>) -> Result<()> {
        instructions::sample_twap::handler(ctx)
    }

    /// Permissionless: aggregate Pyth + Switchboard + the DEX TWAP into the market's cached
    /// collateral price (`spot`) and mint-freeze mode. The borrow blocker until run.
    pub fn update_price(ctx: Context<UpdatePrice>) -> Result<()> {
        instructions::update_price::handler(ctx)
    }

    /// Governance: update the bounded-updatable oracle PROGRAM IDs (Pyth receiver /
    /// Switchboard on-demand) so the Pyth core migration (~2026-07-31) needs no redeploy.
    pub fn set_oracle_program_ids(
        ctx: Context<SetOracleProgramIds>,
        new_pyth_receiver: Option<Pubkey>,
        new_pyth_receiver_alt: Option<Pubkey>,
        new_switchboard: Option<Pubkey>,
    ) -> Result<()> {
        instructions::oracle_admin::set_program_ids(
            ctx,
            new_pyth_receiver,
            new_pyth_receiver_alt,
            new_switchboard,
        )
    }

    /// Governance: rebind a market's oracle feed SOURCES (Pyth feed id / Switchboard feed
    /// account / DEX-TWAP pools) for a feed migration.
    pub fn rebind_market_oracle_feeds(
        ctx: Context<RebindMarketOracleFeeds>,
        args: RebindOracleFeedsArgs,
    ) -> Result<()> {
        instructions::oracle_admin::rebind_feeds(ctx, args)
    }

    /// Governance: create the bounded `GovernanceGate` (the param-tuning authority + the
    /// fUSD-owned timelock). Gated on `config.gov_authority`.
    pub fn init_governance_gate(
        ctx: Context<InitGovernanceGate>,
        inbound_authority: Pubkey,
        timelock_secs: i64,
    ) -> Result<()> {
        instructions::governance::init_gate(ctx, inbound_authority, timelock_secs)
    }

    /// Governance: STEP 1 of the two-step inbound-authority handoff — the current authority
    /// PROPOSES a successor (e.g. launch multisig → MetaDAO Squads vault). The live authority does
    /// not change until the successor calls `accept_inbound_authority`. Pass `Pubkey::default()`
    /// to cancel a pending handoff. Gated on the CURRENT inbound authority.
    pub fn migrate_inbound_authority(
        ctx: Context<MigrateInboundAuthority>,
        new_authority: Pubkey,
    ) -> Result<()> {
        instructions::governance::migrate_authority(ctx, new_authority)
    }

    /// Governance: STEP 2 — the proposed successor signs to ACCEPT the inbound authority. Only now
    /// does the live `inbound_authority` move, so a typo'd / unheld proposal can never brick param
    /// governance.
    pub fn accept_inbound_authority(ctx: Context<AcceptInboundAuthority>) -> Result<()> {
        instructions::governance::accept_authority(ctx)
    }

    /// Governance: QUEUE a clamped market-parameter change behind the timelock. Gated on the
    /// gate's inbound authority (the MetaDAO → Squads → fUSD CPI target).
    pub fn queue_param_change(
        ctx: Context<QueueParamChange>,
        param: MarketParam,
        value: u64,
    ) -> Result<()> {
        instructions::governance::queue(ctx, param, value)
    }

    /// Permissionless: EXECUTE a queued change once its timelock has elapsed.
    pub fn execute_param_change(ctx: Context<ExecuteParamChange>) -> Result<()> {
        instructions::governance::execute(ctx)
    }

    /// Governance: CANCEL a queued change before it executes. Gated on the inbound authority.
    pub fn cancel_param_change(ctx: Context<CancelParamChange>) -> Result<()> {
        instructions::governance::cancel(ctx)
    }

    // --- Global Backstop Reserve (bounded shared second-loss capital) ---

    /// Governance: one-time create the Global Backstop Reserve + its fUSD vault. Gated on
    /// `config.gov_authority`. Ships inert (every param 0/off).
    pub fn init_global_backstop(ctx: Context<InitGlobalBackstop>) -> Result<()> {
        instructions::global_backstop::init(ctx)
    }

    // --- Debt-ceiling auto-line (Maker DC-IAM analog; opt-in, default-absent) ---

    /// Governance: create a market's debt-ceiling auto-line (`line`/`gap`/`ttl`) and apply its
    /// initial ceiling. Gated on `config.gov_authority`.
    pub fn init_debt_ceiling_line(ctx: Context<InitDebtCeilingLine>, line: u64, gap: u64, ttl: i64) -> Result<()> {
        instructions::debt_ceiling_line::init(ctx, line, gap, ttl)
    }

    /// Governance: update the auto-line's `line`/`gap`/`ttl` and apply the new ceiling immediately.
    pub fn set_debt_ceiling_line(ctx: Context<SetDebtCeilingLine>, line: u64, gap: u64, ttl: i64) -> Result<()> {
        instructions::debt_ceiling_line::set(ctx, line, gap, ttl)
    }

    /// Permissionless: bump the market's `debt_ceiling` toward `min(line, debt + gap)`, throttled to
    /// once per `ttl`. Never exceeds the gov-set `line`.
    pub fn bump_debt_ceiling(ctx: Context<BumpDebtCeiling>) -> Result<()> {
        instructions::debt_ceiling_line::bump(ctx)
    }

    // --- Supply reconciliation (proof-of-reserves; auditability, not a solvency gate) ---

    /// Governance: one-time create the global supply-reconciliation singleton.
    pub fn init_supply_reconciliation(ctx: Context<InitSupplyReconciliation>) -> Result<()> {
        instructions::supply_reconciliation::init(ctx)
    }

    /// Permissionless: re-derive `Σ_market (agg − unminted + bad)` over the submitted markets, compare
    /// to the live mint supply, and stamp the residual. Auditability only; gates nothing.
    pub fn reconcile_supply<'info>(
        ctx: Context<'_, '_, 'info, 'info, ReconcileSupply<'info>>,
    ) -> Result<()> {
        instructions::supply_reconciliation::reconcile(ctx)
    }

    /// Permissionless: top up the global backstop reserve with protocol-strengthening fUSD.
    pub fn fund_backstop(ctx: Context<FundBackstop>, amount: u64) -> Result<()> {
        instructions::global_backstop::fund(ctx, amount)
    }

    /// Governance: withdraw ABOVE-CAP excess from the reserve to a recipient. Gated on the gate's
    /// inbound authority; never below `reserve_cap`.
    pub fn withdraw_backstop_excess(ctx: Context<WithdrawBackstopExcess>, amount: u64) -> Result<()> {
        instructions::global_backstop::withdraw_excess(ctx, amount)
    }

    /// Governance: QUEUE a clamped GLOBAL backstop-param change behind the timelock.
    pub fn queue_global_param_change(
        ctx: Context<QueueGlobalParamChange>,
        param: GlobalParam,
        value: u64,
    ) -> Result<()> {
        instructions::global_backstop::queue(ctx, param, value)
    }

    /// Permissionless: EXECUTE a queued global-param change once its timelock has elapsed.
    pub fn execute_global_param_change(ctx: Context<ExecuteGlobalParamChange>) -> Result<()> {
        instructions::global_backstop::execute(ctx)
    }

    /// Governance: CANCEL a queued global-param change before it executes.
    pub fn cancel_global_param_change(ctx: Context<CancelGlobalParamChange>) -> Result<()> {
        instructions::global_backstop::cancel(ctx)
    }

    /// Guardian (independent of futarchy): pause NEW borrowing on a market for `pause_secs`
    /// (clamped to `GUARDIAN_MAX_PAUSE_SECS`; auto-lifts; `0` lifts early). Monotonic de-risk only —
    /// never touches existing positions, funds, repay, liquidation, or redemption.
    pub fn guardian_derisk(ctx: Context<GuardianDerisk>, pause_secs: i64) -> Result<()> {
        instructions::guardian_derisk::handler(ctx, pause_secs)
    }

    /// Governance: rotate/revoke the de-risk guardian (gated on `gov_authority`; immediate, so a
    /// compromised guardian key can be revoked fast). `Pubkey::default()` disables the guardian.
    pub fn set_guardian(ctx: Context<SetGuardian>, new_guardian: Pubkey) -> Result<()> {
        instructions::set_guardian::handler(ctx, new_guardian)
    }

    /// Governance: STEP 1 of the two-step `gov_authority` (bootstrap/admin) handoff — the current
    /// admin PROPOSES a successor. The live authority does not change until the successor calls
    /// `accept_gov_authority`. Pass `Pubkey::default()` to cancel a pending handoff.
    pub fn migrate_gov_authority(
        ctx: Context<MigrateGovAuthority>,
        new_authority: Pubkey,
    ) -> Result<()> {
        instructions::migrate_gov_authority::migrate(ctx, new_authority)
    }

    /// Governance: STEP 2 — the proposed successor signs to ACCEPT the admin authority. Only now
    /// does the live `gov_authority` move, so a typo'd / unheld proposal can never strand the
    /// admin role (market onboarding, guardian rotation).
    pub fn accept_gov_authority(ctx: Context<AcceptGovAuthority>) -> Result<()> {
        instructions::migrate_gov_authority::accept(ctx)
    }

    /// Governance: create the fUSD mint's Metaplex token-metadata account (name/symbol/uri) so
    /// wallets can display the token. Display-only admin lane — cannot mint/move/freeze funds;
    /// the mint-authority PDA signs the CPI because Metaplex requires the mint authority. One-time
    /// in effect (Metaplex rejects an existing metadata account); gov stays the update authority.
    pub fn create_fusd_metadata(
        ctx: Context<CreateFusdMetadata>,
        name: String,
        symbol: String,
        uri: String,
    ) -> Result<()> {
        instructions::create_fusd_metadata::handler(ctx, name, symbol, uri)
    }

    /// Open an (empty) CDP for the signer in a market (posts the SOL liquidation bond).
    pub fn open_position(ctx: Context<OpenPosition>, args: OpenPositionArgs) -> Result<()> {
        instructions::open_position::handler(ctx, args)
    }

    /// Close an empty CDP and reclaim its rent + liquidation bond.
    pub fn close_position(ctx: Context<ClosePosition>) -> Result<()> {
        instructions::close_position::handler(ctx)
    }

    /// Claim a position's liquidation collateral surplus (the bonus-collar remainder).
    pub fn claim_coll_surplus(ctx: Context<ClaimCollSurplus>) -> Result<()> {
        instructions::claim_coll_surplus::handler(ctx)
    }

    /// Deposit collateral into the caller's position.
    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        instructions::deposit::handler(ctx, amount)
    }

    /// Withdraw collateral, keeping the position at/above MCR if it has debt.
    pub fn withdraw(ctx: Context<Withdraw>, amount: u64) -> Result<()> {
        instructions::withdraw::handler(ctx, amount)
    }

    /// Mint fUSD against the position up to MCR and the market debt ceiling.
    pub fn borrow(ctx: Context<Borrow>, amount: u64) -> Result<()> {
        instructions::borrow::handler(ctx, amount)
    }

    /// Burn fUSD to repay the position's debt (capped at current debt).
    pub fn repay(ctx: Context<Repay>, amount: u64) -> Result<()> {
        instructions::repay::handler(ctx, amount)
    }

    /// Change the position's borrower-set rate, moving it to the matching redemption bucket.
    pub fn adjust_rate(ctx: Context<AdjustRate>, new_rate_bps: u16) -> Result<()> {
        instructions::adjust_rate::handler(ctx, new_rate_bps)
    }

    /// Permissionless: redeem fUSD for face-value collateral, draining the lowest non-empty rate
    /// bucket first; candidate positions are passed as `remaining_accounts`.
    pub fn redeem<'info>(
        ctx: Context<'_, '_, 'info, 'info, Redeem<'info>>,
        amount: u64,
    ) -> Result<()> {
        instructions::redeem::handler(ctx, amount)
    }

    /// Permissionless: terminally wind a market down when it is failing (TCR < SCR on a fresh price,
    /// or sustained oracle failure). Closes borrow + ordered redeem; opens `urgent_redeem`.
    pub fn shutdown(ctx: Context<Shutdown>) -> Result<()> {
        instructions::shutdown::handler(ctx)
    }

    /// Permissionless: redeem fUSD for face-value collateral from ANY position in a shut-down market
    /// (unordered, 0-fee, last price). The wind-down floor; candidates via `remaining_accounts`.
    pub fn urgent_redeem<'info>(
        ctx: Context<'_, '_, 'info, 'info, UrgentRedeem<'info>>,
        amount: u64,
    ) -> Result<()> {
        instructions::urgent_redeem::handler(ctx, amount)
    }

    /// Governance: create a market's Reactor Pool (vaults + the bounded S grid).
    pub fn init_reactor_pool(ctx: Context<InitReactorPool>) -> Result<()> {
        instructions::init_reactor_pool::handler(ctx)
    }

    /// Governance: create a market's insurance buffer (the third loss-absorption tier).
    pub fn init_insurance_buffer(ctx: Context<InitInsuranceBuffer>) -> Result<()> {
        instructions::init_insurance_buffer::handler(ctx)
    }

    /// Permissionless: fund a market's insurance buffer with fUSD (realized fees / treasury / keeper).
    pub fn fund_buffer(ctx: Context<FundBuffer>, amount: u64) -> Result<()> {
        instructions::fund_buffer::handler(ctx, amount)
    }

    /// Open an (empty) Reactor-Pool deposit for the signer.
    pub fn open_reactor_deposit(ctx: Context<OpenReactorDeposit>) -> Result<()> {
        instructions::open_reactor_deposit::handler(ctx)
    }

    /// Deposit fUSD into a market's Reactor Pool.
    pub fn provide_to_reactor(ctx: Context<ProvideToReactor>, amount: u64) -> Result<()> {
        instructions::provide_to_reactor::handler(ctx, amount)
    }

    /// Withdraw fUSD from a Reactor Pool (capped at the compounded deposit).
    pub fn withdraw_from_reactor(ctx: Context<WithdrawFromReactor>, amount: u64) -> Result<()> {
        instructions::withdraw_from_reactor::handler(ctx, amount)
    }

    /// Claim a depositor's realized seized-collateral gains.
    pub fn claim_reactor_gains(ctx: Context<ClaimReactorGains>) -> Result<()> {
        instructions::claim_reactor_gains::handler(ctx)
    }

    /// Permissionless: liquidate an under-MCR position via the Reactor Pool.
    pub fn liquidate(ctx: Context<Liquidate>) -> Result<()> {
        instructions::liquidate::handler(ctx)
    }

    /// Governance: withdraw accrued redemption-fee surplus collateral.
    pub fn withdraw_surplus(ctx: Context<WithdrawSurplus>, amount: u64) -> Result<()> {
        instructions::withdraw_surplus::handler(ctx, amount)
    }

    /// Governance: recover retained protocol-owned (un-homed) collateral (recap sweep).
    pub fn sweep_protocol_collateral(
        ctx: Context<SweepProtocolCollateral>,
        amount: u64,
    ) -> Result<()> {
        instructions::sweep_protocol_collateral::handler(ctx, amount)
    }

    /// Governance: burn fUSD to retire realized bad debt (recap settlement).
    pub fn settle_bad_debt(ctx: Context<SettleBadDebt>, amount: u64) -> Result<()> {
        instructions::settle_bad_debt::handler(ctx, amount)
    }

    /// DEV/TEST ONLY (feature `dev-oracle`): set a market's cached collateral price.
    #[cfg(feature = "dev-oracle")]
    pub fn dev_set_price(ctx: Context<DevSetPrice>, spot: u128) -> Result<()> {
        instructions::dev_set_price::handler(ctx, spot)
    }
}
