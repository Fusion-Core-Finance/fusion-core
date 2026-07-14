//! Preference lifecycle end-to-end: REAL fusd-core `Position`s in a REAL canonical-primary
//! fuSOL market, against the REAL controller + mainnet-dumped stake-pool processor.
//!
//! The full stack per test: `pool_genesis` (controller + fork-owned pool, rate exactly 1) →
//! fusd-core protocol + market whose collateral IS the genesis fuSOL mint → canonical-primary
//! oracle bound to the REAL pool account → `update_price` composes `SOL/USD × pool rate` →
//! open_position / deposit fuSOL / borrow. On that stack:
//!
//! 1. `set_preference` — happy-path recording, owner gate, once-per-epoch change limit, the
//!    close-side change-limit guard (close+recreate cannot reset the limit), Draining reject.
//! 2. `snapshot_preference` — Preferences-window gating on both sides of the deadline slot, and
//!    every countable clause: wrong owner, nonce mismatch after a REAL fusd-core withdraw (the
//!    `ink_nonce` integration property of the whole design), eligibility delay, double-count.
//! 3. `sync_preference` — permissionless re-arm after a collateral change with the +1 epoch
//!    delay; a no-op sync never resets eligibility (the anti-grief property).
//! 4. `close_preference` — rent pinned to the recorded owner, owner-only while ink is live,
//!    permissionless once the position is emptied.
//! 5. Directed weights flow into the plan: `close_preference_window` + `plan_directed_batch`
//!    accumulate the epoch-stamped `ValidatorRecord.directed_shares` into `EpochState`'s `D`.
//!
//! Requires the dev-oracle `.so` set (`anchor build -- --features dev-oracle`) plus the dumped
//! stake-pool fixture (`bash scripts/fetch-spl-stake-pool.sh`).

use anchor_lang::AccountSerialize;
use fusd_core::constants::PYTH_SOL_USD_FEED_ID;
use fusd_integration_tests::*;
use fusion_stake_controller::events::{
    PreferenceUpdated, PREF_OP_CLOSED, PREF_OP_COUNTED, PREF_OP_SET, PREF_OP_SYNCED,
};
use fusion_stake_controller::state::{PHASE_PLAN_DIRECTED, PHASE_PLAN_NEUTRAL, PHASE_PREFERENCES};
use litesvm::LiteSVM;
use solana_sdk::instruction::AccountMeta;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const LAMPORTS_PER_SOL: u64 = 1_000_000_000;
const PYTH_EXPO: i32 = -8;
/// `ValidatorStatus::Draining` byte (fusion-stake-math lifecycle; not a direct dep of this crate).
const STATUS_DRAINING: u8 = 3;

/// `n` whole fuSOL in base units (9-decimal, like SOL).
fn fusol(n: u64) -> u64 {
    n * LAMPORTS_PER_SOL
}

// ============================ fixture ============================

/// The whole REAL stack: genesis'd pool + controller, fusd-core market on the fuSOL mint,
/// canonical-primary oracle bound to the REAL pool, a funded permissionless cranker, and two
/// registered (real vote account) validators.
struct Stack {
    svm: LiteSVM,
    gov: Keypair,
    g: PoolGenesis,
    /// The market collateral — the REAL fuSOL mint (`g.fusol_mint`).
    coll: Pubkey,
    cranker: Keypair,
    /// The cranker's fuSOL account (crank-reward sink for the batch instructions).
    cranker_fusol: Pubkey,
    vote1: Pubkey,
    vote2: Pubkey,
}

