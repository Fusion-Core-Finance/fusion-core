//! End-to-end litesvm tests for the live oracle cranks `sample_twap` + `update_price`.
//!
//! These exercise the ACTUAL on-chain parsers against constructed real-shaped fixtures: Pyth
//! `PriceUpdateV2` (anchor-serialized via the SDK), Switchboard `PullFeedAccountData` (the SDK Pod
//! type), and Orca/Raydium CLMM pool byte layouts. They assert the full pipeline — sample → ring →
//! aggregate → `Market.spot` + `mint_frozen` → borrow — and the freeze semantics (a
//! degraded aggregate freezes NEW MINTS only; `spot` keeps a conservative price).
//!
//! Requires the dev-oracle `.so`: `anchor build -- --features dev-oracle`.

use fusd_integration_tests::*;
use fusd_math::oracle_scale::{px_to_ray, sqrt_price_q64_to_ray, usd_ray_to_spot};
use fusd_math::{ray_div, RAY};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

/// Q64.64 sqrt_price encoding $100 USD/collateral for a (base=9-dec, quote=6-dec) pool.
/// `sqrt_price_q64_to_ray(SQRT_100, 9, 6) ≈ 100·RAY` (within Q64.64 decode truncation, sub-bps).
const SQRT_100: u128 = 5_833_372_668_713_516_046;
/// Pyth $100 at expo -8 (price · 10^expo).
const PYTH_PRICE_100: i64 = 100 * 100_000_000;
const PYTH_EXPO: i32 = -8;
/// Switchboard $100 at its 1e18 (PRECISION = 18) scale.
const SB_VALUE_100: i128 = 100 * 1_000_000_000_000_000_000;

fn actors() -> (litesvm::LiteSVM, Keypair, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    (svm, gov, cma)
}

// ============================ sample_twap ============================

#[test]
fn sample_twap_orca_pushes_usd_ray() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, /*raydium=*/ false);
    // Whirlpool $100: collateral = token_a (base, 9 dec), quote = token_b (6 dec).
    set_whirlpool_pool(&mut svm, &h.orca_pool, SQRT_100, &coll, &h.quote);

    send(&mut svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], &gov, &[])
        .expect("sample_twap");

    assert_eq!(dex_twap_count(&svm, &coll), 1);
    let expected = sqrt_price_q64_to_ray(SQRT_100, COLL_DECIMALS, FUSD_DECIMALS).unwrap();
    assert_eq!(dex_twap_last_price(&svm, &coll), expected);
    // ≈ $100 to within Q64.64 truncation (sub-femto-bps; the fixture encodes $100).
    assert!(expected.abs_diff(100 * RAY) < RAY / 1_000_000, "got {expected}");
}

#[test]
fn sample_twap_enforces_min_inter_sample_interval() {
    // Without a minimum spacing, anyone could spam `sample_twap` ~1/sec to evict all
    // window-spanning history from the 64-slot ring → `twap()==None` → mints frozen indefinitely.
    // The gate requires consecutive samples to be ≥ ceil(window/(N-1)) apart. With window=300 and
    // N=64 that is ceil(300/63)=5s.
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, /*raydium=*/ false);
    set_whirlpool_pool(&mut svm, &h.orca_pool, SQRT_100, &coll, &h.quote);

    // First sample (empty ring) always accepted.
    send(&mut svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], &gov, &[]).expect("sample 1");
    assert_eq!(dex_twap_count(&svm, &coll), 1);

    // A second sample 4s later (< 5s min interval) is rejected — the flood lever is closed.
    warp_unix(&mut svm, 4);
    let f = send(&mut svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], &gov, &[])
        .expect_err("too soon");
    assert_eq!(custom_code(&f), E_TWAP_SAMPLE_REJECTED);
    assert_eq!(dex_twap_count(&svm, &coll), 1, "rejected sample did not land");

    // One more second (5s total ≥ min interval) → accepted.
    warp_unix(&mut svm, 1);
    send(&mut svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], &gov, &[]).expect("sample 2");
    assert_eq!(dex_twap_count(&svm, &coll), 2);
}

