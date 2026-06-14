//! In-process litesvm integration test for `redeem` (fusion-docs): redeem
//! fUSD for face-value collateral, draining the lowest non-empty rate bucket first, redeeming
//! within a bucket lowest-collateral-ratio-first, charging the flat fee (retained as market
//! surplus). Candidate positions are passed as remaining_accounts.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_redemption

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

/// Redeem against the lowest-bucket position at face value (no fee): debt cleared, collateral paid
/// out, the borrower leaves its bucket.
#[test]
fn redeem_lowest_bucket_at_face_value() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma); // fee 0
    let market = market_pda(&coll);
    let coll_vault = coll_vault_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    // Victim B at 3% -> bucket 30, $300 debt on 10 tokens.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 300);
    // Redeemer R borrows $300 (its own position is at 5% -> bucket 50, above B).
    let r = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 500);
    assert_eq!(lowest_bucket(&svm, &coll), Some(30));

    let agg_before = read_market(&svm, &market).agg_recorded_debt;
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], usd(300))],
        &r.kp,
        &[],
    )
    .expect("redeem failed");

    // B fully redeemed: debt 0, collateral down 3 tokens ($300 / $100); R received 3 tokens; R burned $300.
    let bp = read_position(&svm, &b.position);
    assert_eq!(bp.recorded_debt, 0, "debt cleared");
    assert_eq!(bp.ink, whole_coll(7), "10 - 3 tokens left to the owner");
    assert_eq!(token_balance(&svm, &r.coll_ata), whole_coll(3), "redeemer got face-value collateral");
    assert_eq!(token_balance(&svm, &r.fusd_ata), 0, "redeemer burned $300");
    assert!(!bucket_is_set(&svm, &coll, 30), "B left its bucket");
    assert_eq!(read_market(&svm, &market).agg_recorded_debt, agg_before - usd(300) as u128);
    // total_collateral == vault (no fee surplus here).
    let m = read_market(&svm, &market);
    assert_eq!(m.surplus_collateral, 0);
    assert_eq!(m.total_collateral, token_balance(&svm, &coll_vault) as u128);
}

/// Within one bucket, redemption hits the lowest-collateral-ratio position first (program sorts;
/// the submitted order is ignored).
#[test]
fn redeem_within_bucket_is_lowest_cr_first() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    // Both at 4% -> bucket 40, same $600 debt; B1 has less collateral (lower CR) than B2.
    let b1 = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(600), 400); // CR 1000/600
    let b2 = open_borrower_rate(&mut svm, &cma, &coll, 20, usd(600), 400); // CR 2000/600
    let r = open_borrower_rate(&mut svm, &cma, &coll, 20, usd(600), 500);

    // Redeem exactly one position's worth, submitting candidates in the WRONG (high-CR-first) order.
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b2.position, b1.position], usd(600))],
        &r.kp,
        &[],
    )
    .expect("redeem failed");

    // The lower-CR B1 is fully redeemed; B2 is untouched.
    assert_eq!(read_position(&svm, &b1.position).recorded_debt, 0, "lowest-CR redeemed first");
    assert_eq!(read_position(&svm, &b2.position).recorded_debt, usd(600) as u128, "higher-CR untouched");
    assert!(bucket_is_set(&svm, &coll, 40), "bucket still has B2");
    assert_eq!(bucket_count(&svm, &coll, 40), 1);
    assert_eq!(token_balance(&svm, &r.coll_ata), whole_coll(6)); // $600 / $100
}

