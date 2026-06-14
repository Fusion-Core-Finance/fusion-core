//! **Black-swan boundary-replay** — the on-chain check on the economic model's worst cells.
//!
//! The `blackswan` economic model (an f64 sweep over MCR × keeper-latency × oracle-stall × RP-depth)
//! produced four findings. They are only worth trusting if the REAL integer program behaves the way
//! the model assumes at the worst cells. This suite replays those cells through the actual
//! `liquidate` instruction and asserts the program reproduces each finding exactly:
//!
//!   sim finding                                          replayed here as
//!   ──────────────────────────────────────────────────  ──────────────────────────────────────────
//!   #1 latency is everything; prompt liquidation         `prompt_liquidation_at_mcr_crossing_is_loss_free`
//!      (≤crossing) forms ZERO bad debt
//!   #2 bad debt == the underwater gap, and grows as       `bad_debt_equals_underwater_gap_and_grows_with_depth`
//!      the execution price falls (= longer latency)
//!   #3 an oracle STALL amplifies bad debt — it turns a    `oracle_stall_converts_a_safe_liquidation_into_bad_debt`
//!      would-be-safe liquidation into a lossy one
//!   #4 the Reactor Pool suppresses the reflexive        `deep_sp_suppresses_the_cascade_an_empty_sp_triggers`
//!      cascade an empty pool triggers
//!
//! "Bad debt" in the model = the realized loss the system socializes when a position is liquidated
//! while underwater (collateral worth < debt). On-chain that loss lands on Reactor-Pool equity:
//! the RP burns `debt` fUSD but receives collateral worth less than `debt`. We measure it directly
//! as `debt_burned − mark_to_market(seized_collateral)` and confirm it matches the gap, is zero at
//! break-even, and is created out of thin air by an oracle stall — validating the staleness-halt
//! design and the still-[Planned] on-resume grace window.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_blackswan_replay

use fusd_integration_tests::*;
use fusd_math::RAY;
use solana_sdk::{
    clock::Clock,
    signature::{Keypair, Signer},
};

/// Mark-to-market value (fUSD-native) of `coll_native` collateral units at RAY-scaled `spot`.
fn coll_value(coll_native: u64, spot: u128) -> u128 {
    (coll_native as u128) * spot / RAY
}

/// Run ONE full-offset liquidation of a standard underwater borrower at `price_usd`, and return the
/// realized RP loss in fUSD-native (`debt_burned − value_of_seized_collateral`; negative == the RP
/// came out ahead). Setup is identical at every price — only the crash depth changes — so the
/// returned loss is a clean function of how far the price fell before the keeper fired (the on-chain
/// proxy for keeper latency / crash severity).
///
/// Along the way it asserts the per-run invariants the model relies on: the liquidation SUCCEEDS
/// even underwater (the RP backstop absorbs it, no revert), the victim is cleared, the RP burns
/// exactly the debt, and fUSD supply drops by exactly that burn (so fUSD held elsewhere stays fully
/// backed — the loss is socialized to RP depositors, not the peg).
fn underwater_liquidation_loss(price_usd: u128) -> i128 {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let market = market_pda(&coll);
    let rp = reactor_pool_pda(&coll);
    let reactor_coll_vault = reactor_coll_vault_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");

    // Deep RP: $1000 (≥ B's $600), so the offset is always FULL — no tier-2 — isolating the RP's
    // own loss. D's own position stays healthy at every tested crash price ($5000+ coll vs $1000).
    let d = open_borrower(&mut svm, &coll_mint_auth, &coll, 100, usd(1_000));
    provide_sp(&mut svm, &d, &coll, usd(1_000));
    // Borrower B: 10 tokens, $600 debt. Healthy at $100 (166%); under-MCR at every price below $90.
    let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));

    let supply_before = mint_supply(&svm, &fusd_mint_pda());
    let agg_before = read_market(&svm, &market).agg_recorded_debt;
    let reactor_coll_before = token_balance(&svm, &reactor_coll_vault);

    // The crash: price drops to `price_usd`, the keeper fires HERE (the lower the price, the later
    // it fired). A fresh print at the new price — staleness is exercised separately.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(price_usd))], &gov, &[])
        .unwrap_or_else(|_| panic!("price ${price_usd}"));
    let spot = read_market(&svm, &market).spot;

    liquidate(&mut svm, &gov, &coll, &b.position).expect("underwater liquidation must still succeed");

    // Victim cleared.
    let bp = read_position(&svm, &b.position);
    assert_eq!((bp.recorded_debt, bp.ink), (0, 0), "victim fully liquidated at ${price_usd}");

    // Full offset: the RP burned exactly the debt; agg_art fell by exactly the debt; the seized
    // collateral (all 10 tokens, gas-comp is 0 in the default market) moved into the RP vault.
    let burned = supply_before - mint_supply(&svm, &fusd_mint_pda());
    assert_eq!(burned, usd(600), "RP burned exactly B's debt at ${price_usd}");
    assert_eq!(agg_before - read_market(&svm, &market).agg_recorded_debt, usd(600) as u128, "agg_art retired");
    let seized = token_balance(&svm, &reactor_coll_vault) - reactor_coll_before;
    assert_eq!(seized, whole_coll(10), "RP seized all of B's collateral at ${price_usd}");
    // fUSD held outside the RP (e.g. B kept its borrowed $600) is untouched and still fully backed.
    assert_eq!(read_reactor_pool(&svm, &rp).total_deposits, (usd(1_000) - usd(600)) as u128);

    // The loss the RP socialized = debt it burned − value of the collateral it got for it.
    usd(600) as i128 - coll_value(seized, spot) as i128
}

