//! Global Backstop Reserve — the account lifecycle + bounded governance surface.
//! Creation, permissionless funding, gov withdrawal of above-cap
//! excess, and the TIMELOCKED global-param flow (cut / caps / draw coefficients). The funding-via-
//! refresh split and the tier-3.5 liquidation draw are covered in their own suites.
//!
//! Requires the dev-oracle `.so`: `anchor build -- --features dev-oracle`.

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

fn actors() -> (litesvm::LiteSVM, Keypair, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    (svm, gov, Keypair::new())
}

/// Stand up: protocol + market + price + gate + an inited (inert) backstop, and return a funded
/// borrower (source of fUSD for the funding tests).
fn setup(svm: &mut litesvm::LiteSVM, gov: &Keypair, cma: &Keypair) -> (solana_sdk::pubkey::Pubkey, Actor) {
    let coll = bootstrap_market(svm, gov, cma);
    init_gov_gate(svm, gov);
    send(svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], gov, &[]).expect("price");
    send(svm, &[init_global_backstop_ix(&gov.pubkey())], gov, &[]).expect("init backstop");
    let whale = open_borrower(svm, cma, &coll, 1_000, usd(50_000)); // a deep fUSD source
    (coll, whale)
}

#[test]
fn init_creates_inert_reserve() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    init_gov_gate(&mut svm, &gov);

    // Non-gov cannot create it.
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let f = send(&mut svm, &[init_global_backstop_ix(&rando.pubkey())], &rando, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED, "non-gov init rejected");

    send(&mut svm, &[init_global_backstop_ix(&gov.pubkey())], &gov, &[]).expect("init backstop");
    let b = read_backstop(&svm);
    assert_eq!(b.cut_bps, 0, "ships inert");
    assert_eq!(b.reserve_cap, 0);
    assert_eq!(b.draw_base_allowance, 0);
    assert_eq!(b.draw_k_bps, 0);
    assert_eq!(b.draw_ceiling_share_bps, 0);
    assert_eq!(b.draw_debt_share_bps, 0);
    assert_eq!(b.total_contributed, 0);
    assert_eq!(backstop_balance(&svm), 0);
    let _ = coll;
}

#[test]
fn fund_then_withdraw_above_cap_excess() {
    let (mut svm, gov, cma) = actors();
    let (_coll, whale) = setup(&mut svm, &gov, &cma);

    // Permissionless top-up.
    send(&mut svm, &[fund_backstop_ix(&whale.kp.pubkey(), &whale.fusd_ata, usd(1_000))], &whale.kp, &[])
        .expect("fund");
    assert_eq!(backstop_balance(&svm), usd(1_000));
    assert_eq!(read_backstop(&svm).total_contributed, usd(1_000) as u128);

    // Set the reserve cap to 600 (timelocked param, gate timelock 0 ⇒ immediate).
    gov_set_global_param(&mut svm, &gov, GlobalParam::ReserveCap, usd(600));
    assert_eq!(read_backstop(&svm).reserve_cap, usd(600));

    // Only the above-cap excess (1000 − 600 = 400) is withdrawable.
    let recip = whale.fusd_ata;
    let before = token_balance(&svm, &recip);
    let f = send(&mut svm, &[withdraw_backstop_excess_ix(&gov.pubkey(), &recip, usd(401))], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_INSUFFICIENT_BACKSTOP_EXCESS, "can't dip below the cap");

    send(&mut svm, &[withdraw_backstop_excess_ix(&gov.pubkey(), &recip, usd(400))], &gov, &[])
        .expect("withdraw the excess");
    assert_eq!(backstop_balance(&svm), usd(600), "reserve floored at the cap");
    assert_eq!(token_balance(&svm, &recip), before + usd(400));
    assert_eq!(read_backstop(&svm).total_withdrawn, usd(400) as u128);

    // Now at the cap: nothing left to withdraw.
    let f = send(&mut svm, &[withdraw_backstop_excess_ix(&gov.pubkey(), &recip, usd(1))], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_INSUFFICIENT_BACKSTOP_EXCESS);

    // Non-gov cannot withdraw.
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let f = send(&mut svm, &[withdraw_backstop_excess_ix(&rando.pubkey(), &recip, usd(1))], &rando, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);
}

