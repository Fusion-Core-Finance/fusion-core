//! Per-market `shutdown` + `urgent_redeem` — the terminal circuit breaker (see fusion-docs).
//!
//! Covers both triggers (TCR < SCR on a fresh price; sustained oracle failure), the grief-resistance
//! of a never-priced market, the post-shutdown gating (borrow + ordered redeem closed), and the
//! urgent wind-down (unordered, 0-fee, face value at the last price even with a dead oracle).
//! Requires the dev-oracle `.so`.

use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

fn oracle_staleness() -> u64 {
    fusd_core::constants::SHUTDOWN_ORACLE_STALENESS_SLOTS
}

/// Bootstrap a market with a live $100 price. `gov` is governance + the permissionless caller.
fn setup() -> (litesvm::LiteSVM, Keypair, Keypair, Pubkey) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("dev_set_price");
    (svm, gov, cma, coll)
}

// ============================ triggers ============================

#[test]
fn shutdown_on_scr_breach() {
    let (mut svm, gov, cma, coll) = setup();
    // 10 tok @ $100 = $1000; borrow $600 (healthy at 150% MCR). Then a 40% crash to $60: collateral
    // $600 vs debt $600 = TCR 100% < SCR 110%.
    let _a = open_borrower(&mut svm, &cma, &coll, 10, usd(600));
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(60))], &gov, &[])
        .expect("drop price");
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown on SCR breach");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.shutdown);
    assert_eq!(m.shutdown_reason, fusd_core::constants::SHUTDOWN_REASON_SCR);
}

#[test]
fn shutdown_rejected_when_healthy() {
    let (mut svm, gov, cma, coll) = setup();
    let _a = open_borrower(&mut svm, &cma, &coll, 10, usd(400)); // TCR 250% at $100
    let f = send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_SHUTDOWN_CONDITION_NOT_MET);
    assert!(!read_market(&svm, &market_pda(&coll)).shutdown);
}

#[test]
fn shutdown_on_oracle_failure() {
    let (mut svm, gov, cma, coll) = setup();
    let _a = open_borrower(&mut svm, &cma, &coll, 10, usd(400)); // healthy, but...
    // ...the oracle goes dark: the cached price ages past the outage threshold.
    warp_slots(&mut svm, oracle_staleness() + 1);
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[])
        .expect("shutdown on oracle failure");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.shutdown);
    assert_eq!(m.shutdown_reason, fusd_core::constants::SHUTDOWN_REASON_ORACLE_FAILURE);
}

#[test]
fn fresh_market_cannot_be_griefed_into_shutdown() {
    // A market that was never priced (spot == 0, no debt) is pre-launch, not "failed" — even after
    // a long slot gap it must not be shut down by a griefer.
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma); // NO dev_set_price ⇒ spot == 0
    warp_slots(&mut svm, oracle_staleness() + 10_000);
    let f = send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_SHUTDOWN_CONDITION_NOT_MET);
}

#[test]
fn shutdown_is_terminal() {
    let (mut svm, gov, cma, coll) = setup();
    let _a = open_borrower(&mut svm, &cma, &coll, 10, usd(400));
    warp_slots(&mut svm, oracle_staleness() + 1);
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown");
    // A second shutdown reverts (already terminal).
    svm.expire_blockhash();
    let f = send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_MARKET_SHUTDOWN);
}

// ============================ post-shutdown gating ============================

#[test]
fn borrow_and_ordered_redeem_blocked_after_shutdown() {
    let (mut svm, gov, cma, coll) = setup();
    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(600));
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(60))], &gov, &[])
        .expect("drop price");
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown");

    // Borrow is closed.
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(1))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_MARKET_SHUTDOWN);

    // Ordered redeem is closed (urgent_redeem replaces it).
    let f = send(
        &mut svm,
        &[redeem_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, &a.coll_ata, &[a.position], usd(50))],
        &a.kp,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_MARKET_SHUTDOWN);
}

// ============================ urgent_redeem wind-down ============================

#[test]
fn urgent_redeem_rejected_when_not_shutdown() {
    let (mut svm, _gov, cma, coll) = setup();
    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(400));
    let f = send(
        &mut svm,
        &[urgent_redeem_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, &a.coll_ata, &[a.position], usd(50))],
        &a.kp,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_MARKET_NOT_SHUTDOWN);
}