fn stack() -> Stack {
    let mut svm = new_svm_full();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);

    // The REAL pool stack: fuSOL mint / vault / pool / list / reserve + controller genesis.
    let g = pool_genesis(&mut svm, &gov);
    let coll = g.fusol_mint;

    // fusd-core protocol + market whose collateral IS the fuSOL mint (bootstrap_market_full
    // creates its own mint, so its init calls are replicated here against the existing one).
    set_program_upgrade_authority(&mut svm, &gov.pubkey());
    send(&mut svm, &[init_protocol_ix(&gov.pubkey())], &gov, &[]).expect("init_protocol");
    send(
        &mut svm,
        &[init_market_ix(&gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, 0, 0, BUCKET_WIDTH_BPS, 0)],
        &gov,
        &[],
    )
    .expect("init_market on the fuSOL mint");
    send(&mut svm, &[init_reactor_pool_ix(&gov.pubkey(), &coll)], &gov, &[])
        .expect("init_reactor_pool");
    send(&mut svm, &[init_insurance_buffer_ix(&gov.pubkey(), &coll)], &gov, &[])
        .expect("init_insurance_buffer");

    // Canonical-primary oracle bound to the REAL pool account (not a synthesized one): shared
    // SOL/USD Pyth id, no DEX pools, mandatory 10% liquidity haircut.
    let quote = create_quote_mint(&mut svm, &gov, FUSD_DECIMALS);
    let pyth = Pubkey::new_unique();
    let sb = Pubkey::new_unique();
    let mut args = default_oracle_args();
    args.pyth_feed_id = PYTH_SOL_USD_FEED_ID;
    args.switchboard_feed = sb;
    args.orca_pool = Pubkey::default();
    args.raydium_pool = Pubkey::default();
    args.lst_stake_pool = g.stake_pool;
    args.canonical_primary = true;
    args.liquidity_haircut_bps = 1_000; // 10%
    send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args)], &gov, &[])
        .expect("init_market_oracle (canonical-primary, REAL pool)");

    // Price crank off the REAL pool: SOL/USD $100 on both legs (conf 0 → no k·σ haircut), pool
    // rate exactly 1 at genesis, freshly stamped by the real Initialize.
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &pyth, PYTH_SOL_USD_FEED_ID, 100 * 100_000_000, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &sb, 100 * 1_000_000_000_000_000_000, 0, 1, now);
    send(
        &mut svm,
        &[update_price_lst_ix(&gov.pubkey(), &coll, &pyth, Some(sb), None, Some(g.stake_pool))],
        &gov,
        &[],
    )
    .expect("update_price off the REAL fork pool");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(!m.mint_frozen, "both legs agree + pool fresh + corridor optional => mints OPEN");
    assert_eq!(m.spot, spot_for_usd(90), "spot = $100 NAV x rate 1 - 10% liquidity haircut");
    assert_eq!(m.debt_spot, spot_for_usd(100), "debt leg = raw NAV (no haircut)");

    // A permissionless cranker with a fuSOL reward sink for the batch instructions.
    let cranker = Keypair::new();
    airdrop_sol(&mut svm, &cranker.pubkey(), 100);
    let cranker_fusol = create_ata_and_fund(&mut svm, &cranker, &cranker.pubkey(), &coll, None, 0);

    // Two registered validators with REAL vote accounts.
    let vote1 = register_real_validator(&mut svm, &gov);
    let vote2 = register_real_validator(&mut svm, &gov);

    Stack { svm, gov, g, coll, cranker, cranker_fusol, vote1, vote2 }
}

/// Create a REAL vote account (native vote-program builtin) and register its ValidatorRecord.
fn register_real_validator(svm: &mut LiteSVM, payer: &Keypair) -> Pubkey {
    let node = Keypair::new();
    let vote_kp = Keypair::new();
    let vote = create_vote_account(svm, &node, &vote_kp, 5);
    send(svm, &[ctrl_register_validator_ix(&payer.pubkey(), &vote)], payer, &[])
        .expect("register_validator");
    vote
}

/// One fuSOL borrower: SOL through the controller's `deposit_sol` (rate 1, +1 SOL headroom
/// covers the 5 bps fee), open a REAL fusd-core position, deposit `deposit_native` fuSOL, and
/// (optionally) borrow FUSD against it.
struct FusolActor {
    kp: Keypair,
    position: Pubkey,
    fusol_ata: Pubkey,
}

