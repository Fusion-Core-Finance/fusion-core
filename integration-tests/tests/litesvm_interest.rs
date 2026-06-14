//! In-process litesvm tests for **per-position interest accrual** (the Liquity-v2 / BOLD
//! weighted-debt-sum model). Exercises: two borrowers
//! at different `user_rate_bps` accrue different amounts; the realized-interest fee stream funds the
//! insurance buffer via `refresh_market` (the funding loop); the supply invariant
//! `circulating == agg_recorded_debt − unminted_interest + bad_debt`; and that interest accrual
//! never changes a position's redemption rate-bucket (debt-in-front ordering stays stable).
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_interest

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

const ONE_YEAR: i64 = 31_536_000;

// `assert_supply_invariant` and `assert_weighted_sum` are shared harness helpers (src/lib.rs).

/// Two borrowers, same debt, DIFFERENT rates: after a year they owe different amounts, exactly
/// `debt · rate · t / (year · 10_000)` each (linear, floor). The per-borrower rate is real.
#[test]
fn two_rates_accrue_differently() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // A: $1000 debt at 10%/yr. B: $1000 debt at 0.5%/yr (the MIN rate). Both deeply collateralized.
    let a = open_borrower_rate(&mut svm, &cma, &coll, 50, usd(1_000), 1_000);
    let b = open_borrower_rate(&mut svm, &cma, &coll, 50, usd(1_000), 50);
    assert_eq!(read_market(&svm, &market).agg_recorded_debt, usd(2_000) as u128);
    // agg_weighted_debt_sum = 1000e6·1000 + 1000e6·50 = 1.05e12 (bps scale, fits u128).
    assert_eq!(read_market(&svm, &market).agg_weighted_debt_sum, 1_000_000_000 * 1_050);

    warp_unix(&mut svm, ONE_YEAR);
    // Touch each position so its interest realizes into `recorded_debt`.
    fund_and_deposit(&mut svm, &cma, &coll, &a, whole_coll(1));
    fund_and_deposit(&mut svm, &cma, &coll, &b, whole_coll(1));

    // A: $1000 + 10% = $1100. B: $1000 + 0.5% = $1005.
    assert_eq!(read_position(&svm, &a.position).recorded_debt, usd(1_100) as u128, "A at 10%");
    assert_eq!(read_position(&svm, &b.position).recorded_debt, usd(1_005) as u128, "B at 0.5%");

    // The aggregate recorded its $105 of interest; per-position sum matches it (no drift here).
    let m = read_market(&svm, &market);
    assert_eq!(m.agg_recorded_debt, usd(2_105) as u128);
    assert_eq!(m.unminted_interest, usd(105) as u128, "interest accrued, not yet minted");
    assert_supply_invariant(&svm, &coll);
    // The weighted sum tracks the realized debts at their rates: 1100e6·1000 + 1005e6·50.
    assert_weighted_sum(&svm, &coll, &[a.position, b.position]);
}

/// `adjust_rate` realizes interest at the OLD rate, then re-weights the whole position into the
/// aggregate at the NEW rate (up AND down). The weighted-sum oracle catches any reweight regression.
#[test]
fn adjust_rate_reweights_at_new_rate() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    let a = open_borrower_rate(&mut svm, &cma, &coll, 50, usd(1_000), 500);
    assert_eq!(read_market(&svm, &market).agg_weighted_debt_sum, usd(1_000) as u128 * 500);

    // Accrue some interest, then raise the rate to 2000 bps.
    warp_unix(&mut svm, ONE_YEAR);
    send(&mut svm, &[adjust_rate_ix(&a.kp.pubkey(), &coll, 2_000)], &a.kp, &[]).expect("rate up");
    let p = read_position(&svm, &a.position);
    assert_eq!(p.user_rate_bps, 2_000);
    // recorded_debt = $1000 + 5% interest realized at the OLD rate = $1050; weighted at the NEW rate.
    assert_eq!(p.recorded_debt, usd(1_050) as u128, "interest realized at the old 5% rate");
    assert_eq!(p.bucket, 200, "moved to the 20% bucket");
    assert_weighted_sum(&svm, &coll, &[a.position]);
    assert_supply_invariant(&svm, &coll);

    // Lower it back to the MIN rate; re-weights down, no further interest (no time passed).
    send(&mut svm, &[adjust_rate_ix(&a.kp.pubkey(), &coll, 50)], &a.kp, &[]).expect("rate down");
    assert_eq!(read_position(&svm, &a.position).recorded_debt, usd(1_050) as u128);
    assert_weighted_sum(&svm, &coll, &[a.position]);
    assert_supply_invariant(&svm, &coll);
}

