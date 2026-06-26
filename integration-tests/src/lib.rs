//! Shared in-process litesvm test harness for `fusd-core`.
//!
//! Host-only (this crate is never deployed and is excluded from `anchor build`). It loads the
//! compiled program from `target/deploy/fusd_core.so`, which MUST be built with the `dev-oracle`
//! feature so the test-only `dev_set_price` exists at runtime:
//!
//! ```text
//! anchor build -- --features dev-oracle
//! ```
//!
//! Then run the scenarios under `tests/`:
//!
//! ```text
//! cargo test -p fusd-integration-tests
//! ```
//!
//! Keeping this harness (and the `dev-oracle` dependency) OUT of the program crate's manifest is
//! deliberate: a non-program member can depend on `fusd-core` with `dev-oracle` without that
//! feature ever unifying into the deployed `.so`/IDL.
#![allow(deprecated)] // solana_sdk::{system_program, system_instruction} moved to solana-system-interface in 2.3; fine for tests.

use anchor_lang::{AccountDeserialize, AccountSerialize, InstructionData, ToAccountMetas};
use litesvm::types::{FailedTransactionMetadata, TransactionResult};
use litesvm::LiteSVM;
use solana_sdk::{
    instruction::{AccountMeta, Instruction, InstructionError},
    message::{v0, VersionedMessage},
    program_pack::Pack,
    pubkey::Pubkey,
    rent::Rent,
    signature::{Keypair, Signer},
    system_instruction, system_program,
    sysvar::rent,
    transaction::{TransactionError, VersionedTransaction},
};

use fusd_core::instructions::init_market::InitMarketArgs;
use fusd_core::instructions::init_market_oracle::InitMarketOracleArgs;
use fusd_core::instructions::init_protocol::InitProtocolArgs;
use fusd_core::instructions::open_position::OpenPositionArgs;
/// Re-exported so scenario files can reference it via `fusd_integration_tests::MarketParam`.
pub use fusd_core::state::{GlobalParam, MarketParam};

// ============================ constants ============================

/// Compiled program (built with `--features dev-oracle`). The harness lives one level under the
/// repo root, so the `.so` is `<repo>/target/deploy/fusd_core.so`.
pub const SO_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/deploy/fusd_core.so");
pub const SPL_TOKEN_ID: Pubkey = spl_token::ID;

// --- fixed scenario knobs shared across tests ---
pub const COLL_DECIMALS: u8 = 9; // a SOL-like collateral
pub const FUSD_DECIMALS: u8 = 6; // fUSD
pub const MCR_BPS: u16 = 15_000; // 150%
pub const DEBT_CEILING: u64 = 1_000_000 * 1_000_000; // $1,000,000 in fUSD-native

// Reserve bond + collateral gas-comp used by the incentive tests (recommended defaults).
pub const RESERVE_LAMPORTS: u64 = 20_000_000; // 0.02 SOL
pub const GAS_COMP_BPS: u16 = 50; // 0.5%

// Redemption: default bucket width (0.10%) + flat fee (0.5%) for the redemption tests.
pub const BUCKET_WIDTH_BPS: u16 = 10;
pub const REDEMPTION_FEE_BPS: u16 = 50;

/// Bucket a borrower rate maps to under [`BUCKET_WIDTH_BPS`] (256 buckets).
pub fn bucket_of(rate_bps: u16) -> usize {
    fusd_math::rate_bucket::bucket_of(rate_bps, BUCKET_WIDTH_BPS, 256)
}

// Anchor custom-error codes (base 6000 + variant index in fusd-core's errors.rs).
pub const E_UNAUTHORIZED: u32 = 6000;
pub const E_PARAM_OUT_OF_BOUNDS: u32 = 6001;
pub const E_COLLATERAL_HAS_FREEZE_AUTHORITY: u32 = 6002;
pub const E_ORACLE_UNAVAILABLE: u32 = 6003;
pub const E_STALE_PRICE: u32 = 6004;
pub const E_BELOW_MIN_COLLATERAL_RATIO: u32 = 6005;
pub const E_POSITION_HEALTHY: u32 = 6009;
pub const E_REACTOR_POOL_TOO_SMALL: u32 = 6010;
pub const E_NO_REDISTRIBUTION_RECIPIENTS: u32 = 6014;
pub const E_POSITION_NOT_EMPTY: u32 = 6016;
pub const E_NOTHING_TO_REDEEM: u32 = 6017;
pub const E_WRONG_REDEMPTION_BUCKET: u32 = 6018;
pub const E_DUPLICATE_REDEMPTION_TARGET: u32 = 6019;
pub const E_TIMELOCK_NOT_ELAPSED: u32 = 6020;
pub const E_TIMELOCK_MARKET_MISMATCH: u32 = 6021;
pub const E_MINT_FROZEN: u32 = 6022;
pub const E_INVALID_PRICE_UPDATE: u32 = 6023;
pub const E_INVALID_SWITCHBOARD_FEED: u32 = 6024;
pub const E_INVALID_CLMM_POOL: u32 = 6025;
pub const E_TWAP_SAMPLE_REJECTED: u32 = 6026;
pub const E_GUARDIAN_PAUSED: u32 = 6027;
pub const E_MARKET_SHUTDOWN: u32 = 6028;
pub const E_MARKET_NOT_SHUTDOWN: u32 = 6029;
pub const E_SHUTDOWN_CONDITION_NOT_MET: u32 = 6030;
pub const E_RATE_LIMIT_EXCEEDED: u32 = 6031;
pub const E_CCR_RESTRICTED: u32 = 6032;
pub const E_LIQUIDATION_GRACE_PERIOD: u32 = 6033;
pub const E_NO_PENDING_AUTHORITY: u32 = 6036;
pub const E_DEBT_BELOW_MINIMUM: u32 = 6037;
pub const E_TOO_MANY_REDEMPTION_CANDIDATES: u32 = 6038;
pub const E_COLLAR_EXCEEDS_MCR: u32 = 6040;
pub const E_PARAM_COMBINATION_INVALID: u32 = 6041;
pub const E_VAULT_RECONCILIATION_FAILED: u32 = 6042;
pub const E_INVALID_RECIPIENT: u32 = 6043;
pub const E_INSUFFICIENT_BACKSTOP_EXCESS: u32 = 6044;
pub const E_ORACLE_DIVERGENT: u32 = 6045; // B3 — liquidation paused under oracle divergence (consolidation: 6044→6045 behind the backstop's 6044)
pub const E_INVALID_STAKE_POOL: u32 = 6046; // C1 — wrong SPL stake-pool account on an LST update_price

// ============================ svm / tx helpers ============================

/// Fresh `LiteSVM` with the fUSD program loaded as an UPGRADEABLE program (BPF loader v3) so the
/// `init_protocol` upgrade-authority gate can be exercised. The upgrade authority starts
/// as `Pubkey::default()`; each bootstrap repoints it to the test `gov` via
/// [`set_program_upgrade_authority`] before calling `init_protocol`.
pub fn new_svm() -> LiteSVM {
    let mut svm = LiteSVM::new();
    load_upgradeable_program(&mut svm, SO_PATH, Pubkey::default());
    svm
}

/// The canonical `ProgramData` PDA of `fusd_core` under the BPF upgradeable loader.
pub fn programdata_pda() -> Pubkey {
    Pubkey::find_program_address(
        &[fusd_core::ID.as_ref()],
        &solana_sdk::bpf_loader_upgradeable::id(),
    )
    .0
}

/// Load `fusd_core.so` under the BPF upgradeable loader with the given upgrade authority: writes the
/// `ProgramData` account (45-byte metadata header + ELF) then the `Program` account that points at
/// it (executable). `add_program_from_file` only loads the non-upgradeable v2 layout, which has no
/// `ProgramData` account and so can't drive the gate.
fn load_upgradeable_program(svm: &mut LiteSVM, so_path: &str, upgrade_authority: Pubkey) {
    use solana_sdk::account::Account;
    use solana_sdk::bpf_loader_upgradeable::{self, UpgradeableLoaderState};

    let elf = std::fs::read(so_path)
        .expect("read fusd_core.so (built with `anchor build -- --features dev-oracle`)");
    let pd_addr = programdata_pda();

    let meta = UpgradeableLoaderState::ProgramData {
        slot: 0,
        upgrade_authority_address: Some(upgrade_authority),
    };
    let mut pd_data = bincode::serialize(&meta).expect("serialize ProgramData metadata");
    debug_assert_eq!(pd_data.len(), UpgradeableLoaderState::size_of_programdata_metadata());
    pd_data.extend_from_slice(&elf);
    // ProgramData must exist before the Program account, whose load reads it.
    let pd_lamports = svm.minimum_balance_for_rent_exemption(pd_data.len());
    svm.set_account(
        pd_addr,
        Account {
            lamports: pd_lamports,
            data: pd_data,
            owner: bpf_loader_upgradeable::id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .expect("set programdata account");

    let prog = UpgradeableLoaderState::Program { programdata_address: pd_addr };
    let prog_data = bincode::serialize(&prog).expect("serialize Program state");
    let prog_lamports = svm.minimum_balance_for_rent_exemption(prog_data.len());
    svm.set_account(
        fusd_core::ID,
        Account {
            lamports: prog_lamports,
            data: prog_data,
            owner: bpf_loader_upgradeable::id(),
            executable: true,
            rent_epoch: 0,
        },
    )
    .expect("set program account (loads the ELF from programdata)");
}

/// Repoint the loaded program's upgrade authority (rewrites only the `ProgramData` metadata header,
/// preserving the ELF) so `init_protocol`'s gate accepts `auth` as the legitimate initializer.
pub fn set_program_upgrade_authority(svm: &mut LiteSVM, auth: &Pubkey) {
    use solana_sdk::bpf_loader_upgradeable::UpgradeableLoaderState;
    let pd_addr = programdata_pda();
    let mut acct = svm.get_account(&pd_addr).expect("programdata account loaded");
    let meta = UpgradeableLoaderState::ProgramData {
        slot: 0,
        upgrade_authority_address: Some(*auth),
    };
    let meta_bytes = bincode::serialize(&meta).expect("serialize ProgramData metadata");
    acct.data[..meta_bytes.len()].copy_from_slice(&meta_bytes);
    svm.set_account(pd_addr, acct).expect("rewrite programdata upgrade authority");
}

/// Compile + sign + send a v0 transaction. `payer` is the fee payer (and a signer); `signers`
/// are any additional required signers (owners of `init`/authority accounts).
pub fn send(
    svm: &mut LiteSVM,
    ixs: &[Instruction],
    payer: &Keypair,
    signers: &[&Keypair],
) -> TransactionResult {
    let mut all: Vec<&Keypair> = vec![payer];
    all.extend_from_slice(signers);
    let msg = v0::Message::try_compile(&payer.pubkey(), ixs, &[], svm.latest_blockhash()).unwrap();
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(msg), &all).unwrap();
    svm.send_transaction(tx)
}

/// Decode every event of type `E` from a successful tx. Events ride
/// the Anchor #[event_cpi] self-CPI transport: each is an INNER instruction whose data is
/// `EVENT_IX_TAG_LE (8) ++ event discriminator (8) ++ borsh(payload)` — preserved in transaction
/// metadata and immune to the RPC log truncation that silently dropped `Program data:` lines in
/// log-heavy txs (exactly the fat Pyth-post + liquidate + DEX-sell bundles where the BadDebt
/// pager matters). Scans inner instructions; the tag prefix uniquely identifies event CPIs.
pub fn events_of<E: anchor_lang::Discriminator + anchor_lang::AnchorDeserialize>(
    meta: &litesvm::types::TransactionMetadata,
) -> Vec<E> {
    let mut out = Vec::new();
    for inner in meta.inner_instructions.iter().flatten() {
        let data: &[u8] = &inner.instruction.data;
        if data.len() < 8 || &data[..8] != anchor_lang::event::EVENT_IX_TAG_LE {
            continue;
        }
        let payload = &data[8..];
        let disc = E::DISCRIMINATOR;
        if payload.len() >= disc.len() && &payload[..disc.len()] == disc {
            if let Ok(ev) = E::try_from_slice(&payload[disc.len()..]) {
                out.push(ev);
            }
        }
    }
    out
}

/// Assert the tx emitted NO log-based (`Program data:`) events — the single-transport guarantee
/// after the event-CPI migration (no dual emission, no decoder ambiguity).
pub fn assert_no_log_events(meta: &litesvm::types::TransactionMetadata) {
    assert!(
        meta.logs.iter().all(|l| !l.starts_with("Program data: ")),
        "found a log-transport event after the event-CPI migration"
    );
}

/// The single event of type `E` a tx emitted (asserts exactly one).
pub fn single_event<E: anchor_lang::Discriminator + anchor_lang::AnchorDeserialize>(
    meta: &litesvm::types::TransactionMetadata,
) -> E {
    let mut evs = events_of::<E>(meta);
    assert_eq!(evs.len(), 1, "expected exactly one event of this type, got {}", evs.len());
    evs.pop().unwrap()
}

/// Extract the Anchor custom-error code from a failed tx (asserts it was a custom error).
pub fn custom_code(f: &FailedTransactionMetadata) -> u32 {
    match f.err {
        TransactionError::InstructionError(_, InstructionError::Custom(code)) => code,
        ref other => panic!(
            "expected a Custom instruction error, got: {other:?}\nlogs:\n{}",
            f.meta.pretty_logs()
        ),
    }
}

/// Fund an account with `sol` whole SOL.
pub fn airdrop_sol(svm: &mut LiteSVM, who: &Pubkey, sol: u64) {
    svm.airdrop(who, sol * 1_000_000_000).unwrap();
}

// ============================ unit helpers ============================

/// `n` whole collateral tokens in native units.
pub fn whole_coll(n: u64) -> u64 {
    n * 10u64.pow(COLL_DECIMALS as u32)
}

/// `n` whole fUSD ($n) in fUSD-native units.
pub fn usd(n: u64) -> u64 {
    n * 10u64.pow(FUSD_DECIMALS as u32)
}

/// `Market.spot` (RAY-scaled fUSD-native per 1 *native* collateral unit) for a whole-token USD
/// price. e.g. $100/token, 9-dec collateral, 6-dec fUSD -> RAY/10.
pub fn spot_for_usd(price_usd: u128) -> u128 {
    price_usd * 10u128.pow(FUSD_DECIMALS as u32) * fusd_math::RAY / 10u128.pow(COLL_DECIMALS as u32)
}

// ============================ SPL helpers ============================

/// Create an SPL mint via raw CreateAccount + InitializeMint2. `freeze` controls whether the
/// mint carries a freeze authority (init_market rejects mints that do).
pub fn create_mint(
    svm: &mut LiteSVM,
    payer: &Keypair,
    mint: &Keypair,
    decimals: u8,
    mint_authority: &Pubkey,
    freeze: bool,
) {
    let rent_lamports = Rent::default().minimum_balance(spl_token::state::Mint::LEN);
    let create = system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        rent_lamports,
        spl_token::state::Mint::LEN as u64,
        &SPL_TOKEN_ID,
    );
    let freeze_authority = freeze.then_some(mint_authority);
    let init = spl_token::instruction::initialize_mint2(
        &SPL_TOKEN_ID,
        &mint.pubkey(),
        mint_authority,
        freeze_authority,
        decimals,
    )
    .unwrap();
    send(svm, &[create, init], payer, &[mint]).expect("create_mint failed");
}

