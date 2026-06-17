//! `GovernanceGate` + the fUSD-owned timelock: QUEUE → delay → EXECUTE (and migrate / cancel /
//! clamps). Keypair-gated here (the Squads-vault-PDA path that QUEUES through this gate is
//! `tests/squads-gov-poc.ts`). The timing is exercised by warping the SVM clock.

use anchor_lang::{InstructionData, ToAccountMetas};
use fusd_core::instructions::init_market::InitMarketArgs;
use fusd_core::instructions::init_protocol::InitProtocolArgs;
use fusd_integration_tests::*;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};

const TL: i64 = 3_600; // 1h timelock for the timing tests

/// Bootstrap a market (gov_authority = gov) and create the gate with inbound_authority = gov.
fn setup(timelock: i64) -> (litesvm::LiteSVM, Keypair, Pubkey) {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), timelock)], &gov, &[])
        .expect("init_governance_gate");
    (svm, gov, coll)
}

#[test]
fn init_protocol_gated_on_upgrade_authority() {
    // `init_protocol` may only be called by the program's upgrade authority, so the
    // deterministic `[b"config"]` singleton can't be front-run to capture governance.
    let mut svm = new_svm();
    let deployer = Keypair::new(); // the legitimate upgrade authority
    let attacker = Keypair::new();
    airdrop_sol(&mut svm, &deployer.pubkey(), 1_000);
    airdrop_sol(&mut svm, &attacker.pubkey(), 1_000);

    // The program is deployed with `deployer` as the upgrade authority.
    set_program_upgrade_authority(&mut svm, &deployer.pubkey());

    // The attacker front-runs init_protocol with itself as gov/guardian → rejected by the gate
    // (its payer is not the upgrade authority).
    let f = send(&mut svm, &[init_protocol_ix(&attacker.pubkey())], &attacker, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);
    assert!(svm.get_account(&config_pda()).is_none(), "config not created by the attacker");

    // The real upgrade authority initializes successfully.
    send(&mut svm, &[init_protocol_ix(&deployer.pubkey())], &deployer, &[]).expect("legit init");
    let cfg = read_protocol_config(&svm);
    assert_eq!(cfg.gov_authority, deployer.pubkey());
}

#[test]
fn queue_delay_execute_applies_after_eta() {
    let (mut svm, gov, coll) = setup(TL);
    let m = market_pda(&coll);
    assert_eq!(read_market(&svm, &m).redemption_fee_bps, 0);

    // QUEUE (nonce 0) — does not apply immediately.
    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::RedemptionFee, 123)], &gov, &[])
        .expect("queue");
    assert_eq!(read_market(&svm, &m).redemption_fee_bps, 0, "not applied at queue");
    assert_eq!(read_gov_gate(&svm).queue_nonce, 1, "nonce advanced");

    // EXECUTE before the timelock elapses → rejected.
    let f = send(&mut svm, &[execute_param_change_ix(&gov.pubkey(), &coll, 0)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_TIMELOCK_NOT_ELAPSED);

    // Warp past eta → permissionless execute applies it, and the op account is reclaimed.
    warp_unix(&mut svm, TL + 1);
    send(&mut svm, &[execute_param_change_ix(&gov.pubkey(), &coll, 0)], &gov, &[]).expect("execute");
    assert_eq!(read_market(&svm, &m).redemption_fee_bps, 123, "applied after eta");
    assert_eq!(lamports(&svm, &timelock_pda(0)), 0, "queued op closed on execute");
}

#[test]
fn anyone_can_execute_after_eta() {
    let (mut svm, gov, coll) = setup(TL);
    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::Mcr, 16_000)], &gov, &[])
        .expect("queue");
    warp_unix(&mut svm, TL + 1);
    // A random, non-governance signer executes — execution is permissionless once the delay passes.
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    send(&mut svm, &[execute_param_change_ix(&rando.pubkey(), &coll, 0)], &rando, &[]).expect("permissionless execute");
    assert_eq!(read_market(&svm, &market_pda(&coll)).mcr_bps, 16_000);
}

