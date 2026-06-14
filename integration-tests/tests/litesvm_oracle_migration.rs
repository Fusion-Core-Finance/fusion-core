//! Bounded-updatable oracle program IDs + feed rebind.
//!
//! Fusion's endgame is an IMMUTABLE program, and Pyth's core program migration (~2026-07-31) changes
//! the receiver program ID. Hard-coded oracle program IDs would be a time bomb. These tests prove the
//! fix: the Pyth/Switchboard program IDs live in `ProtocolConfig` (seeded from the compile-time
//! genesis defaults), `update_price` verifies feed-account owners against THOSE (not constants), and
//! a gov-gated `set_oracle_program_ids` / `rebind_market_oracle_feeds` can absorb the migration in a
//! transaction rather than a redeploy.
//!
//! Requires the dev-oracle `.so`: `anchor build -- --features dev-oracle`.

use fusd_core::instructions::oracle_admin::RebindOracleFeedsArgs;
use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const PYTH_EXPO: i32 = -8;

fn pyth_usd(price_usd: i64) -> i64 {
    price_usd * 100_000_000
}

fn actors() -> (litesvm::LiteSVM, Keypair, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    (svm, gov, Keypair::new())
}

#[test]
fn genesis_seeds_compile_time_oracle_program_ids() {
    let (mut svm, gov, cma) = actors();
    let _coll = bootstrap_market(&mut svm, &gov, &cma);
    let cfg = read_protocol_config(&svm);
    assert_eq!(cfg.pyth_receiver_program_id, fusd_core::constants::PYTH_RECEIVER_PROGRAM_ID);
    assert_eq!(cfg.switchboard_program_id, fusd_core::constants::SWITCHBOARD_ON_DEMAND_PROGRAM_ID);
    // The upgraded receiver is pre-seeded as the second accepted owner ⇒ the 2026-07-31 cutover is a
    // non-event (no gov action needed).
    assert_eq!(
        cfg.pyth_receiver_program_id_alt,
        fusd_core::constants::PYTH_RECEIVER_PROGRAM_ID_UPGRADED,
        "alt receiver pre-seeded to the upgraded program"
    );
}

#[test]
fn upgraded_receiver_accepted_at_genesis_no_gov_action() {
    // The zero-downtime cutover: a PriceUpdateV2 owned by the UPGRADED receiver is honored straight
    // away — no migration transaction. (At/after 2026-07-31, keepers post upgraded-receiver updates
    // and the crank just works; during the dual-running window, old-receiver updates work too.)
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, false);

    // Update owned by the UPGRADED receiver (the post-cutover world) — accepted, spot commits.
    let now = now_unix(&svm);
    set_pyth_price_owned(
        &mut svm,
        &h.pyth,
        h.feed_id,
        pyth_usd(100),
        0,
        PYTH_EXPO,
        now,
        fusd_core::constants::PYTH_RECEIVER_PROGRAM_ID_UPGRADED,
    );
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .expect("upgraded-receiver update accepted with no gov action");
    assert_eq!(read_market(&svm, &market_pda(&coll)).spot, spot_for_usd(100));

    // The original receiver still works too (dual-running) — same account key, original owner.
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, pyth_usd(105), 0, PYTH_EXPO, now);
    svm.expire_blockhash();
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .expect("original receiver still accepted (dual-running)");
    assert_eq!(read_market(&svm, &market_pda(&coll)).spot, spot_for_usd(105));

    // A THIRD, unconfigured program is still rejected — only the two configured receivers are trusted.
    let now = now_unix(&svm);
    set_pyth_price_owned(&mut svm, &h.pyth, h.feed_id, pyth_usd(110), 0, PYTH_EXPO, now, Pubkey::new_unique());
    svm.expire_blockhash();
    let f = send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_PRICE_UPDATE, "unconfigured program rejected");
}

