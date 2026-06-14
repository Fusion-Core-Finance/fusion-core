//! The asymmetric `debt_price` wired into liquidation.
//!
//! `update_price` caches TWO prices: the LOW `Market.spot` (`price − k·σ`, used by borrow/withdraw
//! LTV, redemption payout, and the CCR/SCR gauges) and the HIGH `Market.debt_spot` (`price + k·σ`).
//! **Liquidation eligibility + the seize conversion price off `debt_spot`** — so under price
//! uncertainty a position is liquidated only when it is underwater at the OPTIMISTIC valuation, and
//! a wide confidence band can't drive a destructive, irreversible liquidation on noise.
//!
//! These tests run the REAL `update_price` crank with a non-zero Pyth confidence (the dev-oracle
//! path sets `debt_spot == spot`, so the asymmetry is invisible there by construction — only the
//! real crank exercises it). The discriminating case puts a position in the LIMBO BAND: underwater
//! at the LOW `spot` but healthy at the HIGH `debt_spot`, where the two prices disagree on
//! liquidatability. If liquidation (incorrectly) read `spot` it would seize the position; reading
//! `debt_spot` it must reject it as healthy.
//!
//! Requires the dev-oracle `.so`: `anchor build -- --features dev-oracle`.

use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const SQRT_100: u128 = 5_833_372_668_713_516_046;
const PYTH_EXPO: i32 = -8;
const PYTH_PRICE_100: i64 = 100 * 100_000_000;
const SB_VALUE_100: i128 = 100 * 1_000_000_000_000_000_000;

fn pyth_usd(price_usd: i64) -> i64 {
    price_usd * 100_000_000
}

fn actors() -> (litesvm::LiteSVM, Keypair, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    (svm, gov, Keypair::new())
}

/// Real-crank bootstrap to a FRESH_OK market at $100 with ZERO confidence (so `spot == debt_spot`
/// here; the asymmetry is introduced later by a confident crank). Mirrors the oracle-matrix suite.
fn reach_fresh(svm: &mut litesvm::LiteSVM, gov: &Keypair, cma: &Keypair) -> (Pubkey, OracleHandles) {
    let coll = bootstrap_market(svm, gov, cma);
    let h = bootstrap_oracle(svm, gov, &coll, 300, 3, 300, false);
    set_whirlpool_pool(svm, &h.orca_pool, SQRT_100, &coll, &h.quote);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 1");
    warp_unix(svm, 150);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 2");
    warp_unix(svm, 150);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 3");
    let now = now_unix(svm);
    set_pyth_price(svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    set_switchboard_feed(svm, &h.sb, SB_VALUE_100, 0, 1, now);
    send(svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], gov, &[])
        .expect("fresh-OK crank");
    let m = read_market(svm, &market_pda(&coll));
    assert!(!m.mint_frozen && m.spot == m.debt_spot, "zero-conf crank: spot == debt_spot");
    (coll, h)
}

/// Crank a confident Pyth price (Pyth-only — the secondary's absence freezes mints, which
/// liquidation ignores; the price still commits off the fresh primary). With `conf > 0` the cached
/// `debt_spot` (price + k·σ) strictly exceeds `spot` (price − k·σ).
fn crank_confident(
    svm: &mut litesvm::LiteSVM,
    gov: &Keypair,
    coll: &Pubkey,
    h: &OracleHandles,
    price_usd: i64,
    conf_usd: i64,
) {
    let now = now_unix(svm);
    set_pyth_price(svm, &h.pyth, h.feed_id, pyth_usd(price_usd), pyth_usd(conf_usd) as u64, PYTH_EXPO, now);
    svm.expire_blockhash();
    send(svm, &[update_price_ix(&gov.pubkey(), coll, &h.pyth, None)], gov, &[])
        .expect("confident crank");
}

#[test]
fn liquidation_prices_off_debt_spot_not_spot() {
    // The discriminating test: a position underwater at the LOW spot but healthy at the HIGH
    // debt_spot must NOT be liquidatable. MCR is 150%.
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);

    // Victim: 10 collateral @ $100 = $1000 value, debt $600 ⇒ CR ≈ 167% (healthy to borrow).
    let victim = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    // Crank mid $90 with conf $3 ⇒ k·σ = 2.12·$3 = $6.36, so:
    //   spot      ≈ $83.64 → value $836.4, max_debt $557.6 < $600 ⇒ UNDERWATER at the LOW price.
    //   debt_spot ≈ $96.36 → value $963.6, max_debt $642.4 > $600 ⇒ HEALTHY at the HIGH price.
    crank_confident(&mut svm, &gov, &coll, &h, 90, 3);

    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.debt_spot > m.spot, "conf > 0 ⇒ debt_spot strictly above spot");
    assert!(m.spot > 0, "a fresh conservative spot is committed");

    // Liquidation reads debt_spot ⇒ the position is healthy ⇒ rejected. (Had it read `spot`, the
    // position is underwater there and this would SUCCEED — that is exactly the bug B5 closes.)
    let liquidator = Keypair::new();
    airdrop_sol(&mut svm, &liquidator.pubkey(), 10);
    let f = liquidate(&mut svm, &liquidator, &coll, &victim.position).unwrap_err();
    assert_eq!(
        custom_code(&f),
        E_POSITION_HEALTHY,
        "liquidation must price off debt_spot (HIGH); the position is only underwater at spot (LOW)"
    );
}

#[test]
fn liquidation_succeeds_when_underwater_even_at_debt_spot() {
    // The complementary bound: once the position is underwater even at the OPTIMISTIC debt_spot,
    // liquidation proceeds normally — debt_spot is a band, not a liquidation veto.
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);

    let victim = open_borrower(&mut svm, &cma, &coll, 10, usd(600));
    // A deep Reactor Pool to absorb the liquidation (the funder stays healthy: 100 @ $50 = $5000
    // value vs $2000 debt).
    let funder = open_borrower(&mut svm, &cma, &coll, 100, usd(2_000));
    provide_sp(&mut svm, &funder, &coll, usd(1_500));

    // Crank mid $50 with conf $1 ⇒ debt_spot ≈ $52.12 → value $521.2, max_debt $347.5 < $600:
    // underwater even at the HIGH price.
    crank_confident(&mut svm, &gov, &coll, &h, 50, 1);

    let liquidator = Keypair::new();
    airdrop_sol(&mut svm, &liquidator.pubkey(), 10);
    liquidate(&mut svm, &liquidator, &coll, &victim.position)
        .expect("underwater at debt_spot ⇒ liquidatable");
    // Victim fully liquidated (zeroed).
    let p = read_position(&svm, &victim.position);
    assert_eq!(p.recorded_debt, 0);
    assert_eq!(p.ink, 0);
}
