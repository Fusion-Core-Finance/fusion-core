//! The oracle-pause invariant MATRIX — the exhaustive cross of oracle states
//! × operations, consolidating what was previously piecemeal per-flag coverage and closing the
//! verified gaps: ordered `redeem` under staleness had ZERO coverage, repay/pure-deposit were
//! tested under mint_frozen and shutdown but not under an aged-out cache, and the zero-debt-
//! withdraw NONE-tier property appeared only incidentally in the CCR suite.
//!
//! The locked matrix:
//! - FRESH_OK            → everything open (urgent_redeem closed: not shut down).
//! - FRESH + MINT_FROZEN (disagreement: a present-but-untrusted aggregate) → blocks NEW MINTS
//!   only; repay, deposit, withdraw (fresh price!), adjust_rate, liquidation, redemption all live.
//! - STALE (cache aged past MAX_PRICE_STALENESS_SLOTS) → price-consuming paths pause too:
//!   liquidate, ordered redeem, debt-bearing withdraw, borrow. Repay, pure deposit, zero-debt
//!   withdraw, adjust_rate, RP ops, claim_coll_surplus stay open.
//! - SHUTDOWN (+ stale)  → borrow + ordered redeem closed (MarketShutdown); urgent_redeem is the
//!   wind-down floor and deliberately has NO staleness gate (last price); repay/deposit/zero-debt
//!   withdraw stay open.
//! Redemption ORDERING stays oracle-independent throughout (bucket key = borrower user_rate).
//!
//! The FROZEN rows are driven by the REAL `update_price` crank with a fresh-but-divergent
//! Switchboard (so the freshness clock advances while mints freeze) — never by staleness.
//! Requires the dev-oracle `.so`.

use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const SQRT_100: u128 = 5_833_372_668_713_516_046;
const PYTH_EXPO: i32 = -8;
const PYTH_PRICE_100: i64 = 100 * 100_000_000;
const PYTH_PRICE_85: i64 = 85 * 100_000_000;
const SB_VALUE_100: i128 = 100 * 1_000_000_000_000_000_000;
const SB_VALUE_120: i128 = 120 * 1_000_000_000_000_000_000;

fn actors() -> (litesvm::LiteSVM, Keypair, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    (svm, gov, Keypair::new())
}

/// Real-crank bootstrap to a FRESH_OK market at $100 (mirrors the oracle-crank suite).
fn reach_fresh(svm: &mut litesvm::LiteSVM, gov: &Keypair, cma: &Keypair) -> (Pubkey, OracleHandles) {
    let coll = bootstrap_market(svm, gov, cma);
    let h = bootstrap_oracle(svm, gov, &coll, 300, 3, 300, false);
    set_whirlpool_pool(svm, &h.orca_pool, SQRT_100, &coll, &h.quote);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 1");
    warp_unix(svm, 150);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 2");
    warp_unix(svm, 150);
    send(svm, &[sample_twap_ix(&gov.pubkey(), &coll, &h.orca_pool)], gov, &[]).expect("sample 3");
    let now = now_unix(svm);
    set_pyth_price(svm, &h.pyth, h.feed_id, PYTH_PRICE_100, 0, PYTH_EXPO, now);
    set_switchboard_feed(svm, &h.sb, SB_VALUE_100, 0, 1, now);
    send(svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], gov, &[])
        .expect("fresh-OK crank");
    let m = read_market(svm, &market_pda(&coll));
    assert!(!m.mint_frozen && m.spot == spot_for_usd(100));
    (coll, h)
}

