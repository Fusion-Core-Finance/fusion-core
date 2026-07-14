//! The fuSOL epoch crank state machine END-TO-END against the REAL mainnet-dumped SPL Stake
//! Pool processor (loaded at the fork id) + the real Allocation Controller + real fusd-core.
//!
//! Scenario map (one `#[test]` each):
//! 1. `empty_set_full_cycle_and_phase_order` — the whole IDLE → RECONCILE → FINALIZE →
//!    PREFERENCES → PLAN-DIRECTED → PLAN-NEUTRAL → PLAN-FINALIZE → REBALANCE → IDLE cycle over
//!    an EMPTY validator set, with the phase asserted after every step, every later crank
//!    probed at every phase (specific error, `EpochState` byte-identical), the NAV snapshot /
//!    reserve-target math checked against the reserve formula, and the zero-Active plan
//!    recording the full productive amount as capacity shortfall.
//! 2. `admission_and_increase_via_directed_preference` — validator admission driven by REAL
//!    directed shares and executed as a REAL `AddValidatorToPool` CPI, then a REAL
//!    `IncreaseValidatorStake` (transient creation), the next-epoch transient MERGE back into
//!    the validator stake, Candidate → Active promotion, and a per-validator-cap-clipped
//!    increase. Directed-shares route chosen: the REAL preference path over a MINIMAL
//!    fusd-core market (`init_protocol` + `init_market` on the fuSOL mint — no oracle, no
//!    price, no reactor: `open_position` + `deposit` need none of them). This is the lightest
//!    SOUND path — the alternative (writing `ValidatorRecord.directed_shares` directly) is
//!    unsound because the records are controller-owned.
//! 3. `nav_growth_and_negative_nav_reach_the_fusol_oracle` — synthesized reward lamports on
//!    the reserve raise the finalized NAV (rate checked by cross-multiplication), a
//!    canonical-primary fusd-core market oracle bound to the REAL pool picks the new rate up
//!    through `update_price`, then a synthesized DECREASE emits `NegativeNavObserved` and the
//!    oracle commits the lower NAV on both price legs.
//! 4. `crank_reward_budget_no_op_zero_and_vault_clamp` — rewards paid from the (deposit-fee
//!    funded) maintenance vault, ZERO on no-op work (an out-of-band permissionless upstream
//!    update forfeits the finalize reward but never blocks the phase), the payout clamped to
//!    the vault balance, an empty vault leaving every crank executable unpaid, the
//!    vault-as-recipient grief closed, and `epoch_payout_budget_used` == the sum of actual
//!    payouts <= `CRANK_EPOCH_PAYOUT_BUDGET` each epoch.
//!
//! ## litesvm epoch-machinery ground truth (what works vs what is synthesized)
//!
//! litesvm 0.7.1 has the native stake/vote/system builtins but NO epoch machinery: epoch warp
//! is manual (`warp_epochs`), no rewards are ever paid, and `StakeHistory` stays `Default`
//! (empty). Empirically that makes stake activation INSTANT-after-one-epoch-boundary: the
//! stake program's rate-limited warmup walk only engages when the history has entries, so a
//! delegation created in epoch N reads as FULLY EFFECTIVE at epoch N+1. Consequences proven
//! by scenario 2:
//! - `AddValidatorToPool` (real CPI, funds `1 SOL + rent` from the reserve — litesvm runs
//!   `FeatureSet::all_enabled()`, so `stake_raise_minimum_delegation_to_1_sol` is ACTIVE and
//!   the dumped pool's minimum delegation is 1 SOL here, not the 0.001 SOL mainnet value);
//! - `IncreaseValidatorStake` (real transient stake creation from the reserve);
//! - next-epoch `UpdateValidatorListBalance` MERGING the fully-active transient back into the
//!   validator stake account (empty history never blocks the merge in this lane).
//! No stake-activation residue remained — every §17.2-relevant transient mechanic in these
//! scenarios runs on the REAL path.
//!
//! Synthesized state (each deliberate, none replaceable by a real path in litesvm):
//! - VOTE HEALTH (`make_vote_healthy`): a fresh real vote account has an empty tower and no
//!   prior-epoch credits, so it observes `liveness_ok == false`. The account is rewritten
//!   through the REFERENCE serializer (`solana_sdk::vote::state::VoteState`) with positive
//!   epoch-credit growth for epochs 0..32 and a far-future `last_timestamp.slot` (a future
//!   freshness slot saturates to "fresh" in the observation policy). Landing real votes would
//!   require a leader schedule litesvm does not run.
//! - REWARD/LOSS LAMPORTS (scenario 3): direct lamport bumps on the reserve stake account —
//!   litesvm pays no inflation rewards; the pool's own `UpdateStakePoolBalance` then
//!   recomputes totals from the real accounts.
//! - VAULT BALANCE WRITE-DOWN (scenario 4): the maintenance-vault token amount is set to a
//!   tiny value to reach the `min(task, budget, vault)` clamp — paying the vault down for
//!   real would take ~1000 crank tasks.
//!
//! Requires the dev-oracle `.so` set + the dumped fixture:
//! `anchor build -- --features dev-oracle` and `bash scripts/fetch-spl-stake-pool.sh`.

use fusd_integration_tests::*;
use fusion_stake_controller::constants::{
    ACTIVE_VALIDATOR_CAP_BPS, CANDIDATE_CAP_BPS, CRANK_EPOCH_PAYOUT_BUDGET,
    CRANK_REWARD_FINALIZE_POOL, CRANK_REWARD_PLAN_BATCH, CRANK_REWARD_REBALANCE_ACTION,
    CRANK_REWARD_RECONCILE_BATCH, FEE_BPS_DENOMINATOR, GLOBAL_CHURN_CAP_BPS, HYSTERESIS_BPS,
    HYSTERESIS_MIN_LAMPORTS, MIN_ACTIVATION_TARGET_LAMPORTS, RESERVE_MINIMUM_LAMPORTS,
    RESERVE_TARGET_BPS, SOL_DEPOSIT_FEE_BPS, STAKE_PROGRAM_ID, VALIDATOR_LIST_INDEX_UNSET,
    VALIDATOR_MOVE_CAP_BPS,
};
use fusion_stake_controller::events::{
    EpochPhaseChanged, MaintenanceRewardPaid, NegativeNavObserved, PlanFinalized,
    PreferenceUpdated, RebalanceActionExecuted, ValidatorStatusChanged, ACTION_SKIP,
    PREF_OP_COUNTED, TASK_FINALIZE_POOL, TASK_FINISH_EPOCH, TASK_PLAN_BATCH,
    TASK_REBALANCE_ACTION, TASK_RECONCILE_BATCH,
};
use fusion_stake_controller::spl_cpi;
use fusion_stake_controller::state::{
    PHASE_FINALIZE, PHASE_IDLE, PHASE_PLAN_DIRECTED, PHASE_PLAN_FINALIZE, PHASE_PLAN_NEUTRAL,
    PHASE_PREFERENCES, PHASE_REBALANCE, PHASE_RECONCILE,
};
use litesvm::LiteSVM;
use solana_sdk::epoch_schedule::EpochSchedule;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const SOL: u64 = 1_000_000_000;

// `fusion_stake_math::lifecycle::ValidatorStatus` bytes (the math crate is not a direct dep of
// this test crate; the byte values are compile-pinned by the program's own tests).
const STATUS_REGISTERED: u8 = 0;
const STATUS_CANDIDATE: u8 = 1;
const STATUS_ACTIVE: u8 = 2;

// ============================ local math mirrors ============================
// Formula mirrors of `fusion_stake_math` (reserve / churn), replicated here because the pure
// math crate is not a dependency of the test crate. Each is asserted against on-chain results,
// so a drift between mirror and crate fails the test rather than hiding.

fn bps_of(amount: u64, bps: u64) -> u64 {
    (u128::from(amount) * u128::from(bps) / 10_000) as u64
}

/// `fusion_stake_math::reserve::reserve_target`: `min(total, max(min_lamports, bps_of(total)))`.
fn reserve_target_ref(total: u64) -> u64 {
    total.min(RESERVE_MINIMUM_LAMPORTS.max(bps_of(total, RESERVE_TARGET_BPS)))
}

/// `fusion_stake_math::churn::hysteresis`: `max(min_abs, bps_of(total))`.
fn hysteresis_ref(total: u64) -> u64 {
    HYSTERESIS_MIN_LAMPORTS.max(bps_of(total, HYSTERESIS_BPS))
}

// ============================ harness helpers ============================

fn assert_phase(svm: &LiteSVM, want: u8, ctx: &str) {
    assert_eq!(read_epoch_state(svm).phase, want, "phase after {ctx}");
}

/// Raw `EpochState` account bytes — the total "nothing moved" comparison for rejected cranks.
fn es_bytes(svm: &LiteSVM) -> Vec<u8> {
    svm.get_account(&controller_epoch_state_pda()).expect("EpochState exists").data
}

