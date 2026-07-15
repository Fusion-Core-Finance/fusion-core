//! Spec §17.2 EPOCH-ALLOCATION scenarios against the REAL controller + mainnet-dumped stake
//! pool: multi-Active neutral distribution, mixed directed+neutral conservation, cap-clipped
//! direction redistributed to other Actives, uncounted preferences (wallet/LP/unset/stale/
//! omitted) flowing into neutral, and a fuSOL-collateral LIQUIDATION proving the money path
//! carries no controller account (exit independence). One `#[test]` each:
//!
//! 1. `multi_active_all_undirected_equal_neutral` — three all-undirected Active validators.
//!    With the 2%-of-total per-validator cap, the neutral pool structurally exceeds total
//!    Active capacity, so every Active saturates at the SAME `active_cap` (equal allocation)
//!    and the remainder is recorded as aggregate-capacity shortfall — with the conservation
//!    identity `Σ directed + Σ neutral + shortfall == productive` proved on-chain by
//!    `finalize_plan`.
//! 2. `mixed_directed_and_neutral_conserves` — one directed Active (a below-cap directed
//!    floor) alongside two undirected Actives; the directed floor is claimed first, the
//!    remaining productive lamports distribute as neutral, and the full conservation identity
//!    holds across the mix.
//! 3. `cap_clipped_direction_redistributes_to_other_actives` — direction far exceeding one
//!    validator's cap clips its `directed_target` to `active_cap` (its neutral capacity → 0),
//!    and the clipped excess stays in the neutral pool, redistributed to the OTHER Active
//!    validators (they saturate; the clipped validator receives no neutral).
//! 4. `uncounted_preferences_enter_neutral` — REAL fusd-core positions in five preference
//!    states (counted / unset / stale-nonce / omitted-from-window / wallet-and-LP-held fuSOL
//!    with no position at all) drive PLAN-DIRECTED: only the countable ink becomes
//!    `total_directed_shares`; every uncounted lamport of fuSOL value stays neutral (the plan
//!    never directs supply it did not certify this epoch).
//! 5. `partial_fusol_liquidation_needs_no_controller_account` — a borrower funded with REAL
//!    controller-minted fuSOL, borrowing fUSD, is driven under-MCR by a SOL/USD dip (which,
//!    unlike a pool-NAV drop, arms no liquidation grace — FUSOL-05) and liquidated. The
//!    `liquidate` instruction is asserted to touch NO controller program, PDA, or pool
//!    account: fuSOL debt paths never depend on the allocation controller being live.
//!
//! ## Residual §17.2 rows NOT covered here, and WHY
//!
//! - **Hundreds of validators across multiple batches** and the **sub-cap equal-tranche split
//!   with rotating integer-remainder assignment**: with the fixed 2%-of-total per-validator
//!   cap, the neutral pool can only fall below aggregate Active capacity when there are >49
//!   Active validators (productive ≈ 98% of total vs `N · 2%`). Exercising the tranche/rotation
//!   arithmetic therefore requires ~50+ real Active validators across multi-batch neutral
//!   rounds — each needing a real vote account, stake account, and a multi-epoch admission +
//!   promotion. That is the audit's own "hundreds of validators, adversarial maximum-size"
//!   row; the `fusion_stake_math::targets` round math (tranche, remainder, rotation) is
//!   unit-tested and Kani-proved in the math crate. Here the equal-by-saturation split (test 1)
//!   and the conservation identity (tests 1-3) exercise the on-chain wiring at the feasible
//!   scale.
//! - **Global greatest-deficit selection / rotating ties in REBALANCE**: the engine implements
//!   epoch-rotated two-pass CURSOR order, a recorded, documented deviation from the spec's
//!   global-priority intent (see `execute_next_action.rs` and `docs/stake-pool/README.md`);
//!   there is no greatest-deficit ordering to assert.
//! - **Full commission / liveness-guard / gradual-drain / removal lifecycle** and **reserve
//!   exhaustion → withdrawal → replenishment → resumed increases** and **multi-keeper cursor
//!   races**: multi-epoch lifecycle/keeper choreography; the increase/merge/promotion mechanics
//!   are covered by `litesvm_fusol_epoch_machine.rs`, the one-reward-per-completed-task budget
//!   accounting by its scenario 4, and the preference-countability clauses by
//!   `litesvm_fusol_preferences.rs`.
//! - **Negative-NAV liquidation grace** is covered end-to-end by
//!   `litesvm_fusol_oracle.rs::nav_drop_liquidation_blocked_through_grace_boundary`; test 5
//!   here deliberately uses the SOL/USD-dip path (no grace) to isolate the controller-
//!   independence claim.
//!
//! Requires the dev-oracle `.so` set + the dumped fixture:
//! `anchor build -- --features dev-oracle` and `bash scripts/fetch-spl-stake-pool.sh`.