fn fusol_actor(s: &mut Stack, deposit_native: u64, borrow_native: u64) -> FusolActor {
    let kp = Keypair::new();
    airdrop_sol(&mut s.svm, &kp.pubkey(), 100);
    let fusol_ata = create_ata_and_fund(&mut s.svm, &kp, &kp.pubkey(), &s.coll, None, 0);
    send(
        &mut s.svm,
        &[ctrl_deposit_sol_ix(&kp.pubkey(), &s.g, &fusol_ata, deposit_native + LAMPORTS_PER_SOL)],
        &kp,
        &[],
    )
    .expect("deposit_sol through the controller");
    send(&mut s.svm, &[open_position_ix(&kp.pubkey(), &s.coll, 500)], &kp, &[])
        .expect("open_position");
    send(&mut s.svm, &[deposit_ix(&kp.pubkey(), &s.coll, &fusol_ata, deposit_native)], &kp, &[])
        .expect("deposit fuSOL collateral");
    if borrow_native > 0 {
        let fusd_ata =
            create_ata_and_fund(&mut s.svm, &kp, &kp.pubkey(), &fusd_mint_pda(), None, 0);
        send(&mut s.svm, &[borrow_ix(&kp.pubkey(), &s.coll, &fusd_ata, borrow_native)], &kp, &[])
            .expect("borrow FUSD against fuSOL");
    }
    let position = position_pda(&s.coll, &kp.pubkey());
    FusolActor { kp, position, fusol_ata }
}

/// Warp one epoch and drive the crank state machine to the open Preferences window:
/// `start_epoch` → `reconcile_batch` (empty validator list) → `finalize_pool`.
fn crank_to_window(s: &mut Stack) {
    warp_epochs(&mut s.svm, 1);
    send(&mut s.svm, &[ctrl_start_epoch_ix()], &s.cranker, &[]).expect("start_epoch");
    send(&mut s.svm, &[ctrl_reconcile_batch_ix(&s.g, &s.cranker_fusol, &[])], &s.cranker, &[])
        .expect("reconcile_batch (empty list completes the phase)");
    send(&mut s.svm, &[ctrl_finalize_pool_ix(&s.g, &s.cranker_fusol)], &s.cranker, &[])
        .expect("finalize_pool");
    let es = read_epoch_state(&s.svm);
    assert_eq!(es.phase, PHASE_PREFERENCES, "finalize opens the preference window");
    assert_eq!(es.controller_epoch, now_epoch(&s.svm));
    assert!(es.preference_window_close_slot > current_slot(&s.svm), "window has width");
}

/// Overwrite a program-owned account's data with a re-serialized Anchor account value (state
/// synthesis for defensive-clause probes; same lamports/owner, only the bytes change).
fn overwrite_account<T: AccountSerialize>(svm: &mut LiteSVM, addr: Pubkey, value: &T) {
    let mut acct = svm.get_account(&addr).expect("account exists");
    let mut data = Vec::with_capacity(acct.data.len());
    value.try_serialize(&mut data).expect("serialize account");
    assert!(data.len() <= acct.data.len(), "serialized form fits the allocation");
    data.resize(acct.data.len(), 0);
    acct.data = data;
    svm.set_account(addr, acct).expect("set_account");
}

// ============================ 1. set_preference ============================

