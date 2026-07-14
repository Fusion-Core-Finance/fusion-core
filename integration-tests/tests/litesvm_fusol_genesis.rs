//! Deep genesis scenarios for the fuSOL controller + pool stack against the REAL mainnet-dumped
//! stake-pool processor (loaded at the fork id): `initialize_controller` gating (upgrade
//! authority, default-address args, double-init), `initialize_pool` one-shot sealing + live
//! account validation (maintenance vault, fuSOL mint, validator-list sizing), the FIXED
//! on-chain fee schedule (no setter exists), rate-sensitive deposits across a synthesized
//! reward epoch driven through the controller's own crank cycle, the deposit-authority gates
//! (sealed flag, PDA pinning, direct-upstream bypass), and the genesis event surface.
//!
//! The basics (happy-path genesis + one deposit + register_validator) live in
//! `litesvm_fusol_genesis_smoke.rs`; this file goes deeper on the failure surface and the
//! canonical fee/rate math.
//!
//! Requires the dev-oracle `.so` set (`anchor build -- --features dev-oracle`) and the dumped
//! fixture (`bash scripts/fetch-spl-stake-pool.sh`).

#![allow(deprecated)] // solana_sdk::system_instruction moved to solana-system-interface in 2.3; fine for tests.

use fusd_integration_tests::*;
use fusion_stake_controller::constants::{
    CRANK_REWARD_FINALIZE_POOL, EPOCH_MAINTENANCE_FEE_DENOMINATOR,
    EPOCH_MAINTENANCE_FEE_NUMERATOR, FEE_BPS_DENOMINATOR, MAX_VALIDATORS, SOL_DEPOSIT_FEE_BPS,
    SOL_WITHDRAW_FEE_BPS, STAKE_ACCOUNT_SPACE, STAKE_PROGRAM_ID,
};
use fusion_stake_controller::events::{
    ControllerInitialized, EpochPhaseChanged, MaintenanceRewardPaid, NegativeNavObserved,
    PoolDeposit, PoolInitialized, DEPOSIT_KIND_SOL, TASK_FINALIZE_POOL,
};
use fusion_stake_controller::state::{
    PHASE_FINALIZE, PHASE_IDLE, PHASE_PREFERENCES, PHASE_RECONCILE,
};
use litesvm::LiteSVM;
use solana_sdk::program_pack::Pack;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::rent::Rent;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::system_instruction;
use spl_token::solana_program::program_option::COption;
use spl_token::state::AccountState;

const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

// Upstream `StakePoolError` ordinals (vendor/spl-stake-pool/program/src/error.rs — the enum has
// no explicit discriminants, so each custom code is the 0-based variant index).
const SP_ERR_STAKE_LIST_AND_POOL_OUT_OF_DATE: u32 = 17;
const SP_ERR_UNEXPECTED_VALIDATOR_LIST_ACCOUNT_SIZE: u32 = 20;
const SP_ERR_INVALID_SOL_DEPOSIT_AUTHORITY: u32 = 31;

/// `SystemError::AccountAlreadyInUse` — what the system program returns when Anchor's `init`
/// tries to allocate an already-created PDA (the double-init path).
const SYS_ERR_ACCOUNT_ALREADY_IN_USE: u32 = 0;

/// Upstream `Fee::apply` is CEILING division (vendor state.rs: numerator + denominator − 1).
fn fee_apply_ceil(amount: u64, numerator: u64, denominator: u64) -> u64 {
    ((amount as u128 * numerator as u128 + denominator as u128 - 1) / denominator as u128) as u64
}

/// Read a borsh `Fee { denominator: u64, numerator: u64 }` at `off`.
fn fee_at(data: &[u8], off: usize) -> (u64, u64) {
    (u64_at(data, off), u64_at(data, off + 8))
}

fn u64_at(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
}

// ============================ configurable genesis stack ============================

/// Genesis-stack build knobs. Every deviating field flips exactly ONE property of an otherwise
/// valid stack, so each negative `initialize_pool` test isolates the check it targets.
struct Knobs {
    /// fuSOL mint decimals (spec token requirements: 9).
    mint_decimals: u8,
    /// `None` = the pool withdraw-authority PDA (correct); `Some(k)` = a wrong mint authority.
    mint_authority: Option<Pubkey>,
    /// Create the maintenance vault on a decoy mint instead of the fuSOL mint.
    vault_on_decoy_mint: bool,
    /// `None` = the maintenance PDA (correct); `Some(k)` = a wrong vault token authority.
    vault_authority: Option<Pubkey>,
    /// Synthesize the vault with a token delegate set. Unreachable through the token program
    /// here (the maintenance PDA could never sign an `approve`), so the account is raw-packed.
    vault_with_delegate: bool,
    /// ValidatorList account size (the upstream `Initialize` requires the size to yield
    /// exactly `MAX_VALIDATORS` capacity).
    list_size: usize,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            mint_decimals: 9,
            mint_authority: None,
            vault_on_decoy_mint: false,
            vault_authority: None,
            vault_with_delegate: false,
            list_size: VALIDATOR_LIST_ACCOUNT_SIZE,
        }
    }
}

