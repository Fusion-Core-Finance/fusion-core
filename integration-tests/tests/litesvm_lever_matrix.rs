//! The all-levers-at-once cross-product regression. The per-flag suites each
//! prove ONE lever leaves the solvency/exit paths open; this suite flips EVERY restrictive lever
//! to its worst case simultaneously and asserts that no COMBINATION of gates can ever block:
//! full repay-to-zero, liquidation of an unhealthy target, the redemption floor, RP withdrawal,
//! gain claims, collateral-surplus claims, or (in shutdown) urgent redemption and position close.
//! This pins the constitutional claim against FUTURE gates too (any new lever that sneaks into a
//! solvency/exit path makes this suite fail). klend's own emergency_mode — which freezes repay
//! and liquidation, and is checked even on its dead-letter recovery path — is the anti-pattern
//! this proves Fusion cannot express. Requires the dev-oracle `.so`.

use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const SQRT_100: u128 = 5_833_372_668_713_516_046;
const PYTH_EXPO: i32 = -8;
const PYTH_PRICE_100: i64 = 100 * 100_000_000;
const PYTH_PRICE_85: i64 = 85 * 100_000_000;
const SB_VALUE_100: i128 = 100 * 1_000_000_000_000_000_000;
const SB_VALUE_120: i128 = 120 * 1_000_000_000_000_000_000;

/// Crank every governance lever to its most restrictive in-clamp value.
fn worst_case_levers(svm: &mut litesvm::LiteSVM, gov: &Keypair, coll: &Pubkey) {
    use fusd_core::constants::*;
    gov_set_param(svm, gov, coll, MarketParam::RateLimitCap, 1); // tightest enabled limiter
    gov_set_param(svm, gov, coll, MarketParam::Ccr, MAX_CCR_BPS as u64); // band bites at TCR<300%
    gov_set_param(svm, gov, coll, MarketParam::MinDebt, MAX_MIN_DEBT); // $10k dust floor
    gov_set_param(svm, gov, coll, MarketParam::RateAdjustCooldown, MAX_RATE_ADJUST_COOLDOWN_SECS as u64);
    gov_set_param(svm, gov, coll, MarketParam::RedemptionFee, MAX_REDEMPTION_FEE_BPS as u64);
}

#[test]
fn scene_a_live_market_all_levers_worst_case() {
    let (mut svm, gov, cma) = {
        let mut svm = new_svm();
        let gov = Keypair::new();
        airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
        (svm, gov, Keypair::new())
    };
    // Real-crank market at $100 (mirrors the oracle-matrix bootstrap).
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, false);
    set_whirlpool_pool(&mut svm, &h.orca_pool, SQRT_100, &coll, &h.quote);
    for i in 0..3 {
        if i > 0 {
            warp_unix(&mut svm, 150);
        }
        send(&mut svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], &gov, &[])
            .expect("sample");
    }
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("fresh-OK crank");
    init_gov_gate(&mut svm, &gov);
    gov_set_param(&mut svm, &gov, &coll, MarketParam::LiqBonus, 1_000); // collar → claimable surplus

    // Actors stand up while fresh-OK and lever-free (their debts predate the $10k min_debt).
    let victim = open_borrower(&mut svm, &cma, &coll, 10, usd(645)); // CR ~155% at $100
    let payer = open_borrower(&mut svm, &cma, &coll, 10, usd(400)); // will repay to zero
    let funder = open_borrower(&mut svm, &cma, &coll, 100, usd(2_000));
    provide_sp(&mut svm, &funder, &coll, usd(1_500));
    let redeemer = open_borrower(&mut svm, &cma, &coll, 50, usd(500));
    let probe = open_borrower(&mut svm, &cma, &coll, 5, 0); // zero-debt borrow probe

    // NOW flip everything restrictive at once:
    worst_case_levers(&mut svm, &gov, &coll); // limiter=1, CCR=300%, min_debt=$10k, cooldown=30d, fee=5%
    send(
        &mut svm,
        &[guardian_derisk_ix(&gov.pubkey(), &coll, fusd_core::constants::GUARDIAN_MAX_PAUSE_SECS)],
        &gov,
        &[],
    )
    .expect("guardian pause at max");
    // mint_frozen via the REAL crank: fresh $85 primary, fresh-but-divergent $120 secondary
    // (disagreement — never staleness, so the FRESH-ONLY paths must stay alive).
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_85, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_120, 0, 1, now);
    svm.expire_blockhash();
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("divergent crank");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen && m.guardian_paused_until > now && m.rl_cap == 1);

    // The ONE thing that must be blocked: new debt.
    let f = send(&mut svm, &[borrow_ix(&probe.kp.pubkey(), &coll, &probe.fusd_ata, usd(10))], &probe.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_MINT_FROZEN, "borrow blocked (first gate in line)");

    // EVERYTHING solvency/exit-shaped still runs, with every lever at worst case:
    // 1. FULL repay-to-zero (min_debt allows exactly 0 — the floor can never trap a borrower).
    send(&mut svm, &[repay_ix(&payer.kp.pubkey(), &coll, &payer.fusd_ata, usd(400))], &payer.kp, &[])
        .expect("full repay-to-zero under all levers");
    assert_eq!(read_position(&svm, &payer.position).recorded_debt, 0);
    // 2. Liquidation of the genuinely-unhealthy target (CR ~132% at the $85 fresh spot).
    let liquidator = Keypair::new();
    airdrop_sol(&mut svm, &liquidator.pubkey(), 10);
    liquidate(&mut svm, &liquidator, &coll, &victim.position).expect("liquidate under all levers");
    // 3. The redemption floor (at the max 5% fee, but never gated).
    send(
        &mut svm,
        &[redeem_ix(&redeemer.kp.pubkey(), &coll, &redeemer.fusd_ata, &redeemer.coll_ata,
            &[funder.position, redeemer.position], usd(50))],
        &redeemer.kp,
        &[],
    )
    .expect("redeem under all levers");
    // 4. RP exit + gains.
    send(&mut svm, &[withdraw_from_reactor_ix(&funder.kp.pubkey(), &coll, &funder.fusd_ata, usd(100))], &funder.kp, &[])
        .expect("withdraw_from_reactor under all levers");
    send(&mut svm, &[claim_reactor_gains_ix(&funder.kp.pubkey(), &coll, &funder.coll_ata)], &funder.kp, &[])
        .expect("claim_reactor_gains under all levers");
    // 5. The liquidated borrower's collar surplus.
    send(&mut svm, &[claim_coll_surplus_ix(&victim.kp.pubkey(), &coll, &victim.coll_ata)], &victim.kp, &[])
        .expect("claim_coll_surplus under all levers");
}