/// Create `owner`'s ATA for `mint` and (optionally) mint `amount` into it. `mint_authority` must
/// be a Keypair signer (used only when `amount > 0`).
pub fn create_ata_and_fund(
    svm: &mut LiteSVM,
    payer: &Keypair,
    owner: &Pubkey,
    mint: &Pubkey,
    mint_authority: Option<&Keypair>,
    amount: u64,
) -> Pubkey {
    let ata = spl_associated_token_account::get_associated_token_address(owner, mint);
    let create = spl_associated_token_account::instruction::create_associated_token_account(
        &payer.pubkey(),
        owner,
        mint,
        &SPL_TOKEN_ID,
    );
    send(svm, &[create], payer, &[]).expect("create ATA failed");
    if amount > 0 {
        let ma = mint_authority.expect("mint_authority required to fund");
        let mint_to = spl_token::instruction::mint_to(
            &SPL_TOKEN_ID,
            mint,
            &ata,
            &ma.pubkey(),
            &[],
            amount,
        )
        .unwrap();
        send(svm, &[mint_to], payer, &[ma]).expect("mint_to failed");
    }
    ata
}

pub fn token_balance(svm: &LiteSVM, ata: &Pubkey) -> u64 {
    let acct = svm.get_account(ata).expect("token account exists");
    spl_token::state::Account::unpack(&acct.data).unwrap().amount
}

/// Circulating supply of an SPL mint. Used to assert that an underwater liquidation retires exactly
/// the burned debt (so fUSD held elsewhere stays fully backed — the loss lands on RP equity, not the peg).
pub fn mint_supply(svm: &LiteSVM, mint: &Pubkey) -> u64 {
    let acct = svm.get_account(mint).expect("mint exists");
    spl_token::state::Mint::unpack(&acct.data).unwrap().supply
}

/// The associated token address for `owner`/`mint`.
pub fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    spl_associated_token_account::get_associated_token_address(owner, mint)
}

/// An account's lamport balance (0 if it doesn't exist).
pub fn lamports(svm: &LiteSVM, who: &Pubkey) -> u64 {
    svm.get_account(who).map(|a| a.lamports).unwrap_or(0)
}

// Redemption bitmap readers — parse the zero-copy account's raw layout:
// disc(8) | words[4]·u64 (32) | counts[256]·u32 (1024) | zombie_count·u64 (8).
pub fn bucket_is_set(svm: &LiteSVM, coll: &Pubkey, bucket: usize) -> bool {
    let acct = svm.get_account(&redemption_bitmap_pda(coll)).expect("bitmap exists");
    let off = 8 + (bucket / 64) * 8;
    let word = u64::from_le_bytes(acct.data[off..off + 8].try_into().unwrap());
    word & (1u64 << (bucket % 64)) != 0
}
/// Member count of the redemption zombie pen (collateral-exhausted / sub-min_debt positions).
pub fn zombie_count(svm: &LiteSVM, coll: &Pubkey) -> u64 {
    let acct = svm.get_account(&redemption_bitmap_pda(coll)).expect("bitmap exists");
    let off = 8 + 32 + 1024; // after disc + words + counts
    u64::from_le_bytes(acct.data[off..off + 8].try_into().unwrap())
}
pub fn bucket_count(svm: &LiteSVM, coll: &Pubkey, bucket: usize) -> u32 {
    let acct = svm.get_account(&redemption_bitmap_pda(coll)).expect("bitmap exists");
    let off = 8 + 32 + bucket * 4;
    u32::from_le_bytes(acct.data[off..off + 4].try_into().unwrap())
}
/// The lowest non-empty bucket in a market's bitmap (None if empty).
pub fn lowest_bucket(svm: &LiteSVM, coll: &Pubkey) -> Option<usize> {
    let acct = svm.get_account(&redemption_bitmap_pda(coll)).expect("bitmap exists");
    let mut words = [0u64; 4];
    for (i, w) in words.iter_mut().enumerate() {
        let off = 8 + i * 8;
        *w = u64::from_le_bytes(acct.data[off..off + 8].try_into().unwrap());
    }
    fusd_math::rate_bucket::first_set(&words)
}

// ============================ PDAs ============================

fn pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &fusd_core::ID).0
}

pub fn config_pda() -> Pubkey {
    pda(&[b"config"])
}

/// The Anchor #[event_cpi] event-authority PDA (`[b"__event_authority"]`) — the read-only signer
/// of the self-CPI event transport. Injected (with the program account) into
/// every emitting instruction's account list.
pub fn event_authority_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &fusd_core::ID).0
}
pub fn mint_authority_pda() -> Pubkey {
    pda(&[b"mint_authority"])
}
pub fn fusd_mint_pda() -> Pubkey {
    pda(&[b"fusd_mint"])
}
pub fn market_pda(coll: &Pubkey) -> Pubkey {
    pda(&[b"market", coll.as_ref()])
}
pub fn coll_vault_pda(coll: &Pubkey) -> Pubkey {
    pda(&[b"coll_vault", coll.as_ref()])
}
pub fn position_pda(coll: &Pubkey, owner: &Pubkey) -> Pubkey {
    pda(&[b"position", coll.as_ref(), owner.as_ref()])
}
pub fn reactor_pool_pda(coll: &Pubkey) -> Pubkey {
    pda(&[b"reactor", coll.as_ref()])
}
pub fn ess_pda(coll: &Pubkey) -> Pubkey {
    pda(&[b"ess", coll.as_ref()])
}
pub fn reactor_fusd_vault_pda(coll: &Pubkey) -> Pubkey {
    pda(&[b"reactor_fusd", coll.as_ref()])
}
pub fn reactor_coll_vault_pda(coll: &Pubkey) -> Pubkey {
    pda(&[b"reactor_coll", coll.as_ref()])
}
pub fn buffer_pda(coll: &Pubkey) -> Pubkey {
    pda(&[b"buffer", coll.as_ref()])
}
pub fn buffer_fusd_vault_pda(coll: &Pubkey) -> Pubkey {
    pda(&[b"buffer_fusd", coll.as_ref()])
}
pub fn reactor_deposit_pda(coll: &Pubkey, owner: &Pubkey) -> Pubkey {
    pda(&[b"reactor_dep", coll.as_ref(), owner.as_ref()])
}
pub fn redemption_bitmap_pda(coll: &Pubkey) -> Pubkey {
    pda(&[b"redeem_bitmap", coll.as_ref()])
}
pub fn gov_gate_pda() -> Pubkey {
    pda(&[b"gov_gate"])
}
pub fn timelock_pda(nonce: u64) -> Pubkey {
    pda(&[b"timelock", nonce.to_le_bytes().as_ref()])
}
pub fn backstop_pda() -> Pubkey {
    pda(&[b"backstop"])
}
pub fn backstop_fusd_vault_pda() -> Pubkey {
    pda(&[b"backstop_fusd"])
}
pub fn global_timelock_pda(nonce: u64) -> Pubkey {
    pda(&[b"gtimelock", nonce.to_le_bytes().as_ref()])
}
pub fn market_oracle_pda(coll: &Pubkey) -> Pubkey {
    pda(&[b"oracle", coll.as_ref()])
}
pub fn dex_twap_pda(coll: &Pubkey) -> Pubkey {
    pda(&[b"twap", coll.as_ref()])
}

// ============================ typed account readers ============================

pub fn read_market(svm: &LiteSVM, market: &Pubkey) -> fusd_core::state::Market {
    let acct = svm.get_account(market).unwrap();
    fusd_core::state::Market::try_deserialize(&mut acct.data.as_slice()).unwrap()
}
pub fn read_position(svm: &LiteSVM, position: &Pubkey) -> fusd_core::state::Position {
    let acct = svm.get_account(position).unwrap();
    fusd_core::state::Position::try_deserialize(&mut acct.data.as_slice()).unwrap()
}

/// The per-market supply invariant: circulating fUSD == `agg_recorded_debt − unminted_interest +
/// bad_debt` (read right after a touch/refresh, so the live aggregate interest is already folded in).
/// Asserted in the rearranged `circulating + unminted == agg + bad` form so the intermediate never
/// underflows u128: after a terminal un-homed liquidation `agg_recorded_debt` (the victim's debt
/// extinguished to `bad_debt`) can drop BELOW the still-pending `unminted_interest` lazy-mint backlog,
/// so the literal `agg - unminted` would panic even though the identity holds.
pub fn assert_supply_invariant(svm: &LiteSVM, coll: &Pubkey) {
    let m = read_market(svm, &market_pda(coll));
    let circulating = mint_supply(svm, &fusd_mint_pda()) as u128;
    assert_eq!(
        circulating + m.unminted_interest,
        m.agg_recorded_debt + m.bad_debt,
        "supply invariant: circulating == agg_recorded_debt - unminted_interest + bad_debt"
    );
}

