//! Controller `deposit_stake` END-TO-END against the REAL mainnet-dumped SPL Stake Pool
//! processor + the REAL native stake program — the coverage the external audit flagged as the
//! critical gap (no integration test ever invoked `deposit_stake`; that gap let FUSOL-01 and
//! FUSOL-02 ship). Scenario map (one `#[test]` each):
//!
//! 1. `happy_path_atomic_handoff_mints_fusol_minus_fee` — a user creates + delegates a REAL
//!    native stake account to an admitted pool validator, lets it activate across an epoch
//!    boundary, and deposits it in ONE instruction (the post-FUSOL-02 atomic flow: the
//!    depositor signs as the stake account's current withdrawer; the handler CPIs both native
//!    `Authorize`s and the pool `DepositStake` in the same instruction — no pre-authorize step
//!    exists any more). Asserts the exact fuSOL minted (deposit minus the 5 bps stake+SOL
//!    deposit fees, ceiling per upstream `Fee::apply`), the fee landing on the maintenance
//!    vault, the stake merging into the validator's pool stake account, the rent portion swept
//!    to the reserve, pool totals, and the `PoolDeposit` event.
//! 2. `fusol01_cross_wired_validator_accounts_rejected` — the FUSOL-01 regressions: (a) the
//!    audit's exact attack (stake delegated to pool validator B while eligible under-cap
//!    record A is passed) now fails the DELEGATION binding, and (b) a swapped
//!    `validator_stake_account` (record A, stake→A, but B's pool stake account forwarded)
//!    now fails the PDA re-derivation — both before any CPI, leaving pool state byte-identical.
//! 3. `fusol02_theft_paths_unconstructible` — the FUSOL-02 regressions: a third party cannot
//!    consume someone else's stake account (the native stake program demands the CURRENT
//!    withdrawer's signature inside the atomic handoff), and a stake account pre-authorized to
//!    the deposit PDA in a PRIOR transaction (the OLD documented flow) can no longer be
//!    deposited by ANYONE — the theft window is gone because no window exists.
//! 4. `lifecycle_cap_and_stake_state_rejections` — cap-breach (Candidate lifecycle cap over
//!    the CANONICAL live entry), Draining and Registered (not-in-pool) validators, a
//!    non-delegated (Initialized-only) stake account, a zero-lamport/nonexistent stake
//!    account, and the upstream stake-program rejections for activating, deactivating, and
//!    lockup-bound stake — every rejection pinned to a specific error code.
//!
//! ## Fixture notes
//!
//! Validator ADMISSION here synthesizes `ValidatorRecord.directed_shares` (overwriting the
//! controller-owned record) instead of driving the fusd-core position → preference → snapshot
//! path: that path is already covered end-to-end by `litesvm_fusol_preferences.rs` and
//! `litesvm_fusol_epoch_machine.rs`, and this file's subject — `deposit_stake` — only needs
//! admitted validators to exist. The `AddValidatorToPool` CPIs, the validator list, and every
//! deposit-side account are REAL. Stake activation relies on the litesvm ground truth
//! documented in `litesvm_fusol_epoch_machine.rs`: with an empty `StakeHistory`, a delegation
//! created in epoch N reads as FULLY EFFECTIVE at epoch N+1, and litesvm's
//! `FeatureSet::all_enabled()` makes the runtime minimum delegation 1 SOL.
//!
//! Residual (documented, not covered here): the `ZeroAmount` guard on a positive-lamport
//! account is unreachable through real accounts (rent exemption ⇒ lamports > 0; a zero-lamport
//! account does not load as stake-program-owned — pinned below as the owner-check rejection);
//! `DepositStakeWithSlippage` is upstream surface the controller never emits; lockup-custodian
//! co-signed deposits are unsupported by design (the controller threads no custodian).
//!
//! Requires the dev-oracle `.so` set + the dumped fixture:
//! `anchor build -- --features dev-oracle` and `bash scripts/fetch-spl-stake-pool.sh`.
#![allow(deprecated)] // solana_sdk::stake::{instruction,state} moved to solana-stake-interface in 2.3; fine for tests.

use fusd_integration_tests::*;
use fusion_stake_controller::constants::CANDIDATE_CAP_BPS;
use fusion_stake_controller::events::{PoolDeposit, DEPOSIT_KIND_STAKE};
use fusion_stake_controller::spl_cpi;
use litesvm::LiteSVM;
use solana_sdk::instruction::InstructionError;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::stake::instruction as stake_ix;
use solana_sdk::stake::state::{Authorized, Lockup};
use solana_sdk::transaction::TransactionError;

