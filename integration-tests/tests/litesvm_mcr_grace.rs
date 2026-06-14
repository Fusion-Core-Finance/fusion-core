//! Governance MCR-raise liquidation grace.
//!
//! An executed MCR RAISE instantly expands the liquidatable set over LIVE positions — the
//! retroactive-worsening vector the protocol invariants forbid — and `MIN_GOV_TIMELOCK_SECS = 0` is
//! permitted for guarded launch, so the timelock alone gives no machine-enforced cure window.
//! `execute_param_change` therefore arms the existing `liq_grace_until` (monotone max,
//! `MCR_RAISE_GRACE_SLOTS`) on a raise. `liquidate` is the ONLY reader: redemption, shutdown,
//! and urgent_redeem must run unimpeded while the grace is armed. Requires the dev-oracle `.so`.

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

const GRACE: u64 = fusd_core::constants::MCR_RAISE_GRACE_SLOTS;

/// Market at MCR 150% with a live $100 price, gate at timelock 0, a victim at CR ~155%
/// (healthy at 150%, unhealthy at 160%), and an RP deep enough to absorb the victim.
fn setup() -> (litesvm::LiteSVM, Keypair, Keypair, solana_sdk::pubkey::Pubkey, Actor) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    init_gov_gate(&mut svm, &gov);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("dev_set_price");
    // Victim: 10 tokens @ $100 = $1000 collateral, $645 debt → CR ≈ 155%.
    let victim = open_borrower(&mut svm, &cma, &coll, 10, usd(645));
    // RP funder: covers the victim's whole debt so tier-1 absorbs it.
    let funder = open_borrower(&mut svm, &cma, &coll, 100, usd(2_000));
    provide_sp(&mut svm, &funder, &coll, usd(2_000));
    (svm, gov, cma, coll, victim)
}

#[test]
fn mcr_raise_arms_grace_blocking_liquidation_until_expiry() {
    let (mut svm, gov, _cma, coll, victim) = setup();
    let liquidator = Keypair::new();
    airdrop_sol(&mut svm, &liquidator.pubkey(), 10);

    // Raise MCR 150% → 160% (queue+execute at timelock 0). The victim (CR ~155%) becomes
    // eligible under the NEW threshold — but the grace must hold liquidation off.
    let before_slot = current_slot(&svm);
    gov_set_param(&mut svm, &gov, &coll, MarketParam::Mcr, 16_000);
    let mk = read_market(&svm, &market_pda(&coll));
    assert_eq!(mk.mcr_bps, 16_000);
    assert!(
        mk.liq_grace_until >= before_slot + GRACE,
        "raise must arm the grace window (got {}, want >= {})",
        mk.liq_grace_until,
        before_slot + GRACE
    );

    // T1 — the headline regression: immediately after the raise, liquidation of the
    // newly-eligible position is grace-blocked despite health < new MCR.
    let f = liquidate(&mut svm, &liquidator, &coll, &victim.position).unwrap_err();
    assert_eq!(custom_code(&f), E_LIQUIDATION_GRACE_PERIOD);

    // T2 — walk past the grace keeping the price fresh (keeper-style cranking, so neither
    // staleness nor a resume re-arm interferes); the static rule then resumes and the same
    // liquidation succeeds.
    crank_past_resume_grace(&mut svm, &gov, &coll, spot_for_usd(100));
    liquidate(&mut svm, &liquidator, &coll, &victim.position).expect("liquidate after grace");
    assert_eq!(read_position(&svm, &victim.position).recorded_debt, 0);
}

#[test]
fn mcr_lower_arms_nothing() {
    let (mut svm, gov, _cma, coll, victim) = setup();
    let liquidator = Keypair::new();
    airdrop_sol(&mut svm, &liquidator.pubkey(), 10);

    // LOWER MCR 150% → 120%: pure de-risk for borrowers; no grace may be armed.
    gov_set_param(&mut svm, &gov, &coll, MarketParam::Mcr, 12_000);
    let mk = read_market(&svm, &market_pda(&coll));
    assert_eq!(mk.mcr_bps, 12_000);
    assert_eq!(mk.liq_grace_until, 0, "a lowering must not arm the grace");

    // A position that goes below the (now lower) threshold by PRICE is liquidatable
    // immediately — eligibility was never expanded by governance here.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(76))], &gov, &[])
        .expect("price drop"); // $760 collateral vs $645 debt → CR ≈ 117.8% < 120%
    liquidate(&mut svm, &liquidator, &coll, &victim.position)
        .expect("liquidate immediately after a lowering");
}