#[test]
fn urgent_redeem_winds_down_at_zero_fee_and_last_price() {
    // Use a market WITH a redemption fee configured, to prove urgent_redeem ignores it (0-fee), and
    // shutdown via oracle failure to prove urgent_redeem needs no fresh price (last-price wind-down).
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market_with_fee(&mut svm, &gov, &cma, REDEMPTION_FEE_BPS); // 50 bps fee
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price");

    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(600)); // target
    let b = open_borrower(&mut svm, &cma, &coll, 20, usd(600)); // redeemer (holds $600 fUSD)

    // Oracle dies, then shutdown — spot stays at the last $100.
    warp_slots(&mut svm, oracle_staleness() + 1);
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown");

    let a_art_before = read_position(&svm, &a.position).recorded_debt;
    let b_coll_before = token_balance(&svm, &b.coll_ata);

    // B urgent-redeems $300 against A at the last price ($100) — no staleness gate.
    send(
        &mut svm,
        &[urgent_redeem_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, &b.coll_ata, &[a.position], usd(300))],
        &b.kp,
        &[],
    )
    .expect("urgent_redeem at last price");

    // B paid $300 fUSD and received the FULL 3 tokens (0 fee, despite the 50 bps config).
    assert_eq!(token_balance(&svm, &b.fusd_ata), usd(300));
    assert_eq!(token_balance(&svm, &b.coll_ata), b_coll_before + whole_coll(3));
    // No fee skimmed to surplus, and A's debt + collateral dropped.
    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.surplus_collateral, 0, "urgent_redeem is 0-fee");
    assert!(read_position(&svm, &a.position).recorded_debt < a_art_before, "A's debt reduced");
    // The headline accounting invariant holds exactly: vault == total_collateral + surplus.
    assert_eq!(
        token_balance(&svm, &coll_vault_pda(&coll)),
        m.total_collateral as u64 + m.surplus_collateral,
        "vault == total_collateral + surplus after urgent_redeem"
    );
}

#[test]
fn urgent_redeem_underwater_caps_at_collateral_value_no_overdraw() {
    // Graceful degradation from a genuinely UNDERWATER position (debt > ink·price). urgent_redeem must
    // pay the redeemer only the position's ACTUAL collateral (capped at coll_value), never face value
    // for the full debt — so a redeemer cannot over-draw the vault, the unbacked residual is left
    // orphaned on the drained position (contained), and the 4-term vault invariant still holds exactly.
    // (Djed models this via P_sc = min(r, L/N_sc); fusion-core's per-position `min(debt, coll_value)`
    // cap is the CDP analog — no test exercised the binding-on-coll_value case before.)
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("price $100");

    // A: 10 coll @ $100 = $1000 value, $600 debt (CR 167%, healthy to open) — the redemption target.
    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(600));
    // B: the redeemer, well-collateralized, holding $600 fUSD.
    let b = open_borrower(&mut svm, &cma, &coll, 40, usd(600));

    // Price collapses to $50: A is now UNDERWATER — 10 coll · $50 = $500 value < $600 debt.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(50))], &gov, &[]).expect("price $50");
    // Oracle then dies + shutdown → urgent_redeem winds down at the last ($50) price, no staleness gate.
    warp_slots(&mut svm, oracle_staleness() + 1);
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown");

    let b_coll_before = token_balance(&svm, &b.coll_ata);

    // B urgent-redeems $600 against the underwater A. redeem_amt = min($600 want, $600 debt, $500
    // coll_value) = $500 — BINDING on coll_value. B receives A's FULL collateral (10 tokens, worth $500
    // at $50), NOT $600 worth (12 tokens A does not have), and burns only the $500 it could redeem.
    send(
        &mut svm,
        &[urgent_redeem_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, &b.coll_ata, &[a.position], usd(600))],
        &b.kp,
        &[],
    )
    .expect("urgent_redeem underwater");

    // No over-draw: B got exactly A's collateral (10 tokens), and burned only the redeemable $500.
    assert_eq!(
        token_balance(&svm, &b.coll_ata),
        b_coll_before + whole_coll(10),
        "B receives A's actual collateral, not face value for the full debt",
    );
    assert_eq!(
        token_balance(&svm, &b.fusd_ata),
        usd(600) - usd(500),
        "B burned only the $500 it could redeem against A's collateral, not the full $600",
    );

    // A is drained to ink 0 with the unbacked residual $100 debt left orphaned (contained, never paid out).
    let pa = read_position(&svm, &a.position);
    assert_eq!(pa.ink, 0, "A's collateral is fully drawn");
    assert_eq!(pa.recorded_debt, usd(100) as u128, "residual $100 debt orphaned on the drained position");

    // The vault invariant still holds exactly — the redeemer could not over-draw the vault.
    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.surplus_collateral, 0, "urgent_redeem is 0-fee");
    assert_eq!(
        token_balance(&svm, &coll_vault_pda(&coll)),
        m.total_collateral as u64 + m.surplus_collateral,
        "vault == total_collateral + surplus after an underwater urgent_redeem",
    );
}

