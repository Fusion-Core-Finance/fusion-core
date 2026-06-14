//! In-process litesvm integration test for the fUSD CDP flow (open → deposit → borrow → repay →
//! withdraw) plus the four guard reverts. Loads the real `.so` (built with `--features dev-oracle`)
//! and drives it through the shared harness (`fusd_integration_tests`).
//!
//!     anchor build -- --features dev-oracle
//!     cargo test -p fusd-integration-tests --test litesvm_cdp

use fusd_integration_tests::*;
use solana_sdk::{clock::Clock, signature::Keypair, signature::Signer};
use spl_token::state::Mint;
use solana_sdk::program_pack::Pack;

#[test]
fn full_cdp_flow() {
    let mut svm = new_svm();

    // gov is the governance authority (also the dev-oracle authority) + fee payer; the user here
    // is `gov` for simplicity. coll_mint_auth holds the collateral mint authority.
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    let coll_mint_auth = Keypair::new();

    // ======================= init_protocol =======================
    set_program_upgrade_authority(&mut svm, &gov.pubkey());
    send(&mut svm, &[init_protocol_ix(&gov.pubkey())], &gov, &[]).expect("init_protocol failed");
    {
        let acct = svm.get_account(&config_pda()).unwrap();
        let cfg = <fusd_core::state::ProtocolConfig as anchor_lang::AccountDeserialize>::try_deserialize(
            &mut acct.data.as_slice(),
        )
        .unwrap();
        assert_eq!(cfg.gov_authority, gov.pubkey());
        assert_eq!(cfg.fusd_mint, fusd_mint_pda());
        let mint_acct = svm.get_account(&fusd_mint_pda()).unwrap();
        let fusd = Mint::unpack(&mint_acct.data).unwrap();
        assert_eq!(fusd.decimals, FUSD_DECIMALS);
        assert!(fusd.freeze_authority.is_none());
    }

    // ======================= revert: init_market with a freeze-authority mint =======================
    {
        let bad_mint = Keypair::new();
        create_mint(&mut svm, &gov, &bad_mint, COLL_DECIMALS, &coll_mint_auth.pubkey(), /*freeze=*/ true);
        let ix = init_market_ix(&gov.pubkey(), &bad_mint.pubkey(), MCR_BPS, DEBT_CEILING, 0, 0, BUCKET_WIDTH_BPS, 0);
        let f = send(&mut svm, &[ix], &gov, &[]).expect_err("freeze-authority mint must be rejected");
        assert_eq!(custom_code(&f), E_COLLATERAL_HAS_FREEZE_AUTHORITY);
    }

    // ======================= init_market (good collateral) =======================
    let coll_mint = Keypair::new();
    create_mint(&mut svm, &gov, &coll_mint, COLL_DECIMALS, &coll_mint_auth.pubkey(), /*freeze=*/ false);
    let coll = coll_mint.pubkey();
    let market = market_pda(&coll);
    let coll_vault = coll_vault_pda(&coll);
    {
        send(
            &mut svm,
            &[init_market_ix(&gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, 0, 0, BUCKET_WIDTH_BPS, 0)],
            &gov,
            &[],
        )
        .expect("init_market failed");
        let mk = read_market(&svm, &market);
        assert_eq!(mk.collateral_mint, coll);
        assert_eq!(mk.mcr_bps, MCR_BPS);
        assert_eq!(mk.spot, 0);
        assert_eq!(mk.agg_recorded_debt, 0);
        assert_eq!(mk.agg_weighted_debt_sum, 0);
    }

    // The borrower is `gov` here for simplicity.
    let user = &gov;
    let position = position_pda(&coll, &user.pubkey());
    let user_coll_ata =
        create_ata_and_fund(&mut svm, &gov, &user.pubkey(), &coll, Some(&coll_mint_auth), whole_coll(10));
    let user_fusd_ata = create_ata_and_fund(&mut svm, &gov, &user.pubkey(), &fusd_mint_pda(), None, 0);

    // ======================= open_position =======================
    {
        send(&mut svm, &[open_position_ix(&user.pubkey(), &coll, 500)], user, &[])
            .expect("open_position failed");
        let p = read_position(&svm, &position);
        assert_eq!(p.ink, 0);
        assert_eq!(p.recorded_debt, 0);
    }

    // ======================= deposit 10 collateral tokens =======================
    let deposit_amt = whole_coll(10); // 10_000_000_000 native
    {
        send(&mut svm, &[deposit_ix(&user.pubkey(), &coll, &user_coll_ata, deposit_amt)], user, &[])
            .expect("deposit failed");
        assert_eq!(read_position(&svm, &position).ink, deposit_amt);
        assert_eq!(token_balance(&svm, &coll_vault), deposit_amt);
        assert_eq!(token_balance(&svm, &user_coll_ata), 0);
    }

    // ======================= revert: borrow with NO price set =======================
    {
        let ix = borrow_ix(&user.pubkey(), &coll, &user_fusd_ata, 1);
        let f = send(&mut svm, &[ix], user, &[]).expect_err("borrow without a price must fail");
        assert_eq!(custom_code(&f), E_ORACLE_UNAVAILABLE);
    }

    // ======================= dev_set_price ($100/token => RAY/10) =======================
    let spot = spot_for_usd(100);
    {
        send(&mut svm, &[dev_set_price_ix(&gov.pubkey(), &coll, spot)], &gov, &[])
            .expect("dev_set_price failed");
        assert_eq!(read_market(&svm, &market).spot, spot);
    }
    // collateral_value = 10e9 * 0.1 = 1_000_000_000 fUSD-native ($1000);
    // max_debt @150% = floor(1_000_000_000 * 10_000 / 15_000) = 666_666_666.

    // ======================= revert: borrow beyond MCR =======================
    {
        let ix = borrow_ix(&user.pubkey(), &coll, &user_fusd_ata, usd(700)); // $700 > $666.67
        let f = send(&mut svm, &[ix], user, &[]).expect_err("borrow beyond MCR must fail");
        assert_eq!(custom_code(&f), E_BELOW_MIN_COLLATERAL_RATIO);
    }

    // ======================= borrow $600 (ok) =======================
    let borrow_amt = usd(600); // 600_000_000
    {
        send(&mut svm, &[borrow_ix(&user.pubkey(), &coll, &user_fusd_ata, borrow_amt)], user, &[])
            .expect("borrow failed");
        assert_eq!(token_balance(&svm, &user_fusd_ata), borrow_amt);
        // rate == RAY so art == minted amount exactly.
        assert_eq!(read_position(&svm, &position).recorded_debt, borrow_amt as u128);
        assert_eq!(read_market(&svm, &market).agg_recorded_debt, borrow_amt as u128);
    }

    // ======================= revert: withdraw below MCR =======================
    // $600 debt @150% needs >= $900 collateral = 9 tokens; withdrawing 2 (-> 8 tokens = $800) fails.
    {
        let ix = withdraw_ix(&user.pubkey(), &coll, &user_coll_ata, whole_coll(2));
        let f = send(&mut svm, &[ix], user, &[]).expect_err("withdraw below MCR must fail");
        assert_eq!(custom_code(&f), E_BELOW_MIN_COLLATERAL_RATIO);
    }

    // ======================= repay $600 (full) =======================
    {
        send(&mut svm, &[repay_ix(&user.pubkey(), &coll, &user_fusd_ata, borrow_amt)], user, &[])
            .expect("repay failed");
        assert_eq!(token_balance(&svm, &user_fusd_ata), 0);
        assert_eq!(read_position(&svm, &position).recorded_debt, 0);
        assert_eq!(read_market(&svm, &market).agg_recorded_debt, 0);
    }

    // ======================= withdraw all collateral (now debt-free) =======================
    {
        send(&mut svm, &[withdraw_ix(&user.pubkey(), &coll, &user_coll_ata, deposit_amt)], user, &[])
            .expect("withdraw failed");
        assert_eq!(read_position(&svm, &position).ink, 0);
        assert_eq!(token_balance(&svm, &coll_vault), 0);
        assert_eq!(token_balance(&svm, &user_coll_ata), deposit_amt);
    }

    // clock control APIs exist (not load-bearing for the 0% scenario above):
    let mut clk: Clock = svm.get_sysvar();
    clk.slot += 10;
    clk.unix_timestamp += 5;
    svm.set_sysvar::<Clock>(&clk);
    svm.warp_to_slot(clk.slot);
}