/// Total `MaintenanceRewardPaid` amount a tx paid out.
fn paid(meta: &litesvm::types::TransactionMetadata) -> u64 {
    events_of::<MaintenanceRewardPaid>(meta).iter().map(|e| e.amount).sum()
}

/// The seven epoch-crank instructions by name (execute_next_action needs a validator record and
/// is probed separately where one exists).
fn all_cranks(g: &PoolGenesis, crank: &Pubkey) -> Vec<(&'static str, Instruction)> {
    vec![
        ("reconcile_batch", ctrl_reconcile_batch_ix(g, crank, &[])),
        ("finalize_pool", ctrl_finalize_pool_ix(g, crank)),
        ("close_preference_window", ctrl_close_preference_window_ix()),
        ("plan_directed_batch", ctrl_plan_directed_batch_ix(g, crank, &[])),
        ("plan_neutral_batch", ctrl_plan_neutral_batch_ix(g, crank, &[])),
        ("finalize_plan", ctrl_finalize_plan_ix(g, crank)),
        ("finish_epoch", ctrl_finish_epoch_ix(g, crank)),
    ]
}

/// Every crank NOT in `allowed` must fail `WrongPhase` and leave `EpochState` byte-identical.
fn probe_wrong_phase(
    svm: &mut LiteSVM,
    payer: &Keypair,
    g: &PoolGenesis,
    crank: &Pubkey,
    allowed: &[&str],
    ctx: &str,
) {
    for (name, ix) in all_cranks(g, crank) {
        if allowed.contains(&name) {
            continue;
        }
        svm.expire_blockhash();
        let before = es_bytes(svm);
        let f = send(svm, &[ix], payer, &[])
            .expect_err(&format!("{name} must be phase-gated in {ctx}"));
        assert_eq!(custom_code(&f), E_CTRL_WRONG_PHASE, "{name} in {ctx}");
        assert_eq!(es_bytes(svm), before, "{name} must not mutate EpochState in {ctx}");
    }
}

/// Warp to the preference-window deadline and close the window (PREFERENCES → PLAN-DIRECTED).
fn close_window(svm: &mut LiteSVM, payer: &Keypair) {
    let target = read_epoch_state(svm).preference_window_close_slot;
    let cur = current_slot(svm);
    if cur < target {
        warp_slots(svm, target - cur);
    }
    send(svm, &[ctrl_close_preference_window_ix()], payer, &[])
        .expect("close_preference_window");
}

/// Rewrite a REAL vote account (created by `create_vote_account`) into a "healthy" observation
/// through the reference serializer: positive epoch-credit growth for epochs 0..32 and a
/// far-future freshness slot (saturates to fresh). See the module doc for why this is
/// synthesized rather than voted-in.
fn make_vote_healthy(svm: &mut LiteSVM, vote: &Pubkey) {
    use solana_sdk::vote::state::{BlockTimestamp, VoteState, VoteStateVersions};
    let mut acct = svm.get_account(vote).expect("vote account exists");
    let mut state = VoteState::deserialize(&acct.data).expect("real vote account parses");
    state.epoch_credits = (0u64..32).map(|e| (e, 100 * (e + 1), 100 * e)).collect();
    state.last_timestamp = BlockTimestamp { slot: 1_000_000, timestamp: 1 };
    let mut data = vec![0u8; VoteStateVersions::vote_state_size_of(true)];
    VoteState::serialize(&VoteStateVersions::new_current(state), &mut data)
        .expect("serialize vote state");
    acct.data = data;
    svm.set_account(*vote, acct).unwrap();
}

/// One registered, healthy-observing real validator (vote account + `ValidatorRecord`).
fn register_healthy_validator(svm: &mut LiteSVM, payer: &Keypair, commission: u8) -> Pubkey {
    let node = Keypair::new();
    let vote_kp = Keypair::new();
    let vote = create_vote_account(svm, &node, &vote_kp, commission);
    make_vote_healthy(svm, &vote);
    send(svm, &[ctrl_register_validator_ix(&payer.pubkey(), &vote)], payer, &[])
        .expect("register_validator");
    vote
}

/// Build the 4N reconcile quads for consecutive list indices `[start, start+n)` from the LIVE
/// validator list (the exact derivation the handler re-checks on-chain).
fn reconcile_quads(svm: &LiteSVM, g: &PoolGenesis, start: u32, n: u32) -> Vec<AccountMeta> {
    let list = svm.get_account(&g.validator_list).expect("validator list exists");
    let mut tail = Vec::new();
    for i in start..start + n {
        let entry =
            fusion_stake_view::validator_list::entry_at(&list.data, i).expect("list entry");
        let vote = Pubkey::new_from_array(entry.vote_account_address);
        let vstake =
            spl_cpi::derive_validator_stake(&vote, &g.stake_pool, entry.validator_seed_suffix);
        let tstake =
            spl_cpi::derive_transient_stake(&vote, &g.stake_pool, entry.transient_seed_suffix);
        tail.push(AccountMeta::new(vstake, false));
        tail.push(AccountMeta::new(tstake, false));
        tail.push(AccountMeta::new(validator_record_pda(&vote), false));
        tail.push(AccountMeta::new_readonly(vote, false));
    }
    tail
}

/// `(validator_record [w], vote [])` pairs for `plan_directed_batch`.
fn plan_pairs(votes: &[Pubkey]) -> Vec<AccountMeta> {
    votes
        .iter()
        .flat_map(|v| {
            [
                AccountMeta::new(validator_record_pda(v), false),
                AccountMeta::new_readonly(*v, false),
            ]
        })
        .collect()
}

/// Writable `ValidatorRecord`s for `plan_neutral_batch`.
fn neutral_records(votes: &[Pubkey]) -> Vec<AccountMeta> {
    votes.iter().map(|v| AccountMeta::new(validator_record_pda(v), false)).collect()
}

/// The current validator-list entry at `i` (fusion-stake-view parse over the live bytes).
fn list_entry(svm: &LiteSVM, g: &PoolGenesis, i: u32) -> fusion_stake_view::validator_list::ValidatorEntry {
    let list = svm.get_account(&g.validator_list).expect("validator list exists");
    fusion_stake_view::validator_list::entry_at(&list.data, i).expect("list entry")
}

/// Adjust an account's lamports in place (reward/loss synthesis on the reserve stake).
fn adjust_lamports(svm: &mut LiteSVM, key: &Pubkey, delta: i128) {
    let mut acct = svm.get_account(key).expect("account exists");
    acct.lamports = u64::try_from(i128::from(acct.lamports) + delta).expect("lamports in range");
    svm.set_account(*key, acct).unwrap();
}

/// Overwrite an SPL token account's `amount` field (offset 64) — vault write-down synthesis.
fn set_token_amount(svm: &mut LiteSVM, key: &Pubkey, amount: u64) {
    let mut acct = svm.get_account(key).expect("token account exists");
    acct.data[64..72].copy_from_slice(&amount.to_le_bytes());
    svm.set_account(*key, acct).unwrap();
}

/// The EXACT canonical-primary price pair the fuSOL oracle commits for a conf-0 SOL/USD post:
/// `nav_ray = floor(sol_usd·RAY · total / supply)`, spot = haircut leg, debt = raw NAV leg
/// (mirrors `update_price`'s `scale_view` + haircut + `usd_ray_to_spot`, all floor math).
fn expected_fusol_prices(sol_usd: u128, total: u64, supply: u64, haircut_bps: u128) -> (u128, u128) {
    let nav_ray =
        fusd_math::mul_div_floor(sol_usd * fusd_math::RAY, total as u128, supply as u128).unwrap();
    let coll_ray = fusd_math::mul_div_floor(nav_ray, 10_000 - haircut_bps, 10_000).unwrap();
    let spot =
        fusd_math::oracle_scale::usd_ray_to_spot(coll_ray, COLL_DECIMALS, FUSD_DECIMALS).unwrap();
    let debt =
        fusd_math::oracle_scale::usd_ray_to_spot(nav_ray, COLL_DECIMALS, FUSD_DECIMALS).unwrap();
    (spot, debt)
}

/// Minimal fusd-core protocol + market on the fuSOL mint (no oracle, no price, no reactor —
/// `open_position`/`deposit` need none of them). `gov` must hold no prior protocol state.
fn init_fusol_market(svm: &mut LiteSVM, gov: &Keypair, g: &PoolGenesis) {
    set_program_upgrade_authority(svm, &gov.pubkey());
    send(svm, &[init_protocol_ix(&gov.pubkey())], gov, &[]).expect("init_protocol");
    send(
        svm,
        &[init_market_ix(
            &gov.pubkey(),
            &g.fusol_mint,
            MCR_BPS,
            DEBT_CEILING,
            0,
            0,
            BUCKET_WIDTH_BPS,
            0,
        )],
        gov,
        &[],
    )
    .expect("init_market on the fuSOL mint");
}

