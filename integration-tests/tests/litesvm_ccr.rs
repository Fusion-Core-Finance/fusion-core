//! CCR borrow-restriction band — the mild, non-reflexive RM alternative (fusion-docs.md).
//!
//! When the market's aggregate TCR is below CCR, only risk-increasing ops (borrow, withdraw) are
//! blocked; de-risking ops (deposit, repay) and the peg floor (liquidation, redemption) stay open,
//! the band fails open on a stale price, and it NEVER expands the liquidatable set. CCR is a
//! governable `MarketParam` (0 = disabled); tests enable it via the gate. MCR is 150% (bootstrap),
//! so the tests use CCR = 200% for a clear margin that isolates the band from the per-position MCR.
//! Requires the dev-oracle `.so`.

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

const CCR_200: u64 = 20_000; // 200% (well above the 150% MCR)

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
fn borrow_blocked_when_it_would_leave_market_below_ccr() {
    let (mut svm, gov, cma, coll) = setup();
    // Borrow $400 (CR 250%) with the band OFF, then enable CCR = 200%.
    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(400));
    enable_ccr(&mut svm, &gov, &coll, CCR_200);

    // A borrow that keeps post-op TCR >= CCR is allowed: $450 ⇒ 1000/450 = 222%.
    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(50))], &a.kp, &[])
        .expect("borrow keeping TCR >= CCR");
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(450));

    // A borrow that would drop post-op TCR below CCR is blocked — even though the position stays
    // above its 150% MCR ($550 ⇒ 1000/550 = 182%): the block is purely the CCR band.
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_CCR_RESTRICTED);
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(450), "the over-CCR borrow minted nothing");
}

#[test]
fn withdraw_blocked_for_debt_free_position_when_market_stressed() {
    // A debt-free position has no MCR check, so its withdrawal is gated ONLY by the CCR band —
    // a clean isolation of the band (and proof debt-free positions are covered).
    let (mut svm, gov, cma, coll) = setup();
    let _a = open_borrower(&mut svm, &cma, &coll, 10, usd(400)); // gives the market a TCR
    let c = open_borrower_rate(&mut svm, &cma, &coll, 5, 0, 500); // debt-free depositor
    enable_ccr(&mut svm, &gov, &coll, CCR_200);

    // Crash so the market is stressed: 15 tok @ $35 = $525 vs $400 debt ⇒ TCR 131% < 200%.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(35))], &gov, &[])
        .expect("drop price");

    let f = send(&mut svm, &[withdraw_ix(&c.kp.pubkey(), &coll, &c.coll_ata, whole_coll(1))], &c.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_CCR_RESTRICTED);
}

#[test]
fn withdraw_gate_keys_off_post_op_tcr() {
    // Same position, same price, two amounts: the gate keys off POST-op TCR, not a market-state flag.
    let (mut svm, gov, cma, coll) = setup();
    let _a = open_borrower(&mut svm, &cma, &coll, 7, usd(400)); // 7 tok, CR 175% > MCR
    let c = open_borrower_rate(&mut svm, &cma, &coll, 3, 0, 500); // debt-free, 3 tok
    enable_ccr(&mut svm, &gov, &coll, CCR_200);
    // Market: 10 tok @ $100 = $1000 vs $400 debt ⇒ TCR 250% (not stressed).

    // Withdrawing all 3 of C's tokens ⇒ post-op 7 tok = $700 ⇒ TCR 175% < 200% ⇒ blocked.
    let f = send(&mut svm, &[withdraw_ix(&c.kp.pubkey(), &coll, &c.coll_ata, whole_coll(3))], &c.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_CCR_RESTRICTED);
    // Withdrawing 1 token ⇒ post-op 9 tok = $900 ⇒ TCR 225% ≥ 200% ⇒ allowed.
    send(&mut svm, &[withdraw_ix(&c.kp.pubkey(), &coll, &c.coll_ata, whole_coll(1))], &c.kp, &[])
        .expect("a withdrawal that keeps post-op TCR >= CCR is allowed");
}