const SOL: u64 = 1_000_000_000;
/// `ValidatorStatus::Draining` byte (fusion-stake-math lifecycle; compile-pinned by the
/// program's own tests).
const STATUS_DRAINING: u8 = 3;
/// Native stake-program `StakeError` custom codes surfaced through the failed CPI (pinned at
/// solana-stake-interface's error enum: MergeTransientStake=5, MergeMismatch=6,
/// CustodianMissing=7).
const STAKE_ERR_MERGE_TRANSIENT: u32 = 5;
const STAKE_ERR_MERGE_MISMATCH: u32 = 6;
const STAKE_ERR_CUSTODIAN_MISSING: u32 = 7;

/// Upstream `Fee::apply` is CEILING division (vendor state.rs): `ceil(amt · 5 / 10_000)`.
fn fee_5bps_ceil(amt: u64) -> u64 {
    (amt as u128 * 5).div_ceil(10_000) as u64
}

// ============================ fixture ============================

struct Stack {
    svm: LiteSVM,
    gov: Keypair,
    g: PoolGenesis,
    /// gov's fuSOL ATA — crank-reward sink for the fixture's epoch cranks.
    crank: Pubkey,
    /// Admitted pool validators (Candidate, list indices 0 and 1).
    v0: Pubkey,
    v1: Pubkey,
    /// Registered only — never admitted to the pool.
    v2: Pubkey,
}

/// Genesis the REAL pool, bulk-deposit 40,000 SOL (so the Candidate cap is ~100 SOL), and admit
/// v0 + v1 through the REAL epoch-1 plan + `AddValidatorToPool` CPIs (directed weights
/// synthesized — see the module doc). Returns at cluster epoch 1, controller IDLE,
/// list = [v0@0, v1@1].
fn stack() -> Stack {
    let mut svm = new_svm_full();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 2_000);
    let g = pool_genesis(&mut svm, &gov);
    let crank = create_ata_and_fund(&mut svm, &gov, &gov.pubkey(), &g.fusol_mint, None, 0);

    // Bulk pool value: total == supply == 40,001 SOL exactly (rate 1; the 5 bps deposit fee
    // only moves shares to the maintenance vault).
    let bulk = Keypair::new();
    airdrop_sol(&mut svm, &bulk.pubkey(), 40_100);
    let bulk_ata = create_ata_and_fund(&mut svm, &bulk, &bulk.pubkey(), &g.fusol_mint, None, 0);
    send(&mut svm, &[ctrl_deposit_sol_ix(&bulk.pubkey(), &g, &bulk_ata, 40_000 * SOL)], &bulk, &[])
        .expect("bulk deposit_sol");

    let v0 = register_healthy_validator(&mut svm, &gov, 5);
    let v1 = register_healthy_validator(&mut svm, &gov, 5);
    let v2 = register_healthy_validator(&mut svm, &gov, 5);

    // Epoch 1: reconcile (empty list) + finalize, then synthesize admission-grade directed
    // weight on v0/v1 (raw target ≈ 588 SOL ≥ the 500 SOL activation floor) and run the REAL
    // plan + admission adds.
    warp_epochs(&mut svm, 1);
    send(&mut svm, &[ctrl_start_epoch_ix()], &gov, &[]).expect("start_epoch (1)");
    send(&mut svm, &[ctrl_reconcile_batch_ix(&g, &crank, &[])], &gov, &[]).expect("reconcile (1)");
    send(&mut svm, &[ctrl_finalize_pool_ix(&g, &crank)], &gov, &[]).expect("finalize_pool (1)");
    for v in [&v0, &v1] {
        let mut rec = read_validator_record(&svm, v);
        rec.directed_shares = 600 * SOL;
        rec.directed_shares_epoch = 1;
        overwrite_anchor_account(&mut svm, validator_record_pda(v), &rec);
    }
    ctrl_close_window_at_deadline(&mut svm, &gov);
    send(
        &mut svm,
        &[ctrl_plan_directed_batch_ix(&g, &crank, &plan_pairs(&[v0, v1, v2]))],
        &gov,
        &[],
    )
    .expect("plan-directed with admission extras");
    send(&mut svm, &[ctrl_plan_neutral_batch_ix(&g, &crank, &[])], &gov, &[])
        .expect("plan-neutral (no Active capacity)");
    send(&mut svm, &[ctrl_finalize_plan_ix(&g, &crank)], &gov, &[]).expect("finalize_plan (1)");
    execute_admission_add(&mut svm, &gov, &g, &crank, &v0);
    execute_admission_add(&mut svm, &gov, &g, &crank, &v1);
    send(&mut svm, &[ctrl_finish_epoch_ix(&g, &crank)], &gov, &[]).expect("finish_epoch (1)");

    assert_eq!(read_fork_validator_list_len(&svm, &g.validator_list), 2);
    assert_eq!(read_validator_record(&svm, &v0).validator_list_index, 0);
    assert_eq!(read_validator_record(&svm, &v1).validator_list_index, 1);

    Stack { svm, gov, g, crank, v0, v1, v2 }
}

