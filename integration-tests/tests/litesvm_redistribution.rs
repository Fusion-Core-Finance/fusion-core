//! In-process litesvm integration test for liquidation **tier-2 redistribution**
//! (fusion-docs): when the Reactor Pool can't fully absorb a liquidation, the uncovered debt + collateral
//! redistribute to the remaining positions via the market `l_coll`/`l_art` accumulators, applied
//! lazily when a recipient is next touched. Covers a partial-RP split, a pure (empty-RP)
//! redistribution, and a pro-rata split across two recipients — with exact balances and the
//! `total_collateral == collateral_vault` invariant.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_redistribution

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

/// RP covers part of the debt; the remainder redistributes to a single other position.
#[test]
fn partial_pool_offset_then_redistribute_remainder() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let market = market_pda(&coll);
    let coll_vault = coll_vault_pda(&coll);
    let reactor_fusd_vault = reactor_fusd_vault_pda(&coll);
    let reactor_coll_vault = reactor_coll_vault_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // Recipient C: huge collateral, borrows $400 and provides it ALL to the RP (so the pool covers
    // only part of B's $600 debt). C is both the RP depositor and the redistribution recipient.
    let c = open_borrower(&mut svm, &coll_mint_auth, &coll, 1_000, usd(400));
    provide_sp(&mut svm, &c, &coll, usd(400));

    // Borrower B: 10 tokens ($1000 @ $100), borrows $600.
    let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));

    // Pre-liquidation aggregates: agg debt $1000, collateral 1010 tokens, vault == total_collateral.
    assert_eq!(read_market(&svm, &market).agg_recorded_debt, usd(1_000) as u128);
    assert_eq!(read_market(&svm, &market).total_collateral, whole_coll(1_010) as u128);
    assert_eq!(token_balance(&svm, &coll_vault), whole_coll(1_010));

    // Drop to $80: B is under-MCR ($800 coll vs $600 debt -> max_debt $533).
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    liquidate(&mut svm, &gov, &coll, &b.position)
        .expect("liquidate failed");

    // RP offsets $400 (coll_sp = 10e9 * 400/600 = 6_666_666_666); redistributes $200 + the
    // remaining 3_333_333_334 collateral to C.
    let coll_sp = 6_666_666_666u64;
    let coll_r = whole_coll(10) - coll_sp; // 3_333_333_334
    let bp = read_position(&svm, &b.position);
    assert_eq!(bp.recorded_debt, 0);
    assert_eq!(bp.ink, 0);

    let m = read_market(&svm, &market);
    // Only the RP-offset debt ($400) is extinguished; the redistributed $200 stays owed (by C).
    assert_eq!(m.agg_recorded_debt, usd(600) as u128);
    // l_coll = coll_r * 1e18 / total_stakes(1000e9); l_art = 200e6 * 1e18 / 1000e9.
    assert_eq!(m.l_coll, coll_r as u128 * 1_000_000); // 3_333_333_334 * 1e6
    assert_eq!(m.l_art, usd(200) as u128 * 1_000_000); // 200e6 * 1e6
    // total_collateral == vault: 1010 tokens minus the 6_666_666_666 moved to the RP.
    let expected_total_coll = whole_coll(1_010) as u128 - coll_sp as u128;
    assert_eq!(m.total_collateral, expected_total_coll);
    assert_eq!(token_balance(&svm, &coll_vault) as u128, expected_total_coll);
    assert_eq!(m.total_stakes, whole_coll(1_000) as u128); // C's stake; B removed

    // RP side: $400 burned, the RP's collateral share landed in the RP vault.
    assert_eq!(token_balance(&svm, &reactor_fusd_vault), 0);
    assert_eq!(token_balance(&svm, &reactor_coll_vault), coll_sp);

    // Touch C (deposit 1 token) -> it lazily absorbs the redistributed collateral + debt.
    fund_and_deposit(&mut svm, &coll_mint_auth, &coll, &c, whole_coll(1));
    let cp = read_position(&svm, &c.position);
    assert_eq!(cp.ink, whole_coll(1_000) + coll_r + whole_coll(1), "C absorbed redistributed collateral");
    assert_eq!(cp.recorded_debt, usd(400) as u128 + usd(200) as u128, "C absorbed redistributed debt");
}