#[test]
fn ccr_band_fails_open_on_stale_price() {
    let (mut svm, gov, cma, coll) = setup();
    let _a = open_borrower(&mut svm, &cma, &coll, 10, usd(400));
    let c = open_borrower_rate(&mut svm, &cma, &coll, 5, 0, 500);
    enable_ccr(&mut svm, &gov, &coll, CCR_200);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(35))], &gov, &[])
        .expect("drop price");

    // Fresh + stressed ⇒ blocked.
    let f = send(&mut svm, &[withdraw_ix(&c.kp.pubkey(), &coll, &c.coll_ata, whole_coll(1))], &c.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_CCR_RESTRICTED);

    // Age the price out: the CCR band fails OPEN (a dead oracle can't grief-freeze withdrawals),
    // and a debt-free withdraw has no staleness gate of its own, so it now succeeds.
    warp_slots(&mut svm, fusd_core::constants::MAX_PRICE_STALENESS_SLOTS + 1);
    let c_coll_before = token_balance(&svm, &c.coll_ata);
    send(&mut svm, &[withdraw_ix(&c.kp.pubkey(), &coll, &c.coll_ata, whole_coll(1))], &c.kp, &[])
        .expect("CCR fails open on a stale price");
    assert_eq!(token_balance(&svm, &c.coll_ata), c_coll_before + whole_coll(1));
}

#[test]
fn deposit_and_repay_stay_open_when_market_stressed() {
    let (mut svm, gov, cma, coll) = setup();
    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(400));
    enable_ccr(&mut svm, &gov, &coll, CCR_200);
    // Market stressed (TCR 250% -> below 200% once we drop the price).
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(50))], &gov, &[])
        .expect("drop price"); // 1000... 10 tok @ $50 = $500 vs $400 ⇒ TCR 125% < 200%

    // Repay (de-risking) is never CCR-gated.
    let art_before = read_position(&svm, &a.position).recorded_debt;
    send(&mut svm, &[repay_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .expect("repay stays open when stressed");
    assert!(read_position(&svm, &a.position).recorded_debt < art_before);

    // Deposit (de-risking) is never CCR-gated.
    let tc_before = read_market(&svm, &market_pda(&coll)).total_collateral;
    fund_and_deposit(&mut svm, &cma, &coll, &a, whole_coll(2));
    assert!(read_market(&svm, &market_pda(&coll)).total_collateral > tc_before);
}

#[test]
fn ccr_band_does_not_expand_the_liquidatable_set() {
    // The anti-Recovery-Mode invariant: a position ABOVE its MCR is NOT liquidatable, even when the
    // market is below CCR. CCR only restricts risk-increasing ops; it never changes liquidation.
    let (mut svm, gov, cma, coll) = setup();
    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(600)); // CR 167% > 150% MCR
    enable_ccr(&mut svm, &gov, &coll, CCR_200); // market TCR 167% < 200% ⇒ stressed

    // The band IS live (proves the precondition is load-bearing): a borrow into the already-below-CCR
    // market reverts on CCR — and on CCR specifically, since A's CR (167%) stays above its 150% MCR.
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(1))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_CCR_RESTRICTED);

    // Yet the SAME above-MCR position is NOT liquidatable — the band never expands liquidation.
    let liq = Keypair::new();
    airdrop_sol(&mut svm, &liq.pubkey(), 100);
    let f = liquidate(&mut svm, &liq, &coll, &a.position).unwrap_err();
    assert_eq!(custom_code(&f), E_POSITION_HEALTHY, "a healthy position is not liquidatable in the band");
}

#[test]
fn disabled_when_ccr_zero() {
    // No enable_ccr ⇒ ccr_bps stays 0 ⇒ the band is off and a borrow down to MCR is allowed.
    let (mut svm, _gov, cma, coll) = setup();
    // $660 against 10 tok @ $100 ⇒ CR 151.5% (>= 150% MCR), which the CCR band (if on at ~155%+)
    // would block — but it's off.
    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(660));
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(660));
}

#[test]
fn ccr_param_is_clamped() {
    let min = fusd_core::constants::MIN_CCR_BPS; // 100%
    let max = fusd_core::constants::MAX_CCR_BPS; // 300%
    let (mut svm, gov, _cma, coll) = setup();
    init_gov_gate(&mut svm, &gov);

    // Just-below MIN and just-above MAX are both rejected at queue (fail-fast). A failed queue
    // reverts before consuming the nonce, so both use nonce 0; distinct values ⇒ distinct txs.
    for bad in [(min as u64) - 1, (max as u64) + 1] {
        let f = send(
            &mut svm,
            &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::Ccr, bad)],
            &gov,
            &[],
        )
        .unwrap_err();
        assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
    }

    // Both bounds and 0 (disable) are accepted and applied.
    gov_set_param(&mut svm, &gov, &coll, MarketParam::Ccr, min as u64);
    assert_eq!(read_market(&svm, &market_pda(&coll)).ccr_bps, min);
    gov_set_param(&mut svm, &gov, &coll, MarketParam::Ccr, max as u64);
    assert_eq!(read_market(&svm, &market_pda(&coll)).ccr_bps, max);
    gov_set_param(&mut svm, &gov, &coll, MarketParam::Ccr, 0);
    assert_eq!(read_market(&svm, &market_pda(&coll)).ccr_bps, 0);
}