use fusd_core::constants::PYTH_SOL_USD_FEED_ID;
use fusd_integration_tests::*;
use fusion_stake_controller::constants::{
    ACTIVE_VALIDATOR_CAP_BPS, CANDIDATE_HEALTHY_EPOCHS, RESERVE_MINIMUM_LAMPORTS,
    RESERVE_TARGET_BPS,
};
use fusion_stake_controller::state::{PHASE_PLAN_FINALIZE, PHASE_PLAN_NEUTRAL, PHASE_REBALANCE};
use litesvm::LiteSVM;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const SOL: u64 = 1_000_000_000;
const STATUS_ACTIVE: u8 = 2;
const PYTH_EXPO: i32 = -8;

fn bps_of(amount: u64, bps: u64) -> u64 {
    (u128::from(amount) * u128::from(bps) / 10_000) as u64
}
fn active_cap(total: u64) -> u64 {
    bps_of(total, ACTIVE_VALIDATOR_CAP_BPS)
}
/// `fusion_stake_math::reserve::reserve_target`: `min(total, max(min, bps))`.
fn reserve_target_ref(total: u64) -> u64 {
    total.min(RESERVE_MINIMUM_LAMPORTS.max(bps_of(total, RESERVE_TARGET_BPS)))
}

// ============================ allocation fixture ============================

struct Alloc {
    svm: LiteSVM,
    gov: Keypair,
    g: PoolGenesis,
    crank: Pubkey,
    /// Active pool validators (list indices 0..n).
    votes: Vec<Pubkey>,
}

/// The pool bulk size — big enough that each validator's activation floor clears
/// `MIN_ACTIVATION_TARGET_LAMPORTS` (500 SOL) yet the pool math stays exact.
const BULK_SOL: u64 = 40_000;

/// Genesis + bulk deposit + `n` real validators driven to ACTIVE. The admission (epoch 1) uses
/// synthesized `directed_shares` (the preference → snapshot path is covered in
/// `litesvm_fusol_preferences.rs`; here we only need admitted validators), and the Candidate →
/// Active promotion is reached by fast-forwarding each record's healthy streak before the
/// epoch-2 plan (the exact streak the real machine accumulates over `CANDIDATE_HEALTHY_EPOCHS`
/// epochs, proved organically in `litesvm_fusol_epoch_machine.rs`). Every `AddValidatorToPool`,
/// reconcile, and rebalance CPI is REAL. Returns at controller IDLE, cluster epoch 2, all
/// validators Active with a live list slot.
fn alloc_actives(n: usize) -> Alloc {
    let mut svm = new_svm_full();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 2_000);
    let g = pool_genesis(&mut svm, &gov);
    let crank = create_ata_and_fund(&mut svm, &gov, &gov.pubkey(), &g.fusol_mint, None, 0);

    let bulk = Keypair::new();
    airdrop_sol(&mut svm, &bulk.pubkey(), BULK_SOL + 100);
    let bulk_ata = create_ata_and_fund(&mut svm, &bulk, &bulk.pubkey(), &g.fusol_mint, None, 0);
    send(&mut svm, &[ctrl_deposit_sol_ix(&bulk.pubkey(), &g, &bulk_ata, BULK_SOL * SOL)], &bulk, &[])
        .expect("bulk deposit_sol");

    let votes: Vec<Pubkey> = (0..n).map(|_| register_healthy_validator(&mut svm, &gov, 5)).collect();

    // --- epoch 1: admit all as Candidates (synthesized directed shares) + real adds -----------
    warp_epochs(&mut svm, 1);
    send(&mut svm, &[ctrl_start_epoch_ix()], &gov, &[]).expect("start_epoch (1)");
    send(&mut svm, &[ctrl_reconcile_batch_ix(&g, &crank, &[])], &gov, &[]).expect("reconcile (1)");
    send(&mut svm, &[ctrl_finalize_pool_ix(&g, &crank)], &gov, &[]).expect("finalize_pool (1)");
    for v in &votes {
        let mut rec = read_validator_record(&svm, v);
        rec.directed_shares = 600 * SOL; // raw target ≈ 588 SOL ≥ the 500 SOL activation floor
        rec.directed_shares_epoch = 1;
        overwrite_anchor_account(&mut svm, validator_record_pda(v), &rec);
    }
    ctrl_close_window_at_deadline(&mut svm, &gov);
    send(&mut svm, &[ctrl_plan_directed_batch_ix(&g, &crank, &plan_pairs(&votes))], &gov, &[])
        .expect("plan-directed (admission extras)");
    send(&mut svm, &[ctrl_plan_neutral_batch_ix(&g, &crank, &[])], &gov, &[])
        .expect("plan-neutral (no Active capacity yet)");
    send(&mut svm, &[ctrl_finalize_plan_ix(&g, &crank)], &gov, &[]).expect("finalize_plan (1)");
    for v in &votes {
        execute_admission_add(&mut svm, &gov, &g, &crank, v);
    }
    send(&mut svm, &[ctrl_finish_epoch_ix(&g, &crank)], &gov, &[]).expect("finish_epoch (1)");
    assert_eq!(read_fork_validator_list_len(&svm, &g.validator_list), n as u32);

    // --- epoch 2: promote Candidate → Active (fast-forward the healthy streak) -----------------
    warp_epochs(&mut svm, 1);
    send(&mut svm, &[ctrl_start_epoch_ix()], &gov, &[]).expect("start_epoch (2)");
    let quads = reconcile_quads(&svm, &g, 0, n as u32);
    send(&mut svm, &[ctrl_reconcile_batch_ix(&g, &crank, &quads)], &gov, &[]).expect("reconcile (2)");
    send(&mut svm, &[ctrl_finalize_pool_ix(&g, &crank)], &gov, &[]).expect("finalize_pool (2)");
    // reconcile stamped this epoch's healthy observation; nudge the streak so PLAN-DIRECTED's
    // advance_lifecycle promotes (streak+1 ≥ CANDIDATE_HEALTHY_EPOCHS). Observation fields are
    // preserved (the whole record is read, only the streak changed).
    for v in &votes {
        let mut rec = read_validator_record(&svm, v);
        rec.consecutive_healthy_epochs = CANDIDATE_HEALTHY_EPOCHS - 1;
        overwrite_anchor_account(&mut svm, validator_record_pda(v), &rec);
    }
    ctrl_close_window_at_deadline(&mut svm, &gov);
    send(&mut svm, &[ctrl_plan_directed_batch_ix(&g, &crank, &plan_pairs(&votes))], &gov, &[])
        .expect("plan-directed (2): promotion");
    send(&mut svm, &[ctrl_plan_neutral_batch_ix(&g, &crank, &neutral_records(&votes))], &gov, &[])
        .expect("plan-neutral (2)");
    send(&mut svm, &[ctrl_finalize_plan_ix(&g, &crank)], &gov, &[]).expect("finalize_plan (2)");
    run_rebalance_walk(&mut svm, &gov, &g, &crank);
    send(&mut svm, &[ctrl_finish_epoch_ix(&g, &crank)], &gov, &[]).expect("finish_epoch (2)");

    for v in &votes {
        assert_eq!(read_validator_record(&svm, v).status, STATUS_ACTIVE, "validator promoted");
    }
    Alloc { svm, gov, g, crank, votes }
}

