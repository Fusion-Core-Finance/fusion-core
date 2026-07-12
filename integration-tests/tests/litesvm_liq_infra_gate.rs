//! L-02 liquidation-infrastructure borrow gate — the matrix `litesvm_cdp`'s lifecycle progression
//! (RP-then-buffer, flags 1 → 3 → 7) doesn't cover:
//!
//! 1. Order independence: buffer BEFORE ReactorPool (flags 1 → 5 → 7) — one bit is never enough,
//!    both inits in either order open borrowing.
//! 2. The legacy grandfather sentinel: a market whose `liq_infra_flags` byte is 0 (an account
//!    created before the field was carved from `_reserved` — the live WSOL market) keeps
//!    borrowing unchanged. This is the live-mainnet layout invariant.
//!
//! Requires the dev-oracle `.so`.

use anchor_lang::AccountSerialize;
use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

#[test]
fn buffer_before_reactor_pool_order_is_accepted() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();

    set_program_upgrade_authority(&mut svm, &gov.pubkey());
    send(&mut svm, &[init_protocol_ix(&gov.pubkey())], &gov, &[]).expect("init_protocol");
    let coll_mint = Keypair::new();
    create_mint(&mut svm, &gov, &coll_mint, COLL_DECIMALS, &cma.pubkey(), /*freeze=*/ false);
    let coll = coll_mint.pubkey();
    send(
        &mut svm,
        &[init_market_ix(&gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, 0, 0, BUCKET_WIDTH_BPS, 0)],
        &gov,
        &[],
    )
    .expect("init_market");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price");
    assert_eq!(read_market(&svm, &market_pda(&coll)).liq_infra_flags, 1, "born LIQ_INFRA_GATED");

    let a = open_borrower_rate(&mut svm, &cma, &coll, 10, 0, 500); // open + deposit, no borrow

    // Buffer FIRST: flags 1 → 5. One infra account alone must not open borrowing.
    send(&mut svm, &[init_insurance_buffer_ix(&gov.pubkey(), &coll)], &gov, &[])
        .expect("init buffer");
    assert_eq!(read_market(&svm, &market_pda(&coll)).liq_infra_flags, 1 | 4, "GATED | BUFFER");
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_LIQ_INFRA_NOT_READY);

    // ReactorPool second: flags 5 → 7 — ready, so init order never matters.
    send(&mut svm, &[init_reactor_pool_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("init rp");
    svm.expire_blockhash(); // the prior (failed) borrow tx is otherwise byte-identical
    assert_eq!(
        read_market(&svm, &market_pda(&coll)).liq_infra_flags,
        1 | 2 | 4,
        "GATED | RP | BUFFER"
    );
    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .expect("borrow opens once both infra accounts exist");
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(100));
}

#[test]
fn legacy_zero_flags_market_is_grandfathered() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma); // full infra: flags == 7
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price");
    let a = open_borrower_rate(&mut svm, &cma, &coll, 10, 0, 500);

    // Rewrite the market with `liq_infra_flags = 0`, byte-simulating an account created BEFORE the
    // field was carved (its reserve byte was zero-filled, so it decodes as 0). Same lamports/owner;
    // the length assert pins that the carve kept the serialized layout identical.
    let market_addr = market_pda(&coll);
    let mut m = read_market(&svm, &market_addr);
    assert_eq!(m.liq_infra_flags, 1 | 2 | 4);
    m.liq_infra_flags = 0;
    let mut acct = svm.get_account(&market_addr).unwrap();
    let mut data = Vec::with_capacity(acct.data.len());
    m.try_serialize(&mut data).expect("serialize Market");
    assert_eq!(data.len(), acct.data.len(), "carve must not change the account length");
    acct.data = data;
    svm.set_account(market_addr, acct).expect("overwrite market");

    // The 0 sentinel is grandfathered: borrow passes unchanged (the live WSOL market's path —
    // its RP + buffer already exist and the init PDAs can never re-run, so it stays 0 forever).
    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .expect("legacy 0-flag market must keep borrowing after the upgrade");
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(100));
    assert_eq!(read_market(&svm, &market_addr).liq_infra_flags, 0, "borrow never writes the flags");
}