#[test]
fn set_preference_happy_path_owner_gate_and_change_limits() {
    let mut s = stack();
    // The full CDP flow: fuSOL through the controller, deposit, borrow FUSD against it.
    let alice = fusol_actor(&mut s, fusol(10), usd(100));
    let carol = fusol_actor(&mut s, fusol(2), 0);
    assert_eq!(now_epoch(&s.svm), 0, "everything below happens in the genesis epoch");

    // Happy path: records the live (ink, ink_nonce, owner) triple, delays eligibility +1 epoch.
    let meta = send(
        &mut s.svm,
        &[ctrl_set_preference_ix(&alice.kp.pubkey(), &alice.position, &s.vote1)],
        &alice.kp,
        &[],
    )
    .expect("set_preference (registered validator, real position)");
    let pos = read_position(&s.svm, &alice.position);
    assert_eq!(pos.ink, fusol(10));
    let p = read_preference(&s.svm, &alice.position);
    assert_eq!(p.version, 1);
    assert_eq!(p.fusion_position, alice.position);
    assert_eq!(p.owner, alice.kp.pubkey());
    assert_eq!(p.vote_account, s.vote1);
    assert_eq!(p.observed_ink, pos.ink);
    assert_eq!(p.observed_ink_nonce, pos.ink_nonce);
    assert_eq!(p.eligible_from_epoch, 1, "set in epoch 0 => countable from epoch 1");
    assert_eq!(p.change_epoch, 0);
    assert_eq!(p.last_counted_epoch, 0, "never counted");
    let ev: PreferenceUpdated = single_event(&meta);
    assert_eq!(ev.op, PREF_OP_SET);
    assert_eq!(ev.fusion_position, alice.position);
    assert_eq!(ev.observed_ink, pos.ink);
    assert_eq!(ev.eligible_from_epoch, 1);

    // Owner-only: a stranger signing for Alice's position is rejected against the LIVE
    // position owner.
    let mallory = Keypair::new();
    airdrop_sol(&mut s.svm, &mallory.pubkey(), 10);
    let f = send(
        &mut s.svm,
        &[ctrl_set_preference_ix(&mallory.pubkey(), &alice.position, &s.vote1)],
        &mallory,
        &[],
    )
    .expect_err("non-owner set_preference must fail");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_OWNER_MISMATCH);

    // Once per epoch: re-targeting in the change epoch is rejected.
    let f = send(
        &mut s.svm,
        &[ctrl_set_preference_ix(&alice.kp.pubkey(), &alice.position, &s.vote2)],
        &alice.kp,
        &[],
    )
    .expect_err("second set in the same epoch must fail");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_CHANGE_LIMIT);

    // Close+recreate cannot reset the limit either: a live-ink preference changed THIS epoch
    // refuses to close (the exit-side half of the once-per-epoch rule).
    let f = send(
        &mut s.svm,
        &[ctrl_close_preference_ix(&alice.kp.pubkey(), &alice.position, &alice.kp.pubkey())],
        &alice.kp,
        &[],
    )
    .expect_err("live-ink close in the change epoch must fail");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_CHANGE_LIMIT2);

    // A Draining validator is a dead-end direction (lifecycle cap 0) — rejected up front.
    let vote3 = register_real_validator(&mut s.svm, &s.gov);
    let mut rec = read_validator_record(&s.svm, &vote3);
    rec.status = STATUS_DRAINING;
    overwrite_account(&mut s.svm, validator_record_pda(&vote3), &rec);
    let f = send(
        &mut s.svm,
        &[ctrl_set_preference_ix(&carol.kp.pubkey(), &carol.position, &vote3)],
        &carol.kp,
        &[],
    )
    .expect_err("Draining target must be rejected");
    assert_eq!(custom_code(&f), E_CTRL_VALIDATOR_NOT_ELIGIBLE_FOR_PREFERENCE);
    // The failed set created nothing; a fresh set to an eligible validator then succeeds
    // (fresh init — the change limit binds changes, not first use).
    assert!(
        s.svm.get_account(&preference_pda(&carol.position)).is_none(),
        "rejected set must not leave a preference account behind"
    );
    send(
        &mut s.svm,
        &[ctrl_set_preference_ix(&carol.kp.pubkey(), &carol.position, &s.vote1)],
        &carol.kp,
        &[],
    )
    .expect("fresh set after the rejected Draining attempt");

    // Snapshots are phase-gated: the controller sits in IDLE at genesis — no window, no count.
    let f = send(
        &mut s.svm,
        &[ctrl_snapshot_preference_ix(&alice.position, &s.vote1)],
        &s.cranker,
        &[],
    )
    .expect_err("snapshot outside the Preferences phase must fail");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_WINDOW_CLOSED);
}

// ============================ 2. snapshot_preference + plan-directed D ============================