/// Advance one epoch and re-stamp the pool current (start → reconcile both entries →
/// finalize). Stake accounts delegated in the PREVIOUS epoch are now fully active (empty
/// `StakeHistory`), and the upstream `DepositStake` freshness gate (`last_update_epoch >=
/// clock.epoch`) is satisfied. Leaves the controller mid-PREFERENCES — `deposit_stake` is
/// deliberately NOT phase-gated.
fn advance_epoch_and_restamp(s: &mut Stack) {
    warp_epochs(&mut s.svm, 1);
    send(&mut s.svm, &[ctrl_start_epoch_ix()], &s.gov, &[]).expect("start_epoch");
    let quads = reconcile_quads(&s.svm, &s.g, 0, read_fork_validator_list_len(&s.svm, &s.g.validator_list));
    send(&mut s.svm, &[ctrl_reconcile_batch_ix(&s.g, &s.crank, &quads)], &s.gov, &[])
        .expect("reconcile");
    send(&mut s.svm, &[ctrl_finalize_pool_ix(&s.g, &s.crank)], &s.gov, &[]).expect("finalize_pool");
    assert_eq!(
        read_fork_stake_pool(&s.svm, &s.g.stake_pool).last_update_epoch,
        now_epoch(&s.svm),
        "pool stamped current for the deposit epoch"
    );
}

/// Create a REAL native stake account for `owner` funded with `stake_lamports` above rent
/// (create + initialize; no delegation).
fn create_stake_account(
    svm: &mut LiteSVM,
    owner: &Keypair,
    stake_lamports: u64,
    lockup: &Lockup,
) -> Pubkey {
    let stake_kp = Keypair::new();
    let rent = svm.minimum_balance_for_rent_exemption(200);
    let ixs = stake_ix::create_account(
        &owner.pubkey(),
        &stake_kp.pubkey(),
        &Authorized::auto(&owner.pubkey()),
        lockup,
        rent + stake_lamports,
    );
    send(svm, &ixs, owner, &[&stake_kp]).expect("create + initialize stake account");
    stake_kp.pubkey()
}

/// Create + delegate a stake account to `vote` (delegates the full above-rent amount).
fn create_delegated_stake(
    svm: &mut LiteSVM,
    owner: &Keypair,
    vote: &Pubkey,
    stake_lamports: u64,
) -> Pubkey {
    let stake = create_stake_account(svm, owner, stake_lamports, &Lockup::default());
    send(svm, &[stake_ix::delegate_stake(&stake, &owner.pubkey(), vote)], owner, &[])
        .expect("delegate stake");
    stake
}

/// `deposit_stake` as `depositor` with the given account wiring (the vote/record pair, the
/// forwarded validator stake account, and the fuSOL destination are all caller-chosen — the
/// whole point of the FUSOL-01/02 probes). Takes `svm`/`g` as DISJOINT borrows so a call site
/// can pass `&s.v0` (a third field) in the same expression.
#[allow(clippy::too_many_arguments)]
fn deposit(
    svm: &mut LiteSVM,
    g: &PoolGenesis,
    depositor: &Keypair,
    stake: &Pubkey,
    vote: &Pubkey,
    validator_stake: &Pubkey,
    fusol_ata: &Pubkey,
) -> litesvm::types::TransactionResult {
    svm.expire_blockhash();
    let ix = ctrl_deposit_stake_ix(&depositor.pubkey(), g, stake, vote, validator_stake, fusol_ata);
    send(svm, &[ix], depositor, &[])
}

/// The seed-0 validator pool stake account (the only seed the controller ever creates).
fn vstake_of(s: &Stack, vote: &Pubkey) -> Pubkey {
    spl_cpi::derive_validator_stake(vote, &s.g.stake_pool, 0)
}

