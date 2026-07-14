//! In-process litesvm regression for **`Position.ink_nonce`** (fuSOL stake-pool groundwork): the
//! monotonic collateral-change nonce bumps on every REAL `ink` change — deposit, withdraw,
//! redemption drain, liquidation seize, and the lazy tier-2 redistribution fold on a
//! debt-only touch — and does NOT bump on debt-only ops with nothing pending. The nonce is
//! purely informational to fusd-core; the stake-pool Allocation Controller reads it to
//! invalidate validator-direction preferences whenever collateral moves.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_ink_nonce

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

/// Deposit/withdraw bump; borrow/repay with no pending redistribution do not.
#[test]
fn lifecycle_ops_bump_only_on_ink_change() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // open (nonce 0) + deposit 10 tokens (0 -> 1) + borrow $100 (debt-only, stays 1).
    let a = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(100));
    assert_eq!(read_position(&svm, &a.position).ink_nonce, 1, "open+deposit+borrow");

    // withdraw 1 token -> 2.
    send(
        &mut svm,
        &[withdraw_ix(&a.kp.pubkey(), &coll, &a.coll_ata, whole_coll(1))],
        &a.kp,
        &[],
    )
    .expect("withdraw failed");
    assert_eq!(read_position(&svm, &a.position).ink_nonce, 2, "withdraw");

    // deposit 1 token -> 3.
    fund_and_deposit(&mut svm, &coll_mint_auth, &coll, &a, whole_coll(1));
    assert_eq!(read_position(&svm, &a.position).ink_nonce, 3, "deposit");

    // repay $50 then borrow $10 — debt-only touches, no pending redistribution: still 3.
    send(&mut svm, &[repay_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(50))], &a.kp, &[])
        .expect("repay failed");
    send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(10))], &a.kp, &[])
        .expect("borrow failed");
    assert_eq!(read_position(&svm, &a.position).ink_nonce, 3, "debt-only ops must not bump");
}

/// Liquidation bumps the victim; the lazy redistribution fold bumps a recipient on a
/// DEBT-ONLY touch (borrow) — the case the controller must not miss (spec §7.3 names
/// realized redistribution explicitly).
#[test]
fn liquidation_and_redistribution_fold_bump() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // RP left EMPTY (c never provides) so b's liquidation fully redistributes to c.
    let c = open_borrower(&mut svm, &coll_mint_auth, &coll, 1_000, usd(400));
    let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));
    assert_eq!(read_position(&svm, &b.position).ink_nonce, 1);
    assert_eq!(read_position(&svm, &c.position).ink_nonce, 1);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate failed");

    // Victim: ink 10e9 -> 0 is a collateral change.
    let bp = read_position(&svm, &b.position);
    assert_eq!((bp.ink, bp.ink_nonce), (0, 2), "liquidation seize bumps the victim");

    // Recipient c touches with a DEBT-ONLY borrow: realize folds the pending redistributed
    // collateral into ink — the nonce must bump even though no explicit collateral leg ran.
    let c_ink_before = read_position(&svm, &c.position).ink;
    send(&mut svm, &[borrow_ix(&c.kp.pubkey(), &coll, &c.fusd_ata, usd(1))], &c.kp, &[])
        .expect("borrow failed");
    let cp = read_position(&svm, &c.position);
    assert!(cp.ink > c_ink_before, "fold grew ink");
    assert_eq!(cp.ink_nonce, 2, "redistribution fold on a debt-only touch bumps");
}

/// An ordered redemption that drains collateral from a candidate bumps its nonce.
#[test]
fn redemption_bumps_the_redeemed_candidate() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // Target holds debt in the lowest bucket; redeemer holds fUSD from its own position.
    let target = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(100));
    let redeemer = open_borrower(&mut svm, &coll_mint_auth, &coll, 100, usd(200));
    assert_eq!(read_position(&svm, &target.position).ink_nonce, 1);

    send(
        &mut svm,
        &[redeem_ix(
            &redeemer.kp.pubkey(),
            &coll,
            &redeemer.fusd_ata,
            &redeemer.coll_ata,
            &[target.position, redeemer.position],
            usd(50),
        )],
        &redeemer.kp,
        &[],
    )
    .expect("redeem failed");

    // One of the two same-bucket candidates was drained (lowest-CR-first tiebreak); whichever
    // lost collateral must have bumped. The redeemer has the lower CR ($200 debt on $10k coll
    // vs $100 on $1k — target CR 10x, redeemer 50x -> target is LOWER? No: target 1000/100=10,
    // redeemer 10000/200=50 -> target redeemed first). Assert on the target.
    let tp = read_position(&svm, &target.position);
    assert!(tp.ink < whole_coll(10), "target lost collateral to the redemption");
    assert_eq!(tp.ink_nonce, 2, "redemption drain bumps");
}
