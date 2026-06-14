//! The oracle-divergence gate on liquidations.
//!
//! When a FRESH primary (Pyth) grossly disagrees with a PRESENT secondary (Switchboard or DEX-TWAP)
//! beyond the per-market `liq_max_divergence_bps`, `update_price` arms `Market.liq_divergence_until`
//! and `liquidate` is paused (`OracleDivergent`) — a manipulated or briefly-bad primary the
//! protocol's own secondaries visibly reject cannot drive a liquidation cascade. The pause is
//! LIQUIDATION-ONLY: redemption and repay always clear (the peg floor). The pause self-clears
//! `LIQ_DIVERGENCE_GRACE_SLOTS` after the last divergent observation, so a snap-back can't cascade.
//!
//! The gate is OFF by default (`liq_max_divergence_bps == 0`); the divergence threshold is set LOOSER
//! than the mint deviation thresholds, so mints freeze early on mild disagreement while liquidations
//! pause only on GROSS disagreement.
//!
//! Requires the dev-oracle `.so`: `anchor build -- --features dev-oracle`.

use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const SQRT_100: u128 = 5_833_372_668_713_516_046;
const PYTH_EXPO: i32 = -8;
const LIQ_DIV_BPS: u16 = 2_000; // 20% — looser than the 1% mint deviation

fn pyth_usd(price_usd: i64) -> i64 {
    price_usd * 100_000_000
}
fn sb_usd(value_usd: i128) -> i128 {
    value_usd * 1_000_000_000_000_000_000
}

fn actors() -> (litesvm::LiteSVM, Keypair, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    (svm, gov, Keypair::new())
}

/// Real-crank bootstrap to FRESH_OK at $100 with the B3 liquidation-divergence gate set to
/// `liq_max_div` bps (0 = off). Pyth + Switchboard + TWAP all agree at $100 ⇒ no divergence armed.
fn reach_fresh(
    svm: &mut litesvm::LiteSVM,
    gov: &Keypair,
    cma: &Keypair,
    liq_max_div: u16,
) -> (Pubkey, OracleHandles) {
    let coll = bootstrap_market(svm, gov, cma);
    let h = bootstrap_oracle_full(svm, gov, &coll, 300, 3, 300, false, 0, 0, liq_max_div);
    set_whirlpool_pool(svm, &h.orca_pool, SQRT_100, &coll, &h.quote);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 1");
    warp_unix(svm, 150);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 2");
    warp_unix(svm, 150);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 3");
    let now = now_unix(svm);
    set_pyth_price(svm, &h.pyth, h.feed_id, pyth_usd(100), 0, PYTH_EXPO, now);
    set_switchboard_feed(svm, &h.sb, sb_usd(100), 0, 1, now);
    send(svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], gov, &[])
        .expect("fresh-OK crank");
    let m = read_market(svm, &market_pda(&coll));
    assert!(!m.mint_frozen && m.spot == spot_for_usd(100));
    assert_eq!(m.liq_divergence_until, 0, "agreeing feeds ⇒ no divergence pause armed");
    (coll, h)
}

/// Crank a price with Pyth at `pyth_p` and (optionally) Switchboard at `sb_p`, conf 0.
fn crank(
    svm: &mut litesvm::LiteSVM,
    gov: &Keypair,
    coll: &Pubkey,
    h: &OracleHandles,
    pyth_p: i64,
    sb_p: Option<i64>,
) {
    let now = now_unix(svm);
    set_pyth_price(svm, &h.pyth, h.feed_id, pyth_usd(pyth_p), 0, PYTH_EXPO, now);
    let sb = sb_p.map(|p| {
        set_switchboard_feed(svm, &h.sb, sb_usd(p as i128), 0, 1, now);
        h.sb
    });
    svm.expire_blockhash();
    send(svm, &[update_price_ix(&gov.pubkey(), coll, &h.pyth, sb)], gov, &[]).expect("crank");
}

