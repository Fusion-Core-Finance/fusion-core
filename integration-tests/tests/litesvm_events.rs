//! The event stream. Every state-changing instruction emits an Anchor event so
//! indexers/keepers/PoR monitoring get history without account-diffing. Since the event-CPI
//! migration, events ride the #[event_cpi] self-CPI transport — inner
//! instructions, immune to RPC log truncation — and the harness `events_of`/`single_event`
//! decode them from `meta.inner_instructions`. These tests pin the load-bearing payloads:
//! the position lifecycle, the liquidation waterfall breakdown, redemption totals, shutdown,
//! governance (with the prev/new forensic trail), and the interest/keeper split.
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_events

use anchor_lang::ToAccountMetas;
use fusd_core::events::*;
use fusd_core::state::MarketParam;
use fusd_integration_tests::*;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const ONE_YEAR: i64 = 365 * 86_400;

#[test]
fn position_lifecycle_emits_events() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    // open_borrower drives open(5% rate) → deposit(10) → borrow($300); assert on the borrow's event.
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, 0, 500);
    let meta = send(&mut svm, &[borrow_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(300))], &b.kp, &[])
        .expect("borrow");
    let ev: PositionUpdated = single_event(&meta);
    assert_eq!(ev.collateral_mint, coll);
    assert_eq!(ev.owner, b.kp.pubkey());
    assert_eq!(ev.op, POSITION_OP_BORROW);
    assert_eq!(ev.amount, usd(300));
    assert_eq!(ev.ink, whole_coll(10));
    assert_eq!(ev.recorded_debt, usd(300) as u128, "post-op state in the event");
    assert_eq!(ev.user_rate_bps, 500);
    assert_eq!(ev.bucket, 50); // 500 bps / width 10

    // repay emits with the repaid amount and the post-op debt.
    let meta = send(&mut svm, &[repay_ix(&b.kp.pubkey(), &coll, &b.fusd_ata, usd(100))], &b.kp, &[])
        .expect("repay");
    let ev: PositionUpdated = single_event(&meta);
    assert_eq!(ev.op, POSITION_OP_REPAY);
    assert_eq!(ev.amount, usd(100));
    assert_eq!(ev.recorded_debt, usd(200) as u128);
}

#[test]
fn liquidation_emits_waterfall_breakdown() {
    // The litesvm_liquidation happy-path scenario: a deep RP fully absorbs the liquidation.
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma); // collar/gas-comp/bond all off
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");

    let d = open_borrower(&mut svm, &cma, &coll, 1_000, usd(2_000));
    provide_sp(&mut svm, &d, &coll, usd(2_000));
    let b = open_borrower(&mut svm, &cma, &coll, 10, usd(600));

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(80))], &gov, &[]).expect("p80");
    let liq = Keypair::new();
    airdrop_sol(&mut svm, &liq.pubkey(), 10);
    let meta = liquidate(&mut svm, &liq, &coll, &b.position).expect("liquidate");

    let ev: LiquidationEvent = single_event(&meta);
    assert_eq!(ev.collateral_mint, coll);
    assert_eq!(ev.position, b.position);
    assert_eq!(ev.owner, b.kp.pubkey());
    assert_eq!(ev.liquidator, liq.pubkey());
    assert_eq!(ev.debt, usd(600) as u128);
    assert_eq!(ev.seized_collateral, whole_coll(10), "collar off ⇒ seize-all");
    assert_eq!(ev.gas_comp, 0);
    assert_eq!(ev.coll_surplus, 0);
    // The whole debt offset by the RP — the waterfall conservation in the event payload.
    assert_eq!(ev.reactor_offset, usd(600) as u128);
    assert_eq!(ev.redistributed, 0);
    assert_eq!(ev.buffer_absorbed, 0);
    assert_eq!(ev.backstop_absorbed, 0);
    assert_eq!(ev.unhomed, 0);
    assert_eq!(
        ev.reactor_offset + ev.redistributed + ev.buffer_absorbed + ev.backstop_absorbed + ev.unhomed,
        ev.debt
    );
    assert_eq!(ev.spot, spot_for_usd(80));
    // No bad debt ⇒ no BadDebtEvent, no ShutdownEvent.
    assert!(events_of::<BadDebtEvent>(&meta).is_empty());
    assert!(events_of::<ShutdownEvent>(&meta).is_empty());

    // Event-CPI migration pins: single transport — the event arrived via the
    // inner-instruction self-CPI (decoded above) and NO log-based "Program data:" line exists.
    assert_no_log_events(&meta);
}