/// Raw bytes of the pool-side accounts a rejected deposit must leave untouched.
fn pool_state_bytes(s: &Stack) -> (Vec<u8>, Vec<u8>, u64) {
    let pool = s.svm.get_account(&s.g.stake_pool).unwrap().data;
    let list = s.svm.get_account(&s.g.validator_list).unwrap().data;
    let supply = mint_supply(&s.svm, &s.g.fusol_mint);
    (pool, list, supply)
}

// =====================================================================================
// 1. Happy path — atomic handoff, exact fee, merge, sweep, event
// =====================================================================================

#[test]
fn happy_path_atomic_handoff_mints_fusol_minus_fee() {
    let mut s = stack();

    // Alice creates + delegates 5 SOL to v0 in epoch 1 (both her stake and v0's pool stake
    // account read the SAME healthy vote state, so `credits_observed` match at merge time).
    let alice = Keypair::new();
    airdrop_sol(&mut s.svm, &alice.pubkey(), 20);
    let alice_ata = create_ata_and_fund(&mut s.svm, &alice, &alice.pubkey(), &s.g.fusol_mint, None, 0);
    let stake = create_delegated_stake(&mut s.svm, &alice, &s.v0, 5 * SOL);
    let stake_rent = s.svm.minimum_balance_for_rent_exemption(200);
    let deposit_lamports = s.svm.get_account(&stake).unwrap().lamports;
    assert_eq!(deposit_lamports, 5 * SOL + stake_rent, "5 SOL delegated + the rent reserve");

    // Epoch 2: the delegation is fully active; the pool is re-stamped current.
    advance_epoch_and_restamp(&mut s);
    let pool_before = read_fork_stake_pool(&s.svm, &s.g.stake_pool);
    assert_eq!(
        pool_before.total_lamports, pool_before.pool_token_supply,
        "rate exactly 1 (fees only ever moved shares)"
    );
    let entry_before = fork_list_entry(&s.svm, &s.g.validator_list, 0);
    let vstake0 = vstake_of(&s, &s.v0);
    let vault_before = token_balance(&s.svm, &s.g.maintenance_vault);
    let reserve_before = s.svm.get_account(&s.g.reserve_stake).unwrap().lamports;
    let vstake_before = s.svm.get_account(&vstake0).unwrap().lamports;

    // THE deposit: one instruction, no pre-authorize step anywhere — alice signs as the stake
    // account's current withdrawer and the handler does handoff + pool deposit atomically.
    let meta = deposit(&mut s.svm, &s.g, &alice, &stake, &s.v0, &vstake0, &alice_ata)
        .expect("atomic deposit_stake");

    // Exact upstream fee math at rate 1: tokens minted = the FULL absorbed lamports; the 5 bps
    // deposit fee applies (ceiling) per portion — stake (5 SOL) and rent (the SOL portion).
    let fee = fee_5bps_ceil(5 * SOL) + fee_5bps_ceil(stake_rent);
    assert_eq!(
        token_balance(&s.svm, &alice_ata),
        deposit_lamports - fee,
        "user fuSOL = deposit − 5 bps stake fee − 5 bps SOL(rent) fee"
    );
    assert_eq!(
        token_balance(&s.svm, &s.g.maintenance_vault),
        vault_before + fee,
        "the whole fee lands on the maintenance vault (referral 0)"
    );

    // Physical stake: the delegation merged into v0's pool stake account; the rent portion was
    // swept to the reserve; the user's stake account was absorbed entirely.
    assert_eq!(
        s.svm.get_account(&vstake0).unwrap().lamports,
        vstake_before + 5 * SOL,
        "validator stake gained exactly the delegated 5 SOL"
    );
    let entry_after = fork_list_entry(&s.svm, &s.g.validator_list, 0);
    assert_eq!(
        entry_after.active_stake_lamports,
        entry_before.active_stake_lamports + 5 * SOL,
        "the canonical list entry tracks the merge immediately"
    );
    assert_eq!(
        s.svm.get_account(&s.g.reserve_stake).unwrap().lamports,
        reserve_before + stake_rent,
        "the rent (SOL portion) was withdrawn to the reserve"
    );
    assert!(
        s.svm.get_account(&stake).is_none_or(|a| a.lamports == 0),
        "the deposited stake account is gone (fully absorbed)"
    );

    // Pool accounting: both legs grew by the full deposit (rate preserved at 1).
    let pool_after = read_fork_stake_pool(&s.svm, &s.g.stake_pool);
    assert_eq!(pool_after.total_lamports, pool_before.total_lamports + deposit_lamports);
    assert_eq!(pool_after.pool_token_supply, pool_before.pool_token_supply + deposit_lamports);

    // Event attribution: the enforced owner, the deposited voter, the full absorbed lamports.
    let ev: PoolDeposit = single_event(&meta);
    assert_eq!(ev.depositor, alice.pubkey());
    assert_eq!(ev.kind, DEPOSIT_KIND_STAKE);
    assert_eq!(ev.vote_account, s.v0);
    assert_eq!(ev.lamports, deposit_lamports);
}