#[test]
fn grace_is_monotone_max_across_both_writers() {
    let (mut svm, gov, _cma, coll, _victim) = setup();

    // Writer 1 (oracle resume): age the price out, recrank → arms now + LIQ_RESUME_GRACE_SLOTS.
    warp_slots(&mut svm, fusd_core::constants::MAX_PRICE_STALENESS_SLOTS + 1);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("resume crank");
    let resume_grace = read_market(&svm, &market_pda(&coll)).liq_grace_until;
    assert!(resume_grace > 0, "resume armed");

    // Writer 2 (MCR raise) extends it to the longer window — never shortens.
    let before_slot = current_slot(&svm);
    gov_set_param(&mut svm, &gov, &coll, MarketParam::Mcr, 16_000);
    let after_raise = read_market(&svm, &market_pda(&coll)).liq_grace_until;
    assert!(after_raise >= before_slot + GRACE && after_raise >= resume_grace);

    // Cross-writer shortening hole (closed): an oracle halt-resume DURING the long MCR grace
    // must not truncate it down to now + LIQ_RESUME_GRACE_SLOTS.
    warp_slots(&mut svm, fusd_core::constants::MAX_PRICE_STALENESS_SLOTS + 1);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("resume crank during MCR grace");
    let after_resume = read_market(&svm, &market_pda(&coll)).liq_grace_until;
    assert_eq!(after_resume, after_raise, "resume during a longer grace must not shorten it");
}

#[test]
fn redeem_shutdown_urgent_redeem_unaffected_by_armed_grace() {
    // `liq_grace_until` is read ONLY by `liquidate`. While an MCR-raise grace is armed:
    // ordered redemption stays open (the peg floor), the rule-based terminal `shutdown` still
    // fires, and `urgent_redeem` (the wind-down floor) still runs. Gating any of these on the
    // grace would breach the oracle-pause invariant / remove the backstop that bounds the
    // raise-cycling grief.
    let (mut svm, gov, cma, coll, victim) = setup();

    // Arm the grace via a raise.
    gov_set_param(&mut svm, &gov, &coll, MarketParam::Mcr, 16_000);
    assert!(read_market(&svm, &market_pda(&coll)).liq_grace_until > 0);

    // Ordered redemption succeeds while the grace is armed.
    let redeemer = open_borrower(&mut svm, &cma, &coll, 50, usd(500));
    let pre_coll = token_balance(&svm, &redeemer.coll_ata);
    send(
        &mut svm,
        &[redeem_ix(
            &redeemer.kp.pubkey(),
            &coll,
            &redeemer.fusd_ata,
            &redeemer.coll_ata,
            &[victim.position, redeemer.position],
            usd(100),
        )],
        &redeemer.kp,
        &[],
    )
    .expect("redeem while MCR grace armed");
    assert!(token_balance(&svm, &redeemer.coll_ata) > pre_coll, "collateral paid out");

    // Crash the price so TCR < SCR (≈159 tokens · $20 = $3,180 vs ≈$3,045 debt → TCR ≈ 104%):
    // the permissionless shutdown still fires under the grace.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(20))], &gov, &[])
        .expect("crash price");
    let cranker = Keypair::new();
    airdrop_sol(&mut svm, &cranker.pubkey(), 10);
    send(&mut svm, &[shutdown_ix(&cranker.pubkey(), &coll)], &cranker, &[])
        .expect("shutdown while MCR grace armed");

    // urgent_redeem (unordered, 0-fee) still runs under the grace.
    send(
        &mut svm,
        &[urgent_redeem_ix(
            &redeemer.kp.pubkey(),
            &coll,
            &redeemer.fusd_ata,
            &redeemer.coll_ata,
            &[victim.position],
            usd(50),
        )],
        &redeemer.kp,
        &[],
    )
    .expect("urgent_redeem while MCR grace armed");
}
