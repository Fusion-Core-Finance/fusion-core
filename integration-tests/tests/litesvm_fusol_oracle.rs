//! End-to-end litesvm tests for the CANONICAL-PRIMARY oracle mode (fuSOL).
//!
//! A canonical-primary market has no external market feed for its collateral: the price IS
//! `sol_usd × pool_rate`, composed by `update_price` scaling the bound SOL/USD Pyth (primary)
//! and Switchboard (secondary) views by the FORK stake pool's `total_lamports /
//! pool_token_supply`. Both `spot` AND `debt_spot` track pool NAV; the mandatory liquidity
//! haircut discounts the collateral (mint/LTV) leg only; an unavailable pool rate WITHHOLDS the
//! commit (no market feed exists to fall back on) and freezes mints; the TWAP corridor is
//! optional (no fuSOL venue exists pre-listing).
//!
//! Requires the dev-oracle `.so`: `anchor build -- --features dev-oracle`.

use fusd_core::constants::{
    FUSION_STAKE_POOL_PROGRAM_ID, LIQ_RESUME_GRACE_SLOTS, MAX_PRICE_STALENESS_SLOTS,
    POOL_UPDATE_GRACE_DIVISOR, PYTH_SOL_USD_FEED_ID,
};
use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const PYTH_EXPO: i32 = -8;
fn pyth_usd(d: i64) -> i64 {
    d * 100_000_000 // $d at expo -8
}
fn sb_usd(d: i128) -> i128 {
    d * 1_000_000_000_000_000_000 // $d at Switchboard's 1e18
}
/// 1 fuSOL = `n` × 10^9 smallest units (9-decimal, like SOL).
fn fusol(n: u64) -> u64 {
    n * 1_000_000_000
}

fn actors() -> (litesvm::LiteSVM, Keypair, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    (svm, gov, cma)
}

/// Bootstrap a canonical-primary market at a 10% liquidity haircut. NO TWAP samples are ever
/// posted — the corridor is optional in this mode, so mint-mode must be reachable without one.
fn reach_fusol(
    svm: &mut litesvm::LiteSVM,
    gov: &Keypair,
    cma: &Keypair,
) -> (Pubkey, OracleHandles, Pubkey) {
    let coll = bootstrap_market(svm, gov, cma);
    let (h, stake_pool) = bootstrap_oracle_fusol(svm, gov, &coll, 1_000); // 10% haircut
    (coll, h, stake_pool)
}

/// Post the SOL/USD legs (Pyth + Switchboard, conf 0 so the −k·σ haircut is zero and the
/// composed price is exact).
fn post_sol_usd(svm: &mut litesvm::LiteSVM, h: &OracleHandles, sol_usd: i64) {
    let now = now_unix(svm);
    set_pyth_price(svm, &h.pyth, PYTH_SOL_USD_FEED_ID, pyth_usd(sol_usd), 0, PYTH_EXPO, now);
    set_switchboard_feed(svm, &h.sb, sb_usd(sol_usd as i128), 0, 1, now);
}

/// Fabricate the FORK-owned pool at `rate = total/supply`, freshly finalized this epoch.
fn post_fork_pool(
    svm: &mut litesvm::LiteSVM,
    stake_pool: &Pubkey,
    coll: &Pubkey,
    total_lamports: u64,
    pool_token_supply: u64,
) {
    let epoch = now_epoch(svm);
    set_stake_pool_owned(
        svm,
        stake_pool,
        coll,
        total_lamports,
        pool_token_supply,
        epoch,
        FUSION_STAKE_POOL_PROGRAM_ID,
    );
}

fn crank(
    svm: &mut litesvm::LiteSVM,
    gov: &Keypair,
    coll: &Pubkey,
    h: &OracleHandles,
    stake_pool: Option<Pubkey>,
) -> Result<(), litesvm::types::FailedTransactionMetadata> {
    send(
        svm,
        // Mode 1 never reads the C1 `sol_usd_pyth_update` account (the PRIMARY is already
        // SOL/USD) — pass None to prove it.
        &[update_price_lst_ix(&gov.pubkey(), coll, &h.pyth, Some(h.sb), None, stake_pool)],
        gov,
        &[],
    )
    .map(|_| ())
}