#[test]
fn queue_validates_clamps_and_authority() {
    let (mut svm, gov, coll) = setup(0);

    // out-of-clamp values are rejected at QUEUE (fail fast).
    let f = send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::RedemptionFee, 501)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
    let f = send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::Mcr, 9_999)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
    // MCR above the real upper clamp (MAX_MCR_BPS = 300%) is rejected — the bound is no longer the
    // meaningless u16::MAX (655%) that let a captured governance retroactively mass-liquidate.
    let over = fusd_core::constants::MAX_MCR_BPS as u64 + 1;
    let f = send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::Mcr, over)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);

    // LiqGasComp above MAX_LIQ_GAS_COMP_BPS trips the per-field bound BEFORE the relational branch
    // (validate_param runs first), so this is E_PARAM_OUT_OF_BOUNDS, not E_PARAM_COMBINATION_INVALID.
    let over = fusd_core::constants::MAX_LIQ_GAS_COMP_BPS as u64 + 1;
    let f = send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::LiqGasComp, over)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);

    // RateAdjustCooldown above MAX_RATE_ADJUST_COOLDOWN_SECS is rejected at queue (validate_param).
    let over = fusd_core::constants::MAX_RATE_ADJUST_COOLDOWN_SECS as u64 + 1;
    let f = send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::RateAdjustCooldown, over)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);

    // a non-inbound-authority signer cannot queue.
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let f = send(&mut svm, &[queue_param_change_ix(&rando.pubkey(), &coll, 0, MarketParam::RedemptionFee, 100)], &rando, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);

    assert_eq!(read_gov_gate(&svm).queue_nonce, 0, "no op was created");
}

#[test]
fn cancel_withdraws_a_queued_op() {
    let (mut svm, gov, coll) = setup(TL);
    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::RedemptionFee, 200)], &gov, &[])
        .expect("queue");
    // governance cancels before it can execute.
    send(&mut svm, &[cancel_param_change_ix(&gov.pubkey(), 0)], &gov, &[]).expect("cancel");
    assert_eq!(lamports(&svm, &timelock_pda(0)), 0, "op closed by cancel");

    // even after the delay, the cancelled op cannot be executed, and the market is unchanged.
    warp_unix(&mut svm, TL + 1);
    assert!(send(&mut svm, &[execute_param_change_ix(&gov.pubkey(), &coll, 0)], &gov, &[]).is_err());
    assert_eq!(read_market(&svm, &market_pda(&coll)).redemption_fee_bps, 0);
}

#[test]
fn migrate_repoints_the_inbound_authority() {
    let (mut svm, gov, coll) = setup(0);
    let new_auth = Keypair::new();
    airdrop_sol(&mut svm, &new_auth.pubkey(), 10);

    // only the current inbound authority can PROPOSE a successor.
    let f = send(&mut svm, &[migrate_inbound_authority_ix(&new_auth.pubkey(), &new_auth.pubkey())], &new_auth, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);

    // Step 1: gov proposes new_auth. The live authority does NOT move yet (two-step).
    send(&mut svm, &[migrate_inbound_authority_ix(&gov.pubkey(), &new_auth.pubkey())], &gov, &[]).expect("propose");
    assert_eq!(read_gov_gate(&svm).inbound_authority, gov.pubkey(), "still gov until accept");
    assert_eq!(read_gov_gate(&svm).pending_inbound_authority, new_auth.pubkey());

    // The pending successor must sign the accept; a stranger cannot accept on its behalf.
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let f = send(&mut svm, &[accept_inbound_authority_ix(&rando.pubkey())], &rando, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);
    // gov (the OLD authority) still controls the gate until the handoff completes.
    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::RedemptionFee, 10)], &gov, &[])
        .expect("old authority still queues pre-accept");
    send(&mut svm, &[cancel_param_change_ix(&gov.pubkey(), 0)], &gov, &[]).expect("cancel the probe op");

    // Step 2: new_auth accepts → the live authority finally moves and the pending slot clears.
    send(&mut svm, &[accept_inbound_authority_ix(&new_auth.pubkey())], &new_auth, &[]).expect("accept");
    assert_eq!(read_gov_gate(&svm).inbound_authority, new_auth.pubkey());
    assert_eq!(read_gov_gate(&svm).pending_inbound_authority, Pubkey::default());

    // the OLD authority can no longer queue; the NEW one can.
    let f = send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 1, MarketParam::RedemptionFee, 50)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);
    send(&mut svm, &[queue_param_change_ix(&new_auth.pubkey(), &coll, 1, MarketParam::RedemptionFee, 50)], &new_auth, &[])
        .expect("new authority queues");
    send(&mut svm, &[execute_param_change_ix(&new_auth.pubkey(), &coll, 1)], &new_auth, &[]).expect("execute");
    assert_eq!(read_market(&svm, &market_pda(&coll)).redemption_fee_bps, 50);
}