#[test]
fn urgent_redeem_is_unordered() {
    // Two positions in DIFFERENT rate buckets: A @ 5% (lower bucket), B @ 6% (higher bucket).
    // urgent_redeem can target B directly — which ordered `redeem` could never reach before A.
    let (mut svm, gov, cma, coll) = setup();
    let a = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 500);
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 600);
    assert_ne!(bucket_of(500), bucket_of(600), "A and B must be in different buckets");

    warp_slots(&mut svm, oracle_staleness() + 1);
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown");

    let a_art_before = read_position(&svm, &a.position).recorded_debt;
    // A (holding fUSD) redeems against the HIGHER-bucket B — impossible under ordered redemption.
    send(
        &mut svm,
        &[urgent_redeem_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, &a.coll_ata, &[b.position], usd(100))],
        &a.kp,
        &[],
    )
    .expect("urgent_redeem targets the higher bucket");

    assert!(read_position(&svm, &b.position).recorded_debt < a_art_before, "B (higher bucket) was redeemed");
    assert_eq!(read_position(&svm, &a.position).recorded_debt, a_art_before, "A (lower bucket) untouched");
}

#[test]
fn urgent_redeem_winds_down_a_debt_free_recipient_of_parked_redistribution() {
    // Regression: a position that is debt-free IN STORAGE (never borrowed ⇒ bucket 0, not a
    // bitmap member) but holds STAKE can acquire debt only via `realize` folding in parked tier-2
    // redistribution. `urgent_redeem` must reconcile bucket membership off the pre-realize debt — NOT
    // blindly `leave` — or it underflows `counts[bucket 0]` (which is always 0) and reverts the whole
    // wind-down. Pre-fix this reverted with MathOverflow; post-fix it succeeds.
    let (mut svm, gov, cma, coll) = setup();

    // Debt-free recipient R: 100 tokens of collateral, NO borrow ⇒ recorded_debt 0, bucket 0, stake>0.
    let r = open_borrower_rate(&mut svm, &cma, &coll, 100, 0, 500);
    assert_eq!(read_position(&svm, &r.position).recorded_debt, 0);
    assert_eq!(read_position(&svm, &r.position).bucket, 0);

    // Victim V borrows $600 (and so holds $600 fUSD to redeem with). The RP is empty.
    let v = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    // Crash to $80 ⇒ V is under-MCR; liquidate with an empty RP ⇒ V's whole debt + collateral
    // redistribute to R (the sole stakeholder), parked OUT of R's recorded_debt until R is touched.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("crash");
    liquidate(&mut svm, &gov, &coll, &v.position).expect("liquidate V into redistribution");
    assert_eq!(read_position(&svm, &r.position).recorded_debt, 0, "R's debt still parked (unrealized)");

    // Oracle goes dark ⇒ shutdown; then V urgent-redeems R, whose `realize` folds in the parked debt
    // (0→+) and full redemption takes it back to 0. The bucket reconcile must not underflow.
    warp_slots(&mut svm, oracle_staleness() + 1);
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown on oracle failure");

    send(
        &mut svm,
        &[urgent_redeem_ix(&v.kp.pubkey(), &coll, &v.fusd_ata, &v.coll_ata, &[r.position], usd(600))],
        &v.kp,
        &[],
    )
    .expect("urgent_redeem winds down the debt-free redistribution recipient (no bucket underflow)");

    assert_eq!(read_position(&svm, &r.position).recorded_debt, 0, "R fully wound down");
}

// ============================ the open side stays open ============================

