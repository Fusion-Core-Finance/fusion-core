//! In-process litesvm tests for the liquidation **bonus collar** + `claim_coll_surplus`
//! (fusion-docs.md): a liquidation seizes collateral worth at most `debt · (1 + liq_bonus_bps)`;
//! the surplus above that is returned to the borrower as `Position.coll_surplus` (held in the vault,
//! out of `ink`/stake) and withdrawn via `claim_coll_surplus`. Covers: surplus credited + claimed;
//! collar-off seizes all; underwater = no surplus; close blocked until claimed; the vault invariant
//! `vault == total_collateral + surplus_collateral + total_coll_surplus + protocol_collateral`; and the
//! governance clamp.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_coll_surplus

use fusd_integration_tests::*;
use solana_sdk::signature::{Keypair, Signer};

const E_POSITION_NOT_EMPTY: u32 = 6016;
const E_PARAM_OUT_OF_BOUNDS: u32 = 6001;
const E_NO_COLL_SURPLUS: u32 = 6035; // NoCollateralSurplus (last error variant)

/// A collared liquidation of an over-collateralized position returns the surplus, which the owner
/// then claims. At $80, B's 10 tokens ($800) back a $600 debt; the 10% collar seizes $660 worth
/// (8.25 tokens) and returns 1.75 tokens ($140) to B.
#[test]
fn collar_returns_surplus_then_owner_claims() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");
    enable_liq_collar(&mut svm, &gov, &coll, 1_000); // 10%

    // RP depositor C funds the pool so B's debt fully offsets. Victim B: 10 tokens, $600 debt.
    let c = open_borrower(&mut svm, &cma, &coll, 1_000, usd(600));
    provide_sp(&mut svm, &c, &coll, usd(600));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    // Drop to $80: B is under MCR (CR 133% < 150%) but well above water.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    assert_eq!(token_balance(&svm, &b.coll_ata), 0, "B's wallet empty before claim");

    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate B");

    // Collar: seize 8.25 tokens (worth $660 = debt·1.1), return 1.75 tokens to B.
    let surplus = whole_coll(10) - 8_250_000_000; // 1_750_000_000
    let bp = read_position(&svm, &b.position);
    assert_eq!(bp.recorded_debt, 0, "debt cleared");
    assert_eq!(bp.ink, 0, "no backing collateral left");
    assert_eq!(bp.coll_surplus, surplus, "surplus returned (held, not in ink)");
    let m = read_market(&svm, &market);
    assert_eq!(m.total_coll_surplus, surplus);
    assert_eq!(m.agg_recorded_debt, usd(600) as u128, "only C's $600 remains (B's $600 RP-offset)");
    assert_vault_invariant(&svm, &coll);

    // Owner claims: the 1.75 tokens move to B's wallet; the claim zeroes out.
    send(&mut svm, &[claim_coll_surplus_ix(&b.kp.pubkey(), &coll, &b.coll_ata)], &b.kp, &[])
        .expect("claim");
    assert_eq!(token_balance(&svm, &b.coll_ata), surplus as u64, "B received the surplus");
    assert_eq!(read_position(&svm, &b.position).coll_surplus, 0);
    assert_eq!(read_market(&svm, &market).total_coll_surplus, 0);
    assert_vault_invariant(&svm, &coll);

    // A second claim with nothing left reverts. (Expire the blockhash so the identical tx isn't
    // rejected as AlreadyProcessed before it reaches the program.)
    svm.expire_blockhash();
    let err = send(&mut svm, &[claim_coll_surplus_ix(&b.kp.pubkey(), &coll, &b.coll_ata)], &b.kp, &[])
        .expect_err("nothing to claim");
    assert_eq!(custom_code(&err), E_NO_COLL_SURPLUS);
}

/// Collar OFF (the default): a liquidation seizes the WHOLE position even when over-collateralized —
/// no surplus is returned. The contrast to the collared case above.
#[test]
fn collar_off_seizes_everything() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma); // liq_bonus_bps defaults to 0 (collar off)
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");
    let c = open_borrower(&mut svm, &cma, &coll, 1_000, usd(600));
    provide_sp(&mut svm, &c, &coll, usd(600));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate B");

    let bp = read_position(&svm, &b.position);
    assert_eq!((bp.recorded_debt, bp.ink, bp.coll_surplus), (0, 0, 0), "seize-all, no surplus");
    assert_eq!(read_market(&svm, &market).total_coll_surplus, 0);
    assert_vault_invariant(&svm, &coll);
}

/// An underwater liquidation (collateral value <= debt·(1+bonus)) returns NO surplus even with the
/// collar on — the cap is above the whole position, so it seizes everything.
#[test]
fn underwater_liquidation_returns_no_surplus() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");
    enable_liq_collar(&mut svm, &gov, &coll, 1_000);
    let c = open_borrower(&mut svm, &cma, &coll, 1_000, usd(600));
    provide_sp(&mut svm, &c, &coll, usd(600));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    // Drop to $50: B's 10 tokens are worth $500 < $600 debt — underwater. Seize all 10, surplus 0.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(50))], &gov, &[])
        .expect("price $50");
    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate B");

    let bp = read_position(&svm, &b.position);
    assert_eq!((bp.recorded_debt, bp.ink, bp.coll_surplus), (0, 0, 0), "underwater: no surplus");
    assert_eq!(read_market(&svm, &market_pda(&coll)).total_coll_surplus, 0);
    assert_vault_invariant(&svm, &coll);
}