#[test]
fn liquidate_account_count_stays_within_budget() {
    // #[event_cpi] adds exactly 2 read-only accounts (event_authority + program) to every
    // emitting instruction. Liquidate — the fattest instruction — must stay well under the
    // 64-account / 128-lock transaction limits (the Jupiter >64 DoS class).
    let liq_metas = fusd_core::accounts::Liquidate {
        liquidator: Pubkey::new_unique(),
        collateral_mint: Pubkey::new_unique(),
        market: Pubkey::new_unique(),
        position: Pubkey::new_unique(),
        reactor_pool: Pubkey::new_unique(),
        epoch_to_scale_to_sum: Pubkey::new_unique(),
        market_coll_vault: Pubkey::new_unique(),
        reactor_fusd_vault: Pubkey::new_unique(),
        reactor_coll_vault: Pubkey::new_unique(),
        fusd_mint: Pubkey::new_unique(),
        liquidator_collateral_ata: Pubkey::new_unique(),
        redemption_bitmap: Pubkey::new_unique(),
        insurance_buffer: Pubkey::new_unique(),
        buffer_fusd_vault: Pubkey::new_unique(),
        backstop: Some(Pubkey::new_unique()),
        backstop_fusd_vault: Some(Pubkey::new_unique()),
        token_program: Pubkey::new_unique(),
        event_authority: Pubkey::new_unique(),
        program: Pubkey::new_unique(),
    }
    .to_account_metas(None);
    assert!(
        liq_metas.len() <= 20,
        "Liquidate grew to {} accounts — re-check the ALT/lock budget",
        liq_metas.len()
    );
}

#[test]
fn redemption_emits_totals() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma); // redemption fee 0

    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    let b = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 300); // bucket 30 (lowest)
    let r = open_borrower_rate(&mut svm, &cma, &coll, 10, usd(300), 500);

    let meta = send(
        &mut svm,
        &[redeem_ix(&r.kp.pubkey(), &coll, &r.fusd_ata, &r.coll_ata, &[b.position], usd(100))],
        &r.kp,
        &[],
    )
    .expect("redeem");

    let ev: RedemptionEvent = single_event(&meta);
    assert_eq!(ev.collateral_mint, coll);
    assert_eq!(ev.redeemer, r.kp.pubkey());
    assert_eq!(ev.fusd_burned, usd(100));
    // $100 at $100/token = 1 whole token, fee 0.
    assert_eq!(ev.collateral_paid, whole_coll(1));
    assert_eq!(ev.fee_collateral, 0);
    assert_eq!(ev.bucket, 30);
    assert_eq!(ev.candidates, 1);
}

#[test]
fn shutdown_emits_reason() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    let _a = open_borrower(&mut svm, &cma, &coll, 10, usd(400));

    warp_slots(&mut svm, fusd_core::constants::SHUTDOWN_ORACLE_STALENESS_SLOTS + 1);
    let meta = send(&mut svm, &[shutdown_ix(&gov.pubkey(), &coll)], &gov, &[]).expect("shutdown");

    let ev: ShutdownEvent = single_event(&meta);
    assert_eq!(ev.collateral_mint, coll);
    assert_eq!(ev.reason, fusd_core::constants::SHUTDOWN_REASON_ORACLE_FAILURE);
}