/// The vault invariant (proof-of-reserves): the collateral vault's token balance equals the four
/// `Market`-tracked buckets — live-position backing + redemption-fee surplus + borrower-owed
/// liquidation surpluses + retained protocol-owned (un-homed) collateral.
pub fn assert_vault_invariant(svm: &LiteSVM, coll: &Pubkey) {
    let m = read_market(svm, &market_pda(coll));
    let vault = token_balance(svm, &coll_vault_pda(coll)) as u128;
    assert_eq!(
        vault,
        m.total_collateral
            + m.surplus_collateral as u128
            + m.total_coll_surplus as u128
            + m.protocol_collateral as u128,
        "vault invariant: vault == total_collateral + surplus_collateral + total_coll_surplus + protocol_collateral"
    );
}

/// The weighted-sum oracle: `agg_weighted_debt_sum == Σ recorded_debt_i · user_rate_bps_i` over the
/// given live positions. Both sides use each position's STORED recorded_debt (reweight updates the sum
/// in lockstep with the stored debt on every touch), and parked tier-2 redistributed debt is excluded
/// from BOTH until a recipient realizes it — so the identity holds at all times for the full set of
/// live positions. Pass EVERY position with debt; this catches a reweight regression (a dropped or
/// double-counted contribution) that the supply invariant alone cannot see.
pub fn assert_weighted_sum(svm: &LiteSVM, coll: &Pubkey, positions: &[Pubkey]) {
    let m = read_market(svm, &market_pda(coll));
    let sum: u128 = positions
        .iter()
        .map(|p| {
            let pos = read_position(svm, p);
            pos.recorded_debt * pos.user_rate_bps as u128
        })
        .sum();
    assert_eq!(m.agg_weighted_debt_sum, sum, "agg_weighted_debt_sum == Σ recorded_debt·rate_bps");
}
pub fn read_reactor_pool(svm: &LiteSVM, reactor: &Pubkey) -> fusd_core::state::ReactorPool {
    let acct = svm.get_account(reactor).unwrap();
    fusd_core::state::ReactorPool::try_deserialize(&mut acct.data.as_slice()).unwrap()
}
pub fn read_sp_deposit(svm: &LiteSVM, dep: &Pubkey) -> fusd_core::state::ReactorDeposit {
    let acct = svm.get_account(dep).unwrap();
    fusd_core::state::ReactorDeposit::try_deserialize(&mut acct.data.as_slice()).unwrap()
}
pub fn read_gov_gate(svm: &LiteSVM) -> fusd_core::state::GovernanceGate {
    let acct = svm.get_account(&gov_gate_pda()).unwrap();
    fusd_core::state::GovernanceGate::try_deserialize(&mut acct.data.as_slice()).unwrap()
}
pub fn read_backstop(svm: &LiteSVM) -> fusd_core::state::GlobalBackstopReserve {
    let acct = svm.get_account(&backstop_pda()).unwrap();
    fusd_core::state::GlobalBackstopReserve::try_deserialize(&mut acct.data.as_slice()).unwrap()
}
/// The global backstop reserve vault's fUSD balance.
pub fn backstop_balance(svm: &LiteSVM) -> u64 {
    token_balance(svm, &backstop_fusd_vault_pda())
}

/// Advance the SVM clock's unix timestamp by `secs` (to jump past a timelock `eta`). Also
/// expires the blockhash so a post-warp retry of an otherwise-identical tx isn't deduped as
/// `AlreadyProcessed` (litesvm's static blockhash).
pub fn warp_unix(svm: &mut LiteSVM, secs: i64) {
    let mut clock: solana_sdk::clock::Clock = svm.get_sysvar();
    clock.unix_timestamp = clock.unix_timestamp.saturating_add(secs);
    svm.set_sysvar(&clock);
    svm.expire_blockhash();
}

/// Advance the SVM clock's SLOT by `slots` (to age out the cached `spot` past
/// `MAX_PRICE_STALENESS_SLOTS`). Mirrors the cdp/liquidation suites' slot-warp pattern.
pub fn warp_slots(svm: &mut LiteSVM, slots: u64) {
    let mut clock: solana_sdk::clock::Clock = svm.get_sysvar();
    clock.slot = clock.slot.saturating_add(slots);
    svm.set_sysvar(&clock);
    svm.warp_to_slot(clock.slot);
    svm.expire_blockhash();
}

/// Current SVM slot.
pub fn current_slot(svm: &LiteSVM) -> u64 {
    let clock: solana_sdk::clock::Clock = svm.get_sysvar();
    clock.slot
}

/// After a stall→resume armed the on-resume liquidation grace window, walk the clock PAST
/// `market.liq_grace_until` the way a keeper would: re-post `spot` every `< MAX_PRICE_STALENESS_SLOTS`
/// slots so the price stays fresh and the grace window is never re-armed. Leaves the clock at a slot
/// ≥ the deadline with a fresh `spot`, so `liquidate` is no longer grace-blocked. No-op if no grace
/// is armed (`liq_grace_until == 0`).
pub fn crank_past_resume_grace(svm: &mut LiteSVM, gov: &Keypair, coll: &Pubkey, spot: u128) {
    let deadline = read_market(svm, &market_pda(coll)).liq_grace_until;
    let step = fusd_core::constants::MAX_PRICE_STALENESS_SLOTS - 1; // 249 < 250 → fresh, no re-arm
    while current_slot(svm) < deadline {
        warp_slots(svm, step);
        send(svm, &[dev_set_price_ix(&gov.pubkey(), coll, spot)], gov, &[])
            .expect("re-post price during the grace window");
    }
}

// ============================ instruction builders ============================

pub fn init_protocol_ix(gov: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::InitProtocol {
            payer: *gov,
            program_data: programdata_pda(),
            config: config_pda(),
            mint_authority: mint_authority_pda(),
            fusd_mint: fusd_mint_pda(),
            token_program: SPL_TOKEN_ID,
            system_program: system_program::ID,
            rent: rent::ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::InitProtocol {
            args: InitProtocolArgs { gov_authority: *gov, guardian: *gov },
        }
        .data(),
    }
}

/// `init_protocol` with explicit `InitProtocolArgs` (so the per-field clamps — e.g. the
/// `gov_authority != default` guard — can be exercised). `payer` is the program upgrade authority.
pub fn init_protocol_args_ix(payer: &Pubkey, args: InitProtocolArgs) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::InitProtocol {
            payer: *payer,
            program_data: programdata_pda(),
            config: config_pda(),
            mint_authority: mint_authority_pda(),
            fusd_mint: fusd_mint_pda(),
            token_program: SPL_TOKEN_ID,
            system_program: system_program::ID,
            rent: rent::ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::InitProtocol { args }.data(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn init_market_ix(
    authority: &Pubkey,
    coll: &Pubkey,
    mcr_bps: u16,
    debt_ceiling: u64,
    reserve_lamports: u64,
    liq_gas_comp_bps: u16,
    bucket_width_bps: u16,
    redemption_fee_bps: u16,
) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::InitMarket {
            authority: *authority,
            config: config_pda(),
            collateral_mint: *coll,
            market: market_pda(coll),
            collateral_vault: coll_vault_pda(coll),
            redemption_bitmap: redemption_bitmap_pda(coll),
            token_program: SPL_TOKEN_ID,
            system_program: system_program::ID,
            rent: rent::ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::InitMarket {
            args: InitMarketArgs {
                mcr_bps,
                debt_ceiling,
                reserve_lamports,
                liq_gas_comp_bps,
                bucket_width_bps,
                redemption_fee_bps,
                // Test markets default to the collar OFF (seize-all); collar tests enable it via
                // governance (`MarketParam::LiqBonus`). Deploy scripts pass `DEFAULT_LIQ_BONUS_BPS`.
                liq_bonus_bps: 0,
            },
        }
        .data(),
    }
}

/// Permissionless `refresh_market`: advance the market's aggregate interest and mint the accumulated
/// interest into the insurance buffer (the lazy mint seam). Needs the fUSD mint + buffer accounts.
pub fn refresh_market_ix(coll: &Pubkey) -> Instruction {
    refresh_market_reward_ix(coll, None)
}

/// `refresh_market` with an optional keeper-reward sink (the cranker's fUSD ATA). `None` ⇒ the whole
/// interest funds the buffer; `Some(ata)` ⇒ the `keeper_reward_bps` cut is paid there.
pub fn refresh_market_reward_ix(coll: &Pubkey, cranker_fusd_ata: Option<Pubkey>) -> Instruction {
    refresh_market_full_ix(coll, cranker_fusd_ata, /*with_backstop=*/ false)
}

/// `refresh_market` with explicit control over whether the global-backstop accounts are supplied.
/// `with_backstop = true` routes the backstop cut (requires the reserve to be inited); `false` funds
/// only the local buffer (the backstop accounts are `None`).
pub fn refresh_market_full_ix(
    coll: &Pubkey,
    cranker_fusd_ata: Option<Pubkey>,
    with_backstop: bool,
) -> Instruction {
    let (backstop, backstop_fusd_vault) = if with_backstop {
        (Some(backstop_pda()), Some(backstop_fusd_vault_pda()))
    } else {
        (None, None)
    };
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::RefreshMarket {
            collateral_mint: *coll,
            market: market_pda(coll),
            fusd_mint: fusd_mint_pda(),
            mint_authority: mint_authority_pda(),
            insurance_buffer: buffer_pda(coll),
            buffer_fusd_vault: buffer_fusd_vault_pda(coll),
            cranker_fusd_ata,
            backstop,
            backstop_fusd_vault,
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::RefreshMarket {}.data(),
    }
}

/// Default `InitMarketOracleArgs` for tests: real-shaped feed bindings + the recommended
/// thresholds from `fusd_core::constants`.
pub fn default_oracle_args() -> InitMarketOracleArgs {
    InitMarketOracleArgs {
        pyth_feed_id: [7u8; 32],
        switchboard_feed: Pubkey::new_unique(),
        orca_pool: Pubkey::new_unique(),
        raydium_pool: Pubkey::default(),
        max_conf_bps: fusd_core::constants::DEFAULT_ORACLE_CONF_BPS,
        max_deviation_bps: fusd_core::constants::DEFAULT_ORACLE_DEVIATION_BPS,
        twap_max_divergence_bps: fusd_core::constants::DEFAULT_TWAP_DIVERGENCE_BPS,
        max_age_secs: fusd_core::constants::DEFAULT_ORACLE_MAX_AGE_SECS,
        k_bps: fusd_core::constants::DEFAULT_ORACLE_K_BPS,
        twap_window_secs: fusd_core::constants::DEFAULT_TWAP_WINDOW_SECS,
        twap_min_samples: fusd_core::constants::DEFAULT_TWAP_MIN_SAMPLES,
        twap_max_staleness_secs: fusd_core::constants::DEFAULT_TWAP_STALENESS_SECS,
        // C6 plausibility band OFF by default (byte-identical pre-C6 oracle behavior).
        price_band_lower_ray: 0,
        price_band_upper_ray: 0,
        // B3 liquidation-divergence gate OFF by default.
        liq_max_divergence_bps: 0,
        // C1 LST canonical-rate leg OFF by default (non-LST market).
        lst_stake_pool: Pubkey::default(),
    }
}

pub fn init_market_oracle_ix(
    authority: &Pubkey,
    coll: &Pubkey,
    quote_mint: &Pubkey,
    args: InitMarketOracleArgs,
) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::InitMarketOracle {
            authority: *authority,
            config: config_pda(),
            collateral_mint: *coll,
            quote_mint: *quote_mint,
            market: market_pda(coll),
            market_oracle: market_oracle_pda(coll),
            dex_twap: dex_twap_pda(coll),
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::InitMarketOracle { args }.data(),
    }
}

pub fn read_market_oracle(svm: &LiteSVM, coll: &Pubkey) -> fusd_core::state::MarketOracle {
    let acct = svm.get_account(&market_oracle_pda(coll)).unwrap();
    fusd_core::state::MarketOracle::try_deserialize(&mut acct.data.as_slice()).unwrap()
}

// ============================ oracle cranks: builders + fixtures ============================

/// The market's current unix timestamp (litesvm `Clock`). Use to stamp fresh feed `publish_ts`.
pub fn now_unix(svm: &LiteSVM) -> i64 {
    let clock: solana_sdk::clock::Clock = svm.get_sysvar();
    clock.unix_timestamp
}

