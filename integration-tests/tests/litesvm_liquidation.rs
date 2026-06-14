//! In-process litesvm integration test for permissionless liquidation via the Reactor Pool
//! (fusion-docs, tier 1). Exercises the full path — provide to RP → under-collateralize a
//! borrower → liquidate → depositor claims seized collateral and withdraws the compounded deposit
//! — plus the guard reverts (healthy position, too-small pool, stale price) and pro-rata
//! distribution across two depositors.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_liquidation

use fusd_integration_tests::*;
use fusd_math::reactor_pool::DECIMAL_PRECISION;
use solana_sdk::{
    clock::Clock,
    signature::{Keypair, Signer},
};

/// Happy path: a single RP depositor absorbs a liquidation, claims all the seized collateral, and
/// withdraws their compounded deposit. Also asserts a healthy position cannot be liquidated.
#[test]
fn full_liquidation_flow() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();

    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let market = market_pda(&coll);
    let rp = reactor_pool_pda(&coll);
    let reactor_fusd_vault = reactor_fusd_vault_pda(&coll);
    let reactor_coll_vault = reactor_coll_vault_pda(&coll);
    let coll_vault = coll_vault_pda(&coll);

    // A price is required before anyone can borrow.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("set price $100");

    // Depositor D: $10k collateral, borrows $1000, provides all of it to the RP.
    let d = open_borrower(&mut svm, &coll_mint_auth, &coll, 100, usd(1_000));
    assert_eq!(token_balance(&svm, &d.fusd_ata), usd(1_000));
    provide_sp(&mut svm, &d, &coll, usd(1_000));
    assert_eq!(token_balance(&svm, &d.fusd_ata), 0, "D moved all fUSD into the RP");
    assert_eq!(token_balance(&svm, &reactor_fusd_vault), usd(1_000));
    assert_eq!(read_reactor_pool(&svm, &rp).total_deposits, usd(1_000) as u128);

    // Borrower B: $1000 collateral (10 tokens @ $100), borrows $600 (max @150% = $666.67).
    let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));
    assert_eq!(token_balance(&svm, &b.fusd_ata), usd(600));

    // Aggregate market debt = D's $1000 + B's $600; escrow holds both collateral deposits.
    assert_eq!(read_market(&svm, &market).agg_recorded_debt, (usd(1_000) + usd(600)) as u128);
    assert_eq!(token_balance(&svm, &coll_vault), whole_coll(110));

    // ---- revert: B is HEALTHY at $100 ($1000 coll vs $600 debt => 166%) ----
    {
        let f = liquidate(&mut svm, &gov, &coll, &b.position)
            .expect_err("a healthy position must not be liquidatable");
        assert_eq!(custom_code(&f), E_POSITION_HEALTHY);
    }

    // Rotate the blockhash so the real liquidation isn't a byte-identical replay of the failed
    // attempt above (litesvm dedups by signature; the blockhash is otherwise static).
    svm.expire_blockhash();

    // ---- drop to $80: B is now $800 coll vs $600 debt => max_debt $533.33 < $600 (under MCR) ----
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("set price $80");

    // ---- liquidate B (permissionless; gov stands in as an arbitrary liquidator) ----
    let debt = usd(600) as u128; // rate == RAY so debt == art
    let seized = whole_coll(10); // all of B's collateral
    liquidate(&mut svm, &gov, &coll, &b.position)
        .expect("liquidate failed");

    // Position zeroed.
    let bp = read_position(&svm, &b.position);
    assert_eq!(bp.recorded_debt, 0, "debt cleared");
    assert_eq!(bp.ink, 0, "collateral seized");
    // Aggregate market debt drops by B's debt; only D's $1000 remains.
    assert_eq!(read_market(&svm, &market).agg_recorded_debt, usd(1_000) as u128);
    // RP absorbed the debt: total_deposits -= debt and the fUSD vault burned `debt`.
    assert_eq!(read_reactor_pool(&svm, &rp).total_deposits, usd(1_000) as u128 - debt);
    assert_eq!(token_balance(&svm, &reactor_fusd_vault), usd(1_000) - usd(600));
    // Ledger matches the vault (no phantom deposits): both are exactly $400.
    assert_eq!(
        read_reactor_pool(&svm, &rp).total_deposits,
        token_balance(&svm, &reactor_fusd_vault) as u128,
        "RP ledger == fUSD vault after liquidation"
    );
    // Seized collateral moved market-escrow -> RP collateral vault.
    assert_eq!(token_balance(&svm, &reactor_coll_vault), seized);
    assert_eq!(token_balance(&svm, &coll_vault), whole_coll(100), "only D's collateral remains in escrow");

    // ---- D claims the seized collateral (sole depositor -> gets all of it) ----
    assert_eq!(token_balance(&svm, &d.coll_ata), 0);
    send(&mut svm, &[claim_reactor_gains_ix(&d.kp.pubkey(), &coll, &d.coll_ata)], &d.kp, &[])
        .expect("claim_reactor_gains failed");
    assert_eq!(token_balance(&svm, &d.coll_ata), seized, "sole depositor claims all seized collateral");
    assert_eq!(token_balance(&svm, &reactor_coll_vault), 0, "RP collateral vault drained");

    // ---- D withdraws remaining fUSD: compounded ~ $1000 - $600 = $400 (rounds down by dust) ----
    send(
        &mut svm,
        &[withdraw_from_reactor_ix(&d.kp.pubkey(), &coll, &d.fusd_ata, usd(1_000))],
        &d.kp,
        &[],
    )
    .expect("withdraw_from_reactor failed");
    let withdrawn = token_balance(&svm, &d.fusd_ata);
    // Compounded deposit = floor(1000e6 * (0.4e18 - 1) / 1e18) = 399_999_999 — exactly $400 minus a
    // single unit (Liquity's +1 loss-per-unit margin rounds the depositor down by one).
    assert_eq!(withdrawn, usd(400) - 1, "D recovers exactly $400 minus 1 dust unit");
    // That 1 unit stays in the pool as the solvency buffer; the ledger matches the vault.
    let dust = token_balance(&svm, &reactor_fusd_vault);
    assert_eq!(dust, 1, "exactly 1 dust unit remains in the RP fUSD vault");
    assert_eq!(read_reactor_pool(&svm, &rp).total_deposits, dust as u128, "ledger == vault");
}