/// Drive one allocation epoch through PLAN-DIRECTED only, applying `directed` shares this epoch
/// (synthesized on the records after reconcile). Leaves the machine in PLAN-NEUTRAL so the
/// caller can inspect the INITIAL plan aggregates — `neutral_total` here is `productive −
/// Σ directed`, before `plan_neutral` consumes it. `run_plan_neutral` then distributes it.
fn plan_directed_step(a: &mut Alloc, directed: &[(Pubkey, u64)]) {
    let n = a.votes.len() as u32;
    warp_epochs(&mut a.svm, 1);
    send(&mut a.svm, &[ctrl_start_epoch_ix()], &a.gov, &[]).expect("start_epoch");
    let quads = reconcile_quads(&a.svm, &a.g, 0, n);
    send(&mut a.svm, &[ctrl_reconcile_batch_ix(&a.g, &a.crank, &quads)], &a.gov, &[])
        .expect("reconcile");
    send(&mut a.svm, &[ctrl_finalize_pool_ix(&a.g, &a.crank)], &a.gov, &[]).expect("finalize_pool");
    let epoch = read_epoch_state(&a.svm).controller_epoch;
    for (vote, shares) in directed {
        let mut rec = read_validator_record(&a.svm, vote);
        rec.directed_shares = *shares;
        rec.directed_shares_epoch = epoch;
        overwrite_anchor_account(&mut a.svm, validator_record_pda(vote), &rec);
    }
    ctrl_close_window_at_deadline(&mut a.svm, &a.gov);
    send(&mut a.svm, &[ctrl_plan_directed_batch_ix(&a.g, &a.crank, &plan_pairs(&a.votes))], &a.gov, &[])
        .expect("plan-directed");
    assert_eq!(read_epoch_state(&a.svm).phase, PHASE_PLAN_NEUTRAL, "directed complete");
}

/// Distribute the neutral pool (PLAN-NEUTRAL). After this, `neutral_granted_total` /
/// `capacity_shortfall` are final and `neutral_total` holds the un-absorbable remainder.
fn run_plan_neutral(a: &mut Alloc) {
    send(
        &mut a.svm,
        &[ctrl_plan_neutral_batch_ix(&a.g, &a.crank, &neutral_records(&a.votes))],
        &a.gov,
        &[],
    )
    .expect("plan-neutral");
    assert_eq!(read_epoch_state(&a.svm).phase, PHASE_PLAN_FINALIZE, "neutral complete, pre-finalize");
}