#[test]
fn sample_twap_raydium_pushes_usd_ray() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, /*raydium=*/ true);
    // Raydium PoolState: collateral = mint_0 (9 dec), quote = mint_1 (6 dec). Decimals in-account.
    set_raydium_pool(&mut svm, &h.raydium_pool, SQRT_100, &coll, &h.quote, COLL_DECIMALS, FUSD_DECIMALS);

    send(&mut svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.raydium_pool)], &gov, &[])
        .expect("sample_twap raydium");

    assert_eq!(dex_twap_count(&svm, &coll), 1);
    // Same scaling path as Orca (decimals from the in-account bytes here), same $100 value.
    let expected = sqrt_price_q64_to_ray(SQRT_100, COLL_DECIMALS, FUSD_DECIMALS).unwrap();
    assert_eq!(dex_twap_last_price(&svm, &coll), expected);
}

#[test]
fn sample_twap_inverts_when_collateral_is_quote() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, false);
    // Collateral on the QUOTE side: token_a = quote (6 dec), token_b = collateral (9 dec).
    // The program reads token_b-per-token_a then inverts to USD-per-collateral.
    set_whirlpool_pool(&mut svm, &h.orca_pool, SQRT_100, &h.quote, &coll);

    send(&mut svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], &gov, &[])
        .expect("sample_twap invert");

    // base = token_a = quote (6 dec), quote-of-formula = token_b = collateral (9 dec); then invert.
    let price_ba = sqrt_price_q64_to_ray(SQRT_100, FUSD_DECIMALS, COLL_DECIMALS).unwrap();
    let expected = ray_div(RAY, price_ba).unwrap();
    assert_eq!(dex_twap_last_price(&svm, &coll), expected);
}

#[test]
fn sample_twap_rejects_non_monotonic_in_one_tx() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, false);
    set_whirlpool_pool(&mut svm, &h.orca_pool, SQRT_100, &coll, &h.quote);

    // Two samples at the same clock ⇒ the second push has ts == last ⇒ the ring rejects it.
    let f = send(
        &mut svm,
        &[
            sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool),
            sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool),
        ],
        &gov,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_TWAP_SAMPLE_REJECTED);
}

#[test]
fn sample_twap_rejects_unconfigured_pool() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, false);
    // A valid Whirlpool, but at an address that is NOT the configured orca_pool.
    let rogue = Pubkey::new_unique();
    set_whirlpool_pool(&mut svm, &rogue, SQRT_100, &coll, &h.quote);
    let f = send(&mut svm, &[sample_twap_ix(&gov.pubkey(), &coll, &rogue)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_CLMM_POOL);
}

#[test]
fn sample_twap_rejects_wrong_owner() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, false);
    // The configured Orca pool address, but the account is a Raydium-owned account → owner mismatch.
    set_raydium_pool(&mut svm, &h.orca_pool, SQRT_100, &coll, &h.quote, COLL_DECIMALS, FUSD_DECIMALS);
    let f = send(&mut svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_CLMM_POOL);
}

#[test]
fn sample_twap_rejects_wrong_mint_pair() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, false);
    // Correct owner + address, but the pool's mints are not {collateral, quote}.
    set_whirlpool_pool(&mut svm, &h.orca_pool, SQRT_100, &Pubkey::new_unique(), &Pubkey::new_unique());
    let f = send(&mut svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_CLMM_POOL);
}

// ============================ update_price ============================

/// Bootstrap a market + oracle (short, fast-to-fill TWAP window), inject a $100 Whirlpool, and
/// fill the ring with 3 samples spanning the 300s window. Returns the collateral mint + handles;
/// the caller injects Pyth/Switchboard and calls `update_price`.
fn reach_fresh(svm: &mut litesvm::LiteSVM, gov: &Keypair, cma: &Keypair) -> (Pubkey, OracleHandles) {
    let coll = bootstrap_market(svm, gov, cma);
    let h = bootstrap_oracle(svm, gov, &coll, 300, 3, 300, false);
    set_whirlpool_pool(svm, &h.orca_pool, SQRT_100, &coll, &h.quote);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 1");
    warp_unix(svm, 150);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 2");
    warp_unix(svm, 150);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 3");
    assert_eq!(dex_twap_count(svm, &coll), 3);
    (coll, h)
}