/// SOL $100 × rate 1.2 = $120 NAV; spot = $120 − 10% haircut = $108; debt_spot = $120 raw.
/// Mints OPEN with no TWAP ever posted (the corridor is optional in this mode).
#[test]
fn composes_sol_usd_times_rate_with_haircut() {
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fusol(&mut svm, &gov, &cma);

    post_sol_usd(&mut svm, &h, 100);
    post_fork_pool(&mut svm, &stake_pool, &coll, fusol(1_200_000), fusol(1_000_000)); // rate 1.2
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("crank fusol");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(!m.mint_frozen, "SOL/USD legs agree + pool fresh + corridor optional ⇒ mints OPEN");
    assert_eq!(m.spot, spot_for_usd(108), "spot = 120 NAV − 10% liquidity haircut");
    assert_eq!(m.debt_spot, spot_for_usd(120), "debt/liquidation leg = raw NAV (no haircut)");
}

/// A NAV drop (slashing / accounting correction) reaches BOTH prices on the next crank — the
/// negative-NAV requirement on the liquidation path.
#[test]
fn nav_drop_hits_both_prices_immediately() {
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fusol(&mut svm, &gov, &cma);

    post_sol_usd(&mut svm, &h, 100);
    post_fork_pool(&mut svm, &stake_pool, &coll, fusol(1_200_000), fusol(1_000_000));
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("crank @1.2");
    assert_eq!(read_market(&svm, &market_pda(&coll)).debt_spot, spot_for_usd(120));

    warp_unix(&mut svm, 5);
    post_sol_usd(&mut svm, &h, 100);
    post_fork_pool(&mut svm, &stake_pool, &coll, fusol(800_000), fusol(1_000_000)); // NAV 1.2 → 0.8
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("crank @0.8");

    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.spot, spot_for_usd(72), "80 NAV − 10% haircut");
    assert_eq!(m.debt_spot, spot_for_usd(80), "liquidation leg dropped with NAV, same crank");
}

/// An unavailable pool rate (absent account / epoch lag) WITHHOLDS the commit — the freshness
/// clock must NOT advance off an unscaled SOL/USD price — and freezes mints.
#[test]
fn unavailable_pool_withholds_commit_and_freezes() {
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fusol(&mut svm, &gov, &cma);

    post_sol_usd(&mut svm, &h, 100);
    post_fork_pool(&mut svm, &stake_pool, &coll, fusol(1_200_000), fusol(1_000_000));
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("healthy crank");
    let committed_slot = read_market(&svm, &market_pda(&coll)).spot_updated_slot;
    assert!(!read_market(&svm, &market_pda(&coll)).mint_frozen);

    // Omit the pool account entirely: tx succeeds (permissionless crank must not brick), but the
    // commit is withheld and mints freeze.
    warp_unix(&mut svm, 5);
    warp_slots(&mut svm, 10);
    post_sol_usd(&mut svm, &h, 100);
    crank(&mut svm, &gov, &coll, &h, None).expect("pool-less crank still lands");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen, "no pool rate ⇒ mints frozen");
    assert_eq!(m.spot, spot_for_usd(108), "last good price retained");
    assert_eq!(m.spot_updated_slot, committed_slot, "commit withheld — freshness clock unmoved");

    // Same for an epoch-lagged pool (finalization stopped > MAX_STAKE_POOL_EPOCH_LAG epochs ago).
    warp_unix(&mut svm, 5);
    warp_slots(&mut svm, 10);
    let stale_epoch = now_epoch(&svm); // pool stamped at current epoch...
    set_stake_pool_owned(
        &mut svm,
        &stake_pool,
        &coll,
        fusol(1_200_000),
        fusol(1_000_000),
        stale_epoch,
        FUSION_STAKE_POOL_PROGRAM_ID,
    );
    // ...then the cluster moves 3 epochs ahead.
    let mut clock: solana_sdk::clock::Clock = svm.get_sysvar();
    clock.epoch += 3;
    svm.set_sysvar(&clock);
    post_sol_usd(&mut svm, &h, 100);
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("epoch-lagged crank lands");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen, "epoch-lagged pool ⇒ mints frozen");
    assert_eq!(m.spot_updated_slot, committed_slot, "still withheld");
}