/// The flat redemption fee is withheld from the redeemer and retained as market surplus, keeping
/// `vault == total_collateral + surplus_collateral` exact.
#[test]
fn redeem_charges_flat_fee_to_surplus() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market_with_fee(&mut svm, &gov, &cma, REDEMPTION_FEE_BPS); // 0.5%
    let market = market_pda(&coll);
    let coll_vault = coll_vault_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 300);
    let r = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 500);

    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], usd(300))],
        &r.kp,
        &[],
    )
    .expect("redeem failed");

    // coll_total = 3 tokens; fee = 0.5% = 15_000_000; redeemer gets the rest.
    let coll_total = whole_coll(3);
    let fee = coll_total * REDEMPTION_FEE_BPS as u64 / 10_000; // 15_000_000
    assert_eq!(token_balance(&svm, &r.coll_ata), coll_total - fee, "redeemer paid the fee");
    let m = read_market(&svm, &market);
    assert_eq!(m.surplus_collateral, fee, "fee retained as surplus");
    // Vault holds total_collateral + surplus exactly.
    assert_eq!(
        token_balance(&svm, &coll_vault) as u128,
        m.total_collateral + m.surplus_collateral as u128
    );
}

/// A candidate not in the lowest non-empty bucket can't be redeemed while a lower one is non-empty:
/// it is SKIPPED (skip-not-revert), so a wrong-bucket-only batch nets nothing to redeem.
/// The strict "can't skip a LOWER bucket" guarantee is intact — the higher bucket is simply not drained.
#[test]
fn redeem_skips_wrong_bucket() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    let _low = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 300); // bucket 30 (the lowest)
    let high = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 700); // bucket 70
    let r = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 500);
    assert_eq!(lowest_bucket(&svm, &coll), Some(30));

    // Try to redeem the higher bucket while a lower one is non-empty -> skipped -> nothing redeemed.
    let f = send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[high.position], usd(300))],
        &r.kp,
        &[],
    )
    .expect_err("higher bucket cannot be drained while a lower one is non-empty");
    assert_eq!(custom_code(&f), E_NOTHING_TO_REDEEM);
    assert_eq!(read_position(&svm, &high.position).recorded_debt, usd(300) as u128, "high untouched");
}

/// One stale candidate (wrong bucket) mixed with a valid one does NOT revert the whole batch — the
/// stale one is skipped and the valid lowest-bucket candidate still redeems.
#[test]
fn redeem_skips_invalid_candidate_and_redeems_the_valid_one() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    let low = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 300); // bucket 30 (the lowest)
    let high = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 700); // bucket 70 (wrong bucket)
    let r = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 500);
    assert_eq!(lowest_bucket(&svm, &coll), Some(30));

    // Submit the wrong-bucket candidate FIRST, then the valid lowest-bucket one.
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[high.position, low.position], usd(100))],
        &r.kp,
        &[],
    )
    .expect("the valid candidate still redeems despite the stale one");
    assert_eq!(read_position(&svm, &high.position).recorded_debt, usd(300) as u128, "wrong bucket skipped");
    assert_eq!(read_position(&svm, &low.position).recorded_debt, usd(200) as u128, "valid one redeemed $100");
}

/// Duplicate candidate is rejected; an empty candidate list reverts NothingToRedeem.
#[test]
fn redeem_rejects_duplicates_and_empty() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 300);
    let r = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 500);

    // Same position passed twice.
    let f = send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position, b.position], usd(300))],
        &r.kp,
        &[],
    )
    .expect_err("duplicate must be rejected");
    assert_eq!(custom_code(&f), E_DUPLICATE_REDEMPTION_TARGET);

    // No candidates at all.
    let f2 = send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[], usd(300))],
        &r.kp,
        &[],
    )
    .expect_err("empty candidate list must revert");
    assert_eq!(custom_code(&f2), E_NOTHING_TO_REDEEM);
}