/// Deposit `sol` whole SOL through the controller into the pool for `who` (funds must exist).
fn deposit_sol(svm: &mut LiteSVM, who: &Keypair, g: &PoolGenesis, ata: &Pubkey, sol: u64) {
    send(svm, &[ctrl_deposit_sol_ix(&who.pubkey(), g, ata, sol * SOL)], who, &[])
        .expect("deposit_sol through the controller");
}

// =====================================================================================
// Scenario 1 — EMPTY-SET full cycle + phase-order rejections
// =====================================================================================

#[test]
fn empty_set_full_cycle_and_phase_order() {
    let mut svm = new_svm_full();
    let payer = Keypair::new();
    airdrop_sol(&mut svm, &payer.pubkey(), 2_000);
    let g = pool_genesis(&mut svm, &payer);
    let crank = create_ata_and_fund(&mut svm, &payer, &payer.pubkey(), &g.fusol_mint, None, 0);

    // 100 SOL deposited at epoch 0 (the pool is current right after genesis) so the cycle has a
    // non-trivial NAV snapshot: total == supply == 101 SOL exactly (rate 1; the 5 bps fee only
    // moves shares to the vault, both pool legs grow by the full deposit).
    let depositor = Keypair::new();
    airdrop_sol(&mut svm, &depositor.pubkey(), 200);
    let dep_ata =
        create_ata_and_fund(&mut svm, &depositor, &depositor.pubkey(), &g.fusol_mint, None, 0);
    deposit_sol(&mut svm, &depositor, &g, &dep_ata, 100);
    let deposit_fee = 100 * SOL * SOL_DEPOSIT_FEE_BPS / FEE_BPS_DENOMINATOR;
    let vault_genesis = RESERVE_BOOTSTRAP_LAMPORTS + deposit_fee;
    assert_eq!(token_balance(&svm, &g.maintenance_vault), vault_genesis);

    // --- IDLE: every crank is phase-gated; start_epoch is epoch-gated -----------------------
    assert_phase(&svm, PHASE_IDLE, "genesis");
    assert_eq!(read_epoch_state(&svm).controller_epoch, 0);
    probe_wrong_phase(&mut svm, &payer, &g, &crank, &[], "IDLE");
    let f = send(&mut svm, &[ctrl_start_epoch_ix()], &payer, &[])
        .expect_err("start_epoch in the same epoch must fail");
    assert_eq!(custom_code(&f), E_CTRL_EPOCH_NOT_ADVANCED);

    // --- IDLE -> RECONCILE -------------------------------------------------------------------
    warp_epochs(&mut svm, 1);
    let meta = send(&mut svm, &[ctrl_start_epoch_ix()], &payer, &[]).expect("start_epoch");
    assert_phase(&svm, PHASE_RECONCILE, "start_epoch");
    let ev: EpochPhaseChanged = single_event(&meta);
    assert_eq!((ev.from_phase, ev.to_phase, ev.epoch), (PHASE_IDLE, PHASE_RECONCILE, 1));
    let es = read_epoch_state(&svm);
    assert_eq!(es.controller_epoch, 1);
    assert_eq!(es.reconcile_cursor, 0);
    // Provisional churn budget from the PREVIOUS finalized total — zero at first cycle.
    assert_eq!(es.churn_budget_total, 0, "no previous finalized NAV at the first cycle");
    probe_wrong_phase(&mut svm, &payer, &g, &crank, &["reconcile_batch"], "RECONCILE");
    svm.expire_blockhash();
    let f = send(&mut svm, &[ctrl_start_epoch_ix()], &payer, &[])
        .expect_err("start_epoch again in the same cluster epoch");
    assert_eq!(custom_code(&f), E_CTRL_EPOCH_NOT_ADVANCED);

    // --- RECONCILE completes trivially (empty list) -> FINALIZE ------------------------------
    svm.expire_blockhash();
    let meta = send(&mut svm, &[ctrl_reconcile_batch_ix(&g, &crank, &[])], &payer, &[])
        .expect("empty reconcile batch completes the empty phase");
    assert_phase(&svm, PHASE_FINALIZE, "reconcile_batch");
    let ev: EpochPhaseChanged = single_event(&meta);
    assert_eq!((ev.from_phase, ev.to_phase), (PHASE_RECONCILE, PHASE_FINALIZE));
    assert_eq!(paid(&meta), 0, "no stale entry became current: zero reward");
    assert_eq!(token_balance(&svm, &g.maintenance_vault), vault_genesis);
    probe_wrong_phase(&mut svm, &payer, &g, &crank, &["finalize_pool"], "FINALIZE");

    // --- FINALIZE: canonical totals snapshot + reserve/productive math + window open ---------
    let sched: EpochSchedule = svm.get_sysvar();
    let window = sched.get_slots_in_epoch(1)
        / fusion_stake_controller::constants::PREFERENCE_WINDOW_SLOT_DIVISOR;
    let slot_at_finalize = current_slot(&svm);
    let meta = send(&mut svm, &[ctrl_finalize_pool_ix(&g, &crank)], &payer, &[])
        .expect("finalize_pool");
    assert_phase(&svm, PHASE_PREFERENCES, "finalize_pool");
    let es = read_epoch_state(&svm);
    assert_eq!(es.nav_total_lamports, 101 * SOL, "canonical total snapshot");
    assert_eq!(es.nav_fusol_supply, 101 * SOL, "canonical supply snapshot");
    // Reserve target vs the fusion_stake_math::reserve formula: the 10 SOL absolute floor
    // dominates 2% of 101 SOL.
    assert_eq!(es.reserve_target, reserve_target_ref(101 * SOL));
    assert_eq!(es.reserve_target, 10 * SOL);
    assert_eq!(es.productive_lamports, 101 * SOL - es.reserve_target);
    assert_eq!(es.productive_lamports, 91 * SOL);
    // Churn budget refreshed from the FRESH snapshot.
    assert_eq!(es.churn_budget_total, bps_of(101 * SOL, GLOBAL_CHURN_CAP_BPS));
    assert_eq!(es.preference_window_close_slot, slot_at_finalize + window);
    // The canonical stamp advanced (upstream CPI) and the crank earned the finalize reward.
    assert_eq!(read_fork_stake_pool(&svm, &g.stake_pool).last_update_epoch, 1);
    assert_eq!(paid(&meta), CRANK_REWARD_FINALIZE_POOL);
    let ev: MaintenanceRewardPaid = single_event(&meta);
    assert_eq!((ev.task, ev.amount), (TASK_FINALIZE_POOL, CRANK_REWARD_FINALIZE_POOL));
    assert_eq!(token_balance(&svm, &crank), CRANK_REWARD_FINALIZE_POOL);
    assert!(events_of::<NegativeNavObserved>(&meta).is_empty(), "genesis snapshot never signals");
    probe_wrong_phase(&mut svm, &payer, &g, &crank, &["close_preference_window"], "PREFERENCES");

    // --- PREFERENCES: the deadline gate ------------------------------------------------------
    svm.expire_blockhash();
    let f = send(&mut svm, &[ctrl_close_preference_window_ix()], &payer, &[])
        .expect_err("close before the deadline slot");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_WINDOW_STILL_OPEN);
    close_window(&mut svm, &payer);
    assert_phase(&svm, PHASE_PLAN_DIRECTED, "close_preference_window");
    probe_wrong_phase(&mut svm, &payer, &g, &crank, &["plan_directed_batch"], "PLAN_DIRECTED");

    // --- PLAN-DIRECTED completes trivially ----------------------------------------------------
    svm.expire_blockhash();
    let meta = send(&mut svm, &[ctrl_plan_directed_batch_ix(&g, &crank, &[])], &payer, &[])
        .expect("empty plan-directed batch");
    assert_phase(&svm, PHASE_PLAN_NEUTRAL, "plan_directed_batch");
    assert_eq!(paid(&meta), 0, "zero records processed: zero reward");
    let es = read_epoch_state(&svm);
    assert_eq!(es.total_directed_shares, 0);
    assert_eq!(es.sum_directed_targets, 0);
    assert_eq!(es.neutral_total, 91 * SOL, "the whole productive amount is neutral");
    assert_eq!(es.unsaturated_active_count, 0);
    probe_wrong_phase(&mut svm, &payer, &g, &crank, &["plan_neutral_batch"], "PLAN_NEUTRAL");

    // --- PLAN-NEUTRAL: zero Active capacity => the FULL neutral pool is the shortfall ---------
    svm.expire_blockhash();
    let meta = send(&mut svm, &[ctrl_plan_neutral_batch_ix(&g, &crank, &[])], &payer, &[])
        .expect("empty plan-neutral batch");
    assert_phase(&svm, PHASE_PLAN_FINALIZE, "plan_neutral_batch");
    assert_eq!(paid(&meta), 0);
    let es = read_epoch_state(&svm);
    assert_eq!(es.capacity_shortfall, 91 * SOL, "no Actives: shortfall == productive");
    assert_eq!(es.neutral_granted_total, 0);
    probe_wrong_phase(&mut svm, &payer, &g, &crank, &["finalize_plan"], "PLAN_FINALIZE");

    // --- PLAN-FINALIZE: conservation proof + commit ------------------------------------------
    let meta = send(&mut svm, &[ctrl_finalize_plan_ix(&g, &crank)], &payer, &[])
        .expect("finalize_plan");
    assert_phase(&svm, PHASE_REBALANCE, "finalize_plan");
    let ev: PlanFinalized = single_event(&meta);
    assert_eq!(ev.productive_lamports, 91 * SOL);
    assert_eq!(ev.reserve_target, 10 * SOL);
    assert_eq!(ev.total_directed_shares, 0);
    assert_eq!(ev.neutral_total, 91 * SOL);
    assert_eq!(ev.capacity_shortfall, 91 * SOL);
    assert_eq!(ev.churn_budget, bps_of(101 * SOL, GLOBAL_CHURN_CAP_BPS));
    assert_eq!(paid(&meta), CRANK_REWARD_PLAN_BATCH);
    probe_wrong_phase(&mut svm, &payer, &g, &crank, &["finish_epoch"], "REBALANCE");

    // --- REBALANCE -> IDLE (nothing planned: the walk is vacuously complete) ------------------
    let meta =
        send(&mut svm, &[ctrl_finish_epoch_ix(&g, &crank)], &payer, &[]).expect("finish_epoch");
    assert_phase(&svm, PHASE_IDLE, "finish_epoch");
    assert_eq!(paid(&meta), CRANK_REWARD_FINALIZE_POOL);
    let es = read_epoch_state(&svm);
    // Budget conservation: used == exactly the three payouts, all landed on the crank ATA.
    assert_eq!(es.epoch_payout_budget_used, 3 * 1_000_000);
    assert!(es.epoch_payout_budget_used <= CRANK_EPOCH_PAYOUT_BUDGET);
    assert_eq!(token_balance(&svm, &crank), es.epoch_payout_budget_used);
    assert_eq!(
        token_balance(&svm, &g.maintenance_vault),
        vault_genesis - es.epoch_payout_budget_used
    );

    // The machine is re-armed only by a real epoch advance.
    svm.expire_blockhash();
    let f = send(&mut svm, &[ctrl_start_epoch_ix()], &payer, &[])
        .expect_err("cycle done, same epoch: start_epoch stays gated");
    assert_eq!(custom_code(&f), E_CTRL_EPOCH_NOT_ADVANCED);
}

