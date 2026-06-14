//! Audit B1 — the redemption-floor hardening cluster: the per-market `min_debt` dust floor, the
//! `MAX_REDEMPTION_CANDIDATES` account-count cap, and the BOLD premature-rate-change upfront fee.
//! All three are governance-gated params (default-off), so these tests stand up the GovernanceGate
//! (timelock 0) and set the params before exercising borrow/repay/adjust_rate/redeem.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_b1_floors

use fusd_core::constants::MAX_PRICE_STALENESS_SLOTS;
use fusd_core::state::MarketParam;
use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const WEEK: i64 = 7 * 86_400;
const MONTH: i64 = 30 * 86_400; // == MAX_RATE_ADJUST_COOLDOWN_SECS; a long cooldown ⇒ a large fee

/// Market + a GovernanceGate (timelock 0, so queue+execute land together) + a live $100 price.
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

// ============================ min_debt dust floor ============================

#[test]
fn min_debt_floor_blocks_sub_floor_borrow_and_dust_repay() {
    let (mut svm, gov, cma, coll) = setup();
    set_param(&mut svm, &gov, &coll, 0, MarketParam::MinDebt, usd(100));
    assert_eq!(read_market(&svm, &market_pda(&coll)).min_debt, usd(100));

    // Open a position with collateral but no borrow yet.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, 0, 500);

    // A borrow below the floor is rejected.
    let f = send(&mut svm, &[borrow_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(50))], &b.kp, &[])
        .expect_err("sub-floor borrow");
    assert_eq!(custom_code(&f), E_DEBT_BELOW_MINIMUM);
    assert_eq!(read_position(&svm, &b.position).recorded_debt, 0, "nothing borrowed");

    // A borrow AT the floor is allowed.
    send(&mut svm, &[borrow_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(100))], &b.kp, &[]).expect("at-floor borrow");
    assert_eq!(read_position(&svm, &b.position).recorded_debt, usd(100) as u128);

    // A partial repay that would leave dust ($50 < $100 floor) is rejected.
    let f = send(&mut svm, &[repay_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(50))], &b.kp, &[])
        .expect_err("repay-to-dust");
    assert_eq!(custom_code(&f), E_DEBT_BELOW_MINIMUM);
    assert_eq!(read_position(&svm, &b.position).recorded_debt, usd(100) as u128, "debt unchanged");

    // A FULL repay (to zero) is always allowed.
    send(&mut svm, &[repay_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(100))], &b.kp, &[]).expect("full repay");
    assert_eq!(read_position(&svm, &b.position).recorded_debt, 0);
}

#[test]
fn min_debt_disabled_by_default_allows_small_positions() {
    // With min_debt == 0 (the default), tiny positions are still allowed (the floor is opt-in).
    let (mut svm, _gov, cma, coll) = setup();
    assert_eq!(read_market(&svm, &market_pda(&coll)).min_debt, 0);
    let b = open_borrower_rate(&mut svm, &cma, &coll, 1, usd(1), 500);
    assert_eq!(read_position(&svm, &b.position).recorded_debt, usd(1) as u128);
}

// ============================ candidate cap ============================

#[test]
fn redeem_rejects_too_many_candidates() {
    let (mut svm, _gov, cma, coll) = setup();
    let r = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 500);
    // 21 > MAX_REDEMPTION_CANDIDATES (20). The cap is checked before any candidate is parsed, so the
    // accounts need not be real positions.
    let dummies: Vec<Pubkey> = (0..21).map(|_| Pubkey::new_unique()).collect();
    let f = send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &dummies, usd(100))],
        &r.kp,
        &[],
    )
    .expect_err("over the candidate cap");
    assert_eq!(custom_code(&f), E_TOO_MANY_REDEMPTION_CANDIDATES);
}

// ============================ premature rate-change fee ============================

