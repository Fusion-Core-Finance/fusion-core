//! End-to-end litesvm tests for the C1 LST canonical-rate oracle leg.
//!
//! For an LST market, `update_price` serves the COLLATERAL price at `MIN(market, sol_usd · rate)`
//! where `rate = total_lamports / pool_token_supply` is read from the trustless on-chain SPL stake
//! pool — so an upward-manipulated market feed can't inflate borrowing power past the stake-pool
//! reality (the BOLD-08 over-mint→depeg defense). These exercise the ACTUAL on-chain stake-pool
//! parser + the SOL/USD underlying Pyth account + the canonical MIN against constructed fixtures.
//!
//! Requires the dev-oracle `.so`: `anchor build -- --features dev-oracle`.

use fusd_core::constants::PYTH_SOL_USD_FEED_ID;
use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const SQRT_100: u128 = 5_833_372_668_713_516_046; // Q64.64 sqrt_price ≈ $100 for (9-dec base, 6-dec quote)
const PYTH_EXPO: i32 = -8;
fn pyth_usd(d: i64) -> i64 {
    d * 100_000_000 // $d at expo -8
}
fn sb_usd(d: i128) -> i128 {
    d * 1_000_000_000_000_000_000 // $d at Switchboard's 1e18
}
/// 1 LST token = `n` smallest units (9-decimal, like SOL).
fn lst(n: u64) -> u64 {
    n * 1_000_000_000
}

fn actors() -> (litesvm::LiteSVM, Keypair, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    (svm, gov, cma)
}

/// Bootstrap an LST market and fill its TWAP ring to ~$100 (the LST/USD market leg). Returns the
/// collateral mint, the oracle handles, and the bound stake-pool key.
fn reach_fresh_lst(
    svm: &mut litesvm::LiteSVM,
    gov: &Keypair,
    cma: &Keypair,
) -> (Pubkey, OracleHandles, Pubkey) {
    let coll = bootstrap_market(svm, gov, cma);
    let (h, stake_pool) = bootstrap_oracle_lst(svm, gov, &coll, 300, 3, 300);
    set_whirlpool_pool(svm, &h.orca_pool, SQRT_100, &coll, &h.quote);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 1");
    warp_unix(svm, 150);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 2");
    warp_unix(svm, 150);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 3");
    assert_eq!(dex_twap_count(svm, &coll), 3);
    (coll, h, stake_pool)
}

/// Post the LST/USD market legs (Pyth + Switchboard) at `$market`, conf 0 (so the −k·σ haircut is
/// zero and `spot` equals the chosen mid exactly).
fn post_market(svm: &mut litesvm::LiteSVM, h: &OracleHandles, market_usd: i64) {
    let now = now_unix(svm);
    set_pyth_price(svm, &h.pyth, h.feed_id, pyth_usd(market_usd), 0, PYTH_EXPO, now);
    set_switchboard_feed(svm, &h.sb, sb_usd(market_usd as i128), 0, 1, now);
}

/// Post the SOL/USD canonical underlying (bound to the shared feed id) + the stake-pool account
/// (rate = `total_lamports / pool_token_supply`), both fresh.
fn post_canonical(
    svm: &mut litesvm::LiteSVM,
    sol_usd_key: &Pubkey,
    stake_pool: &Pubkey,
    sol_usd: i64,
    total_lamports: u64,
    pool_token_supply: u64,
) {
    let now = now_unix(svm);
    let epoch = now_epoch(svm);
    set_pyth_price(svm, sol_usd_key, PYTH_SOL_USD_FEED_ID, pyth_usd(sol_usd), 0, PYTH_EXPO, now);
    set_stake_pool(svm, stake_pool, total_lamports, pool_token_supply, epoch);
}

#[test]
fn canonical_caps_collateral_below_inflated_market() {
    // SOL dropped to $90 but the LST/USD market legs still read $100 (lagging high). The trustless
    // canonical (sol_usd $90 · rate 1.0 = $90) caps the collateral price at $90 even though every
    // market leg agrees at $100 — the over-mint defense. Mints stay OPEN (canonical present + market
    // legs agree), but borrowing power is valued off $90, not $100.
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fresh_lst(&mut svm, &gov, &cma);
    let sol_usd = Pubkey::new_unique();

    post_market(&mut svm, &h, 100);
    post_canonical(&mut svm, &sol_usd, &stake_pool, 90, lst(1_000_000), lst(1_000_000)); // rate 1.0
    send(
        &mut svm,
        &[update_price_lst_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb), Some(sol_usd), Some(stake_pool))],
        &gov,
        &[],
    )
    .expect("crank LST");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(!m.mint_frozen, "market legs agree + canonical present ⇒ mints open");
    assert_eq!(m.spot, spot_for_usd(90), "collateral capped at the canonical $90, NOT the market $100");
    assert_eq!(m.debt_spot, spot_for_usd(100), "debt/redemption stays on the raw market $100");
}