// =====================================================================================
// 2. FUSOL-01 — cross-wired validator accounts
// =====================================================================================

#[test]
fn fusol01_cross_wired_validator_accounts_rejected() {
    let mut s = stack();

    // mallory's stake is delegated to v1 ("validator B"); dave's to v0 ("validator A").
    let mallory = Keypair::new();
    airdrop_sol(&mut s.svm, &mallory.pubkey(), 20);
    let mallory_ata =
        create_ata_and_fund(&mut s.svm, &mallory, &mallory.pubkey(), &s.g.fusol_mint, None, 0);
    let stake_to_v1 = create_delegated_stake(&mut s.svm, &mallory, &s.v1, 5 * SOL);

    let dave = Keypair::new();
    airdrop_sol(&mut s.svm, &dave.pubkey(), 20);
    let dave_ata = create_ata_and_fund(&mut s.svm, &dave, &dave.pubkey(), &s.g.fusol_mint, None, 0);
    let stake_to_v0 = create_delegated_stake(&mut s.svm, &dave, &s.v0, 5 * SOL);

    advance_epoch_and_restamp(&mut s);
    let before = pool_state_bytes(&s);

    // (a) THE audit attack: pass eligible under-cap record A (v0) while the stake — and the
    // forwarded validator stake account — belong to validator B (v1). Pre-fix, the controller
    // cap-checked A while upstream derived B from the forwarded account and merged there.
    // Post-fix the deposited account's OWN delegation is bound to the record first.
    let vstake1 = vstake_of(&s, &s.v1);
    let f = deposit(&mut s.svm, &s.g, &mallory, &stake_to_v1, &s.v0, &vstake1, &mallory_ata)
        .expect_err("record A + stake delegated to B must be rejected");
    assert_eq!(custom_code(&f), E_CTRL_STAKE_DELEGATION_MISMATCH);
    assert_eq!(pool_state_bytes(&s), before, "rejected before any CPI — pool untouched");

    // (a') the same attack with a consistent-looking validator_stake_account (A's) changes
    // nothing: the delegation binding fires first.
    let vstake0 = vstake_of(&s, &s.v0);
    let f = deposit(&mut s.svm, &s.g, &mallory, &stake_to_v1, &s.v0, &vstake0, &mallory_ata)
        .expect_err("the delegation binding is independent of the forwarded account");
    assert_eq!(custom_code(&f), E_CTRL_STAKE_DELEGATION_MISMATCH);

    // (b) account swap: record and delegation agree on A (v0), but B's pool stake account is
    // forwarded. The controller re-derives the PDA from the RECORD's vote + the canonical
    // entry's seed suffix and pins the forwarded account to it.
    let f = deposit(&mut s.svm, &s.g, &dave, &stake_to_v0, &s.v0, &vstake1, &dave_ata)
        .expect_err("forwarded validator stake account must equal the record-derived PDA");
    assert_eq!(custom_code(&f), E_CTRL_ADDRESS_MISMATCH);
    assert_eq!(pool_state_bytes(&s), before, "nothing minted, nothing merged");

    // Control: correctly wired, dave's deposit to v0 succeeds.
    deposit(&mut s.svm, &s.g, &dave, &stake_to_v0, &s.v0, &vstake0, &dave_ata)
        .expect("the correctly wired deposit still works");
    assert!(token_balance(&s.svm, &dave_ata) > 0);
}

// =====================================================================================
// 3. FUSOL-02 — theft paths unconstructible under the atomic flow
// =====================================================================================

