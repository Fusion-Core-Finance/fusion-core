//! BOLD-sweep C7 — the upfront borrowing fee. A governance-gated, default-off bps charge added to
//! the position's debt at `borrow`. The fee is NOT minted to the borrower: debt grows by
//! `amount + fee`, only `amount` is minted, and `fee` is booked into `unminted_interest` so
//! `refresh_market` mints it to the buffer (funds first-loss capital like accrued interest). The
//! global supply identity `circulating == agg_recorded_debt − unminted_interest + bad_debt` holds.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_c7_borrow_fee

use fusd_core::state::MarketParam;
use fusd_integration_tests::*;
use fusd_math::{mul_div_floor, RAY};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

/// Market + a GovernanceGate (timelock 0) + a live $100 price.
fn setup() -> (litesvm::LiteSVM, Keypair, Keypair, Pubkey) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), 0)], &gov, &[])
        .expect("init_governance_gate");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");
    (svm, gov, cma, coll)
}

/// Queue + immediately execute a single param change (timelock 0).
fn set_param(svm: &mut litesvm::LiteSVM, gov: &Keypair, coll: &Pubkey, nonce: u64, param: MarketParam, value: u64) {
    send(svm, &[queue_param_change_ix(&gov.pubkey(), coll, nonce, param, value)], gov, &[]).expect("queue param");
    send(svm, &[execute_param_change_ix(&gov.pubkey(), coll, nonce)], gov, &[]).expect("execute param");
}

#[test]
fn borrow_fee_adds_to_debt_not_to_mint() {
    let (mut svm, gov, cma, coll) = setup();
    // 1% upfront fee.
    set_param(&mut svm, &gov, &coll, 0, MarketParam::BorrowFee, 100);
    assert_eq!(read_market(&svm, &market_pda(&coll)).borrow_fee_bps, 100);

    // Open a well-collateralized position (100 tokens = $10k) with no initial debt, then borrow $1000.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, 0, 500);
    let unminted_before = read_market(&svm, &market_pda(&coll)).unminted_interest;
    send(&mut svm, &[borrow_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(1000))], &b.kp, &[])
        .expect("borrow $1000 with a 1% fee");

    // The borrower receives EXACTLY $1000 (the fee is not minted to them) ...
    assert_eq!(token_balance(&svm, &b.fusd_ata), usd(1000), "borrower receives amount, not amount - fee");
    // ... but OWES $1010 (amount + 1% fee), and the market aggregate matches.
    let fee = usd(10);
    assert_eq!(read_position(&svm, &b.position).recorded_debt, (usd(1000) + fee) as u128, "debt = amount + fee");
    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.agg_recorded_debt, (usd(1000) + fee) as u128, "agg debt = amount + fee");
    // The fee is booked as unminted interest (refresh_market will mint it to the buffer).
    assert_eq!(m.unminted_interest - unminted_before, fee as u128, "fee booked into unminted_interest");
    // The global supply identity holds: circulating ($1000 minted) == agg ($1010) − unminted ($10) + bad (0).
    assert_supply_invariant(&svm, &coll);
}

#[test]
fn borrow_fee_disabled_by_default_is_byte_identical() {
    let (mut svm, gov, cma, coll) = setup();
    assert_eq!(read_market(&svm, &market_pda(&coll)).borrow_fee_bps, 0, "fee off by default");

    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, 0, 500);
    let unminted_before = read_market(&svm, &market_pda(&coll)).unminted_interest;
    send(&mut svm, &[borrow_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(1000))], &b.kp, &[])
        .expect("borrow $1000, no fee");

    // No fee: received == debt == $1000, unminted untouched (byte-identical to pre-C7 behavior).
    assert_eq!(token_balance(&svm, &b.fusd_ata), usd(1000));
    assert_eq!(read_position(&svm, &b.position).recorded_debt, usd(1000) as u128);
    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.agg_recorded_debt, usd(1000) as u128);
    assert_eq!(m.unminted_interest, unminted_before, "no fee ⇒ unminted unchanged");
    assert_supply_invariant(&svm, &coll);
}

#[test]
fn borrow_fee_counts_against_the_mcr() {
    // The fee makes the post-fee debt the one checked against MCR: a borrow that is healthy at face
    // value but whose fee tips it past the collateral ratio is rejected.
    let (mut svm, gov, cma, coll) = setup();
    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, 0, 500);

    // The exact max borrow for this position at MCR (same integer floor math as `cdp::is_healthy`).
    let m = read_market(&svm, &market_pda(&coll));
    let ink = 100u128 * 10u128.pow(COLL_DECIMALS as u32);
    let coll_value = mul_div_floor(ink, m.spot, RAY).unwrap();
    let max_debt = mul_div_floor(coll_value, 10_000, m.mcr_bps as u128).unwrap() as u64;

    // CONTROL: with the fee off, borrowing exactly `max_debt` is healthy (debt == max).
    send(&mut svm, &[borrow_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, max_debt)], &b.kp, &[])
        .expect("max borrow is healthy with no fee");

    // Now enable a 5% fee and a FRESH borrower with identical collateral. Borrowing the same
    // `max_debt` now owes `max_debt * 1.05 > max_debt` ⇒ it breaches MCR and reverts.
    set_param(&mut svm, &gov, &coll, 0, MarketParam::BorrowFee, 500);
    let c = open_borrower_rate(&mut svm, &cma, &coll, 100, 0, 500);
    let f = send(&mut svm, &[borrow_ix(&c.kp.pubkey(), &coll, &c.fusd_ata, max_debt)], &c.kp, &[])
        .expect_err("the fee tips the post-fee debt past MCR");
    assert_eq!(custom_code(&f), E_BELOW_MIN_COLLATERAL_RATIO);
    assert_eq!(read_position(&svm, &c.position).recorded_debt, 0, "nothing borrowed on the revert");
}

#[test]
fn borrow_fee_flows_to_the_buffer_on_refresh() {
    // The booked fee drains to the insurance buffer via refresh_market's lazy interest mint, exactly
    // like accrued interest. Cranked at the same timestamp as the borrow so no interest is added and
    // the buffer receives precisely the fee.
    let (mut svm, gov, cma, coll) = setup();
    set_param(&mut svm, &gov, &coll, 0, MarketParam::BorrowFee, 100); // 1%
    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, 0, 500);
    send(&mut svm, &[borrow_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(1000))], &b.kp, &[])
        .expect("borrow with fee");
    assert_eq!(read_market(&svm, &market_pda(&coll)).unminted_interest, usd(10) as u128);

    send(&mut svm, &[refresh_market_ix(&coll)], &gov, &[]).expect("refresh mints the fee to the buffer");
    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.unminted_interest, 0, "fee drained out of unminted_interest");
    assert_eq!(token_balance(&svm, &buffer_fusd_vault_pda(&coll)), usd(10), "the buffer received the fee");
    assert_supply_invariant(&svm, &coll);
}