#[test]
fn typo_proposal_cannot_brick_governance() {
    // A propose to an UNHELD key (a typo) never takes effect: the live authority is unchanged and
    // can re-propose. The two-step handshake is exactly what removes the one-step brick.
    let (mut svm, gov, coll) = setup(0);
    let typo = Keypair::new().pubkey(); // a key nobody signs for

    send(&mut svm, &[migrate_inbound_authority_ix(&gov.pubkey(), &typo)], &gov, &[]).expect("propose typo");
    assert_eq!(read_gov_gate(&svm).inbound_authority, gov.pubkey(), "live authority unchanged");

    // gov still fully controls the gate.
    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::RedemptionFee, 33)], &gov, &[])
        .expect("gov still queues after a typo'd proposal");
    send(&mut svm, &[execute_param_change_ix(&gov.pubkey(), &coll, 0)], &gov, &[]).expect("execute");
    assert_eq!(read_market(&svm, &market_pda(&coll)).redemption_fee_bps, 33);

    // and can replace the bad proposal (or cancel it by proposing default).
    send(&mut svm, &[migrate_inbound_authority_ix(&gov.pubkey(), &Pubkey::default())], &gov, &[]).expect("cancel pending");
    assert_eq!(read_gov_gate(&svm).pending_inbound_authority, Pubkey::default());
}

#[test]
fn multiple_ops_queue_independently() {
    let (mut svm, gov, coll) = setup(0);
    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::RedemptionFee, 111)], &gov, &[]).expect("queue 0");
    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 1, MarketParam::LiqGasComp, 222)], &gov, &[]).expect("queue 1");
    assert_eq!(read_gov_gate(&svm).queue_nonce, 2);

    send(&mut svm, &[execute_param_change_ix(&gov.pubkey(), &coll, 1)], &gov, &[]).expect("execute 1");
    send(&mut svm, &[execute_param_change_ix(&gov.pubkey(), &coll, 0)], &gov, &[]).expect("execute 0");
    let mk = read_market(&svm, &market_pda(&coll));
    assert_eq!(mk.redemption_fee_bps, 111);
    assert_eq!(mk.liq_gas_comp_bps, 222);
}

// ---------------------------------------------------------------------------------------------
// Relational config validation: values individually inside the compile-time
// clamps but JOINTLY lethal are rejected at queue, at execute (against the LIVE market — the
// order-independence defense), and at init_market (no inverted-at-birth market).
// ---------------------------------------------------------------------------------------------

#[test]
fn queue_rejects_mcr_below_scr() {
    let (mut svm, gov, coll) = setup(0);
    // 10_500 passes the per-field clamp (>= MIN_MCR_BPS) but inverts MCR vs SCR (default 11_000):
    // every position in [MCR, SCR) would be healthy yet leave TCR < SCR ⇒ anyone could trigger the
    // irreversible terminal shutdown.
    let f = send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::Mcr, 10_500)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_COMBINATION_INVALID);
    // MCR == SCR is the documented safe boundary.
    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, 0, MarketParam::Mcr, 11_000)], &gov, &[])
        .expect("MCR == SCR accepted");
}

#[test]
fn queue_rejects_sp_negative_gas_comp_combo() {
    let (mut svm, gov, coll) = setup(0);
    // Enable a small collar first (valid: 10_100 <= 15_000, product fine at gas 0).
    gov_set_param(&mut svm, &gov, &coll, MarketParam::LiqBonus, 100);
    // Now max gas comp would make every boundary liquidation RP-negative:
    // seizable 10_100 bps · (10_000 − 1_000) = 90.9M < 1e8 ⇒ rejected at queue.
    let nonce = read_gov_gate(&svm).queue_nonce;
    let f = send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, nonce, MarketParam::LiqGasComp, 1_000)], &gov, &[])
        .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_COMBINATION_INVALID);
}

