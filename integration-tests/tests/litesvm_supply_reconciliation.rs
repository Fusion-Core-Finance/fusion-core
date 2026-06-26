//! Supply reconciliation (proof-of-reserves). The permissionless `reconcile_supply` crank re-derives
//! FUSD's sharded global invariant `mint.supply == Σ_market (agg − unminted + bad)` from the live
//! `Market` accounts and stamps the residual on the `SupplyReconciliation` singleton. Auditability
//! only — it gates no user path; a non-zero residual is an off-chain alarm signal.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_supply_reconciliation

use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

fn actors() -> (litesvm::LiteSVM, Keypair, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    (svm, gov, Keypair::new())
}

#[test]
fn reconciles_to_zero_residual_after_borrow_and_refresh() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("price");
    send(&mut svm, &[init_supply_reconciliation_ix(&gov.pubkey())], &gov, &[]).expect("init recon");

    // Borrow (mints), warp + refresh (mints interest to the buffer) — all captured in agg/unminted/bad.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(2_000), 500);
    warp_unix(&mut svm, 30 * 86_400);
    send(&mut svm, &[refresh_market_ix(&coll)], &gov, &[]).expect("refresh");

    // Reconcile over the single market: residual must be exactly 0.
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    send(&mut svm, &[reconcile_supply_ix(&rando.pubkey(), &[market_pda(&coll)])], &rando, &[]).expect("reconcile");

    let r = read_supply_recon(&svm);
    assert_eq!(r.last_residual, 0, "summed markets exactly back the mint supply");
    assert_eq!(r.last_market_count, 1);
    assert_eq!(r.last_backing, r.last_mint_supply);
    assert!(r.last_mint_supply >= usd(2_000) as u128, "supply at least the borrowed principal");
    let _ = b;
}

#[test]
fn reconciles_across_two_markets() {
    let (mut svm, gov, cma) = actors();
    let coll_a = bootstrap_market(&mut svm, &gov, &cma);
    let coll_b = bootstrap_extra_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll_a, spot_for_usd(100))], &gov, &[]).expect("price a");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll_b, spot_for_usd(100))], &gov, &[]).expect("price b");
    send(&mut svm, &[init_supply_reconciliation_ix(&gov.pubkey())], &gov, &[]).expect("init recon");

    open_borrower_rate(&mut svm, &cma, &coll_a, 100, usd(1_500), 500);
    open_borrower_rate(&mut svm, &cma, &coll_b, 100, usd(2_500), 500);

    // BOTH markets passed ⇒ residual 0.
    send(&mut svm, &[reconcile_supply_ix(&gov.pubkey(), &[market_pda(&coll_a), market_pda(&coll_b)])], &gov, &[]).expect("reconcile both");
    let r = read_supply_recon(&svm);
    assert_eq!(r.last_residual, 0, "two markets sum to the full supply");
    assert_eq!(r.last_market_count, 2);

    // Omitting market B ⇒ the summed backing falls SHORT of the mint supply ⇒ negative residual
    // (the mint shows more than the submitted markets back) — exactly the missing-market signal.
    send(&mut svm, &[reconcile_supply_ix(&gov.pubkey(), &[market_pda(&coll_a)])], &gov, &[]).expect("reconcile a only");
    let r = read_supply_recon(&svm);
    assert!(r.last_residual < 0, "omitting a market under-counts the backing (residual < 0)");
    assert_eq!(r.last_residual, -(usd(2_500) as i128), "short by market B's debt");
}

#[test]
fn reconcile_rejects_duplicate_and_non_market() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("price");
    send(&mut svm, &[init_supply_reconciliation_ix(&gov.pubkey())], &gov, &[]).expect("init recon");
    open_borrower_rate(&mut svm, &cma, &coll, 100, usd(1_000), 500);

    // The SAME market passed twice would double-count its debt — rejected as a duplicate.
    let f = send(&mut svm, &[reconcile_supply_ix(&gov.pubkey(), &[market_pda(&coll), market_pda(&coll)])], &gov, &[])
        .expect_err("duplicate market");
    assert_eq!(custom_code(&f), E_DUPLICATE_REDEMPTION_TARGET);

    // A non-program-owned account (a fresh keypair) is rejected (can't skew the sum with junk).
    let junk = Pubkey::new_unique();
    let f = send(&mut svm, &[reconcile_supply_ix(&gov.pubkey(), &[junk])], &gov, &[]).expect_err("non-market");
    assert_eq!(custom_code(&f), E_INVALID_RECIPIENT);
}
