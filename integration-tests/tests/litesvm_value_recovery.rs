//! In-process litesvm tests for the value-recovery instructions:
//! `withdraw_surplus` (redemption-fee surplus), `sweep_protocol_collateral` (un-homed remainder), and
//! `settle_bad_debt` (the recap settlement burn). All three are gated on the
//! `GovernanceGate.inbound_authority`, only ever move PROTOCOL-OWNED value, and preserve the 4-term
//! vault invariant + the supply invariant. (Market retirement is a deferred follow-on.)
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_value_recovery

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

const E_INSUFFICIENT_PROTOCOL_COLLATERAL: u32 = 6039;

/// Move `amount` fUSD from `from`'s ATA to `to`'s ATA (test plumbing for funding the gov authority).
fn transfer_fusd(svm: &mut litesvm::LiteSVM, from: &Keypair, from_ata: &solana_sdk::pubkey::Pubkey, to_ata: &solana_sdk::pubkey::Pubkey, amount: u64) {
    let ix = spl_token::instruction::transfer(&spl_token::ID, from_ata, to_ata, &from.pubkey(), &[], amount).unwrap();
    send(svm, &[ix], from, &[]).expect("transfer fUSD");
}

/// `withdraw_surplus`: governance recovers accrued redemption-fee surplus to a recipient; only the
/// gate authority may, never more than the tracked surplus, and the vault invariant holds throughout.
#[test]
fn withdraw_surplus_recovers_redemption_fees() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market_with_fee(&mut svm, &gov, &cma, REDEMPTION_FEE_BPS); // 0.5%
    let market = market_pda(&coll);
    send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), 0)], &gov, &[]).expect("gov gate");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    // Accrue fee surplus via a redemption (B at bucket 30 is the target; R is the redeemer).
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 300);
    let r = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 500);
    send(&mut svm, &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], usd(300))], &r.kp, &[])
        .expect("redeem accrues a fee");
    let surplus = read_market(&svm, &market).surplus_collateral;
    assert!(surplus > 0, "redemption fee accrued as surplus");
    assert_vault_invariant(&svm, &coll);

    let recipient = create_ata_and_fund(&mut svm, &gov, &gov.pubkey(), &coll, None, 0);

    // Only the gate authority may withdraw.
    let mallory = Keypair::new();
    airdrop_sol(&mut svm, &mallory.pubkey(), 10);
    let f = send(&mut svm, &[withdraw_surplus_ix(&mallory.pubkey(), &coll, &recipient, surplus)], &mallory, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED, "non-authority rejected");
    // Can't over-withdraw beyond the tracked surplus.
    let f2 = send(&mut svm, &[withdraw_surplus_ix(&gov.pubkey(), &coll, &recipient, surplus + 1)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f2), E_INSUFFICIENT_PROTOCOL_COLLATERAL, "over-withdraw rejected");

    send(&mut svm, &[withdraw_surplus_ix(&gov.pubkey(), &coll, &recipient, surplus)], &gov, &[]).expect("withdraw surplus");
    assert_eq!(read_market(&svm, &market).surplus_collateral, 0, "surplus drained");
    assert_eq!(token_balance(&svm, &recipient), surplus, "recipient received the fee surplus");
    assert_vault_invariant(&svm, &coll);
}

/// `sweep_protocol_collateral`: governance recovers the retained un-homed collateral; bounded by the
/// tracked amount, authority-gated, leaves `bad_debt` (the loss record) intact, preserves the invariant.
#[test]
fn sweep_protocol_collateral_recovers_unhomed() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);
    send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), 0)], &gov, &[]).expect("gov gate");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    // B is the only position, RP + buffer empty: its liquidation is UN-HOMED -> protocol_collateral + bad_debt.
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("p80");
    liquidate(&mut svm, &gov, &coll, &b.position).expect("un-homed liquidation");

    let m = read_market(&svm, &market);
    assert!(m.shutdown, "un-homed trips shutdown");
    assert_eq!(m.bad_debt, usd(600) as u128);
    let pc = m.protocol_collateral;
    assert!(pc > 0, "the seized collateral is retained as protocol_collateral");
    assert_vault_invariant(&svm, &coll);

    // A fresh treasury receives the recovered collateral (gov already holds a collateral ATA from the
    // liquidation gas-comp, so the recipient must be a distinct owner).
    let treasury = Keypair::new();
    let recipient = create_ata_and_fund(&mut svm, &gov, &treasury.pubkey(), &coll, None, 0);

    let mallory = Keypair::new();
    airdrop_sol(&mut svm, &mallory.pubkey(), 10);
    let f = send(&mut svm, &[sweep_protocol_collateral_ix(&mallory.pubkey(), &coll, &recipient, pc)], &mallory, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);
    let f2 = send(&mut svm, &[sweep_protocol_collateral_ix(&gov.pubkey(), &coll, &recipient, pc + 1)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f2), E_INSUFFICIENT_PROTOCOL_COLLATERAL);

    send(&mut svm, &[sweep_protocol_collateral_ix(&gov.pubkey(), &coll, &recipient, pc)], &gov, &[]).expect("sweep");
    let m = read_market(&svm, &market);
    assert_eq!(m.protocol_collateral, 0, "retained collateral recovered");
    assert_eq!(token_balance(&svm, &recipient), pc, "recipient received it");
    assert_eq!(m.bad_debt, usd(600) as u128, "bad_debt (the loss record) is unchanged by the sweep");
    assert_vault_invariant(&svm, &coll);
}

/// `settle_bad_debt`: governance burns recovered fUSD to retire realized bad debt; both sides of the
/// supply invariant drop by the burned amount, and it can't over-settle beyond `bad_debt`.
#[test]
fn settle_bad_debt_burns_fusd_and_clears_the_loss() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);
    send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), 0)], &gov, &[]).expect("gov gate");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600)); // B holds the $600 it borrowed
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("p80");
    liquidate(&mut svm, &gov, &coll, &b.position).expect("un-homed");
    assert_eq!(read_market(&svm, &market).bad_debt, usd(600) as u128);
    assert_supply_invariant(&svm, &coll); // circulating $600 == agg(0) - unminted(0) + bad_debt(600)

    // Recap: governance buys back the unbacked fUSD (here we just hand B's $600 to gov), then burns it.
    let gov_fusd = create_ata_and_fund(&mut svm, &gov, &gov.pubkey(), &fusd_mint_pda(), None, 0);
    transfer_fusd(&mut svm, &b.kp, &b.fusd_ata, &gov_fusd, usd(600));

    // Can't settle more than the realized bad debt.
    let f = send(&mut svm, &[settle_bad_debt_ix(&gov.pubkey(), &coll, &gov_fusd, usd(600) + 1)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_INSUFFICIENT_PROTOCOL_COLLATERAL);

    send(&mut svm, &[settle_bad_debt_ix(&gov.pubkey(), &coll, &gov_fusd, usd(600))], &gov, &[]).expect("settle");
    assert_eq!(read_market(&svm, &market).bad_debt, 0, "bad debt retired");
    assert_eq!(token_balance(&svm, &gov_fusd), 0, "the recovered fUSD was burned");
    assert_eq!(mint_supply(&svm, &fusd_mint_pda()), 0, "circulating supply reduced by the burn");
    assert_supply_invariant(&svm, &coll);
}