#[test]
fn jointly_conflicting_queued_ops_are_order_independent() {
    // Two ops, each valid against the live config AT QUEUE TIME, that jointly conflict: whichever
    // executes second is rejected by the execute-time re-check, regardless of order. Governance
    // cancels the loser and re-queues in the safe order.
    let (mut svm, gov, coll) = setup(0);
    gov_set_param(&mut svm, &gov, &coll, MarketParam::LiqBonus, 2_000); // live: bonus 20%, gas 0

    let n0 = read_gov_gate(&svm).queue_nonce;
    // Op A: LiqBonus → 100. Valid vs live gas 0 (10_100 · 10_000 ≥ 1e8).
    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, n0, MarketParam::LiqBonus, 100)], &gov, &[])
        .expect("queue A");
    // Op B: LiqGasComp → 1_000. Valid vs live bonus 2_000 (12_000 · 9_000 = 108M ≥ 1e8).
    send(&mut svm, &[queue_param_change_ix(&gov.pubkey(), &coll, n0 + 1, MarketParam::LiqGasComp, 1_000)], &gov, &[])
        .expect("queue B");

    // Execute A first ⇒ live becomes (bonus 100, gas 0). B is now jointly invalid and the
    // execute-time re-check rejects it.
    send(&mut svm, &[execute_param_change_ix(&gov.pubkey(), &coll, n0)], &gov, &[]).expect("execute A");
    let f = send(&mut svm, &[execute_param_change_ix(&gov.pubkey(), &coll, n0 + 1)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_COMBINATION_INVALID);

    // The rejected op stays cancellable (rent reclaimed) — no wedged queue.
    send(&mut svm, &[cancel_param_change_ix(&gov.pubkey(), n0 + 1)], &gov, &[]).expect("cancel B");
    assert_eq!(lamports(&svm, &timelock_pda(n0 + 1)), 0, "B closed by cancel");
    let mk = read_market(&svm, &market_pda(&coll));
    assert_eq!(mk.liq_bonus_bps, 100);
    assert_eq!(mk.liq_gas_comp_bps, 0);
}

#[test]
fn init_market_rejects_inverted_and_unfundable_configs() {
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    set_program_upgrade_authority(&mut svm, &gov.pubkey());
    send(&mut svm, &[init_protocol_ix(&gov.pubkey())], &gov, &[]).expect("init_protocol");
    let cma = Keypair::new();

    let try_init = |svm: &mut litesvm::LiteSVM, gov: &Keypair, cma: &Keypair, mcr: u16, bonus: u16, gas: u16| {
        let mint = Keypair::new();
        create_mint(svm, gov, &mint, COLL_DECIMALS, &cma.pubkey(), false);
        let ix = Instruction {
            program_id: fusd_core::ID,
            accounts: fusd_core::accounts::InitMarket {
                authority: gov.pubkey(),
                config: config_pda(),
                collateral_mint: mint.pubkey(),
                market: market_pda(&mint.pubkey()),
                collateral_vault: coll_vault_pda(&mint.pubkey()),
                redemption_bitmap: redemption_bitmap_pda(&mint.pubkey()),
                token_program: SPL_TOKEN_ID,
                system_program: solana_sdk::system_program::ID,
                rent: solana_sdk::sysvar::rent::ID,
                event_authority: event_authority_pda(),
                program: fusd_core::ID,
            }
            .to_account_metas(None),
            data: fusd_core::instruction::InitMarket {
                args: InitMarketArgs {
                    mcr_bps: mcr,
                    debt_ceiling: DEBT_CEILING,
                    reserve_lamports: 0,
                    liq_gas_comp_bps: gas,
                    liq_bonus_bps: bonus,
                    bucket_width_bps: BUCKET_WIDTH_BPS,
                    redemption_fee_bps: 0,
                },
            }
            .data(),
        };
        send(svm, &[ix], gov, &[])
    };

    // MCR 105% < default SCR 110% — the inverted-at-birth market init_market accepted pre-fix.
    let f = try_init(&mut svm, &gov, &cma, 10_500, 0, 0).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_COMBINATION_INVALID);
    // Unfundable collar at birth: 100% + 20% bonus > 115% MCR.
    let f = try_init(&mut svm, &gov, &cma, 11_500, 2_000, 0).unwrap_err();
    assert_eq!(custom_code(&f), E_COLLAR_EXCEEDS_MCR);
    // MCR == SCR boundary accepted.
    try_init(&mut svm, &gov, &cma, 11_000, 0, 0).expect("MCR == SCR accepted at init");

    // Per-field MCR clamp at init (distinct from the gov-queue validate_param path): under
    // MIN_MCR_BPS and over MAX_MCR_BPS are rejected by the inlined require! at init_market, BEFORE
    // the relational validate_market_config check (bonus/gas 0 keep the relational checks inert).
    let f = try_init(&mut svm, &gov, &cma, 9_999, 0, 0).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
    let f = try_init(&mut svm, &gov, &cma, fusd_core::constants::MAX_MCR_BPS + 1, 0, 0).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);

    // Per-field liq_bonus clamp at init: bonus 2_001 > MAX_LIQ_BONUS_BPS trips the per-field require!
    // (mcr == MAX_MCR_BPS keeps the collar check passing so the per-field bound is the sole rejector).
    let f = try_init(&mut svm, &gov, &cma, 30_000, 2_001, 0).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);

    // RP-solvency product unfundable at birth: small collar + high gas comp.
    // seizable = min(10_000+100, 11_000) = 10_100; 10_100 * (10_000-1_000) = 90.9M < 1e8.
    // (mcr 11_000 >= scr 11_000 so NOT (iii); 10_000+100=10_100 <= 11_000 so NOT (i); (ii) rejects.)
    let f = try_init(&mut svm, &gov, &cma, 11_000, 100, 1_000).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_COMBINATION_INVALID);
}

