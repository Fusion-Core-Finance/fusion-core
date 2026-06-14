//! `init_market_oracle`: feed bindings + thresholds + the DexTwap ring account.
//! Requires the dev-oracle `.so` (see the harness crate docs).

use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

/// Returns (svm, gov, collateral mint, quote mint). The quote mint is the USD-stable leg
/// `init_market_oracle` now reads (key + decimals) to bind the CLMM pool's quote side.
fn setup() -> (litesvm::LiteSVM, Keypair, Pubkey, Pubkey) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let quote = create_quote_mint(&mut svm, &gov, FUSD_DECIMALS);
    (svm, gov, coll, quote)
}

#[test]
fn init_market_oracle_happy_path() {
    let (mut svm, gov, coll, quote) = setup();
    let args = default_oracle_args();
    let expected_sb = args.switchboard_feed;
    let expected_orca = args.orca_pool;
    send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args)], &gov, &[])
        .expect("init_market_oracle");

    let o = read_market_oracle(&svm, &coll);
    assert_eq!(o.collateral_mint, coll);
    assert_eq!(o.pyth_feed_id, [7u8; 32]);
    assert_eq!(o.switchboard_feed, expected_sb);
    assert_eq!(o.orca_pool, expected_orca);
    assert_eq!(o.raydium_pool, Pubkey::default());
    // The quote leg + both decimals are bound from the real mints.
    assert_eq!(o.quote_mint, quote);
    assert_eq!(o.collateral_decimals, COLL_DECIMALS);
    assert_eq!(o.quote_decimals, FUSD_DECIMALS);
    assert_eq!(o.max_conf_bps, fusd_core::constants::DEFAULT_ORACLE_CONF_BPS);
    assert_eq!(o.k_bps, fusd_core::constants::DEFAULT_ORACLE_K_BPS);
    assert_eq!(o.twap_window_secs, fusd_core::constants::DEFAULT_TWAP_WINDOW_SECS);

    // The DexTwap ring account exists with the right size and is zeroed (empty ring).
    let twap = svm.get_account(&dex_twap_pda(&coll)).expect("dex_twap created");
    assert_eq!(twap.data.len(), fusd_core::state::DexTwap::SPACE);
    assert!(twap.data[8..].iter().all(|&b| b == 0), "ring starts empty");
}

#[test]
fn init_market_oracle_rejects_non_gov() {
    let (mut svm, _gov, coll, quote) = setup();
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let f = send(
        &mut svm,
        &[init_market_oracle_ix(&rando.pubkey(), &coll, &quote, default_oracle_args())],
        &rando,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);
}

#[test]
fn init_market_oracle_rejects_out_of_clamp_params() {
    let (mut svm, gov, coll, quote) = setup();

    // conf cap above the 5% clamp
    let mut args = default_oracle_args();
    args.max_conf_bps = fusd_core::constants::MAX_ORACLE_CONF_BPS + 1;
    let f = send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);

    // k below 1 sigma
    let mut args = default_oracle_args();
    args.k_bps = fusd_core::constants::MIN_ORACLE_K_BPS - 1;
    let f = send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);

    // TWAP window too short (flash-pump resistance floor)
    let mut args = default_oracle_args();
    args.twap_window_secs = fusd_core::constants::MIN_TWAP_WINDOW_SECS - 1;
    let f = send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
}

#[test]
fn init_market_oracle_rejects_missing_bindings() {
    let (mut svm, gov, coll, quote) = setup();

    // zero Pyth feed id
    let mut args = default_oracle_args();
    args.pyth_feed_id = [0u8; 32];
    let f = send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);

    // no DEX pool at all (the corridor is load-bearing for mint mode)
    let mut args = default_oracle_args();
    args.orca_pool = Pubkey::default();
    args.raydium_pool = Pubkey::default();
    let f = send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
}

#[test]
fn init_market_oracle_rejects_same_pool_for_both_venues() {
    let (mut svm, gov, coll, quote) = setup();
    // Configuring the same address as BOTH the Orca and Raydium pool would let the
    // address-based venue selection shadow one venue with the other's program owner.
    let same = Pubkey::new_unique();
    let mut args = default_oracle_args();
    args.orca_pool = same;
    args.raydium_pool = same;
    let f = send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
}

#[test]
fn init_market_oracle_rejects_quote_equals_collateral() {
    let (mut svm, gov, coll, _quote) = setup();
    // Pass the collateral mint itself as the quote leg → rejected (a pool's two mints differ).
    let f = send(
        &mut svm,
        &[init_market_oracle_ix(&gov.pubkey(), &coll, &coll, default_oracle_args())],
        &gov,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
}

#[test]
fn init_market_oracle_cannot_reinit() {
    let (mut svm, gov, coll, quote) = setup();
    send(
        &mut svm,
        &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, default_oracle_args())],
        &gov,
        &[],
    )
    .expect("first init");
    // Second init must fail (account already exists).
    assert!(send(
        &mut svm,
        &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, default_oracle_args())],
        &gov,
        &[],
    )
    .is_err());
}