/// Write a raw, rent-exempt, non-executable account owned by `owner` (the venue/oracle program).
fn set_raw_account(svm: &mut LiteSVM, key: &Pubkey, data: Vec<u8>, owner: Pubkey) {
    let lamports = Rent::default().minimum_balance(data.len()).max(1);
    let acct = solana_sdk::account::Account { lamports, data, owner, executable: false, rent_epoch: 0 };
    svm.set_account(*key, acct).expect("set_account");
}

/// Inject a Pyth `PriceUpdateV2` account (owned by the Pyth receiver program) — the ACTUAL SDK type,
/// anchor-serialized, so `update_price`'s `try_deserialize` + `get_price_unchecked` see real bytes.
/// `VerificationLevel::Full`. `price * 10^expo` is the human price; `conf` is σ in the same scale.
#[allow(clippy::too_many_arguments)]
pub fn set_pyth_price(
    svm: &mut LiteSVM,
    key: &Pubkey,
    feed_id: [u8; 32],
    price: i64,
    conf: u64,
    expo: i32,
    publish_ts: i64,
) {
    use pyth_solana_receiver_sdk::price_update::{
        PriceFeedMessage, PriceUpdateV2, VerificationLevel,
    };
    let pu = PriceUpdateV2 {
        write_authority: Pubkey::new_unique(),
        verification_level: VerificationLevel::Full,
        price_message: PriceFeedMessage {
            feed_id,
            price,
            conf,
            exponent: expo,
            publish_time: publish_ts,
            prev_publish_time: publish_ts.saturating_sub(1),
            ema_price: price,
            ema_conf: conf,
        },
        posted_slot: 0,
    };
    let mut data = Vec::new();
    pu.try_serialize(&mut data).expect("serialize PriceUpdateV2");
    set_raw_account(svm, key, data, fusd_core::constants::PYTH_RECEIVER_PROGRAM_ID);
}

/// The market's current epoch (litesvm `Clock`). Use to stamp a fresh stake-pool `last_update_epoch`.
pub fn now_epoch(svm: &LiteSVM) -> u64 {
    let clock: solana_sdk::clock::Clock = svm.get_sysvar();
    clock.epoch
}

/// Inject a synthetic SPL Stake Pool `StakePool` account (owned by the SPL stake-pool program) at
/// the verified byte offsets `stake_pool::parse` reads (account_type@0, total_lamports@258,
/// pool_token_supply@266, last_update_epoch@274). `total_lamports / pool_token_supply` is the
/// canonical SOL/LST rate the C1 leg consumes (both 9-decimal ⇒ a whole-token ratio).
pub fn set_stake_pool(
    svm: &mut LiteSVM,
    key: &Pubkey,
    total_lamports: u64,
    pool_token_supply: u64,
    last_update_epoch: u64,
) {
    let mut data = vec![0u8; 320]; // past min_len (282), mimicking the real account's tail
    data[0] = 1; // AccountType::StakePool
    data[258..266].copy_from_slice(&total_lamports.to_le_bytes());
    data[266..274].copy_from_slice(&pool_token_supply.to_le_bytes());
    data[274..282].copy_from_slice(&last_update_epoch.to_le_bytes());
    set_raw_account(svm, key, data, fusd_core::constants::SPL_STAKE_POOL_PROGRAM_ID);
}

/// As [`set_pyth_price`], but the account is owned by `owner_program` (D3 — to exercise a migrated
/// Pyth receiver program ID: a fresh price posted under the NEW program should be accepted only once
/// `set_oracle_program_ids` points config at it).
#[allow(clippy::too_many_arguments)]
pub fn set_pyth_price_owned(
    svm: &mut LiteSVM,
    key: &Pubkey,
    feed_id: [u8; 32],
    price: i64,
    conf: u64,
    expo: i32,
    publish_ts: i64,
    owner_program: Pubkey,
) {
    use pyth_solana_receiver_sdk::price_update::{
        PriceFeedMessage, PriceUpdateV2, VerificationLevel,
    };
    let pu = PriceUpdateV2 {
        write_authority: Pubkey::new_unique(),
        verification_level: VerificationLevel::Full,
        price_message: PriceFeedMessage {
            feed_id,
            price,
            conf,
            exponent: expo,
            publish_time: publish_ts,
            prev_publish_time: publish_ts.saturating_sub(1),
            ema_price: price,
            ema_conf: conf,
        },
        posted_slot: 0,
    };
    let mut data = Vec::new();
    pu.try_serialize(&mut data).expect("serialize PriceUpdateV2");
    set_raw_account(svm, key, data, owner_program);
}

/// Inject a Pyth update with a caller-chosen `VerificationLevel` (to test the Full-only gate).
#[allow(clippy::too_many_arguments)]
pub fn set_pyth_price_partial(
    svm: &mut LiteSVM,
    key: &Pubkey,
    feed_id: [u8; 32],
    price: i64,
    conf: u64,
    expo: i32,
    publish_ts: i64,
    num_signatures: u8,
) {
    use pyth_solana_receiver_sdk::price_update::{
        PriceFeedMessage, PriceUpdateV2, VerificationLevel,
    };
    let pu = PriceUpdateV2 {
        write_authority: Pubkey::new_unique(),
        verification_level: VerificationLevel::Partial { num_signatures },
        price_message: PriceFeedMessage {
            feed_id,
            price,
            conf,
            exponent: expo,
            publish_time: publish_ts,
            prev_publish_time: publish_ts.saturating_sub(1),
            ema_price: price,
            ema_conf: conf,
        },
        posted_slot: 0,
    };
    let mut data = Vec::new();
    pu.try_serialize(&mut data).expect("serialize PriceUpdateV2");
    set_raw_account(svm, key, data, fusd_core::constants::PYTH_RECEIVER_PROGRAM_ID);
}

/// Inject a Switchboard `PullFeedAccountData` account (owned by the On-Demand program) — the ACTUAL
/// SDK Pod type. `value`/`std_dev` are 1e18-scaled (PRECISION = 18); `slot > 0` marks a live result.
pub fn set_switchboard_feed(
    svm: &mut LiteSVM,
    key: &Pubkey,
    value: i128,
    std_dev: i128,
    slot: u64,
    last_update_ts: i64,
) {
    // Default: a healthy result backed by a quorum (num_samples 3 >= min_responses 1).
    set_switchboard_feed_quorum(svm, key, value, std_dev, slot, last_update_ts, 3, 1);
}

/// As [`set_switchboard_feed`], but with explicit quorum fields so tests can inject a sub-quorum
/// (num_samples < min_responses) result and assert it degrades to "absent" (mints freeze).
#[allow(clippy::too_many_arguments)]
pub fn set_switchboard_feed_quorum(
    svm: &mut LiteSVM,
    key: &Pubkey,
    value: i128,
    std_dev: i128,
    slot: u64,
    last_update_ts: i64,
    num_samples: u8,
    min_responses: u32,
) {
    use switchboard_on_demand::PullFeedAccountData;
    // Switchboard's anchor account discriminator (sha256("account:PullFeedAccountData")[..8]).
    const SB_DISC: [u8; 8] = [196, 27, 108, 196, 10, 215, 219, 40];
    let mut feed: PullFeedAccountData = bytemuck::Zeroable::zeroed();
    feed.result.value = value;
    feed.result.std_dev = std_dev;
    feed.result.slot = slot;
    feed.result.submission_idx = 0;
    feed.result.num_samples = num_samples;
    feed.min_responses = min_responses;
    feed.max_staleness = 1_000_000; // slot staleness is unused on-chain; keep it generous
    feed.last_update_timestamp = last_update_ts;
    let mut data = Vec::with_capacity(8 + core::mem::size_of::<PullFeedAccountData>());
    data.extend_from_slice(&SB_DISC);
    data.extend_from_slice(bytemuck::bytes_of(&feed));
    set_raw_account(svm, key, data, fusd_core::constants::SWITCHBOARD_ON_DEMAND_PROGRAM_ID);
}

/// Inject an Orca Whirlpool pool account (owned by the Whirlpool program) with the given
/// `sqrt_price` (Q64.64) and token pair (a = base, b = quote). Offsets per clmm-pool-layouts.md.
pub fn set_whirlpool_pool(
    svm: &mut LiteSVM,
    key: &Pubkey,
    sqrt_price: u128,
    mint_a: &Pubkey,
    mint_b: &Pubkey,
) {
    const DISC: [u8; 8] = [63, 149, 209, 12, 225, 128, 99, 9];
    let mut data = vec![0u8; 256];
    data[..8].copy_from_slice(&DISC);
    data[65..81].copy_from_slice(&sqrt_price.to_le_bytes());
    data[101..133].copy_from_slice(mint_a.as_ref());
    data[181..213].copy_from_slice(mint_b.as_ref());
    set_raw_account(svm, key, data, fusd_core::constants::ORCA_WHIRLPOOL_PROGRAM_ID);
}

/// Inject a Raydium CLMM `PoolState` account (owned by the Raydium CLMM program) with the given
/// `sqrt_price`, token pair (0/1, sorted), and in-account decimals. Offsets per clmm-pool-layouts.md.
#[allow(clippy::too_many_arguments)]
pub fn set_raydium_pool(
    svm: &mut LiteSVM,
    key: &Pubkey,
    sqrt_price: u128,
    mint_0: &Pubkey,
    mint_1: &Pubkey,
    dec_0: u8,
    dec_1: u8,
) {
    const DISC: [u8; 8] = [247, 237, 227, 245, 215, 195, 222, 70];
    let mut data = vec![0u8; 320];
    data[..8].copy_from_slice(&DISC);
    data[73..105].copy_from_slice(mint_0.as_ref());
    data[105..137].copy_from_slice(mint_1.as_ref());
    data[233] = dec_0;
    data[234] = dec_1;
    data[253..269].copy_from_slice(&sqrt_price.to_le_bytes());
    set_raw_account(svm, key, data, fusd_core::constants::RAYDIUM_CLMM_PROGRAM_ID);
}

/// Number of observations currently retained in the market's DexTwap ring (raw layout read).
pub fn dex_twap_count(svm: &LiteSVM, coll: &Pubkey) -> u64 {
    let n = fusd_core::constants::TWAP_RING_CAPACITY;
    let acct = svm.get_account(&dex_twap_pda(coll)).expect("dex_twap exists");
    let off = 8 + n * 16 + n * 8 + 8; // disc + prices[N]u128 + ts[N]i64 + next(u64)
    u64::from_le_bytes(acct.data[off..off + 8].try_into().unwrap())
}

/// The most recently pushed observation's price (usd_ray) in the ring.
pub fn dex_twap_last_price(svm: &LiteSVM, coll: &Pubkey) -> u128 {
    let n = fusd_core::constants::TWAP_RING_CAPACITY;
    let acct = svm.get_account(&dex_twap_pda(coll)).expect("dex_twap exists");
    let next_off = 8 + n * 16 + n * 8;
    let next = u64::from_le_bytes(acct.data[next_off..next_off + 8].try_into().unwrap()) as usize;
    let idx = (next + n - 1) % n;
    let poff = 8 + idx * 16;
    u128::from_le_bytes(acct.data[poff..poff + 16].try_into().unwrap())
}

/// Build a `sample_twap` (permissionless): sample `clmm_pool` into the market's DexTwap ring.
pub fn sample_twap_ix(cranker: &Pubkey, coll: &Pubkey, clmm_pool: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::SampleTwap {
            cranker: *cranker,
            collateral_mint: *coll,
            market_oracle: market_oracle_pda(coll),
            dex_twap: dex_twap_pda(coll),
            clmm_pool: *clmm_pool,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::SampleTwap {}.data(),
    }
}

/// Build an `update_price` (permissionless). `switchboard_feed` is `None` to omit the SB leg
/// (Anchor optional account → the program treats it as absent ⇒ aggregate freezes mints).
pub fn update_price_ix(
    cranker: &Pubkey,
    coll: &Pubkey,
    pyth_price_update: &Pubkey,
    switchboard_feed: Option<Pubkey>,
) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::UpdatePrice {
            cranker: *cranker,
            config: config_pda(),
            collateral_mint: *coll,
            market: market_pda(coll),
            market_oracle: market_oracle_pda(coll),
            pyth_price_update: *pyth_price_update,
            switchboard_feed,
            dex_twap: dex_twap_pda(coll),
            // Non-LST: the C1 canonical-rate accounts are omitted (see `update_price_lst_ix`).
            sol_usd_pyth_update: None,
            lst_stake_pool: None,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::UpdatePrice {}.data(),
    }
}

