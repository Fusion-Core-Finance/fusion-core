//! In-process litesvm tests for the per-market **insurance buffer** — the third liquidation
//! loss-absorption tier (RP → redistribution → buffer → un-homed; fusion-docs.md) and its
//! funding hook. Exercises: `fund_buffer` accounting; the buffer fully absorbing a liquidation the
//! RP and redistribution can't (no shutdown); the fail-closed haircut → terminal shutdown when the
//! buffer is short; and that buffer fUSD is excluded from market backing (it never makes a position
//! healthy).
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_buffer

use fusd_core::constants::SHUTDOWN_REASON_UNHOMED_BAD_DEBT;
use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

/// `fund_buffer` moves fUSD into the buffer vault and accumulates `total_funded`; a zero amount reverts.
#[test]
fn fund_buffer_accumulates() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("set price $100");

    // A borrower mints fUSD and donates some to the buffer.
    let f = open_borrower(&mut svm, &coll_mint_auth, &coll, 20, usd(700));
    assert_eq!(buffer_balance(&svm, &coll), 0, "buffer starts empty (no treasury seed)");

    send(&mut svm, &[fund_buffer_ix(&f.kp.pubkey(), &coll, &f.fusd_ata, usd(500))], &f.kp, &[])
        .expect("fund 500");
    assert_eq!(buffer_balance(&svm, &coll), usd(500));
    assert_eq!(read_insurance_buffer(&svm, &coll).total_funded, usd(500) as u128);

    send(&mut svm, &[fund_buffer_ix(&f.kp.pubkey(), &coll, &f.fusd_ata, usd(150))], &f.kp, &[])
        .expect("fund 150 more");
    assert_eq!(buffer_balance(&svm, &coll), usd(650), "funding accumulates");
    assert_eq!(read_insurance_buffer(&svm, &coll).total_funded, usd(650) as u128);

    let err = send(&mut svm, &[fund_buffer_ix(&f.kp.pubkey(), &coll, &f.fusd_ata, 0)], &f.kp, &[])
        .expect_err("zero fund reverts");
    assert_eq!(custom_code(&err), 6007 /* ZeroAmount */);
}

/// The buffer fully absorbs a liquidation the RP and redistribution can't: the RP is empty and the
/// victim is the ONLY position (no redistribution recipient), but the buffer holds enough fUSD to
/// burn the whole debt. The liquidation succeeds WITHOUT shutdown; the debt is extinguished (no bad
/// debt) and the seized collateral becomes protocol-owned. Also asserts the buffer is NOT backing —
/// funding it does not change `total_collateral` and does not make the victim healthy.
#[test]
fn buffer_absorbs_when_no_recipients() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("set price $100");

    // F is the ONLY position: 20 tokens, $700 debt (healthy at $100). It funds the buffer with the
    // full $700 it borrowed, so the buffer can later burn exactly F's debt.
    let f = open_borrower(&mut svm, &coll_mint_auth, &coll, 20, usd(700));
    let coll_before = read_market(&svm, &market).total_collateral;
    send(&mut svm, &[fund_buffer_ix(&f.kp.pubkey(), &coll, &f.fusd_ata, usd(700))], &f.kp, &[])
        .expect("fund the buffer with 700");
    // Buffer fUSD is NOT backing: funding it left total_collateral unchanged.
    assert_eq!(read_market(&svm, &market).total_collateral, coll_before, "buffer is not collateral");
    assert_eq!(buffer_balance(&svm, &coll), usd(700));

    // Crash to $50: F is underwater (20·$50 = $1000 vs $700 → 142% < 150% MCR) — the funded buffer
    // does NOT make it healthy.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(50))], &gov, &[])
        .expect("crash to $50");

    liquidate(&mut svm, &gov, &coll, &f.position).expect("buffer absorbs the liquidation");

    let m = read_market(&svm, &market);
    assert!(!m.shutdown, "buffer fully covered — NOT a terminal shutdown");
    assert_eq!(m.bad_debt, 0, "no un-homed bad debt");
    assert_eq!(m.agg_recorded_debt, 0, "the debt is fully extinguished");
    assert_eq!(buffer_balance(&svm, &coll), 0, "the buffer burned its fUSD to cover the debt");
    assert_eq!(read_insurance_buffer(&svm, &coll).total_absorbed, usd(700) as u128);
    // F is liquidated; its post-RP collateral stays protocol-owned — now tracked in `protocol_collateral`
    // (recoverable via `sweep_protocol_collateral`), NOT `total_collateral` (which backs only live positions).
    let p = read_position(&svm, &f.position);
    assert_eq!(p.recorded_debt, 0);
    assert_eq!(p.ink, 0);
    assert_eq!(m.total_collateral, 0, "no live position remains to back collateral");
    assert_eq!(m.protocol_collateral, token_balance(&svm, &coll_vault_pda(&coll)), "retained protocol collateral == vault");
    assert_vault_invariant(&svm, &coll);
}