/// RP empty: the entire liquidation redistributes to a single other position.
#[test]
fn empty_pool_full_redistribution() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let market = market_pda(&coll);
    let coll_vault = coll_vault_pda(&coll);
    let reactor_coll_vault = reactor_coll_vault_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // Recipient C borrows $400 (the RP is left empty); borrower B borrows $600.
    let c = open_borrower(&mut svm, &coll_mint_auth, &coll, 1_000, usd(400));
    let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    liquidate(&mut svm, &gov, &coll, &b.position)
        .expect("liquidate failed");

    let bp = read_position(&svm, &b.position);
    assert_eq!((bp.recorded_debt, bp.ink), (0, 0));

    let m = read_market(&svm, &market);
    // Nothing extinguished (RP didn't offset) — all $600 stays owed, now by C.
    assert_eq!(m.agg_recorded_debt, usd(1_000) as u128);
    // All 10 tokens + $600 redistributed across total_stakes 1000e9.
    assert_eq!(m.l_coll, whole_coll(10) as u128 * 1_000_000); // 10e9 * 1e18 / 1000e9
    assert_eq!(m.l_art, usd(600) as u128 * 1_000_000);
    // No collateral left the market (nothing went to the RP); vault unchanged and == total_collateral.
    assert_eq!(m.total_collateral, whole_coll(1_010) as u128);
    assert_eq!(token_balance(&svm, &coll_vault), whole_coll(1_010));
    assert_eq!(token_balance(&svm, &reactor_coll_vault), 0);
    assert_eq!(m.total_stakes, whole_coll(1_000) as u128);

    // Touch C: it absorbs all of B's collateral and debt.
    fund_and_deposit(&mut svm, &coll_mint_auth, &coll, &c, whole_coll(1));
    let cp = read_position(&svm, &c.position);
    assert_eq!(cp.ink, whole_coll(1_011)); // 1000 + 10 redistributed + 1 deposited
    assert_eq!(cp.recorded_debt, usd(1_000) as u128); // 400 own + 600 absorbed
}

/// Pure redistribution splits across two recipients pro-rata to their collateral stake (60/40),
/// debt included even though the recipients carried no debt of their own.
#[test]
fn redistribution_splits_pro_rata_across_two_recipients() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // C1 600 tokens, C2 400 tokens, no debt of their own (stakes 600/400). RP empty.
    let c1 = open_borrower(&mut svm, &coll_mint_auth, &coll, 600, 0);
    let c2 = open_borrower(&mut svm, &coll_mint_auth, &coll, 400, 0);
    // Borrower B: 10 tokens, $600 debt.
    let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    liquidate(&mut svm, &gov, &coll, &b.position)
        .expect("liquidate failed");

    let m = read_market(&svm, &market);
    assert_eq!(m.agg_recorded_debt, usd(600) as u128); // all of B's debt, now owed by C1+C2
    assert_eq!(m.total_stakes, whole_coll(1_000) as u128);

    // Touch both recipients; each absorbs its stake-weighted share of (10 tokens, $600).
    fund_and_deposit(&mut svm, &coll_mint_auth, &coll, &c1, whole_coll(1));
    fund_and_deposit(&mut svm, &coll_mint_auth, &coll, &c2, whole_coll(1));
    let p1 = read_position(&svm, &c1.position);
    let p2 = read_position(&svm, &c2.position);

    // C1 = 60%: +6 tokens, +$360 debt; C2 = 40%: +4 tokens, +$240 debt (zero dust — 1000 divides).
    assert_eq!(p1.ink, whole_coll(600) + whole_coll(6) + whole_coll(1));
    assert_eq!(p1.recorded_debt, usd(360) as u128);
    assert_eq!(p2.ink, whole_coll(400) + whole_coll(4) + whole_coll(1));
    assert_eq!(p2.recorded_debt, usd(240) as u128);
    // Conservation: the recipients' absorbed debt sums to B's debt.
    assert_eq!(p1.recorded_debt + p2.recorded_debt, usd(600) as u128);
}