/// C1: build an `update_price` for an LST market, supplying the canonical-rate accounts — the
/// SOL/USD Pyth `PriceUpdateV2` and the SPL stake-pool account. `sol_usd_pyth` / `stake_pool` are
/// `None` to deliberately omit a leg (testing the degrade-to-freeze path).
#[allow(clippy::too_many_arguments)]
pub fn update_price_lst_ix(
    cranker: &Pubkey,
    coll: &Pubkey,
    pyth_price_update: &Pubkey,
    switchboard_feed: Option<Pubkey>,
    sol_usd_pyth: Option<Pubkey>,
    stake_pool: Option<Pubkey>,
) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::UpdatePrice {
            cranker: *cranker,
            config: config_pda(),
            collateral_mint: *coll,
            market: market_pda(coll),
            market_oracle: market_oracle_pda(coll),
            pyth_price_update: *pyth_price_update,
            switchboard_feed,
            dex_twap: dex_twap_pda(coll),
            sol_usd_pyth_update: sol_usd_pyth,
            lst_stake_pool: stake_pool,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::UpdatePrice {}.data(),
    }
}

/// D3: update the bounded-updatable oracle program IDs (gov_authority-gated). `None` leaves a field;
/// the alt may be set to `Pubkey::default()` to disable the second accepted Pyth receiver.
pub fn set_oracle_program_ids_ix(
    authority: &Pubkey,
    new_pyth_receiver: Option<Pubkey>,
    new_pyth_receiver_alt: Option<Pubkey>,
    new_switchboard: Option<Pubkey>,
) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::SetOracleProgramIds {
            authority: *authority,
            config: config_pda(),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::SetOracleProgramIds {
            new_pyth_receiver,
            new_pyth_receiver_alt,
            new_switchboard,
        }
        .data(),
    }
}

/// D3: rebind a market's oracle feed sources (gov_authority-gated).
pub fn rebind_market_oracle_feeds_ix(
    authority: &Pubkey,
    coll: &Pubkey,
    args: fusd_core::instructions::oracle_admin::RebindOracleFeedsArgs,
) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::RebindMarketOracleFeeds {
            authority: *authority,
            config: config_pda(),
            collateral_mint: *coll,
            market_oracle: market_oracle_pda(coll),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::RebindMarketOracleFeeds { args }.data(),
    }
}

/// Create a fresh no-freeze SPL mint with the given decimals (the USD-stable quote leg for a
/// market oracle). Returns the mint pubkey; the mint authority is a throwaway keypair.
pub fn create_quote_mint(svm: &mut LiteSVM, payer: &Keypair, decimals: u8) -> Pubkey {
    let kp = Keypair::new();
    let auth = Keypair::new();
    create_mint(svm, payer, &kp, decimals, &auth.pubkey(), /*freeze=*/ false);
    kp.pubkey()
}

/// Pubkeys produced by [`bootstrap_oracle`] — the fixtures a crank test injects into.
pub struct OracleHandles {
    pub quote: Pubkey,
    pub orca_pool: Pubkey,
    pub raydium_pool: Pubkey,
    pub pyth: Pubkey,
    pub sb: Pubkey,
    pub feed_id: [u8; 32],
}

/// Bind a market's oracle for the crank tests: creates a 6-dec quote mint and configures the
/// feeds with the recommended threshold defaults but caller-chosen TWAP guards (so a test can use
/// a short, fast-to-fill window). `raydium = true` configures the Raydium pool slot instead of Orca.
/// The C6 plausibility band is OFF (0, 0).
pub fn bootstrap_oracle(
    svm: &mut LiteSVM,
    gov: &Keypair,
    coll: &Pubkey,
    twap_window_secs: i64,
    twap_min_samples: u32,
    twap_max_staleness_secs: i64,
    raydium: bool,
) -> OracleHandles {
    bootstrap_oracle_banded(svm, gov, coll, twap_window_secs, twap_min_samples, twap_max_staleness_secs, raydium, 0, 0)
}

/// As [`bootstrap_oracle`], but with an explicit C6 plausibility band (`band_*_ray`, RAY-scaled
/// USD/token; `(0, 0)` = off). usd_ray for `$X` is `X · fusd_math::RAY`.
#[allow(clippy::too_many_arguments)]
pub fn bootstrap_oracle_banded(
    svm: &mut LiteSVM,
    gov: &Keypair,
    coll: &Pubkey,
    twap_window_secs: i64,
    twap_min_samples: u32,
    twap_max_staleness_secs: i64,
    raydium: bool,
    band_lower_ray: u128,
    band_upper_ray: u128,
) -> OracleHandles {
    bootstrap_oracle_full(
        svm, gov, coll, twap_window_secs, twap_min_samples, twap_max_staleness_secs, raydium,
        band_lower_ray, band_upper_ray, 0,
    )
}

/// As [`bootstrap_oracle_banded`], but also sets the B3 liquidation-divergence threshold
/// (`liq_max_divergence_bps`; `0` = off). The most general oracle bootstrap.
#[allow(clippy::too_many_arguments)]
pub fn bootstrap_oracle_full(
    svm: &mut LiteSVM,
    gov: &Keypair,
    coll: &Pubkey,
    twap_window_secs: i64,
    twap_min_samples: u32,
    twap_max_staleness_secs: i64,
    raydium: bool,
    band_lower_ray: u128,
    band_upper_ray: u128,
    liq_max_divergence_bps: u16,
) -> OracleHandles {
    use fusd_core::constants::{
        DEFAULT_ORACLE_CONF_BPS, DEFAULT_ORACLE_DEVIATION_BPS, DEFAULT_ORACLE_K_BPS,
        DEFAULT_ORACLE_MAX_AGE_SECS, DEFAULT_TWAP_DIVERGENCE_BPS,
    };
    let quote = create_quote_mint(svm, gov, FUSD_DECIMALS);
    let pool = Pubkey::new_unique();
    let (orca_pool, raydium_pool) =
        if raydium { (Pubkey::default(), pool) } else { (pool, Pubkey::default()) };
    let pyth = Pubkey::new_unique();
    let sb = Pubkey::new_unique();
    let feed_id = [7u8; 32];
    let args = InitMarketOracleArgs {
        pyth_feed_id: feed_id,
        switchboard_feed: sb,
        orca_pool,
        raydium_pool,
        max_conf_bps: DEFAULT_ORACLE_CONF_BPS,
        max_deviation_bps: DEFAULT_ORACLE_DEVIATION_BPS,
        twap_max_divergence_bps: DEFAULT_TWAP_DIVERGENCE_BPS,
        max_age_secs: DEFAULT_ORACLE_MAX_AGE_SECS,
        k_bps: DEFAULT_ORACLE_K_BPS,
        twap_window_secs,
        twap_min_samples,
        twap_max_staleness_secs,
        price_band_lower_ray: band_lower_ray,
        price_band_upper_ray: band_upper_ray,
        liq_max_divergence_bps,
        lst_stake_pool: Pubkey::default(),
    };
    send(svm, &[init_market_oracle_ix(&gov.pubkey(), coll, &quote, args)], gov, &[])
        .expect("init_market_oracle");
    OracleHandles { quote, orca_pool, raydium_pool, pyth, sb, feed_id }
}

/// C1: as [`bootstrap_oracle`], but binds an SPL stake pool so this is an LST market (Orca venue,
/// band/liq off). Returns the handles plus the bound `lst_stake_pool` key. The collateral mint is
/// 9-decimal (`COLL_DECIMALS`), satisfying the init-time LST decimal check.
pub fn bootstrap_oracle_lst(
    svm: &mut LiteSVM,
    gov: &Keypair,
    coll: &Pubkey,
    twap_window_secs: i64,
    twap_min_samples: u32,
    twap_max_staleness_secs: i64,
) -> (OracleHandles, Pubkey) {
    use fusd_core::constants::{
        DEFAULT_ORACLE_CONF_BPS, DEFAULT_ORACLE_DEVIATION_BPS, DEFAULT_ORACLE_K_BPS,
        DEFAULT_ORACLE_MAX_AGE_SECS, DEFAULT_TWAP_DIVERGENCE_BPS,
    };
    let quote = create_quote_mint(svm, gov, FUSD_DECIMALS);
    let orca_pool = Pubkey::new_unique();
    let pyth = Pubkey::new_unique();
    let sb = Pubkey::new_unique();
    let stake_pool = Pubkey::new_unique();
    let feed_id = [7u8; 32];
    let args = InitMarketOracleArgs {
        pyth_feed_id: feed_id,
        switchboard_feed: sb,
        orca_pool,
        raydium_pool: Pubkey::default(),
        max_conf_bps: DEFAULT_ORACLE_CONF_BPS,
        max_deviation_bps: DEFAULT_ORACLE_DEVIATION_BPS,
        twap_max_divergence_bps: DEFAULT_TWAP_DIVERGENCE_BPS,
        max_age_secs: DEFAULT_ORACLE_MAX_AGE_SECS,
        k_bps: DEFAULT_ORACLE_K_BPS,
        twap_window_secs,
        twap_min_samples,
        twap_max_staleness_secs,
        price_band_lower_ray: 0,
        price_band_upper_ray: 0,
        liq_max_divergence_bps: 0,
        lst_stake_pool: stake_pool,
    };
    send(svm, &[init_market_oracle_ix(&gov.pubkey(), coll, &quote, args)], gov, &[])
        .expect("init_market_oracle (LST)");
    (OracleHandles { quote, orca_pool, raydium_pool: Pubkey::default(), pyth, sb, feed_id }, stake_pool)
}

pub fn dev_set_price_ix(gov: &Pubkey, coll: &Pubkey, spot: u128) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::DevSetPrice {
            authority: *gov,
            config: config_pda(),
            market: market_pda(coll),
        }
        .to_account_metas(None),
        data: fusd_core::instruction::DevSetPrice { spot }.data(),
    }
}

// ---- GovernanceGate + timelock builders (the bounded two-speed governance path) ----

pub fn init_governance_gate_ix(
    authority: &Pubkey,
    inbound_authority: &Pubkey,
    timelock_secs: i64,
) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::InitGovernanceGate {
            authority: *authority,
            config: config_pda(),
            gov_gate: gov_gate_pda(),
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::InitGovernanceGate {
            inbound_authority: *inbound_authority,
            timelock_secs,
        }
        .data(),
    }
}

pub fn migrate_inbound_authority_ix(authority: &Pubkey, new_authority: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::MigrateInboundAuthority {
            authority: *authority,
            gov_gate: gov_gate_pda(),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::MigrateInboundAuthority { new_authority: *new_authority }.data(),
    }
}

/// STEP 2 of the two-step handoff: the proposed successor signs to accept the inbound authority.
pub fn accept_inbound_authority_ix(new_authority: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::AcceptInboundAuthority {
            new_authority: *new_authority,
            gov_gate: gov_gate_pda(),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::AcceptInboundAuthority {}.data(),
    }
}

/// QUEUE a clamped param change (gated on `gov_gate.inbound_authority`). `nonce` must be the
/// gate's current `queue_nonce` (the op's PDA seed).
pub fn queue_param_change_ix(
    authority: &Pubkey,
    coll: &Pubkey,
    nonce: u64,
    param: MarketParam,
    value: u64,
) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::QueueParamChange {
            authority: *authority,
            gov_gate: gov_gate_pda(),
            market: market_pda(coll),
            timelocked_param: timelock_pda(nonce),
            system_program: system_program::ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::QueueParamChange { param, value }.data(),
    }
}

/// EXECUTE a queued change (permissionless; `executor` is any signer, receives the op's rent).
pub fn execute_param_change_ix(executor: &Pubkey, coll: &Pubkey, nonce: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::ExecuteParamChange {
            executor: *executor,
            market: market_pda(coll),
            timelocked_param: timelock_pda(nonce),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::ExecuteParamChange {}.data(),
    }
}

