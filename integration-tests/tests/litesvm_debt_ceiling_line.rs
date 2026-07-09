//! Debt-ceiling auto-line (Maker DC-IAM analog). A market's effective `Market.debt_ceiling` tracks
//! utilization: a permissionless `bump` raises it toward `MIN(line, agg_recorded_debt + gap)` in
//! `gap`-sized steps, no more often than every `ttl`, never above the gov-set hard `line`. The hot
//! `borrow` path is unchanged (it reads `Market.debt_ceiling`); the auto-line only moves that value.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_debt_ceiling_line

use fusd_core::state::MarketParam;
use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

const HOUR: i64 = 3_600;

fn actors() -> (litesvm::LiteSVM, Keypair, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    (svm, gov, Keypair::new())
}

#[test]
fn init_applies_initial_ceiling_min_line_debt_gap() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("price");

    // No debt yet ⇒ initial ceiling = min(line, 0 + gap) = gap (gap < line).
    send(&mut svm, &[init_debt_ceiling_line_ix(&gov.pubkey(), &coll, usd(1_000_000), usd(5_000), HOUR)], &gov, &[])
        .expect("init auto-line");
    let dcl = read_debt_ceiling_line(&svm, &coll);
    assert_eq!((dcl.line, dcl.gap, dcl.ttl), (usd(1_000_000), usd(5_000), HOUR));
    assert_eq!(read_market(&svm, &market).debt_ceiling, usd(5_000), "ceiling = min(line, 0 + gap) = gap");
}

#[test]
fn bump_follows_debt_up_by_gap_capped_at_line() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("price");
    // line $50k, gap $10k, ttl 1h.
    send(&mut svm, &[init_debt_ceiling_line_ix(&gov.pubkey(), &coll, usd(50_000), usd(10_000), HOUR)], &gov, &[]).expect("init");
    assert_eq!(read_market(&svm, &market).debt_ceiling, usd(10_000), "start = gap");

    // Borrow up to near the ceiling, then bump: ceiling rises to min(line, debt + gap).
    let b = open_borrower_rate(&mut svm, &cma, &coll, 1_000, usd(9_000), 100); // $9k debt, under $10k ceiling
    warp_unix(&mut svm, HOUR);
    send(&mut svm, &[bump_debt_ceiling_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("bump 1");
    let debt = read_market(&svm, &market).agg_recorded_debt as u64;
    assert_eq!(read_market(&svm, &market).debt_ceiling, debt + usd(10_000), "ceiling = debt + gap");

    // Borrow more (now allowed up to the raised ceiling), bump again — keeps following.
    send(&mut svm, &[borrow_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(9_000))], &b.kp, &[]).expect("borrow more");
    warp_unix(&mut svm, HOUR);
    send(&mut svm, &[bump_debt_ceiling_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("bump 2");
    let debt2 = read_market(&svm, &market).agg_recorded_debt as u64;
    assert_eq!(read_market(&svm, &market).debt_ceiling, debt2 + usd(10_000));

    // Drive debt high enough that debt + gap would exceed line: ceiling clamps to line.
    // Borrow up toward the current ceiling repeatedly, bumping, until debt + gap > line.
    for _ in 0..6 {
        let m = read_market(&svm, &market);
        let room = m.debt_ceiling.saturating_sub(m.agg_recorded_debt as u64);
        if room > usd(100) {
            let _ = send(&mut svm, &[borrow_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, room - usd(100))], &b.kp, &[]);
        }
        warp_unix(&mut svm, HOUR);
        let _ = send(&mut svm, &[bump_debt_ceiling_ix(&gov.pubkey(), &coll)], &gov, &[]);
    }
    assert_eq!(read_market(&svm, &market).debt_ceiling, usd(50_000), "ceiling clamps at the hard line");
}

#[test]
fn bump_throttled_by_ttl() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("price");
    send(&mut svm, &[init_debt_ceiling_line_ix(&gov.pubkey(), &coll, usd(50_000), usd(10_000), HOUR)], &gov, &[]).expect("init");

    // A bump before ttl elapses is rejected (the DC-IAM throttle, reusing E_RATE_LIMIT_EXCEEDED).
    warp_unix(&mut svm, HOUR - 60);
    let f = send(&mut svm, &[bump_debt_ceiling_ix(&gov.pubkey(), &coll)], &gov, &[]).expect_err("too soon");
    assert_eq!(custom_code(&f), E_RATE_LIMIT_EXCEEDED);

    // One more minute (>= ttl since init) → accepted.
    warp_unix(&mut svm, 60);
    send(&mut svm, &[bump_debt_ceiling_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("bump after ttl");
}

#[test]
fn permissionless_bump_never_exceeds_line() {
    // Anyone may bump, but the ceiling can never exceed the gov-set `line` — so opening the crank to
    // an arbitrary caller grants no authority over the cap.
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("price");
    // Tiny line $3k, huge gap $1M: even with a giant gap, the ceiling clamps to the $3k line.
    send(&mut svm, &[init_debt_ceiling_line_ix(&gov.pubkey(), &coll, usd(3_000), usd(1_000_000), 0)], &gov, &[]).expect("init");
    assert_eq!(read_market(&svm, &market).debt_ceiling, usd(3_000), "min(line, gap) = line");

    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    warp_unix(&mut svm, 1);
    send(&mut svm, &[bump_debt_ceiling_ix(&rando.pubkey(), &coll)], &rando, &[]).expect("anyone can bump");
    assert_eq!(read_market(&svm, &market).debt_ceiling, usd(3_000), "still clamped at line, no matter who bumps");
}

#[test]
fn governance_set_applies_new_params_immediately() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("price");
    send(&mut svm, &[init_debt_ceiling_line_ix(&gov.pubkey(), &coll, usd(50_000), usd(10_000), HOUR)], &gov, &[]).expect("init");
    assert_eq!(read_market(&svm, &market).debt_ceiling, usd(10_000));

    // Governance tightens to line $4k, gap $2k → effective ceiling min($4k, 0 + $2k) = $2k, at once.
    send(&mut svm, &[set_debt_ceiling_line_ix(&gov.pubkey(), &coll, usd(4_000), usd(2_000), HOUR)], &gov, &[]).expect("set");
    let dcl = read_debt_ceiling_line(&svm, &coll);
    assert_eq!((dcl.line, dcl.gap), (usd(4_000), usd(2_000)));
    assert_eq!(read_market(&svm, &market).debt_ceiling, usd(2_000), "new ceiling applied immediately");

    // A non-gov signer cannot set.
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let f = send(&mut svm, &[set_debt_ceiling_line_ix(&rando.pubkey(), &coll, usd(99_999), usd(99_999), 0)], &rando, &[]).expect_err("not gov");
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);
}