/// **Finding #1 — prompt liquidation forms zero bad debt.** A position liquidated right as it
/// crosses MCR still holds collateral worth ≈ MCR × debt > debt, so the RP receives MORE value than
/// the debt it burns: the system banks a surplus, never bad debt. This is the model's claim that
/// "bad debt is born entirely in the gap between crossing MCR and getting liquidated" — at the
/// crossing, the gap is zero.
#[test]
fn prompt_liquidation_at_mcr_crossing_is_loss_free() {
    // $80 is just past B's MCR crossing ($90): $800 collateral vs $600 debt (CR 133%, under the 150%
    // MCR so it IS liquidatable, but still far above water). The RP burns $600 and receives $800 —
    // a $200 surplus to depositors, NOT bad debt.
    let loss = underwater_liquidation_loss(80);
    assert!(loss < 0, "prompt liquidation banks a surplus, not bad debt (loss = {loss})");
    assert_eq!(loss, -(usd(200) as i128), "RP gains exactly $200 of collateral over the debt burned");
}

/// **Findings #1 + #2 — bad debt == the underwater gap, zero at break-even, growing with depth.**
/// Sweeping the execution price (the on-chain proxy for keeper latency) reproduces the model's
/// frontier: zero loss until the collateral is worth less than the debt, then a loss that equals the
/// shortfall exactly and grows as the price falls further.
#[test]
fn bad_debt_equals_underwater_gap_and_grows_with_depth() {
    // Break-even: 10 tokens × $60 = $600 == debt. The RP burns $600 and receives $600 — zero loss.
    assert_eq!(underwater_liquidation_loss(60), 0, "no bad debt at break-even (CR 100%)");

    // Below break-even the loss is exactly the underwater gap (debt − collateral value):
    assert_eq!(underwater_liquidation_loss(50), usd(100) as i128, "$50: gap = $600 − $500 = $100");
    assert_eq!(underwater_liquidation_loss(40), usd(200) as i128, "$40: gap = $600 − $400 = $200");

    // And it is monotone in crash depth (= latency): the later the liquidation, the more bad debt.
    let ladder = [
        underwater_liquidation_loss(80), // −200 (surplus)
        underwater_liquidation_loss(60), //    0 (break-even)
        underwater_liquidation_loss(50), // +100
        underwater_liquidation_loss(40), // +200
    ];
    for w in ladder.windows(2) {
        assert!(w[1] > w[0], "bad debt grows monotonically with crash depth: {ladder:?}");
    }
}