// =====================================================================================
// Scenario 2 — validator admission + increase, directed by a REAL preference
// =====================================================================================

#[test]
fn admission_and_increase_via_directed_preference() {
    let mut svm = new_svm_full();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let g = pool_genesis(&mut svm, &gov);
    let crank = create_ata_and_fund(&mut svm, &gov, &gov.pubkey(), &g.fusol_mint, None, 0);
    init_fusol_market(&mut svm, &gov, &g);

    // Depositor: 40,000 SOL through the controller (total == supply == 40,001 SOL exactly),
    // then a fusd-core position holding 600 fuSOL of ink — enough directed weight for the
    // 500 SOL activation floor (raw target ≈ 588 SOL).
    let depositor = Keypair::new();
    airdrop_sol(&mut svm, &depositor.pubkey(), 41_000);
    let user_ata =
        create_ata_and_fund(&mut svm, &depositor, &depositor.pubkey(), &g.fusol_mint, None, 0);
    deposit_sol(&mut svm, &depositor, &g, &user_ata, 40_000);
    send(
        &mut svm,
        &[open_position_ix(&depositor.pubkey(), &g.fusol_mint, 500)],
        &depositor,
        &[],
    )
    .expect("open_position on the fuSOL market");
    send(
        &mut svm,
        &[deposit_ix(&depositor.pubkey(), &g.fusol_mint, &user_ata, 600 * SOL)],
        &depositor,
        &[],
    )
    .expect("deposit 600 fuSOL into the position");
    let position = position_pda(&g.fusol_mint, &depositor.pubkey());
    assert_eq!(read_position(&svm, &position).ink, 600 * SOL);

    // Three real vote accounts (healthy observations synthesized — see module doc), registered.
    let v0 = register_healthy_validator(&mut svm, &gov, 5);
    let v1 = register_healthy_validator(&mut svm, &gov, 5);
    let _v2 = register_healthy_validator(&mut svm, &gov, 5);

    // Direction: position -> v0 (recorded at cluster epoch 0, eligible from epoch 1).
    send(&mut svm, &[ctrl_set_preference_ix(&depositor.pubkey(), &position, &v0)], &depositor, &[])
        .expect("set_preference");
    assert_eq!(read_preference(&svm, &position).eligible_from_epoch, 1);

    let vstake0 = spl_cpi::derive_validator_stake(&v0, &g.stake_pool, 0);
    let tstake0 = spl_cpi::derive_transient_stake(&v0, &g.stake_pool, 0);
    let exec_v0 = |g: &PoolGenesis, crank: &Pubkey| {
        ctrl_execute_next_action_ix(g, &v0, &vstake0, &tstake0, crank)
    };

    // ============================ EPOCH 1: admission cycle ============================
    warp_epochs(&mut svm, 1);
    send(&mut svm, &[ctrl_start_epoch_ix()], &gov, &[]).expect("start_epoch (1)");
    assert_phase(&svm, PHASE_RECONCILE, "start_epoch (1)");
    let crank_epoch_start = token_balance(&svm, &crank);

    // Snapshot outside the window (wrong phase) is window-gated, not phase-machine-gated.
    let f = send(&mut svm, &[ctrl_snapshot_preference_ix(&position, &v0)], &gov, &[])
        .expect_err("snapshot before the window opens");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_WINDOW_CLOSED);

    send(&mut svm, &[ctrl_reconcile_batch_ix(&g, &crank, &[])], &gov, &[])
        .expect("empty reconcile (list still empty)");
    assert_phase(&svm, PHASE_FINALIZE, "reconcile (1)");
    send(&mut svm, &[ctrl_finalize_pool_ix(&g, &crank)], &gov, &[]).expect("finalize_pool (1)");
    assert_phase(&svm, PHASE_PREFERENCES, "finalize (1)");

    // Count the preference into v0's epoch directed weight. (The identical ix was sent — and
    // rejected — before the window opened; expire the static blockhash or litesvm dedups this
    // retry as AlreadyProcessed.)
    svm.expire_blockhash();
    let meta = send(&mut svm, &[ctrl_snapshot_preference_ix(&position, &v0)], &gov, &[])
        .expect("snapshot_preference");
    let ev: PreferenceUpdated = single_event(&meta);
    assert_eq!((ev.op, ev.observed_ink, ev.vote_account), (PREF_OP_COUNTED, 600 * SOL, v0));
    let rec0 = read_validator_record(&svm, &v0);
    assert_eq!((rec0.directed_shares, rec0.directed_shares_epoch), (600 * SOL, 1));
    assert_eq!(read_preference(&svm, &position).last_counted_epoch, 1);
    // One count per epoch.
    svm.expire_blockhash();
    let f = send(&mut svm, &[ctrl_snapshot_preference_ix(&position, &v0)], &gov, &[])
        .expect_err("double count in one epoch");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_NOT_COUNTABLE);
    // execute_next_action is phase-gated too (a record exists here to probe it with).
    let f = send(&mut svm, &[exec_v0(&g, &crank)], &gov, &[])
        .expect_err("execute_next_action outside REBALANCE");
    assert_eq!(custom_code(&f), E_CTRL_WRONG_PHASE);

    close_window(&mut svm, &gov);
    assert_phase(&svm, PHASE_PLAN_DIRECTED, "close window (1)");
    // The window owns countability entirely: after close, snapshots reject.
    svm.expire_blockhash();
    let f = send(&mut svm, &[ctrl_snapshot_preference_ix(&position, &v0)], &gov, &[])
        .expect_err("snapshot after the window closed");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_WINDOW_CLOSED);

    // A duplicated admission extra fails atomically (once-per-epoch plan idempotency) and
    // moves nothing.
    let before = es_bytes(&svm);
    let f = send(
        &mut svm,
        &[ctrl_plan_directed_batch_ix(&g, &crank, &plan_pairs(&[v0, v0]))],
        &gov,
        &[],
    )
    .expect_err("duplicate record in one plan batch");
    assert_eq!(custom_code(&f), E_CTRL_RECORD_ALREADY_PLANNED);
    assert_eq!(es_bytes(&svm), before, "failed plan batch must not advance");

    // The real batch: empty list slice + three admission extras.
    let meta = send(
        &mut svm,
        &[ctrl_plan_directed_batch_ix(&g, &crank, &plan_pairs(&[v0, v1, _v2]))],
        &gov,
        &[],
    )
    .expect("plan-directed with admission extras");
    assert_phase(&svm, PHASE_PLAN_NEUTRAL, "plan-directed (1)");
    assert_eq!(paid(&meta), CRANK_REWARD_PLAN_BATCH);
    let changes = events_of::<ValidatorStatusChanged>(&meta);
    assert_eq!(changes.len(), 1, "only v0 has admission-grade directed support");
    assert_eq!(
        (changes[0].vote_account, changes[0].old_status, changes[0].new_status),
        (v0, STATUS_REGISTERED, STATUS_CANDIDATE)
    );
    let es = read_epoch_state(&svm);
    assert_eq!(es.nav_total_lamports, 40_001 * SOL);
    assert_eq!(es.nav_fusol_supply, 40_001 * SOL);
    assert_eq!(es.total_directed_shares, 600 * SOL);
    let productive = es.productive_lamports;
    assert_eq!(productive, 40_001 * SOL - reserve_target_ref(40_001 * SOL));
    let raw = (u128::from(productive) * u128::from(600 * SOL) / u128::from(40_001 * SOL)) as u64;
    assert!(raw >= MIN_ACTIVATION_TARGET_LAMPORTS, "the ink sizing must clear the floor");
    let candidate_cap = bps_of(40_001 * SOL, CANDIDATE_CAP_BPS);
    assert!(raw > candidate_cap, "this scenario exercises the Candidate-cap clip");
    let rec0 = read_validator_record(&svm, &v0);
    assert_eq!(rec0.status, STATUS_CANDIDATE);
    assert_eq!(rec0.directed_target, candidate_cap, "directed floor clipped to the Candidate cap");
    assert_eq!(rec0.remaining_capacity, 0, "out-of-pool records expose zero neutral capacity");
    assert_eq!(read_validator_record(&svm, &v1).status, STATUS_REGISTERED);
    assert_eq!(es.sum_directed_targets, candidate_cap);
    assert_eq!(es.neutral_total, productive - candidate_cap);
    assert_eq!(es.unsaturated_active_count, 0);

    send(&mut svm, &[ctrl_plan_neutral_batch_ix(&g, &crank, &[])], &gov, &[])
        .expect("plan-neutral (no Active capacity)");
    assert_phase(&svm, PHASE_PLAN_FINALIZE, "plan-neutral (1)");
    assert_eq!(read_epoch_state(&svm).capacity_shortfall, productive - candidate_cap);
    send(&mut svm, &[ctrl_finalize_plan_ix(&g, &crank)], &gov, &[]).expect("finalize_plan (1)");
    assert_phase(&svm, PHASE_REBALANCE, "finalize_plan (1)");

    // Admission add negatives first: v1 is planned but Registered — never an add target.
    let before = es_bytes(&svm);
    let f = send(
        &mut svm,
        &[ctrl_execute_next_action_ix(
            &g,
            &v1,
            &spl_cpi::derive_validator_stake(&v1, &g.stake_pool, 0),
            &spl_cpi::derive_transient_stake(&v1, &g.stake_pool, 0),
            &crank,
        )],
        &gov,
        &[],
    )
    .expect_err("Registered validator cannot be added");
    assert_eq!(custom_code(&f), E_CTRL_WRONG_ACTION_TARGET);
    assert_eq!(es_bytes(&svm), before, "rejected action must not advance anything");

    // THE admission add: a real AddValidatorToPool CPI, validator stake account funded from
    // the reserve at the derived PDA.
    let reserve_before = svm.get_account(&g.reserve_stake).unwrap().lamports;
    let meta = send(&mut svm, &[exec_v0(&g, &crank)], &gov, &[]).expect("AddValidatorToPool");
    let ev: RebalanceActionExecuted = single_event(&meta);
    assert_eq!((ev.action, ev.vote_account, ev.lamports), (spl_cpi::IX_ADD_VALIDATOR_TO_POOL, v0, 0));
    assert_eq!(paid(&meta), CRANK_REWARD_REBALANCE_ACTION);
    assert_eq!(read_fork_validator_list_len(&svm, &g.validator_list), 1);
    let entry = list_entry(&svm, &g, 0);
    assert_eq!(entry.vote_account_address, v0.to_bytes());
    assert_eq!(entry.status, 0, "upstream StakeStatus::Active");
    assert_eq!(entry.transient_stake_lamports, 0);
    let vstake_acct = svm.get_account(&vstake0).expect("validator stake account created");
    assert_eq!(vstake_acct.owner, STAKE_PROGRAM_ID);
    assert_eq!(
        entry.active_stake_lamports, vstake_acct.lamports,
        "list entry mirrors the funded stake account"
    );
    assert!(entry.active_stake_lamports >= SOL, "litesvm minimum delegation is 1 SOL");
    assert_eq!(
        reserve_before - svm.get_account(&g.reserve_stake).unwrap().lamports,
        vstake_acct.lamports,
        "the add is funded exactly from the reserve"
    );
    let rec0 = read_validator_record(&svm, &v0);
    assert_eq!(rec0.validator_list_index, 0);
    assert_eq!(rec0.pool_entry_status, 0);
    assert_eq!(read_epoch_state(&svm).rebalance_actions_done, 1);

    // v0 now has a list slot: it leaves admission mode, and this epoch planned ZERO list
    // ordinals, so the cursor walk is already complete.
    svm.expire_blockhash();
    let f = send(&mut svm, &[exec_v0(&g, &crank)], &gov, &[])
        .expect_err("nothing left in the epoch-1 walk");
    assert_eq!(custom_code(&f), E_CTRL_REBALANCE_COMPLETE);

    send(&mut svm, &[ctrl_finish_epoch_ix(&g, &crank)], &gov, &[]).expect("finish_epoch (1)");
    assert_phase(&svm, PHASE_IDLE, "finish (1)");
    let es = read_epoch_state(&svm);
    assert_eq!(
        es.epoch_payout_budget_used,
        token_balance(&svm, &crank) - crank_epoch_start,
        "budget accounting == actual payouts"
    );
    assert!(es.epoch_payout_budget_used <= CRANK_EPOCH_PAYOUT_BUDGET);

    // ============================ EPOCH 2: reconcile + increase ============================
    warp_epochs(&mut svm, 1);
    send(&mut svm, &[ctrl_start_epoch_ix()], &gov, &[]).expect("start_epoch (2)");
    let crank_epoch_start = token_balance(&svm, &crank);

    // A malformed quad (validator/transient swapped) fails loudly WITHOUT advancing.
    let good = reconcile_quads(&svm, &g, 0, 1);
    let bad = vec![good[1].clone(), good[0].clone(), good[2].clone(), good[3].clone()];
    let before = es_bytes(&svm);
    let f = send(&mut svm, &[ctrl_reconcile_batch_ix(&g, &crank, &bad)], &gov, &[])
        .expect_err("swapped pair addresses");
    assert_eq!(custom_code(&f), E_CTRL_INVALID_REMAINING_ACCOUNTS);
    assert_eq!(es_bytes(&svm), before, "failed reconcile batch must not advance the cursor");

    let meta = send(&mut svm, &[ctrl_reconcile_batch_ix(&g, &crank, &good)], &gov, &[])
        .expect("reconcile the one-entry list");
    assert_phase(&svm, PHASE_FINALIZE, "reconcile (2)");
    assert_eq!(paid(&meta), CRANK_REWARD_RECONCILE_BATCH, "a stale entry became current");
    let ev: MaintenanceRewardPaid = single_event(&meta);
    assert_eq!(ev.task, TASK_RECONCILE_BATCH);
    let rec0 = read_validator_record(&svm, &v0);
    assert_eq!(rec0.observed_epoch, 2);
    assert!(rec0.observed_liveness_ok && rec0.observed_commission_ok);
    assert!(rec0.has_pool_stake);
    let entry = list_entry(&svm, &g, 0);
    assert_eq!(rec0.last_active_lamports, entry.active_stake_lamports);
    assert_eq!(entry.last_update_epoch, 2, "the CPI stamped the entry current");
    let es = read_epoch_state(&svm);
    assert_eq!(es.total_delegated_lamports, entry.active_stake_lamports);
    assert_eq!(es.healthy_delegated_lamports, entry.active_stake_lamports);

    send(&mut svm, &[ctrl_finalize_pool_ix(&g, &crank)], &gov, &[]).expect("finalize_pool (2)");
    let es = read_epoch_state(&svm);
    let pool = read_fork_stake_pool(&svm, &g.stake_pool);
    assert_eq!(es.nav_total_lamports, pool.total_lamports);
    assert_eq!(es.nav_fusol_supply, pool.pool_token_supply);

    // Re-count the preference for THIS epoch (shares are epoch-stamped; stale = zero).
    send(&mut svm, &[ctrl_snapshot_preference_ix(&position, &v0)], &gov, &[])
        .expect("snapshot (epoch 2)");
    assert_eq!(read_validator_record(&svm, &v0).directed_shares_epoch, 2);

    close_window(&mut svm, &gov);
    let meta = send(
        &mut svm,
        &[ctrl_plan_directed_batch_ix(&g, &crank, &plan_pairs(&[v0]))],
        &gov,
        &[],
    )
    .expect("plan-directed (list slice)");
    assert_phase(&svm, PHASE_PLAN_NEUTRAL, "plan-directed (2)");
    assert!(events_of::<ValidatorStatusChanged>(&meta).is_empty(), "healthy streak 1 < 2: still Candidate");
    let rec0 = read_validator_record(&svm, &v0);
    assert_eq!(rec0.status, STATUS_CANDIDATE);
    let candidate_cap = bps_of(read_epoch_state(&svm).nav_total_lamports, CANDIDATE_CAP_BPS);
    assert_eq!(rec0.directed_target, candidate_cap);
    assert_eq!(rec0.final_target, candidate_cap);
    assert_eq!(rec0.remaining_capacity, 0, "Candidates never expose neutral capacity");

    send(&mut svm, &[ctrl_plan_neutral_batch_ix(&g, &crank, &[])], &gov, &[])
        .expect("plan-neutral (2)");
    send(&mut svm, &[ctrl_finalize_plan_ix(&g, &crank)], &gov, &[]).expect("finalize_plan (2)");
    assert_phase(&svm, PHASE_REBALANCE, "finalize_plan (2)");

    // The epoch is NOT finishable before the walk (planned 1 => 2 slots) with budget left.
    let f = send(&mut svm, &[ctrl_finish_epoch_ix(&g, &crank)], &gov, &[])
        .expect_err("walk incomplete, budget available");
    assert_eq!(custom_code(&f), E_CTRL_EPOCH_NOT_FINISHED);
    // A record without a CURRENT plan is rejected before anything else.
    let f = send(
        &mut svm,
        &[ctrl_execute_next_action_ix(
            &g,
            &v1,
            &spl_cpi::derive_validator_stake(&v1, &g.stake_pool, 0),
            &spl_cpi::derive_transient_stake(&v1, &g.stake_pool, 0),
            &crank,
        )],
        &gov,
        &[],
    )
    .expect_err("v1 was not planned this epoch");
    assert_eq!(custom_code(&f), E_CTRL_STALE_VALIDATOR_RECORD);

    // Walk slot 0 (pass 0 = draining pass): v0 is Candidate => a SKIP that advances the
    // cursor, pays zero, and emits the executed choice.
    let vault_before = token_balance(&svm, &g.maintenance_vault);
    svm.expire_blockhash();
    let meta = send(&mut svm, &[exec_v0(&g, &crank)], &gov, &[]).expect("pass-0 visit");
    let ev: RebalanceActionExecuted = single_event(&meta);
    assert_eq!((ev.action, ev.lamports), (ACTION_SKIP, 0));
    assert_eq!(paid(&meta), 0, "skips earn zero");
    assert_eq!(token_balance(&svm, &g.maintenance_vault), vault_before);
    let es = read_epoch_state(&svm);
    assert_eq!((es.rebalance_cursor, es.rebalance_actions_done), (1, 0));

    // Walk slot 1 (pass 1): deficit -> a REAL IncreaseValidatorStake of exactly the deviation
    // (all caps are far larger here).
    let entry = list_entry(&svm, &g, 0);
    let rec0 = read_validator_record(&svm, &v0);
    let deviation = rec0.final_target - entry.active_stake_lamports;
    let es = read_epoch_state(&svm);
    assert!(deviation > hysteresis_ref(es.nav_total_lamports), "sized past hysteresis");
    assert!(deviation < bps_of(es.nav_total_lamports, VALIDATOR_MOVE_CAP_BPS));
    svm.expire_blockhash();
    let meta = send(&mut svm, &[exec_v0(&g, &crank)], &gov, &[]).expect("pass-1 increase");
    let ev: RebalanceActionExecuted = single_event(&meta);
    assert_eq!((ev.action, ev.lamports), (spl_cpi::IX_INCREASE_VALIDATOR_STAKE, deviation));
    assert_eq!(paid(&meta), CRANK_REWARD_REBALANCE_ACTION);
    let ev: MaintenanceRewardPaid = single_event(&meta);
    assert_eq!(ev.task, TASK_REBALANCE_ACTION);
    let es = read_epoch_state(&svm);
    assert_eq!(es.churn_budget_used, deviation);
    assert_eq!((es.rebalance_cursor, es.rebalance_actions_done), (2, 1));
    let rec0 = read_validator_record(&svm, &v0);
    assert_eq!(rec0.last_increase_epoch, 2);
    // Real transient stake exists and the list entry tracks it.
    let entry = list_entry(&svm, &g, 0);
    assert!(entry.transient_stake_lamports >= deviation, "transient carries the move");
    let tstake_acct = svm.get_account(&tstake0).expect("transient stake account created");
    assert_eq!(tstake_acct.owner, STAKE_PROGRAM_ID);
    assert_eq!(entry.transient_stake_lamports, tstake_acct.lamports);

    // The walk is complete; a further call names no slot.
    svm.expire_blockhash();
    let f = send(&mut svm, &[exec_v0(&g, &crank)], &gov, &[]).expect_err("walk complete");
    assert_eq!(custom_code(&f), E_CTRL_REBALANCE_COMPLETE);

    send(&mut svm, &[ctrl_finish_epoch_ix(&g, &crank)], &gov, &[]).expect("finish_epoch (2)");
    assert_phase(&svm, PHASE_IDLE, "finish (2)");
    let es = read_epoch_state(&svm);
    assert_eq!(es.epoch_payout_budget_used, token_balance(&svm, &crank) - crank_epoch_start);
    assert!(es.epoch_payout_budget_used <= CRANK_EPOCH_PAYOUT_BUDGET);

    // ============ EPOCH 3: transient MERGE + Candidate->Active + cap-clipped increase ==========
    let pre_merge_active = list_entry(&svm, &g, 0).active_stake_lamports;
    let pre_merge_transient = list_entry(&svm, &g, 0).transient_stake_lamports;
    warp_epochs(&mut svm, 1);
    send(&mut svm, &[ctrl_start_epoch_ix()], &gov, &[]).expect("start_epoch (3)");

    // Reconcile merges the (now fully active — empty StakeHistory) transient back in.
    let quads = reconcile_quads(&svm, &g, 0, 1);
    send(&mut svm, &[ctrl_reconcile_batch_ix(&g, &crank, &quads)], &gov, &[])
        .expect("reconcile (3): transient merge");
    let entry = list_entry(&svm, &g, 0);
    assert_eq!(entry.transient_stake_lamports, 0, "transient merged");
    // Upstream merge semantics: the transient's DELEGATION is absorbed into active stake, while
    // its rent-exempt reserve is swept back to the pool reserve (UpdateValidatorListBalance
    // withdraws any validator-account surplus above delegation + rent).
    let stake_rent = svm.minimum_balance_for_rent_exemption(200);
    assert_eq!(
        entry.active_stake_lamports,
        pre_merge_active + pre_merge_transient - stake_rent,
        "active absorbed the transient delegation (rent swept to the reserve)"
    );
    assert!(svm.get_account(&tstake0).is_none_or(|a| a.lamports == 0), "transient account gone");

    send(&mut svm, &[ctrl_finalize_pool_ix(&g, &crank)], &gov, &[]).expect("finalize_pool (3)");
    // NO snapshot this epoch: the direction goes stale and re-enters neutral allocation.
    close_window(&mut svm, &gov);

    let meta = send(
        &mut svm,
        &[ctrl_plan_directed_batch_ix(&g, &crank, &plan_pairs(&[v0]))],
        &gov,
        &[],
    )
    .expect("plan-directed (3)");
    let changes = events_of::<ValidatorStatusChanged>(&meta);
    assert_eq!(changes.len(), 1, "the healthy streak reached 2: promotion");
    assert_eq!((changes[0].old_status, changes[0].new_status), (STATUS_CANDIDATE, STATUS_ACTIVE));
    let es = read_epoch_state(&svm);
    let active_cap = bps_of(es.nav_total_lamports, ACTIVE_VALIDATOR_CAP_BPS);
    let rec0 = read_validator_record(&svm, &v0);
    assert_eq!(rec0.status, STATUS_ACTIVE);
    assert_eq!(rec0.directed_target, 0, "stale directed shares read as zero");
    assert_eq!(rec0.remaining_capacity, active_cap, "Active exposes cap-minus-target capacity");
    assert_eq!(es.unsaturated_active_count, 1);
    assert_eq!(es.total_directed_shares, 0);

    // One capacity round: the single Active absorbs its full cap; the rest is shortfall.
    send(&mut svm, &[ctrl_plan_neutral_batch_ix(&g, &crank, &neutral_records(&[v0]))], &gov, &[])
        .expect("plan-neutral (3): one capacity round");
    assert_phase(&svm, PHASE_PLAN_FINALIZE, "plan-neutral (3)");
    let es = read_epoch_state(&svm);
    let rec0 = read_validator_record(&svm, &v0);
    assert_eq!(rec0.neutral_granted, active_cap);
    assert_eq!(rec0.final_target, active_cap);
    assert_eq!(rec0.remaining_capacity, 0);
    assert_eq!(es.neutral_granted_total, active_cap);
    // Conservation identity (the same one finalize_plan proves).
    assert_eq!(
        es.sum_directed_targets + es.neutral_granted_total + es.capacity_shortfall,
        es.productive_lamports
    );

    send(&mut svm, &[ctrl_finalize_plan_ix(&g, &crank)], &gov, &[]).expect("finalize_plan (3)");

    // Pass 0: skip. Pass 1: the deficit exceeds the per-validator move cap => clipped increase.
    svm.expire_blockhash();
    let meta = send(&mut svm, &[exec_v0(&g, &crank)], &gov, &[]).expect("pass-0 (3)");
    assert_eq!(single_event::<RebalanceActionExecuted>(&meta).action, ACTION_SKIP);
    let entry = list_entry(&svm, &g, 0);
    let rec0 = read_validator_record(&svm, &v0);
    let es = read_epoch_state(&svm);
    let move_cap = bps_of(es.nav_total_lamports, VALIDATOR_MOVE_CAP_BPS);
    assert!(rec0.final_target - entry.active_stake_lamports > move_cap, "deficit exceeds the cap");
    svm.expire_blockhash();
    let meta = send(&mut svm, &[exec_v0(&g, &crank)], &gov, &[]).expect("pass-1 (3): clipped");
    let ev: RebalanceActionExecuted = single_event(&meta);
    assert_eq!(
        (ev.action, ev.lamports),
        (spl_cpi::IX_INCREASE_VALIDATOR_STAKE, move_cap),
        "the increase is clipped to the per-validator move cap"
    );
    assert_eq!(read_epoch_state(&svm).churn_budget_used, move_cap);

    send(&mut svm, &[ctrl_finish_epoch_ix(&g, &crank)], &gov, &[]).expect("finish_epoch (3)");
    assert_phase(&svm, PHASE_IDLE, "finish (3)");
}