/// `close_position` is blocked while a liquidation surplus is unclaimed (closing would strand the
/// vault collateral); claiming first unblocks it.
#[test]
fn close_blocked_until_surplus_claimed() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");
    enable_liq_collar(&mut svm, &gov, &coll, 1_000);
    let c = open_borrower(&mut svm, &cma, &coll, 1_000, usd(600));
    provide_sp(&mut svm, &c, &coll, usd(600));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate B");
    assert!(read_position(&svm, &b.position).coll_surplus > 0);

    // Close is rejected while the surplus is outstanding.
    let err = send(&mut svm, &[close_position_ix(&b.kp.pubkey(), &coll)], &b.kp, &[])
        .expect_err("close blocked");
    assert_eq!(custom_code(&err), E_POSITION_NOT_EMPTY);

    // Claim, then close succeeds. (Expire the blockhash so the second close isn't the identical tx
    // to the first, rejected client-side as AlreadyProcessed.)
    send(&mut svm, &[claim_coll_surplus_ix(&b.kp.pubkey(), &coll, &b.coll_ata)], &b.kp, &[])
        .expect("claim");
    svm.expire_blockhash();
    send(&mut svm, &[close_position_ix(&b.kp.pubkey(), &coll)], &b.kp, &[]).expect("close after claim");
}

/// Collar + PARTIAL Reactor Pool: the RP covers half the debt, the rest redistributes to a
/// co-borrower, AND a surplus is carved out — the one path that exercises the coll_r redistribution
/// add-back, the surplus carve-out, and the gas-comp base together. Vault invariant must hold.
#[test]
fn collar_with_partial_sp_and_redistribution() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");
    enable_liq_collar(&mut svm, &gov, &coll, 1_000);

    // C: RP depositor + redistribution recipient. Provides only $300 — half of B's $600 debt.
    let c = open_borrower(&mut svm, &cma, &coll, 1_000, usd(300));
    provide_sp(&mut svm, &c, &coll, usd(300));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate B");

    // seize 8.25 tokens (worth $660); coll_sp 4.125 (RP's 300/600 share), coll_r 4.125 (redistributed),
    // surplus 1.75 -> B.
    let surplus = whole_coll(10) - 8_250_000_000; // 1_750_000_000
    let m = read_market(&svm, &market);
    assert_eq!(read_position(&svm, &b.position).coll_surplus, surplus);
    assert_eq!(m.total_coll_surplus, surplus);
    assert!(m.l_art > 0, "the post-RP remainder redistributed");
    assert_eq!(m.agg_recorded_debt, usd(600) as u128, "C $300 + B's redistributed $300");
    assert_vault_invariant(&svm, &coll); // exercises coll_r add-back + surplus carve-out together

    send(&mut svm, &[claim_coll_surplus_ix(&b.kp.pubkey(), &coll, &b.coll_ata)], &b.kp, &[])
        .expect("claim");
    assert_eq!(token_balance(&svm, &b.coll_ata), surplus as u64);
    assert_vault_invariant(&svm, &coll);
}

/// Collar + the BUFFER tier: the victim is the only position (no RP, no redistribution recipient) but
/// funds the buffer with its own borrowed fUSD, so the buffer absorbs the debt while the collar still
/// carves out a surplus. No shutdown; the retained `seize_coll` stays protocol-owned; surplus claimable.
#[test]
fn collar_with_buffer_absorb() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");
    enable_liq_collar(&mut svm, &gov, &coll, 1_000);

    // B is the ONLY position: 20 tokens, $700 debt; it funds the buffer with the full $700.
    let b = open_borrower(&mut svm, &cma, &coll, 20, usd(700));
    send(&mut svm, &[fund_buffer_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(700))], &b.kp, &[])
        .expect("fund buffer $700");

    // Crash to $50: 20·$50 = $1000 vs $700 debt (CR 142% < 150%) — over the $770 collar cap, so a surplus.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(50))], &gov, &[])
        .expect("crash $50");
    liquidate(&mut svm, &gov, &coll, &b.position).expect("buffer absorbs");

    // seize 15.4 tokens (worth $770), surplus 4.6 tokens -> B; buffer burns the $700.
    let surplus = whole_coll(20) - 15_400_000_000; // 4_600_000_000
    let m = read_market(&svm, &market);
    assert!(!m.shutdown, "buffer fully covered -> no shutdown");
    assert_eq!(m.bad_debt, 0);
    assert_eq!(m.agg_recorded_debt, 0, "debt extinguished by the buffer");
    assert_eq!(buffer_balance(&svm, &coll), 0);
    assert_eq!(read_position(&svm, &b.position).coll_surplus, surplus, "surplus returned even via buffer");
    assert_eq!(m.total_coll_surplus, surplus);
    assert_vault_invariant(&svm, &coll);

    send(&mut svm, &[claim_coll_surplus_ix(&b.kp.pubkey(), &coll, &b.coll_ata)], &b.kp, &[])
        .expect("claim");
    assert_vault_invariant(&svm, &coll);
}