/// When the Reactor Pool can't absorb a liquidation, there is no other position to receive the
/// redistribution, AND the insurance buffer is empty, the debt is UN-HOMED: `liquidate` does NOT
/// revert — it realizes the bad debt and trips the terminal per-market shutdown (the wind-down via
/// `urgent_redeem`). This replaces the old NoRedistributionRecipients revert: liquidation always
/// terminates. See fusion-docs.
#[test]
fn liquidation_with_no_absorber_trips_terminal_shutdown() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let rp = reactor_pool_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("set price $100");

    // B is the ONLY position, the RP is empty, and the buffer is empty: nothing can absorb it.
    let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("set price $80");

    // Liquidation SUCCEEDS (no revert): it realizes the un-homed bad debt and trips shutdown.
    liquidate(&mut svm, &gov, &coll, &b.position).expect("terminal-recovery liquidation succeeds");

    let m = read_market(&svm, &market_pda(&coll));
    assert!(m.shutdown, "the market is shut down (terminal wind-down)");
    assert_eq!(
        m.shutdown_reason,
        fusd_core::constants::SHUTDOWN_REASON_UNHOMED_BAD_DEBT,
        "reason recorded as un-homed bad debt"
    );
    assert_eq!(m.bad_debt, usd(600) as u128, "the full $600 is realized as un-homed bad debt");

    // The victim is liquidated (zeroed); its debt left agg_art; the RP was untouched.
    let p = read_position(&svm, &b.position);
    assert_eq!(p.recorded_debt, 0);
    assert_eq!(p.ink, 0);
    assert_eq!(read_reactor_pool(&svm, &rp).total_deposits, 0);
    assert_eq!(m.agg_recorded_debt, 0, "no debt remains in the aggregate");
    // The seized collateral stays protocol-owned in the market vault — now tracked in
    // `protocol_collateral` (recoverable via `sweep_protocol_collateral`), not `total_collateral`.
    assert_eq!(m.total_collateral, 0, "no live position remains to back collateral");
    assert_eq!(m.protocol_collateral, token_balance(&svm, &coll_vault_pda(&coll)), "retained == vault");
    assert_vault_invariant(&svm, &coll);
}