#[test]
fn canonical_above_market_is_a_noop() {
    // SOL at $110, rate 1.0 ⇒ canonical $110, above the $100 market. MIN keeps the (lower) market
    // price — the leg only defends against an UPWARD-manipulated market, never inflates collateral.
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fresh_lst(&mut svm, &gov, &cma);
    let sol_usd = Pubkey::new_unique();

    post_market(&mut svm, &h, 100);
    post_canonical(&mut svm, &sol_usd, &stake_pool, 110, lst(1_000_000), lst(1_000_000));
    send(
        &mut svm,
        &[update_price_lst_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb), Some(sol_usd), Some(stake_pool))],
        &gov,
        &[],
    )
    .expect("crank LST");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(!m.mint_frozen);
    assert_eq!(m.spot, spot_for_usd(100), "canonical above market ⇒ collateral unchanged at $100");
}

#[test]
fn canonical_uses_stake_pool_rate() {
    // SOL at $100, rate 1.2 (1.2M lamports backing 1M tokens) ⇒ canonical $120, above the $100
    // market ⇒ MIN keeps $100. Proves the rate is read from total_lamports/pool_token_supply: drop
    // the rate to 0.8 and the canonical ($80) now caps below the market.
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fresh_lst(&mut svm, &gov, &cma);
    let sol_usd = Pubkey::new_unique();

    post_market(&mut svm, &h, 100);
    post_canonical(&mut svm, &sol_usd, &stake_pool, 100, lst(1_200_000), lst(1_000_000)); // rate 1.2
    send(
        &mut svm,
        &[update_price_lst_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb), Some(sol_usd), Some(stake_pool))],
        &gov,
        &[],
    )
    .expect("crank rate 1.2");
    assert_eq!(read_market(&svm, &market_pda(&coll)).spot, spot_for_usd(100), "rate 1.2 ⇒ canonical $120 > market");

    // Re-crank with rate 0.8 (800k lamports / 1M tokens) ⇒ canonical $80 < market $100 ⇒ cap to $80.
    warp_unix(&mut svm, 5);
    post_market(&mut svm, &h, 100);
    post_canonical(&mut svm, &sol_usd, &stake_pool, 100, lst(800_000), lst(1_000_000));
    send(
        &mut svm,
        &[update_price_lst_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb), Some(sol_usd), Some(stake_pool))],
        &gov,
        &[],
    )
    .expect("crank rate 0.8");
    assert_eq!(read_market(&svm, &market_pda(&coll)).spot, spot_for_usd(80), "rate 0.8 ⇒ canonical $80 caps");
}

#[test]
fn missing_canonical_freezes_mints_but_serves_market_spot() {
    // An LST market crank WITHOUT the canonical accounts (omit the stake pool) cannot verify the
    // over-mint defense ⇒ mints freeze. But the market legs are fresh + agreeing, so a conservative
    // price is still committed (repay/liquidation/redemption stay alive — the peg floor).
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fresh_lst(&mut svm, &gov, &cma);
    let sol_usd = Pubkey::new_unique();
    post_market(&mut svm, &h, 100);
    post_canonical(&mut svm, &sol_usd, &stake_pool, 100, lst(1_000_000), lst(1_000_000));

    // Omit BOTH canonical accounts: the LST market freezes mints.
    send(
        &mut svm,
        &[update_price_lst_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb), None, None)],
        &gov,
        &[],
    )
    .expect("crank without canonical");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen, "LST market with no canonical leg ⇒ mints frozen");
    assert_eq!(m.spot, spot_for_usd(100), "but a conservative price is still served off the market");

    // Supplying the canonical accounts on the next crank clears the freeze.
    warp_unix(&mut svm, 5);
    post_market(&mut svm, &h, 100);
    post_canonical(&mut svm, &sol_usd, &stake_pool, 100, lst(1_000_000), lst(1_000_000));
    send(
        &mut svm,
        &[update_price_lst_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb), Some(sol_usd), Some(stake_pool))],
        &gov,
        &[],
    )
    .expect("crank with canonical");
    assert!(!read_market(&svm, &market_pda(&coll)).mint_frozen, "canonical present ⇒ mints open");
}

#[test]
fn wrong_stake_pool_account_reverts() {
    // A present-but-WRONG stake-pool account (key != market_oracle.lst_stake_pool) is a mis-built
    // crank ⇒ hard revert (InvalidStakePool), not a silent degrade — distinct from an ABSENT leg.
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fresh_lst(&mut svm, &gov, &cma);
    let sol_usd = Pubkey::new_unique();
    post_market(&mut svm, &h, 100);
    post_canonical(&mut svm, &sol_usd, &stake_pool, 100, lst(1_000_000), lst(1_000_000));

    // A different, valid-looking stake pool the market did not bind.
    let wrong_pool = Pubkey::new_unique();
    let epoch = now_epoch(&svm);
    set_stake_pool(&mut svm, &wrong_pool, lst(1_000_000), lst(1_000_000), epoch);
    let f = send(
        &mut svm,
        &[update_price_lst_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb), Some(sol_usd), Some(wrong_pool))],
        &gov,
        &[],
    )
    .expect_err("wrong stake pool key must revert");
    assert_eq!(custom_code(&f), E_INVALID_STAKE_POOL);
}
