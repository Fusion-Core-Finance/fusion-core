//! In-process litesvm integration test for the **on-resume liquidation grace window**
//! (the Solana-halt breaker, the Chainlink-L2 sequencer-uptime-feed pattern).
//!
//! When the cached price recovers from a staleness halt (the prior gap exceeded
//! `MAX_PRICE_STALENESS_SLOTS` — a Solana halt or a sustained feed outage), `liquidate` stays paused
//! for `LIQ_RESUME_GRACE_SLOTS` (`liq_grace_until = resume_slot + GRACE`). Borrowers who could not act
//! while the chain/feed was down get a fair window to cure before a stale-then-fresh price can trigger
//! a liquidation cascade at the resume trough. The arming lives in `Market::commit_fresh_spot`, the
//! single freshness-clock writer shared by `update_price` (prod) and `dev_set_price` (test).
//!
//! Covers: arming + the liquidation block; the window lifting for an uncured borrower; a borrower
//! CURING during the window (the whole point); steady-state updates never arming it (no regression);
//! and redemption staying open throughout (the redemption floor must never freeze on an oracle event).
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_oracle_grace

use fusd_core::constants::{
    LIQ_RESUME_GRACE_SLOTS, MAX_PRICE_STALENESS_SLOTS, SHUTDOWN_ORACLE_STALENESS_SLOTS,
};
use fusd_integration_tests::*;
use fusd_math::RAY;
use solana_sdk::signature::{Keypair, Signer};

/// Standard scene: a $1000 Reactor Pool (so any liquidation can be absorbed) and borrower B with
/// 10 tokens @ $600 debt (healthy at $100, under-MCR below ~$90). Price last printed at $100.
/// Returns `(coll, B)`.
fn scene(svm: &mut litesvm::LiteSVM, gov: &Keypair, cma: &Keypair) -> (solana_sdk::pubkey::Pubkey, Actor) {
    let coll = bootstrap_market(svm, gov, cma);
    send(svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], gov, &[]).expect("p100");
    let d = open_borrower(svm, cma, &coll, 100, usd(1_000));
    provide_sp(svm, &d, &coll, usd(1_000));
    let b = open_borrower(svm, cma, &coll, 10, usd(600));
    (coll, b)
}

/// Assert `pos` is strictly under MCR at the market's current spot (so the ONLY thing that can block
/// its liquidation is the grace window, not a health/oracle guard).
fn assert_under_mcr(svm: &litesvm::LiteSVM, coll: &solana_sdk::pubkey::Pubkey, pos: &solana_sdk::pubkey::Pubkey) {
    let m = read_market(svm, &market_pda(coll));
    let p = read_position(svm, pos);
    let coll_v = (p.ink as u128) * m.spot / RAY;
    let max_debt = coll_v * 10_000 / (MCR_BPS as u128);
    assert!(p.recorded_debt > max_debt, "expected under-MCR (debt {} > max_debt {max_debt})", p.recorded_debt);
}

/// A stall → resume ARMS the grace window, and liquidation of an under-MCR borrower is blocked by it.
#[test]
fn resume_after_stall_arms_grace_and_blocks_liquidation() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let (coll, b) = scene(&mut svm, &gov, &cma);

    // Oracle stalls (gap > MAX_PRICE_STALENESS_SLOTS), then the feed RESUMES at a fresh $80 — under
    // which B is liquidatable. The resume arms the grace window.
    warp_slots(&mut svm, MAX_PRICE_STALENESS_SLOTS + 50);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("resume $80");

    // Armed to exactly resume_slot + GRACE.
    assert_eq!(
        read_market(&svm, &market_pda(&coll)).liq_grace_until,
        current_slot(&svm) + LIQ_RESUME_GRACE_SLOTS,
        "grace armed to resume_slot + LIQ_RESUME_GRACE_SLOTS"
    );
    // B is genuinely under-MCR ($800 vs $600), so the only thing blocking liquidation is the grace.
    assert_under_mcr(&svm, &coll, &b.position);

    let f = liquidate(&mut svm, &gov, &coll, &b.position)
        .expect_err("the on-resume grace window must block liquidation");
    assert_eq!(custom_code(&f), E_LIQUIDATION_GRACE_PERIOD);
    // Nothing happened to the victim.
    assert_eq!(read_position(&svm, &b.position).recorded_debt, usd(600) as u128);
}

