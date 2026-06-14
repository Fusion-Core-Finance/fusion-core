//! In-process litesvm integration test for redemption rate-bucket **maintenance**
//! (fusion-docs.md): positions join their `user_rate` bucket on first debt, leave on full repay / liquidation,
//! move on `adjust_rate`, and lazily join when redistribution gives a debt-free position debt. The
//! per-market bitmap + member counts stay exact, so `redeem` (2b) can find-first-set the lowest
//! non-empty bucket.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_buckets

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

/// First borrow joins the rate bucket; full repay leaves it (bit + count track it).
#[test]
fn borrow_joins_and_repay_leaves_bucket() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    // Borrow at 7.00% -> bucket 70.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 700);
    let bucket = bucket_of(700);
    assert_eq!(bucket, 70);
    assert!(bucket_is_set(&svm, &coll, bucket), "joined on first borrow");
    assert_eq!(bucket_count(&svm, &coll, bucket), 1);
    assert_eq!(lowest_bucket(&svm, &coll), Some(bucket));
    assert_eq!(read_position(&svm, &b.position).bucket as usize, bucket);

    // Full repay -> leaves.
    send(&mut svm, &[repay_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(300))], &b.kp, &[]).expect("repay");
    assert!(!bucket_is_set(&svm, &coll, bucket), "left on full repay");
    assert_eq!(bucket_count(&svm, &coll, bucket), 0);
    assert_eq!(lowest_bucket(&svm, &coll), None);
}

/// find-first-set returns the lowest-rate (lowest-index) non-empty bucket.
#[test]
fn lowest_bucket_is_the_lowest_rate() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    let b_hi = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 700); // bucket 70
    let _b_lo = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 300); // bucket 30
    assert!(bucket_is_set(&svm, &coll, 70) && bucket_is_set(&svm, &coll, 30));
    assert_eq!(lowest_bucket(&svm, &coll), Some(30), "lowest rate first");

    // The 3% borrower repays -> bucket 30 empties -> lowest becomes 70.
    let b_lo_pos = _b_lo;
    send(&mut svm, &[repay_ix(&b_lo_pos.kp.pubkey(), &coll, &b_lo_pos.fusd_ata, usd(300))], &b_lo_pos.kp, &[]).expect("repay");
    assert!(!bucket_is_set(&svm, &coll, 30));
    assert_eq!(lowest_bucket(&svm, &coll), Some(70));
    let _ = b_hi;
}

/// `adjust_rate` moves a debt-bearing position to its new bucket.
#[test]
fn adjust_rate_moves_bucket() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 700); // bucket 70
    assert_eq!(read_position(&svm, &b.position).bucket, 70);
    assert!(bucket_is_set(&svm, &coll, 70));

    // Re-rate to 2.00% -> bucket 20.
    send(&mut svm, &[adjust_rate_ix(&b.kp.pubkey(), &coll, 200)], &b.kp, &[]).expect("adjust_rate");
    assert_eq!(read_position(&svm, &b.position).bucket, 20);
    assert!(!bucket_is_set(&svm, &coll, 70), "left old bucket");
    assert_eq!(bucket_count(&svm, &coll, 70), 0);
    assert!(bucket_is_set(&svm, &coll, 20), "joined new bucket");
    assert_eq!(bucket_count(&svm, &coll, 20), 1);
    assert_eq!(read_position(&svm, &b.position).user_rate_bps, 200);
}

/// Liquidation removes the victim from its bucket.
#[test]
fn liquidation_leaves_bucket() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    // Depositor funds the RP (and borrows at the default 5% -> bucket 50).
    let d = open_borrower(&mut svm, &cma, &coll, 100, usd(1_000));
    provide_sp(&mut svm, &d, &coll, usd(1_000));
    // Victim at 7% -> bucket 70.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(600), 700);
    assert_eq!(bucket_count(&svm, &coll, 70), 1);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("p80");
    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate");

    assert!(!bucket_is_set(&svm, &coll, 70), "victim left its bucket");
    assert_eq!(bucket_count(&svm, &coll, 70), 0);
    // The depositor's own bucket (50) is untouched.
    assert!(bucket_is_set(&svm, &coll, bucket_of(500)));
}