#[test]
fn premature_rate_change_charges_upfront_fee_within_cooldown() {
    let (mut svm, gov, cma, coll) = setup();
    set_param(&mut svm, &gov, &coll, 0, MarketParam::RateAdjustCooldown, WEEK as u64);

    // $1000 debt at 5%. No time passes before the adjust, so realize accrues 0 interest and the only
    // debt change is the upfront fee.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(1_000), 500);
    let debt_before = read_position(&svm, &b.position).recorded_debt;
    assert_eq!(debt_before, usd(1_000) as u128);

    // Adjust to 6% immediately (within the cooldown) → upfront fee = WEEK of interest at the NEW rate.
    send(&mut svm, &[adjust_rate_ix(&b.kp.pubkey(), &coll, 600)], &b.kp, &[]).expect("premature adjust");
    let expected_fee =
        fusd_math::interest::premature_adjustment_fee(debt_before, 600, WEEK as u64).unwrap();
    assert!(expected_fee > 0);
    assert_eq!(
        read_position(&svm, &b.position).recorded_debt,
        debt_before + expected_fee,
        "upfront fee capitalized into recorded_debt"
    );
    // The fee is owed but not yet minted; the supply + weighted-sum invariants both hold.
    assert_supply_invariant(&svm, &coll);
    assert_weighted_sum(&svm, &coll, &[b.position]);

    // After the cooldown elapses, a further adjust accrues only linear interest (at the now-6% rate) —
    // NO second upfront fee.
    warp_unix(&mut svm, WEEK + 1);
    let debt_pre2 = read_position(&svm, &b.position).recorded_debt;
    send(&mut svm, &[adjust_rate_ix(&b.kp.pubkey(), &coll, 500)], &b.kp, &[]).expect("post-cooldown adjust");
    let interest = fusd_math::interest::accrued_interest(debt_pre2, 600, (WEEK + 1) as u64).unwrap();
    assert_eq!(
        read_position(&svm, &b.position).recorded_debt,
        debt_pre2 + interest,
        "post-cooldown: only interest, no upfront fee"
    );
    assert_supply_invariant(&svm, &coll);
}

#[test]
fn rate_change_is_free_when_cooldown_disabled() {
    // Default cooldown 0 ⇒ no upfront fee, no cooldown (existing behavior).
    let (mut svm, _gov, cma, coll) = setup();
    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(1_000), 500);
    send(&mut svm, &[adjust_rate_ix(&b.kp.pubkey(), &coll, 600)], &b.kp, &[]).expect("free adjust");
    assert_eq!(read_position(&svm, &b.position).recorded_debt, usd(1_000) as u128, "no fee with cooldown 0");
}

// ============ premature fee: post-application MCR/CCR re-check (BOLD-sweep C6) ============
// The premature fee grows recorded_debt, so on a near-MCR position the fee could itself make the
// borrower liquidatable. The fee branch — and ONLY the fee branch — re-checks health against a FRESH
// price (fail-closed) and reverts if the fee would breach MCR (or, when enabled, push TCR below CCR).
// The common no-fee path stays oracle-free (the NONE tier).

#[test]
fn premature_fee_that_would_breach_mcr_reverts() {
    let (mut svm, gov, cma, coll) = setup();
    set_param(&mut svm, &gov, &coll, 0, MarketParam::RateAdjustCooldown, MONTH as u64);

    // 1 collateral token @ $100 ⇒ $100 value; MCR 150% ⇒ max debt ~$66.67. Borrow $66 (≈151.5% CR:
    // healthy, but only ~$0.67 of headroom to the MCR edge).
    let b = open_borrower_rate(&mut svm, &cma, &coll, 1, usd(66), 500);
    let debt_before = read_position(&svm, &b.position).recorded_debt;
    assert_eq!(debt_before, usd(66) as u128);

    // A within-cooldown jump to 25.5% adds ~$1.38 of upfront fee (a full month at the new rate),
    // pushing debt past the MCR edge — so the fee-bearing adjust is REJECTED (it can't make the
    // borrower liquidatable). The whole tx reverts atomically: rate + debt are unchanged.
    let f = send(&mut svm, &[adjust_rate_ix(&b.kp.pubkey(), &coll, 2550)], &b.kp, &[])
        .expect_err("fee would breach MCR");
    assert_eq!(custom_code(&f), E_BELOW_MIN_COLLATERAL_RATIO);
    let p = read_position(&svm, &b.position);
    assert_eq!(p.recorded_debt, debt_before, "reverted: no fee charged");
    assert_eq!(p.user_rate_bps, 500, "reverted: rate unchanged");

    // The SAME re-rate succeeds once the borrower de-risks (repay restores headroom so the fee fits
    // under MCR) — proving the gate is the fee-vs-headroom relationship, not a blanket block.
    send(&mut svm, &[repay_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(10))], &b.kp, &[]).expect("repay to de-risk");
    // Re-rate to 2540 (≠ the reverted 2550 so the tx isn't a byte-identical duplicate); the fee now
    // fits under MCR, so it goes through.
    send(&mut svm, &[adjust_rate_ix(&b.kp.pubkey(), &coll, 2540)], &b.kp, &[]).expect("now fits under MCR");
    let fee = fusd_math::interest::premature_adjustment_fee(usd(56) as u128, 2540, MONTH as u64).unwrap();
    assert_eq!(read_position(&svm, &b.position).recorded_debt, usd(56) as u128 + fee, "fee now charged");
    assert_supply_invariant(&svm, &coll);
}