/// **Finding #3 — an oracle STALL amplifies bad debt.** The model showed a staleness halt turns
/// survivable cells dangerous: liquidations can't fire while the feed is frozen, so the backlog
/// clears at the lower trough price once it resumes. Replayed here: the SAME borrower that a prompt
/// keeper would have liquidated at a $200 SURPLUS ($80) is instead blocked by the stale-price gate,
/// and only clears once the feed resumes at the $50 trough — now at a $100 LOSS. The stall created
/// $100 of bad debt out of nothing.
#[test]
fn oracle_stall_converts_a_safe_liquidation_into_bad_debt() {
    // Counterfactual (no stall): a keeper acting at the first $80 print banks a $200 surplus.
    let loss_prompt = underwater_liquidation_loss(80);
    assert_eq!(loss_prompt, -(usd(200) as i128), "prompt keeper at $80 → $200 surplus, zero bad debt");

    // Now the stalled path on a fresh market.
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
    let market = market_pda(&coll);
    let reactor_coll_vault = reactor_coll_vault_pda(&coll);

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
        .expect("price $100");
    let d = open_borrower(&mut svm, &coll_mint_auth, &coll, 100, usd(1_000));
    provide_sp(&mut svm, &d, &coll, usd(1_000));
    let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));

    // The crash's first oracle print is $80 — B is under-MCR and could be liquidated loss-free RIGHT
    // NOW. But the keeper is slow, and before it acts the feed FREEZES.
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[])
        .expect("price $80 (last good print before the stall)");

    // Oracle stalls: warp far past MAX_PRICE_STALENESS_SLOTS (250) with no new print. The keeper
    // finally tries — and is BLOCKED, because the protocol refuses to liquidate against a stale
    // price. This is the halt the model assumes; bad debt accrues in the real economy meanwhile.
    let mut clk: Clock = svm.get_sysvar();
    clk.slot += 300;
    svm.set_sysvar::<Clock>(&clk);
    svm.warp_to_slot(clk.slot);
    let blocked = liquidate(&mut svm, &gov, &coll, &b.position)
        .expect_err("a stale price must block liquidation during the stall");
    assert_eq!(custom_code(&blocked), E_STALE_PRICE);

    // The feed resumes — but the real price has fallen to $50 while it was frozen. The resume ARMS
    // the on-resume grace window (the mitigation this test motivated): the backlog can't clear
    // instantly at the trough, so B gets a window to cure rather than being liquidated on a price it
    // had no chance to react to during the halt.
    svm.expire_blockhash();
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(50))], &gov, &[])
        .expect("feed resumes at the $50 trough");
    let g = liquidate(&mut svm, &gov, &coll, &b.position)
        .expect_err("the on-resume grace window blocks the instant trough liquidation");
    assert_eq!(custom_code(&g), E_LIQUIDATION_GRACE_PERIOD);

    // B does NOT use the window to cure, so once it elapses the loss still realizes at the trough —
    // the grace window gives a fair chance, it does not erase bad debt for a borrower who ignores it.
    // (The cure path is proven in litesvm_oracle_grace::borrower_can_cure_during_the_grace_window.)
    crank_past_resume_grace(&mut svm, &gov, &coll, spot_for_usd(50));
    let spot = read_market(&svm, &market).spot;
    let reactor_coll_before = token_balance(&svm, &reactor_coll_vault);
    liquidate(&mut svm, &gov, &coll, &b.position).expect("clears once past the grace window");

    let seized = token_balance(&svm, &reactor_coll_vault) - reactor_coll_before;
    let loss_stalled = usd(600) as i128 - coll_value(seized, spot) as i128;
    assert_eq!(loss_stalled, usd(100) as i128, "an uncured stalled borrower still realizes $100 of bad debt");

    // The stall converted a $200 surplus into a $100 loss for an uncured borrower: bad debt created
    // by the halt. The grace window (asserted above) is what gives the borrower the chance to avoid it.
    assert!(loss_stalled > loss_prompt, "the stall amplified bad debt: {loss_prompt} → {loss_stalled}");
}