#[test]
fn governance_lifecycle_emits_events() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), 0)], &gov, &[]).expect("gate");

    let meta = send(
        &mut svm,
        &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::RedemptionFee, 123)],
        &gov,
        &[],
    )
    .expect("queue");
    let ev: ParamChangeQueued = single_event(&meta);
    assert_eq!(ev.market, market_pda(&coll));
    assert_eq!(ev.nonce, 0);
    assert_eq!(ev.param, MarketParam::RedemptionFee);
    assert_eq!(ev.value, 123);

    let meta = send(&mut svm, &[execute_param_change_ix(&gov.pubkey(), &coll, 0)], &gov, &[])
        .expect("execute");
    let ev: ParamChangeExecuted = single_event(&meta);
    assert_eq!((ev.nonce, ev.prev_value, ev.value), (0, 0, 123), "forensic Prv/New trail");
    assert_eq!(ev.param, MarketParam::RedemptionFee);

    // Prv/New across a SECOND change of the same param (prev now non-default), and across an
    // unclamped param arm (RateLimitCap) — the forensic-trail coverage.
    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 1, MarketParam::RedemptionFee, 77)], &gov, &[])
        .expect("queue 2nd fee change");
    let meta = send(&mut svm, &[execute_param_change_ix(&gov.pubkey(), &coll, 1)], &gov, &[])
        .expect("execute 2nd fee change");
    let ev: ParamChangeExecuted = single_event(&meta);
    assert_eq!((ev.prev_value, ev.value), (123, 77), "prev reflects the live pre-change value");

    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 2, MarketParam::RateLimitCap, 5_000)], &gov, &[])
        .expect("queue cap");
    let meta = send(&mut svm, &[execute_param_change_ix(&gov.pubkey(), &coll, 2)], &gov, &[])
        .expect("execute cap");
    let ev: ParamChangeExecuted = single_event(&meta);
    assert_eq!((ev.param, ev.prev_value, ev.value), (MarketParam::RateLimitCap, 0, 5_000));

    // Two-step authority handoff emits propose + accept events.
    let new_auth = Keypair::new();
    airdrop_sol(&mut svm, &new_auth.pubkey(), 10);
    let meta = send(&mut svm, &[migrate_inbound_authority_ix(&gov.pubkey(), &new_auth.pubkey())], &gov, &[])
        .expect("propose");
    let ev: InboundAuthorityProposed = single_event(&meta);
    assert_eq!((ev.current, ev.pending), (gov.pubkey(), new_auth.pubkey()));

    let meta = send(&mut svm, &[accept_inbound_authority_ix(&new_auth.pubkey())], &new_auth, &[])
        .expect("accept");
    let ev: InboundAuthorityMigrated = single_event(&meta);
    assert_eq!((ev.previous, ev.new_authority), (gov.pubkey(), new_auth.pubkey()));
}

#[test]
fn refresh_emits_interest_split() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot_for_usd(100))], &gov, &[]).expect("p100");
    let _a = open_borrower_rate(&mut svm, &cma, &coll, 50, usd(1_000), 1_000); // 10%/yr

    warp_unix(&mut svm, ONE_YEAR);
    let meta = send(&mut svm, &[refresh_market_ix(&coll)], &gov, &[]).expect("refresh");
    let ev: InterestMinted = single_event(&meta);
    assert_eq!(ev.collateral_mint, coll);
    assert_eq!(ev.amount, usd(100), "$100 of interest after a year at 10%");
    assert_eq!(ev.to_buffer, usd(100), "keeper reward off ⇒ all to the buffer");
    assert_eq!(ev.to_backstop, 0, "no backstop accounts ⇒ no cut routed");
    assert_eq!(ev.keeper_cut, 0);
    assert_eq!(ev.unminted_remaining, 0);

    // No new interest ⇒ no event (the early return mints nothing).
    svm.expire_blockhash();
    let meta = send(&mut svm, &[refresh_market_ix(&coll)], &gov, &[]).expect("refresh again");
    assert!(events_of::<InterestMinted>(&meta).is_empty(), "no mint ⇒ no event");
}