/// A pool owned by the UPSTREAM program (`SPoo1…`) is the wrong deployment for a canonical-primary
/// market — a mis-built crank, hard revert (never a silent mis-price).
#[test]
fn upstream_owned_pool_hard_reverts() {
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fusol(&mut svm, &gov, &cma);

    post_sol_usd(&mut svm, &h, 100);
    // `set_stake_pool` (no _owned) fabricates under the upstream SPoo1… owner.
    let epoch = now_epoch(&svm);
    set_stake_pool(&mut svm, &stake_pool, &coll, fusol(1_200_000), fusol(1_000_000), epoch);
    let err = crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect_err("wrong owner");
    assert_eq!(custom_code(&err), E_INVALID_STAKE_POOL);
}

/// Without the Switchboard secondary the deviation corridor can't verify ⇒ mints freeze, but the
/// composed price still commits (freeze-mints-only, never a price outage).
#[test]
fn missing_secondary_freezes_but_still_prices() {
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fusol(&mut svm, &gov, &cma);

    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, PYTH_SOL_USD_FEED_ID, pyth_usd(100), 0, PYTH_EXPO, now);
    post_fork_pool(&mut svm, &stake_pool, &coll, fusol(1_200_000), fusol(1_000_000));
    send(
        &mut svm,
        &[update_price_lst_ix(&gov.pubkey(), &coll, &h.pyth, None, None, Some(stake_pool))],
        &gov,
        &[],
    )
    .expect("SB-less crank");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen, "no secondary ⇒ mint corridor unverifiable");
    assert_eq!(m.spot, spot_for_usd(108), "composed price still committed");
    assert_eq!(m.debt_spot, spot_for_usd(120));
}

/// Init-time validation: the mode's requirements are enforced, and the haircut knob is
/// canonical-primary-only.
#[test]
fn init_rejections() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let quote = create_quote_mint(&mut svm, &gov, FUSD_DECIMALS);

    let base = || {
        let mut a = default_oracle_args();
        a.pyth_feed_id = PYTH_SOL_USD_FEED_ID;
        a.orca_pool = Pubkey::default();
        a.raydium_pool = Pubkey::default();
        a.lst_stake_pool = Pubkey::new_unique();
        a.canonical_primary = true;
        a.liquidity_haircut_bps = 1_000;
        a
    };

    let reject = |svm: &mut litesvm::LiteSVM, args| {
        let err = send(
            svm,
            &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args)],
            &gov,
            &[],
        )
        .expect_err("must reject");
        assert_eq!(custom_code(&err), E_PARAM_OUT_OF_BOUNDS);
    };

    // Haircut mandatory in mode 1 (and clamped).
    let mut a = base();
    a.liquidity_haircut_bps = 0;
    reject(&mut svm, a);
    let mut a = base();
    a.liquidity_haircut_bps = 2_001; // > MAX_LIQUIDITY_HAIRCUT_BPS
    reject(&mut svm, a);
    // No DEX pools in mode 1 (the corridor is a deferred rate-domain design).
    let mut a = base();
    a.orca_pool = Pubkey::new_unique();
    reject(&mut svm, a);
    // The bound Pyth feed MUST be the shared SOL/USD id.
    let mut a = base();
    a.pyth_feed_id = [7u8; 32];
    reject(&mut svm, a);
    // The fork pool binding is mandatory.
    let mut a = base();
    a.lst_stake_pool = Pubkey::default();
    reject(&mut svm, a);
    // Mode 0 rejects the haircut knob (canonical-primary-only).
    let mut a = default_oracle_args();
    a.liquidity_haircut_bps = 500;
    reject(&mut svm, a);

    // Sanity: the base config itself is accepted.
    send(
        &mut svm,
        &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, base())],
        &gov,
        &[],
    )
    .expect("valid canonical-primary init");
    let mo = read_market_oracle(&svm, &coll);
    assert_eq!((mo.canonical_primary, mo.liquidity_haircut_bps), (1, 1_000));
    assert_eq!(mo.last_canonical_rate_ray, 0, "no canonical rate committed yet (sentinel)");
}

