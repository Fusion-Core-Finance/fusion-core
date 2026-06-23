//! BOLD-sweep C16 — the auto bad-debt paydown interlock. When a market carries realized un-homed
//! `bad_debt`, `refresh_market` diverts a governable (default-off) fraction of the interest it would
//! otherwise mint to the buffer and uses it to RETIRE `bad_debt` instead — automatic
//! recapitalization-from-revenue. Supply-preserving: the diverted slice is simply NOT minted while
//! `bad_debt` drops by the same amount.
//!
//! Producing un-homed `bad_debt` requires a SOLE staked position (any surviving position would absorb
//! the loss via redistribution first), so the diverted interest is the `unminted_interest` that
//! accrued BEFORE the terminal liquidation (folded in by `liquidate`'s `accrue` call, which runs
//! before it flips `shutdown`; `accrue` is then a no-op post-shutdown but the pending interest
//! remains for `refresh_market` to process).
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_c16_bad_debt_paydown

use fusd_core::state::MarketParam;
use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const YEAR: i64 = 365 * 86_400;

fn actors() -> (litesvm::LiteSVM, Keypair, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    (svm, gov, Keypair::new())
}

/// Queue + immediately execute a single param change (timelock 0).
fn set_param(svm: &mut litesvm::LiteSVM, gov: &Keypair, coll: &Pubkey, nonce: u64, param: MarketParam, value: u64) {
    send(svm, &[queue_param_change_ix(&gov.pubkey(), coll, nonce, param, value)], gov, &[]).expect("queue param");
    send(svm, &[execute_param_change_ix(&gov.pubkey(), coll, nonce)], gov, &[]).expect("execute param");
}

/// Stand up a market with a SOLE borrower at a non-zero rate, accrue a year of interest into
/// `unminted_interest`, then crash + liquidate it into un-homed `bad_debt` (no RP, empty buffer, no
/// backstop accounts ⇒ the whole loss is un-homed, shutting the market down). Returns `(svm, gov,
/// coll)` with `bad_debt > 0` AND `unminted_interest > 0`.
fn market_with_bad_debt_and_pending_interest() -> (litesvm::LiteSVM, Keypair, Pubkey) {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    init_gov_gate(&mut svm, &gov);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("price $100");

    // Sole borrower at the max rate so a year accrues a meaningful interest backlog.
    let victim = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(3_000), 2_550);
    warp_unix(&mut svm, YEAR);

    // Crash hard and liquidate. `liquidate` accrues (folding the year of interest into
    // `unminted_interest`) BEFORE it sets `shutdown`, so the pending interest survives the wind-down.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(20))], &gov, &[]).expect("crash $20");
    let liq = Keypair::new();
    airdrop_sol(&mut svm, &liq.pubkey(), 10);
    liquidate(&mut svm, &liq, &coll, &victim.position).expect("terminal liquidation");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.bad_debt > 0 && m.shutdown, "sole-position terminal liquidation ⇒ un-homed bad debt + shutdown");
    assert!(m.unminted_interest > 0, "a year of interest is pending in unminted_interest");
    assert_supply_invariant(&svm, &coll);
    (svm, gov, coll)
}

#[test]
fn paydown_diverts_interest_to_retire_bad_debt() {
    let (mut svm, gov, coll) = market_with_bad_debt_and_pending_interest();
    set_param(&mut svm, &gov, &coll, 0, MarketParam::BadDebtPaydown, 10_000); // 100% of post-keeper interest

    let m0 = read_market(&svm, &market_pda(&coll));
    let (bad0, pending) = (m0.bad_debt, m0.unminted_interest);
    // The year of interest (< the multi-thousand-dollar loss) is smaller than the bad debt, so at 100%
    // the entire interest backlog is diverted to paydown and the buffer receives nothing.
    assert!(pending < bad0, "pending interest < bad debt (so this is a partial paydown)");
    let buffer_before = token_balance(&svm, &buffer_fusd_vault_pda(&coll));

    send(&mut svm, &[refresh_market_ix(&coll)], &gov, &[]).expect("refresh diverts to paydown");

    let m1 = read_market(&svm, &market_pda(&coll));
    assert_eq!(m1.bad_debt, bad0 - pending, "bad_debt reduced by exactly the diverted interest");
    assert_eq!(m1.unminted_interest, 0, "the whole pending interest was consumed");
    assert_eq!(
        token_balance(&svm, &buffer_fusd_vault_pda(&coll)),
        buffer_before,
        "buffer got nothing at 100% paydown (loss recovery has priority)"
    );
    assert_supply_invariant(&svm, &coll);
}

#[test]
fn paydown_disabled_by_default_mints_to_buffer() {
    let (mut svm, gov, coll) = market_with_bad_debt_and_pending_interest();
    assert_eq!(read_market(&svm, &market_pda(&coll)).bad_debt_paydown_bps, 0, "off by default");

    let m0 = read_market(&svm, &market_pda(&coll));
    let (bad0, pending) = (m0.bad_debt, m0.unminted_interest);
    let buffer_before = token_balance(&svm, &buffer_fusd_vault_pda(&coll));

    send(&mut svm, &[refresh_market_ix(&coll)], &gov, &[]).expect("refresh, paydown off");

    let m1 = read_market(&svm, &market_pda(&coll));
    assert_eq!(m1.bad_debt, bad0, "bad_debt untouched when paydown disabled");
    assert_eq!(m1.unminted_interest, 0, "interest still drains");
    assert_eq!(
        token_balance(&svm, &buffer_fusd_vault_pda(&coll)),
        buffer_before + pending as u64,
        "the whole interest minted to the buffer (byte-identical to pre-C16)"
    );
    assert_supply_invariant(&svm, &coll);
}

#[test]
fn paydown_splits_interest_between_recovery_and_buffer() {
    let (mut svm, gov, coll) = market_with_bad_debt_and_pending_interest();
    set_param(&mut svm, &gov, &coll, 0, MarketParam::BadDebtPaydown, 4_000); // 40% to paydown, 60% to buffer

    let m0 = read_market(&svm, &market_pda(&coll));
    let (bad0, pending) = (m0.bad_debt, m0.unminted_interest);
    let expected_paydown = pending * 4_000 / 10_000; // floored, capped at bad0 (pending << bad0 here)
    assert!(expected_paydown < bad0);
    let buffer_before = token_balance(&svm, &buffer_fusd_vault_pda(&coll));

    send(&mut svm, &[refresh_market_ix(&coll)], &gov, &[]).expect("refresh splits");

    let m1 = read_market(&svm, &market_pda(&coll));
    assert_eq!(m1.bad_debt, bad0 - expected_paydown, "40% of interest retired bad debt");
    assert_eq!(
        token_balance(&svm, &buffer_fusd_vault_pda(&coll)),
        buffer_before + (pending - expected_paydown) as u64,
        "the remaining 60% funded the buffer"
    );
    assert_eq!(m1.unminted_interest, 0);
    assert_supply_invariant(&svm, &coll);
}
