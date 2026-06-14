//! On-chain vault sufficiency reconciliation.
//!
//! Every collateral-moving handler now ends with `vault >= total_collateral + surplus_collateral
//! + total_coll_surplus + protocol_collateral` (strictly `>=`, never `==`). The load-bearing
//! property pinned here is the DONATION NO-BRICK regression: a permissionless 1-unit donation to
//! the market vault must never block any flow — an absolute on-chain `==` would have let it
//! permanently brick liquidation and redemption with no admin recourse. The under-funded
//! direction is unit-tested in `reconcile.rs` (it is unreachable through honest instructions —
//! which is the point of the assert). Requires the dev-oracle `.so`.

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

#[test]
fn donation_never_bricks_any_flow() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("dev_set_price");

    let borrower = open_borrower(&mut svm, &cma, &coll, 10, usd(400));
    let victim = open_borrower(&mut svm, &cma, &coll, 10, usd(645)); // CR ~155%
    let funder = open_borrower(&mut svm, &cma, &coll, 100, usd(2_000));
    provide_sp(&mut svm, &funder, &coll, usd(2_000));

    // DONATE collateral straight into the market vault (permissionless — anyone can do this).
    let donor = open_borrower(&mut svm, &cma, &coll, 5, 0);
    fund_collateral(&mut svm, &cma, &coll, &donor, whole_coll(1)); // ATA holds the donation
    let donate = spl_token::instruction::transfer(
        &SPL_TOKEN_ID,
        &donor.coll_ata,
        &coll_vault_pda(&coll),
        &donor.kp.pubkey(),
        &[],
        whole_coll(1),
    )
    .unwrap();
    send(&mut svm, &[donate], &donor.kp, &[]).expect("donation lands");

    // Every reconciled flow still succeeds with the surplus vault balance present.
    fund_and_deposit(&mut svm, &cma, &coll, &borrower, whole_coll(1)); // deposit
    send(
        &mut svm,
        &[withdraw_ix(&borrower.kp.pubkey(), &coll, &borrower.coll_ata, whole_coll(1))],
        &borrower.kp,
        &[],
    )
    .expect("withdraw with donation present"); // withdraw

    // Ordered redemption.
    send(
        &mut svm,
        &[redeem_ix(
            &borrower.kp.pubkey(),
            &coll,
            &borrower.fusd_ata,
            &borrower.coll_ata,
            &[victim.position, borrower.position],
            usd(50),
        )],
        &borrower.kp,
        &[],
    )
    .expect("redeem with donation present");

    // Liquidation (drop the price so the victim is under MCR).
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(85))], &gov, &[])
        .expect("price drop");
    let liquidator = Keypair::new();
    airdrop_sol(&mut svm, &liquidator.pubkey(), 10);
    liquidate(&mut svm, &liquidator, &coll, &victim.position)
        .expect("liquidate with donation present");

    // The donation shows up as vault slack: vault > tracked sum, by exactly the donated unit.
    let mk = read_market(&svm, &market_pda(&coll));
    let vault = token_balance(&svm, &coll_vault_pda(&coll));
    let tracked = mk.total_collateral
        + mk.surplus_collateral as u128
        + mk.total_coll_surplus as u128
        + mk.protocol_collateral as u128;
    assert_eq!(vault as u128, tracked + whole_coll(1) as u128, "donation = untracked slack");
}

#[test]
fn donation_never_bricks_shutdown_paths() {
    // Same property through the wind-down: claim_coll_surplus and urgent_redeem stay open with
    // a donated surplus in the vault.
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    init_gov_gate(&mut svm, &gov);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("dev_set_price");
    // Collar ON so a liquidation books a claimable surplus.
    gov_set_param(&mut svm, &gov, &coll, MarketParam::LiqBonus, 1_000);

    let victim = open_borrower(&mut svm, &cma, &coll, 10, usd(645));
    let funder = open_borrower(&mut svm, &cma, &coll, 100, usd(2_000));
    provide_sp(&mut svm, &funder, &coll, usd(1_500)); // keep $500 in the ATA for urgent_redeem

    // Donate, then liquidate under the collar (surplus accrues to the victim).
    let donor = open_borrower(&mut svm, &cma, &coll, 5, 0);
    fund_collateral(&mut svm, &cma, &coll, &donor, whole_coll(1)); // ATA holds the donation
    let donate = spl_token::instruction::transfer(
        &SPL_TOKEN_ID,
        &donor.coll_ata,
        &coll_vault_pda(&coll),
        &donor.kp.pubkey(),
        &[],
        whole_coll(1),
    )
    .unwrap();
    send(&mut svm, &[donate], &donor.kp, &[]).expect("donation lands");

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(85))], &gov, &[])
        .expect("price drop");
    let liquidator = Keypair::new();
    airdrop_sol(&mut svm, &liquidator.pubkey(), 10);
    liquidate(&mut svm, &liquidator, &coll, &victim.position).expect("collared liquidation");
    assert!(read_position(&svm, &victim.position).coll_surplus > 0, "surplus booked");

    // claim_coll_surplus succeeds with the donation present.
    send(
        &mut svm,
        &[claim_coll_surplus_ix(&victim.kp.pubkey(), &coll, &victim.coll_ata)],
        &victim.kp,
        &[],
    )
    .expect("claim_coll_surplus with donation present");

    // Crash + shutdown → urgent_redeem succeeds with the donation present.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(15))], &gov, &[])
        .expect("crash");
    send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown");
    send(
        &mut svm,
        &[urgent_redeem_ix(
            &funder.kp.pubkey(),
            &coll,
            &funder.fusd_ata,
            &funder.coll_ata,
            &[funder.position],
            usd(100),
        )],
        &funder.kp,
        &[],
    )
    .expect("urgent_redeem with donation present");
}

#[test]
fn governance_recipient_must_not_alias_the_vault() {
    // The two governance recovery instructions are the only token recipients in the program not
    // pinned to a signer's authority — a fat-finger passing the vault itself would self-transfer
    // (no-op) while the counter is debited, silently stranding value as vault slack.
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 10_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    init_gov_gate(&mut svm, &gov);

    let vault = coll_vault_pda(&coll);
    let f = send(&mut svm, &[withdraw_surplus_ix(&gov.pubkey(), &coll, &vault, 1)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_RECIPIENT);
    let f =
        send(&mut svm, &[sweep_protocol_collateral_ix(&gov.pubkey(), &coll, &vault, 1)], &gov, &[])
            .unwrap_err();
    assert_eq!(custom_code(&f), E_INVALID_RECIPIENT);
}