/// Fail-closed haircut → terminal shutdown: the buffer holds LESS than the un-absorbable debt, so it
/// burns what it has and the residual is realized as un-homed bad debt, tripping the per-market
/// shutdown (the `urgent_redeem` wind-down). Liquidation still succeeds — it never stalls.
#[test]
fn buffer_haircut_then_shutdown() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("set price $100");

    // F borrows $700 but funds the buffer with only $400 (keeping $300, which becomes the realized
    // bad debt). F is the only position; the RP is empty.
    let f = open_borrower(&mut svm, &coll_mint_auth, &coll, 20, usd(700));
    send(&mut svm, &[fund_buffer_ix(&f.kp.pubkey(), &coll, &f.fusd_ata, usd(400))], &f.kp, &[])
        .expect("fund the buffer with 400");

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(50))], &gov, &[])
        .expect("crash to $50");

    liquidate(&mut svm, &gov, &coll, &f.position).expect("haircut + shutdown, never a revert");

    let m = read_market(&svm, &market);
    assert!(m.shutdown, "the residual trips the terminal shutdown");
    assert_eq!(m.shutdown_reason, SHUTDOWN_REASON_UNHOMED_BAD_DEBT);
    assert_eq!(m.bad_debt, usd(300) as u128, "the uncovered $300 is realized as un-homed bad debt");
    assert_eq!(m.agg_recorded_debt, 0, "no debt remains in the aggregate (buffer-burned + un-homed)");
    assert_eq!(buffer_balance(&svm, &coll), 0, "the buffer is drained (fail-closed haircut)");
    assert_eq!(read_insurance_buffer(&svm, &coll).total_absorbed, usd(400) as u128);
    let p = read_position(&svm, &f.position);
    assert_eq!(p.recorded_debt, 0);
    assert_eq!(p.ink, 0);
    // Vault invariant survives the haircut: the protocol-owned remainder is accounted in
    // `protocol_collateral` (recoverable via sweep), not `total_collateral`.
    assert_eq!(m.total_collateral, 0, "no live position remains to back collateral");
    assert_eq!(m.protocol_collateral, token_balance(&svm, &coll_vault_pda(&coll)), "retained protocol collateral == vault");
    assert_vault_invariant(&svm, &coll);
}

/// Regression for the snapshot-poison bug: after the buffer fully absorbs the LAST position (market
/// stays live — no shutdown), the redistribution snapshots must NOT be left as
/// `(total_stakes=0, total_collateral=coll_r>0)`, which would make `compute_stake` return 0 for every
/// future position and permanently brick tier-2 redistribution. A fresh borrower must receive a
/// non-zero stake and re-arm `total_stakes`.
#[test]
fn tier2_rearms_after_buffer_absorb_on_a_live_market() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("set price $100");

    // F is the only position; it funds the buffer with its full debt, then is liquidated — the buffer
    // covers it entirely, so the market is NOT shut down (it stays live).
    let f = open_borrower(&mut svm, &coll_mint_auth, &coll, 20, usd(700));
    send(&mut svm, &[fund_buffer_ix(&f.kp.pubkey(), &coll, &f.fusd_ata, usd(700))], &f.kp, &[])
        .expect("fund 700");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(50))], &gov, &[])
        .expect("crash to $50");
    liquidate(&mut svm, &gov, &coll, &f.position).expect("buffer absorbs");
    assert!(!read_market(&svm, &market).shutdown, "buffer covered — market stays live");
    assert_eq!(read_market(&svm, &market).total_stakes, 0, "no positions left after the absorb");

    // A fresh borrower opens on the still-live market (the $50 price is still fresh; G is healthy at
    // 10·$50 = $500 vs $200 debt). It MUST get a non-zero stake (the poisoned snapshot was correctly
    // excluded), re-arming tier-2 redistribution.
    let g = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(200));
    let gp = read_position(&svm, &g.position);
    assert!(gp.stake > 0, "fresh position must get a non-zero stake (tier-2 re-armed)");
    assert_eq!(read_market(&svm, &market).total_stakes, gp.stake, "total_stakes re-armed");
}