/// The collar caps the INTEREST-GROWN debt, not the face value: a position whose debt doubled from
/// accrued interest returns surplus computed off the realized debt. Catches a regression that fed
/// pre-`realize` debt to the collar.
#[test]
fn collar_caps_interest_grown_debt() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");
    enable_liq_collar(&mut svm, &gov, &coll, 1_000);

    // C provides $800 to the RP (covers B's grown debt). B: 10 tokens, $400 at 20%/yr.
    let c = open_borrower(&mut svm, &cma, &coll, 1_000, usd(800));
    provide_sp(&mut svm, &c, &coll, usd(800));
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(400), 2_000);

    // 5 years -> +100% -> B's debt is $800. At $100, CR = 1000/800 = 125% < 150% -> liquidatable.
    warp_unix(&mut svm, 5 * 31_536_000);
    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate B");

    // Collar on the GROWN $800: seize $880 worth = 8.8 tokens, surplus 1.2 (NOT the $400 face value,
    // which would seize only $440 = 4.4 tokens and return 5.6).
    let surplus = whole_coll(10) - 8_800_000_000; // 1_200_000_000
    assert_eq!(read_position(&svm, &b.position).coll_surplus, surplus, "collar used the realized $800 debt");
    assert_eq!(read_market(&svm, &market).total_coll_surplus, surplus);
    assert_vault_invariant(&svm, &coll);
}

/// The surplus is isolated: it survives a position REUSE, and a liquidated owner does NOT inherit a
/// redistribution that happened while its stake was 0. (The load-bearing reason coll_surplus is a
/// separate field, not left in `ink`.)
#[test]
fn surplus_isolated_across_reuse_and_redistribution() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");
    enable_liq_collar(&mut svm, &gov, &coll, 1_000);

    // C: RP depositor + redistribution recipient. V: a second victim (empty-RP redistribution source).
    let c = open_borrower(&mut svm, &cma, &coll, 1_000, usd(600));
    provide_sp(&mut svm, &c, &coll, usd(600));
    let v = open_borrower(&mut svm, &cma, &coll, 10, usd(600));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    // Liquidate B at $80 (RP offsets its $600) -> B gets a surplus, stake 0.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80");
    liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate B");
    let b_surplus = read_position(&svm, &b.position).coll_surplus;
    assert!(b_surplus > 0);

    // Now liquidate V with the RP empty -> its remainder redistributes to C, bumping l_coll/l_art
    // WHILE B's stake is 0.
    liquidate(&mut svm, &gov, &coll, &v.position).expect("liquidate V (redistributes)");
    assert!(read_market(&svm, &market_pda(&coll)).l_art > 0, "redistribution happened");

    // B re-uses its position (re-deposit at $80 + re-borrow). It must NOT inherit V's redistribution
    // (its snapshot rolls to NOW on the deposit), and its earlier surplus must be intact.
    fund_and_deposit(&mut svm, &cma, &coll, &b, whole_coll(50));
    send(&mut svm, &[borrow_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(100))], &b.kp, &[])
        .expect("re-borrow");
    let bp = read_position(&svm, &b.position);
    assert_eq!(bp.recorded_debt, usd(100) as u128, "no inherited redistributed debt — only the new borrow");
    assert_eq!(bp.coll_surplus, b_surplus, "the earlier liquidation surplus is preserved across reuse");

    // And it remains claimable.
    send(&mut svm, &[claim_coll_surplus_ix(&b.kp.pubkey(), &coll, &b.coll_ata)], &b.kp, &[])
        .expect("claim preserved surplus");
    assert_eq!(token_balance(&svm, &b.coll_ata), b_surplus);
}

/// Governance can tune the collar (`MarketParam::LiqBonus`) and the clamp rejects an over-max value.
#[test]
fn governance_tunes_collar_within_clamp() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let market = market_pda(&coll);

    assert_eq!(read_market(&svm, &market).liq_bonus_bps, 0, "collar off by default");
    init_gov_gate(&mut svm, &gov);
    gov_set_param(&mut svm, &gov, &coll, MarketParam::LiqBonus, 1_500); // 15% — in range
    assert_eq!(read_market(&svm, &market).liq_bonus_bps, 1_500);

    // Over the 20% (2000 bps) clamp -> queue rejects.
    let nonce = read_gov_gate(&svm).queue_nonce;
    let err = send(
        &mut svm,
        &[queue_param_change_ix(&gov.pubkey(), &coll, nonce, MarketParam::LiqBonus, 2_001)],
        &gov,
        &[],
    )
    .expect_err("over clamp");
    assert_eq!(custom_code(&err), E_PARAM_OUT_OF_BOUNDS);
    assert_eq!(read_market(&svm, &market).liq_bonus_bps, 1_500, "unchanged after a rejected change");
}
