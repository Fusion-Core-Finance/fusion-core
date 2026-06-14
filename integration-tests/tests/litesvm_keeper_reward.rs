//! Audit B2 — the self-funding keeper reward. `refresh_market` mints accrued interest into the
//! insurance buffer; when governance enables `keeper_reward_bps`, the cranker that supplies an fUSD
//! sink is paid that cut of the minted interest (the rest still funds the buffer). It is a SPLIT of
//! interest the protocol already mints — never a fresh mint — so the supply invariant and credible
//! neutrality hold, and it is spam-proof (a second immediate crank mints ~0).
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_keeper_reward

use fusd_core::state::MarketParam;
use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const ONE_YEAR: i64 = 365 * 86_400;

/// Market + a GovernanceGate (timelock 0) + a live $100 price.
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

fn set_param(svm: &mut litesvm::LiteSVM, gov: &Keypair, coll: &Pubkey, nonce: u64, param: MarketParam, value: u64) {
    send(svm, &[queue_param_change_ix(&gov.pubkey(), coll, nonce, param, value)], gov, &[]).expect("queue param");
    send(svm, &[execute_param_change_ix(&gov.pubkey(), coll, nonce)], gov, &[]).expect("execute param");
}

#[test]
fn refresh_market_pays_keeper_cut_of_interest() {
    let (mut svm, gov, cma, coll) = setup();
    // Enable a 5% keeper reward (500 bps of the minted interest).
    set_param(&mut svm, &gov, &coll, 0, MarketParam::KeeperReward, 500);
    assert_eq!(read_market(&svm, &market_pda(&coll)).keeper_reward_bps, 500);

    // $1000 debt at 10%/yr ⇒ $100 of interest after a year.
    let _a = open_borrower_rate(&mut svm, &cma, &coll, 50, usd(1_000), 1_000);

    // A keeper (not gov) with its own fUSD ATA does the crank.
    let keeper = Keypair::new();
    airdrop_sol(&mut svm, &keeper.pubkey(), 10);
    let keeper_ata = create_ata_and_fund(&mut svm, &keeper, &keeper.pubkey(), &fusd_mint_pda(), None, 0);

    warp_unix(&mut svm, ONE_YEAR);
    send(&mut svm, &[refresh_market_reward_ix(&coll, Some(keeper_ata))], &keeper, &[])
        .expect("refresh with keeper reward");

    // $100 interest split: 5% ($5) to the keeper, 95% ($95) to the buffer.
    assert_eq!(token_balance(&svm, &keeper_ata), usd(5), "keeper got the 5% cut");
    assert_eq!(buffer_balance(&svm, &coll), usd(95), "buffer got the remaining 95%");
    assert_eq!(
        read_insurance_buffer(&svm, &coll).total_funded,
        usd(95) as u128,
        "total_funded tracks only the buffer's share"
    );
    // The whole $100 was minted (both shares) and booked as debt — supply invariant holds.
    assert_eq!(read_market(&svm, &market_pda(&coll)).agg_recorded_debt, usd(1_100) as u128);
    assert_eq!(read_market(&svm, &market_pda(&coll)).unminted_interest, 0);
    assert_eq!(mint_supply(&svm, &fusd_mint_pda()), usd(1_100));
    assert_supply_invariant(&svm, &coll);

    // Spam-proof: a second immediate crank mints ~0 (no elapsed interest), so ~0 reward.
    svm.expire_blockhash();
    send(&mut svm, &[refresh_market_reward_ix(&coll, Some(keeper_ata))], &keeper, &[])
        .expect("refresh again");
    assert_eq!(token_balance(&svm, &keeper_ata), usd(5), "no extra reward without new interest");
    assert_eq!(buffer_balance(&svm, &coll), usd(95));
    assert_supply_invariant(&svm, &coll);
}

#[test]
fn refresh_without_sink_sends_all_interest_to_buffer() {
    // Reward enabled but the cranker supplies no fUSD sink ⇒ the WHOLE interest funds the buffer
    // (the reward is never forfeited to nowhere).
    let (mut svm, gov, cma, coll) = setup();
    set_param(&mut svm, &gov, &coll, 0, MarketParam::KeeperReward, 500);
    let _a = open_borrower_rate(&mut svm, &cma, &coll, 50, usd(1_000), 1_000);

    warp_unix(&mut svm, ONE_YEAR);
    send(&mut svm, &[refresh_market_ix(&coll)], &gov, &[]).expect("refresh, no sink");
    assert_eq!(buffer_balance(&svm, &coll), usd(100), "all $100 to the buffer when no sink is given");
    assert_supply_invariant(&svm, &coll);
}

#[test]
fn keeper_reward_disabled_by_default_pays_nothing() {
    // Default keeper_reward_bps == 0 ⇒ even with a sink provided, the whole interest funds the buffer.
    let (mut svm, _gov, cma, coll) = setup();
    assert_eq!(read_market(&svm, &market_pda(&coll)).keeper_reward_bps, 0);
    let _a = open_borrower_rate(&mut svm, &cma, &coll, 50, usd(1_000), 1_000);

    let keeper = Keypair::new();
    airdrop_sol(&mut svm, &keeper.pubkey(), 10);
    let keeper_ata = create_ata_and_fund(&mut svm, &keeper, &keeper.pubkey(), &fusd_mint_pda(), None, 0);

    warp_unix(&mut svm, ONE_YEAR);
    send(&mut svm, &[refresh_market_reward_ix(&coll, Some(keeper_ata))], &keeper, &[]).expect("refresh");
    assert_eq!(token_balance(&svm, &keeper_ata), 0, "no reward when the param is 0");
    assert_eq!(buffer_balance(&svm, &coll), usd(100), "all interest to the buffer");
    assert_supply_invariant(&svm, &coll);
}

#[test]
fn keeper_reward_clamp_enforced() {
    let (mut svm, gov, _cma, coll) = setup();
    let over = fusd_core::constants::MAX_KEEPER_REWARD_BPS as u64 + 1;
    let f = send(
        &mut svm,
        &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::KeeperReward, over)],
        &gov,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
}
