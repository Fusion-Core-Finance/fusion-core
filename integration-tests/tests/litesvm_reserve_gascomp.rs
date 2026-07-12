//! In-process litesvm integration test for the liquidation incentive layer (see fusion-docs):
//! the per-position SOL **reserve bond** (posted at open, paid to the liquidator on liquidation,
//! refunded on close) and the **collateral gas-comp** (a % of seized collateral skimmed to the
//! liquidator before the RP/redistribution split). Both are per-market, governance-adjustable
//! within compile-time clamps.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_reserve_gascomp

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

/// 0.5% of seized collateral goes to the liquidator; the RP gets the rest.
#[test]
fn gas_comp_skimmed_to_liquidator() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    // Gas-comp 0.5%, no reserve.
    let coll = bootstrap_market_full(&mut svm, &gov, &cma, 0, GAS_COMP_BPS);
    let market = market_pda(&coll);
    let coll_vault = coll_vault_pda(&coll);
    let reactor_coll_vault = reactor_coll_vault_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");
    // D funds the RP ($1000 -> full offset of B's $600). B: 10 tokens, $600.
    let d = open_borrower(&mut svm, &cma, &coll, 100, usd(1_000));
    provide_sp(&mut svm, &d, &coll, usd(1_000));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    // gov is the liquidator; the helper creates its collateral ATA.
    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate failed");

    let gas_comp = whole_coll(10) * GAS_COMP_BPS as u64 / 10_000; // 0.5% of 10 tokens = 50_000_000
    assert_eq!(token_balance(&svm, &ata(&gov.pubkey(), &coll)), gas_comp, "liquidator got 0.5%");
    assert_eq!(token_balance(&svm, &reactor_coll_vault), whole_coll(10) - gas_comp, "RP got the rest");

    let bp = read_position(&svm, &b.position);
    assert_eq!((bp.recorded_debt, bp.ink), (0, 0));
    let m = read_market(&svm, &market);
    assert_eq!(m.agg_recorded_debt, usd(1_000) as u128); // D's debt remains
    // total_collateral == vault, and only D's 100 tokens remain in the market (B's 10 split out).
    assert_eq!(m.total_collateral, whole_coll(100) as u128);
    assert_eq!(token_balance(&svm, &coll_vault) as u128, m.total_collateral);
}

/// The SOL bond is posted at open, paid to the liquidator on liquidation, and the leftover rent is
/// reclaimable via close_position.
#[test]
fn reserve_bond_paid_to_liquidator_then_rent_reclaimed() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    // Reserve 0.02 SOL, no gas-comp.
    let coll = bootstrap_market_full(&mut svm, &gov, &cma, RESERVE_LAMPORTS, 0);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");
    let d = open_borrower(&mut svm, &cma, &coll, 100, usd(1_000));
    provide_sp(&mut svm, &d, &coll, usd(1_000));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    // The bond was posted on B's position at open (recorded + held on top of rent).
    assert_eq!(read_position(&svm, &b.position).reserve_lamports, RESERVE_LAMPORTS);
    let pos_lamports_before = lamports(&svm, &b.position);

    // Dedicated liquidator so its balance delta is clean.
    let liq = Keypair::new();
    airdrop_sol(&mut svm, &liq.pubkey(), 1);
    let liq_before = lamports(&svm, &liq.pubkey());

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    liquidate(&mut svm, &liq, &coll, &b.position).expect("liquidate failed");

    // The position lost exactly the bond; the liquidator came out net positive (bond > its costs).
    assert_eq!(
        pos_lamports_before - lamports(&svm, &b.position),
        RESERVE_LAMPORTS,
        "bond left the position"
    );
    assert!(lamports(&svm, &liq.pubkey()) > liq_before, "liquidator received the bond (net positive)");
    assert_eq!(read_position(&svm, &b.position).reserve_lamports, 0, "bond consumed");

    // Owner reclaims the remaining rent by closing the now-empty position.
    let bp = read_position(&svm, &b.position);
    assert_eq!((bp.recorded_debt, bp.ink), (0, 0));
    send(&mut svm, &[close_position_ix(&b.kp.pubkey(), &coll)], &b.kp, &[]).expect("close failed");
    assert!(svm.get_account(&b.position).is_none(), "position closed");
}

/// A voluntarily wound-down position (never liquidated) refunds rent + the full bond on close.
#[test]
fn voluntary_close_refunds_rent_and_bond() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market_full(&mut svm, &gov, &cma, RESERVE_LAMPORTS, 0);

    // An actor opens a position (posts the bond) and never borrows.
    let actor = Keypair::new();
    airdrop_sol(&mut svm, &actor.pubkey(), 1);
    send(&mut svm, &[open_position_ix(&actor.pubkey(), &coll, 500)], &actor, &[]).expect("open");
    let pos = position_pda(&coll, &actor.pubkey());
    assert_eq!(read_position(&svm, &pos).reserve_lamports, RESERVE_LAMPORTS);

    let actor_before = lamports(&svm, &actor.pubkey());
    send(&mut svm, &[close_position_ix(&actor.pubkey(), &coll)], &actor, &[]).expect("close");
    assert!(svm.get_account(&pos).is_none(), "position closed");
    // Refund returns the bond plus the account rent (minus the close tx fee), so the net exceeds
    // the bond alone.
    assert!(
        lamports(&svm, &actor.pubkey()) >= actor_before + RESERVE_LAMPORTS,
        "refund returned at least the bond"
    );
}