#[test]
fn fresh_ok_row_everything_open() {
    let (mut svm, gov, cma) = actors();
    let (coll, _h) = reach_fresh(&mut svm, &gov, &cma);

    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(400)); // borrow ✓ (the mint path itself)
    let z = open_borrower(&mut svm, &cma, &coll, 5, 0);
    let funder = open_borrower(&mut svm, &cma, &coll, 100, usd(2_000));
    provide_sp(&mut svm, &funder, &coll, usd(1_500)); // provide_to_reactor ✓

    send(&mut svm, &[repay_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(50))], &b.kp, &[])
        .expect("repay");
    fund_and_deposit(&mut svm, &cma, &coll, &z, whole_coll(1)); // pure deposit ✓
    send(&mut svm, &[withdraw_ix(&b.kp.pubkey(), &coll, &b.coll_ata, whole_coll(1))], &b.kp, &[])
        .expect("debt-bearing withdraw (fresh)");
    send(&mut svm, &[withdraw_ix(&z.kp.pubkey(), &coll, &z.coll_ata, whole_coll(1))], &z.kp, &[])
        .expect("zero-debt withdraw");
    send(&mut svm, &[adjust_rate_ix(&b.kp.pubkey(), &coll, 600)], &b.kp, &[]).expect("adjust_rate");
    send(
        &mut svm,
        &[withdraw_from_reactor_ix(&funder.kp.pubkey(), &coll, &funder.fusd_ata, usd(100))],
        &funder.kp,
        &[],
    )
    .expect("withdraw_from_reactor");
    send(
        &mut svm,
        &[redeem_ix(&funder.kp.pubkey(), &coll, &funder.fusd_ata, &funder.coll_ata,
            &[b.position, funder.position], usd(50))],
        &funder.kp,
        &[],
    )
    .expect("ordered redeem");

    // urgent_redeem is closed on a LIVE market — its only gate is shutdown.
    let f = send(
        &mut svm,
        &[urgent_redeem_ix(&funder.kp.pubkey(), &coll, &funder.fusd_ata, &funder.coll_ata,
            &[b.position], usd(10))],
        &funder.kp,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_MARKET_NOT_SHUTDOWN);
}

#[test]
fn mint_frozen_row_blocks_only_borrow() {
    let (mut svm, gov, cma) = actors();
    let (coll, h) = reach_fresh(&mut svm, &gov, &cma);

    // Stand the actors up while FRESH_OK.
    let victim = open_borrower(&mut svm, &cma, &coll, 10, usd(645)); // CR ~155% at $100
    let z = open_borrower(&mut svm, &cma, &coll, 5, 0);
    let funder = open_borrower(&mut svm, &cma, &coll, 100, usd(2_000));
    provide_sp(&mut svm, &funder, &coll, usd(1_500));
    let redeemer = open_borrower(&mut svm, &cma, &coll, 50, usd(500));

    // FROZEN via the REAL crank: Pyth fresh at $85, Switchboard fresh but divergent at $120.
    // The freshness clock advances (fresh primary) while disagreement freezes mints — the
    // disagreement state, never staleness.
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, PYTH_PRICE_85, 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, SB_VALUE_120, 0, 1, now);
    svm.expire_blockhash();
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("divergent crank");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen, "disagreement freezes mints");
    assert_eq!(m.spot, spot_for_usd(85), "fresh conservative spot committed");
    assert_eq!(m.liq_grace_until, 0, "no grace armed (no stall happened)");

    // The ONLY blocked op: new mints.
    let f = send(&mut svm, &[borrow_ix(&z.kp.pubkey(), &coll, &z.fusd_ata, usd(10))], &z.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_MINT_FROZEN);

    // Everything else lives, ON the fresh conservative price.
    send(&mut svm, &[repay_ix(&victim.kp.pubkey(), &coll, &victim.fusd_ata, usd(50))], &victim.kp, &[])
        .expect("repay under FROZEN");
    fund_and_deposit(&mut svm, &cma, &coll, &z, whole_coll(1));
    send(&mut svm, &[withdraw_ix(&redeemer.kp.pubkey(), &coll, &redeemer.coll_ata, whole_coll(1))], &redeemer.kp, &[])
        .expect("debt-bearing withdraw under FROZEN (price is FRESH — only mints freeze)");
    send(&mut svm, &[adjust_rate_ix(&redeemer.kp.pubkey(), &coll, 600)], &redeemer.kp, &[])
        .expect("adjust_rate under FROZEN");
    send(&mut svm, &[provide_to_reactor_ix(&funder.kp.pubkey(), &coll, &funder.fusd_ata, usd(100))], &funder.kp, &[])
        .expect("provide_to_reactor under FROZEN");

    // Liquidation survives the degraded secondary (victim CR ≈ 132% < 150% at the $85 spot;
    // no armed grace — the assertion isolates the oracle gate).
    let liquidator = Keypair::new();
    airdrop_sol(&mut svm, &liquidator.pubkey(), 10);
    liquidate(&mut svm, &liquidator, &coll, &victim.position).expect("liquidate under FROZEN");

    // Ordered redemption stays live: ordering is rate-bucket (oracle-independent),
    // payout uses the fresh conservative spot.
    send(
        &mut svm,
        &[redeem_ix(&redeemer.kp.pubkey(), &coll, &redeemer.fusd_ata, &redeemer.coll_ata,
            &[funder.position, redeemer.position], usd(50))],
        &redeemer.kp,
        &[],
    )
    .expect("redeem under FROZEN");
}