// =====================================================================================
// Scenario 3 — NAV growth + negative NAV, picked up by the canonical-primary oracle
// =====================================================================================

#[test]
fn nav_growth_and_negative_nav_reach_the_fusol_oracle() {
    use fusd_core::constants::PYTH_SOL_USD_FEED_ID;
    const HAIRCUT_BPS: u16 = 2_000; // 20% — keeps every expected price an integer USD amount

    let mut svm = new_svm_full();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let g = pool_genesis(&mut svm, &gov);
    let crank = create_ata_and_fund(&mut svm, &gov, &gov.pubkey(), &g.fusol_mint, None, 0);

    // fusd-core market on the fuSOL mint + a canonical-primary oracle bound to the REAL pool.
    init_fusol_market(&mut svm, &gov, &g);
    let quote = create_quote_mint(&mut svm, &gov, FUSD_DECIMALS);
    let pyth = Pubkey::new_unique();
    let sb = Pubkey::new_unique();
    let mut args = default_oracle_args();
    args.pyth_feed_id = PYTH_SOL_USD_FEED_ID;
    args.switchboard_feed = sb;
    args.orca_pool = Pubkey::default();
    args.raydium_pool = Pubkey::default();
    args.lst_stake_pool = g.stake_pool; // the REAL genesis pool, not a fabricated account
    args.canonical_primary = true;
    args.liquidity_haircut_bps = HAIRCUT_BPS;
    send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &g.fusol_mint, &quote, args)], &gov, &[])
        .expect("init_market_oracle (canonical-primary on the real pool)");

    // 999 SOL deposit => total == supply == 1000 SOL exactly (rate 1).
    let depositor = Keypair::new();
    airdrop_sol(&mut svm, &depositor.pubkey(), 1_100);
    let dep_ata =
        create_ata_and_fund(&mut svm, &depositor, &depositor.pubkey(), &g.fusol_mint, None, 0);
    deposit_sol(&mut svm, &depositor, &g, &dep_ata, 999);

    // One full trivial cycle (empty validator set) for the given epoch.
    fn run_cycle(
        svm: &mut LiteSVM,
        gov: &Keypair,
        g: &PoolGenesis,
        crank: &Pubkey,
    ) -> litesvm::types::TransactionMetadata {
        send(svm, &[ctrl_start_epoch_ix()], gov, &[]).expect("start_epoch");
        send(svm, &[ctrl_reconcile_batch_ix(g, crank, &[])], gov, &[]).expect("reconcile");
        let finalize_meta =
            send(svm, &[ctrl_finalize_pool_ix(g, crank)], gov, &[]).expect("finalize_pool");
        close_window(svm, gov);
        send(svm, &[ctrl_plan_directed_batch_ix(g, crank, &[])], gov, &[]).expect("plan-directed");
        send(svm, &[ctrl_plan_neutral_batch_ix(g, crank, &[])], gov, &[]).expect("plan-neutral");
        send(svm, &[ctrl_finalize_plan_ix(g, crank)], gov, &[]).expect("finalize_plan");
        send(svm, &[ctrl_finish_epoch_ix(g, crank)], gov, &[]).expect("finish_epoch");
        finalize_meta
    }

    // Post fresh SOL/USD legs ($100, conf 0 => the composed price is exact) and crank the
    // fusd oracle against the REAL pool account.
    fn crank_oracle(svm: &mut LiteSVM, gov: &Keypair, g: &PoolGenesis, pyth: &Pubkey, sb: &Pubkey) {
        let now = now_unix(svm);
        set_pyth_price(svm, pyth, fusd_core::constants::PYTH_SOL_USD_FEED_ID, 100 * 100_000_000, 0, -8, now);
        set_switchboard_feed(svm, sb, 100 * 1_000_000_000_000_000_000, 0, 1, now);
        send(
            svm,
            &[update_price_lst_ix(&gov.pubkey(), &g.fusol_mint, pyth, Some(*sb), None, Some(g.stake_pool))],
            gov,
            &[],
        )
        .expect("update_price on the canonical-primary market");
    }

    // --- Epoch 1 baseline: rate exactly 1 -----------------------------------------------------
    warp_epochs(&mut svm, 1);
    let meta = run_cycle(&mut svm, &gov, &g, &crank);
    assert!(events_of::<NegativeNavObserved>(&meta).is_empty(), "genesis -> first snapshot never signals");
    let es = read_epoch_state(&svm);
    assert_eq!((es.nav_total_lamports, es.nav_fusol_supply), (1_000 * SOL, 1_000 * SOL));
    crank_oracle(&mut svm, &gov, &g, &pyth, &sb);
    let m = read_market(&svm, &market_pda(&g.fusol_mint));
    assert!(!m.mint_frozen, "healthy legs + fresh pool => mints open");
    assert_eq!(m.spot, spot_for_usd(80), "$100 x rate 1 - 20% haircut");
    assert_eq!(m.debt_spot, spot_for_usd(100), "debt leg = raw NAV");

    // --- Synthesize +50 SOL of rewards on the reserve, run the cycle, NAV rate rises ----------
    adjust_lamports(&mut svm, &g.reserve_stake, 50 * SOL as i128);
    warp_epochs(&mut svm, 1);
    let meta = run_cycle(&mut svm, &gov, &g, &crank);
    assert!(events_of::<NegativeNavObserved>(&meta).is_empty(), "a NAV RISE never signals");
    let es = read_epoch_state(&svm);
    assert_eq!(es.nav_total_lamports, 1_050 * SOL);
    // The 1% epoch fee minted fuSOL to the maintenance vault, so supply grew slightly; the
    // RATE still rose (exact cross-multiplication, the on-chain comparison).
    let grown_supply = es.nav_fusol_supply;
    assert!(grown_supply > 1_000 * SOL && grown_supply < 1_001 * SOL, "epoch fee minted");
    assert!(
        u128::from(es.nav_total_lamports) * u128::from(1_000 * SOL)
            > u128::from(1_000 * SOL) * u128::from(grown_supply),
        "finalized NAV rate rose"
    );
    let spot_before = read_market(&svm, &market_pda(&g.fusol_mint)).spot;
    crank_oracle(&mut svm, &gov, &g, &pyth, &sb);
    let m = read_market(&svm, &market_pda(&g.fusol_mint));
    let (want_spot, want_debt) =
        expected_fusol_prices(100, 1_050 * SOL, grown_supply, HAIRCUT_BPS as u128);
    assert_eq!(m.spot, want_spot, "update_price picked up the higher finalized rate");
    assert_eq!(m.debt_spot, want_debt);
    assert!(m.spot > spot_before, "the committed price rose with NAV");
    assert!(!m.mint_frozen);

    // --- Synthesize a 100 SOL DECREASE: NegativeNavObserved + the oracle commits lower --------
    adjust_lamports(&mut svm, &g.reserve_stake, -(100 * SOL as i128));
    warp_epochs(&mut svm, 1);
    let meta = run_cycle(&mut svm, &gov, &g, &crank);
    let evs = events_of::<NegativeNavObserved>(&meta);
    assert_eq!(evs.len(), 1, "the finalize crank must flag the NAV decrease");
    assert_eq!(evs[0].previous_total_lamports, 1_050 * SOL);
    assert_eq!(evs[0].new_total_lamports, 950 * SOL);
    assert_eq!(evs[0].fusol_supply, grown_supply, "no fee on non-positive rewards");
    let es = read_epoch_state(&svm);
    assert_eq!((es.nav_total_lamports, es.nav_fusol_supply), (950 * SOL, grown_supply));

    crank_oracle(&mut svm, &gov, &g, &pyth, &sb);
    let m = read_market(&svm, &market_pda(&g.fusol_mint));
    let (want_spot, want_debt) =
        expected_fusol_prices(100, 950 * SOL, grown_supply, HAIRCUT_BPS as u128);
    assert_eq!(m.spot, want_spot, "the oracle committed the LOWER NAV immediately");
    assert_eq!(m.debt_spot, want_debt, "the liquidation leg dropped in the same crank");
    assert!(m.spot < spot_before, "strictly below even the pre-growth price");
    assert!(!m.mint_frozen, "a committed lower NAV freezes nothing by itself");
}