/// Liquidation must price against a fresh oracle: a stale cached price blocks it even when the
/// position is under-collateralized and the pool is large enough.
#[test]
fn liquidation_reverts_on_stale_price() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let market = market_pda(&coll);
    let rp = reactor_pool_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("set price $100");
    let d = open_borrower(&mut svm, &coll_mint_auth, &coll, 100, usd(1_000));
    provide_sp(&mut svm, &d, &coll, usd(1_000));
    let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));

    // Drop B under MCR at a fresh $80 price.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("set price $80");

    // Pin the preconditions so the ONLY thing that can block the liquidation below is staleness
    // (the handler checks staleness BEFORE the health and pool-size guards): the pool can cover the
    // debt, and the position is genuinely under-MCR ($800 collateral, max_debt @150% = $533 < $600).
    assert!(read_reactor_pool(&svm, &rp).total_deposits >= usd(600) as u128, "pool can absorb");
    {
        let m = read_market(&svm, &market);
        let p = read_position(&svm, &b.position);
        let coll_value = (p.ink as u128) * m.spot / fusd_math::RAY;
        let max_debt = coll_value * 10_000 / (MCR_BPS as u128);
        assert!(p.recorded_debt > max_debt, "B is under-MCR (debt {} > max_debt {})", p.recorded_debt, max_debt);
    }

    // Warp far past MAX_PRICE_STALENESS_SLOTS (250) WITHOUT refreshing the price -> stale.
    let mut clk: Clock = svm.get_sysvar();
    clk.slot += 300;
    svm.set_sysvar::<Clock>(&clk);
    svm.warp_to_slot(clk.slot);

    let f = liquidate(&mut svm, &gov, &coll, &b.position)
        .expect_err("a stale price must block liquidation");
    assert_eq!(custom_code(&f), E_STALE_PRICE);

    // Positive control: refresh the SAME $80 price at the current slot. The staleness revert is now
    // gone — but because the price just RECOVERED from a stall, the on-resume grace window is armed,
    // so liquidation is blocked by GRACE rather than staleness (the two oracle-freshness gates in
    // sequence). This proves the prior revert was the staleness guard, not some other failure.
    svm.expire_blockhash();
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("refresh price $80");
    let g = liquidate(&mut svm, &gov, &coll, &b.position)
        .expect_err("fresh again, but the on-resume grace window now applies");
    assert_eq!(custom_code(&g), E_LIQUIDATION_GRACE_PERIOD);

    // Once the grace window elapses (the keeper keeps the price fresh through it), the SAME position
    // liquidates — confirming the path is otherwise sound.
    crank_past_resume_grace(&mut svm, &gov, &coll, spot_for_usd(80));
    liquidate(&mut svm, &gov, &coll, &b.position)
        .expect("liquidation succeeds once fresh AND past the grace window");
    let bp = read_position(&svm, &b.position);
    assert_eq!(bp.recorded_debt, 0);
    assert_eq!(bp.ink, 0);
}