/// init_market clamps both incentive params; a position can't be closed while it holds debt.
#[test]
fn init_market_clamps_incentives_and_close_requires_empty() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    set_program_upgrade_authority(&mut svm, &gov.pubkey());
    send(&mut svm, &[init_protocol_ix(&gov.pubkey())], &gov, &[]).expect("init_protocol");
    let coll_mint = Keypair::new();
    create_mint(&mut svm, &gov, &coll_mint, COLL_DECIMALS, &cma.pubkey(), /*freeze=*/ false);
    let coll = coll_mint.pubkey();

    // reserve > MAX_RESERVE_LAMPORTS (1 SOL) is rejected.
    let f = send(
        &mut svm,
        &[init_market_ix(&gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, 2_000_000_000, 0, BUCKET_WIDTH_BPS, 0)],
        &gov,
        &[],
    )
    .expect_err("reserve over clamp");
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);

    // gas-comp > MAX_LIQ_GAS_COMP_BPS (1000 bps) is rejected.
    let f2 = send(
        &mut svm,
        &[init_market_ix(&gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, 0, 2_000, BUCKET_WIDTH_BPS, 0)],
        &gov,
        &[],
    )
    .expect_err("gas-comp over clamp");
    assert_eq!(custom_code(&f2), E_PARAM_OUT_OF_BOUNDS);

    // Within bounds: succeeds and stores the values.
    send(
        &mut svm,
        &[init_market_ix(&gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, RESERVE_LAMPORTS, GAS_COMP_BPS, BUCKET_WIDTH_BPS, 0)],
        &gov,
        &[],
    )
    .expect("within bounds ok");
    send(&mut svm, &[init_reactor_pool_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("init rp");
    send(&mut svm, &[init_insurance_buffer_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("init buffer");
    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.reserve_lamports, RESERVE_LAMPORTS);
    assert_eq!(m.liq_gas_comp_bps, GAS_COMP_BPS);

    // A position holding debt cannot be closed.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price");
    let actor = open_borrower(&mut svm, &cma, &coll, 10, usd(100));
    let f3 = send(&mut svm, &[close_position_ix(&actor.kp.pubkey(), &coll)], &actor.kp, &[])
        .expect_err("close with debt must fail");
    assert_eq!(custom_code(&f3), E_POSITION_NOT_EMPTY);
}

/// The bond and the gas-comp compose: in one liquidation the liquidator earns both.
#[test]
fn reserve_and_gas_comp_compose() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market_full(&mut svm, &gov, &cma, RESERVE_LAMPORTS, GAS_COMP_BPS);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");
    let d = open_borrower(&mut svm, &cma, &coll, 100, usd(1_000));
    provide_sp(&mut svm, &d, &coll, usd(1_000));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    let liq = Keypair::new();
    airdrop_sol(&mut svm, &liq.pubkey(), 1);
    let liq_before = lamports(&svm, &liq.pubkey());
    let pos_before = lamports(&svm, &b.position);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    liquidate(&mut svm, &liq, &coll, &b.position).expect("liquidate failed");

    let gas_comp = whole_coll(10) * GAS_COMP_BPS as u64 / 10_000;
    // Collateral gas-comp landed in the liquidator's ATA...
    assert_eq!(token_balance(&svm, &ata(&liq.pubkey(), &coll)), gas_comp, "liquidator got the gas-comp");
    // ...and the SOL bond moved out of the position to the liquidator.
    assert_eq!(pos_before - lamports(&svm, &b.position), RESERVE_LAMPORTS, "bond paid out");
    assert!(lamports(&svm, &liq.pubkey()) > liq_before, "liquidator net positive on SOL");
    // RP took the rest of the collateral (10 tokens minus the gas-comp).
    assert_eq!(token_balance(&svm, &reactor_coll_vault_pda(&coll)), whole_coll(10) - gas_comp);
}

/// A position reused after liquidation (the account survives, bond consumed) re-posts the bond on
/// its next deposit, so the next liquidation still pays the liquidator. Guards the lifecycle gap
/// where a once-liquidated CDP would otherwise run bond-free forever.
#[test]
fn reused_position_reposts_bond_for_next_liquidation() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market_full(&mut svm, &gov, &cma, RESERVE_LAMPORTS, 0);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    // D funds the RP enough to absorb two liquidations.
    let d = open_borrower(&mut svm, &cma, &coll, 1_000, usd(2_000));
    provide_sp(&mut svm, &d, &coll, usd(2_000));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));
    assert_eq!(read_position(&svm, &b.position).reserve_lamports, RESERVE_LAMPORTS);

    let liq = Keypair::new();
    airdrop_sol(&mut svm, &liq.pubkey(), 10);

    // ---- liquidation #1: bond paid out, position left zeroed (bond consumed) ----
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("p80");
    let pos_before1 = lamports(&svm, &b.position);
    liquidate(&mut svm, &liq, &coll, &b.position).expect("liq1");
    assert_eq!(pos_before1 - lamports(&svm, &b.position), RESERVE_LAMPORTS, "bond #1 paid");
    assert_eq!(read_position(&svm, &b.position).reserve_lamports, 0, "bond consumed");

    // A fresh blockhash so the second cycle's txs aren't byte-identical replays of the first.
    svm.expire_blockhash();

    // ---- reuse the SAME position: deposit re-posts the bond, then borrow again ----
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100b");
    fund_and_deposit(&mut svm, &cma, &coll, &b, whole_coll(10));
    assert_eq!(
        read_position(&svm, &b.position).reserve_lamports,
        RESERVE_LAMPORTS,
        "deposit re-posted the bond on the reused position"
    );
    send(&mut svm, &[borrow_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(600))], &b.kp, &[]).expect("borrow2");

    // ---- liquidation #2: still pays a bond (the gap is closed) ----
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("p80b");
    let pos_before2 = lamports(&svm, &b.position);
    let liq_before2 = lamports(&svm, &liq.pubkey());
    liquidate(&mut svm, &liq, &coll, &b.position).expect("liq2");
    assert_eq!(pos_before2 - lamports(&svm, &b.position), RESERVE_LAMPORTS, "bond #2 paid (reuse re-bonded)");
    assert!(lamports(&svm, &liq.pubkey()) > liq_before2, "liquidator received bond #2 (net positive)");
}