/// CANCEL a queued change before it executes (gated on `gov_gate.inbound_authority`).
pub fn cancel_param_change_ix(authority: &Pubkey, nonce: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::CancelParamChange {
            authority: *authority,
            gov_gate: gov_gate_pda(),
            timelocked_param: timelock_pda(nonce),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::CancelParamChange {}.data(),
    }
}

// ============================ global backstop reserve ============================

/// Create the global backstop reserve + its fUSD vault (gated on `config.gov_authority`).
pub fn init_global_backstop_ix(authority: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::InitGlobalBackstop {
            authority: *authority,
            config: config_pda(),
            fusd_mint: fusd_mint_pda(),
            backstop: backstop_pda(),
            backstop_fusd_vault: backstop_fusd_vault_pda(),
            token_program: SPL_TOKEN_ID,
            system_program: system_program::ID,
            rent: rent::ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::InitGlobalBackstop {}.data(),
    }
}

// ---- supply reconciliation (proof-of-reserves) ----

pub fn supply_recon_pda() -> Pubkey {
    pda(&[b"supply_recon"])
}

pub fn read_supply_recon(svm: &LiteSVM) -> fusd_core::state::SupplyReconciliation {
    let acct = svm.get_account(&supply_recon_pda()).unwrap();
    fusd_core::state::SupplyReconciliation::try_deserialize(&mut acct.data.as_slice()).unwrap()
}

pub fn init_supply_reconciliation_ix(authority: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::InitSupplyReconciliation {
            authority: *authority,
            config: config_pda(),
            supply_recon: supply_recon_pda(),
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::InitSupplyReconciliation {}.data(),
    }
}

/// `reconcile_supply` over the given market PDAs (passed as remaining_accounts, all writable=false).
pub fn reconcile_supply_ix(cranker: &Pubkey, markets: &[Pubkey]) -> Instruction {
    let mut metas = fusd_core::accounts::ReconcileSupply {
        cranker: *cranker,
        fusd_mint: fusd_mint_pda(),
        supply_recon: supply_recon_pda(),
    }
    .to_account_metas(None);
    for m in markets {
        metas.push(anchor_lang::solana_program::instruction::AccountMeta::new_readonly(*m, false));
    }
    Instruction {
        program_id: fusd_core::ID,
        accounts: metas,
        data: fusd_core::instruction::ReconcileSupply {}.data(),
    }
}

// ---- debt-ceiling auto-line (Maker DC-IAM) ----

pub fn debt_ceiling_line_pda(coll: &Pubkey) -> Pubkey {
    pda(&[b"ratelimit", coll.as_ref()])
}

pub fn read_debt_ceiling_line(svm: &LiteSVM, coll: &Pubkey) -> fusd_core::state::DebtCeilingLine {
    let acct = svm.get_account(&debt_ceiling_line_pda(coll)).unwrap();
    fusd_core::state::DebtCeilingLine::try_deserialize(&mut acct.data.as_slice()).unwrap()
}

pub fn init_debt_ceiling_line_ix(authority: &Pubkey, coll: &Pubkey, line: u64, gap: u64, ttl: i64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::InitDebtCeilingLine {
            authority: *authority,
            config: config_pda(),
            collateral_mint: *coll,
            market: market_pda(coll),
            debt_ceiling_line: debt_ceiling_line_pda(coll),
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::InitDebtCeilingLine { line, gap, ttl }.data(),
    }
}

pub fn set_debt_ceiling_line_ix(authority: &Pubkey, coll: &Pubkey, line: u64, gap: u64, ttl: i64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::SetDebtCeilingLine {
            authority: *authority,
            config: config_pda(),
            collateral_mint: *coll,
            market: market_pda(coll),
            debt_ceiling_line: debt_ceiling_line_pda(coll),
        }
        .to_account_metas(None),
        data: fusd_core::instruction::SetDebtCeilingLine { line, gap, ttl }.data(),
    }
}

pub fn bump_debt_ceiling_ix(cranker: &Pubkey, coll: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::BumpDebtCeiling {
            cranker: *cranker,
            collateral_mint: *coll,
            market: market_pda(coll),
            debt_ceiling_line: debt_ceiling_line_pda(coll),
        }
        .to_account_metas(None),
        data: fusd_core::instruction::BumpDebtCeiling {}.data(),
    }
}

/// Permissionless top-up of the reserve from `funder_fusd_ata`.
pub fn fund_backstop_ix(funder: &Pubkey, funder_fusd_ata: &Pubkey, amount: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::FundBackstop {
            funder: *funder,
            backstop: backstop_pda(),
            backstop_fusd_vault: backstop_fusd_vault_pda(),
            fusd_mint: fusd_mint_pda(),
            funder_fusd_ata: *funder_fusd_ata,
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::FundBackstop { amount }.data(),
    }
}

/// Governance withdraws above-cap excess to `recipient_fusd_ata` (gated on `gov_gate.inbound_authority`).
pub fn withdraw_backstop_excess_ix(
    authority: &Pubkey,
    recipient_fusd_ata: &Pubkey,
    amount: u64,
) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::WithdrawBackstopExcess {
            authority: *authority,
            gov_gate: gov_gate_pda(),
            backstop: backstop_pda(),
            backstop_fusd_vault: backstop_fusd_vault_pda(),
            fusd_mint: fusd_mint_pda(),
            recipient_fusd_ata: *recipient_fusd_ata,
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::WithdrawBackstopExcess { amount }.data(),
    }
}

/// QUEUE a clamped global-param change (gated on `gov_gate.inbound_authority`); `nonce` = the gate's
/// current `queue_nonce`.
pub fn queue_global_param_ix(authority: &Pubkey, nonce: u64, param: GlobalParam, value: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::QueueGlobalParamChange {
            authority: *authority,
            gov_gate: gov_gate_pda(),
            timelocked_param: global_timelock_pda(nonce),
            system_program: system_program::ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::QueueGlobalParamChange { param, value }.data(),
    }
}

/// EXECUTE a queued global-param change (permissionless after the timelock).
pub fn execute_global_param_ix(executor: &Pubkey, nonce: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::ExecuteGlobalParamChange {
            executor: *executor,
            backstop: backstop_pda(),
            timelocked_param: global_timelock_pda(nonce),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::ExecuteGlobalParamChange {}.data(),
    }
}

/// CANCEL a queued global-param change (gated on `gov_gate.inbound_authority`).
pub fn cancel_global_param_ix(authority: &Pubkey, nonce: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::CancelGlobalParamChange {
            authority: *authority,
            gov_gate: gov_gate_pda(),
            timelocked_param: global_timelock_pda(nonce),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::CancelGlobalParamChange {}.data(),
    }
}

/// Apply a `GlobalParam` immediately via the gate (timelock 0 ⇒ queue then execute at the same
/// instant). The gate must already be inited (`init_gov_gate`) and the backstop created.
pub fn gov_set_global_param(svm: &mut LiteSVM, gov: &Keypair, param: GlobalParam, value: u64) {
    let nonce = read_gov_gate(svm).queue_nonce;
    send(svm, &[queue_global_param_ix(&gov.pubkey(), nonce, param, value)], gov, &[])
        .expect("queue_global_param");
    send(svm, &[execute_global_param_ix(&gov.pubkey(), nonce)], gov, &[])
        .expect("execute_global_param");
}

/// The independent guardian pauses NEW borrowing on a market for `pause_secs` (0 lifts early).
/// Gated on `ProtocolConfig.guardian`.
pub fn guardian_derisk_ix(guardian: &Pubkey, coll: &Pubkey, pause_secs: i64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::GuardianDerisk {
            guardian: *guardian,
            config: config_pda(),
            collateral_mint: *coll,
            market: market_pda(coll),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::GuardianDerisk { pause_secs }.data(),
    }
}

/// Governance (`gov_authority`) rotates/revokes the de-risk guardian.
pub fn set_guardian_ix(authority: &Pubkey, new_guardian: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::SetGuardian {
            authority: *authority,
            config: config_pda(),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::SetGuardian { new_guardian: *new_guardian }.data(),
    }
}

pub fn read_protocol_config(svm: &LiteSVM) -> fusd_core::state::ProtocolConfig {
    let acct = svm.get_account(&config_pda()).unwrap();
    fusd_core::state::ProtocolConfig::try_deserialize(&mut acct.data.as_slice()).unwrap()
}

/// STEP 1 of the two-step admin handoff: the current `gov_authority` proposes a successor
/// (`Pubkey::default()` cancels a pending handoff).
pub fn migrate_gov_authority_ix(authority: &Pubkey, new_authority: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::MigrateGovAuthority {
            authority: *authority,
            config: config_pda(),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::MigrateGovAuthority { new_authority: *new_authority }.data(),
    }
}

/// STEP 2: the proposed successor signs to take the admin seat.
pub fn accept_gov_authority_ix(new_authority: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::AcceptGovAuthority {
            new_authority: *new_authority,
            config: config_pda(),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::AcceptGovAuthority {}.data(),
    }
}

/// Init the GovernanceGate with `inbound_authority = gov` and timelock 0 (immediate execute).
pub fn init_gov_gate(svm: &mut LiteSVM, gov: &Keypair) {
    send(svm, &[init_governance_gate_ix(&gov.pubkey(), &gov.pubkey(), 0)], gov, &[])
        .expect("init_governance_gate");
}

/// Apply a `MarketParam` immediately via the gate (timelock 0 ⇒ queue then execute at the same
/// instant). The gate must already be inited (`init_gov_gate`).
pub fn gov_set_param(svm: &mut LiteSVM, gov: &Keypair, coll: &Pubkey, param: MarketParam, value: u64) {
    let nonce = read_gov_gate(svm).queue_nonce;
    send(svm, &[queue_param_change_ix(&gov.pubkey(), coll, nonce, param, value)], gov, &[])
        .expect("queue_param_change");
    send(svm, &[execute_param_change_ix(&gov.pubkey(), coll, nonce)], gov, &[])
        .expect("execute_param_change");
}

/// Init the gate and set the market's net-outflow rate-limit cap (fUSD-native; 0 disables).
pub fn enable_rate_limit(svm: &mut LiteSVM, gov: &Keypair, coll: &Pubkey, cap: u64) {
    init_gov_gate(svm, gov);
    gov_set_param(svm, gov, coll, MarketParam::RateLimitCap, cap);
}

/// Init the gate and set the market's liquidation bonus collar (bps; 0 = collar off / seize-all).
pub fn enable_liq_collar(svm: &mut LiteSVM, gov: &Keypair, coll: &Pubkey, bonus_bps: u64) {
    init_gov_gate(svm, gov);
    gov_set_param(svm, gov, coll, MarketParam::LiqBonus, bonus_bps);
}

/// Init the gate and set the market's CCR borrow-restriction band threshold (bps; 0 disables).
pub fn enable_ccr(svm: &mut LiteSVM, gov: &Keypair, coll: &Pubkey, ccr_bps: u64) {
    init_gov_gate(svm, gov);
    gov_set_param(svm, gov, coll, MarketParam::Ccr, ccr_bps);
}

/// Permissionless: terminally wind a failing market down (TCR < SCR or sustained oracle failure).
pub fn shutdown_ix(cranker: &Pubkey, coll: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::Shutdown {
            cranker: *cranker,
            collateral_mint: *coll,
            market: market_pda(coll),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::Shutdown {}.data(),
    }
}

/// Build an `urgent_redeem` (shutdown wind-down), appending candidate positions as writable
/// `remaining_accounts` (any bucket; unordered).
#[allow(clippy::too_many_arguments)]
pub fn urgent_redeem_ix(
    redeemer: &Pubkey,
    coll: &Pubkey,
    redeemer_fusd_ata: &Pubkey,
    redeemer_coll_ata: &Pubkey,
    candidates: &[Pubkey],
    amount: u64,
) -> Instruction {
    let mut accounts = fusd_core::accounts::UrgentRedeem {
        redeemer: *redeemer,
        collateral_mint: *coll,
        market: market_pda(coll),
        redemption_bitmap: redemption_bitmap_pda(coll),
        fusd_mint: fusd_mint_pda(),
        market_coll_vault: coll_vault_pda(coll),
        redeemer_fusd_ata: *redeemer_fusd_ata,
        redeemer_collateral_ata: *redeemer_coll_ata,
        token_program: SPL_TOKEN_ID,
        event_authority: event_authority_pda(),
        program: fusd_core::ID,
    }
    .to_account_metas(None);
    for c in candidates {
        accounts.push(AccountMeta::new(*c, false));
    }
    Instruction {
        program_id: fusd_core::ID,
        accounts,
        data: fusd_core::instruction::UrgentRedeem { amount }.data(),
    }
}