/// finalize_plan (proves conservation on-chain) + drain the rebalance walk + finish_epoch.
fn finish_alloc_epoch(a: &mut Alloc) {
    send(&mut a.svm, &[ctrl_finalize_plan_ix(&a.g, &a.crank)], &a.gov, &[]).expect("finalize_plan");
    assert_eq!(read_epoch_state(&a.svm).phase, PHASE_REBALANCE);
    run_rebalance_walk(&mut a.svm, &a.gov, &a.g, &a.crank);
    send(&mut a.svm, &[ctrl_finish_epoch_ix(&a.g, &a.crank)], &a.gov, &[]).expect("finish_epoch");
}

// =====================================================================================
// 1. multi-Active, all-undirected — equal (saturation) neutral allocation
// =====================================================================================

#[test]
fn multi_active_all_undirected_equal_neutral() {
    let mut a = alloc_actives(3);
    plan_directed_step(&mut a, &[]); // no direction this epoch

    // After PLAN-DIRECTED: all-undirected ⇒ the whole productive pool is the neutral pool.
    let es = read_epoch_state(&a.svm);
    let total = es.nav_total_lamports;
    let cap = active_cap(total);
    let productive = es.productive_lamports;
    assert_eq!(productive, total - reserve_target_ref(total), "productive = total − reserve");
    assert_eq!(es.total_directed_shares, 0);
    assert_eq!(es.sum_directed_targets, 0);
    assert_eq!(es.neutral_total, productive, "all-undirected: the whole productive pool is neutral");
    assert_eq!(es.unsaturated_active_count, 3, "three Actives expose neutral capacity");
    for v in &a.votes {
        let rec = read_validator_record(&a.svm, v);
        assert_eq!(rec.status, STATUS_ACTIVE);
        assert_eq!(rec.directed_target, 0, "undirected");
        assert_eq!(rec.remaining_capacity, cap, "Active exposes the full cap as neutral capacity");
    }

    // After PLAN-NEUTRAL: every Active saturates at the SAME cap (equal allocation); the neutral
    // pool structurally exceeds 3·cap, so the remainder is aggregate-capacity shortfall.
    run_plan_neutral(&mut a);
    let es = read_epoch_state(&a.svm);
    for v in &a.votes {
        let rec = read_validator_record(&a.svm, v);
        assert_eq!(rec.neutral_granted, cap, "each Active saturated at exactly active_cap");
        assert_eq!(rec.final_target, cap);
        assert_eq!(rec.remaining_capacity, 0, "saturated");
    }
    assert_eq!(es.neutral_granted_total, 3 * cap, "equal grants sum to 3·cap");
    assert_eq!(es.capacity_shortfall, productive - 3 * cap, "the un-absorbable remainder");
    // The on-chain conservation identity (finalize_plan reproves it).
    assert_eq!(es.sum_directed_targets + es.neutral_granted_total + es.capacity_shortfall, productive);
    finish_alloc_epoch(&mut a);
}

// =====================================================================================
// 2. mixed directed + neutral — conservation across the mix
// =====================================================================================

#[test]
fn mixed_directed_and_neutral_conserves() {
    let mut a = alloc_actives(3);
    let (v0, v1, v2) = (a.votes[0], a.votes[1], a.votes[2]);

    // Direct a BELOW-cap floor to v0 only: raw target = productive·shares/supply must land
    // under active_cap. With a ~40k pool, supply ≈ 40,001 SOL and cap ≈ 800 SOL; 600 SOL of
    // shares gives raw ≈ 588 SOL < cap.
    plan_directed_step(&mut a, &[(v0, 600 * SOL)]);

    let es = read_epoch_state(&a.svm);
    let total = es.nav_total_lamports;
    let cap = active_cap(total);
    let productive = es.productive_lamports;
    let supply = es.nav_fusol_supply;

    let rec0 = read_validator_record(&a.svm, &v0);
    let raw0 = (u128::from(productive) * u128::from(600 * SOL) / u128::from(supply)) as u64;
    assert!(raw0 < cap, "the directed floor must be BELOW the cap (a genuine mix)");
    assert_eq!(rec0.directed_target, raw0, "v0 claims its directed floor first");
    assert_eq!(es.total_directed_shares, 600 * SOL);
    assert_eq!(es.sum_directed_targets, raw0);
    assert_eq!(es.neutral_total, productive - raw0, "neutral = productive − the directed floor");
    // v0 keeps `cap − raw0` neutral capacity; v1/v2 keep the full cap.
    assert_eq!(rec0.remaining_capacity, cap - raw0, "v0 exposes cap − directed as neutral capacity");

    // PLAN-NEUTRAL: all three still saturate (neutral ≫ total capacity), each ending at its cap.
    run_plan_neutral(&mut a);
    let es = read_epoch_state(&a.svm);
    let rec0 = read_validator_record(&a.svm, &v0);
    assert_eq!(rec0.neutral_granted, cap - raw0, "v0's neutral top-up fills it to cap");
    assert_eq!(rec0.final_target, cap, "v0 = directed floor + neutral top-up = cap");
    for v in [&v1, &v2] {
        let rec = read_validator_record(&a.svm, v);
        assert_eq!(rec.directed_target, 0);
        assert_eq!(rec.neutral_granted, cap);
        assert_eq!(rec.final_target, cap);
    }
    // neutral grants: v0's top-up (cap−raw0) + v1 + v2 (cap each).
    assert_eq!(es.neutral_granted_total, (cap - raw0) + 2 * cap);
    assert_eq!(es.capacity_shortfall, (productive - raw0) - es.neutral_granted_total);
    // Full conservation identity.
    assert_eq!(es.sum_directed_targets + es.neutral_granted_total + es.capacity_shortfall, productive);
    finish_alloc_epoch(&mut a);
}

