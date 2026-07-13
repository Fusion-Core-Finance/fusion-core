//! `guardian_derisk` — the independent emergency brake (fusion-docs §7.2).
//!
//! The guardian (independent of the governance authority) can pause NEW borrowing on a market without the
//! governance timelock. The envelope is constitutional: it pauses ONLY `borrow`, auto-lifts, and
//! can never touch existing positions, user funds, repay, withdraw, liquidation, or redemption.
//! These tests pin both the gate and that de-risk-only guarantee. Requires the dev-oracle `.so`.

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

/// Bootstrap a market with a live price (via `dev_set_price`) so borrowing is otherwise possible.
/// `gov` is also the guardian (`init_protocol` sets `guardian = gov`).
fn setup() -> (litesvm::LiteSVM, Keypair, Keypair, solana_sdk::pubkey::Pubkey) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("dev_set_price");
    (svm, gov, cma, coll)
}

#[test]
fn guardian_pause_blocks_borrow() {
    let (mut svm, gov, cma, coll) = setup();
    let a = open_borrower_rate(&mut svm, &cma, &coll, 10, 0, 500); // open + deposit, no borrow
    send(&mut svm, &[guardian_derisk_ix(&gov.pubkey(), &coll, 3600)], &gov, &[]).expect("pause");
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_GUARDIAN_PAUSED);
}

#[test]
fn guardian_derisk_rejects_non_guardian() {
    let (mut svm, _gov, _cma, coll) = setup();
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let f = send(&mut svm, &[guardian_derisk_ix(&rando.pubkey(), &coll, 3600)], &rando, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);
}

#[test]
fn guardian_derisk_clamps_pause_window() {
    let (mut svm, gov, _cma, coll) = setup();
    // Above the max.
    let over = fusd_core::constants::GUARDIAN_MAX_PAUSE_SECS + 1;
    let f = send(&mut svm, &[guardian_derisk_ix(&gov.pubkey(), &coll, over)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
    // Negative.
    let f = send(&mut svm, &[guardian_derisk_ix(&gov.pubkey(), &coll, -1)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
    // Exactly the max is accepted AND stored effectively (deadline = now + MAX).
    let max = fusd_core::constants::GUARDIAN_MAX_PAUSE_SECS;
    let now = now_unix(&svm);
    send(&mut svm, &[guardian_derisk_ix(&gov.pubkey(), &coll, max)], &gov, &[])
        .expect("max pause accepted");
    assert_eq!(read_market(&svm, &market_pda(&coll)).guardian_paused_until, now + max);
}

#[test]
fn guardian_pause_auto_lifts() {
    let (mut svm, gov, cma, coll) = setup();
    let a = open_borrower_rate(&mut svm, &cma, &coll, 10, 0, 500);
    send(&mut svm, &[guardian_derisk_ix(&gov.pubkey(), &coll, 3600)], &gov, &[]).expect("pause");
    // Blocked now.
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_GUARDIAN_PAUSED);
    // Past the window ⇒ auto-lifted, borrow succeeds (warp also expires the blockhash).
    warp_unix(&mut svm, 3601);
    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .expect("borrow after auto-lift");
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(100));
}

#[test]
fn guardian_lifts_pause_early() {
    let (mut svm, gov, cma, coll) = setup();
    let a = open_borrower_rate(&mut svm, &cma, &coll, 10, 0, 500);
    send(&mut svm, &[guardian_derisk_ix(&gov.pubkey(), &coll, 7200)], &gov, &[]).expect("pause");
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_GUARDIAN_PAUSED);
    // `pause_secs = 0` lifts immediately.
    send(&mut svm, &[guardian_derisk_ix(&gov.pubkey(), &coll, 0)], &gov, &[]).expect("lift");
    svm.expire_blockhash(); // the prior (failed) borrow tx is otherwise byte-identical
    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .expect("borrow after early lift");
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(100));
}

#[test]
fn guardian_pause_allows_repay_withdraw_redeem() {
    // The de-risk-only guarantee: while new borrowing is paused, every fund-returning / floor path
    // still works. A borrows $500 @ rate 500 (lower bucket); B borrows $300 @ rate 600 (holds fUSD).
    let (mut svm, gov, cma, coll) = setup();
    let a = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(500), 500);
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 600);

    send(&mut svm, &[guardian_derisk_ix(&gov.pubkey(), &coll, 7200)], &gov, &[]).expect("pause");

    // New debt is blocked (the contrast).
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(1))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_GUARDIAN_PAUSED);

    // Repay works while paused — observable: fUSD burned AND the position's debt drops.
    let art_before_repay = read_position(&svm, &a.position).recorded_debt;
    send(&mut svm, &[repay_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .expect("repay while paused");
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(400));
    assert!(read_position(&svm, &a.position).recorded_debt < art_before_repay, "repay reduced debt");

    // Withdraw works while paused — observable: collateral leaves the vault to A's ATA, ink drops.
    let ink_before = read_position(&svm, &a.position).ink;
    let a_coll_before = token_balance(&svm, &a.coll_ata);
    send(&mut svm, &[withdraw_ix(&a.kp.pubkey(), &coll, &a.coll_ata, whole_coll(1))], &a.kp, &[])
        .expect("withdraw while paused");
    assert_eq!(token_balance(&svm, &a.coll_ata), a_coll_before + whole_coll(1));
    assert_eq!(read_position(&svm, &a.position).ink, ink_before - whole_coll(1));

    // Redemption (the peg floor) works while paused: B redeems against A's lower-rate bucket —
    // observable: B's fUSD burned AND A's collateral/debt drop.
    let b_fusd_before = token_balance(&svm, &b.fusd_ata);
    let a_ink_before_redeem = read_position(&svm, &a.position).ink;
    send(
        &mut svm,
        &[redeem_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, &b.coll_ata, &[a.position], usd(50))],
        &b.kp,
        &[],
    )
    .expect("redeem while paused");
    assert_eq!(token_balance(&svm, &b.fusd_ata), b_fusd_before - usd(50));
    assert!(read_position(&svm, &a.position).ink < a_ink_before_redeem, "redeem took A's collateral");
}