#[test]
fn stale_row_pauses_price_consuming_paths_only() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    init_gov_gate(&mut svm, &gov);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price");
    gov_set_param(&mut svm, &gov, &coll, MarketParam::LiqBonus, 1_000); // collar → claimable surplus

    let victim1 = open_borrower(&mut svm, &cma, &coll, 10, usd(645));
    let victim2 = open_borrower(&mut svm, &cma, &coll, 10, usd(645));
    let z = open_borrower(&mut svm, &cma, &coll, 5, 0);
    let funder = open_borrower(&mut svm, &cma, &coll, 100, usd(2_000));
    provide_sp(&mut svm, &funder, &coll, usd(1_500));
    let redeemer = open_borrower(&mut svm, &cma, &coll, 50, usd(500));

    // While FRESH at $85: liquidate victim1 under the collar so a surplus is claimable later.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(85))], &gov, &[])
        .expect("drop");
    let liquidator = Keypair::new();
    airdrop_sol(&mut svm, &liquidator.pubkey(), 10);
    liquidate(&mut svm, &liquidator, &coll, &victim1.position).expect("fresh collared liq");
    assert!(read_position(&svm, &victim1.position).coll_surplus > 0);

    // Age the cache out: the STALE state (no recrank).
    warp_slots(&mut svm, fusd_core::constants::MAX_PRICE_STALENESS_SLOTS + 1);

    // --- PAUSED: every price-consuming path, each on the staleness gate specifically. ---
    let f = send(&mut svm, &[borrow_ix(&z.kp.pubkey(), &coll, &z.fusd_ata, usd(10))], &z.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_STALE_PRICE, "borrow");
    let f = liquidate(&mut svm, &liquidator, &coll, &victim2.position).unwrap_err();
    assert_eq!(custom_code(&f), E_STALE_PRICE, "liquidate");
    let f = send(
        &mut svm,
        &[redeem_ix(&redeemer.kp.pubkey(), &coll, &redeemer.fusd_ata, &redeemer.coll_ata,
            &[victim2.position], usd(50))],
        &redeemer.kp,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_STALE_PRICE, "ordered redeem — previously ZERO coverage");
    let f = send(
        &mut svm,
        &[withdraw_ix(&redeemer.kp.pubkey(), &coll, &redeemer.coll_ata, whole_coll(1))],
        &redeemer.kp,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_STALE_PRICE, "debt-bearing withdraw");

    // --- OPEN: everything that needs no price, in the SAME staleness state. ---
    send(&mut svm, &[repay_ix(&victim2.kp.pubkey(), &coll, &victim2.fusd_ata, usd(50))], &victim2.kp, &[])
        .expect("repay under STALE (live market — not the shutdown variant)");
    fund_and_deposit(&mut svm, &cma, &coll, &z, whole_coll(1)); // pure deposit
    send(&mut svm, &[withdraw_ix(&z.kp.pubkey(), &coll, &z.coll_ata, whole_coll(1))], &z.kp, &[])
        .expect("zero-debt withdraw under STALE (the NONE-tier row, first-class)");
    send(&mut svm, &[adjust_rate_ix(&redeemer.kp.pubkey(), &coll, 600)], &redeemer.kp, &[])
        .expect("adjust_rate under STALE (NONE tier — no oracle gate by design)");
    send(&mut svm, &[provide_to_reactor_ix(&funder.kp.pubkey(), &coll, &funder.fusd_ata, usd(100))], &funder.kp, &[])
        .expect("provide_to_reactor under STALE");
    send(&mut svm, &[withdraw_from_reactor_ix(&funder.kp.pubkey(), &coll, &funder.fusd_ata, usd(100))], &funder.kp, &[])
        .expect("withdraw_from_reactor under STALE");
    send(&mut svm, &[claim_coll_surplus_ix(&victim1.kp.pubkey(), &coll, &victim1.coll_ata)], &victim1.kp, &[])
        .expect("claim_coll_surplus under STALE");

    // --- RECOVERY: a fresh recrank arms the on-resume grace (stall → resume); liquidation
    // waits out the grace, then the static rule resumes. The grace is honored, never weakened.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(85))], &gov, &[])
        .expect("resume crank");
    svm.expire_blockhash(); // the stale-state liquidate attempt above is otherwise byte-identical
    let f = liquidate(&mut svm, &liquidator, &coll, &victim2.position).unwrap_err();
    assert_eq!(custom_code(&f), E_LIQUIDATION_GRACE_PERIOD, "on-resume grace armed");
    crank_past_resume_grace(&mut svm, &gov, &coll, spot_for_usd(85));
    liquidate(&mut svm, &liquidator, &coll, &victim2.position)
        .expect("liquidate after the grace expires");
}