#[test]
fn premature_fee_branch_fails_closed_on_stale_price() {
    let (mut svm, gov, cma, coll) = setup();
    set_param(&mut svm, &gov, &coll, 0, MarketParam::RateAdjustCooldown, MONTH as u64);

    // A comfortably-healthy position (CR ~1000%) — so MCR is never the issue; isolate the staleness gate.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(1_000), 500);
    let debt_before = read_position(&svm, &b.position).recorded_debt;

    // Age the cached spot past the staleness window. The cooldown is unix-based (warp_slots leaves unix
    // untouched), so the adjust is still WITHIN cooldown ⇒ the fee branch runs ⇒ it reads the now-stale
    // price and FAILS CLOSED (acting on an untrusted price is worse than waiting out the cooldown).
    warp_slots(&mut svm, MAX_PRICE_STALENESS_SLOTS + 1);
    let f = send(&mut svm, &[adjust_rate_ix(&b.kp.pubkey(), &coll, 600)], &b.kp, &[])
        .expect_err("fee branch on a stale price");
    assert_eq!(custom_code(&f), E_STALE_PRICE);
    assert_eq!(read_position(&svm, &b.position).recorded_debt, debt_before, "reverted: no fee");
}

#[test]
fn no_fee_adjust_is_price_free_even_when_stale() {
    // Cooldown disabled (default 0) ⇒ no fee ⇒ the NONE-tier no-fee path reads no oracle, so a re-rate
    // succeeds even with a fully-stale price (a degraded oracle never blocks a costless re-rate).
    let (mut svm, _gov, cma, coll) = setup();
    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(1_000), 500);
    warp_slots(&mut svm, MAX_PRICE_STALENESS_SLOTS + 1_000);
    send(&mut svm, &[adjust_rate_ix(&b.kp.pubkey(), &coll, 600)], &b.kp, &[])
        .expect("no-fee re-rate ignores staleness");
    let p = read_position(&svm, &b.position);
    assert_eq!(p.user_rate_bps, 600, "rate updated");
    assert_eq!(p.recorded_debt, usd(1_000) as u128, "no fee charged");
}

#[test]
fn premature_fee_blocked_by_ccr_band_below_ccr() {
    let (mut svm, gov, cma, coll) = setup();
    // Borrow FIRST (CCR off): 1 token @ $100, $64.50 debt ⇒ CR ~155% — healthy for MCR 150%.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 1, 64_500_000, 500);
    let debt_before = read_position(&svm, &b.position).recorded_debt;
    assert_eq!(debt_before, 64_500_000);

    // Enable the CCR band at 160% (above the position's ~155% TCR) and a month cooldown.
    set_param(&mut svm, &gov, &coll, 0, MarketParam::Ccr, 16_000);
    set_param(&mut svm, &gov, &coll, 1, MarketParam::RateAdjustCooldown, MONTH as u64);

    // A within-cooldown re-rate charges a fee (risk-increasing) that leaves TCR < CCR ⇒ blocked by the
    // band, even though the fee keeps the position above MCR (so the error is CCR, not MCR — proving the
    // CCR leg fired). Reverts atomically.
    let f = send(&mut svm, &[adjust_rate_ix(&b.kp.pubkey(), &coll, 2550)], &b.kp, &[])
        .expect_err("fee leaves TCR below CCR");
    assert_eq!(custom_code(&f), E_CCR_RESTRICTED);
    assert_eq!(read_position(&svm, &b.position).recorded_debt, debt_before, "reverted: no fee");

    // De-risk above CCR (repay), and the re-rate goes through — the band, not MCR, was the gate. Use a
    // distinct rate (2540 ≠ the reverted 2550) so the tx isn't a byte-identical duplicate.
    send(&mut svm, &[repay_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(10))], &b.kp, &[]).expect("repay above CCR");
    send(&mut svm, &[adjust_rate_ix(&b.kp.pubkey(), &coll, 2540)], &b.kp, &[]).expect("succeeds once TCR ≥ CCR");
    assert!(read_position(&svm, &b.position).recorded_debt > 64_500_000 - usd(10) as u128, "fee now charged");
    assert_supply_invariant(&svm, &coll);
}