#[test]
fn fusol02_theft_paths_unconstructible() {
    let mut s = stack();

    let victim = Keypair::new();
    airdrop_sol(&mut s.svm, &victim.pubkey(), 20);
    let victim_ata =
        create_ata_and_fund(&mut s.svm, &victim, &victim.pubkey(), &s.g.fusol_mint, None, 0);
    let victim_stake = create_delegated_stake(&mut s.svm, &victim, &s.v0, 5 * SOL);

    let victim2 = Keypair::new();
    airdrop_sol(&mut s.svm, &victim2.pubkey(), 20);
    let victim2_stake = create_delegated_stake(&mut s.svm, &victim2, &s.v0, 5 * SOL);

    advance_epoch_and_restamp(&mut s);
    let vstake0 = vstake_of(&s, &s.v0);
    let attacker = Keypair::new();
    airdrop_sol(&mut s.svm, &attacker.pubkey(), 20);
    let attacker_ata =
        create_ata_and_fund(&mut s.svm, &attacker, &attacker.pubkey(), &s.g.fusol_mint, None, 0);

    // (a) The atomic-flow theft probe: the attacker names themself `depositor`, the victim's
    // stake account, and their OWN fuSOL destination. The handler's first `Authorize` CPI
    // demands the CURRENT withdrawer's signature — the attacker's signature is worthless.
    let victim_stake_lamports = s.svm.get_account(&victim_stake).unwrap().lamports;
    let f = deposit(&mut s.svm, &s.g, &attacker, &victim_stake, &s.v0, &vstake0, &attacker_ata)
        .expect_err("a third party must not be able to consume someone else's stake");
    assert!(
        matches!(
            f.err,
            TransactionError::InstructionError(_, InstructionError::MissingRequiredSignature)
        ),
        "the native stake program rejects the handoff without the real withdrawer: {:?}",
        f.err
    );
    assert_eq!(
        s.svm.get_account(&victim_stake).unwrap().lamports,
        victim_stake_lamports,
        "the victim's stake account is untouched"
    );
    assert_eq!(token_balance(&s.svm, &attacker_ata), 0, "the attacker minted nothing");

    // The audit's pre-fix theft shape no longer EXISTS as a sequence: there is no documented
    // (or working) pre-authorize step whose completion strands an ownerless stake account
    // between transactions. Probe the old flow anyway: victim2 pre-authorizes both roles to
    // the deposit PDA in a PRIOR transaction...
    let pda = deposit_authority_pda();
    let auth_staker = stake_ix::authorize(
        &victim2_stake,
        &victim2.pubkey(),
        &pda,
        solana_sdk::stake::state::StakeAuthorize::Staker,
        None,
    );
    let auth_withdrawer = stake_ix::authorize(
        &victim2_stake,
        &victim2.pubkey(),
        &pda,
        solana_sdk::stake::state::StakeAuthorize::Withdrawer,
        None,
    );
    send(&mut s.svm, &[auth_staker, auth_withdrawer], &victim2, &[])
        .expect("the OLD flow's separate pre-authorize transaction");

    // ...(b) and the attacker races in exactly as FUSOL-02 described. Pre-fix this minted the
    // victim's shares to the attacker; post-fix the handler re-runs `Authorize` as the
    // depositor, and the depositor is not the (now-PDA) withdrawer.
    let f = deposit(&mut s.svm, &s.g, &attacker, &victim2_stake, &s.v0, &vstake0, &attacker_ata)
        .expect_err("the pre-authorized account is not consumable by an attacker");
    assert!(matches!(
        f.err,
        TransactionError::InstructionError(_, InstructionError::MissingRequiredSignature)
    ));
    assert_eq!(token_balance(&s.svm, &attacker_ata), 0);

    // (c) Not even the victim can deposit it any more — the pre-authorized account is bricked
    // for this program (documented loudly in the instruction's module doc: never pre-assign
    // the authorities out-of-band).
    let f = deposit(&mut s.svm, &s.g, &victim2, &victim2_stake, &s.v0, &vstake0, &victim_ata)
        .expect_err("a pre-authorized account cannot be recovered through deposit_stake");
    assert!(matches!(
        f.err,
        TransactionError::InstructionError(_, InstructionError::MissingRequiredSignature)
    ));
    // Its authorities really are the PDA (StakeStateV2: Meta.authorized at bytes 12..44
    // staker, 44..76 withdrawer), and its lamports never moved.
    let raw = s.svm.get_account(&victim2_stake).unwrap();
    assert_eq!(raw.data[12..44], pda.to_bytes(), "staker = deposit PDA");
    assert_eq!(raw.data[44..76], pda.to_bytes(), "withdrawer = deposit PDA");

    // (d) Control: the victim's own atomic deposit of their NON-pre-authorized account works,
    // minting to the destination THEY chose.
    deposit(&mut s.svm, &s.g, &victim, &victim_stake, &s.v0, &vstake0, &victim_ata)
        .expect("the owner's own single-transaction deposit");
    assert!(token_balance(&s.svm, &victim_ata) > 0, "shares minted to the owner's account");
}

