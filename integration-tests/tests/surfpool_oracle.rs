//! Surfpool mainnet-fork oracle test (`#[ignore]` — network + a running surfpool).
//!
//! Run via `tests/surfpool/run.sh`, which starts surfpool forking mainnet, deploys the
//! program, then runs this with `--ignored`.
//!
//! Unique value over the litesvm suite: it exercises our on-chain `clmm.rs` parser +
//! `sample_twap` against the REAL, live Orca/Raydium pool accounts (forked from mainnet),
//! catching any drift between our pinned byte offsets and reality that synthetic fixtures
//! can't. The aggregation/mode logic is already covered hermetically by the litesvm suite
//! (incl. the self-signed-quote `mode == Ok` path); the Switchboard *gateway* real-quote
//! leg (a fresh signed quote → `update_price` → `mode == Ok`) needs the JS SDK and is
//! documented in tests/surfpool/README.md as the manual extension.

#![cfg(not(doctest))]

use fusd_integration_tests::*;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    message::Message,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    transaction::Transaction,
};
use std::str::FromStr;

// Real mainnet accounts (see docs/clmm-pool-layouts.md).
const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const ORCA_SOL_USDC_WHIRLPOOL: &str = "HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ";
// Pyth SOL/USD feed id + a real Switchboard SOL/USD feed (bindings only — this test doesn't
// post a Pyth/SB update; it exercises the TWAP sampler against the live pool).
const PYTH_SOL_USD_FEED_ID_HEX: &str =
    "ef0d8b6fcd0104e3e75096912fc8e1e432893da4f18faedaacca7e5875da620f";

fn rpc() -> RpcClient {
    let url = std::env::var("SURFPOOL_RPC").unwrap_or_else(|_| "http://127.0.0.1:8899".to_string());
    RpcClient::new_with_commitment(url, CommitmentConfig::confirmed())
}

fn send(rpc: &RpcClient, ixs: &[Instruction], payer: &Keypair, signers: &[&Keypair]) {
    let bh = rpc.get_latest_blockhash().expect("blockhash");
    let msg = Message::new(ixs, Some(&payer.pubkey()));
    let mut all = vec![payer];
    all.extend_from_slice(signers);
    let tx = Transaction::new(&all, msg, bh);
    rpc.send_and_confirm_transaction(&tx).expect("tx confirmed");
}

fn feed_id_bytes() -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&PYTH_SOL_USD_FEED_ID_HEX[i * 2..i * 2 + 2], 16).unwrap();
    }
    out
}

#[test]
#[ignore = "requires a running surfpool mainnet fork; run via tests/surfpool/run.sh"]
fn sample_twap_parses_live_orca_pool() {
    let rpc = rpc();
    let coll = Pubkey::from_str(WSOL).unwrap();
    let usdc = Pubkey::from_str(USDC).unwrap();
    let pool = Pubkey::from_str(ORCA_SOL_USDC_WHIRLPOOL).unwrap();

    // Funded payer (surfpool airdrops the default keypair; we just need lamports).
    let payer = Keypair::new();
    rpc.request_airdrop(&payer.pubkey(), 100_000_000_000)
        .and_then(|sig| rpc.confirm_transaction(&sig))
        .expect("airdrop");

    // Protocol + a WSOL market (WSOL has no freeze authority → passes the allowlist).
    send(&rpc, &[init_protocol_ix(&payer.pubkey())], &payer, &[]);
    send(
        &rpc,
        &[init_market_ix(
            &payer.pubkey(),
            &coll,
            MCR_BPS,
            DEBT_CEILING,
            0, // reserve_lamports
            0, // liq_gas_comp_bps
            BUCKET_WIDTH_BPS,
            REDEMPTION_FEE_BPS,
        )],
        &payer,
        &[],
    );

    // Oracle bound to the REAL Orca SOL/USDC whirlpool + the real Pyth SOL/USD feed id.
    let mut args = default_oracle_args();
    args.pyth_feed_id = feed_id_bytes();
    args.orca_pool = pool;
    args.raydium_pool = Pubkey::default();
    // init_market_oracle binds the quote mint (USDC) in MarketOracle; sample_twap
    // reads decimals from config rather than taking the mint per-call.
    send(&rpc, &[init_market_oracle_ix(&payer.pubkey(), &coll, &usdc, args)], &payer, &[]);

    // Sample the live pool — this runs our clmm.rs parser on real mainnet bytes.
    send(&rpc, &[sample_twap_ix(&payer.pubkey(), &coll, &pool)], &payer, &[]);

    // Read the ring back and assert a plausible live SOL price landed (spot scale = usd·1e24).
    let twap_acct = rpc
        .get_account(&dex_twap_pda(&coll))
        .expect("dex_twap account");
    // Unaligned: RPC account data isn't 16-aligned for the u128 ring.
    let twap: fusd_core::state::DexTwap = bytemuck::pod_read_unaligned(
        &twap_acct.data[8..8 + std::mem::size_of::<fusd_core::state::DexTwap>()],
    );
    assert_eq!(twap.count, 1, "one observation recorded");
    let price = twap.prices[0];
    let usd = price / 10u128.pow(24); // spot scale → whole USD
    assert!(
        (10..=10_000).contains(&usd),
        "live SOL price {usd} USD out of sane band (spot {price}) — parser/layout drift?"
    );
    println!("live Orca SOL/USDC via sample_twap: ~${usd} (spot {price})");
}