#[test]
fn repay_deposit_withdraw_stay_open_after_shutdown() {
    // The load-bearing other half of the circuit-breaker contract: fund-returning ops MUST keep
    // working during a wind-down (a regression that gated repay/withdraw would freeze user funds).
    let (mut svm, gov, cma, coll) = setup();
    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(500)); // has debt
    let c = open_borrower_rate(&mut svm, &cma, &coll, 5, 0, 500); // debt-free (deposited, no borrow)

    // Shut down via oracle failure (price stays $100 ⇒ positions stay healthy for the open-side ops).
    warp_slots(&mut svm, oracle_staleness() + 1);
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown");

    // Repay reduces debt.
    let a_art_before = read_position(&svm, &a.position).recorded_debt;
    send(&mut svm, &[repay_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(100))], &a.kp, &[])
        .expect("repay stays open");
    assert!(read_position(&svm, &a.position).recorded_debt < a_art_before);

    // Deposit increases collateral.
    let tc_before = read_market(&svm, &market_pda(&coll)).total_collateral;
    fund_and_deposit(&mut svm, &cma, &coll, &c, whole_coll(2));
    assert!(read_market(&svm, &market_pda(&coll)).total_collateral > tc_before);

    // Debt-free withdraw succeeds (no MCR gate when art == 0).
    let c_coll_before = token_balance(&svm, &c.coll_ata);
    send(&mut svm, &[withdraw_ix(&c.kp.pubkey(), &coll, &c.coll_ata, whole_coll(1))], &c.kp, &[])
        .expect("debt-free withdraw stays open");
    assert_eq!(token_balance(&svm, &c.coll_ata), c_coll_before + whole_coll(1));

    // Borrow is the only thing closed.
    let f = send(&mut svm, &[borrow_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, usd(1))], &a.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_MARKET_SHUTDOWN);
}

#[test]
fn liquidation_stays_open_after_shutdown() {
    let (mut svm, gov, cma, coll) = setup();
    let a = open_borrower(&mut svm, &cma, &coll, 10, usd(500)); // liquidation target
    let d = open_borrower(&mut svm, &cma, &coll, 30, usd(1000));
    provide_sp(&mut svm, &d, &coll, usd(1000));

    // Oracle fails → shutdown; then the oracle "recovers" to a crashed $60 (fresh again), under
    // which A (10 tok @ $60 = $600 vs $500 debt, CR 120% < 150% MCR) is liquidatable but not underwater.
    warp_slots(&mut svm, oracle_staleness() + 1);
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(60))], &gov, &[])
        .expect("reprice");

    let liq = Keypair::new();
    airdrop_sol(&mut svm, &liq.pubkey(), 100);
    // The long oracle outage that triggered shutdown also armed the on-resume grace window, so the
    // instant post-resume liquidation is held off (grace gates `liquidate` uniformly — even in
    // shutdown, where `urgent_redeem` is the ungated wind-down path). Once the window elapses,
    // liquidation proceeds: the point is that SHUTDOWN itself never closes liquidation.
    let g = liquidate(&mut svm, &liq, &coll, &a.position).expect_err("grace holds off the instant liquidation");
    assert_eq!(custom_code(&g), E_LIQUIDATION_GRACE_PERIOD);
    crank_past_resume_grace(&mut svm, &gov, &coll, spot_for_usd(60));
    liquidate(&mut svm, &liq, &coll, &a.position).expect("liquidation stays open during shutdown (past grace)");
    assert_eq!(read_position(&svm, &a.position).recorded_debt, 0, "A liquidated while shut down");
}

#[test]
fn shutdown_on_interest_driven_scr_breach() {
    // Exercises the accrue → TCR linkage: a position healthy at borrow time crosses below SCR purely
    // from accrued interest (not a price move), and shutdown's accrue() must fold that in.
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price");
    // $1000 collateral, $500 debt at 20%/yr ⇒ TCR 200% now.
    let _a = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(500), 2_000);

    // Healthy now (fresh price, TCR 200% > SCR 110%) ⇒ shutdown rejected.
    let f = send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_SHUTDOWN_CONDITION_NOT_MET);

    // Warp 5 years: shutdown's accrue() folds +100% interest ⇒ debt ~$1000 ⇒ TCR ~100% < SCR 110%.
    warp_unix(&mut svm, 5 * 31_536_000);
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[])
        .expect("interest pushed TCR below SCR");
    assert!(read_market(&svm, &market_pda(&coll)).shutdown);
}