/// A canonical-primary crank that ALSO supplies the (mode-0-only) optional `sol_usd_pyth_update`
/// account must still succeed and commit: mode 1 never runs the C1 min-cap leg (the rate is IN the
/// composed price), so the extra account is ignored — it must not hard-fail the permissionless
/// crank against the FORK-owned pool (whose owner is not the upstream `SPoo1…` program the C1 leg
/// checks for).
#[test]
fn crank_with_sol_usd_account_supplied_still_succeeds() {
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fusol(&mut svm, &gov, &cma);

    post_sol_usd(&mut svm, &h, 100);
    post_fork_pool(&mut svm, &stake_pool, &coll, fusol(1_200_000), fusol(1_000_000)); // rate 1.2
    // The realistic keeper shape: the same SOL/USD update passed in BOTH the primary and the
    // C1 `sol_usd_pyth_update` slot.
    send(
        &mut svm,
        &[update_price_lst_ix(
            &gov.pubkey(),
            &coll,
            &h.pyth,
            Some(h.sb),
            Some(h.pyth),
            Some(stake_pool),
        )],
        &gov,
        &[],
    )
    .expect("canonical-primary crank with the optional sol_usd account must not hard-fail");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(!m.mint_frozen, "commit is byte-identical to the omit path");
    assert_eq!(m.spot, spot_for_usd(108), "$120 NAV − 10% haircut");
    assert_eq!(m.debt_spot, spot_for_usd(120));
}

/// Warp to `(epoch, first_slot_of_epoch + offset)` keeping Clock/EpochSchedule consistent (the
/// slot-precise sibling of the harness `warp_epochs`, which always lands on offset 0).
fn warp_to_epoch_slot(svm: &mut litesvm::LiteSVM, epoch: u64, offset: u64) {
    let schedule: solana_sdk::epoch_schedule::EpochSchedule = svm.get_sysvar();
    let mut clock: solana_sdk::clock::Clock = svm.get_sysvar();
    clock.epoch = epoch;
    clock.slot = schedule.get_first_slot_in_epoch(epoch) + offset;
    svm.set_sysvar(&clock);
    svm.warp_to_slot(clock.slot);
    svm.expire_blockhash();
}