#[test]
fn set_oracle_program_ids_migrates_the_owner_check() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, false);

    // Genesis: a price posted under the REAL Pyth receiver commits.
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, pyth_usd(100), 0, PYTH_EXPO, now);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .expect("genesis crank");
    assert_eq!(read_market(&svm, &market_pda(&coll)).spot, spot_for_usd(100));

    // Migrate the Pyth receiver program ID to a NEW program (the ~2026-07-31 core migration).
    let new_pyth = Pubkey::new_unique();
    send(&mut svm, &[set_oracle_program_ids_ix(&gov.pubkey(), Some(new_pyth), None, None)], &gov, &[])
        .expect("migrate program id");
    assert_eq!(read_protocol_config(&svm).pyth_receiver_program_id, new_pyth);

    // A price still posted under the OLD program is now REJECTED — the owner check uses config's new ID.
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, pyth_usd(110), 0, PYTH_EXPO, now); // OLD owner
    svm.expire_blockhash();
    let f = send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_PRICE_UPDATE, "old-program account rejected post-migration");
    assert_eq!(read_market(&svm, &market_pda(&coll)).spot, spot_for_usd(100), "spot unchanged");

    // A price posted under the NEW program is ACCEPTED → spot moves to $110, proving the crank reads
    // the program ID from config rather than the compile-time constant.
    let now = now_unix(&svm);
    set_pyth_price_owned(&mut svm, &h.pyth, h.feed_id, pyth_usd(110), 0, PYTH_EXPO, now, new_pyth);
    svm.expire_blockhash();
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[])
        .expect("new-program crank");
    assert_eq!(read_market(&svm, &market_pda(&coll)).spot, spot_for_usd(110), "migrated program drives the price");
}

#[test]
fn set_oracle_program_ids_auth_and_validation() {
    let (mut svm, gov, cma) = actors();
    let _coll = bootstrap_market(&mut svm, &gov, &cma);

    // Only gov_authority may update the program IDs.
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let f = send(
        &mut svm,
        &[set_oracle_program_ids_ix(&rando.pubkey(), Some(Pubkey::new_unique()), None, None)],
        &rando,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED, "non-gov rejected");

    // A zero program ID (would brick the crank) is rejected.
    let f = send(
        &mut svm,
        &[set_oracle_program_ids_ix(&gov.pubkey(), Some(Pubkey::default()), None, None)],
        &gov,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS, "default pubkey rejected");

    // None leaves a field untouched (Switchboard unchanged when only Pyth is updated).
    let new_pyth = Pubkey::new_unique();
    let before = read_protocol_config(&svm).switchboard_program_id;
    send(&mut svm, &[set_oracle_program_ids_ix(&gov.pubkey(), Some(new_pyth), None, None)], &gov, &[])
        .expect("update pyth only");
    let cfg = read_protocol_config(&svm);
    assert_eq!(cfg.pyth_receiver_program_id, new_pyth);
    assert_eq!(cfg.switchboard_program_id, before, "None leaves switchboard unchanged");
}

#[test]
fn rebind_market_oracle_feeds_changes_the_feed_binding() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, false); // bound to feed_id h.feed_id

    // Crank with the original feed id works.
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, pyth_usd(100), 0, PYTH_EXPO, now);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[]).expect("crank A");
    assert_eq!(read_market(&svm, &market_pda(&coll)).spot, spot_for_usd(100));

    // Rebind to a new Pyth feed id (keeping the same SB account + pools).
    let feed_b = [9u8; 32];
    let args = RebindOracleFeedsArgs {
        pyth_feed_id: feed_b,
        switchboard_feed: h.sb,
        orca_pool: h.orca_pool,
        raydium_pool: h.raydium_pool,
    };
    send(&mut svm, &[rebind_market_oracle_feeds_ix(&gov.pubkey(), &coll, args)], &gov, &[])
        .expect("rebind feeds");
    assert_eq!(read_market_oracle(&svm, &coll).pyth_feed_id, feed_b);

    // A price under the OLD feed id is now rejected (feed-id binding mismatch).
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, h.feed_id, pyth_usd(120), 0, PYTH_EXPO, now);
    svm.expire_blockhash();
    let f = send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_PRICE_UPDATE, "old feed id rejected after rebind");

    // A price under the NEW feed id is accepted.
    let now = now_unix(&svm);
    set_pyth_price(&mut svm, &h.pyth, feed_b, pyth_usd(120), 0, PYTH_EXPO, now);
    svm.expire_blockhash();
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[]).expect("crank B");
    assert_eq!(read_market(&svm, &market_pda(&coll)).spot, spot_for_usd(120), "rebound feed drives the price");
}