/// Interest FREEZES at shutdown (BOLD): once the wind-down begins, no further interest accrues on the
/// aggregate OR per-position, even as wall-clock advances. Two distinct one-line regressions (dropping
/// `|| market.shutdown` in `accrue`, or the per-position period cap in `realize`) would over-charge
/// here. This is the only test that warps `unix_timestamp` AFTER shutdown.
#[test]
fn shutdown_freezes_interest() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price");
    // $1000 collateral, $500 debt at 20%/yr.
    let a = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(500), 2_000);

    // Warp 5 years -> +100% interest -> debt $1000, TCR 100% < SCR -> shutdown (its accrue folds the
    // interest into the aggregate and freezes the clock at THIS moment).
    warp_unix(&mut svm, 5 * 31_536_000);
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown");
    assert_eq!(read_market(&svm, &market).agg_recorded_debt, usd(1_000) as u128, "interest folded at shutdown");

    // Warp ANOTHER 5 years AFTER shutdown — the stored aggregate must NOT change (frozen clock).
    warp_unix(&mut svm, 5 * 31_536_000);
    assert_eq!(read_market(&svm, &market).agg_recorded_debt, usd(1_000) as u128, "aggregate frozen post-shutdown");

    // Touch A via urgent_redeem ($100). Its interest is charged ONLY up to the shutdown moment (+$500),
    // NOT the extra 5 post-shutdown years -> debt $1000, minus $100 redeemed = $900 (a broken freeze
    // would charge 10 years -> $1500 - $100 = $1400).
    send(
        &mut svm,
        &[urgent_redeem_ix(&a.kp.pubkey(), &coll, &a.fusd_ata, &a.coll_ata, &[a.position], usd(100))],
        &a.kp,
        &[],
    )
    .expect("urgent_redeem");
    let p = read_position(&svm, &a.position);
    assert_eq!(p.recorded_debt, usd(900) as u128, "interest frozen at shutdown: $1000 - $100, not $1400");
    let m = read_market(&svm, &market);
    assert_eq!(m.agg_recorded_debt, usd(900) as u128, "no post-shutdown interest in the aggregate");
    assert!(m.agg_recorded_debt >= p.recorded_debt, "Σ recorded_debt <= agg_recorded_debt holds");
    assert_weighted_sum(&svm, &coll, &[a.position]); // urgent_redeem post-shutdown reweight
    assert_supply_invariant(&svm, &coll);
}

// ============================ per-market isolation ============================

#[test]
fn shutdown_is_per_market() {
    // Shutting down X must leave Y fully operational.
    let (mut svm, gov, cma, coll_x) = setup();

    // Second market Y in the same protocol, priced at $100.
    let mint_y = Keypair::new();
    create_mint(&mut svm, &gov, &mint_y, COLL_DECIMALS, &cma.pubkey(), /*freeze=*/ false);
    let coll_y = mint_y.pubkey();
    send(
        &mut svm,
        &[init_market_ix(&gov.pubkey(), &coll_y, MCR_BPS, DEBT_CEILING, 0, 0, BUCKET_WIDTH_BPS, 0)],
        &gov,
        &[],
    )
    .expect("init market Y");
    send(&mut svm, &[init_reactor_pool_ix(&gov.pubkey(), &coll_y)], &gov, &[]).expect("init RP Y");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll_y, spot_for_usd(100))], &gov, &[])
        .expect("price Y");

    let ax = open_borrower(&mut svm, &cma, &coll_x, 10, usd(600));
    let ay = open_borrower_rate(&mut svm, &cma, &coll_y, 10, 0, 500); // open + deposit, no borrow

    // Crash + shut down X only (Y stays at $100).
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll_x, spot_for_usd(60))], &gov, &[])
        .expect("drop X");
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll_x)], &gov, &[]).expect("shutdown X");
    assert!(read_market(&svm, &market_pda(&coll_x)).shutdown);
    assert!(!read_market(&svm, &market_pda(&coll_y)).shutdown);

    // Borrow on X is closed; borrow on Y is unaffected.
    let f = send(&mut svm, &[borrow_ix(&ax.kp.pubkey(), &coll_x, &ax.fusd_ata, usd(1))], &ax.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_MARKET_SHUTDOWN);
    send(&mut svm, &[borrow_ix(&ay.kp.pubkey(), &coll_y, &ay.fusd_ata, usd(100))], &ay.kp, &[])
        .expect("borrow on the healthy market Y succeeds");
    assert_eq!(token_balance(&svm, &ay.fusd_ata), usd(100));
}