#[test]
fn update_price_happy_path_then_borrow() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);

    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, /*slot=*/ 1, now);

    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("update_price");

    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.spot, spot_for_usd(100), "conf=0 ⇒ collateral price is exactly $100");
    assert!(!m.mint_frozen, "fresh + agreeing feeds ⇒ minting allowed");

    // A production-shaped borrow now succeeds: 10 tokens @ $100 = $1000 collateral, MCR 150% ⇒
    // max debt ~$666; borrow $500.
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(500));
    assert_eq!(token_balance(&svm, &b.fusd_ata), usd(500));
}

#[test]
fn update_price_freezes_on_stale_pyth_but_still_serves_spot() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);

    let now = now_unix(&svm);
    // Pyth stale (120s > 60s max_age); Switchboard fresh at $100.
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now - 120);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now);

    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("update_price");

    let m = read_market(&svm, &market_pda(&coll));
    // Mint frozen, but spot is still served from the fresh secondary (Switchboard, $100).
    assert!(m.mint_frozen);
    assert_eq!(m.spot, spot_for_usd(100));

    // Borrow is blocked; repay/liquidation/redemption (which ignore mint_frozen) would still work.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, 0, 500);
    let f = send(&mut svm, &[borrow_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(100))], &b.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_MINT_FROZEN);
}

#[test]
fn update_price_freezes_on_divergent_switchboard() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);

    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    // Switchboard $120 — 20% above Pyth, far beyond the 1% agreement band.
    set_switchboard_feed(&mut svm, &h.sb, 120 * 1_000_000_000_000_000_000, 0, 1, now);

    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("update_price");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen, "cross-oracle divergence freezes mints");
    // Priced off Pyth (the primary) — still $100.
    assert_eq!(m.spot, spot_for_usd(100));
}

#[test]
fn update_price_freezes_without_switchboard() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);

    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);

    // No Switchboard leg (optional account omitted) ⇒ aggregate cannot reach Ok.
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .expect("update_price");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen, "no secondary feed ⇒ mints frozen");
    assert_eq!(m.spot, spot_for_usd(100), "but spot still served from Pyth");
}

#[test]
fn update_price_freezes_on_subquorum_switchboard() {
    // A Switchboard result backed by fewer responses than the feed's `min_responses` is a
    // degraded result. It must be treated as ABSENT (freeze mints), never silently trusted.
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);

    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    // num_samples 0 < min_responses 2 ⇒ sub-quorum ⇒ SB leg drops to None.
    set_switchboard_feed_quorum(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now, /*num_samples=*/ 0, /*min_responses=*/ 2);

    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("update_price");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen, "sub-quorum Switchboard ⇒ treated as absent ⇒ mints frozen");
    assert_eq!(m.spot, spot_for_usd(100), "spot still served from Pyth (the primary)");
}

#[test]
fn update_price_freezes_on_wide_confidence() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);

    let now = now_unix(&svm);
    // Pyth conf = 3% of price (> the 2% default cap).
    let conf = (PYTH_PRICE_100 as u64) * 3 / 100;
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, conf, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now);

    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("update_price");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen, "confidence wider than the cap freezes mints");
    // Spot is the conservative collateral price (price − k·σ). Pin it exactly: chosen view is
    // Pyth ($100, conf=$3), haircut = conf_ray · k_bps / BPS.
    let conf_ray = px_to_ray(conf as u128, PYTH_EXPO).unwrap();
    let k = fusd_core::constants::DEFAULT_ORACLE_K_BPS as u128;
    let collateral_price = (100 * RAY) - conf_ray * k / 10_000;
    let expected_spot = usd_ray_to_spot(collateral_price, COLL_DECIMALS, FUSD_DECIMALS).unwrap();
    assert_eq!(m.spot, expected_spot);
    assert!(m.spot > 0 && m.spot < spot_for_usd(100));
}

#[test]
fn update_price_freezes_when_twap_corridor_missing() {
    let (mut svm, gov, cma) = actors();
    // Market + oracle, but NO TWAP samples (empty ring) ⇒ corridor unavailable.
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, false);

    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now);

    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("update_price");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen, "no TWAP corridor ⇒ mints frozen");
    assert_eq!(m.spot, spot_for_usd(100), "spot still served from Pyth");
}