#[test]
fn snapshot_window_countable_clauses_and_directed_aggregation() {
    let mut s = stack();
    let alice = fusol_actor(&mut s, fusol(10), usd(100)); // counts on vote1
    let bob = fusol_actor(&mut s, fusol(5), 0); // nonce-mismatch probe (vote1, never counts)
    let dave = fusol_actor(&mut s, fusol(3), 0); // wrong-owner probe, then counts on vote2
    let carol = fusol_actor(&mut s, fusol(2), 0); // sets late -> eligibility-delay probe
    for (actor, vote) in [(&alice, &s.vote1), (&bob, &s.vote1), (&dave, &s.vote2)] {
        send(
            &mut s.svm,
            &[ctrl_set_preference_ix(&actor.kp.pubkey(), &actor.position, vote)],
            &actor.kp,
            &[],
        )
        .expect("set_preference in epoch 0");
    }

    // Epoch 1: RECONCILE -> FINALIZE -> the Preferences window opens.
    crank_to_window(&mut s);
    let close_slot = read_epoch_state(&s.svm).preference_window_close_slot;

    // Countable happy path: Alice's live ink lands on vote1's epoch-stamped record.
    let meta = send(
        &mut s.svm,
        &[ctrl_snapshot_preference_ix(&alice.position, &s.vote1)],
        &s.cranker,
        &[],
    )
    .expect("snapshot inside the window");
    let ev: PreferenceUpdated = single_event(&meta);
    assert_eq!(ev.op, PREF_OP_COUNTED);
    assert_eq!(ev.observed_ink, fusol(10));
    let rec = read_validator_record(&s.svm, &s.vote1);
    assert_eq!(rec.directed_shares, fusol(10));
    assert_eq!(rec.directed_shares_epoch, 1);
    assert_eq!(read_preference(&s.svm, &alice.position).last_counted_epoch, 1);

    // Clause: one count per epoch.
    s.svm.expire_blockhash();
    let f = send(
        &mut s.svm,
        &[ctrl_snapshot_preference_ix(&alice.position, &s.vote1)],
        &s.cranker,
        &[],
    )
    .expect_err("double count in one epoch must fail");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_NOT_COUNTABLE);

    // Clause: recorded owner must match the LIVE position owner (probed by patching the
    // preference bytes — no program path can desynchronize them, which is the point).
    let orig = read_preference(&s.svm, &dave.position);
    let mut patched = orig.clone();
    patched.owner = Pubkey::new_unique();
    overwrite_account(&mut s.svm, preference_pda(&dave.position), &patched);
    let f = send(
        &mut s.svm,
        &[ctrl_snapshot_preference_ix(&dave.position, &s.vote2)],
        &s.cranker,
        &[],
    )
    .expect_err("owner mismatch must not count");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_NOT_COUNTABLE);
    overwrite_account(&mut s.svm, preference_pda(&dave.position), &orig);
    s.svm.expire_blockhash();
    send(&mut s.svm, &[ctrl_snapshot_preference_ix(&dave.position, &s.vote2)], &s.cranker, &[])
        .expect("counts once the recorded owner is restored");
    assert_eq!(read_validator_record(&s.svm, &s.vote2).directed_shares, fusol(3));

    // Clause: nonce mismatch after a REAL fusd-core withdraw — THE integration property. The
    // debt-free withdraw needs no oracle and no controller account (exit independence), yet it
    // bumps `Position.ink_nonce` and instantly invalidates the recorded direction.
    let pre = read_position(&s.svm, &bob.position);
    send(
        &mut s.svm,
        &[withdraw_ix(&bob.kp.pubkey(), &s.coll, &bob.fusol_ata, fusol(1))],
        &bob.kp,
        &[],
    )
    .expect("debt-free fusd-core withdraw");
    let post = read_position(&s.svm, &bob.position);
    assert_eq!(post.ink, fusol(4));
    assert_eq!(post.ink_nonce, pre.ink_nonce + 1, "withdraw bumps the collateral nonce");
    assert_ne!(read_preference(&s.svm, &bob.position).observed_ink_nonce, post.ink_nonce);
    let f = send(
        &mut s.svm,
        &[ctrl_snapshot_preference_ix(&bob.position, &s.vote1)],
        &s.cranker,
        &[],
    )
    .expect_err("stale nonce must not count");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_NOT_COUNTABLE);

    // Clause: `eligible_from_epoch` not yet reached — a preference set inside the current
    // epoch counts no earlier than the NEXT one.
    send(
        &mut s.svm,
        &[ctrl_set_preference_ix(&carol.kp.pubkey(), &carol.position, &s.vote2)],
        &carol.kp,
        &[],
    )
    .expect("set during the epoch-1 window");
    assert_eq!(read_preference(&s.svm, &carol.position).eligible_from_epoch, 2);
    let f = send(
        &mut s.svm,
        &[ctrl_snapshot_preference_ix(&carol.position, &s.vote2)],
        &s.cranker,
        &[],
    )
    .expect_err("not yet eligible must not count");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_NOT_COUNTABLE);

    // The window is slot-gated on BOTH sides: it cannot close early, and the deadline slot
    // itself belongs to `close_preference_window`, not to snapshots.
    let f = send(&mut s.svm, &[ctrl_close_preference_window_ix()], &s.cranker, &[])
        .expect_err("close before the deadline must fail");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_WINDOW_STILL_OPEN);
    let to_deadline = close_slot - current_slot(&s.svm);
    warp_slots(&mut s.svm, to_deadline);
    let f = send(
        &mut s.svm,
        &[ctrl_snapshot_preference_ix(&alice.position, &s.vote1)],
        &s.cranker,
        &[],
    )
    .expect_err("snapshot at the deadline slot must fail");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_WINDOW_CLOSED);
    send(&mut s.svm, &[ctrl_close_preference_window_ix()], &s.cranker, &[])
        .expect("close_preference_window at the deadline");
    assert_eq!(read_epoch_state(&s.svm).phase, PHASE_PLAN_DIRECTED);
    s.svm.expire_blockhash();
    let f = send(
        &mut s.svm,
        &[ctrl_snapshot_preference_ix(&alice.position, &s.vote1)],
        &s.cranker,
        &[],
    )
    .expect_err("snapshot after the window closed must fail");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_WINDOW_CLOSED);

    // PLAN-DIRECTED folds the epoch-stamped record weights into `D`. The validator list is
    // empty (no admissions yet), so both records ride as admission extras behind the cursor.
    let tail = [
        AccountMeta::new(validator_record_pda(&s.vote1), false),
        AccountMeta::new_readonly(s.vote1, false),
        AccountMeta::new(validator_record_pda(&s.vote2), false),
        AccountMeta::new_readonly(s.vote2, false),
    ];
    send(
        &mut s.svm,
        &[ctrl_plan_directed_batch_ix(&s.g, &s.cranker_fusol, &tail)],
        &s.cranker,
        &[],
    )
    .expect("plan_directed_batch over the admission extras");
    let es = read_epoch_state(&s.svm);
    assert_eq!(es.phase, PHASE_PLAN_NEUTRAL, "empty-list cursor completes in one batch");
    assert_eq!(
        es.total_directed_shares,
        fusol(13),
        "D = Alice(10) + Dave(3); Bob/Carol never counted"
    );
    assert_eq!(
        es.neutral_total, es.productive_lamports,
        "Registered validators cap at 0 directed target, so ALL productive lamports stay neutral"
    );
    let rec1 = read_validator_record(&s.svm, &s.vote1);
    assert_eq!(rec1.directed_shares, fusol(10), "Bob's failed snapshots added nothing");
    assert_eq!(rec1.plan_epoch, 1);
    assert_eq!(rec1.directed_target, 0, "Registered lifecycle cap is 0");
    let rec2 = read_validator_record(&s.svm, &s.vote2);
    assert_eq!(rec2.directed_shares, fusol(3));
    assert_eq!(rec2.plan_epoch, 1);
}