/// redeem must realize pending tier-2 redistribution first: redeeming a candidate that carries
/// redistributed debt to zero must clear its TRUE debt (recorded + pending) and roll its snapshot,
/// so the debt does NOT resurrect on the position's next touch. (Regression for the critical review
/// finding.)
#[test]
fn redeem_realizes_pending_redistribution_no_resurrection() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma); // RP left empty -> liquidation redistributes
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    // C (bucket 30) is the ONLY other position, so it receives ALL of B's redistributed debt.
    let c = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(200), 300);
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(600), 700);

    // Liquidate B with an empty RP -> its $600 redistributes to C (C's recorded art is still $200).
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("p80");
    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate");
    assert_eq!(read_position(&svm, &c.position).recorded_debt, usd(200) as u128, "C's pending debt not yet realized");

    // The redeemer opens AFTER the liquidation, so it receives no redistribution itself.
    let r = open_borrower_rate(&mut svm, &cma, &coll, 20, usd(800), 500);

    // Redeem C's full TRUE debt ($800 = $200 own + $600 redistributed); redeem realizes it first.
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[c.position], usd(800))],
        &r.kp,
        &[],
    )
    .expect("redeem failed");
    assert_eq!(read_position(&svm, &c.position).recorded_debt, 0, "C fully redeemed (true debt)");
    assert!(!bucket_is_set(&svm, &coll, 30), "C left its bucket");

    // The fix: touching C again must NOT resurrect the redistributed debt.
    fund_and_deposit(&mut svm, &cma, &coll, &c, whole_coll(1));
    assert_eq!(read_position(&svm, &c.position).recorded_debt, 0, "no resurrected debt — redeem realized it");

    // agg_art now carries only R's remaining $800 (C and B fully cleared).
    assert_eq!(read_market(&svm, &market).agg_recorded_debt, usd(800) as u128);
}

/// Regression for the redeem validation-pass `set_stake`: when a candidate carries
/// pending tier-2 redistribution (so `realize` grows its `ink`) but is NOT reached in the redeem pass
/// (the `remaining == 0` break), its stake must still be recomputed in the validation pass — else it
/// persists grown ink with a stale stake. Asserts each candidate's stored stake equals
/// `compute_stake(current_ink, snapshots)` for BOTH the redeemed and the unreached candidate.
#[test]
fn redeem_unreached_candidate_keeps_consistent_stake() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // X and Y: 100 tokens each, both at 50 bps (the same lowest bucket). Y has slightly more debt, so
    // after the redist gain it sorts lower-CR and is redeemed FIRST, leaving X unreached.
    let x = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(100), 50);
    let y = open_borrower_rate(&mut svm, &cma, &coll, 100, usd(120), 50);
    // V: a higher-rate victim (different bucket) with an EMPTY RP, so its debt redistributes to X+Y.
    let v = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(600), 1_000);

    // Drop the price so V is under-MCR, then liquidate -> redistributes V's debt+collateral to X and Y
    // (each gains pending ink + debt; l_coll/l_art advance; system snapshots set).
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    liquidate(&mut svm, &gov, &coll, &v.position).expect("liquidate V");
    // Expire the blockhash so re-setting the price to $100 isn't rejected as an identical tx.
    svm.expire_blockhash();
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price back to $100");

    // Redeem a small amount over [X, Y]: both are realized in the validation pass (ink grows), but the
    // amount only covers part of the first-sorted (Y) -> the redeem pass breaks before X.
    let r = open_borrower(&mut svm, &cma, &coll, 50, usd(2_000));
    send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[x.position, y.position], usd(50))],
        &r.kp,
        &[],
    )
    .expect("redeem $50");

    // Both candidates' stored stake must equal compute_stake(their CURRENT ink, the system snapshots) —
    // including X, which was realized (ink grew) but never redeemed. Without the fix, X's stake lags.
    let m = read_market(&svm, &market);
    for pos_pk in [x.position, y.position] {
        let p = read_position(&svm, &pos_pk);
        let expected = fusd_math::redistribution::compute_stake(
            p.ink as u128,
            m.total_stakes_snapshot,
            m.total_collateral_snapshot,
        )
        .unwrap();
        assert_eq!(p.stake, expected, "stored stake == compute_stake(current ink)");
    }
    // total_stakes is exactly the sum of ALL live stored stakes (X, Y, and the redeemer R) — no drift.
    let sum: u128 =
        [x.position, y.position, r.position].iter().map(|p| read_position(&svm, p).stake).sum();
    assert_eq!(m.total_stakes, sum);
}
