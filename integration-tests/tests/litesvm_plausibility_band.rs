//! The oracle plausibility band.
//!
//! `update_price` commits a fresh aggregate to `Market.spot`/`debt_spot` only when it is also
//! PLAUSIBLE: the chosen mid price must lie inside the per-market `[band_lower_ray, band_upper_ray]`
//! rail. The band is a COARSE 10^k-scale / absolute-nonsense guard (the Sept-2021 Pyth mis-scale
//! class, or a wrong feed rebind during the Pyth core migration), NOT a tight price opinion — it
//! catches exactly what the cross-oracle divergence checks CAN'T: a secondary-absent or
//! all-legs-agreeing-but-absurd price. An implausible fresh price is WITHHELD (the freshness clock
//! does not advance), so the cache ages into the staleness machinery rather than
//! committing nonsense as the liquidation/redemption price.
//!
//! Requires the dev-oracle `.so`: `anchor build -- --features dev-oracle`.

use fusd_core::events::PriceCommitted;
use fusd_integration_tests::*;
use fusd_math::RAY;
use solana_sdk::signature::{Keypair, Signer};

const PYTH_EXPO: i32 = -8;

fn pyth_usd(price_usd: i64) -> i64 {
    price_usd * 100_000_000
}
fn sb_usd(value_usd: i128) -> i128 {
    value_usd * 1_000_000_000_000_000_000
}

fn actors() -> (litesvm::LiteSVM, Keypair, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    (svm, gov, Keypair::new())
}

#[test]
fn band_withholds_implausible_commit_even_when_legs_agree() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    // A coarse rail [$50, $500] (10× wide; usd_ray for $X is X·RAY).
    let h = bootstrap_oracle_banded(&mut svm, &gov, &coll, 300, 3, 300, false, 50 * RAY, 500 * RAY);

    // In-band $100 (conf 0): commits. (Pyth-only ⇒ mints frozen, but the price still commits off the
    // fresh primary; the band gate is what we're isolating, not mode.)
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, pyth_usd(100), 0, PYTH_EXPO, now);
    let meta = send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .expect("in-band crank");
    assert!(single_event::<PriceCommitted>(&meta).plausible, "$100 is inside [$50,$500]");
    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.spot, spot_for_usd(100), "in-band price commits");
    let committed_slot = m.spot_updated_slot;

    // Absurd $10_000 (a 100× mis-scale) — with Pyth AND Switchboard AGREEING at the absurd value, so
    // the conf/deviation/TWAP checks would all pass. ONLY the band catches it.
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, pyth_usd(10_000), 0, PYTH_EXPO, now);
    set_switchboard_feed(&mut svm, &h.sb, sb_usd(10_000), 0, 1, now);
    svm.expire_blockhash();
    let meta = send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, Some(h.sb))], &gov, &[])
        .expect("absurd crank still runs (degrades, never reverts)");
    let ev = single_event::<PriceCommitted>(&meta);
    assert!(!ev.plausible, "an agreeing-but-absurd aggregate is implausible — the band's whole point");
    assert!(ev.mint_frozen, "implausible ⇒ mints frozen");

    let m = read_market(&svm, &market_pda(&coll));
    assert_eq!(m.spot, spot_for_usd(100), "implausible commit WITHHELD — spot unchanged");
    assert_eq!(m.debt_spot, spot_for_usd(100), "debt_spot likewise unchanged");
    assert_eq!(
        m.spot_updated_slot, committed_slot,
        "freshness clock did NOT advance — the withheld price ages into the staleness gate"
    );
}

#[test]
fn band_aged_out_then_recovers() {
    // After an implausible run withholds the commit, the cache ages out (staleness machinery), and a
    // later PLAUSIBLE price commits normally — the band is self-clearing, not a terminal wedge.
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle_banded(&mut svm, &gov, &coll, 300, 3, 300, false, 50 * RAY, 500 * RAY);

    // Implausible from the first crank ⇒ never commits ⇒ spot stays 0 (never priced).
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, pyth_usd(9_999), 0, PYTH_EXPO, now);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .expect("implausible crank");
    assert_eq!(read_market(&svm, &market_pda(&coll)).spot, 0, "never committed a nonsense price");

    // A plausible price now commits.
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, pyth_usd(120), 0, PYTH_EXPO, now);
    svm.expire_blockhash();
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .expect("plausible crank");
    assert_eq!(read_market(&svm, &market_pda(&coll)).spot, spot_for_usd(120), "plausible price commits");
}

#[test]
fn band_init_clamps() {
    // The band can only ever be a COARSE rail: init rejects a reversed/degenerate band and any band
    // narrower than MIN_PRICE_BAND_RATIO (4×), so a captured governance can't weaponize a tight
    // always-breaching band into a synthetic oracle outage. Single-sided (one bound disabled) is OK.
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let quote = create_quote_mint(&mut svm, &gov, FUSD_DECIMALS);

    // Reversed: upper < lower (and < lower·4).
    let mut args = default_oracle_args();
    args.price_band_lower_ray = 500 * RAY;
    args.price_band_upper_ray = 50 * RAY;
    let f = send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args.clone())], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS, "reversed band rejected");

    // Too narrow: [100, 300] is only 3× (< 4×).
    args.price_band_lower_ray = 100 * RAY;
    args.price_band_upper_ray = 300 * RAY;
    let f = send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args.clone())], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS, "sub-ratio band rejected");

    // Single-sided (lower set, upper disabled) is accepted — the success path also proves init
    // succeeds once the band is valid (the both-sided wide case is exercised by the commit tests).
    args.price_band_lower_ray = 50 * RAY;
    args.price_band_upper_ray = 0;
    send(&mut svm, &[init_market_oracle_ix(&gov.pubkey(), &coll, &quote, args)], &gov, &[])
        .expect("single-sided band accepted");
}
