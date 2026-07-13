//! Two-step `ProtocolConfig.gov_authority` rotation.
//!
//! The bootstrap/admin authority (gates `init_market`, `init_market_oracle`,
//! `init_reactor_pool`, `init_insurance_buffer`, `init_governance_gate`, `set_guardian`)
//! previously had NO transfer path; the roadmap's governance-minimization path requires handing
//! it to a successor signer or PDA. The handoff is propose/accept: the live key moves only when
//! the proposed successor itself signs, so a typo'd / unheld proposal can never strand the role.
//! Requires the dev-oracle `.so`.

use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

fn setup() -> (litesvm::LiteSVM, Keypair) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    set_program_upgrade_authority(&mut svm, &gov.pubkey());
    send(&mut svm, &[init_protocol_ix(&gov.pubkey())], &gov, &[]).expect("init_protocol");
    (svm, gov)
}

#[test]
fn happy_path_rotates_and_old_key_loses_gated_paths() {
    let (mut svm, gov) = setup();
    let successor = Keypair::new();
    airdrop_sol(&mut svm, &successor.pubkey(), 10_000);

    // Propose: live authority unchanged, pending recorded.
    send(&mut svm, &[migrate_gov_authority_ix(&gov.pubkey(), &successor.pubkey())], &gov, &[])
        .expect("propose");
    let cfg = read_protocol_config(&svm);
    assert_eq!(cfg.gov_authority, gov.pubkey());
    assert_eq!(cfg.pending_gov_authority, successor.pubkey());

    // Accept: live authority moves, pending clears.
    send(&mut svm, &[accept_gov_authority_ix(&successor.pubkey())], &successor, &[])
        .expect("accept");
    let cfg = read_protocol_config(&svm);
    assert_eq!(cfg.gov_authority, successor.pubkey());
    assert_eq!(cfg.pending_gov_authority, Pubkey::default());

    // A gov_authority-gated path proves the rotation took: set_guardian by the OLD key fails,
    // by the NEW key succeeds.
    let new_guardian = Keypair::new();
    let f = send(&mut svm, &[set_guardian_ix(&gov.pubkey(), &new_guardian.pubkey())], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);
    send(&mut svm, &[set_guardian_ix(&successor.pubkey(), &new_guardian.pubkey())], &successor, &[])
        .expect("set_guardian by the rotated-in admin");
    assert_eq!(read_protocol_config(&svm).guardian, new_guardian.pubkey());
}

#[test]
fn post_rotation_old_key_cannot_init_market_new_key_can() {
    let (mut svm, gov) = setup();
    let successor = Keypair::new();
    airdrop_sol(&mut svm, &successor.pubkey(), 10_000);
    send(&mut svm, &[migrate_gov_authority_ix(&gov.pubkey(), &successor.pubkey())], &gov, &[])
        .expect("propose");
    send(&mut svm, &[accept_gov_authority_ix(&successor.pubkey())], &successor, &[])
        .expect("accept");

    // init_market gated on the LIVE authority: old key rejected, new key succeeds.
    let cma = Keypair::new();
    let coll = Keypair::new();
    create_mint(&mut svm, &gov, &coll, COLL_DECIMALS, &cma.pubkey(), /*freeze=*/ false);
    let mk = |auth: &solana_sdk::pubkey::Pubkey| {
        init_market_ix(auth, &coll.pubkey(), MCR_BPS, DEBT_CEILING, 0, 0, BUCKET_WIDTH_BPS, 0)
    };
    let f = send(&mut svm, &[mk(&gov.pubkey())], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);
    send(&mut svm, &[mk(&successor.pubkey())], &successor, &[])
        .expect("init_market by the rotated-in admin");
}

#[test]
fn accept_with_no_pending_fails() {
    let (mut svm, _gov) = setup();
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let f = send(&mut svm, &[accept_gov_authority_ix(&rando.pubkey())], &rando, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_NO_PENDING_AUTHORITY);
}

#[test]
fn accept_by_non_pending_key_fails() {
    let (mut svm, gov) = setup();
    let successor = Keypair::new();
    let interloper = Keypair::new();
    airdrop_sol(&mut svm, &interloper.pubkey(), 10);
    send(&mut svm, &[migrate_gov_authority_ix(&gov.pubkey(), &successor.pubkey())], &gov, &[])
        .expect("propose");
    let f = send(&mut svm, &[accept_gov_authority_ix(&interloper.pubkey())], &interloper, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);
}

#[test]
fn propose_by_non_authority_fails() {
    let (mut svm, _gov) = setup();
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let f = send(&mut svm, &[migrate_gov_authority_ix(&rando.pubkey(), &rando.pubkey())], &rando, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);
}

#[test]
fn proposing_default_cancels_pending() {
    let (mut svm, gov) = setup();
    let successor = Keypair::new();
    airdrop_sol(&mut svm, &successor.pubkey(), 10);
    send(&mut svm, &[migrate_gov_authority_ix(&gov.pubkey(), &successor.pubkey())], &gov, &[])
        .expect("propose");
    send(&mut svm, &[migrate_gov_authority_ix(&gov.pubkey(), &Pubkey::default())], &gov, &[])
        .expect("cancel");
    assert_eq!(read_protocol_config(&svm).pending_gov_authority, Pubkey::default());
    // The canceled successor can no longer accept.
    let f = send(&mut svm, &[accept_gov_authority_ix(&successor.pubkey())], &successor, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_NO_PENDING_AUTHORITY);
}

#[test]
fn typo_recovery_live_authority_unaffected() {
    let (mut svm, gov) = setup();
    // Propose an unheld random key — the core two-step claim: nothing breaks.
    let typo = Pubkey::new_unique();
    send(&mut svm, &[migrate_gov_authority_ix(&gov.pubkey(), &typo)], &gov, &[]).expect("typo");
    // Live authority is untouched: a gated path still works for the current admin.
    let g2 = Keypair::new();
    send(&mut svm, &[set_guardian_ix(&gov.pubkey(), &g2.pubkey())], &gov, &[])
        .expect("live authority unaffected by a pending typo");
    // Re-propose the real key; accept succeeds.
    let successor = Keypair::new();
    airdrop_sol(&mut svm, &successor.pubkey(), 10);
    send(&mut svm, &[migrate_gov_authority_ix(&gov.pubkey(), &successor.pubkey())], &gov, &[])
        .expect("re-propose");
    send(&mut svm, &[accept_gov_authority_ix(&successor.pubkey())], &successor, &[])
        .expect("accept after typo recovery");
    assert_eq!(read_protocol_config(&svm).gov_authority, successor.pubkey());
}
