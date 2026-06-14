//! In-process litesvm tests for the redemption **zombie pen** (see fusion-docs). A position that
//! carries debt but is no longer a normal redemption target — collateral-exhausted (`ink == 0`, the
//! audit's wedge) OR sub-`min_debt` dust — is moved OUT of the normal rate buckets into an
//! always-out-of-ordering zombie pen (`Position.bucket == ZOMBIE_BUCKET`, counted in
//! `RedemptionBitmap.zombie_count`). This (a) un-wedges the floor: a drained stub can no longer be the
//! sole member of the lowest bucket and block every redemption; (b) keeps dust from clogging the
//! lowest bucket; (c) lets a collateralized zombie still be drained out-of-band; and (d) lets a
//! topped-up zombie rejoin its real rate bucket.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_zombie_bucket

use fusd_core::constants::ZOMBIE_BUCKET;
use fusd_core::state::MarketParam;
use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const ZB: u16 = ZOMBIE_BUCKET as u16; // 256

fn set_param(svm: &mut litesvm::LiteSVM, gov: &Keypair, coll: &Pubkey, nonce: u64, param: MarketParam, value: u64) {
    send(svm, &[queue_param_change_ix(&gov.pubkey(), coll, nonce, param, value)], gov, &[]).expect("queue param");
    send(svm, &[execute_param_change_ix(&gov.pubkey(), coll, nonce)], gov, &[]).expect("execute param");
}

/// THE WEDGE FIX. An underwater stub redeemed to `ink == 0` with residual debt is moved to the zombie
/// pen (its lowest-bucket bit clears), so the floor stays live: the stub can no longer block redemption
/// of a higher bucket. Pre-fix, the stub stayed in the lowest bucket and every subsequent redeem
/// reverted `NothingToRedeem`.
#[test]
fn redeem_underwater_stub_pens_and_floor_stays_live() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma); // fee 0, min_debt 0 (default)
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    // Z: lowest bucket (3% -> bucket 30), 10 tokens, $600 debt — will go underwater. SOLE member of 30.
    let z = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(600), 300);
    // A: higher bucket (10% -> bucket 100), deeply collateralized, healthy throughout.
    let a = open_borrower_rate(&mut svm, &cma, &coll, 1_000, usd(500), 1_000);
    // R: the redeemer, highest bucket (20% -> bucket 200) so it never becomes the lowest. Holds $1000.
    let r = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(1_000), 2_000);
    assert_eq!(lowest_bucket(&svm, &coll), Some(30), "Z is the lowest bucket");
    assert_eq!(zombie_count(&svm, &coll), 0);

    // Crash to $50: Z is now underwater ($500 collateral value < $600 debt); A and R stay healthy.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(50))], &gov, &[]).expect("p50");

    // Redeem $500 against Z: takes ALL its collateral (10 tokens) and leaves $100 of residual debt at
    // ink == 0 -> Z is moved to the zombie pen.
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[z.position], usd(500))],
        &r.kp,
        &[],
    )
    .expect("redeem Z to ink==0");

    let zp = read_position(&svm, &z.position);
    assert_eq!(zp.ink, 0, "Z drained of all collateral");
    assert_eq!(zp.recorded_debt, usd(100) as u128, "$100 residual debt remains");
    assert_eq!(zp.bucket, ZB, "Z moved to the zombie pen");
    assert_eq!(zombie_count(&svm, &coll), 1, "pen has one member");
    assert!(!bucket_is_set(&svm, &coll, 30), "bucket 30 cleared — no longer wedging the floor");
    assert_eq!(lowest_bucket(&svm, &coll), Some(100), "lowest normal bucket is now A");
    assert_supply_invariant(&svm, &coll);

    // The stub is unredeemable (no collateral) — submitting it alone nets nothing, but it does NOT
    // revert-wedge the floor for others (it's simply skipped).
    let f = send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[z.position], usd(100))],
        &r.kp,
        &[],
    )
    .expect_err("an ink==0 zombie can't be redeemed");
    assert_eq!(custom_code(&f), E_NOTHING_TO_REDEEM);

    // THE FLOOR IS LIVE: redemption of the higher bucket A succeeds (pre-fix this reverted, wedged on Z).
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[a.position], usd(200))],
        &r.kp,
        &[],
    )
    .expect("floor stays live: A redeems despite the penned stub");
    assert_eq!(read_position(&svm, &a.position).recorded_debt, usd(300) as u128, "A redeemed $200");
    assert_eq!(zombie_count(&svm, &coll), 1, "the stub is still parked, still not blocking");
    assert_supply_invariant(&svm, &coll);
    // R burned $500 (Z) + $200 (A) = $700 of its $1000.
    assert_eq!(token_balance(&svm, &r.fusd_ata), usd(300));
    let m = read_market(&svm, &market);
    assert_eq!(m.total_collateral, token_balance(&svm, &coll_vault_pda(&coll)) as u128, "vault == total_collateral (fee 0)");
}