/// Redistribution driven by PER-POSITION interest (the Liquity-v2 / BOLD model): a borrower accrues
/// interest at its own `user_rate_bps` until it crosses MCR, then the loss-absorption waterfall splits
/// the interest-grown debt RP → redistribution. Recorded debt is native, so the RP offsets exactly
/// `split.reactor` and redistributes exactly `split.redist` (no `art*rate` floor conversion); the
/// redistributed debt is parked until C realizes it, then carries C's own rate forward.
#[test]
fn redistribution_with_per_position_interest() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let market = market_pda(&coll);
    let coll_vault = coll_vault_pda(&coll);
    let reactor_fusd_vault = reactor_fusd_vault_pda(&coll);
    let reactor_coll_vault = reactor_coll_vault_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // C: huge collateral, borrows $500 at the MIN rate (50 bps) and provides it all to the RP.
    // B: 10 tokens ($1000 collateral, max_debt $666 at 150% MCR), borrows $400 at 20%/yr.
    let c = open_borrower_rate(&mut svm, &coll_mint_auth, &coll, 1_000, usd(500), 50);
    provide_sp(&mut svm, &c, &coll, usd(500));
    let b = open_borrower_rate(&mut svm, &coll_mint_auth, &coll, 10, usd(400), 2_000);
    // No time has passed: no interest yet, so recorded debt == borrowed.
    assert_eq!(read_market(&svm, &market).agg_recorded_debt, usd(900) as u128);

    // Warp 5 years. At 20%/yr simple interest that is exactly +100% on B ($400 -> $800 > $666, now
    // liquidatable); at 50 bps it is +2.5% on C ($500 -> $512.5).
    const FIVE_YEARS: i64 = 5 * 31_536_000;
    warp_unix(&mut svm, FIVE_YEARS);

    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate failed");

    let m = read_market(&svm, &market);
    let bp = read_position(&svm, &b.position);
    assert_eq!((bp.recorded_debt, bp.ink), (0, 0), "B fully liquidated by accrued interest");

    // B's realized debt is $800: RP offsets its $500 (extinguished exactly), $300 redistributes.
    // agg_recorded_debt = C's realized $512.5 + the parked redistributed $300 = $812.5.
    assert_eq!(m.agg_recorded_debt, 812_500_000, "agg = C (incl interest) + parked redist");
    let coll_sp = 6_250_000_000u64; // floor(10e9 * 500/800)
    let coll_r = whole_coll(10) - coll_sp; // 3_750_000_000
    assert_eq!(token_balance(&svm, &reactor_fusd_vault), 0, "RP burned its $500");
    assert_eq!(token_balance(&svm, &reactor_coll_vault), coll_sp);
    // Redistributed recorded debt $300 over total_stakes 1000e9 (PRECISION 1e18): per-unit = X·1e6.
    assert_eq!(m.l_art, usd(300) as u128 * 1_000_000);
    assert_eq!(m.l_coll, coll_r as u128 * 1_000_000);
    let expected_total_coll = whole_coll(1_010) as u128 - coll_sp as u128;
    assert_eq!(m.total_collateral, expected_total_coll);
    assert_eq!(token_balance(&svm, &coll_vault) as u128, expected_total_coll);
    // Weighted-sum crux: the victim's whole 2000-bps weight is stripped, and the parked $300 is
    // EXCLUDED from the weighted sum while parked. C is untouched, so its stored debt is still $500 ->
    // agg_weighted_debt_sum == 500e6·50. (Supply also holds: the RP burned its $500.)
    assert_weighted_sum(&svm, &coll, &[c.position]);
    assert_supply_invariant(&svm, &coll);

    // Touch C: it realizes its own interest ($12.5) AND absorbs the redistributed $300 of debt, which
    // then accrues at C's OWN rate going forward (BOLD lazy redistribution).
    fund_and_deposit(&mut svm, &coll_mint_auth, &coll, &c, whole_coll(1));
    let cp = read_position(&svm, &c.position);
    assert_eq!(cp.recorded_debt, 812_500_000, "C: $500 + $12.5 interest + $300 redistributed");
    assert_eq!(cp.ink, whole_coll(1_000) + coll_r + whole_coll(1));
    // The parked $300 is now folded into C's debt and re-weighted at C's OWN 50 bps (not B's 2000):
    // agg_weighted_debt_sum == 812.5e6·50.
    assert_weighted_sum(&svm, &coll, &[c.position]);
    assert_supply_invariant(&svm, &coll);
}

