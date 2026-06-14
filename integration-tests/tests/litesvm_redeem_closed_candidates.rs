//! Closed-candidate skip-not-revert in `redeem` / `urgent_redeem`.
//!
//! A candidate Position that was repaid + `close_position`d between tx build and execution is no
//! longer a program-owned account; before this guard, `Account::try_from` reverted the WHOLE
//! batch — a borrower-controllable grief (closing even refunds the rent), worst during a depeg
//! when the redemption floor matters most. Now such candidates are skipped like any other
//! no-longer-valid candidate, while PRESENT-but-wrong accounts (the deliberate hard-revert
//! boundary) still revert: program-owned non-Position → discriminator mismatch; wrong-market
//! Position → Unauthorized. Requires the dev-oracle `.so`.

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

/// Anchor's AccountDiscriminatorMismatch error code.
const ANCHOR_DISCRIMINATOR_MISMATCH: u32 = 3002;

fn setup() -> (litesvm::LiteSVM, Keypair, Keypair, solana_sdk::pubkey::Pubkey) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("dev_set_price");
    (svm, gov, cma, coll)
}

/// Fully unwind `who`'s position: repay everything, withdraw all collateral, close (rent+bond
/// refunded). The borrower can always do this — which is exactly why a closed candidate must be
/// a skip, not a batch revert.
fn repay_and_close(svm: &mut litesvm::LiteSVM, coll: &solana_sdk::pubkey::Pubkey, who: &Actor, debt: u64, ink: u64) {
    send(svm, &[repay_ix(&who.kp.pubkey(), coll, &who.fusd_ata, debt)], &who.kp, &[])
        .expect("repay");
    send(svm, &[withdraw_ix(&who.kp.pubkey(), coll, &who.coll_ata, ink)], &who.kp, &[])
        .expect("withdraw all");
    send(svm, &[close_position_ix(&who.kp.pubkey(), coll)], &who.kp, &[]).expect("close");
    assert!(svm.get_account(&who.position).map_or(true, |a| a.data.is_empty()));
}

#[test]
fn redeem_skips_closed_candidates_even_duplicated() {
    let (mut svm, _gov, cma, coll) = setup();
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(400)); // will close
    let c = open_borrower(&mut svm, &cma, &coll, 10, usd(400)); // stays — same (lowest) bucket
    let redeemer = open_borrower(&mut svm, &cma, &coll, 50, usd(500));

    repay_and_close(&mut svm, &coll, &b, usd(400), whole_coll(10));

    // The closed key is passed TWICE: the guard runs before the dedup, so it is skipped twice
    // (never DuplicateRedemptionTarget) — the pinned, deliberate semantics.
    let c_debt_before = read_position(&svm, &c.position).recorded_debt;
    send(
        &mut svm,
        &[redeem_ix(
            &redeemer.kp.pubkey(),
            &coll,
            &redeemer.fusd_ata,
            &redeemer.coll_ata,
            &[b.position, b.position, c.position],
            usd(100),
        )],
        &redeemer.kp,
        &[],
    )
    .expect("redeem skips the closed candidate and redeems the live one");
    assert_eq!(
        read_position(&svm, &c.position).recorded_debt,
        c_debt_before - usd(100) as u128,
        "the live same-bucket candidate absorbed the redemption"
    );
}

#[test]
fn urgent_redeem_skips_closed_candidate_mid_winddown() {
    let (mut svm, gov, cma, coll) = setup();
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(400));
    let c = open_borrower(&mut svm, &cma, &coll, 10, usd(600));
    let redeemer = open_borrower(&mut svm, &cma, &coll, 50, usd(500));

    // Crash + shutdown (TCR < SCR): ~70 tokens * $15 = $1050 vs $1500 debt.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(15))], &gov, &[])
        .expect("crash");
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown");

    // B unwinds mid-shutdown (repay/withdraw/close all stay open in shutdown).
    repay_and_close(&mut svm, &coll, &b, usd(400), whole_coll(10));

    send(
        &mut svm,
        &[urgent_redeem_ix(
            &redeemer.kp.pubkey(),
            &coll,
            &redeemer.fusd_ata,
            &redeemer.coll_ata,
            &[b.position, c.position],
            usd(50),
        )],
        &redeemer.kp,
        &[],
    )
    .expect("urgent_redeem skips the closed candidate");
    assert!(read_position(&svm, &c.position).recorded_debt < usd(600) as u128);
}

#[test]
fn all_candidates_closed_yields_nothing_to_redeem() {
    let (mut svm, _gov, cma, coll) = setup();
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(400));
    let _c = open_borrower(&mut svm, &cma, &coll, 10, usd(400)); // keeps the bitmap non-empty
    let redeemer = open_borrower(&mut svm, &cma, &coll, 50, usd(500));

    repay_and_close(&mut svm, &coll, &b, usd(400), whole_coll(10));

    // Every submitted candidate was closed ⇒ the program's own NothingToRedeem, never a raw
    // Anchor account error (defined semantics for the degenerate batch).
    let f = send(
        &mut svm,
        &[redeem_ix(
            &redeemer.kp.pubkey(),
            &coll,
            &redeemer.fusd_ata,
            &redeemer.coll_ata,
            &[b.position],
            usd(100),
        )],
        &redeemer.kp,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_NOTHING_TO_REDEEM);
}

#[test]
fn present_but_wrong_accounts_still_hard_revert() {
    let (mut svm, gov, cma, coll) = setup();
    let _b = open_borrower(&mut svm, &cma, &coll, 10, usd(400));
    let redeemer = open_borrower(&mut svm, &cma, &coll, 50, usd(500));

    // A Position from ANOTHER market: program-owned, right discriminator, wrong collateral_mint
    // → Unauthorized (account-substitution detection is not weakened by the skip guard).
    let cma_y = Keypair::new();
    let coll_y_kp = Keypair::new();
    create_mint(&mut svm, &gov, &coll_y_kp, COLL_DECIMALS, &cma_y.pubkey(), false);
    let coll_y = coll_y_kp.pubkey();
    send(
        &mut svm,
        &[init_market_ix(&gov.pubkey(), &coll_y, MCR_BPS, DEBT_CEILING, 0, 0, BUCKET_WIDTH_BPS, 0)],
        &gov,
        &[],
    )
    .expect("init market Y");
    let y_pos = open_borrower(&mut svm, &cma_y, &coll_y, 1, 0);

    let f = send(
        &mut svm,
        &[redeem_ix(
            &redeemer.kp.pubkey(),
            &coll,
            &redeemer.fusd_ata,
            &redeemer.coll_ata,
            &[y_pos.position],
            usd(100),
        )],
        &redeemer.kp,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED, "wrong-market Position hard-reverts");

    // A program-owned NON-Position account (the Market PDA itself) → discriminator mismatch.
    let f = send(
        &mut svm,
        &[redeem_ix(
            &redeemer.kp.pubkey(),
            &coll,
            &redeemer.fusd_ata,
            &redeemer.coll_ata,
            &[market_pda(&coll)],
            usd(100),
        )],
        &redeemer.kp,
        &[],
    )
    .unwrap_err();
    assert_eq!(
        custom_code(&f),
        ANCHOR_DISCRIMINATOR_MISMATCH,
        "program-owned non-Position hard-reverts"
    );
}