// =====================================================================================
// 4. Lifecycle cap + stake-state rejections
// =====================================================================================

#[test]
fn lifecycle_cap_and_stake_state_rejections() {
    let mut s = stack();
    let stake_rent = s.svm.minimum_balance_for_rent_exemption(200);

    // Epoch-1 stake accounts (all fully active after the epoch warp).
    let carol = Keypair::new(); // cap breach: 150 SOL vs the ~100 SOL Candidate cap
    airdrop_sol(&mut s.svm, &carol.pubkey(), 200);
    let carol_ata = create_ata_and_fund(&mut s.svm, &carol, &carol.pubkey(), &s.g.fusol_mint, None, 0);
    let stake_150 = create_delegated_stake(&mut s.svm, &carol, &s.v0, 150 * SOL);

    let dora = Keypair::new(); // Draining-record probe (delegated to v1)
    airdrop_sol(&mut s.svm, &dora.pubkey(), 20);
    let dora_ata = create_ata_and_fund(&mut s.svm, &dora, &dora.pubkey(), &s.g.fusol_mint, None, 0);
    let stake_v1 = create_delegated_stake(&mut s.svm, &dora, &s.v1, 5 * SOL);

    let reg = Keypair::new(); // Registered (never admitted) probe (delegated to v2)
    airdrop_sol(&mut s.svm, &reg.pubkey(), 20);
    let reg_ata = create_ata_and_fund(&mut s.svm, &reg, &reg.pubkey(), &s.g.fusol_mint, None, 0);
    let stake_v2 = create_delegated_stake(&mut s.svm, &reg, &s.v2, 5 * SOL);

    let erin = Keypair::new(); // deactivating probe
    airdrop_sol(&mut s.svm, &erin.pubkey(), 20);
    let erin_ata = create_ata_and_fund(&mut s.svm, &erin, &erin.pubkey(), &s.g.fusol_mint, None, 0);
    let stake_deact = create_delegated_stake(&mut s.svm, &erin, &s.v0, 5 * SOL);

    let lucy = Keypair::new(); // lockup probe: in-force custodial lockup
    airdrop_sol(&mut s.svm, &lucy.pubkey(), 20);
    let lucy_ata = create_ata_and_fund(&mut s.svm, &lucy, &lucy.pubkey(), &s.g.fusol_mint, None, 0);
    let custodian = Pubkey::new_unique();
    let lockup = Lockup { unix_timestamp: i64::MAX, epoch: u64::MAX, custodian };
    let stake_locked = create_stake_account(&mut s.svm, &lucy, 5 * SOL, &lockup);
    send(&mut s.svm, &[stake_ix::delegate_stake(&stake_locked, &lucy.pubkey(), &s.v0)], &lucy, &[])
        .expect("delegate the locked stake");

    let nate = Keypair::new(); // non-delegated (Initialized-only) probe
    airdrop_sol(&mut s.svm, &nate.pubkey(), 20);
    let nate_ata = create_ata_and_fund(&mut s.svm, &nate, &nate.pubkey(), &s.g.fusol_mint, None, 0);
    let stake_undelegated = create_stake_account(&mut s.svm, &nate, 5 * SOL, &Lockup::default());

    advance_epoch_and_restamp(&mut s);
    let vstake0 = vstake_of(&s, &s.v0);
    let vstake1 = vstake_of(&s, &s.v1);
    let vstake2 = vstake_of(&s, &s.v2); // never exists — v2 was never added

    // --- cap breach: the Candidate lifecycle cap over the CANONICAL live entry ---------------
    // Premise check from live reads: cap = 25 bps of the live pool total; the 150 SOL deposit
    // would blow through it.
    let pool = read_fork_stake_pool(&s.svm, &s.g.stake_pool);
    let cap = (u128::from(pool.total_lamports) * u128::from(CANDIDATE_CAP_BPS) / 10_000) as u64;
    let entry = fork_list_entry(&s.svm, &s.g.validator_list, 0);
    let physical_after =
        entry.active_stake_lamports + entry.transient_stake_lamports + 150 * SOL + stake_rent;
    assert!(physical_after > cap, "the scenario must actually breach the Candidate cap");
    assert!(entry.active_stake_lamports + 5 * SOL + stake_rent <= cap, "small deposits still fit");
    let f = deposit(&mut s.svm, &s.g, &carol, &stake_150, &s.v0, &vstake0, &carol_ata)
        .expect_err("cap-breaching deposit");
    assert_eq!(custom_code(&f), E_CTRL_VALIDATOR_CAP_EXCEEDED);

    // --- Draining validator: no stake deposits of any kind -----------------------------------
    // (Status synthesized on the record — reaching Draining organically is lifecycle-machine
    // territory; deposit_stake only reads the byte.)
    let mut rec = read_validator_record(&s.svm, &s.v1);
    rec.status = STATUS_DRAINING;
    overwrite_anchor_account(&mut s.svm, validator_record_pda(&s.v1), &rec);
    let f = deposit(&mut s.svm, &s.g, &dora, &stake_v1, &s.v1, &vstake1, &dora_ata)
        .expect_err("Draining validator must reject stake deposits");
    assert_eq!(custom_code(&f), E_CTRL_VALIDATOR_NOT_IN_POOL);

    // --- Registered (never admitted): no pool stake account to merge into --------------------
    let f = deposit(&mut s.svm, &s.g, &reg, &stake_v2, &s.v2, &vstake2, &reg_ata)
        .expect_err("Registered validator is not in the pool");
    assert_eq!(custom_code(&f), E_CTRL_VALIDATOR_NOT_IN_POOL);

    // --- non-delegated stake: the raw StakeStateV2 parse fails closed on Initialized ---------
    let f = deposit(&mut s.svm, &s.g, &nate, &stake_undelegated, &s.v0, &vstake0, &nate_ata)
        .expect_err("Initialized-only stake has no delegation to bind");
    assert_eq!(custom_code(&f), E_CTRL_INVALID_USER_STAKE_ACCOUNT);

    // --- zero-lamport "stake account": does not load as a stake-program account --------------
    // (A positive-lamport account can never trip the ZeroAmount guard — rent exemption keeps
    // lamports > 0 — so the observable rejection for a zeroed account is the owner check.)
    let ghost = Pubkey::new_unique();
    let mut ghost_acct = s.svm.get_account(&stake_v1).unwrap();
    ghost_acct.lamports = 0;
    s.svm.set_account(ghost, ghost_acct).unwrap();
    let f = deposit(&mut s.svm, &s.g, &dora, &ghost, &s.v1, &vstake1, &dora_ata)
        .expect_err("zero-lamport account must be rejected");
    assert_eq!(custom_code(&f), E_CTRL_INVALID_USER_STAKE_ACCOUNT);

    // --- activating stake (delegated THIS epoch): upstream merge classifies the source as
    // ActivationEpoch against a FullyActive destination → StakeError::MergeMismatch ----------
    let frank = Keypair::new();
    airdrop_sol(&mut s.svm, &frank.pubkey(), 20);
    let frank_ata = create_ata_and_fund(&mut s.svm, &frank, &frank.pubkey(), &s.g.fusol_mint, None, 0);
    let stake_activating = create_delegated_stake(&mut s.svm, &frank, &s.v0, 5 * SOL);
    let f = deposit(&mut s.svm, &s.g, &frank, &stake_activating, &s.v0, &vstake0, &frank_ata)
        .expect_err("activating stake must be rejected upstream");
    assert_eq!(custom_code(&f), STAKE_ERR_MERGE_MISMATCH);

    // --- deactivating stake: effective + deactivating → StakeError::MergeTransientStake ------
    send(
        &mut s.svm,
        &[stake_ix::deactivate_stake(&stake_deact, &erin.pubkey())],
        &erin,
        &[],
    )
    .expect("deactivate erin's active stake");
    let f = deposit(&mut s.svm, &s.g, &erin, &stake_deact, &s.v0, &vstake0, &erin_ata)
        .expect_err("deactivating stake must be rejected upstream");
    assert_eq!(custom_code(&f), STAKE_ERR_MERGE_TRANSIENT);

    // --- lockup in force: the withdrawer handoff needs the custodian, which the controller
    // (by design) never threads → StakeError::CustodianMissing ---------------------------------
    let f = deposit(&mut s.svm, &s.g, &lucy, &stake_locked, &s.v0, &vstake0, &lucy_ata)
        .expect_err("locked stake must be rejected at the withdrawer handoff");
    assert_eq!(custom_code(&f), STAKE_ERR_CUSTODIAN_MISSING);

    // Nothing above minted anything.
    for ata in [&carol_ata, &dora_ata, &reg_ata, &nate_ata, &erin_ata, &lucy_ata] {
        assert_eq!(token_balance(&s.svm, ata), 0);
    }
}