/// The funding loop: realized interest is minted into the insurance buffer by `refresh_market`. The
/// buffer starts empty (no treasury seed); after a year + a refresh it holds exactly the accrued
/// interest, `unminted_interest` is drained to 0, and the supply invariant still holds.
#[test]
fn interest_funds_the_buffer() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // $1000 debt at 10%/yr. Buffer empty.
    let _a = open_borrower_rate(&mut svm, &cma, &coll, 50, usd(1_000), 1_000);
    assert_eq!(buffer_balance(&svm, &coll), 0, "buffer starts empty");
    assert_supply_invariant(&svm, &coll);

    warp_unix(&mut svm, ONE_YEAR);
    // Permissionless crank: accrue the aggregate interest ($100) AND mint it into the buffer.
    send(&mut svm, &[refresh_market_ix(&coll)], &gov, &[]).expect("refresh_market");

    let m = read_market(&svm, &market);
    assert_eq!(m.agg_recorded_debt, usd(1_100) as u128, "interest folded into aggregate debt");
    assert_eq!(m.unminted_interest, 0, "minted out to the buffer");
    assert_eq!(buffer_balance(&svm, &coll), usd(100), "buffer captured the $100 of interest");
    assert_eq!(read_insurance_buffer(&svm, &coll).total_funded, usd(100) as u128);
    // circulating == $1000 (borrowed) + $100 (interest minted to buffer) == agg_recorded_debt.
    assert_eq!(mint_supply(&svm, &fusd_mint_pda()), usd(1_100));
    assert_supply_invariant(&svm, &coll);

    // A second refresh with no elapsed time mints nothing (idempotent). Expire the blockhash so
    // litesvm doesn't reject the identical tx as AlreadyProcessed (no time passes ⇒ dt 0 ⇒ mint 0).
    svm.expire_blockhash();
    send(&mut svm, &[refresh_market_ix(&coll)], &gov, &[]).expect("refresh again");
    assert_eq!(buffer_balance(&svm, &coll), usd(100), "no double mint");
    assert_supply_invariant(&svm, &coll);
}

/// Interest accrual must NEVER move a position's redemption rate-bucket: the bucket keys on
/// `user_rate_bps` (unchanged by accrual), so debt-in-front ordering is stable as debt grows.
#[test]
fn interest_preserves_redemption_bucket() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // Rate 500 bps, default bucket width 10 bps => bucket 50.
    let a = open_borrower_rate(&mut svm, &cma, &coll, 50, usd(1_000), 500);
    assert_eq!(read_position(&svm, &a.position).bucket, 50, "rate 5.00% -> bucket 50");
    assert!(bucket_is_set(&svm, &coll, 50));

    warp_unix(&mut svm, 3 * ONE_YEAR);
    // Touch (deposit) realizes a large chunk of interest into recorded_debt...
    fund_and_deposit(&mut svm, &cma, &coll, &a, whole_coll(1));
    let p = read_position(&svm, &a.position);
    assert!(p.recorded_debt > usd(1_000) as u128, "debt grew with interest");
    // ...but the bucket (and thus the redemption queue position) is unchanged.
    assert_eq!(p.bucket, 50, "interest never moves the redemption bucket");
    assert!(bucket_is_set(&svm, &coll, 50));
}

/// The supply invariant survives a borrow → accrue → refresh(mint) → repay round-trip: repaying the
/// interest-grown debt burns exactly that much circulating fUSD.
#[test]
fn supply_invariant_through_interest_and_repay() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    let a = open_borrower_rate(&mut svm, &cma, &coll, 50, usd(1_000), 1_000);
    assert_supply_invariant(&svm, &coll);

    warp_unix(&mut svm, ONE_YEAR);
    send(&mut svm, &[refresh_market_ix(&coll)], &gov, &[]).expect("refresh");
    assert_supply_invariant(&svm, &coll);

    // The borrower repays $300 of its now-$1100 debt (it must have acquired the extra fUSD; here it
    // still holds its borrowed $1000, enough to cover a $300 repay).
    send(&mut svm, &[repay_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(300))], &a.kp, &[])
        .expect("repay 300");
    let p = read_position(&svm, &a.position);
    assert_eq!(p.recorded_debt, usd(800) as u128, "1100 - 300 repaid");
    assert_eq!(read_market(&svm, &market).agg_recorded_debt, usd(800) as u128);
    assert_supply_invariant(&svm, &coll);
}