#[test]
fn guardian_pause_allows_liquidation() {
    let (mut svm, gov, cma, coll) = setup();
    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(500)); // 10 tok, $500 debt
    let d = open_borrower(&mut svm, &cma, &coll, 30, usd(1000));
    provide_sp(&mut svm, &d, &coll, usd(1000));

    // Drop the price so A ($500 debt / 10 tok = $400 @ $40) is under MCR, then pause.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(40))], &gov, &[])
        .expect("drop price");
    send(&mut svm, &[guardian_derisk_ix(&gov.pubkey(), &coll, 7200)], &gov, &[]).expect("pause");

    // Liquidation IGNORES the guardian pause and succeeds.
    let liq = Keypair::new();
    airdrop_sol(&mut svm, &liq.pubkey(), 100);
    liquidate(&mut svm, &liq, &coll, &a.position).expect("liquidate while paused");
    assert_eq!(read_position(&svm, &a.position).recorded_debt, 0, "A fully liquidated while paused");
}

#[test]
fn set_guardian_rotates_and_proves_independence() {
    // Governance can rotate the guardian; the pause gate keys on `config.guardian`, NOT gov_authority.
    let (mut svm, gov, cma, coll) = setup();
    let g2 = Keypair::new();
    airdrop_sol(&mut svm, &g2.pubkey(), 100);

    // A non-gov signer cannot rotate the guardian.
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let f = send(&mut svm, &[set_guardian_ix(&rando.pubkey(), &g2.pubkey())], &rando, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);

    // gov_authority rotates the guardian to g2.
    send(&mut svm, &[set_guardian_ix(&gov.pubkey(), &g2.pubkey())], &gov, &[]).expect("set_guardian");
    assert_eq!(read_protocol_config(&svm).guardian, g2.pubkey());

    // The OLD guardian (gov, which is still gov_authority) can no longer pause — the gate is
    // `config.guardian`, not `gov_authority`. This is the independence property.
    let f = send(&mut svm, &[guardian_derisk_ix(&gov.pubkey(), &coll, 3600)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);

    // The NEW guardian g2 can pause, and the pause is effective.
    send(&mut svm, &[guardian_derisk_ix(&g2.pubkey(), &coll, 3600)], &g2, &[]).expect("g2 pauses");
    let a = open_borrower_rate(&mut svm, &cma, &coll, 10, 0, 500);
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_GUARDIAN_PAUSED);
}

#[test]
fn guardian_pause_is_per_market() {
    // Pausing market X must leave market Y borrowable (the flag lives on `Market`, per-collateral).
    let (mut svm, gov, cma, coll_x) = setup();

    // Stand up a second market Y in the same protocol (init_protocol already ran in setup()).
    let mint_y = Keypair::new();
    create_mint(&mut svm, &gov, &mint_y, COLL_DECIMALS, &cma.pubkey(), /*freeze=*/ false);
    let coll_y = mint_y.pubkey();
    send(
        &mut svm,
        &[init_market_ix(&gov.pubkey(), &coll_y, MCR_BPS, DEBT_CEILING, 0, 0, BUCKET_WIDTH_BPS, 0)],
        &gov,
        &[],
    )
    .expect("init market Y");
    send(&mut svm, &[init_reactor_pool_ix(&gov.pubkey(), &coll_y)], &gov, &[]).expect("init RP Y");
    send(&mut svm, &[init_insurance_buffer_ix(&gov.pubkey(), &coll_y)], &gov, &[])
        .expect("init buffer Y");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll_y, spot_for_usd(100))], &gov, &[])
        .expect("price Y");

    let ax = open_borrower_rate(&mut svm, &cma, &coll_x, 10, 0, 500);
    let ay = open_borrower_rate(&mut svm, &cma, &coll_y, 10, 0, 500);

    // Pause ONLY X.
    send(&mut svm, &[guardian_derisk_ix(&gov.pubkey(), &coll_x, 3600)], &gov, &[]).expect("pause X");

    // Borrow on X is blocked; borrow on Y is unaffected.
    let f = send(&mut svm, &[borrow_ix(&ax.kp.pubkey(), &coll_x, &ax.fusd_ata, usd(100))], &ax.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_GUARDIAN_PAUSED);
    send(&mut svm, &[borrow_ix(&ay.kp.pubkey(), &coll_y, &ay.fusd_ata, usd(100))], &ay.kp, &[])
        .expect("borrow on the unpaused market Y succeeds");
    assert_eq!(token_balance(&svm, &ay.fusd_ata), usd(100));
}