#[test]
fn auto_line_bump_overrides_a_debt_ceiling_param_change() {
    // Audit #10: on a market WITH an auto-line, the permissionless bump re-derives Market.debt_ceiling
    // from MIN(line, debt+gap) — so a (timelocked) DebtCeiling PARAM change is transient, overridden by
    // the next bump. It is BOUNDED (never above the gov-set `line`), not an escalation; the correct
    // lever for an auto-line market is set_debt_ceiling_line (the `line`). This pins that interaction.
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);
    send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), 0)], &gov, &[]).expect("gov gate");
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("price");
    // Auto-line: line $50k, gap $10k. No debt ⇒ ceiling = min(50k, 0+10k) = $10k.
    send(&mut svm, &[init_debt_ceiling_line_ix(&gov.pubkey(), &coll, usd(50_000), usd(10_000), HOUR)], &gov, &[]).expect("init auto-line");
    assert_eq!(read_market(&svm, &market).debt_ceiling, usd(10_000));

    // A timelocked DebtCeiling param LOWERS the live ceiling to $2k (queue + execute, timelock 0).
    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::DebtCeiling, usd(2_000))], &gov, &[]).expect("queue DebtCeiling");
    send(&mut svm, &[execute_param_change_ix(&gov.pubkey(), &coll, 0)], &gov, &[]).expect("execute DebtCeiling");
    assert_eq!(read_market(&svm, &market).debt_ceiling, usd(2_000), "DebtCeiling param applied");

    // The next permissionless bump re-derives the ceiling from the auto-line, OVERRIDING the param
    // back to min(line, debt+gap) = min(50k, 0+10k) = $10k — never above the hard `line`.
    warp_unix(&mut svm, HOUR);
    send(&mut svm, &[bump_debt_ceiling_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("bump");
    assert_eq!(
        read_market(&svm, &market).debt_ceiling,
        usd(10_000),
        "auto-line bump overrode the DebtCeiling param (bounded by line)"
    );
}