pub fn open_position_ix(owner: &Pubkey, coll: &Pubkey, user_rate_bps: u16) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::OpenPosition {
            owner: *owner,
            collateral_mint: *coll,
            market: market_pda(coll),
            position: position_pda(coll, owner),
            system_program: system_program::ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::OpenPosition { args: OpenPositionArgs { user_rate_bps } }
            .data(),
    }
}

pub fn deposit_ix(owner: &Pubkey, coll: &Pubkey, owner_coll_ata: &Pubkey, amount: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::Deposit {
            owner: *owner,
            collateral_mint: *coll,
            market: market_pda(coll),
            position: position_pda(coll, owner),
            owner_collateral_ata: *owner_coll_ata,
            collateral_vault: coll_vault_pda(coll),
            redemption_bitmap: redemption_bitmap_pda(coll),
            token_program: SPL_TOKEN_ID,
            system_program: system_program::ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::Deposit { amount }.data(),
    }
}

pub fn withdraw_ix(owner: &Pubkey, coll: &Pubkey, owner_coll_ata: &Pubkey, amount: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::Withdraw {
            owner: *owner,
            collateral_mint: *coll,
            market: market_pda(coll),
            position: position_pda(coll, owner),
            collateral_vault: coll_vault_pda(coll),
            owner_collateral_ata: *owner_coll_ata,
            redemption_bitmap: redemption_bitmap_pda(coll),
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::Withdraw { amount }.data(),
    }
}

pub fn claim_coll_surplus_ix(owner: &Pubkey, coll: &Pubkey, owner_coll_ata: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::ClaimCollSurplus {
            owner: *owner,
            collateral_mint: *coll,
            market: market_pda(coll),
            position: position_pda(coll, owner),
            collateral_vault: coll_vault_pda(coll),
            owner_collateral_ata: *owner_coll_ata,
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::ClaimCollSurplus {}.data(),
    }
}

/// Governance: withdraw redemption-fee surplus collateral to a recipient ATA.
pub fn withdraw_surplus_ix(authority: &Pubkey, coll: &Pubkey, recipient_ata: &Pubkey, amount: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::WithdrawSurplus {
            authority: *authority,
            collateral_mint: *coll,
            market: market_pda(coll),
            gov_gate: gov_gate_pda(),
            market_coll_vault: coll_vault_pda(coll),
            recipient: *recipient_ata,
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::WithdrawSurplus { amount }.data(),
    }
}

/// Governance: sweep retained protocol-owned (un-homed) collateral to a recipient ATA.
pub fn sweep_protocol_collateral_ix(authority: &Pubkey, coll: &Pubkey, recipient_ata: &Pubkey, amount: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::SweepProtocolCollateral {
            authority: *authority,
            collateral_mint: *coll,
            market: market_pda(coll),
            gov_gate: gov_gate_pda(),
            market_coll_vault: coll_vault_pda(coll),
            recipient: *recipient_ata,
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::SweepProtocolCollateral { amount }.data(),
    }
}

/// Governance: burn fUSD from the authority to retire realized bad debt.
pub fn settle_bad_debt_ix(authority: &Pubkey, coll: &Pubkey, authority_fusd_ata: &Pubkey, amount: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::SettleBadDebt {
            authority: *authority,
            collateral_mint: *coll,
            market: market_pda(coll),
            gov_gate: gov_gate_pda(),
            fusd_mint: fusd_mint_pda(),
            authority_fusd_ata: *authority_fusd_ata,
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::SettleBadDebt { amount }.data(),
    }
}


pub fn borrow_ix(owner: &Pubkey, coll: &Pubkey, owner_fusd_ata: &Pubkey, amount: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::Borrow {
            owner: *owner,
            collateral_mint: *coll,
            market: market_pda(coll),
            position: position_pda(coll, owner),
            fusd_mint: fusd_mint_pda(),
            mint_authority: mint_authority_pda(),
            owner_fusd_ata: *owner_fusd_ata,
            redemption_bitmap: redemption_bitmap_pda(coll),
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::Borrow { amount }.data(),
    }
}

pub fn repay_ix(owner: &Pubkey, coll: &Pubkey, owner_fusd_ata: &Pubkey, amount: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::Repay {
            owner: *owner,
            collateral_mint: *coll,
            market: market_pda(coll),
            position: position_pda(coll, owner),
            fusd_mint: fusd_mint_pda(),
            owner_fusd_ata: *owner_fusd_ata,
            redemption_bitmap: redemption_bitmap_pda(coll),
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::Repay { amount }.data(),
    }
}

pub fn adjust_rate_ix(owner: &Pubkey, coll: &Pubkey, new_rate_bps: u16) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::AdjustRate {
            owner: *owner,
            collateral_mint: *coll,
            market: market_pda(coll),
            position: position_pda(coll, owner),
            redemption_bitmap: redemption_bitmap_pda(coll),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::AdjustRate { new_rate_bps }.data(),
    }
}

/// Build a `redeem`, appending the candidate positions as writable `remaining_accounts`.
#[allow(clippy::too_many_arguments)]
pub fn redeem_ix(
    redeemer: &Pubkey,
    coll: &Pubkey,
    redeemer_fusd_ata: &Pubkey,
    redeemer_coll_ata: &Pubkey,
    candidates: &[Pubkey],
    amount: u64,
) -> Instruction {
    let mut accounts = fusd_core::accounts::Redeem {
        redeemer: *redeemer,
        collateral_mint: *coll,
        market: market_pda(coll),
        redemption_bitmap: redemption_bitmap_pda(coll),
        fusd_mint: fusd_mint_pda(),
        market_coll_vault: coll_vault_pda(coll),
        redeemer_fusd_ata: *redeemer_fusd_ata,
        redeemer_collateral_ata: *redeemer_coll_ata,
        token_program: SPL_TOKEN_ID,
        event_authority: event_authority_pda(),
        program: fusd_core::ID,
    }
    .to_account_metas(None);
    for c in candidates {
        accounts.push(AccountMeta::new(*c, false));
    }
    Instruction { program_id: fusd_core::ID, accounts, data: fusd_core::instruction::Redeem { amount }.data() }
}

pub fn init_reactor_pool_ix(authority: &Pubkey, coll: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::InitReactorPool {
            authority: *authority,
            config: config_pda(),
            collateral_mint: *coll,
            fusd_mint: fusd_mint_pda(),
            market: market_pda(coll),
            reactor_pool: reactor_pool_pda(coll),
            epoch_to_scale_to_sum: ess_pda(coll),
            reactor_fusd_vault: reactor_fusd_vault_pda(coll),
            reactor_coll_vault: reactor_coll_vault_pda(coll),
            token_program: SPL_TOKEN_ID,
            system_program: system_program::ID,
            rent: rent::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::InitReactorPool {}.data(),
    }
}

pub fn init_insurance_buffer_ix(authority: &Pubkey, coll: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::InitInsuranceBuffer {
            authority: *authority,
            config: config_pda(),
            collateral_mint: *coll,
            fusd_mint: fusd_mint_pda(),
            market: market_pda(coll),
            insurance_buffer: buffer_pda(coll),
            buffer_fusd_vault: buffer_fusd_vault_pda(coll),
            token_program: SPL_TOKEN_ID,
            system_program: system_program::ID,
            rent: rent::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::InitInsuranceBuffer {}.data(),
    }
}

pub fn fund_buffer_ix(
    funder: &Pubkey,
    coll: &Pubkey,
    funder_fusd_ata: &Pubkey,
    amount: u64,
) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::FundBuffer {
            funder: *funder,
            collateral_mint: *coll,
            insurance_buffer: buffer_pda(coll),
            buffer_fusd_vault: buffer_fusd_vault_pda(coll),
            fusd_mint: fusd_mint_pda(),
            funder_fusd_ata: *funder_fusd_ata,
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::FundBuffer { amount }.data(),
    }
}

/// Current fUSD balance held in a market's insurance buffer vault.
pub fn buffer_balance(svm: &LiteSVM, coll: &Pubkey) -> u64 {
    token_balance(svm, &buffer_fusd_vault_pda(coll))
}

pub fn read_insurance_buffer(svm: &LiteSVM, coll: &Pubkey) -> fusd_core::state::InsuranceBuffer {
    let acct = svm.get_account(&buffer_pda(coll)).expect("insurance buffer exists");
    fusd_core::state::InsuranceBuffer::try_deserialize(&mut &acct.data[..]).unwrap()
}

pub fn open_reactor_deposit_ix(owner: &Pubkey, coll: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::OpenReactorDeposit {
            owner: *owner,
            collateral_mint: *coll,
            reactor_pool: reactor_pool_pda(coll),
            reactor_deposit: reactor_deposit_pda(coll, owner),
            system_program: system_program::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::OpenReactorDeposit {}.data(),
    }
}

pub fn provide_to_reactor_ix(
    owner: &Pubkey,
    coll: &Pubkey,
    owner_fusd_ata: &Pubkey,
    amount: u64,
) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::ProvideToReactor {
            owner: *owner,
            collateral_mint: *coll,
            fusd_mint: fusd_mint_pda(),
            reactor_pool: reactor_pool_pda(coll),
            epoch_to_scale_to_sum: ess_pda(coll),
            reactor_deposit: reactor_deposit_pda(coll, owner),
            owner_fusd_ata: *owner_fusd_ata,
            reactor_fusd_vault: reactor_fusd_vault_pda(coll),
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::ProvideToReactor { amount }.data(),
    }
}

pub fn withdraw_from_reactor_ix(
    owner: &Pubkey,
    coll: &Pubkey,
    owner_fusd_ata: &Pubkey,
    amount: u64,
) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::WithdrawFromReactor {
            owner: *owner,
            collateral_mint: *coll,
            fusd_mint: fusd_mint_pda(),
            reactor_pool: reactor_pool_pda(coll),
            epoch_to_scale_to_sum: ess_pda(coll),
            reactor_deposit: reactor_deposit_pda(coll, owner),
            reactor_fusd_vault: reactor_fusd_vault_pda(coll),
            owner_fusd_ata: *owner_fusd_ata,
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::WithdrawFromReactor { amount }.data(),
    }
}

pub fn claim_reactor_gains_ix(owner: &Pubkey, coll: &Pubkey, owner_coll_ata: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::ClaimReactorGains {
            owner: *owner,
            collateral_mint: *coll,
            reactor_pool: reactor_pool_pda(coll),
            epoch_to_scale_to_sum: ess_pda(coll),
            reactor_deposit: reactor_deposit_pda(coll, owner),
            reactor_coll_vault: reactor_coll_vault_pda(coll),
            owner_collateral_ata: *owner_coll_ata,
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::ClaimReactorGains {}.data(),
    }
}

/// `liquidator` may be anyone; `victim_position` is the under-MCR position's PDA;
/// `liquidator_collateral_ata` receives the gas-comp skim.
pub fn liquidate_ix(
    liquidator: &Pubkey,
    coll: &Pubkey,
    victim_position: &Pubkey,
    liquidator_collateral_ata: &Pubkey,
) -> Instruction {
    liquidate_ix_full(liquidator, coll, victim_position, liquidator_collateral_ata, /*with_backstop=*/ false)
}