/// Token-2022 collateral is STRUCTURALLY rejected:
/// the legacy-only Anchor typing (`Account<token::Mint>` under `Program<Token>`) fails account
/// validation with `AccountOwnedByWrongProgram` (3007) before the handler runs — no extension
/// scan needed, because the strictest allowlist is the empty set. This regression pins the gate
/// so a future `token_interface` refactor cannot silently reopen T22 onboarding without
/// consciously revisiting the onboarding decision.
#[test]
fn init_market_rejects_token_2022_mint() {
    use solana_sdk::account::Account as SolanaAccount;
    use solana_sdk::pubkey::Pubkey;
    use std::str::FromStr;

    const ANCHOR_ACCOUNT_OWNED_BY_WRONG_PROGRAM: u32 = 3007;
    let token_2022_id = Pubkey::from_str("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb").unwrap();

    let mut svm = new_svm();
    let gov = Keypair::new();
    airdrop_sol(&mut svm, &gov.pubkey(), 1_000);
    set_program_upgrade_authority(&mut svm, &gov.pubkey());
    send(&mut svm, &[init_protocol_ix(&gov.pubkey())], &gov, &[]).expect("init_protocol");

    // A perfectly-formed base mint (no extensions even needed — the OWNER check alone must fire),
    // owned by the Token-2022 program.
    let t22_mint = Pubkey::new_unique();
    let mut data = vec![0u8; Mint::LEN];
    Mint {
        mint_authority: solana_sdk::program_option::COption::Some(gov.pubkey()),
        supply: 0,
        decimals: COLL_DECIMALS,
        is_initialized: true,
        freeze_authority: solana_sdk::program_option::COption::None,
    }
    .pack_into_slice(&mut data);
    svm.set_account(
        t22_mint,
        SolanaAccount { lamports: 10_000_000, data, owner: token_2022_id, executable: false, rent_epoch: 0 },
    )
    .unwrap();

    let f = send(
        &mut svm,
        &[init_market_ix(&gov.pubkey(), &t22_mint, MCR_BPS, DEBT_CEILING, 0, 0, BUCKET_WIDTH_BPS, 0)],
        &gov,
        &[],
    )
    .unwrap_err();
    assert_eq!(
        custom_code(&f),
        ANCHOR_ACCOUNT_OWNED_BY_WRONG_PROGRAM,
        "a Token-2022 mint must die at Anchor account validation, before the handler"
    );
}