// =====================================================================================
// 3. cap-clipped direction redistributed to other Actives
// =====================================================================================

#[test]
fn cap_clipped_direction_redistributes_to_other_actives() {
    let mut a = alloc_actives(3);
    let (v0, v1, v2) = (a.votes[0], a.votes[1], a.votes[2]);

    // Direct FAR more than v0's cap (2000 SOL of shares → raw ≈ 1960 SOL vs ~800 SOL cap), but
    // keep total directed shares ≤ supply (the D ≤ S plan guard).
    plan_directed_step(&mut a, &[(v0, 2_000 * SOL)]);

    let es = read_epoch_state(&a.svm);
    let total = es.nav_total_lamports;
    let cap = active_cap(total);
    let productive = es.productive_lamports;
    let supply = es.nav_fusol_supply;

    let raw0 = (u128::from(productive) * u128::from(2_000 * SOL) / u128::from(supply)) as u64;
    assert!(raw0 > cap, "the scenario must actually exceed the cap");

    let rec0 = read_validator_record(&a.svm, &v0);
    assert_eq!(rec0.directed_target, cap, "the directed floor is CLIPPED to active_cap");
    assert_eq!(rec0.remaining_capacity, 0, "a cap-filled validator exposes no neutral capacity");
    assert_eq!(rec0.final_target, cap);
    // The clipped excess (raw0 − cap) stayed in the neutral pool: sum_directed_targets counts
    // only the clipped cap, so neutral = productive − cap (NOT productive − raw0).
    assert_eq!(es.sum_directed_targets, cap, "only the clipped target is charged to directed");
    assert_eq!(es.neutral_total, productive - cap, "the clipped excess stays in the neutral pool");
    assert_eq!(es.unsaturated_active_count, 2, "only v1/v2 expose neutral capacity");

    // PLAN-NEUTRAL redistributes the clipped excess to the OTHER Actives (v0 is already full).
    run_plan_neutral(&mut a);
    let es = read_epoch_state(&a.svm);
    assert_eq!(
        read_validator_record(&a.svm, &v0).neutral_granted,
        0,
        "the cap-filled validator receives NONE of the redistributed neutral"
    );
    for v in [&v1, &v2] {
        let rec = read_validator_record(&a.svm, v);
        assert_eq!(rec.directed_target, 0);
        assert_eq!(rec.neutral_granted, cap, "the other Actives absorb the redistributed neutral");
        assert_eq!(rec.final_target, cap);
    }
    assert_eq!(es.neutral_granted_total, 2 * cap);
    assert_eq!(es.sum_directed_targets + es.neutral_granted_total + es.capacity_shortfall, productive);
    finish_alloc_epoch(&mut a);
}

// =====================================================================================
// 4. uncounted preferences (wallet / LP / unset / stale / omitted) enter neutral
// =====================================================================================