// ---- bad Pyth / Switchboard inputs hard-revert update_price ----

fn oracle_only(svm: &mut litesvm::LiteSVM, gov: &Keypair, cma: &Keypair) -> (Pubkey, OracleHandles) {
    let coll = bootstrap_market(svm, gov, cma);
    let h = bootstrap_oracle(svm, gov, &coll, 300, 3, 300, false);
    (coll, h)
}

#[test]
fn update_price_rejects_pyth_wrong_owner() {
    let (mut svm, gov, cma) = actors();
    let (coll, _h) = oracle_only(&mut svm, &gov, &cma);
    // A pubkey that was never injected ⇒ owner is the system program, not the Pyth receiver.
    let bogus = Pubkey::new_unique();
    let f = send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &bogus, None)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_PRICE_UPDATE);
}

#[test]
fn update_price_rejects_pyth_partial_verification() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = oracle_only(&mut svm, &gov, &cma);
    let now = now_unix(&svm);
    set_pyth_price_partial(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now, 5);
    let f = send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_PRICE_UPDATE);
}

#[test]
fn update_price_rejects_pyth_wrong_feed_id() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = oracle_only(&mut svm, &gov, &cma);
    let now = now_unix(&svm);
    // feed_id [9;32] != the configured [7;32].
    set_pyth_price(&mut svm, &h.pyth, [9u8; 32], PYTH_PRICE_100, 0, PYTH_EXPO, now);
    let f = send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_PRICE_UPDATE);
}

#[test]
fn update_price_rejects_pyth_non_positive_price() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = oracle_only(&mut svm, &gov, &cma);
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, -1, 0, PYTH_EXPO, now);
    let f = send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_PRICE_UPDATE);
}

#[test]
fn update_price_rejects_switchboard_wrong_key() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = oracle_only(&mut svm, &gov, &cma);
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    // Pass a Switchboard account whose key is not the configured feed.
    let wrong = Pubkey::new_unique();
    set_switchboard_feed(&mut svm, &wrong, SB_VALUE_100, 0, 1, now);
    let f = send(
        &mut svm,
        &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(wrong))],
        &gov,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_SWITCHBOARD_FEED);
}

// ---- safety: the freshness cache must not advance off a stale or unusable aggregate ----

#[test]
fn update_price_stale_aggregate_does_not_refresh_cache() {
    // Regression for the HIGH-severity review finding: liquidate/redeem/withdraw gate freshness
    // ONLY on `slot - spot_updated_slot`, so a keeper must not be able to keep that cache "fresh"
    // by re-posting an old (still-signed) Pyth update. A stale aggregate must leave the slot alone.
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);

    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("fresh crank");
    let baseline_slot = read_market(&svm, &market_pda(&coll)).spot_updated_slot;

    // Advance the slot, then crank a STALE Pyth update with no Switchboard ⇒ no fresh feed.
    warp_slots(&mut svm, 100);
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now - 120);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .expect("stale crank still executes");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen, "stale feeds freeze mints");
    assert_eq!(m.spot, spot_for_usd(100), "last good spot retained");
    assert_eq!(
        m.spot_updated_slot, baseline_slot,
        "a stale aggregate must NOT advance the freshness cache (anti-replay)"
    );

    // Positive control: a genuinely fresh crank at the new slot DOES advance the cache.
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("fresh crank 2");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(!m.mint_frozen);
    assert!(m.spot_updated_slot > baseline_slot, "fresh feed advances the cache");
}

#[test]
fn update_price_extreme_confidence_does_not_brick_spot() {
    // Regression for the MEDIUM review finding: when k·σ >= price the conservative collateral
    // price saturates to 0; writing spot=0 would brick liquidation/redemption (they require
    // spot>0). The last good price must be retained instead.
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);

    // First a clean $100 crank.
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("clean crank");
    assert_eq!(read_market(&svm, &market_pda(&coll)).spot, spot_for_usd(100));

    // Now a fresh-but-50%-confidence Pyth (k·σ > price ⇒ collateral_price saturates to 0).
    // Expire the blockhash so this otherwise-identical update_price tx isn't deduped.
    let conf = (PYTH_PRICE_100 as u64) * 50 / 100;
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, conf, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now);
    svm.expire_blockhash();
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("extreme-conf crank");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen);
    assert!(m.spot > 0, "spot must never be bricked to 0");
    assert_eq!(m.spot, spot_for_usd(100), "last good spot retained, not overwritten with 0");
}