#[test]
fn timelocked_global_param_set_clamp_and_auth() {
    let (mut svm, gov, cma) = actors();
    let (_coll, _whale) = setup(&mut svm, &gov, &cma);

    // Queue + execute a cut change (gate timelock 0 ⇒ same instant).
    gov_set_global_param(&mut svm, &gov, GlobalParam::Cut, 1_500);
    assert_eq!(read_backstop(&svm).cut_bps, 1_500);

    // Clamp: a cut above MAX_BACKSTOP_CUT_BPS (3000) is rejected at queue.
    let nonce = read_gov_gate(&svm).queue_nonce;
    let f = send(&mut svm, &[queue_global_param_ix(&gov.pubkey(), nonce, GlobalParam::Cut, 3_001)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS, "cut clamp");

    // Auth: a non-inbound-authority cannot queue.
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let nonce = read_gov_gate(&svm).queue_nonce;
    let f = send(&mut svm, &[queue_global_param_ix(&rando.pubkey(), nonce, GlobalParam::DrawK, 1)], &rando, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);

    // The draw-cap coefficients tune through the same flow.
    gov_set_global_param(&mut svm, &gov, GlobalParam::DrawCeilingShare, 2_500);
    gov_set_global_param(&mut svm, &gov, GlobalParam::DrawDebtShare, 2_000);
    gov_set_global_param(&mut svm, &gov, GlobalParam::DrawBase, usd(5_000));
    let b = read_backstop(&svm);
    assert_eq!(b.draw_ceiling_share_bps, 2_500);
    assert_eq!(b.draw_debt_share_bps, 2_000);
    assert_eq!(b.draw_base_allowance, usd(5_000));
}

#[test]
fn queued_global_param_can_be_canceled() {
    let (mut svm, gov, cma) = actors();
    let (_coll, _whale) = setup(&mut svm, &gov, &cma);

    // Queue (don't execute), then cancel — the param never applies.
    let nonce = read_gov_gate(&svm).queue_nonce;
    send(&mut svm, &[queue_global_param_ix(&gov.pubkey(), nonce, GlobalParam::Cut, 2_000)], &gov, &[])
        .expect("queue");
    send(&mut svm, &[cancel_global_param_ix(&gov.pubkey(), nonce)], &gov, &[]).expect("cancel");
    assert_eq!(read_backstop(&svm).cut_bps, 0, "canceled change never applied");

    // The op account is closed — executing it now fails.
    let f = send(&mut svm, &[execute_global_param_ix(&gov.pubkey(), nonce)], &gov, &[]).unwrap_err();
    // (AccountNotInitialized / similar — just assert it did not succeed.)
    let _ = f;
}

const YEAR: i64 = 31_536_000;

/// Stand up a market with the backstop inited + funded by the interest cut: returns the collateral and
/// the buffer-vault balance reader is via `token_balance(&svm, &buffer_fusd_vault_pda(&coll))`.
fn setup_funding(svm: &mut litesvm::LiteSVM, gov: &Keypair, cma: &Keypair, cut_bps: u64, cap: u64) -> solana_sdk::pubkey::Pubkey {
    let coll = bootstrap_market(svm, gov, cma);
    init_gov_gate(svm, gov);
    send(svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], gov, &[]).expect("price");
    send(svm, &[init_global_backstop_ix(&gov.pubkey())], gov, &[]).expect("init backstop");
    gov_set_global_param(svm, gov, GlobalParam::ReserveCap, cap);
    gov_set_global_param(svm, gov, GlobalParam::Cut, cut_bps);
    // A borrower paying 20%/yr on $10k so a year of refresh has meaningful interest to split.
    open_borrower_rate(svm, cma, &coll, 1_000, usd(10_000), 2_000);
    coll
}

#[test]
fn funding_split_routes_cut_to_backstop() {
    let (mut svm, gov, cma) = actors();
    let coll = setup_funding(&mut svm, &gov, &cma, /*cut=*/ 2_000, /*cap=*/ usd(10_000_000));

    warp_unix(&mut svm, YEAR);
    send(&mut svm, &[refresh_market_full_ix(&coll, None, /*with_backstop=*/ true)], &gov, &[])
        .expect("refresh with backstop");

    let to_backstop = backstop_balance(&svm);
    let to_buffer = token_balance(&svm, &buffer_fusd_vault_pda(&coll));
    assert!(to_backstop > 0, "the cut funded the reserve");
    assert!(to_buffer > to_backstop, "the local buffer keeps the majority");
    // ~20% of the minted interest went to the reserve (floor ⇒ allow 1 bps slack).
    let total = to_backstop + to_buffer;
    let ratio_bps = to_backstop * 10_000 / total;
    assert!((1_999..=2_000).contains(&ratio_bps), "cut ≈ 20% (got {ratio_bps} bps)");
    // The reserve-solvency + draw-cap counters track the cut exactly.
    assert_eq!(read_market(&svm, &market_pda(&coll)).global_contributed, to_backstop as u128);
    assert_eq!(read_backstop(&svm).total_contributed, to_backstop as u128);
}

#[test]
fn funding_cut_reverts_to_local_at_cap() {
    let (mut svm, gov, cma) = actors();
    // A tiny cap so the cut is capped and the excess reverts to the local buffer.
    let coll = setup_funding(&mut svm, &gov, &cma, /*cut=*/ 2_000, /*cap=*/ usd(100));

    warp_unix(&mut svm, YEAR);
    send(&mut svm, &[refresh_market_full_ix(&coll, None, true)], &gov, &[]).expect("refresh");

    // The reserve fills exactly to its cap; everything above the cap stayed local.
    assert_eq!(backstop_balance(&svm), usd(100), "reserve floored to its cap");
    assert_eq!(read_market(&svm, &market_pda(&coll)).global_contributed, usd(100) as u128);
    // A further refresh contributes nothing more (already at cap) — the cut fully reverts to local.
    warp_unix(&mut svm, YEAR);
    svm.expire_blockhash();
    send(&mut svm, &[refresh_market_full_ix(&coll, None, true)], &gov, &[]).expect("refresh again");
    assert_eq!(backstop_balance(&svm), usd(100), "stays at cap; cut reverts to local");
}

#[test]
fn funding_omitted_backstop_is_all_local() {
    // Without the backstop accounts (or with cut disabled), the whole interest funds the local buffer —
    // byte-identical to pre-backstop behavior.
    let (mut svm, gov, cma) = actors();
    let coll = setup_funding(&mut svm, &gov, &cma, /*cut=*/ 2_000, /*cap=*/ usd(10_000_000));

    warp_unix(&mut svm, YEAR);
    // with_backstop = false ⇒ no cut routed even though cut_bps is set.
    send(&mut svm, &[refresh_market_full_ix(&coll, None, false)], &gov, &[]).expect("refresh local-only");
    assert_eq!(backstop_balance(&svm), 0, "omitted backstop ⇒ nothing routed");
    assert!(token_balance(&svm, &buffer_fusd_vault_pda(&coll)) > 0, "all interest funded the local buffer");
    assert_eq!(read_market(&svm, &market_pda(&coll)).global_contributed, 0);
}
