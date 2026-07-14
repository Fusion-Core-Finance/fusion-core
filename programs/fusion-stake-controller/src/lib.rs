//! fuSOL Native Stake Share Pool — the Allocation Controller.
//!
//! An immutable, user-directed, permissionless-cranked control plane over a pinned,
//! upstream-compatible SPL Stake Pool fork (`vendor/spl-stake-pool`). The controller holds the
//! pool's manager/staker/deposit authorities through PDAs and can sign ONLY the exact CPI
//! allowlist in [`spl_cpi`] — no fee setter, no authority migration, no generic CPI, no
//! arbitrary transfer, no admin. Validator direction is optional and expressed by Fusion
//! collateral owners (one preference per fuSOL-backed position); all backing above a small
//! operational reserve is assigned by fixed directed or equal-neutral rules. Pure math lives
//! in `crates/fusion-stake-math`; external-account byte parsers in `crates/fusion-stake-view`.
//!
//! Fusion debt operations never CPI into this program, and this program never writes Fusion
//! state: a controller failure can delay validator direction, never funds or solvency.

use anchor_lang::prelude::*;

pub mod constants;
pub mod errors;
pub mod events;
pub mod instructions;
pub mod logic;
pub mod maintenance;
pub mod spl_cpi;
pub mod state;

use instructions::*;

declare_id!("Fz3z1yh21PQ59smsPjmjeyK6ngh8KoK6PiPxUgCgspFq");

// On-chain disclosure contact (Neodyme `security.txt`). Embedded in the deployable program only
// (`not(no-entrypoint)`), so CPI/lib consumers don't carry it. Mirrors fusd-core; frozen forever
// once the program is made immutable.
#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "Fusion (fuSOL Allocation Controller)",
    project_url: "https://github.com/Fusion-Core-Finance/fusion-core",
    contacts: "link:https://github.com/Fusion-Core-Finance/fusion-core/security/advisories/new",
    policy: "https://github.com/Fusion-Core-Finance/fusion-core/blob/master/SECURITY.md",
    preferred_languages: "en",
    source_code: "https://github.com/Fusion-Core-Finance/fusion-core",
    auditors: "None yet — pre-audit"
}

#[program]
pub mod fusion_stake_controller {
    use super::*;

    /// One-time genesis: create `ControllerConfig` + `EpochState` and record the predeclared
    /// immutable address set. No authority is stored (the payer only funds rent).
    pub fn initialize_controller(
        ctx: Context<InitializeController>,
        args: InitializeControllerArgs,
    ) -> Result<()> {
        instructions::initialize_controller::handler(ctx, args)
    }

    /// One-time: CPI the stake-pool `Initialize` with the controller PDAs and the fixed fee
    /// set, then seal the controller (`sealed = true`, forever).
    pub fn initialize_pool(ctx: Context<InitializePool>) -> Result<()> {
        instructions::initialize_pool::handler(ctx)
    }

    /// Permissionless: create a `ValidatorRecord` for a vote account (registration, not
    /// admission).
    pub fn register_validator(ctx: Context<RegisterValidator>) -> Result<()> {
        instructions::register_validator::handler(ctx)
    }

    /// Permissionless: deposit SOL through the controller deposit authority; fuSOL mints
    /// immediately at the pool rate. Backing is undirected until a preference is snapshotted.
    pub fn deposit_sol(ctx: Context<DepositSol>, lamports: u64) -> Result<()> {
        instructions::deposit_sol::handler(ctx, lamports)
    }

    /// Stake owner: deposit a fully active stake account delegated to a pool validator (the
    /// account's staker+withdrawer must already be authorized to the deposit-authority PDA).
    pub fn deposit_stake(ctx: Context<DepositStake>) -> Result<()> {
        instructions::deposit_stake::handler(ctx)
    }

    /// Position owner: select or change the validator direction for one fuSOL position
    /// (effective next epoch; at most one change per epoch).
    pub fn set_preference(ctx: Context<SetPreference>) -> Result<()> {
        instructions::set_preference::handler(ctx)
    }

    /// Anyone: refresh a preference's observed ink/nonce after a collateral change
    /// (eligibility delayed to next epoch).
    pub fn sync_preference(ctx: Context<SyncPreference>) -> Result<()> {
        instructions::sync_preference::handler(ctx)
    }

    /// Anyone, during the preference window: count one valid position into its selected
    /// validator's epoch directed weight (once per epoch).
    pub fn snapshot_preference(ctx: Context<SnapshotPreference>) -> Result<()> {
        instructions::snapshot_preference::handler(ctx)
    }

    /// Owner (or anyone once the position is gone): close a preference and refund rent to the
    /// preference owner.
    pub fn close_preference(ctx: Context<ClosePreference>) -> Result<()> {
        instructions::close_preference::handler(ctx)
    }

    /// Anyone: IDLE → RECONCILE when the cluster epoch advances past the controller epoch.
    pub fn start_epoch(ctx: Context<StartEpoch>) -> Result<()> {
        instructions::start_epoch::handler(ctx)
    }

    /// Anyone: reconcile the next bounded validator-list slice (merge completed transients).
    pub fn reconcile_batch<'info>(
        ctx: Context<'_, '_, 'info, 'info, ReconcileBatch<'info>>,
    ) -> Result<()> {
        instructions::reconcile_batch::handler(ctx)
    }

    /// Anyone: finalize canonical pool totals, snapshot NAV/supply, open the preference
    /// window.
    pub fn finalize_pool(ctx: Context<FinalizePool>) -> Result<()> {
        instructions::finalize_pool::handler(ctx)
    }

    /// Anyone, after the deadline: freeze direction totals and enter PLAN-DIRECTED.
    pub fn close_preference_window(ctx: Context<ClosePreferenceWindow>) -> Result<()> {
        instructions::close_preference_window::handler(ctx)
    }

    /// Anyone: refresh eligibility + directed targets/caps/capacity for the next validator
    /// slice.
    pub fn plan_directed_batch<'info>(
        ctx: Context<'_, '_, 'info, 'info, PlanDirectedBatch<'info>>,
    ) -> Result<()> {
        instructions::plan_directed_batch::handler(ctx)
    }

    /// Anyone: fold one bounded slice of the current deterministic neutral capacity round.
    pub fn plan_neutral_batch<'info>(
        ctx: Context<'_, '_, 'info, 'info, PlanNeutralBatch<'info>>,
    ) -> Result<()> {
        instructions::plan_neutral_batch::handler(ctx)
    }

    /// Anyone: prove target conservation, record capacity shortfall, commit final targets.
    pub fn finalize_plan(ctx: Context<FinalizePlan>) -> Result<()> {
        instructions::finalize_plan::handler(ctx)
    }

    /// Anyone: execute the one deterministic valid rebalance action within churn limits.
    pub fn execute_next_action(ctx: Context<ExecuteNextAction>) -> Result<()> {
        instructions::execute_next_action::handler(ctx)
    }

    /// Anyone: REBALANCE → IDLE once targets are exhausted or the churn budget is reached.
    pub fn finish_epoch(ctx: Context<FinishEpoch>) -> Result<()> {
        instructions::finish_epoch::handler(ctx)
    }
}