// =====================================================================================
// Scenario 4 — crank reward budget: funded payouts, no-op zero, vault clamp
// =====================================================================================

#[test]
fn crank_reward_budget_no_op_zero_and_vault_clamp() {
    let mut svm = new_svm_full();
    let payer = Keypair::new();
    airdrop_sol(&mut svm, &payer.pubkey(), 2_000);
    let g = pool_genesis(&mut svm, &payer);
    let crank = create_ata_and_fund(&mut svm, &payer, &payer.pubkey(), &g.fusol_mint, None, 0);

    // Deposit fees fund the vault: genesis 1 fuSOL + 5 bps of 100 SOL.
    let depositor = Keypair::new();
    airdrop_sol(&mut svm, &depositor.pubkey(), 200);
    let dep_ata =
        create_ata_and_fund(&mut svm, &depositor, &depositor.pubkey(), &g.fusol_mint, None, 0);
    deposit_sol(&mut svm, &depositor, &g, &dep_ata, 100);
    let fee = 100 * SOL * SOL_DEPOSIT_FEE_BPS / FEE_BPS_DENOMINATOR;
    assert_eq!(token_balance(&svm, &g.maintenance_vault), RESERVE_BOOTSTRAP_LAMPORTS + fee);

    // ---- Epoch 1: funded payouts; an out-of-band upstream update makes finalize a no-op -----
    warp_epochs(&mut svm, 1);
    send(&mut svm, &[ctrl_start_epoch_ix()], &payer, &[]).expect("start_epoch (1)");
    let vault_start = token_balance(&svm, &g.maintenance_vault);

    let meta = send(&mut svm, &[ctrl_reconcile_batch_ix(&g, &crank, &[])], &payer, &[])
        .expect("empty reconcile");
    assert_eq!(paid(&meta), 0, "nothing became current: zero reward");

    // Out-of-band PERMISSIONLESS upstream update (any keeper can run the raw stake-pool ix):
    // the canonical snapshot is already current when finalize_pool runs.
    let oob = spl_cpi::update_stake_pool_balance(
        &g.stake_pool,
        &g.pool_withdraw_authority,
        &g.validator_list,
        &g.reserve_stake,
        &g.maintenance_vault,
        &g.fusol_mint,
        &SPL_TOKEN_ID,
    );
    send(&mut svm, &[oob], &payer, &[]).expect("out-of-band UpdateStakePoolBalance");
    assert_eq!(read_fork_stake_pool(&svm, &g.stake_pool).last_update_epoch, 1);

    let meta = send(&mut svm, &[ctrl_finalize_pool_ix(&g, &crank)], &payer, &[])
        .expect("finalize_pool still advances the phase");
    assert_phase(&svm, PHASE_PREFERENCES, "no-op finalize");
    assert_eq!(paid(&meta), 0, "the out-of-band cranker forfeited the reward: no-op pays zero");
    assert!(events_of::<MaintenanceRewardPaid>(&meta).is_empty());
    assert_eq!(token_balance(&svm, &g.maintenance_vault), vault_start);

    close_window(&mut svm, &payer);
    send(&mut svm, &[ctrl_plan_directed_batch_ix(&g, &crank, &[])], &payer, &[]).expect("plan-d");
    send(&mut svm, &[ctrl_plan_neutral_batch_ix(&g, &crank, &[])], &payer, &[]).expect("plan-n");

    // The vault itself can never be the reward recipient (self-transfer grief, closed).
    let before = es_bytes(&svm);
    let f = send(&mut svm, &[ctrl_finalize_plan_ix(&g, &g.maintenance_vault)], &payer, &[])
        .expect_err("vault as reward recipient");
    assert_eq!(custom_code(&f), E_CTRL_INVALID_REWARD_RECIPIENT);
    assert_eq!(es_bytes(&svm), before);

    let meta =
        send(&mut svm, &[ctrl_finalize_plan_ix(&g, &crank)], &payer, &[]).expect("finalize_plan");
    assert_eq!(paid(&meta), CRANK_REWARD_PLAN_BATCH);
    assert_eq!(single_event::<MaintenanceRewardPaid>(&meta).task, TASK_PLAN_BATCH);
    let meta =
        send(&mut svm, &[ctrl_finish_epoch_ix(&g, &crank)], &payer, &[]).expect("finish_epoch");
    assert_eq!(paid(&meta), CRANK_REWARD_FINALIZE_POOL);
    assert_eq!(single_event::<MaintenanceRewardPaid>(&meta).task, TASK_FINISH_EPOCH);

    let es = read_epoch_state(&svm);
    assert_eq!(es.epoch_payout_budget_used, CRANK_REWARD_PLAN_BATCH + CRANK_REWARD_FINALIZE_POOL);
    assert!(es.epoch_payout_budget_used <= CRANK_EPOCH_PAYOUT_BUDGET);
    assert_eq!(token_balance(&svm, &crank), es.epoch_payout_budget_used);
    assert_eq!(
        token_balance(&svm, &g.maintenance_vault),
        vault_start - es.epoch_payout_budget_used,
        "every paid lamport left the vault"
    );

    // ---- Epoch 2: payout clamps to the vault; an empty vault pays zero but blocks nothing ----
    // (Synthesized vault write-down — paying it down for real would take ~1000 tasks.)
    set_token_amount(&mut svm, &g.maintenance_vault, 500);
    warp_epochs(&mut svm, 1);
    send(&mut svm, &[ctrl_start_epoch_ix()], &payer, &[]).expect("start_epoch (2)");
    assert_eq!(read_epoch_state(&svm).epoch_payout_budget_used, 0, "budget resets per epoch");
    let crank_start = token_balance(&svm, &crank);

    send(&mut svm, &[ctrl_reconcile_batch_ix(&g, &crank, &[])], &payer, &[]).expect("reconcile");
    // finalize EARNS this epoch (the pool is stale) but the vault only holds 500 units:
    // payout = min(task 1_000_000, budget, vault 500) = 500.
    let meta = send(&mut svm, &[ctrl_finalize_pool_ix(&g, &crank)], &payer, &[])
        .expect("finalize_pool (2)");
    assert_eq!(paid(&meta), 500, "payout clamped to the vault balance");
    assert_eq!(token_balance(&svm, &g.maintenance_vault), 0, "vault drained");

    close_window(&mut svm, &payer);
    send(&mut svm, &[ctrl_plan_directed_batch_ix(&g, &crank, &[])], &payer, &[]).expect("plan-d");
    send(&mut svm, &[ctrl_plan_neutral_batch_ix(&g, &crank, &[])], &payer, &[]).expect("plan-n");
    // Empty vault: the earning cranks still execute, they just pay nothing.
    let meta = send(&mut svm, &[ctrl_finalize_plan_ix(&g, &crank)], &payer, &[])
        .expect("finalize_plan executes unpaid");
    assert_eq!(paid(&meta), 0);
    assert!(events_of::<MaintenanceRewardPaid>(&meta).is_empty());
    let meta = send(&mut svm, &[ctrl_finish_epoch_ix(&g, &crank)], &payer, &[])
        .expect("finish_epoch executes unpaid");
    assert_eq!(paid(&meta), 0);
    assert_phase(&svm, PHASE_IDLE, "cycle completed with an empty vault");

    let es = read_epoch_state(&svm);
    assert_eq!(es.epoch_payout_budget_used, 500, "budget tracks ACTUAL payouts only");
    assert!(es.epoch_payout_budget_used <= CRANK_EPOCH_PAYOUT_BUDGET);
    assert_eq!(token_balance(&svm, &crank) - crank_start, 500);
}