/// Redistribution × interest **lazy window**: after a victim's debt is redistributed, the parked
/// principal must accrue NOTHING while it is un-owned — it carries no weighted-sum term — and only
/// begins accruing (at the SURVIVOR's own rate, never the victim's) once the survivor realizes it.
/// This is the time-dimension the sibling `redistribution_with_per_position_interest` does not cover
/// (that one realizes the recipient immediately). The crux is a single number: after a full un-owned
/// year the aggregate grows by ONLY the survivor's own interest — $1125, not $1155 (parked at C's 5%)
/// nor $1185 (parked at B's 10%).
#[test]
fn redistribution_parked_debt_is_dormant_until_realized() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // Survivor C: deep collateral, $500 debt at 5% (500 bps). Victim B: 10 tokens ($1000 coll), $600
    // debt at 10% (1000 bps). RP is empty, so B's debt redistributes wholly to C. No time has passed,
    // so there is no interest yet.
    let c = open_borrower_rate(&mut svm, &coll_mint_auth, &coll, 1_000, usd(500), 500);
    let b = open_borrower_rate(&mut svm, &coll_mint_auth, &coll, 10, usd(600), 1_000);
    assert_eq!(read_market(&svm, &market).agg_recorded_debt, usd(1_100) as u128);
    assert_eq!(buffer_balance(&svm, &coll), 0, "buffer starts empty");

    // Crash to $80: B ($800 coll vs $600 debt, max_debt $533 at 150% MCR) is liquidatable; C is not.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate failed");

    let m = read_market(&svm, &market);
    // B's $600 redistributed (not extinguished) ⇒ still in agg_recorded_debt; B's whole 1000-bps
    // weight is stripped, and the parked $600 carries NO weighted term until C realizes it.
    assert_eq!(m.agg_recorded_debt, usd(1_100) as u128, "redistributed debt stays owed in the aggregate");
    assert_weighted_sum(&svm, &coll, &[c.position]); // only C's $500·500 remains; B removed
    assert_supply_invariant(&svm, &coll);

    // THE LAZY WINDOW: warp a full year BEFORE C realizes, then fold the aggregate. The parked $600
    // has no weighted term, so the aggregate accrues ONLY C's own $500 at 5% = +$25 → $1125. If the
    // inherited debt wrongly accrued, we'd see $1155 (at C's 5%) or $1185 (at B's 10%).
    const ONE_YEAR: i64 = 31_536_000;
    warp_unix(&mut svm, ONE_YEAR);
    send(&mut svm, &[refresh_market_ix(&coll)], &gov, &[]).expect("refresh");
    let m = read_market(&svm, &market);
    assert_eq!(
        m.agg_recorded_debt,
        usd(1_125) as u128,
        "parked debt accrues NOTHING while un-owned (lazy window); only C's own $500 grows"
    );
    assert_eq!(m.unminted_interest, 0, "the $25 of interest was minted out to the buffer");
    assert_eq!(buffer_balance(&svm, &coll), usd(25), "only C's own interest funded the buffer");
    assert_weighted_sum(&svm, &coll, &[c.position]); // C un-touched: still $500·500
    assert_supply_invariant(&svm, &coll);

    // C realizes (deposit 1 token): its own $500 capitalizes +$25 (→ $525), then the parked $600 folds
    // in (→ $1125) and from now accrues at C's OWN 5% — the weighted sum re-bases to $1125·500, never
    // B's 1000 bps.
    fund_and_deposit(&mut svm, &coll_mint_auth, &coll, &c, whole_coll(1));
    let cp = read_position(&svm, &c.position);
    assert_eq!(cp.recorded_debt, usd(1_125) as u128, "C: own $500 + $25 interest + $600 inherited");
    assert_weighted_sum(&svm, &coll, &[c.position]); // 1125·500 — inherited debt entered at C's rate
    assert_supply_invariant(&svm, &coll);
}