// ============================ 3. sync_preference re-arm ============================

#[test]
fn sync_re_arms_after_collateral_change_with_one_epoch_delay() {
    let mut s = stack();
    let alice = fusol_actor(&mut s, fusol(10), 0); // counts in epoch 1 (stale-stamp control)
    let bob = fusol_actor(&mut s, fusol(5), 0); // collateral change -> sync -> counts in epoch 2
    for actor in [&alice, &bob] {
        send(
            &mut s.svm,
            &[ctrl_set_preference_ix(&actor.kp.pubkey(), &actor.position, &s.vote1)],
            &actor.kp,
            &[],
        )
        .expect("set_preference in epoch 0");
    }

    // Epoch 1: Alice counts; Bob's collateral changes, invalidating his direction.
    crank_to_window(&mut s);
    send(&mut s.svm, &[ctrl_snapshot_preference_ix(&alice.position, &s.vote1)], &s.cranker, &[])
        .expect("Alice counts in epoch 1");
    assert_eq!(read_validator_record(&s.svm, &s.vote1).directed_shares, fusol(10));
    send(
        &mut s.svm,
        &[withdraw_ix(&bob.kp.pubkey(), &s.coll, &bob.fusol_ata, fusol(1))],
        &bob.kp,
        &[],
    )
    .expect("debt-free withdraw (nonce bump)");
    let f = send(
        &mut s.svm,
        &[ctrl_snapshot_preference_ix(&bob.position, &s.vote1)],
        &s.cranker,
        &[],
    )
    .expect_err("stale nonce must not count");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_NOT_COUNTABLE);

    // Permissionless re-arm: records the live (nonce, ink, owner) and delays eligibility to
    // the NEXT epoch — the same fungible shares can never direct twice in one epoch.
    let live = read_position(&s.svm, &bob.position);
    let meta = send(&mut s.svm, &[ctrl_sync_preference_ix(&bob.position)], &s.cranker, &[])
        .expect("sync_preference (anyone)");
    let ev: PreferenceUpdated = single_event(&meta);
    assert_eq!(ev.op, PREF_OP_SYNCED);
    let p = read_preference(&s.svm, &bob.position);
    assert_eq!(p.observed_ink_nonce, live.ink_nonce);
    assert_eq!(p.observed_ink, fusol(4));
    assert_eq!(p.eligible_from_epoch, 2, "resync in epoch 1 => countable from epoch 2");
    assert_eq!(p.vote_account, s.vote1, "sync never re-targets");
    s.svm.expire_blockhash();
    let f = send(
        &mut s.svm,
        &[ctrl_snapshot_preference_ix(&bob.position, &s.vote1)],
        &s.cranker,
        &[],
    )
    .expect_err("re-armed preference still waits out the delay epoch");
    assert_eq!(custom_code(&f), E_CTRL_PREFERENCE_NOT_COUNTABLE);

    // No-op sync must NOT reset eligibility — otherwise a griefer could keep any preference
    // perpetually one epoch away from countability by syncing it every epoch.
    let meta = send(&mut s.svm, &[ctrl_sync_preference_ix(&bob.position)], &s.cranker, &[])
        .expect("no-op sync succeeds");
    assert_eq!(events_of::<PreferenceUpdated>(&meta).len(), 0, "no-op sync emits nothing");
    assert_eq!(
        read_preference(&s.svm, &bob.position).eligible_from_epoch,
        2,
        "no-op sync leaves eligibility untouched"
    );

    // Epoch 2: a fresh window; even a no-op sync in the counting epoch itself cannot push the
    // preference away, and the count lands with Bob's LIVE post-withdraw ink.
    crank_to_window(&mut s);
    send(&mut s.svm, &[ctrl_sync_preference_ix(&bob.position)], &s.cranker, &[])
        .expect("no-op sync in the counting epoch");
    assert_eq!(read_preference(&s.svm, &bob.position).eligible_from_epoch, 2);
    send(&mut s.svm, &[ctrl_snapshot_preference_ix(&bob.position, &s.vote1)], &s.cranker, &[])
        .expect("counts after the one-epoch delay");
    let rec = read_validator_record(&s.svm, &s.vote1);
    assert_eq!(rec.directed_shares_epoch, 2);
    assert_eq!(
        rec.directed_shares,
        fusol(4),
        "epoch-stamped weight: Alice's epoch-1 count self-cleared, only Bob's live ink counts"
    );
    assert_eq!(read_preference(&s.svm, &bob.position).last_counted_epoch, 2);
}

