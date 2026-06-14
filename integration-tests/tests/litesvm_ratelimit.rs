//! Net-outflow rate limiter — the leaky bucket on net fUSD issuance (see fusion-docs).
//!
//! `borrow` consumes bucket capacity, `repay` restores it, the bucket refills over the window, and
//! liquidation/redemption are hard-exempt. The cap is a governable `MarketParam` (0 = disabled);
//! tests enable it through the GovernanceGate (which also exercises the fast loosen-path). Requires
//! the dev-oracle `.so`.

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

fn window() -> i64 {
    fusd_core::constants::RATELIMIT_WINDOW_SECS
}

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
fn borrow_throttled_at_cap() {
    let (mut svm, gov, cma, coll) = setup();
    enable_rate_limit(&mut svm, &gov, &coll, usd(1000));
    // 20 tok @ $100 = $2000 collateral (max debt $1333 at 150% MCR) ⇒ MCR is never the blocker.
    let a = open_borrower_rate(&mut svm, &cma, &coll, 20, 0, 500);

    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(600))], &a.kp, &[])
        .expect("first borrow consumes 600 of 1000");
    // 600 + 500 = 1100 > the 1000 cap ⇒ rejected purely by the limiter (MCR would allow it).
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(500))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_RATE_LIMIT_EXCEEDED);
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(600), "only the first borrow landed");
}

#[test]
fn repay_restores_capacity() {
    let (mut svm, gov, cma, coll) = setup();
    enable_rate_limit(&mut svm, &gov, &coll, usd(1000));
    let a = open_borrower_rate(&mut svm, &cma, &coll, 20, 0, 500);

    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(1000))], &a.kp, &[])
        .expect("borrow to the cap");
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(1))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_RATE_LIMIT_EXCEEDED);

    // Repay restores capacity by the burned amount (the "net" in net-outflow).
    send(&mut svm, &[repay_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(400))], &a.kp, &[])
        .expect("repay restores 400");
    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(300))], &a.kp, &[])
        .expect("300 fits the restored capacity");
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(900)); // 1000 - 400 + 300
}

#[test]
fn capacity_refills_over_window() {
    let (mut svm, gov, cma, coll) = setup();
    enable_rate_limit(&mut svm, &gov, &coll, usd(1000));
    let a = open_borrower_rate(&mut svm, &cma, &coll, 20, 0, 500);

    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(1000))], &a.kp, &[])
        .expect("borrow to the cap");
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(1))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_RATE_LIMIT_EXCEEDED);

    // A full window fully refills the bucket.
    warp_unix(&mut svm, window());
    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(300))], &a.kp, &[])
        .expect("bucket refilled after a full window");
}

#[test]
fn disabled_when_cap_zero() {
    // No enable_rate_limit ⇒ rl_cap stays 0 ⇒ the limiter is off and a large borrow is unthrottled.
    let (mut svm, _gov, cma, coll) = setup();
    let a = open_borrower_rate(&mut svm, &cma, &coll, 100, 0, 500); // $10k collateral
    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(5000))], &a.kp, &[])
        .expect("no limit when cap is 0");
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(5000));
}

#[test]
fn redemption_is_exempt_from_the_limiter() {
    let (mut svm, gov, cma, coll) = setup();
    enable_rate_limit(&mut svm, &gov, &coll, usd(1000));
    // A @ 5% and B @ 6% each borrow $500 ⇒ the bucket is exactly full (1000).
    let a = open_borrower_rate(&mut svm, &cma, &coll, 20, usd(500), 500);
    let b = open_borrower_rate(&mut svm, &cma, &coll, 20, usd(500), 600);

    // Bucket full ⇒ a fresh borrow is rejected.
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(1))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_RATE_LIMIT_EXCEEDED);

    // B redeems against A's (lowest) bucket — redemption burns fUSD but is HARD-EXEMPT: it succeeds
    // and does NOT restore the bucket.
    send(
        &mut svm,
        &[redeem_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, &b.coll_ata, &[a.position], usd(200))],
        &b.kp,
        &[],
    )
    .expect("redemption is not blocked by the limiter");

    // The bucket is still full (redemption did not restore it): another borrow still fails.
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(2))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_RATE_LIMIT_EXCEEDED);
}

#[test]
fn lowering_cap_clamps_accrued() {
    // Regression for the review fix: lowering the cap below the stored pressure clamps `rl_accrued`
    // down to the new cap (so the `rl_accrued <= rl_cap` invariant holds; it then drains at the
    // new rate), rather than leaving it transiently above the cap.
    let (mut svm, gov, cma, coll) = setup();
    enable_rate_limit(&mut svm, &gov, &coll, usd(1000));
    let a = open_borrower_rate(&mut svm, &cma, &coll, 20, 0, 500);
    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(800))], &a.kp, &[])
        .expect("consume 800 of 1000");

    // Lower the cap to 500 (< the 800 of accrued pressure).
    gov_set_param(&mut svm, &gov, &coll, MarketParam::RateLimitCap, usd(500));
    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.rl_cap, usd(500));
    assert_eq!(m.rl_accrued, usd(500), "stored pressure clamped to the new cap");

    // The bucket is now full at the new cap ⇒ a further borrow is rejected.
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(1))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_RATE_LIMIT_EXCEEDED);
}

#[test]
fn liquidation_is_exempt_from_the_limiter() {
    let (mut svm, gov, cma, coll) = setup();
    enable_rate_limit(&mut svm, &gov, &coll, usd(1000));
    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(500)); // consumes 500
    let d = open_borrower(&mut svm, &cma, &coll, 30, usd(500)); // consumes 500 ⇒ bucket full
    provide_sp(&mut svm, &d, &coll, usd(500));

    // Crash so A (10 tok @ $60 = $600 vs $500 debt) is under MCR but not underwater.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(60))], &gov, &[])
        .expect("drop price");

    // Liquidation is HARD-EXEMPT: it succeeds (not blocked) and does NOT restore the bucket.
    let liq = Keypair::new();
    airdrop_sol(&mut svm, &liq.pubkey(), 100);
    liquidate(&mut svm, &liq, &coll, &a.position).expect("liquidation is not blocked by the limiter");
    assert_eq!(read_position(&svm, &a.position).recorded_debt, 0);

    // Bucket still full (liquidation didn't restore it): a borrow is still rejected.
    let f = send(&mut svm, &[borrow_ix(&d.kp.pubkey(), &coll, &d.fusd_ata, usd(1))], &d.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_RATE_LIMIT_EXCEEDED);
}

#[test]
fn governance_loosen_path_raises_the_cap() {
    let (mut svm, gov, cma, coll) = setup();
    enable_rate_limit(&mut svm, &gov, &coll, usd(500)); // a low cap
    let a = open_borrower_rate(&mut svm, &cma, &coll, 30, 0, 500); // $3000 collateral

    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(500))], &a.kp, &[])
        .expect("borrow to the low cap");
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(1))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_RATE_LIMIT_EXCEEDED);

    // Governance raises the cap through the gate (the fast loosen-path). The follow-up borrow uses
    // a distinct amount ($400) so it isn't deduped against the first $500 borrow (litesvm reuses the
    // blockhash); 500 + 400 = 900 now fits the raised 2000 cap.
    gov_set_param(&mut svm, &gov, &coll, MarketParam::RateLimitCap, usd(2000));
    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(400))], &a.kp, &[])
        .expect("borrow succeeds under the raised cap");
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(900));
}