/// The POOL_UPDATE_GRACE deadline on a lagged pool, at exact boundaries: a pool finalized in the
/// PREVIOUS epoch commits while the current epoch is within its first 1/16 of slots, WITHHOLDS one
/// slot past that, and a 2-epoch-lagged pool withholds even at the very first slot. Without the
/// deadline a keeper could keep re-committing a pre-loss pool rate under fresh SOL/USD legs,
/// refreshing `spot_updated_slot` forever and defeating the staleness fuse.
#[test]
fn lagged_pool_update_grace_boundaries() {
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fusol(&mut svm, &gov, &cma);

    // Land on a consistent (epoch, slot) and baseline-commit with a pool finalized THIS epoch.
    warp_epochs(&mut svm, 6);
    let e = now_epoch(&svm);
    post_sol_usd(&mut svm, &h, 100);
    post_fork_pool(&mut svm, &stake_pool, &coll, fusol(1_200_000), fusol(1_000_000)); // stamps e
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("baseline crank");
    assert!(!read_market(&svm, &market_pda(&coll)).mint_frozen);

    let schedule: solana_sdk::epoch_schedule::EpochSchedule = svm.get_sysvar();
    let grace = schedule.get_slots_in_epoch(e + 1) / POOL_UPDATE_GRACE_DIVISOR;
    assert!(grace > 1, "epoch long enough to probe the boundary");

    // Just INSIDE the grace (the window's last slot): the 1-epoch-lagged pool still commits.
    warp_to_epoch_slot(&mut svm, e + 1, grace);
    post_sol_usd(&mut svm, &h, 100);
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("inside-grace crank");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(!m.mint_frozen, "1-epoch lag INSIDE the grace window still prices + mints");
    assert_eq!(m.spot_updated_slot, current_slot(&svm), "committed — freshness clock advanced");
    let committed_slot = m.spot_updated_slot;

    // Just PAST the grace (one slot later, same lagged epoch): the commit is WITHHELD.
    warp_to_epoch_slot(&mut svm, e + 1, grace + 1);
    post_sol_usd(&mut svm, &h, 100);
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool))
        .expect("past-grace crank still lands (the permissionless crank never bricks)");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen, "1-epoch lag PAST the grace ⇒ mints frozen");
    assert_eq!(m.spot_updated_slot, committed_slot, "withheld — freshness clock unmoved");
    assert_eq!(m.spot, spot_for_usd(108), "last good price retained");

    // 2 epochs of lag: withheld even at the FIRST slot of the epoch (the grace never applies).
    warp_to_epoch_slot(&mut svm, e + 2, 0);
    post_sol_usd(&mut svm, &h, 100);
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("2-epoch-lag crank lands");
    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.mint_frozen, "2-epoch-lagged pool ⇒ withheld under canonical-primary");
    assert_eq!(m.spot_updated_slot, committed_slot, "still withheld");
}