#[test]
fn uncounted_preferences_enter_neutral() {
    // A canonical-primary fuSOL market so REAL positions carry ink; one Active validator to
    // receive whatever direction IS certified.
    let mut a = alloc_actives(1);
    let v0 = a.votes[0];
    let coll = a.g.fusol_mint;

    // fusd-core protocol + market on the REAL fuSOL mint (no oracle/price needed: open_position
    // + deposit require none).
    set_program_upgrade_authority(&mut a.svm, &a.gov.pubkey());
    send(&mut a.svm, &[init_protocol_ix(&a.gov.pubkey())], &a.gov, &[]).expect("init_protocol");
    send(
        &mut a.svm,
        &[init_market_ix(&a.gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, 0, 0, BUCKET_WIDTH_BPS, 0)],
        &a.gov,
        &[],
    )
    .expect("init_market on the fuSOL mint");

    // Five holders, each funded with REAL fuSOL through the controller's deposit_sol:
    //   counted  — sets + snapshots a valid preference (the only directed weight)
    //   unset    — a position with NO preference
    //   stale    — a preference invalidated by a post-set collateral withdraw (nonce bump)
    //   omitted  — a valid preference that is simply never snapshotted this window
    //   wallet   — fuSOL held in a wallet with NO fusd-core position at all (and, equivalently,
    //              the "LP-held" case: fuSOL parked outside any position never directs)
    let counted = fusol_holder(&mut a, &coll, 10);
    let unset = fusol_holder(&mut a, &coll, 7);
    let stale = fusol_holder(&mut a, &coll, 5);
    let omitted = fusol_holder(&mut a, &coll, 4);
    let _wallet = fusol_holder_wallet_only(&mut a, &coll, 9); // never opens a position

    // Positions + ink for the four position-holders.
    for h in [&counted, &unset, &stale, &omitted] {
        send(&mut a.svm, &[open_position_ix(&h.kp.pubkey(), &coll, 500)], &h.kp, &[])
            .expect("open_position");
        send(&mut a.svm, &[deposit_ix(&h.kp.pubkey(), &coll, &h.ata, h.deposit)], &h.kp, &[])
            .expect("deposit fuSOL");
    }
    // counted / stale / omitted set a preference to v0 in epoch 2 (unset never does).
    for h in [&counted, &stale, &omitted] {
        send(
            &mut a.svm,
            &[ctrl_set_preference_ix(&h.kp.pubkey(), &position_pda(&coll, &h.kp.pubkey()), &v0)],
            &h.kp,
            &[],
        )
        .expect("set_preference");
    }

    // Epoch 3 window: snapshot only `counted`. `stale` gets its collateral changed (nonce bump)
    // then a rejected snapshot; `omitted` is simply never snapshotted.
    let n = a.votes.len() as u32;
    warp_epochs(&mut a.svm, 1);
    send(&mut a.svm, &[ctrl_start_epoch_ix()], &a.gov, &[]).expect("start_epoch (3)");
    let quads = reconcile_quads(&a.svm, &a.g, 0, n);
    send(&mut a.svm, &[ctrl_reconcile_batch_ix(&a.g, &a.crank, &quads)], &a.gov, &[])
        .expect("reconcile (3)");
    send(&mut a.svm, &[ctrl_finalize_pool_ix(&a.g, &a.crank)], &a.gov, &[]).expect("finalize_pool (3)");

    send(
        &mut a.svm,
        &[ctrl_snapshot_preference_ix(&position_pda(&coll, &counted.kp.pubkey()), &v0)],
        &a.gov,
        &[],
    )
    .expect("the counted preference lands");

    // stale: a debt-free withdraw bumps ink_nonce, invalidating the recorded direction.
    send(
        &mut a.svm,
        &[withdraw_ix(&stale.kp.pubkey(), &coll, &stale.ata, SOL)],
        &stale.kp,
        &[],
    )
    .expect("debt-free withdraw (nonce bump)");
    let f = send(
        &mut a.svm,
        &[ctrl_snapshot_preference_ix(&position_pda(&coll, &stale.kp.pubkey()), &v0)],
        &a.gov,
        &[],
    )
    .expect_err("a stale-nonce preference must not count");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_NOT_COUNTABLE);

    // Only `counted`'s ink reached v0's epoch-stamped record.
    let rec = read_validator_record(&a.svm, &v0);
    assert_eq!(rec.directed_shares, 10 * SOL, "only the counted 10 fuSOL directs");
    assert_eq!(rec.directed_shares_epoch, read_epoch_state(&a.svm).controller_epoch);

    // PLAN-DIRECTED: total_directed_shares == ONLY the counted ink; every uncounted lamport of
    // fuSOL (unset 7 + stale 5 + omitted 4 + wallet 9, plus all undirected supply) is neutral.
    ctrl_close_window_at_deadline(&mut a.svm, &a.gov);
    send(&mut a.svm, &[ctrl_plan_directed_batch_ix(&a.g, &a.crank, &plan_pairs(&a.votes))], &a.gov, &[])
        .expect("plan-directed (3)");
    let es = read_epoch_state(&a.svm);
    assert_eq!(es.total_directed_shares, 10 * SOL, "unset/stale/omitted/wallet never direct");
    let cap = active_cap(es.nav_total_lamports);
    let raw = (u128::from(es.productive_lamports) * u128::from(10 * SOL)
        / u128::from(es.nav_fusol_supply)) as u64;
    let directed = raw.min(cap);
    assert_eq!(es.sum_directed_targets, directed);
    assert_eq!(
        es.neutral_total,
        es.productive_lamports - directed,
        "all uncounted fuSOL value stays in the neutral pool"
    );
    send(
        &mut a.svm,
        &[ctrl_plan_neutral_batch_ix(&a.g, &a.crank, &neutral_records(&a.votes))],
        &a.gov,
        &[],
    )
    .expect("plan-neutral (3)");
    finish_alloc_epoch(&mut a);
}