/// Two depositors split a liquidation's seized collateral pro-rata to their deposits (O(1)
/// product-sum), end to end.
#[test]
fn two_depositors_share_seized_collateral_pro_rata() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let rp = reactor_pool_pda(&coll);
    let reactor_coll_vault = reactor_coll_vault_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("set price $100");

    // D1 provides $600, D2 provides $400 -> pool $1000 (60/40), both snapshot the fresh pool.
    let d1 = open_borrower(&mut svm, &coll_mint_auth, &coll, 100, usd(600));
    provide_sp(&mut svm, &d1, &coll, usd(600));
    let d2 = open_borrower(&mut svm, &coll_mint_auth, &coll, 100, usd(400));
    provide_sp(&mut svm, &d2, &coll, usd(400));
    assert_eq!(read_reactor_pool(&svm, &rp).total_deposits, usd(1_000) as u128);

    // Borrower owes $600 on 10 tokens; drop to $80 and liquidate.
    let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("set price $80");
    liquidate(&mut svm, &gov, &coll, &b.position)
        .expect("liquidate failed");

    let seized = whole_coll(10);
    // Debt side (signed by the market PDA path, independent of the per-depositor split): the
    // position is cleared, the RP burned the $600 debt, and the seized collateral landed in the RP
    // vault. Asserting this guards against a regression that breaks the burn / agg_art / zeroing
    // while leaving the collateral split intact.
    let bp = read_position(&svm, &b.position);
    assert_eq!(bp.recorded_debt, 0, "debt cleared");
    assert_eq!(bp.ink, 0, "collateral seized");
    assert_eq!(read_reactor_pool(&svm, &rp).total_deposits, (usd(1_000) - usd(600)) as u128);
    assert_eq!(token_balance(&svm, &reactor_fusd_vault_pda(&coll)), usd(1_000) - usd(600));
    assert_eq!(token_balance(&svm, &reactor_coll_vault), seized);

    // Each depositor claims pro-rata, exactly: the $1000 pool divides the 10-token seizure evenly,
    // so D1 (60%) gets 6 tokens, D2 (40%) gets 4, summing to the whole seizure with ZERO dust.
    send(&mut svm, &[claim_reactor_gains_ix(&d1.kp.pubkey(), &coll, &d1.coll_ata)], &d1.kp, &[])
        .expect("D1 claim failed");
    send(&mut svm, &[claim_reactor_gains_ix(&d2.kp.pubkey(), &coll, &d2.coll_ata)], &d2.kp, &[])
        .expect("D2 claim failed");

    let g1 = token_balance(&svm, &d1.coll_ata);
    let g2 = token_balance(&svm, &d2.coll_ata);
    assert_eq!(g1, whole_coll(6), "D1 gets exactly 6 tokens");
    assert_eq!(g2, whole_coll(4), "D2 gets exactly 4 tokens");
    assert_eq!(g1 + g2, seized, "collateral fully conserved (zero dust)");
    assert_eq!(token_balance(&svm, &reactor_coll_vault), 0, "RP collateral vault fully drained");
}

/// A liquidation whose debt exactly equals the pool fully drains it, which rolls the epoch and
/// wipes every depositor's compounded deposit — the riskiest path in the P/S machinery. The wiped
/// depositor still claims (essentially) all of the seized collateral.
#[test]
fn liquidation_full_drain_rolls_epoch() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let rp = reactor_pool_pda(&coll);
    let reactor_fusd_vault = reactor_fusd_vault_pda(&coll);
    let reactor_coll_vault = reactor_coll_vault_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("set price $100");

    // RP holds EXACTLY the borrower's debt -> the liquidation drains the whole pool.
    let d = open_borrower(&mut svm, &coll_mint_auth, &coll, 100, usd(600));
    provide_sp(&mut svm, &d, &coll, usd(600));
    let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("set price $80");
    liquidate(&mut svm, &gov, &coll, &b.position)
        .expect("liquidate failed");

    // Full-pool drain: epoch advances, scale resets, P resets to DECIMAL_PRECISION, deposits zeroed.
    let pool = read_reactor_pool(&svm, &rp);
    assert_eq!(pool.epoch, 1, "epoch rolled");
    assert_eq!(pool.scale, 0, "scale reset");
    assert_eq!(pool.p, DECIMAL_PRECISION, "P reset to 1e18");
    assert_eq!(pool.total_deposits, 0, "all deposits absorbed");
    assert_eq!(token_balance(&svm, &reactor_fusd_vault), 0, "entire fUSD deposit burned");
    let seized = whole_coll(10);
    assert_eq!(token_balance(&svm, &reactor_coll_vault), seized, "seized collateral held for claim");

    // The wiped depositor still claims (essentially) all the seized collateral; a unit or two stays
    // as the error-feedback residual.
    send(&mut svm, &[claim_reactor_gains_ix(&d.kp.pubkey(), &coll, &d.coll_ata)], &d.kp, &[])
        .expect("claim failed");
    let claimed = token_balance(&svm, &d.coll_ata);
    assert!(
        claimed <= seized && seized - claimed <= 2,
        "wiped depositor still claims ~all seized collateral: got {claimed}"
    );

    // Their compounded fUSD deposit is gone — nothing left to withdraw.
    send(&mut svm, &[withdraw_from_reactor_ix(&d.kp.pubkey(), &coll, &d.fusd_ata, usd(600))], &d.kp, &[])
        .expect("withdraw failed");
    assert_eq!(token_balance(&svm, &d.fusd_ata), 0, "deposit wiped: nothing to withdraw");
}