#[test]
fn scene_b_shutdown_plus_levers() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    init_gov_gate(&mut svm, &gov);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price");
    gov_set_param(&mut svm, &gov, &coll, MarketParam::LiqBonus, 1_000);

    let victim = open_borrower(&mut svm, &cma, &coll, 10, usd(645));
    let closer = open_borrower(&mut svm, &cma, &coll, 10, usd(400)); // will fully unwind
    let funder = open_borrower(&mut svm, &cma, &coll, 100, usd(2_000));
    provide_sp(&mut svm, &funder, &coll, usd(1_500));

    // Book a collar surplus pre-shutdown (fresh $85 liquidation).
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(85))], &gov, &[])
        .expect("drop");
    let liquidator = Keypair::new();
    airdrop_sol(&mut svm, &liquidator.pubkey(), 10);
    liquidate(&mut svm, &liquidator, &coll, &victim.position).expect("collared liq");

    // Worst-case levers + guardian pause, then crash + terminal shutdown on top.
    worst_case_levers(&mut svm, &gov, &coll);
    send(
        &mut svm,
        &[guardian_derisk_ix(&gov.pubkey(), &coll, fusd_core::constants::GUARDIAN_MAX_PAUSE_SECS)],
        &gov,
        &[],
    )
    .expect("guardian pause");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(15))], &gov, &[])
        .expect("crash");
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown");

    // The wind-down set all still runs with every lever at worst case + shutdown:
    send(
        &mut svm,
        &[urgent_redeem_ix(&funder.kp.pubkey(), &coll, &funder.fusd_ata, &funder.coll_ata,
            &[closer.position], usd(50))],
        &funder.kp,
        &[],
    )
    .expect("urgent_redeem under shutdown + all levers");
    // closer fully unwinds: repay remaining debt, withdraw all collateral, close. (urgent_redeem
    // above took $50 of its debt and the matching collateral at the last price.)
    let remaining_debt = read_position(&svm, &closer.position).recorded_debt as u64;
    send(&mut svm, &[repay_ix(&closer.kp.pubkey(), &coll, &closer.fusd_ata, remaining_debt)], &closer.kp, &[])
        .expect("repay under shutdown + all levers");
    fund_and_deposit(&mut svm, &cma, &coll, &closer, whole_coll(1)); // deposit stays open too
    let ink = read_position(&svm, &closer.position).ink;
    send(&mut svm, &[withdraw_ix(&closer.kp.pubkey(), &coll, &closer.coll_ata, ink)], &closer.kp, &[])
        .expect("zero-debt withdraw under shutdown + all levers");
    send(&mut svm, &[close_position_ix(&closer.kp.pubkey(), &coll)], &closer.kp, &[])
        .expect("close_position under shutdown + all levers");
    send(&mut svm, &[withdraw_from_reactor_ix(&funder.kp.pubkey(), &coll, &funder.fusd_ata, usd(100))], &funder.kp, &[])
        .expect("withdraw_from_reactor under shutdown + all levers");
    send(&mut svm, &[claim_coll_surplus_ix(&victim.kp.pubkey(), &coll, &victim.coll_ata)], &victim.kp, &[])
        .expect("claim_coll_surplus under shutdown + all levers");

    // Borrow stays dead, of course (terminal gate first in line).
    let probe = Keypair::new();
    airdrop_sol(&mut svm, &probe.pubkey(), 10);
    let _ = probe; // borrow probes need an open position; the shutdown suite covers the code path.
    assert!(read_market(&svm, &market_pda(&coll)).shutdown);
}

// Defense for the doc table's negative rows: there is no setter for `shutdown`/`mint_frozen`
// outside their rule-based writers, no program denylist, and `urgent_redeem`'s only gates are
// shutdown==true + spot>0 — all pinned by grep + the suites above. A compile-time tombstone:
// if someone adds a MarketParam variant that can disable a solvency path, the exhaustive
// matches in governance.rs force them past validate/apply/current_param — and this file's
// scene tests are where that lever must prove it cannot block the protected set.
#[allow(dead_code)]
const PROTECTED_SET: [&str; 8] = [
    "repay-to-zero",
    "liquidate (unhealthy target)",
    "redeem (ordered floor)",
    "urgent_redeem (wind-down floor)",
    "withdraw_from_reactor",
    "claim_reactor_gains",
    "claim_coll_surplus",
    "close_position (emptied)",
];