/// A partial redemption that leaves `0 < recorded_debt < min_debt` (but collateral remains) moves the
/// dust position to the zombie pen, clearing its normal bucket so dust can't clog the lowest bucket.
#[test]
fn redeem_sub_min_debt_dust_pens() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), 0)], &gov, &[]).expect("gov gate");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    // Enable a $100 min-debt floor.
    set_param(&mut svm, &gov, &coll, 0, MarketParam::MinDebt, usd(100));
    assert_eq!(read_market(&svm, &market_pda(&coll)).min_debt, usd(100));

    // B: lowest bucket (3% -> 30), 100 tokens, $150 debt (>= the $100 floor, healthy).
    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(150), 300);
    // Redeemer at a higher bucket (9% -> 90) so B stays the lowest.
    let r = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(1_000), 900);
    assert_eq!(lowest_bucket(&svm, &coll), Some(30));

    // Redeem $100 of B's $150 -> $50 residual, BELOW the $100 floor, but B keeps 99 tokens of collateral.
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], usd(100))],
        &r.kp,
        &[],
    )
    .expect("redeem B to dust");

    let bp = read_position(&svm, &b.position);
    assert_eq!(bp.recorded_debt, usd(50) as u128, "$50 residual — below the $100 floor");
    assert_eq!(bp.ink, whole_coll(99), "still collateralized (only the dust is the problem)");
    assert_eq!(bp.bucket, ZB, "dust moved to the zombie pen");
    assert_eq!(zombie_count(&svm, &coll), 1);
    assert!(!bucket_is_set(&svm, &coll, 30), "bucket 30 cleared — dust no longer clogs it");
    assert_supply_invariant(&svm, &coll);
}

/// A zombie rejoins its real rate bucket when a touch restores its health. Here an `ink == 0` stub
/// (min_debt disabled) gets a collateral top-up via `deposit` -> `ink > 0` -> back to bucket 30.
#[test]
fn zombie_rejoins_bucket_on_collateral_topup() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    let z = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(600), 300); // bucket 30
    let r = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(1_000), 2_000); // bucket 200, the redeemer

    // Crash + redeem Z to ink==0 -> pen (same as the wedge test).
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(50))], &gov, &[]).expect("p50");
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[z.position], usd(500))],
        &r.kp,
        &[],
    )
    .expect("pen Z");
    assert_eq!(read_position(&svm, &z.position).bucket, ZB);
    assert_eq!(zombie_count(&svm, &coll), 1);

    // Z's owner tops up collateral: ink 0 -> +, so Z is a normal target again and rejoins bucket 30.
    fund_and_deposit(&mut svm, &cma, &coll, &z, whole_coll(5));
    let zp = read_position(&svm, &z.position);
    assert_eq!(zp.ink, whole_coll(5), "collateral restored");
    assert_eq!(zp.bucket, 30, "rejoined its rate bucket");
    assert_eq!(zombie_count(&svm, &coll), 0, "pen empty");
    assert!(bucket_is_set(&svm, &coll, 30), "bucket 30 set again");
    assert_supply_invariant(&svm, &coll);
}

/// A collateralized zombie (sub-`min_debt` dust) is still drainable out-of-band: a redeemer may submit
/// it even while normal buckets exist, and redeeming it to zero removes it from the pen.
#[test]
fn collateralized_zombie_is_drainable_out_of_band() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), 0)], &gov, &[]).expect("gov gate");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    set_param(&mut svm, &gov, &coll, 0, MarketParam::MinDebt, usd(100));

    // B -> dust zombie (as in the dust test). A normal-bucket position N also exists.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(150), 300); // bucket 30
    let _n = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(500), 600); // bucket 60 (a live normal bucket)
    let r = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(1_000), 900); // bucket 90, redeemer
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], usd(100))],
        &r.kp,
        &[],
    )
    .expect("pen B as dust");
    assert_eq!(read_position(&svm, &b.position).bucket, ZB);
    assert_eq!(zombie_count(&svm, &coll), 1);
    assert_eq!(lowest_bucket(&svm, &coll), Some(60), "a normal bucket is live alongside the zombie");

    // Drain the zombie out-of-band: submit B (a pen member) even though bucket 60 is the lowest normal.
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], usd(50))],
        &r.kp,
        &[],
    )
    .expect("zombie drainable out-of-band");
    assert_eq!(read_position(&svm, &b.position).recorded_debt, 0, "zombie fully redeemed");
    assert_eq!(zombie_count(&svm, &coll), 0, "left the pen");
    assert!(bucket_is_set(&svm, &coll, 60), "the normal bucket is untouched");
    assert_supply_invariant(&svm, &coll);
}