/// A debt-free position that receives redistributed debt joins a bucket only when it's next
/// touched (its debt is realized lazily) — exercising the realize→reconcile interaction.
#[test]
fn redistribution_recipient_joins_bucket_on_touch() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    // Recipient C: 100 tokens collateral, NO borrow (art == 0, not in any bucket), rate 3%.
    let c = open_borrower_rate(&mut svm, &cma, &coll, 100, 0, 300);
    assert_eq!(read_position(&svm, &c.position).recorded_debt, 0);
    assert!(!bucket_is_set(&svm, &coll, 30), "no debt -> not in a bucket");

    // Borrower B is liquidated with an empty RP -> its debt redistributes to C.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(600), 700);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("p80");
    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate");
    // C's debt is still pending (recorded art == 0) until C is touched.
    assert!(!bucket_is_set(&svm, &coll, 30), "dormant until touched");

    // Touch C (a tiny deposit realizes the redistributed debt; deposit needs no price) -> it joins
    // its rate bucket.
    fund_and_deposit(&mut svm, &cma, &coll, &c, whole_coll(1));
    assert!(read_position(&svm, &c.position).recorded_debt > 0, "C realized redistributed debt");
    assert!(bucket_is_set(&svm, &coll, 30), "C joined its bucket on touch");
    assert_eq!(read_position(&svm, &c.position).bucket, 30);
    assert_eq!(bucket_count(&svm, &coll, 30), 1);
}

/// Multiple members in one bucket: the bit stays set until the last one leaves.
#[test]
fn bucket_count_tracks_multiple_members() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    // 5.00% and 5.05% both quantize to bucket 50.
    let b1 = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 500);
    let b2 = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 505);
    assert_eq!(bucket_of(500), 50);
    assert_eq!(bucket_of(505), 50);
    assert_eq!(bucket_count(&svm, &coll, 50), 2);
    assert!(bucket_is_set(&svm, &coll, 50));

    send(&mut svm, &[repay_ix(&b1.kp.pubkey(), &coll, &b1.fusd_ata, usd(300))], &b1.kp, &[]).expect("repay1");
    assert_eq!(bucket_count(&svm, &coll, 50), 1);
    assert!(bucket_is_set(&svm, &coll, 50), "bit stays set while a member remains");

    send(&mut svm, &[repay_ix(&b2.kp.pubkey(), &coll, &b2.fusd_ata, usd(300))], &b2.kp, &[]).expect("repay2");
    assert_eq!(bucket_count(&svm, &coll, 50), 0);
    assert!(!bucket_is_set(&svm, &coll, 50), "bit clears when the last leaves");
}

/// init_market clamps the bucket width + redemption fee.
#[test]
fn init_market_clamps_bucket_params() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    set_program_upgrade_authority(&mut svm, &gov.pubkey());
    send(&mut svm, &[init_protocol_ix(&gov.pubkey())], &gov, &[]).expect("init_protocol");
    let coll_mint = Keypair::new();
    create_mint(&mut svm, &gov, &coll_mint, COLL_DECIMALS, &cma.pubkey(), false);
    let coll = coll_mint.pubkey();

    // width 0 -> rejected.
    let f = send(&mut svm, &[init_market_ix(&gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, 0, 0, 0, 0)], &gov, &[])
        .expect_err("width 0");
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
    // width 200 (> 100 max) -> rejected.
    let f2 = send(&mut svm, &[init_market_ix(&gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, 0, 0, 200, 0)], &gov, &[])
        .expect_err("width over clamp");
    assert_eq!(custom_code(&f2), E_PARAM_OUT_OF_BOUNDS);
    // fee 600 (> 500 max) -> rejected.
    let f3 = send(&mut svm, &[init_market_ix(&gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, 0, 0, 10, 600)], &gov, &[])
        .expect_err("fee over clamp");
    assert_eq!(custom_code(&f3), E_PARAM_OUT_OF_BOUNDS);
    // width 1 is within [1,100] but 1*256 = 256 < MAX_USER_RATE_BPS (2550), so it can't address the
    // whole valid rate range (high rates would collapse into the top bucket) -> rejected.
    let f4 = send(&mut svm, &[init_market_ix(&gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, 0, 0, 1, 0)], &gov, &[])
        .expect_err("width under-covers the rate range");
    assert_eq!(custom_code(&f4), E_PARAM_OUT_OF_BOUNDS);
    // width 10 (the default) covers exactly: 10*256 = 2560 >= 2550 -> accepted.
    send(&mut svm, &[init_market_ix(&gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, 0, 0, 10, 0)], &gov, &[])
        .expect("default width covers the range");
}