/// Redistribution across coprime stakes (7/11/13) does NOT divide evenly, so each recipient
/// realizes a FLOORED share and a unit or two of dust stays in the market aggregates. Asserts the
/// protocol-favoring inequality (aggregates >= Σ positions, never the reverse) rather than a strict
/// equality that would be wrong — and that `total_collateral == vault` stays exact regardless.
#[test]
fn redistribution_dust_three_recipients_inequality() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let market = market_pda(&coll);
    let coll_vault = coll_vault_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // Coprime stakes (7, 11, 13 tokens), no debt of their own. RP empty.
    let c1 = open_borrower(&mut svm, &coll_mint_auth, &coll, 7, 0);
    let c2 = open_borrower(&mut svm, &coll_mint_auth, &coll, 11, 0);
    let c3 = open_borrower(&mut svm, &coll_mint_auth, &coll, 13, 0);
    // Borrower B: 10 tokens, $600 debt -> full redistribution across 31 tokens of stake.
    let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    liquidate(&mut svm, &gov, &coll, &b.position)
        .expect("liquidate failed");

    // Touch all three so they realize their floored shares.
    fund_and_deposit(&mut svm, &coll_mint_auth, &coll, &c1, whole_coll(1));
    fund_and_deposit(&mut svm, &coll_mint_auth, &coll, &c2, whole_coll(1));
    fund_and_deposit(&mut svm, &coll_mint_auth, &coll, &c3, whole_coll(1));

    let m = read_market(&svm, &market);
    let p1 = read_position(&svm, &c1.position);
    let p2 = read_position(&svm, &c2.position);
    let p3 = read_position(&svm, &c3.position);
    let sum_ink = (p1.ink + p2.ink + p3.ink) as u128;
    let sum_art = p1.recorded_debt + p2.recorded_debt + p3.recorded_debt;

    // Protocol-favoring dust: aggregates >= Σ realized positions, gap bounded by ~num_recipients;
    // Σ ink/art must NEVER exceed the aggregate (that direction would be a solvency hole).
    assert!(sum_ink <= m.total_collateral, "Σ ink must not exceed total_collateral (solvency)");
    assert!(m.total_collateral - sum_ink <= 3, "collateral dust bounded: {}", m.total_collateral - sum_ink);
    assert!(sum_art <= m.agg_recorded_debt, "Σ art must not exceed agg_art (solvency)");
    assert!(m.agg_recorded_debt - sum_art <= 3, "debt dust bounded: {}", m.agg_recorded_debt - sum_art);

    // total_collateral == vault stays EXACT despite the per-position floor dust.
    assert_eq!(m.total_collateral, token_balance(&svm, &coll_vault) as u128);
    // No RP offset -> all of B's $600 is owed by C1..C3 (plus the rounding dust held in the market).
    assert_eq!(m.agg_recorded_debt, usd(600) as u128);
}
