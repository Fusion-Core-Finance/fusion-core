//! Genesis smoke for the fuSOL pool stack — the whole fixture chain against the REAL programs:
//! fusd-core + the Allocation Controller (both upgradeable-loaded) + the mainnet-DUMPED SPL
//! Stake Pool processor at the fork id. `pool_genesis` builds mint/vault/pool/list/reserve for
//! real, runs `initialize_controller` + `initialize_pool` (the one-time upstream `Initialize`
//! CPI), and the test then pushes a SOL deposit THROUGH the controller and reads the canonical
//! pool totals back via `fusion_stake_view`.
//!
//! Requires the dev-oracle `.so` set: `anchor build -- --features dev-oracle`, plus the dumped
//! fixture: `bash scripts/fetch-spl-stake-pool.sh`.

use fusd_integration_tests::*;
use fusion_stake_controller::constants::{
    FEE_BPS_DENOMINATOR, SOL_DEPOSIT_FEE_BPS, VALIDATOR_LIST_INDEX_UNSET,
};
use fusion_stake_controller::events::{PoolDeposit, DEPOSIT_KIND_SOL};
use solana_sdk::signature::{Keypair, Signer};

const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

#[test]
fn pool_genesis_then_deposit_sol_mints_at_rate_one_minus_fee() {
    let mut svm = new_svm_full();
    let payer = Keypair::new();
    airdrop_sol(&mut svm, &payer.pubkey(), 1_000);

    // Genesis — authority graph is asserted inside the fixture; re-assert the load-bearing
    // edges here so the test stands alone.
    let g = pool_genesis(&mut svm, &payer);
    let pool = read_fork_stake_pool(&svm, &g.stake_pool);
    assert_eq!(pool.manager, pool_authority_pda().to_bytes(), "manager = pool-authority PDA");
    assert_eq!(pool.staker, pool_authority_pda().to_bytes(), "staker = pool-authority PDA");
    assert_eq!(
        pool.stake_deposit_authority,
        deposit_authority_pda().to_bytes(),
        "stake deposit authority = deposit-authority PDA"
    );
    assert_eq!(
        pool.manager_fee_account,
        g.maintenance_vault.to_bytes(),
        "manager fee account = maintenance vault"
    );

    // The REAL Initialize counted the reserve's above-rent funding as pre-existing pool value:
    // rate 1 with the bootstrap supply minted to the maintenance vault.
    assert_eq!(pool.total_lamports, RESERVE_BOOTSTRAP_LAMPORTS);
    assert_eq!(pool.pool_token_supply, RESERVE_BOOTSTRAP_LAMPORTS);
    assert_eq!(token_balance(&svm, &g.maintenance_vault), RESERVE_BOOTSTRAP_LAMPORTS);
    assert_eq!(mint_supply(&svm, &g.fusol_mint), RESERVE_BOOTSTRAP_LAMPORTS);
    assert_eq!(read_fork_validator_list_len(&svm, &g.validator_list), 0);

    // deposit_sol THROUGH the controller: 10 SOL at rate 1 → 10 fuSOL minted, 5 bps to the
    // maintenance vault (the manager fee account), the rest to the depositor.
    let depositor = Keypair::new();
    airdrop_sol(&mut svm, &depositor.pubkey(), 100);
    let user_ata =
        create_ata_and_fund(&mut svm, &depositor, &depositor.pubkey(), &g.fusol_mint, None, 0);
    let lamports = 10 * LAMPORTS_PER_SOL;
    let meta = send(
        &mut svm,
        &[ctrl_deposit_sol_ix(&depositor.pubkey(), &g, &user_ata, lamports)],
        &depositor,
        &[],
    )
    .expect("deposit_sol through the controller against the real dumped processor");

    let fee = lamports * SOL_DEPOSIT_FEE_BPS / FEE_BPS_DENOMINATOR; // 5 bps = 5_000_000
    assert_eq!(token_balance(&svm, &user_ata), lamports - fee, "depositor gets 1:1 minus 5 bps");
    assert_eq!(
        token_balance(&svm, &g.maintenance_vault),
        RESERVE_BOOTSTRAP_LAMPORTS + fee,
        "fee shares land in the maintenance vault"
    );

    // Canonical pool totals via fusion_stake_view: still rate 1, both legs grew by the deposit.
    let pool = read_fork_stake_pool(&svm, &g.stake_pool);
    assert_eq!(pool.total_lamports, RESERVE_BOOTSTRAP_LAMPORTS + lamports);
    assert_eq!(pool.pool_token_supply, RESERVE_BOOTSTRAP_LAMPORTS + lamports);
    assert_eq!(mint_supply(&svm, &g.fusol_mint), RESERVE_BOOTSTRAP_LAMPORTS + lamports);

    // The controller's own event rode the event-CPI transport.
    let ev: PoolDeposit = single_event(&meta);
    assert_eq!(ev.kind, DEPOSIT_KIND_SOL);
    assert_eq!(ev.lamports, lamports);
    assert_eq!(ev.depositor, depositor.pubkey());

    // A REAL vote account (native vote-program builtin, V3 layout) passes register_validator's
    // owner check + fail-closed VoteState parse; the record starts Registered with no list slot.
    let node = Keypair::new();
    let vote_kp = Keypair::new();
    let vote = create_vote_account(&mut svm, &node, &vote_kp, 5);
    send(&mut svm, &[ctrl_register_validator_ix(&payer.pubkey(), &vote)], &payer, &[])
        .expect("register_validator accepts a real vote account");
    let rec = read_validator_record(&svm, &vote);
    assert_eq!(rec.vote_account, vote);
    assert_eq!(rec.validator_list_index, VALIDATOR_LIST_INDEX_UNSET, "registered ≠ admitted");
    assert_eq!(rec.status, 0, "ValidatorStatus::Registered");

    // warp_epochs drives the real epoch machinery: start_epoch (which requires
    // `clock.epoch > controller_epoch`) fails before the warp and enters RECONCILE after it.
    let f = send(&mut svm, &[ctrl_start_epoch_ix()], &payer, &[])
        .expect_err("start_epoch before any epoch advance must fail");
    assert_eq!(custom_code(&f), E_CTRL_EPOCH_NOT_ADVANCED);
    warp_epochs(&mut svm, 1);
    send(&mut svm, &[ctrl_start_epoch_ix()], &payer, &[]).expect("start_epoch after warp");
    let es = read_epoch_state(&svm);
    assert_eq!(es.controller_epoch, 1);
    assert_eq!(es.phase, fusion_stake_controller::state::PHASE_RECONCILE);
}