#[test]
fn shutdown_row_urgent_redeem_has_no_staleness_gate() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price");

    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(400));
    let z = open_borrower(&mut svm, &cma, &coll, 5, 0);
    let redeemer = open_borrower(&mut svm, &cma, &coll, 50, usd(500));

    // Crash to breach SCR (65 tokens · $15 = $975 vs $900 debt → TCR ≈ 108% < 110%), shut down.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(15))], &gov, &[])
        .expect("crash");
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown");

    // Age the cache out ON TOP of shutdown: the wind-down must proceed on a dead oracle.
    warp_slots(&mut svm, fusd_core::constants::MAX_PRICE_STALENESS_SLOTS + 1);

    // urgent_redeem deliberately has NO staleness gate (its gates: shutdown == true, spot > 0).
    send(
        &mut svm,
        &[urgent_redeem_ix(&redeemer.kp.pubkey(), &coll, &redeemer.fusd_ata, &redeemer.coll_ata,
            &[b.position], usd(50))],
        &redeemer.kp,
        &[],
    )
    .expect("urgent_redeem on a dead oracle — the wind-down floor");

    // Open side stays open under shutdown + stale.
    send(&mut svm, &[repay_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(50))], &b.kp, &[])
        .expect("repay");
    fund_and_deposit(&mut svm, &cma, &coll, &z, whole_coll(1));
    send(&mut svm, &[withdraw_ix(&z.kp.pubkey(), &coll, &z.coll_ata, whole_coll(1))], &z.kp, &[])
        .expect("zero-debt withdraw");

    // Closed side: borrow + ordered redeem are MarketShutdown (the terminal gate outranks
    // staleness); debt-bearing withdraw hits the staleness gate (withdraw has no shutdown gate).
    let f = send(&mut svm, &[borrow_ix(&z.kp.pubkey(), &coll, &z.fusd_ata, usd(10))], &z.kp, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_MARKET_SHUTDOWN, "borrow");
    let f = send(
        &mut svm,
        &[redeem_ix(&redeemer.kp.pubkey(), &coll, &redeemer.fusd_ata, &redeemer.coll_ata,
            &[b.position], usd(10))],
        &redeemer.kp,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_MARKET_SHUTDOWN, "ordered redeem");
    let f = send(
        &mut svm,
        &[withdraw_ix(&redeemer.kp.pubkey(), &coll, &redeemer.coll_ata, whole_coll(1))],
        &redeemer.kp,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_STALE_PRICE, "debt-bearing withdraw under shutdown+stale");
}