#[test]
fn divergence_pauses_liquidation_but_not_redemption() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma, LIQ_DIV_BPS);

    // Victim healthy at $100 (CR 167%); a redeemer + RP funder stand up while fresh.
    let victim = open_borrower(&mut svm, &cma, &coll, 10, usd(600));
    let redeemer = open_borrower(&mut svm, &cma, &coll, 50, usd(500));
    let funder = open_borrower(&mut svm, &cma, &coll, 100, usd(2_000));
    provide_sp(&mut svm, &funder, &coll, usd(1_500));

    // Crash + DIVERGE: Pyth $80 (victim CR 133% < 150% ⇒ underwater, liquidatable absent the gate),
    // Switchboard $120 — a 50% disagreement, far past the 20% liquidation-divergence threshold.
    crank(&mut svm, &gov, &coll, &h, 80, Some(120));
    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.spot, spot_for_usd(80), "fresh primary still commits a conservative spot");
    assert!(m.mint_frozen, "gross disagreement also freezes mints");
    assert!(m.liq_divergence_until > 0, "divergence pause armed");

    // Liquidation PAUSED — the position is underwater but the secondaries reject the primary.
    let liquidator = Keypair::new();
    airdrop_sol(&mut svm, &liquidator.pubkey(), 10);
    let f = liquidate(&mut svm, &liquidator, &coll, &victim.position).unwrap_err();
    assert_eq!(custom_code(&f), E_ORACLE_DIVERGENT, "liquidation paused under divergence");

    // Redemption STILL CLEARS: the peg floor never gates on divergence. The redeemer
    // redeems against the lowest-CR member of the lowest bucket (the underwater victim first).
    send(
        &mut svm,
        &[redeem_ix(&redeemer.kp.pubkey(), &coll, &redeemer.fusd_ata, &redeemer.coll_ata,
            &[victim.position, redeemer.position], usd(100))],
        &redeemer.kp,
        &[],
    )
    .expect("redeem clears under divergence");

    // Repay also stays open.
    send(&mut svm, &[repay_ix(&victim.kp.pubkey(), &coll, &victim.fusd_ata, usd(50))], &victim.kp, &[])
        .expect("repay clears under divergence");
}

#[test]
fn divergence_gate_off_by_default() {
    // liq_max_divergence_bps == 0 ⇒ the SAME divergent crank arms NO pause; liquidation proceeds
    // (mints still freeze on the disagreement, but the liquidation engine is not wedged).
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma, /*liq_max_div=*/ 0);

    let victim = open_borrower(&mut svm, &cma, &coll, 10, usd(600));
    let funder = open_borrower(&mut svm, &cma, &coll, 100, usd(2_000));
    provide_sp(&mut svm, &funder, &coll, usd(1_500));

    crank(&mut svm, &gov, &coll, &h, 80, Some(120)); // same 50% divergence as above
    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.liq_divergence_until, 0, "gate disabled ⇒ no pause armed even on gross divergence");
    assert!(m.mint_frozen, "disagreement still freezes mints");

    let liquidator = Keypair::new();
    airdrop_sol(&mut svm, &liquidator.pubkey(), 10);
    liquidate(&mut svm, &liquidator, &coll, &victim.position)
        .expect("gate off ⇒ underwater position liquidates normally");
}

#[test]
fn divergence_pause_self_clears_after_grace() {
    // After the feeds re-converge, the divergence pause clears once the grace elapses. We converge
    // back UP to $100 (where the DEX-TWAP already sits, so ALL present secondaries agree with the
    // primary — no leg re-arms the pause) and re-crank in sub-staleness steps so the cache stays
    // fresh (no on-resume grace). Proof that the gate cleared: the liquidation error transitions from
    // OracleDivergent (paused) to PositionHealthy (the position is now evaluated on health again — the
    // victim is healthy at $100). A still-paused gate would keep returning OracleDivergent forever.
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma, LIQ_DIV_BPS);

    let victim = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    // Diverge (Pyth $80 vs Switchboard $120) ⇒ pause armed.
    crank(&mut svm, &gov, &coll, &h, 80, Some(120));
    assert!(read_market(&svm, &market_pda(&coll)).liq_divergence_until > 0, "pause armed");
    let liquidator = Keypair::new();
    airdrop_sol(&mut svm, &liquidator.pubkey(), 10);
    let f = liquidate(&mut svm, &liquidator, &coll, &victim.position).unwrap_err();
    assert_eq!(custom_code(&f), E_ORACLE_DIVERGENT, "paused while divergent");

    // Converge up to $100 (agrees with the $100 TWAP) and age out the grace in sub-staleness steps.
    let step = fusd_core::constants::MAX_PRICE_STALENESS_SLOTS - 10;
    for _ in 0..6 {
        warp_slots(&mut svm, step);
        crank(&mut svm, &gov, &coll, &h, 100, Some(100)); // all legs agree ⇒ no re-arm
        match liquidate(&mut svm, &liquidator, &coll, &victim.position) {
            Err(f) if custom_code(&f) == E_ORACLE_DIVERGENT => continue, // still inside the grace
            Err(f) => {
                // Gate cleared: liquidation is evaluated on HEALTH again, and the victim is healthy
                // at the converged $100 price.
                assert_eq!(custom_code(&f), E_POSITION_HEALTHY, "divergence pause self-cleared");
                return;
            }
            Ok(_) => panic!("victim is healthy at $100 — should not have liquidated"),
        }
    }
    panic!("divergence pause never cleared after the grace elapsed");
}