// ---- a frozen market still serves repay / liquidation; only borrow is blocked ----

#[test]
fn frozen_market_allows_repay_blocks_borrow() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);

    // Reach Ok and mint a real position.
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("ok crank");
    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(500));
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(500));

    // Freeze via a divergent Switchboard while Pyth stays fresh at $100. (Expire the blockhash so
    // this otherwise-identical update_price tx isn't deduped as AlreadyProcessed.)
    set_switchboard_feed(&mut svm, &h.sb, 120 * 1_000_000_000_000_000_000, 0, 1, now);
    svm.expire_blockhash();
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("freeze crank");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen);
    assert_eq!(m.spot, spot_for_usd(100), "spot still fresh & served while frozen");

    // Repay IGNORES mint_frozen and succeeds.
    send(&mut svm, &[repay_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .expect("repay must work while frozen");
    assert_eq!(token_balance(&svm, &a.fusd_ata), usd(400));

    // Borrow is the ONLY path blocked.
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(1))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_MINT_FROZEN);
}

#[test]
fn frozen_market_still_liquidates() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);

    // Reach Ok; mint a borrower A ($500 debt) and an RP-funding borrower D ($1000 -> RP).
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("ok crank");
    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(500));
    let d = open_borrower(&mut svm, &cma, &coll, 30, usd(1000));
    provide_sp(&mut svm, &d, &coll, usd(1000));

    // Price drops to $40 (Pyth fresh): the TWAP corridor (still ~$100) breaks ⇒ frozen, but spot
    // updates to a FRESH $40, and A ($500 debt / 10 tok = $400) is now under MCR.
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, 40 * 100_000_000, 0, PYTH_EXPO, now);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .expect("drop crank");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen);
    assert_eq!(m.spot, spot_for_usd(40));

    // Liquidation IGNORES mint_frozen and succeeds against the fresh $40 price.
    let liq = Keypair::new();
    airdrop_sol(&mut svm, &liq.pubkey(), 100);
    liquidate(&mut svm, &liq, &coll, &a.position).expect("liquidate must work while frozen");
    let pos = read_position(&svm, &a.position);
    assert_eq!(pos.recorded_debt, 0, "A fully liquidated");
    assert_eq!(pos.ink, 0);
}

// ---- additional guard / coverage gaps from the review ----

#[test]
fn sample_twap_rejects_raydium_decimal_mismatch() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, /*raydium=*/ true);
    // In-account decimals (6,6) disagree with the configured (collateral 9, quote 6) ⇒ rejected
    // (defense-in-depth against a spoofed/misconfigured pool).
    set_raydium_pool(&mut svm, &h.raydium_pool, SQRT_100, &coll, &h.quote, 6, 6);
    let f = send(&mut svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.raydium_pool)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_CLMM_POOL);
}

#[test]
fn sample_twap_routes_both_configured_venues() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let quote = create_quote_mint(&mut svm, &gov, FUSD_DECIMALS);
    let orca = Pubkey::new_unique();
    let raydium = Pubkey::new_unique();
    let mut args = default_oracle_args();
    args.orca_pool = orca;
    args.raydium_pool = raydium;
    args.twap_window_secs = 300;
    args.twap_min_samples = 3;
    args.twap_max_staleness_secs = 300;
    send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args)], &gov, &[])
        .expect("init oracle with both pools");

    set_whirlpool_pool(&mut svm, &orca, SQRT_100, &coll, &quote);
    set_raydium_pool(&mut svm, &raydium, SQRT_100, &coll, &quote, COLL_DECIMALS, FUSD_DECIMALS);

    // Each venue's account is owner-checked against ITS program; both succeeding proves the
    // address-based disambiguation routes to the correct parser/owner.
    send(&mut svm, &[sample_twap_ix(&gov.pubkey(), &coll, &orca)], &gov, &[]).expect("sample orca");
    // ≥ the anti-flood min inter-sample interval (ceil(window/(N-1)) = ceil(300/63) = 5s).
    warp_unix(&mut svm, 10);
    send(&mut svm, &[sample_twap_ix(&gov.pubkey(), &coll, &raydium)], &gov, &[])
        .expect("sample raydium");
    assert_eq!(dex_twap_count(&svm, &coll), 2);
}