/// The NAV-decrease grace detector is keyed on the POOL RATE, never the composed price: a pool
/// rate drop arms `liq_grace_until` (while the lower price commits IMMEDIATELY, same crank), a
/// plain SOL/USD dip never does, and the first commit only seeds the rate memory.
#[test]
fn nav_drop_arms_liq_grace_but_sol_usd_dip_does_not() {
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fusol(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    // First crank: seeds the rate memory, never arms.
    post_sol_usd(&mut svm, &h, 100);
    post_fork_pool(&mut svm, &stake_pool, &coll, fusol(1_200_000), fusol(1_000_000)); // rate 1.2
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("first crank");
    assert_eq!(read_market(&svm, &market).liq_grace_until, 0, "first commit never arms");
    assert_eq!(
        read_market_oracle(&svm, &coll).last_canonical_rate_ray,
        fusd_math::RAY * 12 / 10,
        "rate memory seeded at 1.2 RAY"
    );

    // SOL/USD dips $100 → $75 with the pool rate UNCHANGED: normal market movement — the lower
    // price commits, the grace stays unarmed.
    warp_unix(&mut svm, 5);
    warp_slots(&mut svm, 10);
    post_sol_usd(&mut svm, &h, 75);
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("sol/usd dip crank");
    let m = read_market(&svm, &market);
    assert_eq!(m.spot, spot_for_usd(81), "75 × 1.2 = 90 NAV − 10% haircut, committed");
    assert_eq!(m.debt_spot, spot_for_usd(90));
    assert_eq!(m.liq_grace_until, 0, "a SOL/USD move must NOT arm the NAV grace");

    // Pool NAV drops 1.2 → 0.8 (slashing / negative finalization): the loss commits in the SAME
    // crank AND the standard liquidation grace arms.
    warp_unix(&mut svm, 5);
    warp_slots(&mut svm, 10);
    post_sol_usd(&mut svm, &h, 75);
    post_fork_pool(&mut svm, &stake_pool, &coll, fusol(800_000), fusol(1_000_000)); // rate 0.8
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("NAV-drop crank");
    let m = read_market(&svm, &market);
    assert_eq!(m.spot, spot_for_usd(54), "75 × 0.8 = 60 NAV − 10% haircut — committed immediately");
    assert_eq!(m.debt_spot, spot_for_usd(60), "liquidation leg dropped in the same crank");
    let armed = current_slot(&svm) + LIQ_RESUME_GRACE_SLOTS;
    assert_eq!(m.liq_grace_until, armed, "pool NAV decrease arms the on-resume grace");
    assert_eq!(
        read_market_oracle(&svm, &coll).last_canonical_rate_ray,
        fusd_math::RAY * 8 / 10,
        "rate memory tracks the committed rate"
    );

    // An equal-rate follow-up crank never re-arms (monotone; no new loss observed).
    warp_unix(&mut svm, 5);
    warp_slots(&mut svm, 10);
    post_sol_usd(&mut svm, &h, 75);
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("equal-rate crank");
    assert_eq!(read_market(&svm, &market).liq_grace_until, armed, "no re-arm at an equal rate");
}

/// End-to-end 12.4 semantics on a borrower: a pool NAV drop makes the position under-MCR at the
/// (immediately committed) lower price, but liquidation is REJECTED through the grace window and
/// opens exactly once the boundary passes — with a keeper keeping the price fresh the whole time.
#[test]
fn nav_drop_liquidation_blocked_through_grace_boundary() {
    let (mut svm, gov, cma) = actors();
    let (coll, h, stake_pool) = reach_fusol(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    // Healthy market at SOL $100 × rate 1.2 (spot $108 / debt_spot $120); mints are open.
    post_sol_usd(&mut svm, &h, 100);
    post_fork_pool(&mut svm, &stake_pool, &coll, fusol(1_200_000), fusol(1_000_000));
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("healthy crank");
    assert!(!read_market(&svm, &market).mint_frozen);

    // A $1000 Reactor Pool (absorbs any liquidation) and borrower B: 10 fuSOL vs $600 debt —
    // healthy at debt_spot $120 (10×120 = $1200 ≥ 600×1.5), under-MCR once debt_spot hits $80.
    let d = open_borrower(&mut svm, &cma, &coll, 100, usd(1_000));
    provide_sp(&mut svm, &d, &coll, usd(1_000));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    // NAV 1.2 → 0.8: the loss reaches BOTH prices this crank, and the grace arms.
    warp_unix(&mut svm, 5);
    warp_slots(&mut svm, 10);
    post_sol_usd(&mut svm, &h, 100);
    post_fork_pool(&mut svm, &stake_pool, &coll, fusol(800_000), fusol(1_000_000));
    crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("NAV-drop crank");
    let m = read_market(&svm, &market);
    assert_eq!(m.debt_spot, spot_for_usd(80), "loss committed immediately (10×80 = $800 < $900)");
    let deadline = m.liq_grace_until;
    assert_eq!(deadline, current_slot(&svm) + LIQ_RESUME_GRACE_SLOTS, "grace armed");

    // Inside the window: liquidation is grace-blocked, the victim untouched.
    let f = liquidate(&mut svm, &gov, &coll, &b.position)
        .expect_err("the NAV-decrease grace must block liquidation");
    assert_eq!(custom_code(&f), E_LIQUIDATION_GRACE_PERIOD);
    assert_eq!(read_position(&svm, &b.position).recorded_debt, usd(600) as u128);

    // Walk past the boundary the way a keeper would: fresh cranks at the SAME 0.8 rate (equal
    // rate never re-arms; hops < MAX_PRICE_STALENESS_SLOTS never resume-arm).
    while current_slot(&svm) < deadline {
        warp_slots(&mut svm, MAX_PRICE_STALENESS_SLOTS - 1);
        warp_unix(&mut svm, 5);
        post_sol_usd(&mut svm, &h, 100);
        crank(&mut svm, &gov, &coll, &h, Some(stake_pool)).expect("keeper crank during grace");
    }
    assert_eq!(read_market(&svm, &market).liq_grace_until, deadline, "window never extended");

    // At/past the boundary the uncured borrower liquidates normally.
    liquidate(&mut svm, &gov, &coll, &b.position)
        .expect("liquidation re-enabled once the NAV-decrease grace elapses");
    let bp = read_position(&svm, &b.position);
    assert_eq!((bp.recorded_debt, bp.ink), (0, 0), "uncured borrower liquidated after the window");
}