#[test]
fn rebind_market_oracle_feeds_auth_and_validation() {
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, false);

    // Non-gov rejected.
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let ok_args = RebindOracleFeedsArgs {
        pyth_feed_id: [9u8; 32],
        switchboard_feed: h.sb,
        orca_pool: h.orca_pool,
        raydium_pool: Pubkey::default(),
    };
    let f = send(&mut svm, &[rebind_market_oracle_feeds_ix(&rando.pubkey(), &coll, ok_args.clone())], &rando, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED, "non-gov rejected");

    // No pool configured ⇒ rejected (the TWAP corridor is load-bearing for mint-mode).
    let no_pool = RebindOracleFeedsArgs { orca_pool: Pubkey::default(), ..ok_args.clone() };
    let f = send(&mut svm, &[rebind_market_oracle_feeds_ix(&gov.pubkey(), &coll, no_pool)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS, "no pool rejected");

    // Default Switchboard feed ⇒ rejected.
    let no_sb = RebindOracleFeedsArgs { switchboard_feed: Pubkey::default(), ..ok_args };
    let f = send(&mut svm, &[rebind_market_oracle_feeds_ix(&gov.pubkey(), &coll, no_sb)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS, "default switchboard rejected");
}

#[test]
fn gov_can_disable_and_reenable_the_alt_receiver() {
    // The alt receiver is gov-manageable: it may be disabled (set to default — the post-cutover
    // defense-in-depth cleanup) and re-enabled. Disabling it makes upgraded-receiver updates revert.
    let (mut svm, gov, cma) = actors();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let h = bootstrap_oracle(&mut svm, &gov, &coll, 300, 3, 300, false);
    let upgraded = fusd_core::constants::PYTH_RECEIVER_PROGRAM_ID_UPGRADED;

    // Seeded alt ⇒ upgraded-receiver update accepted.
    let now = now_unix(&svm);
    set_pyth_price_owned(&mut svm, &h.pyth, h.feed_id, pyth_usd(100), 0, PYTH_EXPO, now, upgraded);
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[]).expect("alt accepted");

    // Disable the alt (default = off). Primary/switchboard left untouched (None).
    send(&mut svm, &[set_oracle_program_ids_ix(&gov.pubkey(), None, Some(Pubkey::default()), None)], &gov, &[])
        .expect("disable alt");
    assert_eq!(read_protocol_config(&svm).pyth_receiver_program_id_alt, Pubkey::default());

    // Now an upgraded-receiver update is rejected (only the primary is accepted).
    let now = now_unix(&svm);
    set_pyth_price_owned(&mut svm, &h.pyth, h.feed_id, pyth_usd(105), 0, PYTH_EXPO, now, upgraded);
    svm.expire_blockhash();
    let f = send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_PRICE_UPDATE, "alt disabled ⇒ upgraded receiver rejected");

    // Re-enable the alt → accepted again.
    send(&mut svm, &[set_oracle_program_ids_ix(&gov.pubkey(), None, Some(upgraded), None)], &gov, &[])
        .expect("re-enable alt");
    let now = now_unix(&svm);
    set_pyth_price_owned(&mut svm, &h.pyth, h.feed_id, pyth_usd(105), 0, PYTH_EXPO, now, upgraded);
    svm.expire_blockhash();
    send(&mut svm, &[update_price_ix(&gov.pubkey(), &coll, &h.pyth, None)], &gov, &[]).expect("alt re-accepted");
    assert_eq!(read_market(&svm, &market_pda(&coll)).spot, spot_for_usd(105));
}