#[test]
fn borrow_reverts_when_cached_price_goes_stale() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("ok crank");

    // Open + deposit (no borrow), then age the cached price past the staleness window.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, 0, 500);
    warp_slots(&mut svm, fusd_core::constants::MAX_PRICE_STALENESS_SLOTS + 1);
    let f = send(&mut svm, &[borrow_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(100))], &b.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_STALE_PRICE);
}

/// The on-resume liquidation grace window arms through the REAL production crank, not only the dev
/// path. `dev_set_price` calls `commit_fresh_spot` unconditionally; `update_price` reaches it only
/// via `if result.fresh { if spot > 0 { .. } }`. This proves a genuine stall→resume on the
/// production path arms grace (and that the first-ever fresh price does NOT — genesis). The slot warp
/// creates the staleness gap while the timestamp-based feeds/TWAP stay fresh, isolating the
/// slot-based arming. (All other grace tests drive arming via `dev_set_price`.)
#[test]
fn update_price_arms_on_resume_grace_after_a_stall() {
    use fusd_core::constants::{LIQ_RESUME_GRACE_SLOTS, MAX_PRICE_STALENESS_SLOTS};
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    // First fresh aggregate via the real crank — genesis (prior spot == 0), must NOT arm.
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[]).expect("crank 1");
    assert_eq!(read_market(&svm, &market).spot, spot_for_usd(100));
    assert_eq!(read_market(&svm, &market).liq_grace_until, 0, "first fresh price (genesis) must not arm");

    // A borrower to liquidate after the resume. Healthy at $100 — but the grace gate is checked
    // BEFORE the health gate, so a liquidation attempt during grace reverts on grace regardless.
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(500));

    // Oracle/chain stalls: warp the SLOT clock past the staleness gate (timestamp untouched, so the
    // feeds + TWAP stay fresh). Re-post the (still-fresh) feeds and crank again → a fresh aggregate
    // that RECOVERS from the gap, arming grace through the production path.
    warp_slots(&mut svm, MAX_PRICE_STALENESS_SLOTS + 1);
    let now2 = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now2);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_100, 0, 1, now2);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[]).expect("crank 2 (resume)");

    // Armed to exactly resume_slot + GRACE, through `update_price`.
    assert_eq!(read_market(&svm, &market).spot, spot_for_usd(100), "still fresh & priced");
    assert_eq!(
        read_market(&svm, &market).liq_grace_until,
        current_slot(&svm) + LIQ_RESUME_GRACE_SLOTS,
        "the production crank armed the grace window on resume"
    );
    // And the gate engages: liquidation is blocked (grace is checked before health, so this fires
    // even though B is healthy at $100).
    let f = liquidate(&mut svm, &gov, &coll, &b.position).expect_err("grace blocks liquidation post-resume");
    assert_eq!(custom_code(&f), E_LIQUIDATION_GRACE_PERIOD);
}

// ============================ init_market_oracle param bounds ============================

/// `twap_min_samples` must be upper-bounded by the ring capacity. A value the 64-slot ring can never
/// reach would leave `twap()` permanently None → aggregate stuck `MintFrozen` → that market's mints
/// frozen forever. `init_market_oracle` rejects it; capacity itself (satisfiable — `count` saturates
/// AT capacity) is accepted.
#[test]
fn init_market_oracle_rejects_unreachable_twap_min_samples() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let quote = create_quote_mint(&mut svm, &gov, FUSD_DECIMALS);
    let cap = fusd_core::constants::TWAP_RING_CAPACITY as u32;

    // Above capacity → rejected (the ring can never satisfy it).
    let mut args = default_oracle_args();
    args.twap_min_samples = cap + 1;
    let f = send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args)], &gov, &[])
        .expect_err("min_samples > ring capacity must be rejected");
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);

    // Exactly capacity → accepted (a full ring satisfies it).
    let mut args = default_oracle_args();
    args.twap_min_samples = cap;
    send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args)], &gov, &[])
        .expect("min_samples == ring capacity is valid");
}