/// `liquidate` with explicit control over whether the global-backstop accounts are supplied
/// (`with_backstop = true` enables the tier-3.5 draw; requires the reserve to be inited).
pub fn liquidate_ix_full(
    liquidator: &Pubkey,
    coll: &Pubkey,
    victim_position: &Pubkey,
    liquidator_collateral_ata: &Pubkey,
    with_backstop: bool,
) -> Instruction {
    let (backstop, backstop_fusd_vault) = if with_backstop {
        (Some(backstop_pda()), Some(backstop_fusd_vault_pda()))
    } else {
        (None, None)
    };
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::Liquidate {
            liquidator: *liquidator,
            collateral_mint: *coll,
            market: market_pda(coll),
            position: *victim_position,
            reactor_pool: reactor_pool_pda(coll),
            epoch_to_scale_to_sum: ess_pda(coll),
            market_coll_vault: coll_vault_pda(coll),
            reactor_fusd_vault: reactor_fusd_vault_pda(coll),
            reactor_coll_vault: reactor_coll_vault_pda(coll),
            fusd_mint: fusd_mint_pda(),
            liquidator_collateral_ata: *liquidator_collateral_ata,
            redemption_bitmap: redemption_bitmap_pda(coll),
            insurance_buffer: buffer_pda(coll),
            buffer_fusd_vault: buffer_fusd_vault_pda(coll),
            backstop,
            backstop_fusd_vault,
            token_program: SPL_TOKEN_ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::Liquidate {}.data(),
    }
}

/// Liquidate `victim_position`, ensuring `liquidator` has a collateral ATA (created on demand) to
/// receive the gas-comp. `liquidator` is the fee payer + signer. Returns the raw tx result so
/// callers can `.expect(..)` / `.expect_err(..)`.
pub fn liquidate(
    svm: &mut LiteSVM,
    liquidator: &Keypair,
    coll: &Pubkey,
    victim_position: &Pubkey,
) -> TransactionResult {
    let ata = spl_associated_token_account::get_associated_token_address(&liquidator.pubkey(), coll);
    if svm.get_account(&ata).is_none() {
        let create = spl_associated_token_account::instruction::create_associated_token_account(
            &liquidator.pubkey(),
            &liquidator.pubkey(),
            coll,
            &SPL_TOKEN_ID,
        );
        send(svm, &[create], liquidator, &[]).expect("create liquidator collateral ATA failed");
    }
    send(svm, &[liquidate_ix(&liquidator.pubkey(), coll, victim_position, &ata)], liquidator, &[])
}

/// As [`liquidate`], but supplies the global-backstop accounts so the tier-3.5 draw can fire.
pub fn liquidate_with_backstop(
    svm: &mut LiteSVM,
    liquidator: &Keypair,
    coll: &Pubkey,
    victim_position: &Pubkey,
) -> TransactionResult {
    let ata = spl_associated_token_account::get_associated_token_address(&liquidator.pubkey(), coll);
    if svm.get_account(&ata).is_none() {
        let create = spl_associated_token_account::instruction::create_associated_token_account(
            &liquidator.pubkey(),
            &liquidator.pubkey(),
            coll,
            &SPL_TOKEN_ID,
        );
        send(svm, &[create], liquidator, &[]).expect("create liquidator collateral ATA failed");
    }
    send(svm, &[liquidate_ix_full(&liquidator.pubkey(), coll, victim_position, &ata, true)], liquidator, &[])
}

pub fn close_position_ix(owner: &Pubkey, coll: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::ClosePosition {
            owner: *owner,
            collateral_mint: *coll,
            position: position_pda(coll, owner),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::ClosePosition {}.data(),
    }
}

// ============================ scenario bootstrap ============================

/// Initialize the protocol, create a fresh no-freeze collateral mint, onboard it as a market
/// (MCR 150%, 0% interest, $1M ceiling), and create its Reactor Pool. Returns the collateral
/// mint pubkey. `gov` is the governance authority + fee payer; `coll_mint_auth` holds the
/// collateral mint authority (used later to fund borrowers).
pub fn bootstrap_market(svm: &mut LiteSVM, gov: &Keypair, coll_mint_auth: &Keypair) -> Pubkey {
    // Default test market: NO reserve / gas-comp (so liquidation-distribution tests see exactly the
    // seized collateral). Interest is per-position (`user_rate_bps`) and only accrues over warped
    // time, so a no-warp test sees `recorded_debt == borrowed`. The reserve/gas-comp tests opt in via
    // `bootstrap_market_full`.
    bootstrap_market_full(svm, gov, coll_mint_auth, 0, 0)
}

/// Full control over liquidation incentives (reserve bond + collateral gas-comp). Interest is set
/// per-position via the borrower's `user_rate_bps` (see [`open_borrower_rate`]) + `warp_unix`.
pub fn bootstrap_market_full(
    svm: &mut LiteSVM,
    gov: &Keypair,
    coll_mint_auth: &Keypair,
    reserve_lamports: u64,
    liq_gas_comp_bps: u16,
) -> Pubkey {
    set_program_upgrade_authority(svm, &gov.pubkey());
    send(svm, &[init_protocol_ix(&gov.pubkey())], gov, &[]).expect("init_protocol failed");

    let coll_mint = Keypair::new();
    create_mint(svm, gov, &coll_mint, COLL_DECIMALS, &coll_mint_auth.pubkey(), /*freeze=*/ false);
    let coll = coll_mint.pubkey();

    send(
        svm,
        &[init_market_ix(
            &gov.pubkey(),
            &coll,
            MCR_BPS,
            DEBT_CEILING,
            reserve_lamports,
            liq_gas_comp_bps,
            BUCKET_WIDTH_BPS,
            0, // redemption fee off by default; the redemption tests opt in
        )],
        gov,
        &[],
    )
    .expect("init_market failed");
    send(svm, &[init_reactor_pool_ix(&gov.pubkey(), &coll)], gov, &[])
        .expect("init_reactor_pool failed");
    send(svm, &[init_insurance_buffer_ix(&gov.pubkey(), &coll)], gov, &[])
        .expect("init_insurance_buffer failed");
    coll
}

/// Add a SECOND (or Nth) market reusing the already-initialized protocol + singleton fUSD mint (does
/// NOT call `init_protocol`). For multi-market tests (e.g. supply reconciliation across markets).
pub fn bootstrap_extra_market(svm: &mut LiteSVM, gov: &Keypair, coll_mint_auth: &Keypair) -> Pubkey {
    let coll_mint = Keypair::new();
    create_mint(svm, gov, &coll_mint, COLL_DECIMALS, &coll_mint_auth.pubkey(), /*freeze=*/ false);
    let coll = coll_mint.pubkey();
    send(
        svm,
        &[init_market_ix(&gov.pubkey(), &coll, MCR_BPS, DEBT_CEILING, 0, 0, BUCKET_WIDTH_BPS, 0)],
        gov,
        &[],
    )
    .expect("init_market (extra) failed");
    send(svm, &[init_reactor_pool_ix(&gov.pubkey(), &coll)], gov, &[]).expect("init_reactor_pool (extra)");
    send(svm, &[init_insurance_buffer_ix(&gov.pubkey(), &coll)], gov, &[]).expect("init_insurance_buffer (extra)");
    coll
}

/// Bootstrap a default market (0% interest, no reserve/gas-comp) with a non-zero redemption fee,
/// for the redemption tests.
pub fn bootstrap_market_with_fee(
    svm: &mut LiteSVM,
    gov: &Keypair,
    coll_mint_auth: &Keypair,
    redemption_fee_bps: u16,
) -> Pubkey {
    set_program_upgrade_authority(svm, &gov.pubkey());
    send(svm, &[init_protocol_ix(&gov.pubkey())], gov, &[]).expect("init_protocol failed");
    let coll_mint = Keypair::new();
    create_mint(svm, gov, &coll_mint, COLL_DECIMALS, &coll_mint_auth.pubkey(), /*freeze=*/ false);
    let coll = coll_mint.pubkey();
    send(
        svm,
        &[init_market_ix(
            &gov.pubkey(),
            &coll,
            MCR_BPS,
            DEBT_CEILING,
            0,
            0,
            BUCKET_WIDTH_BPS,
            redemption_fee_bps,
        )],
        gov,
        &[],
    )
    .expect("init_market failed");
    send(svm, &[init_reactor_pool_ix(&gov.pubkey(), &coll)], gov, &[])
        .expect("init_reactor_pool failed");
    send(svm, &[init_insurance_buffer_ix(&gov.pubkey(), &coll)], gov, &[])
        .expect("init_insurance_buffer failed");
    coll
}

/// Handles for a borrower/depositor actor.
pub struct Actor {
    pub kp: Keypair,
    pub position: Pubkey,
    pub coll_ata: Pubkey,
    pub fusd_ata: Pubkey,
}

/// Stand up an actor that owns a CDP at the default 5% borrower rate. See [`open_borrower_rate`].
pub fn open_borrower(
    svm: &mut LiteSVM,
    coll_mint_auth: &Keypair,
    coll: &Pubkey,
    coll_tokens: u64,
    borrow_fusd: u64,
) -> Actor {
    open_borrower_rate(svm, coll_mint_auth, coll, coll_tokens, borrow_fusd, 500)
}

/// Stand up an actor that owns a CDP: airdrop SOL, fund `coll_tokens` whole collateral tokens,
/// open at `user_rate_bps` + deposit them all, and (if `borrow_fusd > 0`) borrow that much fUSD. A
/// market price must already be set when `borrow_fusd > 0`. The actor pays its own rents/fees;
/// `coll_mint_auth` signs the collateral mint-to.
pub fn open_borrower_rate(
    svm: &mut LiteSVM,
    coll_mint_auth: &Keypair,
    coll: &Pubkey,
    coll_tokens: u64,
    borrow_fusd: u64,
    user_rate_bps: u16,
) -> Actor {
    let kp = Keypair::new();
    airdrop_sol(svm, &kp.pubkey(), 1_000);

    let coll_native = whole_coll(coll_tokens);
    let coll_ata =
        create_ata_and_fund(svm, &kp, &kp.pubkey(), coll, Some(coll_mint_auth), coll_native);
    let fusd_ata = create_ata_and_fund(svm, &kp, &kp.pubkey(), &fusd_mint_pda(), None, 0);

    send(svm, &[open_position_ix(&kp.pubkey(), coll, user_rate_bps)], &kp, &[])
        .expect("open_position failed");
    send(svm, &[deposit_ix(&kp.pubkey(), coll, &coll_ata, coll_native)], &kp, &[])
        .expect("deposit failed");
    if borrow_fusd > 0 {
        send(svm, &[borrow_ix(&kp.pubkey(), coll, &fusd_ata, borrow_fusd)], &kp, &[])
            .expect("borrow failed");
    }

    let position = position_pda(coll, &kp.pubkey());
    Actor { kp, position, coll_ata, fusd_ata }
}

/// Mint `native_amount` collateral into `actor`'s collateral ATA (no deposit).
pub fn fund_collateral(
    svm: &mut LiteSVM,
    coll_mint_auth: &Keypair,
    coll: &Pubkey,
    actor: &Actor,
    native_amount: u64,
) {
    let mint_to = spl_token::instruction::mint_to(
        &SPL_TOKEN_ID,
        coll,
        &actor.coll_ata,
        &coll_mint_auth.pubkey(),
        &[],
        native_amount,
    )
    .unwrap();
    send(svm, &[mint_to], &actor.kp, &[coll_mint_auth]).expect("mint_to failed");
}

/// Mint `native_amount` collateral to `actor` and deposit it — also a convenient way to **touch**
/// a position so it lazily realizes any pending tier-2 redistribution.
pub fn fund_and_deposit(
    svm: &mut LiteSVM,
    coll_mint_auth: &Keypair,
    coll: &Pubkey,
    actor: &Actor,
    native_amount: u64,
) {
    fund_collateral(svm, coll_mint_auth, coll, actor, native_amount);
    send(
        svm,
        &[deposit_ix(&actor.kp.pubkey(), coll, &actor.coll_ata, native_amount)],
        &actor.kp,
        &[],
    )
    .expect("deposit failed");
}

/// Open `actor`'s Reactor-Pool deposit and provide `amount` fUSD-native (the actor must already
/// hold at least that much fUSD).
pub fn provide_sp(svm: &mut LiteSVM, actor: &Actor, coll: &Pubkey, amount: u64) {
    send(svm, &[open_reactor_deposit_ix(&actor.kp.pubkey(), coll)], &actor.kp, &[])
        .expect("open_reactor_deposit failed");
    send(
        svm,
        &[provide_to_reactor_ix(&actor.kp.pubkey(), coll, &actor.fusd_ata, amount)],
        &actor.kp,
        &[],
    )
    .expect("provide_to_reactor failed");
}