/// The under-bond top-up debits lamports from `owner` via a system transfer, so `owner`
/// must be writable even when a THIRD PARTY pays the tx fee. Pre-fix (`owner` lacked `#[account(mut)]`)
/// this reverted whenever the fee payer differed from the owner; the existing reuse tests missed it
/// because there owner == fee payer (always writable). Here a separate fee payer drives the gap.
#[test]
fn deposit_bond_topup_succeeds_with_separate_fee_payer() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market_full(&mut svm, &gov, &cma, RESERVE_LAMPORTS, 0);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    // RP absorbs the liquidation; B borrows then is liquidated, leaving its position under-bonded.
    let d = open_borrower(&mut svm, &cma, &coll, 1_000, usd(2_000));
    provide_sp(&mut svm, &d, &coll, usd(2_000));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("p80");
    let liq = Keypair::new();
    airdrop_sol(&mut svm, &liq.pubkey(), 10);
    liquidate(&mut svm, &liq, &coll, &b.position).expect("liq");
    assert_eq!(read_position(&svm, &b.position).reserve_lamports, 0, "bond consumed → under-bonded");
    svm.expire_blockhash();

    // Re-deposit with owner ≠ fee payer: owner signs (and funds the bond), a separate payer pays fees.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100b");
    fund_collateral(&mut svm, &cma, &coll, &b, whole_coll(10));
    airdrop_sol(&mut svm, &b.kp.pubkey(), 10); // owner must hold lamports for the bond transfer itself
    let fee_payer = Keypair::new();
    airdrop_sol(&mut svm, &fee_payer.pubkey(), 10);
    send(
        &mut svm,
        &[deposit_ix(&b.kp.pubkey(), &coll, &b.coll_ata, whole_coll(10))],
        &fee_payer,   // fee payer (writable for fees)
        &[&b.kp],     // owner signs but is NOT the fee payer
    )
    .expect("deposit top-up with a separate fee payer (owner writable via #[account(mut)])");
    assert_eq!(read_position(&svm, &b.position).reserve_lamports, RESERVE_LAMPORTS, "bond re-posted");
}

/// The liquidator's gas-comp sink is pinned to the liquidator (`token::authority = liquidator`), so
/// it cannot alias a program vault (which would desync `total_collateral` from the vault balance).
#[test]
fn liquidate_rejects_aliased_gas_comp_sink() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market_full(&mut svm, &gov, &cma, 0, GAS_COMP_BPS);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    let d = open_borrower(&mut svm, &cma, &coll, 1_000, usd(1_000));
    provide_sp(&mut svm, &d, &coll, usd(1_000));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("p80");

    // Point the gas-comp sink at the market's own collateral vault (authority = market PDA).
    let ix = liquidate_ix(&gov.pubkey(), &coll, &b.position, &coll_vault_pda(&coll));
    let f = send(&mut svm, &[ix], &gov, &[]).expect_err("aliased gas-comp sink must be rejected");
    // Anchor token-constraint violation (framework error, code >= 2000).
    assert!(custom_code(&f) >= 2000, "expected an Anchor account-constraint error");
}