// =====================================================================================
// 5. partial fuSOL liquidation carries no controller account
// =====================================================================================

#[test]
fn partial_fusol_liquidation_needs_no_controller_account() {
    // Real controller pool + a canonical-primary fusd-core market whose collateral IS the
    // controller-minted fuSOL.
    let mut a = alloc_actives(1);
    let coll = a.g.fusol_mint;
    let market = market_pda(&coll);

    set_program_upgrade_authority(&mut a.svm, &a.gov.pubkey());
    send(&mut a.svm, &[init_protocol_ix(&a.gov.pubkey())], &a.gov, &[]).expect("init_protocol");
    send(
        &mut a.svm,
        &[init_market_ix(&a.gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, 0, 0, BUCKET_WIDTH_BPS, 0)],
        &a.gov,
        &[],
    )
    .expect("init_market");
    send(&mut a.svm, &[init_reactor_pool_ix(&a.gov.pubkey(), &coll)], &a.gov, &[])
        .expect("init_reactor_pool");
    send(&mut a.svm, &[init_insurance_buffer_ix(&a.gov.pubkey(), &coll)], &a.gov, &[])
        .expect("init_insurance_buffer");

    // Canonical-primary oracle bound to the REAL pool; crank it at SOL/USD $100 × rate 1 →
    // debt_spot $100 (10% haircut on the mint leg only).
    let quote = create_quote_mint(&mut a.svm, &a.gov, FUSD_DECIMALS);
    let pyth = Pubkey::new_unique();
    let sb = Pubkey::new_unique();
    let mut args = default_oracle_args();
    args.pyth_feed_id = PYTH_SOL_USD_FEED_ID;
    args.switchboard_feed = sb;
    args.orca_pool = Pubkey::default();
    args.raydium_pool = Pubkey::default();
    args.lst_stake_pool = a.g.stake_pool;
    args.canonical_primary = true;
    args.liquidity_haircut_bps = 1_000;
    send(&mut a.svm, &[init_market_oracle_ix(&a.gov.pubkey(), &coll, &quote, args)], &a.gov, &[])
        .expect("init_market_oracle (canonical-primary, REAL pool)");
    crank_fusol_oracle(&mut a, &coll, &pyth, &sb, 100);
    assert_eq!(read_market(&a.svm, &market).debt_spot, spot_for_usd(100), "SOL $100 × rate 1");

    // Reactor Pool depositor (absorbs the liquidation) + borrower B: 10 fuSOL vs $500 debt —
    // healthy at debt_spot $100 (10·100 = $1000 ≥ 500·1.5 = $750), under-MCR at $60.
    let d = fusol_borrower(&mut a, &coll, 100, usd(500));
    provide_sp(&mut a.svm, &to_actor(&d), &coll, usd(500));
    let b = fusol_borrower(&mut a, &coll, 10, usd(500));

    // A SOL/USD dip $100 → $60 (NOT a pool-NAV drop) pushes B under-MCR WITHOUT arming any
    // liquidation grace (FUSOL-05: grace keys on the pool RATE only).
    crank_fusol_oracle(&mut a, &coll, &pyth, &sb, 60);
    let m = read_market(&a.svm, &market);
    assert_eq!(m.debt_spot, spot_for_usd(60), "10·60 = $600 < $750 → under-MCR");
    assert_eq!(m.liq_grace_until, 0, "a SOL/USD dip arms no NAV-decrease grace");

    // THE assertion: the liquidate instruction touches NO controller program, PDA, or pool
    // account — fuSOL debt paths never require the allocation controller to be live.
    let b_position = position_pda(&coll, &b.kp.pubkey());
    let liq_ata = ata(&a.gov.pubkey(), &coll);
    let ix = liquidate_ix(&a.gov.pubkey(), &coll, &b_position, &liq_ata);
    assert_eq!(ix.program_id, fusd_core::ID, "liquidation is a pure fusd-core instruction");
    let controller_touchpoints = [
        fusion_stake_controller::ID,
        a.g.config,
        a.g.epoch_state,
        a.g.stake_pool,
        a.g.validator_list,
        a.g.reserve_stake,
        a.g.pool_authority,
        a.g.deposit_authority,
        a.g.maintenance_authority,
        a.g.maintenance_vault,
        a.g.pool_withdraw_authority,
    ];
    for meta in &ix.accounts {
        assert!(
            !controller_touchpoints.contains(&meta.pubkey),
            "liquidate must not carry the controller account {}",
            meta.pubkey
        );
    }

    // And it actually liquidates the fuSOL-collateral position.
    let pre = read_position(&a.svm, &b_position);
    assert_eq!(pre.recorded_debt, usd(500) as u128);
    liquidate(&mut a.svm, &a.gov, &coll, &b_position).expect("fuSOL liquidation with no controller");
    let post = read_position(&a.svm, &b_position);
    assert_eq!(post.recorded_debt, 0, "debt cleared");
    assert_eq!(post.ink, 0, "collateral seized");
    // The controller pool is entirely unaffected by the fusd-core liquidation.
    assert_eq!(read_validator_record(&a.svm, &a.votes[0]).status, STATUS_ACTIVE);
}

