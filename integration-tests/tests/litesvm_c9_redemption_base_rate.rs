//! BOLD-sweep C9 — the dynamic redemption base-rate (Liquity-style decaying volume-spike fee).
//! When enabled (`redemption_base_rate_max_bps > 0`), the redemption fee is the flat
//! `redemption_fee_bps` FLOOR plus a `base_rate` that spikes with redemption volume and decays
//! exponentially (6h half-life). Disabled (the default), redemptions price off the flat fee alone,
//! byte-identical to pre-C9. The base-rate math is `fusd_math::redemption` (unit-tested); these drive
//! the on-chain `redeem` wiring end-to-end.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_c9_redemption_base_rate

use fusd_core::state::MarketParam;
use fusd_integration_tests::*;
use fusd_math::redemption as rdm;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

/// Queue + immediately execute a single param change (timelock 0).
fn set_param(svm: &mut litesvm::LiteSVM, gov: &Keypair, coll: &Pubkey, nonce: u64, param: MarketParam, value: u64) {
    send(svm, &[queue_param_change_ix(&gov.pubkey(), coll, nonce, param, value)], gov, &[]).expect("queue param");
    send(svm, &[execute_param_change_ix(&gov.pubkey(), coll, nonce)], gov, &[]).expect("execute param");
}

/// Market + gov gate (timelock 0) + a live $100 price.
fn setup() -> (litesvm::LiteSVM, Keypair, Keypair, Pubkey) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    init_gov_gate(&mut svm, &gov);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("price $100");
    (svm, gov, cma, coll)
}

#[test]
fn base_rate_rises_on_redemption_then_decays() {
    let (mut svm, gov, cma, coll) = setup();
    let market = market_pda(&coll);
    // Enable the dynamic component (cap 5%); floor 0 so the fee IS the base-rate (cleanest to observe).
    set_param(&mut svm, &gov, &coll, 0, MarketParam::RedemptionBaseRateMax, 500);

    // Two borrowers: a low-rate victim in the lowest bucket + a higher-rate one, so the market carries
    // meaningful total debt (the bump denominator). Redeemer R holds the fUSD to redeem.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(500), 100); // bucket 10, $500 debt
    let _o = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(500), 800); // other debt → total $1000
    let r = open_borrower_rate(&mut svm, &cma, &coll, 200, usd(1_000), 2_000);
    assert_eq!(read_market(&svm, &market).redemption_base_rate, 0, "starts at 0");

    let debt_before = read_market(&svm, &market).agg_recorded_debt;
    let redeem_amt = usd(100);
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], redeem_amt)],
        &r.kp,
        &[],
    )
    .expect("redeem 1");

    // base_rate bumped by exactly (redeemed / pre-redemption debt) / BETA — the real lib computation.
    let m = read_market(&svm, &market);
    let expected = rdm::bump_base_rate(0, redeem_amt as u128, debt_before);
    assert_eq!(m.redemption_base_rate, expected, "base_rate rose by the redeemed-fraction bump");
    assert!(m.redemption_base_rate > 0);

    // Warp two half-lives (12h) and redeem a tiny amount: the stored base-rate should be the decayed
    // prior value plus a negligible bump — i.e. roughly a QUARTER of where it was (2 half-lives).
    let prev = m.redemption_base_rate;
    warp_unix(&mut svm, 12 * 3600);
    // Crank interest to `now` FIRST so the market's agg (the bump denominator) is fully accrued and
    // stable — otherwise redeem2's internal accrue folds in 12h of interest the test couldn't see,
    // and the bump denominator wouldn't match. After this, redeem2's accrue is a no-op (same ts).
    send(&mut svm, &[refresh_market_ix(&coll)], &gov, &[]).expect("crank interest");
    let debt_before2 = read_market(&svm, &market).agg_recorded_debt;
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], usd(1))],
        &r.kp,
        &[],
    )
    .expect("redeem 2 (tiny)");
    let after = read_market(&svm, &market).redemption_base_rate;
    let decayed_only = rdm::decay_base_rate(prev, 12 * 3600);
    let expected2 = rdm::bump_base_rate(decayed_only, usd(1) as u128, debt_before2);
    assert_eq!(after, expected2, "decayed ~2 half-lives then bumped by the tiny redemption");
    assert!(after < prev / 3, "12h ≈ 2 half-lives ⇒ well under a third of the prior rate");
}

#[test]
fn disabled_by_default_is_flat_fee() {
    let (mut svm, gov, cma, coll) = setup();
    let market = market_pda(&coll);
    // Flat fee 50 bps, dynamic component DISABLED (max_bps stays 0 by default).
    set_param(&mut svm, &gov, &coll, 0, MarketParam::RedemptionFee, 50);
    assert_eq!(read_market(&svm, &market).redemption_base_rate_max_bps, 0, "dynamic off by default");

    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(500), 100);
    let r = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(500), 2_000);

    let surplus_before = read_market(&svm, &market).surplus_collateral;
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], usd(100))],
        &r.kp,
        &[],
    )
    .expect("redeem");

    let m = read_market(&svm, &market);
    assert_eq!(m.redemption_base_rate, 0, "base-rate stays inert when disabled (no state churn)");
    // Flat 50 bps on $100 of redeemed collateral (1 token @ $100) = 0.005 tokens retained as surplus.
    let coll_redeemed = whole_coll(1); // $100 / $100 per token = 1 token
    let expected_fee = coll_redeemed * 50 / 10_000;
    assert_eq!(m.surplus_collateral - surplus_before, expected_fee, "exactly the flat-fee surplus, byte-identical");
}

#[test]
fn back_to_back_redemptions_charge_a_rising_fee() {
    let (mut svm, gov, cma, coll) = setup();
    let market = market_pda(&coll);
    // Floor 50 bps + dynamic enabled (cap 5%).
    set_param(&mut svm, &gov, &coll, 0, MarketParam::RedemptionFee, 50);
    set_param(&mut svm, &gov, &coll, 1, MarketParam::RedemptionBaseRateMax, 500);

    let b = open_borrower_rate(&mut svm, &cma, &coll, 200, usd(1_000), 100); // lowest bucket, $1000 debt
    let _o = open_borrower_rate(&mut svm, &cma, &coll, 200, usd(1_000), 800);
    let r = open_borrower_rate(&mut svm, &cma, &coll, 400, usd(2_000), 2_000);

    // First redemption: base_rate is 0 ⇒ fee is the flat 50 bps floor.
    let s0 = read_market(&svm, &market).surplus_collateral;
    send(&mut svm, &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], usd(200))], &r.kp, &[]).expect("redeem 1");
    let s1 = read_market(&svm, &market).surplus_collateral;
    let fee1_per_tok = (s1 - s0) * 10_000 / whole_coll(2); // $200 = 2 tokens redeemed; fee bps recovered
    assert_eq!(fee1_per_tok, 50, "first redemption pays the flat floor (base-rate was 0)");

    // Second redemption, SAME minute (a 1s nudge avoids the identical-tx dedup but is < the 60s decay
    // granularity, so the base-rate does NOT decay): base_rate is now > 0 ⇒ fee strictly exceeds the floor.
    warp_unix(&mut svm, 1);
    let s1b = read_market(&svm, &market).surplus_collateral;
    send(&mut svm, &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], usd(200))], &r.kp, &[]).expect("redeem 2");
    let s2 = read_market(&svm, &market).surplus_collateral;
    let fee2_per_tok = (s2 - s1b) * 10_000 / whole_coll(2);
    assert!(fee2_per_tok > 50, "second redemption charges the spiked fee (floor + base-rate): {fee2_per_tok} bps");
}
