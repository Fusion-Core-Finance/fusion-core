//! Global Backstop Reserve — the tier-3.5 liquidation DRAW.
//!
//! When a liquidation's loss spills past RP + redistribution + the local buffer, the global reserve
//! absorbs it (up to the per-market hybrid cap) BEFORE any un-homed bad debt / shutdown. The global
//! tier only fires when the victim is the SOLE staked position (no redistribution recipient — a
//! recipient would absorb the whole remainder first), so each test funds the reserve from the victim's
//! own borrowed fUSD (a stand-in for the real interest-cut funding) and then liquidates it underwater.
//!
//! Requires the dev-oracle `.so`: `anchor build -- --features dev-oracle`.

use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

fn actors() -> (litesvm::LiteSVM, Keypair, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    (svm, gov, Keypair::new())
}

/// Market + gate + an inited backstop with draw params set; a single victim borrowing `debt` at $100
/// whose borrowed fUSD funds the reserve. Returns (coll, victim). `debt_share_bps` tunes the binding
/// draw-cap arm. The reserve is funded to exactly `reserve_fund`.
fn setup_draw(
    svm: &mut litesvm::LiteSVM,
    gov: &Keypair,
    cma: &Keypair,
    debt: u64,
    reserve_fund: u64,
    debt_share_bps: u64,
) -> (Pubkey, Actor) {
    let coll = bootstrap_market(svm, gov, cma);
    init_gov_gate(svm, gov);
    send(svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], gov, &[]).expect("price");
    send(svm, &[init_global_backstop_ix(&gov.pubkey())], gov, &[]).expect("init backstop");
    gov_set_global_param(svm, gov, GlobalParam::ReserveCap, usd(100_000_000)); // non-binding
    gov_set_global_param(svm, gov, GlobalParam::DrawBase, usd(100_000)); // generous base access
    gov_set_global_param(svm, gov, GlobalParam::DrawCeilingShare, 10_000); // 100% of the reserve
    gov_set_global_param(svm, gov, GlobalParam::DrawDebtShare, debt_share_bps);

    // One position only (so no redistribution recipient at liquidation). It borrows `debt`, then funds
    // the reserve from that fUSD (the test's funding stand-in; production funds via the interest cut).
    let victim = open_borrower(svm, cma, &coll, 100, debt);
    send(svm, &[fund_backstop_ix(&victim.kp.pubkey(), &victim.fusd_ata, reserve_fund)], &victim.kp, &[])
        .expect("fund reserve");
    assert_eq!(backstop_balance(svm), reserve_fund);
    (coll, victim)
}

fn liquidator(svm: &mut litesvm::LiteSVM) -> Keypair {
    let kp = Keypair::new();
    airdrop_sol(svm, &kp.pubkey(), 10);
    kp
}

#[test]
fn backstop_fully_absorbs_a_tail_loss() {
    // Reserve + cap both cover the loss ⇒ the backstop absorbs the WHOLE debt: no un-homed bad debt,
    // no shutdown. This is the headline — a contained failure does not become a system shortfall.
    let (mut svm, gov, cma) = actors();
    let (coll, victim) = setup_draw(&mut svm, &gov, &cma, /*debt=*/ usd(3_000), /*fund=*/ usd(3_000), /*debt_share=*/ 10_000);

    // Crash so the sole position is deeply underwater; no RP, empty local buffer.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(20))], &gov, &[]).expect("crash");
    let liq = liquidator(&mut svm);
    liquidate_with_backstop(&mut svm, &liq, &coll, &victim.position).expect("liquidate w/ backstop");

    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(backstop_balance(&svm), 0, "reserve drew the full debt");
    assert_eq!(m.global_drawn, usd(3_000) as u128);
    assert_eq!(m.bad_debt, 0, "backstop prevented bad debt");
    assert!(!m.shutdown, "backstop prevented the terminal shutdown");
    let b = read_backstop(&svm);
    assert_eq!(b.total_absorbed, usd(3_000) as u128);
    // Reserve-solvency: vault == contributed − absorbed − withdrawn.
    assert_eq!(
        backstop_balance(&svm) as u128,
        b.total_contributed - b.total_absorbed - b.total_withdrawn
    );
    assert_supply_invariant(&svm, &coll);
    let p = read_position(&svm, &victim.position);
    assert_eq!(p.recorded_debt, 0);
    assert_eq!(p.ink, 0);
}

#[test]
fn backstop_draw_capped_then_unhomed() {
    // The per-market draw cap binds (debt_share 1/3) ⇒ the backstop absorbs only its capped share; the
    // residual is un-homed bad debt → shutdown. The backstop is bounded second-loss, not a blank check.
    let (mut svm, gov, cma) = actors();
    let (coll, victim) = setup_draw(&mut svm, &gov, &cma, /*debt=*/ usd(3_000), /*fund=*/ usd(3_000), /*debt_share=*/ 3_333);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(20))], &gov, &[]).expect("crash");
    let liq = liquidator(&mut svm);
    liquidate_with_backstop(&mut svm, &liq, &coll, &victim.position).expect("liquidate w/ backstop");

    let m = read_market(&svm, &market_pda(&coll));
    // The debt-share arm binds: floor(debt · 3333/10000) drawn, the rest un-homed.
    let expected_draw = usd(3_000) as u128 * 3_333 / 10_000; // = 999_900_000 (floor)
    assert_eq!(m.global_drawn, expected_draw, "capped at debt_share·debt");
    assert_eq!(backstop_balance(&svm) as u128, usd(3_000) as u128 - expected_draw);
    assert!(m.bad_debt > 0 && m.shutdown, "residual un-homed ⇒ shutdown");
    assert_eq!(m.bad_debt, usd(3_000) as u128 - m.global_drawn, "unhomed == debt − drawn");
    assert_eq!(read_backstop(&svm).total_absorbed, m.global_drawn);
    assert_supply_invariant(&svm, &coll);
}

#[test]
fn omitted_backstop_is_pre_backstop_four_tier() {
    // The SAME underwater scenario, liquidated WITHOUT the backstop accounts: the global tier is 0, so
    // the whole post-buffer loss is un-homed → shutdown. Proves the draw is opt-in via the accounts.
    let (mut svm, gov, cma) = actors();
    let (coll, victim) = setup_draw(&mut svm, &gov, &cma, /*debt=*/ usd(3_000), /*fund=*/ usd(3_000), /*debt_share=*/ 10_000);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(20))], &gov, &[]).expect("crash");
    let liq = liquidator(&mut svm);
    liquidate(&mut svm, &liq, &coll, &victim.position).expect("liquidate (no backstop accounts)");

    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.global_drawn, 0, "no draw without the backstop accounts");
    assert_eq!(backstop_balance(&svm), usd(3_000), "reserve untouched");
    assert!(m.bad_debt > 0 && m.shutdown, "full loss un-homed (4-tier)");
    assert_supply_invariant(&svm, &coll);
}
