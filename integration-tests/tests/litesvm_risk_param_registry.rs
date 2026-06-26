//! RiskParamRegistry — the broader gate-tunable set beyond the Market params: the `MarketOracle`
//! thresholds + `Market.scr_bps`, all governed through the SAME timelocked gate (queue → delay →
//! execute), clamped at both ends, with the oracle account threaded through for the oracle params.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_risk_param_registry

use fusd_core::state::MarketParam;
use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

/// Market + oracle + gov gate (timelock 0 so queue+execute land together).
fn setup() -> (litesvm::LiteSVM, Keypair, Keypair, solana_sdk::pubkey::Pubkey) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, /*raydium=*/ false);
    send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), 0)], &gov, &[]).expect("gate");
    (svm, gov, cma, coll)
}

/// Queue + execute an ORACLE-targeting param (carries the MarketOracle account).
fn set_oracle_param(svm: &mut litesvm::LiteSVM, gov: &Keypair, coll: &solana_sdk::pubkey::Pubkey, nonce: u64, param: MarketParam, value: u64) {
    send(svm, &[queue_param_change_oracle_ix(&gov.pubkey(), coll, nonce, param, value)], gov, &[]).expect("queue oracle param");
    send(svm, &[execute_param_change_oracle_ix(&gov.pubkey(), coll, nonce)], gov, &[]).expect("execute oracle param");
}

#[test]
fn gate_tunes_oracle_thresholds() {
    let (mut svm, gov, _cma, coll) = setup();
    // Tune several oracle thresholds through the gate; read them back off the MarketOracle account.
    set_oracle_param(&mut svm, &gov, &coll, 0, MarketParam::OracleMaxConf, 300);
    set_oracle_param(&mut svm, &gov, &coll, 1, MarketParam::OracleMaxDeviation, 150);
    set_oracle_param(&mut svm, &gov, &coll, 2, MarketParam::OracleMaxAge, 120);
    set_oracle_param(&mut svm, &gov, &coll, 3, MarketParam::OracleK, 25_000);
    set_oracle_param(&mut svm, &gov, &coll, 4, MarketParam::OracleTwapStaleness, 600);

    let o = read_market_oracle(&svm, &coll);
    assert_eq!(o.max_conf_bps, 300);
    assert_eq!(o.max_deviation_bps, 150);
    assert_eq!(o.max_age_secs, 120);
    assert_eq!(o.k_bps, 25_000);
    assert_eq!(o.twap_max_staleness_secs, 600);
}

#[test]
fn gate_tunes_scr_on_the_market() {
    let (mut svm, gov, _cma, coll) = setup();
    // Scr lives on the Market (not the oracle) → use the normal (non-oracle) builders.
    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::Scr, 12_000)], &gov, &[]).expect("queue scr");
    send(&mut svm, &[execute_param_change_ix(&gov.pubkey(), &coll, 0)], &gov, &[]).expect("execute scr");
    assert_eq!(read_market(&svm, &market_pda(&coll)).scr_bps, 12_000);
}

#[test]
fn oracle_param_clamps_are_enforced() {
    let (mut svm, gov, _cma, coll) = setup();
    // k_bps below MIN_ORACLE_K_BPS (10_000) is rejected at queue.
    let f = send(&mut svm, &[queue_param_change_oracle_ix(&gov.pubkey(), &coll, 0, MarketParam::OracleK, 5_000)], &gov, &[])
        .expect_err("k too low");
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
    // max_age above MAX_ORACLE_MAX_AGE_SECS (300) is rejected.
    let f = send(&mut svm, &[queue_param_change_oracle_ix(&gov.pubkey(), &coll, 0, MarketParam::OracleMaxAge, 9_999)], &gov, &[])
        .expect_err("age too high");
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
    // Scr above MAX_SCR_BPS (15_000) is rejected (Market param).
    let f = send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::Scr, 20_000)], &gov, &[])
        .expect_err("scr too high");
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
}

#[test]
fn oracle_divergence_relational_is_enforced() {
    let (mut svm, gov, _cma, coll) = setup();
    // The mint corridor (twap_max_divergence) defaults to 500. Setting the LIQ gate BELOW it (and
    // non-zero) violates the "liq >= twap" rule → ParamCombinationInvalid.
    let twap = read_market_oracle(&svm, &coll).twap_max_divergence_bps;
    assert!(twap > 0);
    let f = send(&mut svm, &[queue_param_change_oracle_ix(&gov.pubkey(), &coll, 0, MarketParam::OracleLiqDivergence, (twap - 1) as u64)], &gov, &[])
        .expect_err("liq < twap");
    assert_eq!(custom_code(&f), E_PARAM_COMBINATION_INVALID);
    // At/above the corridor it's accepted.
    set_oracle_param(&mut svm, &gov, &coll, 0, MarketParam::OracleLiqDivergence, (twap + 1_000) as u64);
    assert_eq!(read_market_oracle(&svm, &coll).liq_max_divergence_bps, twap + 1_000);
}

#[test]
fn scr_above_mcr_is_rejected_relationally() {
    let (mut svm, gov, _cma, coll) = setup();
    // MCR defaults to MCR_BPS; raise SCR above it (but still under MAX_SCR_BPS) → mcr >= scr fails.
    let mcr = read_market(&svm, &market_pda(&coll)).mcr_bps;
    // Pick an in-clamp scr (<= MAX_SCR_BPS 15_000) that exceeds mcr. If mcr is already high, skip.
    if mcr < 15_000 {
        let bad_scr = (mcr as u64 + 1).max(10_500); // > mcr, >= MIN_SCR_BPS
        if bad_scr <= 15_000 {
            let f = send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::Scr, bad_scr)], &gov, &[])
                .expect_err("scr > mcr");
            assert_eq!(custom_code(&f), E_PARAM_COMBINATION_INVALID);
        }
    }
}

#[test]
fn oracle_param_without_oracle_account_is_rejected() {
    let (mut svm, gov, _cma, coll) = setup();
    // Queuing an oracle-targeting param WITHOUT supplying the MarketOracle account is rejected
    // (the relational check requires the sibling in scope).
    let f = send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::OracleMaxConf, 300)], &gov, &[])
        .expect_err("missing oracle account");
    assert_eq!(custom_code(&f), E_PARAM_COMBINATION_INVALID);
}