#[test]
fn init_gate_clamps_and_authority() {
    // non-gov cannot create the gate; out-of-clamp timelock is rejected.
    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let cma = Keypair::new();
    let coll = bootstrap_market(&mut svm, &gov, &cma);
    let _ = coll;

    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let f = send(&mut svm, &[init_governance_gate_ix(&rando.pubkey(), &rando.pubkey(), 0)], &rando, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_UNAUTHORIZED);

    let too_long = fusd_core::constants::MAX_GOV_TIMELOCK_SECS + 1;
    let f = send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), too_long)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);

    // default inbound_authority would permanently brick param governance — rejected. (Ordered before
    // the valid creation: the gate is an init-only PDA, so the reject must run while it doesn't exist.)
    let f = send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &Pubkey::default(), TL)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
    assert!(svm.get_account(&gov_gate_pda()).is_none(), "bricking init must not create the gate");

    // negative (under-bound) timelock rejected.
    let f = send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), -1)], &gov, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);

    // valid creation works.
    send(&mut svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), TL)], &gov, &[]).expect("init gate");
    assert_eq!(read_gov_gate(&svm).timelock_secs, TL);
}

#[test]
fn accept_inbound_with_no_pending_fails() {
    // A fresh gate has pending_inbound_authority == Pubkey::default() (no handoff in flight), so an
    // accept hits the NoPendingAuthority branch before the Unauthorized check.
    let (mut svm, _gov, _coll) = setup(0);
    let rando = Keypair::new();
    airdrop_sol(&mut svm, &rando.pubkey(), 10);
    let f = send(&mut svm, &[accept_inbound_authority_ix(&rando.pubkey())], &rando, &[]).unwrap_err();
    assert_eq!(custom_code(&f), E_NO_PENDING_AUTHORITY);
}

#[test]
fn init_protocol_rejects_default_gov_authority() {
    // gov_authority == default has no signer ⇒ it could never drive migrate_gov_authority, bricking
    // every gov-gated bootstrap. init_protocol rejects it. The legit upgrade authority is the payer so
    // the gate at the program_data constraint passes and we reach the line-81 require!.
    let mut svm = new_svm();
    let auth = Keypair::new();
    airdrop_sol(&mut svm, &auth.pubkey(), 1_000);
    set_program_upgrade_authority(&mut svm, &auth.pubkey());

    let f = send(
        &mut svm,
        &[init_protocol_args_ix(&auth.pubkey(), InitProtocolArgs { gov_authority: Pubkey::default(), guardian: auth.pubkey() })],
        &auth,
        &[],
    )
    .unwrap_err();
    assert_eq!(custom_code(&f), E_PARAM_OUT_OF_BOUNDS);
    assert!(svm.get_account(&config_pda()).is_none(), "bricking init must not create config");

    // Deliberate asymmetry: a default GUARDIAN is accepted ("no guardian yet", repairable via the
    // gov-gated set_guardian).
    send(
        &mut svm,
        &[init_protocol_args_ix(&auth.pubkey(), InitProtocolArgs { gov_authority: auth.pubkey(), guardian: Pubkey::default() })],
        &auth,
        &[],
    )
    .expect("default guardian accepted");
    assert_eq!(read_protocol_config(&svm).gov_authority, auth.pubkey());
}