/// **Finding #4 — the Reactor Pool suppresses the reflexive cascade.** When the pool is too small,
/// an underwater liquidation REDISTRIBUTES the shortfall onto the surviving borrowers, which can
/// drag a previously-healthy position below MCR — a second-order liquidation (cascade). A deep pool
/// absorbs the whole hit as depositor equity instead, leaving survivors untouched. Replayed as a
/// symmetric pair on identical positions: empty pool → cascade; deep pool → no cascade.
#[test]
fn deep_sp_suppresses_the_cascade_an_empty_sp_triggers() {
    // ---- Empty RP: the cascade FIRES ----
    {
        let mut svm = new_svm();
        let gov = Keypair::new();
        airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
        let coll_mint_auth = Keypair::new();
        let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);
        let market = market_pda(&coll);

        send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
            .expect("price $100");

        // Survivor C: 19 tokens, $600 debt — comfortably healthy at $100 (316%) AND still healthy at
        // the $50 crash on its own ($950 vs $600 = 158% > 150% MCR). The RP is left EMPTY.
        let c = open_borrower(&mut svm, &coll_mint_auth, &coll, 19, usd(600));
        // Victim B: 10 tokens, $600 debt.
        let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));

        // Crash to $50: B underwater ($500 vs $600); C still healthy on its own.
        send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(50))], &gov, &[])
            .expect("price $50");
        // Empty pool → B's entire $600 debt + 10 tokens redistribute onto C (the only other position).
        liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate B (full redistribution)");

        // Touch C so it realizes the redistribution, then show it has been dragged UNDER MCR.
        fund_and_deposit(&mut svm, &coll_mint_auth, &coll, &c, whole_coll(1));
        let m = read_market(&svm, &market);
        let cp = read_position(&svm, &c.position);
        // C now holds 19 + 10 redistributed + 1 touched = 30 tokens, and 600 + 600 = $1200 debt.
        let coll_v = coll_value(cp.ink as u64, m.spot);
        let max_debt = coll_v * 10_000 / (MCR_BPS as u128);
        assert!(
            cp.recorded_debt > max_debt,
            "redistribution pushed the healthy survivor UNDER MCR (debt {} > max_debt {max_debt}): cascade",
            cp.recorded_debt
        );

        // The cascade is real on-chain: liquidating C now passes the health gate (it is NOT rejected
        // as healthy). With B gone and BOTH the RP and the insurance buffer empty, C's debt is
        // UN-HOMED — so liquidate no longer reverts: it realizes the bad debt and trips the terminal
        // shutdown (the `urgent_redeem` wind-down). The cascade's terminal danger is now a clean,
        // tracked wind-down instead of a stuck revert.
        liquidate(&mut svm, &gov, &coll, &c.position)
            .expect("the dragged-down survivor is liquidatable; its un-homed debt trips shutdown");
        let m2 = read_market(&svm, &market);
        assert!(m2.shutdown, "the cascade's terminal wall is a clean shutdown, not a revert");
        assert_eq!(m2.shutdown_reason, fusd_core::constants::SHUTDOWN_REASON_UNHOMED_BAD_DEBT);
        assert!(m2.bad_debt > 0, "C's unabsorbable debt is realized as un-homed bad debt");
    }

    // ---- Deep RP: the cascade is SUPPRESSED ----
    {
        let mut svm = new_svm();
        let gov = Keypair::new();
        airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
        let coll_mint_auth = Keypair::new();
        let coll = bootstrap_market(&mut svm, &gov, &coll_mint_auth);

        send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[])
            .expect("price $100");

        // Identical survivor C (19 tokens, $600). This time a depositor D funds the RP with $1000 —
        // enough to fully offset B's $600 debt, so NOTHING redistributes.
        let c = open_borrower(&mut svm, &coll_mint_auth, &coll, 19, usd(600));
        let d = open_borrower(&mut svm, &coll_mint_auth, &coll, 100, usd(1_000));
        provide_sp(&mut svm, &d, &coll, usd(1_000));
        let b = open_borrower(&mut svm, &coll_mint_auth, &coll, 10, usd(600));

        send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(50))], &gov, &[])
            .expect("price $50");
        // Full RP offset → no redistribution → C is never touched. The RP eats the $100 underwater
        // loss as depositor equity, shielding the borrowers.
        liquidate(&mut svm, &gov, &coll, &b.position).expect("liquidate B (full RP offset)");

        // C is untouched and still healthy ($950 vs $600 at $50) — the cascade did not propagate.
        let f = liquidate(&mut svm, &gov, &coll, &c.position)
            .expect_err("the deep pool shielded C — it must remain healthy");
        assert_eq!(
            custom_code(&f),
            E_POSITION_HEALTHY,
            "deep RP suppressed the cascade: C stays above MCR, rejected as healthy"
        );
    }
}