// ============================ fuSOL borrower/holder helpers ============================

struct Holder {
    kp: Keypair,
    ata: Pubkey,
    /// fuSOL collateral to deposit (native).
    deposit: u64,
}

/// Fund a fresh holder with `sol` whole fuSOL through the controller's `deposit_sol` (+1 SOL
/// headroom covers the 5 bps deposit fee). Opens no position.
fn fusol_holder_wallet_only(a: &mut Alloc, coll: &Pubkey, sol: u64) -> Holder {
    let kp = Keypair::new();
    airdrop_sol(&mut a.svm, &kp.pubkey(), sol + 20);
    let ata = create_ata_and_fund(&mut a.svm, &kp, &kp.pubkey(), coll, None, 0);
    send(
        &mut a.svm,
        &[ctrl_deposit_sol_ix(&kp.pubkey(), &a.g, &ata, (sol + 1) * SOL)],
        &kp,
        &[],
    )
    .expect("deposit_sol for the holder");
    Holder { kp, ata, deposit: sol * SOL }
}

/// As [`fusol_holder_wallet_only`]; the caller opens the position + deposits `deposit`.
fn fusol_holder(a: &mut Alloc, coll: &Pubkey, sol: u64) -> Holder {
    fusol_holder_wallet_only(a, coll, sol)
}

/// A fuSOL borrower: fund via deposit_sol, open a REAL fusd-core position, deposit `sol` whole
/// fuSOL, and (if `borrow > 0`) borrow that much fUSD.
fn fusol_borrower(a: &mut Alloc, coll: &Pubkey, sol: u64, borrow: u64) -> Holder {
    let h = fusol_holder_wallet_only(a, coll, sol);
    send(&mut a.svm, &[open_position_ix(&h.kp.pubkey(), coll, 500)], &h.kp, &[])
        .expect("open_position");
    send(&mut a.svm, &[deposit_ix(&h.kp.pubkey(), coll, &h.ata, h.deposit)], &h.kp, &[])
        .expect("deposit fuSOL collateral");
    if borrow > 0 {
        let fusd_ata = create_ata_and_fund(&mut a.svm, &h.kp, &h.kp.pubkey(), &fusd_mint_pda(), None, 0);
        send(&mut a.svm, &[borrow_ix(&h.kp.pubkey(), coll, &fusd_ata, borrow)], &h.kp, &[])
            .expect("borrow fUSD");
    }
    h
}

/// Adapt a `Holder` to the harness `Actor` shape `provide_sp` expects (it reads `kp`/`fusd_ata`;
/// the Reactor deposit is funded from the holder's own fUSD ATA).
fn to_actor(h: &Holder) -> Actor {
    Actor {
        kp: h.kp.insecure_clone(),
        position: Pubkey::default(),
        coll_ata: h.ata,
        fusd_ata: ata(&h.kp.pubkey(), &fusd_mint_pda()),
    }
}

/// Post fresh SOL/USD legs (conf 0 ⇒ exact composed price) and crank the canonical-primary
/// fuSOL oracle against the REAL pool.
fn crank_fusol_oracle(a: &mut Alloc, coll: &Pubkey, pyth: &Pubkey, sb: &Pubkey, sol_usd: i64) {
    let now = now_unix(&a.svm);
    set_pyth_price(&mut a.svm, pyth, PYTH_SOL_USD_FEED_ID, sol_usd * 100_000_000, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut a.svm, sb, sol_usd as i128 * 1_000_000_000_000_000_000, 0, 1, now);
    // Two identical-price cranks would collide on litesvm's static blockhash (AlreadyProcessed).
    a.svm.expire_blockhash();
    send(
        &mut a.svm,
        &[update_price_lst_ix(&a.gov.pubkey(), coll, pyth, Some(*sb), None, Some(a.g.stake_pool))],
        &a.gov,
        &[],
    )
    .expect("update_price (canonical-primary, REAL pool)");
}