/// Build the pre-genesis account set (mint, vault, pool + list shells, reserve) per `knobs` and
/// repoint the controller's upgrade authority to `payer`, WITHOUT running either init
/// instruction — the mirror of the harness `pool_genesis` steps with the sends left to the
/// caller, so tests can drive `initialize_controller` / `initialize_pool` and assert failures.
fn prep_stack(svm: &mut LiteSVM, payer: &Keypair, knobs: &Knobs) -> PoolGenesis {
    set_upgrade_authority_at(svm, &fusion_stake_controller::ID, &payer.pubkey());

    let stake_pool_kp = Keypair::new();
    let validator_list_kp = Keypair::new();
    let reserve_kp = Keypair::new();
    let mint_kp = Keypair::new();
    let vault_kp = Keypair::new();

    let g = PoolGenesis {
        config: controller_config_pda(),
        epoch_state: controller_epoch_state_pda(),
        pool_authority: pool_authority_pda(),
        deposit_authority: deposit_authority_pda(),
        maintenance_authority: maintenance_authority_pda(),
        stake_pool: stake_pool_kp.pubkey(),
        validator_list: validator_list_kp.pubkey(),
        reserve_stake: reserve_kp.pubkey(),
        fusol_mint: mint_kp.pubkey(),
        maintenance_vault: vault_kp.pubkey(),
        pool_withdraw_authority: pool_withdraw_authority_pda(&stake_pool_kp.pubkey()),
    };

    // fuSOL mint (knobs may bend decimals / authority; freeze always None here).
    let mint_authority = knobs.mint_authority.unwrap_or(g.pool_withdraw_authority);
    create_mint(svm, payer, &mint_kp, knobs.mint_decimals, &mint_authority, false);

    // Maintenance vault.
    if knobs.vault_with_delegate {
        let mut data = vec![0u8; spl_token::state::Account::LEN];
        spl_token::state::Account::pack(
            spl_token::state::Account {
                mint: g.fusol_mint,
                owner: g.maintenance_authority,
                amount: 0,
                delegate: COption::Some(Pubkey::new_unique()),
                state: AccountState::Initialized,
                is_native: COption::None,
                delegated_amount: 1,
                close_authority: COption::None,
            },
            &mut data,
        )
        .unwrap();
        svm.set_account(
            g.maintenance_vault,
            solana_sdk::account::Account {
                lamports: Rent::default().minimum_balance(spl_token::state::Account::LEN),
                data,
                owner: SPL_TOKEN_ID,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    } else {
        let vault_mint = if knobs.vault_on_decoy_mint {
            let decoy = Keypair::new();
            create_mint(svm, payer, &decoy, 9, &payer.pubkey(), false);
            decoy.pubkey()
        } else {
            g.fusol_mint
        };
        let vault_authority = knobs.vault_authority.unwrap_or(g.maintenance_authority);
        let rent = Rent::default().minimum_balance(spl_token::state::Account::LEN);
        let create = system_instruction::create_account(
            &payer.pubkey(),
            &g.maintenance_vault,
            rent,
            spl_token::state::Account::LEN as u64,
            &SPL_TOKEN_ID,
        );
        let init = spl_token::instruction::initialize_account3(
            &SPL_TOKEN_ID,
            &g.maintenance_vault,
            &vault_mint,
            &vault_authority,
        )
        .unwrap();
        send(svm, &[create, init], payer, &[&vault_kp]).expect("create maintenance vault");
    }

    // Zeroed, rent-exempt, fork-owned StakePool + ValidatorList shells.
    let create_pool = system_instruction::create_account(
        &payer.pubkey(),
        &g.stake_pool,
        Rent::default().minimum_balance(STAKE_POOL_ACCOUNT_SIZE),
        STAKE_POOL_ACCOUNT_SIZE as u64,
        &STAKE_POOL_FORK_ID,
    );
    let create_list = system_instruction::create_account(
        &payer.pubkey(),
        &g.validator_list,
        Rent::default().minimum_balance(knobs.list_size),
        knobs.list_size as u64,
        &STAKE_POOL_FORK_ID,
    );
    send(svm, &[create_pool, create_list], payer, &[&stake_pool_kp, &validator_list_kp])
        .expect("create StakePool + ValidatorList shells");

    // Reserve stake via the REAL stake program, bootstrap-funded above rent.
    let stake_rent = Rent::default().minimum_balance(STAKE_ACCOUNT_SPACE);
    let create_reserve = system_instruction::create_account(
        &payer.pubkey(),
        &g.reserve_stake,
        stake_rent + RESERVE_BOOTSTRAP_LAMPORTS,
        STAKE_ACCOUNT_SPACE as u64,
        &STAKE_PROGRAM_ID,
    );
    let init_reserve = solana_sdk::stake::instruction::initialize(
        &g.reserve_stake,
        &solana_sdk::stake::state::Authorized {
            staker: g.pool_withdraw_authority,
            withdrawer: g.pool_withdraw_authority,
        },
        &solana_sdk::stake::state::Lockup::default(),
    );
    send(svm, &[create_reserve, init_reserve], payer, &[&reserve_kp])
        .expect("create + initialize reserve stake");

    g
}

// ============================ initialize_controller ============================

#[test]
fn initialize_controller_gates_payer_args_and_double_init() {
    let mut svm = new_svm_full();
    let payer = Keypair::new();
    airdrop_sol(&mut svm, &payer.pubkey(), 1_000);
    let mallory = Keypair::new();
    airdrop_sol(&mut svm, &mallory.pubkey(), 10);

    let g = prep_stack(&mut svm, &payer, &Knobs::default());

    // (1) A payer that does NOT hold the program's upgrade authority is rejected — the
    // front-run gate on the deterministic [b"controller"] PDA.
    let f = send(&mut svm, &[ctrl_initialize_controller_ix(&mallory.pubkey(), &g)], &mallory, &[])
        .expect_err("non-upgrade-authority payer must not run genesis");
    assert_eq!(custom_code(&f), E_CTRL_INVALID_CONFIG_ADDRESS);

    // (2) A default-pubkey predeclared address is rejected (it would permanently wedge
    // initialize_pool — nothing valid can ever exist at the default address).
    let mut bad = g.clone();
    bad.reserve_stake = Pubkey::default();
    let f = send(&mut svm, &[ctrl_initialize_controller_ix(&payer.pubkey(), &bad)], &payer, &[])
        .expect_err("default-pubkey predeclared address must be rejected");
    assert_eq!(custom_code(&f), E_CTRL_INVALID_CONFIG_ADDRESS);

    // (3) Real genesis: records the address set, derives + records the pool withdraw
    // authority, zero-initializes the crank machine, emits ControllerInitialized.
    let meta = send(&mut svm, &[ctrl_initialize_controller_ix(&payer.pubkey(), &g)], &payer, &[])
        .expect("initialize_controller");
    let ev: ControllerInitialized = single_event(&meta);
    assert_eq!(ev.stake_pool, g.stake_pool);
    assert_eq!(ev.validator_list, g.validator_list);
    assert_eq!(ev.reserve_stake, g.reserve_stake);
    assert_eq!(ev.fusol_mint, g.fusol_mint);
    assert_eq!(ev.pool_withdraw_authority, g.pool_withdraw_authority);
    assert_eq!(ev.maintenance_vault, g.maintenance_vault);
    assert_eq!(ev.fusd_core_program, fusd_core::ID);
    assert_no_log_events(&meta);

    let cfg = read_controller_config(&svm);
    assert_eq!(cfg.version, 1);
    assert!(!cfg.sealed, "unsealed until initialize_pool");
    assert_eq!(cfg.stake_pool_program, STAKE_POOL_FORK_ID);
    assert_eq!(cfg.stake_pool, g.stake_pool);
    assert_eq!(cfg.validator_list, g.validator_list);
    assert_eq!(cfg.reserve_stake, g.reserve_stake);
    assert_eq!(cfg.fusol_mint, g.fusol_mint);
    assert_eq!(cfg.pool_withdraw_authority, g.pool_withdraw_authority);
    assert_eq!(cfg.maintenance_vault, g.maintenance_vault);
    assert_eq!(cfg.fusd_core_program, fusd_core::ID);
    assert_eq!(cfg.fusol_collateral_mint, g.fusol_mint, "collateral mint IS the pool mint");

    let es = read_epoch_state(&svm);
    assert_eq!(es.controller_epoch, 0);
    assert_eq!(es.phase, PHASE_IDLE);

    // (4) Double-init: the config PDA already exists, so Anchor's `init` fails inside the
    // system-program allocate with AccountAlreadyInUse. Fresh blockhash so the retry is not
    // deduped as AlreadyProcessed.
    svm.expire_blockhash();
    let f = send(&mut svm, &[ctrl_initialize_controller_ix(&payer.pubkey(), &g)], &payer, &[])
        .expect_err("initialize_controller must not run twice");
    assert_eq!(custom_code(&f), SYS_ERR_ACCOUNT_ALREADY_IN_USE);

    // The recorded set survived untouched.
    let cfg = read_controller_config(&svm);
    assert_eq!(cfg.stake_pool, g.stake_pool);
    assert!(!cfg.sealed);
}

// ============================ initialize_pool ============================

#[test]
fn initialize_pool_seals_one_shot() {
    let mut svm = new_svm_full();
    let payer = Keypair::new();
    airdrop_sol(&mut svm, &payer.pubkey(), 1_000);
    // `pool_genesis` runs both inits and asserts `sealed == true` + the authority graph.
    let g = pool_genesis(&mut svm, &payer);
    let before = read_fork_stake_pool(&svm, &g.stake_pool);

    // A second call — by anyone, payer is not special — is rejected by the seal, and no
    // fee/authority mutation path exists anywhere else in the program.
    let mallory = Keypair::new();
    airdrop_sol(&mut svm, &mallory.pubkey(), 10);
    let f = send(&mut svm, &[ctrl_initialize_pool_ix(&mallory.pubkey(), &g)], &mallory, &[])
        .expect_err("initialize_pool must run exactly once");
    assert_eq!(custom_code(&f), E_CTRL_ALREADY_SEALED);

    // The live pool bytes are untouched.
    assert_eq!(read_fork_stake_pool(&svm, &g.stake_pool), before);
    assert!(read_controller_config(&svm).sealed);
}

#[test]
fn initialize_pool_rejects_invalid_maintenance_vault() {
    // One deviation per case; every case must fail the vault validation, never seal.
    let cases = [
        ("vault on a decoy mint", Knobs { vault_on_decoy_mint: true, ..Knobs::default() }),
        (
            "vault owned by a foreign token authority",
            Knobs { vault_authority: Some(Pubkey::new_unique()), ..Knobs::default() },
        ),
        ("vault with a token delegate set", Knobs { vault_with_delegate: true, ..Knobs::default() }),
    ];
    for (label, knobs) in cases {
        let mut svm = new_svm_full();
        let payer = Keypair::new();
        airdrop_sol(&mut svm, &payer.pubkey(), 1_000);
        let g = prep_stack(&mut svm, &payer, &knobs);
        // initialize_controller records addresses without validating the live accounts…
        send(&mut svm, &[ctrl_initialize_controller_ix(&payer.pubkey(), &g)], &payer, &[])
            .expect("initialize_controller records the addresses unvalidated");
        // …initialize_pool is where the live vault is checked.
        let f = send(&mut svm, &[ctrl_initialize_pool_ix(&payer.pubkey(), &g)], &payer, &[])
            .expect_err(label);
        assert_eq!(custom_code(&f), E_CTRL_INVALID_MAINTENANCE_VAULT, "{label}");
        assert!(!read_controller_config(&svm).sealed, "{label}: must stay unsealed");
    }
}

#[test]
fn initialize_pool_rejects_invalid_fusol_mint() {
    let cases = [
        ("non-9-decimals mint", Knobs { mint_decimals: 6, ..Knobs::default() }),
        (
            "mint authority is not the pool withdraw-authority PDA",
            Knobs { mint_authority: Some(Pubkey::new_unique()), ..Knobs::default() },
        ),
    ];
    for (label, knobs) in cases {
        let mut svm = new_svm_full();
        let payer = Keypair::new();
        airdrop_sol(&mut svm, &payer.pubkey(), 1_000);
        let g = prep_stack(&mut svm, &payer, &knobs);
        send(&mut svm, &[ctrl_initialize_controller_ix(&payer.pubkey(), &g)], &payer, &[])
            .expect("initialize_controller records the addresses unvalidated");
        let f = send(&mut svm, &[ctrl_initialize_pool_ix(&payer.pubkey(), &g)], &payer, &[])
            .expect_err(label);
        assert_eq!(custom_code(&f), E_CTRL_INVALID_FUSOL_MINT, "{label}");
        assert!(!read_controller_config(&svm).sealed, "{label}: must stay unsealed");
    }
}

#[test]
fn initialize_pool_rejects_missized_validator_list() {
    // One entry short of MAX_VALIDATORS: the controller passes the account through and the
    // REAL processor's exact-capacity check rejects the CPI.
    let knobs = Knobs { list_size: VALIDATOR_LIST_ACCOUNT_SIZE - 73, ..Knobs::default() };
    let mut svm = new_svm_full();
    let payer = Keypair::new();
    airdrop_sol(&mut svm, &payer.pubkey(), 1_000);
    let g = prep_stack(&mut svm, &payer, &knobs);
    send(&mut svm, &[ctrl_initialize_controller_ix(&payer.pubkey(), &g)], &payer, &[])
        .expect("initialize_controller records the addresses unvalidated");
    let f = send(&mut svm, &[ctrl_initialize_pool_ix(&payer.pubkey(), &g)], &payer, &[])
        .expect_err("a 1023-capacity list must fail the upstream Initialize");
    assert_eq!(custom_code(&f), SP_ERR_UNEXPECTED_VALIDATOR_LIST_ACCOUNT_SIZE);
    assert!(!read_controller_config(&svm).sealed);
}

// ============================ fixed fee schedule ============================

#[test]
fn fee_schedule_is_fixed_on_chain_at_genesis() {
    let mut svm = new_svm_full();
    let payer = Keypair::new();
    airdrop_sol(&mut svm, &payer.pubkey(), 1_000);
    let g = prep_stack(&mut svm, &payer, &Knobs::default());
    send(&mut svm, &[ctrl_initialize_controller_ix(&payer.pubkey(), &g)], &payer, &[])
        .expect("initialize_controller");
    let meta = send(&mut svm, &[ctrl_initialize_pool_ix(&payer.pubkey(), &g)], &payer, &[])
        .expect("initialize_pool (real stake-pool Initialize CPI)");

    let ev: PoolInitialized = single_event(&meta);
    assert_eq!(ev.stake_pool, g.stake_pool);
    assert_eq!(ev.fusol_mint, g.fusol_mint);
    assert_eq!(ev.max_validators, MAX_VALIDATORS);
    assert_no_log_events(&meta);

    // Fixed-offset region via the fusion-stake-view parser: epoch fee = 1/100 of positive
    // net rewards.
    let pool = read_fork_stake_pool(&svm, &g.stake_pool);
    assert_eq!(pool.epoch_fee.denominator, EPOCH_MAINTENANCE_FEE_DENOMINATOR);
    assert_eq!(pool.epoch_fee.numerator, EPOCH_MAINTENANCE_FEE_NUMERATOR);

    // The variable-width borsh tail past STAKE_POOL_FIXED_LEN (346) — walked field by field in
    // the pinned upstream's serialization order, with every FutureEpoch/Option in its genesis
    // shape (so all offsets below are deterministic).
    let data = svm.get_account(&g.stake_pool).unwrap().data;
    let mut o = fusion_stake_view::stake_pool::STAKE_POOL_FIXED_LEN;
    assert_eq!(data[o], 0, "next_epoch_fee = FutureEpoch::None");
    o += 1;
    assert_eq!(data[o], 0, "preferred_deposit_validator = None");
    o += 1;
    assert_eq!(data[o], 0, "preferred_withdraw_validator = None");
    o += 1;
    assert_eq!(
        fee_at(&data, o),
        (FEE_BPS_DENOMINATOR, SOL_DEPOSIT_FEE_BPS),
        "stake deposit fee = 5 bps (Initialize fans one deposit fee to both kinds)"
    );
    o += 16;
    assert_eq!(
        fee_at(&data, o),
        (FEE_BPS_DENOMINATOR, SOL_WITHDRAW_FEE_BPS),
        "stake withdrawal fee = 5 bps"
    );
    o += 16;
    assert_eq!(data[o], 0, "next_stake_withdrawal_fee = FutureEpoch::None");
    o += 1;
    assert_eq!(data[o], 0, "stake referral fee = 0 (disabled)");
    o += 1;
    assert_eq!(data[o], 1, "sol_deposit_authority = Some(..)");
    o += 1;
    assert_eq!(
        &data[o..o + 32],
        g.deposit_authority.as_ref(),
        "SOL deposit authority = the controller's deposit PDA (deposits flow through it)"
    );
    o += 32;
    assert_eq!(
        fee_at(&data, o),
        (FEE_BPS_DENOMINATOR, SOL_DEPOSIT_FEE_BPS),
        "SOL deposit fee = 5 bps"
    );
    o += 16;
    assert_eq!(data[o], 0, "sol referral fee = 0 (disabled)");
    o += 1;
    assert_eq!(data[o], 0, "sol_withdraw_authority = None — withdrawals are NEVER gated");
    o += 1;
    assert_eq!(
        fee_at(&data, o),
        (FEE_BPS_DENOMINATOR, SOL_WITHDRAW_FEE_BPS),
        "SOL withdrawal fee = 5 bps"
    );
    o += 16;
    assert_eq!(data[o], 0, "next_sol_withdrawal_fee = FutureEpoch::None");
    o += 1;
    assert_eq!(u64_at(&data, o), 0, "last_epoch_pool_token_supply starts 0");
    o += 8;
    assert_eq!(u64_at(&data, o), 0, "last_epoch_total_lamports starts 0");

    // Spot-check the deposit fee with a REAL deposit outcome: 4 SOL at rate exactly 1 mints
    // 4e9 tokens, ceil(4e9 × 5 / 10 000) = 2_000_000 to the vault, the rest to the user.
    let depositor = Keypair::new();
    airdrop_sol(&mut svm, &depositor.pubkey(), 20);
    let ata = create_ata_and_fund(&mut svm, &depositor, &depositor.pubkey(), &g.fusol_mint, None, 0);
    let deposit = 4 * LAMPORTS_PER_SOL;
    let vault_before = token_balance(&svm, &g.maintenance_vault);
    let meta = send(
        &mut svm,
        &[ctrl_deposit_sol_ix(&depositor.pubkey(), &g, &ata, deposit)],
        &depositor,
        &[],
    )
    .expect("deposit_sol through the controller");
    let fee = fee_apply_ceil(deposit, SOL_DEPOSIT_FEE_BPS, FEE_BPS_DENOMINATOR);
    assert_eq!(fee, 2_000_000);
    assert_eq!(token_balance(&svm, &ata), deposit - fee, "user gets 1:1 minus the 5 bps fee");
    assert_eq!(
        token_balance(&svm, &g.maintenance_vault),
        vault_before + fee,
        "fee shares land in the maintenance vault (the manager fee account)"
    );
    let ev: PoolDeposit = single_event(&meta);
    assert_eq!(ev.depositor, depositor.pubkey());
    assert_eq!(ev.kind, DEPOSIT_KIND_SOL);
    assert_eq!(ev.vote_account, Pubkey::default());
    assert_eq!(ev.lamports, deposit);
}

// ============================ deposit rate across a reward epoch ============================

#[test]
fn deposit_rate_drops_after_synthesized_rewards_and_refinalize() {
    let mut svm = new_svm_full();
    let payer = Keypair::new();
    airdrop_sol(&mut svm, &payer.pubkey(), 1_000);
    let g = pool_genesis(&mut svm, &payer);

    // Depositor 1 at the genesis rate (exactly 1).
    let user1 = Keypair::new();
    airdrop_sol(&mut svm, &user1.pubkey(), 100);
    let ata1 = create_ata_and_fund(&mut svm, &user1, &user1.pubkey(), &g.fusol_mint, None, 0);
    let deposit = 10 * LAMPORTS_PER_SOL;
    send(&mut svm, &[ctrl_deposit_sol_ix(&user1.pubkey(), &g, &ata1, deposit)], &user1, &[])
        .expect("deposit 1 at rate 1");
    let user1_tokens = token_balance(&svm, &ata1);
    assert_eq!(user1_tokens, deposit - fee_apply_ceil(deposit, SOL_DEPOSIT_FEE_BPS, FEE_BPS_DENOMINATOR));

    // Synthesize one epoch of staking rewards: +2 SOL onto the reserve (litesvm pays no real
    // rewards — the epoch machinery is manual), then advance the epoch.
    let reward = 2 * LAMPORTS_PER_SOL;
    let mut reserve = svm.get_account(&g.reserve_stake).unwrap();
    reserve.lamports += reward;
    svm.set_account(g.reserve_stake, reserve).unwrap();
    warp_epochs(&mut svm, 1);

    // Upstream staleness gate: no deposit until the pool is reconciled + refinalized.
    let user2 = Keypair::new();
    airdrop_sol(&mut svm, &user2.pubkey(), 100);
    let ata2 = create_ata_and_fund(&mut svm, &user2, &user2.pubkey(), &g.fusol_mint, None, 0);
    let f = send(&mut svm, &[ctrl_deposit_sol_ix(&user2.pubkey(), &g, &ata2, deposit)], &user2, &[])
        .expect_err("deposit against a stale pool must fail upstream");
    assert_eq!(custom_code(&f), SP_ERR_STAKE_LIST_AND_POOL_OUT_OF_DATE);

    // The controller's own crank cycle refinalizes the canonical rate:
    // start_epoch → RECONCILE (empty list ⇒ one empty batch) → FINALIZE → PREFERENCES.
    let cranker = Keypair::new();
    airdrop_sol(&mut svm, &cranker.pubkey(), 10);
    let crank_ata = create_ata_and_fund(&mut svm, &cranker, &cranker.pubkey(), &g.fusol_mint, None, 0);
    send(&mut svm, &[ctrl_start_epoch_ix()], &cranker, &[]).expect("start_epoch");
    assert_eq!(read_epoch_state(&svm).phase, PHASE_RECONCILE);
    send(&mut svm, &[ctrl_reconcile_batch_ix(&g, &crank_ata, &[])], &cranker, &[])
        .expect("an empty batch completes reconcile over an empty validator list");
    assert_eq!(read_epoch_state(&svm).phase, PHASE_FINALIZE);
    assert_eq!(token_balance(&svm, &crank_ata), 0, "no stale entry became current: no reward");

    // Expected canonical outcome of the upstream balance update (all pool value sits in the
    // reserve): total = reserve lamports − stake rent; the 1% epoch fee on the positive reward
    // is minted to the maintenance vault at the post-reward rate (floor division upstream).
    let pool_before = read_fork_stake_pool(&svm, &g.stake_pool);
    let stake_rent = Rent::default().minimum_balance(STAKE_ACCOUNT_SPACE);
    let expected_total = lamports(&svm, &g.reserve_stake) - stake_rent;
    let reward_lamports = expected_total - pool_before.total_lamports;
    assert_eq!(reward_lamports, reward, "the lamport bump is the whole reward");
    let fee_lamports =
        fee_apply_ceil(reward_lamports, EPOCH_MAINTENANCE_FEE_NUMERATOR, EPOCH_MAINTENANCE_FEE_DENOMINATOR);
    let epoch_fee_tokens = (pool_before.pool_token_supply as u128 * fee_lamports as u128
        / (expected_total as u128 - fee_lamports as u128)) as u64;

    let vault_before = token_balance(&svm, &g.maintenance_vault);
    let meta = send(&mut svm, &[ctrl_finalize_pool_ix(&g, &crank_ata)], &cranker, &[])
        .expect("finalize_pool");

    let pool_after = read_fork_stake_pool(&svm, &g.stake_pool);
    assert_eq!(pool_after.total_lamports, expected_total);
    assert_eq!(pool_after.pool_token_supply, pool_before.pool_token_supply + epoch_fee_tokens);
    assert_eq!(pool_after.last_update_epoch, 1, "canonical snapshot advanced to the new epoch");
    assert!(pool_after.total_lamports > pool_after.pool_token_supply, "rate is now above 1");
    assert_eq!(mint_supply(&svm, &g.fusol_mint), pool_after.pool_token_supply);

    // Epoch fee in, bounded finalize crank reward out (a TRANSFER from the vault, not a mint).
    assert_eq!(
        token_balance(&svm, &g.maintenance_vault),
        vault_before + epoch_fee_tokens - CRANK_REWARD_FINALIZE_POOL
    );
    assert_eq!(token_balance(&svm, &crank_ata), CRANK_REWARD_FINALIZE_POOL);
    let paid: MaintenanceRewardPaid = single_event(&meta);
    assert_eq!(paid.crank, crank_ata);
    assert_eq!(paid.task, TASK_FINALIZE_POOL);
    assert_eq!(paid.amount, CRANK_REWARD_FINALIZE_POOL);
    let phases = events_of::<EpochPhaseChanged>(&meta);
    assert_eq!(phases.len(), 1);
    assert_eq!((phases[0].from_phase, phases[0].to_phase), (PHASE_FINALIZE, PHASE_PREFERENCES));
    assert!(
        events_of::<NegativeNavObserved>(&meta).is_empty(),
        "NAV growth must not emit a negative-NAV event"
    );

    let es = read_epoch_state(&svm);
    assert_eq!(es.controller_epoch, 1);
    assert_eq!(es.phase, PHASE_PREFERENCES);
    assert_eq!(es.nav_total_lamports, pool_after.total_lamports);
    assert_eq!(es.nav_fusol_supply, pool_after.pool_token_supply);
    assert!(es.preference_window_close_slot > 0, "preference window opened at finalize");

    // Depositor 2 at the refinalized rate: exact upstream math, and strictly FEWER shares per
    // SOL than depositor 1 got at rate 1.
    let expected_tokens = (deposit as u128 * pool_after.pool_token_supply as u128
        / pool_after.total_lamports as u128) as u64;
    let fee2 = fee_apply_ceil(expected_tokens, SOL_DEPOSIT_FEE_BPS, FEE_BPS_DENOMINATOR);
    let vault_pre_deposit = token_balance(&svm, &g.maintenance_vault);
    svm.expire_blockhash(); // the earlier (stale-pool) attempt used identical tx bytes
    send(&mut svm, &[ctrl_deposit_sol_ix(&user2.pubkey(), &g, &ata2, deposit)], &user2, &[])
        .expect("deposit 2 after refinalize");
    let user2_tokens = token_balance(&svm, &ata2);
    assert_eq!(user2_tokens, expected_tokens - fee2);
    assert_eq!(token_balance(&svm, &g.maintenance_vault), vault_pre_deposit + fee2);
    assert!(
        user2_tokens < user1_tokens,
        "the same SOL must mint fewer shares after NAV growth ({user2_tokens} !< {user1_tokens})"
    );
}

// ============================ deposit gates ============================

#[test]
fn deposit_sol_requires_sealed_pool() {
    let mut svm = new_svm_full();
    let payer = Keypair::new();
    airdrop_sol(&mut svm, &payer.pubkey(), 1_000);
    let g = prep_stack(&mut svm, &payer, &Knobs::default());
    send(&mut svm, &[ctrl_initialize_controller_ix(&payer.pubkey(), &g)], &payer, &[])
        .expect("initialize_controller");

    // The controller exists but initialize_pool has not sealed it: deposits are rejected
    // before any CPI is attempted.
    let user = Keypair::new();
    airdrop_sol(&mut svm, &user.pubkey(), 10);
    let ata = create_ata_and_fund(&mut svm, &user, &user.pubkey(), &g.fusol_mint, None, 0);
    let f = send(&mut svm, &[ctrl_deposit_sol_ix(&user.pubkey(), &g, &ata, LAMPORTS_PER_SOL)], &user, &[])
        .expect_err("deposit before initialize_pool must fail");
    assert_eq!(custom_code(&f), E_CTRL_POOL_NOT_INITIALIZED);
}

#[test]
fn deposit_sol_authority_and_amount_gates() {
    let mut svm = new_svm_full();
    let payer = Keypair::new();
    airdrop_sol(&mut svm, &payer.pubkey(), 1_000);
    let g = pool_genesis(&mut svm, &payer);

    let user = Keypair::new();
    airdrop_sol(&mut svm, &user.pubkey(), 10);
    let ata = create_ata_and_fund(&mut svm, &user, &user.pubkey(), &g.fusol_mint, None, 0);

    // Zero amount is rejected by the controller before the CPI.
    let f = send(&mut svm, &[ctrl_deposit_sol_ix(&user.pubkey(), &g, &ata, 0)], &user, &[])
        .expect_err("zero-lamport deposit");
    assert_eq!(custom_code(&f), E_CTRL_ZERO_AMOUNT);

    // A spoofed deposit-authority ACCOUNT through the controller: the [b"deposit_authority"]
    // seeds pin it, so Anchor rejects the substitution.
    let mut ix = ctrl_deposit_sol_ix(&user.pubkey(), &g, &ata, LAMPORTS_PER_SOL);
    let pos = ix.accounts.iter().position(|m| m.pubkey == g.deposit_authority).unwrap();
    ix.accounts[pos].pubkey = Pubkey::new_unique();
    let f = send(&mut svm, &[ix], &user, &[])
        .expect_err("spoofed deposit-authority account must fail the seeds constraint");
    assert_eq!(custom_code(&f), anchor_lang::error::ErrorCode::ConstraintSeeds as u32);

    // Bypassing the controller entirely — a DIRECT upstream DepositSol with a hostile signer
    // in the sol-deposit-authority slot — is rejected by the REAL processor, because
    // initialize_pool installed the controller's deposit PDA as the pool's deposit authority.
    let attacker = Keypair::new();
    airdrop_sol(&mut svm, &attacker.pubkey(), 10);
    let attacker_ata =
        create_ata_and_fund(&mut svm, &attacker, &attacker.pubkey(), &g.fusol_mint, None, 0);
    let ix = fusion_stake_controller::spl_cpi::deposit_sol(
        &g.stake_pool,
        &g.pool_withdraw_authority,
        &g.reserve_stake,
        &attacker.pubkey(),
        &attacker_ata,
        &g.maintenance_vault,
        &g.maintenance_vault,
        &g.fusol_mint,
        &SPL_TOKEN_ID,
        &attacker.pubkey(), // NOT the controller's deposit-authority PDA (attacker signs)
        LAMPORTS_PER_SOL,
    );
    let f = send(&mut svm, &[ix], &attacker, &[])
        .expect_err("direct pool deposit must be authority-gated");
    assert_eq!(custom_code(&f), SP_ERR_INVALID_SOL_DEPOSIT_AUTHORITY);
    assert_eq!(token_balance(&svm, &attacker_ata), 0, "nothing minted on the bypass attempt");
}