// ============================ 4. close_preference ============================

#[test]
fn close_preference_rent_flows_and_permissionless_close_of_emptied_position() {
    let mut s = stack();
    let alice = fusol_actor(&mut s, fusol(10), 0);
    let bob = fusol_actor(&mut s, fusol(5), 0);
    for actor in [&alice, &bob] {
        send(
            &mut s.svm,
            &[ctrl_set_preference_ix(&actor.kp.pubkey(), &actor.position, &s.vote1)],
            &actor.kp,
            &[],
        )
        .expect("set_preference in epoch 0");
    }

    // Rent is pinned to the RECORDED owner — even the owner herself cannot redirect it.
    let f = send(
        &mut s.svm,
        &[ctrl_close_preference_ix(&alice.kp.pubkey(), &alice.position, &bob.kp.pubkey())],
        &alice.kp,
        &[],
    )
    .expect_err("rent to a non-owner must fail");
    assert_eq!(custom_code(&f), E_CTRL_INVALID_RENT_RECIPIENT);

    // While the position holds live ink, only its owner may drop the direction.
    let f = send(
        &mut s.svm,
        &[ctrl_close_preference_ix(&s.cranker.pubkey(), &alice.position, &alice.kp.pubkey())],
        &s.cranker,
        &[],
    )
    .expect_err("stranger closing a live-ink preference must fail");
    assert_eq!(custom_code(&f), E_CTRL_POSITION_STILL_OPEN);

    // Past the change epoch (no controller cranks needed — close reads only the cluster
    // clock), the owner close refunds the rent to the OWNER even when someone else pays the
    // transaction fee.
    warp_epochs(&mut s.svm, 1);
    let pref_addr = preference_pda(&alice.position);
    let rent = s.svm.get_account(&pref_addr).expect("preference exists").lamports;
    assert!(rent > 0);
    let before = lamports(&s.svm, &alice.kp.pubkey());
    let meta = send(
        &mut s.svm,
        &[ctrl_close_preference_ix(&alice.kp.pubkey(), &alice.position, &alice.kp.pubkey())],
        &s.cranker,
        &[&alice.kp],
    )
    .expect("owner close (cranker pays the fee)");
    let ev: PreferenceUpdated = single_event(&meta);
    assert_eq!(ev.op, PREF_OP_CLOSED);
    assert_eq!(
        lamports(&s.svm, &alice.kp.pubkey()),
        before + rent,
        "the exact rent lands on the recorded owner"
    );
    assert!(
        s.svm.get_account(&pref_addr).map_or(true, |a| a.lamports == 0),
        "preference account closed"
    );
    // The owner may re-establish direction afterwards (a fresh account, delayed as usual).
    send(
        &mut s.svm,
        &[ctrl_set_preference_ix(&alice.kp.pubkey(), &alice.position, &s.vote2)],
        &alice.kp,
        &[],
    )
    .expect("set again after close");
    assert_eq!(read_preference(&s.svm, &alice.position).eligible_from_epoch, 2);

    // Once the position is emptied (debt-free full withdraw — no oracle, no controller), the
    // preference is dead weight and ANYONE may close it; rent still goes to the recorded owner.
    send(
        &mut s.svm,
        &[withdraw_ix(&bob.kp.pubkey(), &s.coll, &bob.fusol_ata, fusol(5))],
        &bob.kp,
        &[],
    )
    .expect("full debt-free withdraw");
    assert_eq!(read_position(&s.svm, &bob.position).ink, 0);
    let bob_pref = preference_pda(&bob.position);
    let rent = s.svm.get_account(&bob_pref).expect("preference exists").lamports;
    let before = lamports(&s.svm, &bob.kp.pubkey());
    send(
        &mut s.svm,
        &[ctrl_close_preference_ix(&s.cranker.pubkey(), &bob.position, &bob.kp.pubkey())],
        &s.cranker,
        &[],
    )
    .expect("permissionless close of a zero-ink preference");
    assert_eq!(
        lamports(&s.svm, &bob.kp.pubkey()),
        before + rent,
        "permissionless close can only do the owner the favor of reclaiming rent"
    );
    assert!(
        s.svm.get_account(&bob_pref).map_or(true, |a| a.lamports == 0),
        "preference account closed"
    );
}