/// The window LIFTS: a borrower who does NOT cure during the grace window is liquidated once it
/// elapses (the price stays fresh throughout, as a keeper would keep it).
#[test]
fn grace_lifts_after_window_so_an_uncured_borrower_is_liquidated() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let (coll, b) = scene(&mut svm, &gov, &cma);

    warp_slots(&mut svm, MAX_PRICE_STALENESS_SLOTS + 50);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("resume $80");

    // Blocked during the window.
    let f = liquidate(&mut svm, &gov, &coll, &b.position).expect_err("blocked during grace");
    assert_eq!(custom_code(&f), E_LIQUIDATION_GRACE_PERIOD);

    // Walk past the window (keeper keeps the price fresh); B did nothing to cure.
    crank_past_resume_grace(&mut svm, &gov, &coll, spot_for_usd(80));
    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidation re-enabled once the grace window elapses");
    let bp = read_position(&svm, &b.position);
    assert_eq!((bp.recorded_debt, bp.ink), (0, 0), "uncured borrower is liquidated after the window");
}

/// The POINT of the window: a borrower uses it to cure (top up collateral) and is no longer
/// liquidatable when the window lifts. This is the bad debt the window prevents — the borrower could
/// not have acted during the outage, and the grace gives them the chance the halt denied.
#[test]
fn borrower_can_cure_during_the_grace_window() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let (coll, b) = scene(&mut svm, &gov, &cma);

    warp_slots(&mut svm, MAX_PRICE_STALENESS_SLOTS + 50);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("resume $80");
    assert_under_mcr(&svm, &coll, &b.position); // under-MCR at resume ($800 vs $600)

    // During the window the borrower tops up 10 tokens → 20 tokens @ $80 = $1600 vs $600 (CR 266%).
    // Deposits are never gated by the grace window (they only improve health).
    fund_and_deposit(&mut svm, &cma, &coll, &b, whole_coll(10));

    // Walk past the window; B is now healthy, so liquidation is refused — the cure stuck.
    crank_past_resume_grace(&mut svm, &gov, &coll, spot_for_usd(80));
    let f = liquidate(&mut svm, &gov, &coll, &b.position)
        .expect_err("a borrower that cured during the grace window must not be liquidatable");
    assert_eq!(custom_code(&f), E_POSITION_HEALTHY);
    // Position intact: it survived the halt because the window let it react.
    let bp = read_position(&svm, &b.position);
    assert_eq!((bp.recorded_debt, bp.ink), (usd(600) as u128, whole_coll(20)));
}

/// Steady state (no halt): consecutive fresh updates — each within the staleness gate — NEVER arm
/// the grace window, so normal-operation liquidation is never spuriously blocked. The positive
/// control proving the block above is specifically the resume-grace, not a regression.
#[test]
fn steady_state_updates_never_arm_grace() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let (coll, b) = scene(&mut svm, &gov, &cma);

    // Regular keeper cadence: hops shorter than the staleness gate, so no resume is ever detected.
    warp_slots(&mut svm, MAX_PRICE_STALENESS_SLOTS - 50);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(90))], &gov, &[]).expect("p90");
    warp_slots(&mut svm, MAX_PRICE_STALENESS_SLOTS - 50);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("p80");

    assert_eq!(read_market(&svm, &market_pda(&coll)).liq_grace_until, 0, "grace never armed in steady state");
    // B is under-MCR at $80 and liquidates immediately — no grace block.
    assert_under_mcr(&svm, &coll, &b.position);
    liquidate(&mut svm, &gov, &coll, &b.position).expect("steady-state liquidation is never grace-blocked");
    assert_eq!(read_position(&svm, &b.position).recorded_debt, 0);
}

/// Redemption is oracle-ORDERING-independent and must NEVER be frozen by an oracle event: the grace
/// window gates `liquidate` ONLY. A redemption succeeds during an armed grace window.
#[test]
fn redemption_stays_open_during_the_grace_window() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma); // redemption fee 0
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    // Redemption target B at 3% → bucket 30 ($300 debt, 10 tokens); redeemer R at 5% (higher bucket).
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 300);
    let r = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 500);

    // Stall → resume at the same $100, arming the grace window.
    warp_slots(&mut svm, MAX_PRICE_STALENESS_SLOTS + 50);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("resume $100");
    assert!(read_market(&svm, &market).liq_grace_until > 0, "grace is armed");

    // Redeem $300 against B — succeeds despite the armed grace window (redemption floor never freezes).
    let agg_before = read_market(&svm, &market).agg_recorded_debt;
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], usd(300))],
        &r.kp,
        &[],
    )
    .expect("redemption must stay open during the grace window");
    assert_eq!(read_position(&svm, &b.position).recorded_debt, 0, "B redeemed at face value during grace");
    assert_eq!(read_market(&svm, &market).agg_recorded_debt, agg_before - usd(300) as u128);
}

/// The arming frontier is exactly off-by-one and complements the staleness gate. The predicate is
/// `gap > MAX_PRICE_STALENESS_SLOTS` (strict), so a gap of exactly MAX is still "fresh" and must NOT
/// arm (mirroring liquidate's `<= MAX` accept bound), while one slot more MUST arm. Pinning both
/// values locks the single most security-sensitive comparison in the feature against an off-by-one
/// regression (`>` → `>=`, or a constant drift) that the wide-margin tests above would not catch.
/// Also pins genesis: the very first price a market receives must not arm despite the huge slot gap.
#[test]
fn arming_boundary_is_exact() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    // Genesis: the very first price must NOT arm (the `self.spot > 0` short-circuit), even though the
    // gap from the zero-init slot is large.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("genesis");
    assert_eq!(read_market(&svm, &market).liq_grace_until, 0, "the first price must not arm grace");

    // Gap == MAX_PRICE_STALENESS_SLOTS exactly: the boundary is still "fresh enough", must NOT arm.
    warp_slots(&mut svm, MAX_PRICE_STALENESS_SLOTS);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("gap==MAX");
    assert_eq!(
        read_market(&svm, &market).liq_grace_until, 0,
        "gap == MAX_PRICE_STALENESS_SLOTS must NOT arm (complements liquidate's `<= MAX` accept bound)"
    );

    // Gap == MAX + 1: a stall resume — MUST arm to resume_slot + GRACE.
    warp_slots(&mut svm, MAX_PRICE_STALENESS_SLOTS + 1);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("gap==MAX+1");
    assert_eq!(
        read_market(&svm, &market).liq_grace_until,
        current_slot(&svm) + LIQ_RESUME_GRACE_SLOTS,
        "gap == MAX_PRICE_STALENESS_SLOTS + 1 MUST arm"
    );
}

/// `urgent_redeem` — the ONLY redemption path open after shutdown (the wind-down peg floor), named
/// in the grace invariant — must NOT be gated by the grace window either. Build the exact dangerous
/// state (terminal shutdown via oracle failure, co-occurring with an armed grace window on resume)
/// and confirm the post-shutdown wind-down stays open.
#[test]
fn urgent_redeem_stays_open_during_the_grace_window() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    // Urgent-redeem target B (debt to wind down) and redeemer R (holds fUSD to burn).
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(300));
    let r = open_borrower(&mut svm, &cma, &coll, 100, usd(300));

    // Oracle fails (sustained outage) → terminal shutdown; the feed then resumes at $90, ARMING the
    // grace window. This is the worst-case overlap: a wind-down market with grace freshly armed.
    warp_slots(&mut svm, SHUTDOWN_ORACLE_STALENESS_SLOTS + 1);
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown on oracle failure");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(90))], &gov, &[]).expect("resume $90");
    assert!(read_market(&svm, &market).liq_grace_until > current_slot(&svm), "grace is armed");

    // urgent_redeem must stay open: R burns $300 of fUSD against B at face value, untouched by grace.
    let b_art_before = read_position(&svm, &b.position).recorded_debt;
    send(
        &mut svm,
        &[urgent_redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], usd(300))],
        &r.kp,
        &[],
    )
    .expect("urgent_redeem must stay open during the grace window (the post-shutdown peg floor never freezes)");
    assert!(read_position(&svm, &b.position).recorded_debt < b_art_before, "B wound down by urgent_redeem during grace");
}
