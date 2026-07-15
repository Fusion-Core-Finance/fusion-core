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

/// The fuSOL stake-pool FORK program id (vendor/spl-stake-pool — `declare_id!` swap only; see
/// UPSTREAM.md). Tests load the mainnet-dumped upstream `.so` at this address: the one source
/// diff is unused at runtime, so the dump is behaviorally identical to a from-source fork build.
pub const STAKE_POOL_FORK_ID: Pubkey =
    solana_sdk::pubkey!("3pYHXui7Zk21TKE6oqivqbVJWRXt74wdDkqsnb3Q8mMi");
/// Mainnet dump of the upstream SPL Stake Pool program (gitignored; re-create with
/// `scripts/fetch-spl-stake-pool.sh`).
pub const STAKE_POOL_SO_PATH: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../fixtures/spl_stake_pool.so");

/// Compiled fuSOL Allocation Controller (built by the SAME `anchor build -- --features
/// dev-oracle` invocation that produces `fusd_core.so`; the feature is INERT for this program —
/// it declares no dev instructions, so no dev-marker assert applies to this `.so`).
pub const CONTROLLER_SO_PATH: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../target/deploy/fusion_stake_controller.so");

/// Load the stake-pool fork into `svm` at [`STAKE_POOL_FORK_ID`] (plain v2 loader — the pool
/// program never reads its own ProgramData, and the fork ships sealed/immutable anyway).
pub fn load_stake_pool_fork(svm: &mut LiteSVM) {
    let elf = std::fs::read(STAKE_POOL_SO_PATH).unwrap_or_else(|e| {
        panic!(
            "cannot read {STAKE_POOL_SO_PATH}: {e}\n\
             the stake-pool tests load the dumped upstream program — fetch it first:\n\
                 bash scripts/fetch-spl-stake-pool.sh"
        )
    });
    svm.add_program(STAKE_POOL_FORK_ID, &elf);
}

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
pub const E_MATH_OVERFLOW: u32 = 6013;
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
pub const E_LIQ_INFRA_NOT_READY: u32 = 6048; // L-02 — borrow gated until init_reactor_pool + init_insurance_buffer (6047 = InvalidMetadataAccount, unpinned)

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

/// The canonical `ProgramData` PDA of `program_id` under the BPF upgradeable loader.
pub fn programdata_pda_of(program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[program_id.as_ref()],
        &solana_sdk::bpf_loader_upgradeable::id(),
    )
    .0
}

/// The canonical `ProgramData` PDA of `fusd_core` under the BPF upgradeable loader.
pub fn programdata_pda() -> Pubkey {
    programdata_pda_of(&fusd_core::ID)
}

/// Load `fusd_core.so` under the BPF upgradeable loader with the given upgrade authority (dev
/// marker required — see [`load_upgradeable_program_at`]).
fn load_upgradeable_program(svm: &mut LiteSVM, so_path: &str, upgrade_authority: Pubkey) {
    load_upgradeable_program_at(svm, so_path, fusd_core::ID, upgrade_authority, true);
}

/// Load any `.so` under the BPF upgradeable loader at `program_id` with the given upgrade
/// authority: writes the `ProgramData` account (45-byte metadata header + ELF) then the
/// `Program` account that points at it (executable). `add_program_from_file` only loads the
/// non-upgradeable v2 layout, which has no `ProgramData` account and so can't drive an
/// upgrade-authority gate (fusd-core `init_protocol`, controller `initialize_controller`).
///
/// `expect_dev_marker`: assert the ELF embeds the dev-oracle instruction marker. `true` for
/// fusd-core (dev_set_price must exist at runtime), `false` for programs whose dev-oracle
/// feature is inert (the controller declares no dev instructions).
pub fn load_upgradeable_program_at(
    svm: &mut LiteSVM,
    so_path: &str,
    program_id: Pubkey,
    upgrade_authority: Pubkey,
    expect_dev_marker: bool,
) {
    use solana_sdk::account::Account;
    use solana_sdk::bpf_loader_upgradeable::{self, UpgradeableLoaderState};

    // Fail fast with an actionable message: a clean checkout has no .so, and scripts/ci-checks.sh
    // deliberately leaves a PRODUCTION .so behind (step 7), which lacks dev_set_price.
    let elf = std::fs::read(so_path).unwrap_or_else(|e| {
        panic!(
            "cannot read {so_path}: {e}\n\
             the litesvm suite loads the compiled program — build it first:\n\
                 anchor build -- --features dev-oracle\n\
             (scripts/ci-checks.sh step 3 does this automatically; bare \
             `cargo test --workspace` on a clean checkout always hits this)"
        )
    });
    // Anchor codegen embeds `msg!("Instruction: DevSetPrice")` in every dev-oracle build — the same
    // marker scripts/check-no-dev-oracle.sh asserts is ABSENT from production builds.
    const DEV_ORACLE_MARKER: &[u8] = b"Instruction: DevSetPrice";
    if expect_dev_marker {
        assert!(
            elf.windows(DEV_ORACLE_MARKER.len()).any(|w| w == DEV_ORACLE_MARKER),
            "{so_path} is a PRODUCTION build (no dev-oracle feature) — ci-checks.sh intentionally \
             leaves one after step 7; rebuild with: anchor build -- --features dev-oracle"
        );
    }
    let pd_addr = programdata_pda_of(&program_id);

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
        program_id,
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

/// Repoint fusd-core's upgrade authority (rewrites only the `ProgramData` metadata header,
/// preserving the ELF) so `init_protocol`'s gate accepts `auth` as the legitimate initializer.
pub fn set_program_upgrade_authority(svm: &mut LiteSVM, auth: &Pubkey) {
    set_upgrade_authority_at(svm, &fusd_core::ID, auth);
}

/// As [`set_program_upgrade_authority`], for any loaded upgradeable program — the controller's
/// `initialize_controller` carries the same upgrade-authority gate as fusd-core's `init_protocol`.
pub fn set_upgrade_authority_at(svm: &mut LiteSVM, program_id: &Pubkey, auth: &Pubkey) {
    use solana_sdk::bpf_loader_upgradeable::UpgradeableLoaderState;
    let pd_addr = programdata_pda_of(program_id);
    let mut acct = svm.get_account(&pd_addr).expect("programdata account loaded");
    let meta = UpgradeableLoaderState::ProgramData {
        slot: 0,
        upgrade_authority_address: Some(*auth),
    };
    let meta_bytes = bincode::serialize(&meta).expect("serialize ProgramData metadata");
    acct.data[..meta_bytes.len()].copy_from_slice(&meta_bytes);
    svm.set_account(pd_addr, acct).expect("rewrite programdata upgrade authority");
}

/// Load the fuSOL Allocation Controller as an UPGRADEABLE program with the given upgrade
/// authority — `initialize_controller` gates genesis to it (front-run protection). No dev
/// marker: the controller's `dev-oracle` feature is inert.
pub fn load_controller(svm: &mut LiteSVM, upgrade_authority: Pubkey) {
    load_upgradeable_program_at(
        svm,
        CONTROLLER_SO_PATH,
        fusion_stake_controller::ID,
        upgrade_authority,
        false,
    );
}

/// Fresh `LiteSVM` with the FULL fuSOL stack: fusd-core (upgradeable, like [`new_svm`]) + the
/// Allocation Controller (upgradeable, authority `Pubkey::default()` — [`pool_genesis`] repoints
/// it to the genesis payer) + the mainnet-dumped stake-pool program at [`STAKE_POOL_FORK_ID`].
pub fn new_svm_full() -> LiteSVM {
    let mut svm = new_svm();
    load_controller(&mut svm, Pubkey::default());
    load_stake_pool_fork(&mut svm);
    svm
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

/// Advance the SVM clock by `n` EPOCHS, keeping the slot consistent with the epoch schedule
/// (litesvm has no epoch machinery: no rewards are paid and StakeHistory stays `Default` — epoch
/// warp is purely manual). The slot lands on the target epoch's first slot (or just past the
/// current slot if that is already beyond it), and the blockhash is expired so a post-warp retry
/// of an identical tx isn't deduped as `AlreadyProcessed`.
pub fn warp_epochs(svm: &mut LiteSVM, n: u64) {
    let mut clock: solana_sdk::clock::Clock = svm.get_sysvar();
    let schedule: solana_sdk::epoch_schedule::EpochSchedule = svm.get_sysvar();
    let target_epoch = clock.epoch.saturating_add(n);
    let slot = schedule.get_first_slot_in_epoch(target_epoch).max(clock.slot.saturating_add(1));
    clock.epoch = target_epoch;
    clock.slot = slot;
    svm.set_sysvar(&clock);
    svm.warp_to_slot(slot);
    svm.expire_blockhash();
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
        canonical_primary: false,
        liquidity_haircut_bps: 0,
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
    pool_mint: &Pubkey,
    total_lamports: u64,
    pool_token_supply: u64,
    last_update_epoch: u64,
) {
    let mut data = vec![0u8; 320]; // past min_len (282), mimicking the real account's tail
    data[0] = 1; // AccountType::StakePool
    data[162..194].copy_from_slice(pool_mint.as_ref()); // pool_mint (bound to the market collateral)
    data[258..266].copy_from_slice(&total_lamports.to_le_bytes());
    data[266..274].copy_from_slice(&pool_token_supply.to_le_bytes());
    data[274..282].copy_from_slice(&last_update_epoch.to_le_bytes());
    set_raw_account(svm, key, data, fusd_core::constants::SPL_STAKE_POOL_PROGRAM_ID);
}

/// As [`set_stake_pool`], but owned by `owner` — the canonical-primary (fuSOL) mode binds a pool
/// owned by the FUSION stake-pool FORK (`FUSION_STAKE_POOL_PROGRAM_ID`), not the upstream `SPoo1…`.
pub fn set_stake_pool_owned(
    svm: &mut LiteSVM,
    key: &Pubkey,
    pool_mint: &Pubkey,
    total_lamports: u64,
    pool_token_supply: u64,
    last_update_epoch: u64,
    owner: Pubkey,
) {
    let mut data = vec![0u8; 320];
    data[0] = 1; // AccountType::StakePool
    data[162..194].copy_from_slice(pool_mint.as_ref());
    data[258..266].copy_from_slice(&total_lamports.to_le_bytes());
    data[266..274].copy_from_slice(&pool_token_supply.to_le_bytes());
    data[274..282].copy_from_slice(&last_update_epoch.to_le_bytes());
    set_raw_account(svm, key, data, owner);
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
            market: market_pda(coll),
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
        canonical_primary: false,
        liquidity_haircut_bps: 0,
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
        canonical_primary: false,
        liquidity_haircut_bps: 0,
    };
    send(svm, &[init_market_oracle_ix(&gov.pubkey(), coll, &quote, args)], gov, &[])
        .expect("init_market_oracle (LST)");
    (OracleHandles { quote, orca_pool, raydium_pool: Pubkey::default(), pyth, sb, feed_id }, stake_pool)
}

/// Bootstrap a CANONICAL-PRIMARY (fuSOL) market oracle: the bound Pyth feed is the shared SOL/USD
/// id, no DEX pools (the TWAP corridor is optional in this mode), a FORK-owned stake pool is
/// bound, and the mandatory liquidity haircut is set. Returns the handles + the stake-pool key
/// (fabricate it with [`set_stake_pool_owned`] + `FUSION_STAKE_POOL_PROGRAM_ID`).
pub fn bootstrap_oracle_fusol(
    svm: &mut LiteSVM,
    gov: &Keypair,
    coll: &Pubkey,
    liquidity_haircut_bps: u16,
) -> (OracleHandles, Pubkey) {
    use fusd_core::constants::{
        DEFAULT_ORACLE_CONF_BPS, DEFAULT_ORACLE_DEVIATION_BPS, DEFAULT_ORACLE_K_BPS,
        DEFAULT_ORACLE_MAX_AGE_SECS, DEFAULT_TWAP_DIVERGENCE_BPS, PYTH_SOL_USD_FEED_ID,
    };
    let quote = create_quote_mint(svm, gov, FUSD_DECIMALS);
    let pyth = Pubkey::new_unique();
    let sb = Pubkey::new_unique();
    let stake_pool = Pubkey::new_unique();
    let args = InitMarketOracleArgs {
        pyth_feed_id: PYTH_SOL_USD_FEED_ID,
        switchboard_feed: sb,
        orca_pool: Pubkey::default(),
        raydium_pool: Pubkey::default(),
        max_conf_bps: DEFAULT_ORACLE_CONF_BPS,
        max_deviation_bps: DEFAULT_ORACLE_DEVIATION_BPS,
        twap_max_divergence_bps: DEFAULT_TWAP_DIVERGENCE_BPS,
        max_age_secs: DEFAULT_ORACLE_MAX_AGE_SECS,
        k_bps: DEFAULT_ORACLE_K_BPS,
        twap_window_secs: 300,
        twap_min_samples: 3,
        twap_max_staleness_secs: 300,
        price_band_lower_ray: 0,
        price_band_upper_ray: 0,
        liq_max_divergence_bps: 0,
        lst_stake_pool: stake_pool,
        canonical_primary: true,
        liquidity_haircut_bps,
    };
    send(svm, &[init_market_oracle_ix(&gov.pubkey(), coll, &quote, args)], gov, &[])
        .expect("init_market_oracle (fusol)");
    (
        OracleHandles {
            quote,
            orca_pool: Pubkey::default(),
            raydium_pool: Pubkey::default(),
            pyth,
            sb,
            feed_id: PYTH_SOL_USD_FEED_ID,
        },
        stake_pool,
    )
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
            market_oracle: None,
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
            market_oracle: None,
            timelocked_param: timelock_pda(nonce),
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::ExecuteParamChange {}.data(),
    }
}

/// QUEUE an oracle-targeting (RiskParamRegistry) param change — includes the `MarketOracle` account.
pub fn queue_param_change_oracle_ix(authority: &Pubkey, coll: &Pubkey, nonce: u64, param: MarketParam, value: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::QueueParamChange {
            authority: *authority,
            gov_gate: gov_gate_pda(),
            market: market_pda(coll),
            market_oracle: Some(market_oracle_pda(coll)),
            timelocked_param: timelock_pda(nonce),
            system_program: system_program::ID,
            event_authority: event_authority_pda(),
            program: fusd_core::ID,
        }
        .to_account_metas(None),
        data: fusd_core::instruction::QueueParamChange { param, value }.data(),
    }
}

/// EXECUTE an oracle-targeting param change — includes the `MarketOracle` account.
pub fn execute_param_change_oracle_ix(executor: &Pubkey, coll: &Pubkey, nonce: u64) -> Instruction {
    Instruction {
        program_id: fusd_core::ID,
        accounts: fusd_core::accounts::ExecuteParamChange {
            executor: *executor,
            market: market_pda(coll),
            market_oracle: Some(market_oracle_pda(coll)),
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

// ============================ fuSOL allocation controller: PDAs + pins ============================
//
// Everything below drives `programs/fusion-stake-controller` against the REAL mainnet-dumped
// stake-pool program (loaded at [`STAKE_POOL_FORK_ID`] by [`new_svm_full`]). Seeds are imported
// from the program's constants module — never restrung.

use fusion_stake_controller::constants::{
    CONTROLLER_SEED, DEPOSIT_AUTHORITY_SEED, EPOCH_STATE_SEED, MAINTENANCE_AUTHORITY_SEED,
    MAX_VALIDATORS, POOL_AUTHORITY_SEED, PREFERENCE_SEED, STAKE_ACCOUNT_SPACE, STAKE_CONFIG_ID,
    STAKE_PROGRAM_ID, VALIDATOR_RECORD_SEED,
};
/// Re-exported so scenario files can build genesis args without spelling the module path.
pub use fusion_stake_controller::instructions::initialize_controller::InitializeControllerArgs;

fn ctrl_pda(seeds: &[&[u8]]) -> Pubkey {
    Pubkey::find_program_address(seeds, &fusion_stake_controller::ID).0
}

/// `[b"controller"]` — the singleton `ControllerConfig`.
pub fn controller_config_pda() -> Pubkey {
    ctrl_pda(&[CONTROLLER_SEED])
}
/// `[b"epoch_state"]` — the singleton zero-copy crank state machine.
pub fn controller_epoch_state_pda() -> Pubkey {
    ctrl_pda(&[EPOCH_STATE_SEED])
}
/// `[b"validator", vote_account]` — one `ValidatorRecord` per registered vote account.
pub fn validator_record_pda(vote: &Pubkey) -> Pubkey {
    ctrl_pda(&[VALIDATOR_RECORD_SEED, vote.as_ref()])
}
/// `[b"preference", fusion_position]` — one `Preference` per fuSOL Fusion position.
pub fn preference_pda(position: &Pubkey) -> Pubkey {
    ctrl_pda(&[PREFERENCE_SEED, position.as_ref()])
}
/// `[b"pool_authority"]` — the stake pool's manager AND staker.
pub fn pool_authority_pda() -> Pubkey {
    ctrl_pda(&[POOL_AUTHORITY_SEED])
}
/// `[b"deposit_authority"]` — the pool's SOL + stake deposit authority.
pub fn deposit_authority_pda() -> Pubkey {
    ctrl_pda(&[DEPOSIT_AUTHORITY_SEED])
}
/// `[b"maintenance"]` — the maintenance vault's token authority.
pub fn maintenance_authority_pda() -> Pubkey {
    ctrl_pda(&[MAINTENANCE_AUTHORITY_SEED])
}
/// The controller's `#[event_cpi]` event-authority PDA (`[b"__event_authority"]`).
pub fn controller_event_authority_pda() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &fusion_stake_controller::ID).0
}
/// The stake-pool program's withdraw-authority PDA — `[stake_pool, b"withdraw"]` under the FORK
/// program id (the upstream `AUTHORITY_WITHDRAW` seed literal; `initialize_controller` derives
/// and records the same address).
pub fn pool_withdraw_authority_pda(stake_pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[stake_pool.as_ref(), b"withdraw"], &STAKE_POOL_FORK_ID).0
}

// Anchor custom-error codes (base 6000 + variant index in fusion-stake-controller's errors.rs —
// the litesvm pins that make that file's append-only comment true).
pub const E_CTRL_ALREADY_SEALED: u32 = 6000;
pub const E_CTRL_INVALID_CONFIG_ADDRESS: u32 = 6001;
pub const E_CTRL_ADDRESS_MISMATCH: u32 = 6002;
pub const E_CTRL_INVALID_FUSOL_MINT: u32 = 6003;
pub const E_CTRL_INVALID_MAINTENANCE_VAULT: u32 = 6004;
pub const E_CTRL_INVALID_VOTE_ACCOUNT: u32 = 6005;
pub const E_CTRL_ZERO_AMOUNT: u32 = 6006;
pub const E_CTRL_MATH_OVERFLOW: u32 = 6007;
pub const E_CTRL_POOL_NOT_INITIALIZED: u32 = 6008;
pub const E_CTRL_VALIDATOR_NOT_IN_POOL: u32 = 6009;
pub const E_CTRL_VALIDATOR_CAP_EXCEEDED: u32 = 6010;
pub const E_CTRL_CORRUPT_VALIDATOR_STATUS: u32 = 6011;
pub const E_CTRL_INVALID_STAKE_POOL_ACCOUNT: u32 = 6012;
pub const E_CTRL_INVALID_VALIDATOR_LIST_ENTRY: u32 = 6013;
pub const E_CTRL_NOT_YET_IMPLEMENTED: u32 = 6014; // unreachable — kept for code stability
pub const E_CTRL_WRONG_PHASE: u32 = 6015;
pub const E_CTRL_EPOCH_NOT_ADVANCED: u32 = 6016;
pub const E_CTRL_INVALID_REMAINING_ACCOUNTS: u32 = 6017;
pub const E_CTRL_STALE_VALIDATOR_RECORD: u32 = 6018;
pub const E_CTRL_RECORD_ALREADY_PLANNED: u32 = 6019;
pub const E_CTRL_PREFERENCE_WINDOW_CLOSED: u32 = 6020;
pub const E_CTRL_PREFERENCE_WINDOW_STILL_OPEN: u32 = 6021;
pub const E_CTRL_PREFERENCE_NOT_COUNTABLE: u32 = 6022;
pub const E_CTRL_PREFERENCE_CHANGE_LIMIT: u32 = 6023;
pub const E_CTRL_PREFERENCE_OWNER_MISMATCH: u32 = 6024;
pub const E_CTRL_INVALID_POSITION_ACCOUNT: u32 = 6025;
pub const E_CTRL_VALIDATOR_NOT_ELIGIBLE_FOR_PREFERENCE: u32 = 6026;
pub const E_CTRL_DIRECTED_SHARES_EXCEED_SUPPLY: u32 = 6027;
pub const E_CTRL_PLAN_CONSERVATION_VIOLATED: u32 = 6028;
pub const E_CTRL_NEUTRAL_ROUND_INCONSISTENT: u32 = 6029;
pub const E_CTRL_REBALANCE_COMPLETE: u32 = 6030;
pub const E_CTRL_WRONG_ACTION_TARGET: u32 = 6031;
pub const E_CTRL_EPOCH_NOT_FINISHED: u32 = 6032;
pub const E_CTRL_POSITION_STILL_OPEN: u32 = 6033;
pub const E_CTRL_INVALID_RENT_RECIPIENT: u32 = 6034;
pub const E_CTRL_INVALID_REWARD_RECIPIENT: u32 = 6035;
pub const E_CTRL_PREFERENCE_CHANGE_LIMIT2: u32 = 6036;
pub const E_CTRL_INVALID_USER_STAKE_ACCOUNT: u32 = 6037;
pub const E_CTRL_STAKE_DELEGATION_MISMATCH: u32 = 6038;
pub const E_CTRL_MINIMUM_DELEGATION_UNAVAILABLE: u32 = 6039;

// ============================ controller: typed account readers ============================

pub fn read_controller_config(svm: &LiteSVM) -> fusion_stake_controller::state::ControllerConfig {
    let acct = svm.get_account(&controller_config_pda()).expect("ControllerConfig exists");
    fusion_stake_controller::state::ControllerConfig::try_deserialize(&mut acct.data.as_slice())
        .unwrap()
}

/// The zero-copy `EpochState` (read via `pod_read_unaligned` — account data is 1-aligned).
pub fn read_epoch_state(svm: &LiteSVM) -> fusion_stake_controller::state::EpochState {
    let acct = svm.get_account(&controller_epoch_state_pda()).expect("EpochState exists");
    let size = core::mem::size_of::<fusion_stake_controller::state::EpochState>();
    bytemuck::pod_read_unaligned(&acct.data[8..8 + size])
}

pub fn read_validator_record(
    svm: &LiteSVM,
    vote: &Pubkey,
) -> fusion_stake_controller::state::ValidatorRecord {
    let acct = svm.get_account(&validator_record_pda(vote)).expect("ValidatorRecord exists");
    fusion_stake_controller::state::ValidatorRecord::try_deserialize(&mut acct.data.as_slice())
        .unwrap()
}

pub fn read_preference(
    svm: &LiteSVM,
    position: &Pubkey,
) -> fusion_stake_controller::state::Preference {
    let acct = svm.get_account(&preference_pda(position)).expect("Preference exists");
    fusion_stake_controller::state::Preference::try_deserialize(&mut acct.data.as_slice()).unwrap()
}

/// Parse the FORK pool's `StakePool` account through the same byte view the controller trusts
/// on-chain (authority graph, canonical totals, `last_update_epoch`).
pub fn read_fork_stake_pool(
    svm: &LiteSVM,
    stake_pool: &Pubkey,
) -> fusion_stake_view::stake_pool::StakePoolView {
    let acct = svm.get_account(stake_pool).expect("StakePool account exists");
    assert_eq!(acct.owner, STAKE_POOL_FORK_ID, "StakePool must be owned by the fork program");
    fusion_stake_view::stake_pool::parse(&acct.data).expect("StakePool parses")
}

/// Header + `entry_at` view of the FORK pool's `ValidatorList`.
pub fn read_fork_validator_list_len(svm: &LiteSVM, validator_list: &Pubkey) -> u32 {
    let acct = svm.get_account(validator_list).expect("ValidatorList account exists");
    assert_eq!(acct.owner, STAKE_POOL_FORK_ID, "ValidatorList must be owned by the fork program");
    fusion_stake_view::validator_list::parse_header(&acct.data).expect("ValidatorList parses").len
}

// ============================ controller: instruction builders ============================
//
// All 18 instructions, built from the program's generated `accounts::X` / `instruction::X`
// types (the fusd-core builder convention). Every instruction is `#[event_cpi]`, so the
// event-authority PDA + the program account ride at the END of the named accounts; the three
// remaining-accounts cranks take a `&[AccountMeta]` tail appended AFTER those.

pub fn ctrl_initialize_controller_ix(payer: &Pubkey, g: &PoolGenesis) -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::InitializeController {
            payer: *payer,
            program_data: programdata_pda_of(&fusion_stake_controller::ID),
            config: g.config,
            epoch_state: g.epoch_state,
            pool_authority: g.pool_authority,
            deposit_authority: g.deposit_authority,
            maintenance_authority: g.maintenance_authority,
            system_program: system_program::ID,
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::InitializeController {
            args: InitializeControllerArgs {
                stake_pool: g.stake_pool,
                validator_list: g.validator_list,
                reserve_stake: g.reserve_stake,
                fusol_mint: g.fusol_mint,
                maintenance_vault: g.maintenance_vault,
            },
        }
        .data(),
    }
}

pub fn ctrl_initialize_pool_ix(payer: &Pubkey, g: &PoolGenesis) -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::InitializePool {
            payer: *payer,
            config: g.config,
            stake_pool: g.stake_pool,
            pool_authority: g.pool_authority,
            deposit_authority: g.deposit_authority,
            pool_withdraw_authority: g.pool_withdraw_authority,
            validator_list: g.validator_list,
            reserve_stake: g.reserve_stake,
            fusol_mint: g.fusol_mint,
            maintenance_vault: g.maintenance_vault,
            maintenance_authority: g.maintenance_authority,
            stake_pool_program: STAKE_POOL_FORK_ID,
            token_program: SPL_TOKEN_ID,
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::InitializePool {}.data(),
    }
}

pub fn ctrl_register_validator_ix(payer: &Pubkey, vote: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::RegisterValidator {
            payer: *payer,
            config: controller_config_pda(),
            vote_account: *vote,
            validator_record: validator_record_pda(vote),
            system_program: system_program::ID,
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::RegisterValidator {}.data(),
    }
}

pub fn ctrl_deposit_sol_ix(
    depositor: &Pubkey,
    g: &PoolGenesis,
    user_fusol_account: &Pubkey,
    lamports: u64,
) -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::DepositSol {
            depositor: *depositor,
            config: g.config,
            stake_pool: g.stake_pool,
            pool_withdraw_authority: g.pool_withdraw_authority,
            reserve_stake: g.reserve_stake,
            fusol_mint: g.fusol_mint,
            user_fusol_account: *user_fusol_account,
            maintenance_vault: g.maintenance_vault,
            deposit_authority: g.deposit_authority,
            stake_pool_program: STAKE_POOL_FORK_ID,
            token_program: SPL_TOKEN_ID,
            system_program: system_program::ID,
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::DepositSol { lamports }.data(),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn ctrl_deposit_stake_ix(
    depositor: &Pubkey,
    g: &PoolGenesis,
    user_stake_account: &Pubkey,
    vote: &Pubkey,
    validator_stake_account: &Pubkey,
    user_fusol_account: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::DepositStake {
            depositor: *depositor,
            config: g.config,
            stake_pool: g.stake_pool,
            validator_list: g.validator_list,
            deposit_authority: g.deposit_authority,
            pool_withdraw_authority: g.pool_withdraw_authority,
            user_stake_account: *user_stake_account,
            vote_account: *vote,
            validator_record: validator_record_pda(vote),
            validator_stake_account: *validator_stake_account,
            reserve_stake: g.reserve_stake,
            fusol_mint: g.fusol_mint,
            user_fusol_account: *user_fusol_account,
            maintenance_vault: g.maintenance_vault,
            clock: solana_sdk::sysvar::clock::ID,
            stake_history: solana_sdk::sysvar::stake_history::ID,
            stake_program: STAKE_PROGRAM_ID,
            stake_pool_program: STAKE_POOL_FORK_ID,
            token_program: SPL_TOKEN_ID,
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::DepositStake {}.data(),
    }
}

pub fn ctrl_set_preference_ix(owner: &Pubkey, position: &Pubkey, vote: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::SetPreference {
            owner: *owner,
            config: controller_config_pda(),
            fusion_position: *position,
            vote_account: *vote,
            validator_record: validator_record_pda(vote),
            preference: preference_pda(position),
            system_program: system_program::ID,
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::SetPreference {}.data(),
    }
}

/// Permissionless — any tx payer may carry it; there is no signer in the account set.
pub fn ctrl_sync_preference_ix(position: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::SyncPreference {
            config: controller_config_pda(),
            fusion_position: *position,
            preference: preference_pda(position),
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::SyncPreference {}.data(),
    }
}

/// `vote` must be the preference's RECORDED vote account (the record PDA is seeded by it).
pub fn ctrl_snapshot_preference_ix(position: &Pubkey, vote: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::SnapshotPreference {
            config: controller_config_pda(),
            epoch_state: controller_epoch_state_pda(),
            fusion_position: *position,
            preference: preference_pda(position),
            validator_record: validator_record_pda(vote),
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::SnapshotPreference {}.data(),
    }
}

pub fn ctrl_close_preference_ix(
    closer: &Pubkey,
    position: &Pubkey,
    rent_recipient: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::ClosePreference {
            closer: *closer,
            config: controller_config_pda(),
            fusion_position: *position,
            preference: preference_pda(position),
            rent_recipient: *rent_recipient,
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::ClosePreference {}.data(),
    }
}

pub fn ctrl_start_epoch_ix() -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::StartEpoch {
            config: controller_config_pda(),
            epoch_state: controller_epoch_state_pda(),
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::StartEpoch {}.data(),
    }
}

/// `tail`: 4N remaining accounts — `(validator_stake [w], transient_stake [w],
/// validator_record [w], vote_account [])` quads for consecutive list indices starting at
/// `reconcile_cursor` (empty only to complete an already-covered/empty phase).
pub fn ctrl_reconcile_batch_ix(
    g: &PoolGenesis,
    crank_reward_account: &Pubkey,
    tail: &[AccountMeta],
) -> Instruction {
    let mut accounts = fusion_stake_controller::accounts::ReconcileBatch {
        config: g.config,
        epoch_state: g.epoch_state,
        stake_pool: g.stake_pool,
        pool_withdraw_authority: g.pool_withdraw_authority,
        validator_list: g.validator_list,
        reserve_stake: g.reserve_stake,
        clock: solana_sdk::sysvar::clock::ID,
        stake_history: solana_sdk::sysvar::stake_history::ID,
        stake_program: STAKE_PROGRAM_ID,
        stake_pool_program: STAKE_POOL_FORK_ID,
        maintenance_vault: g.maintenance_vault,
        maintenance_authority: g.maintenance_authority,
        crank_reward_account: *crank_reward_account,
        token_program: SPL_TOKEN_ID,
        event_authority: controller_event_authority_pda(),
        program: fusion_stake_controller::ID,
    }
    .to_account_metas(None);
    accounts.extend_from_slice(tail);
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts,
        data: fusion_stake_controller::instruction::ReconcileBatch {}.data(),
    }
}

pub fn ctrl_finalize_pool_ix(g: &PoolGenesis, crank_reward_account: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::FinalizePool {
            config: g.config,
            epoch_state: g.epoch_state,
            stake_pool: g.stake_pool,
            pool_withdraw_authority: g.pool_withdraw_authority,
            validator_list: g.validator_list,
            reserve_stake: g.reserve_stake,
            fusol_mint: g.fusol_mint,
            maintenance_vault: g.maintenance_vault,
            maintenance_authority: g.maintenance_authority,
            crank_reward_account: *crank_reward_account,
            stake_pool_program: STAKE_POOL_FORK_ID,
            token_program: SPL_TOKEN_ID,
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::FinalizePool {}.data(),
    }
}

pub fn ctrl_close_preference_window_ix() -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::ClosePreferenceWindow {
            config: controller_config_pda(),
            epoch_state: controller_epoch_state_pda(),
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::ClosePreferenceWindow {}.data(),
    }
}

/// `tail`: 2N remaining accounts — `(validator_record [w], vote_account [])` pairs: list-slice
/// pairs from `plan_directed_cursor` first, then admission extras (UNSET-index records).
pub fn ctrl_plan_directed_batch_ix(
    g: &PoolGenesis,
    crank_reward_account: &Pubkey,
    tail: &[AccountMeta],
) -> Instruction {
    let mut accounts = fusion_stake_controller::accounts::PlanDirectedBatch {
        config: g.config,
        epoch_state: g.epoch_state,
        validator_list: g.validator_list,
        maintenance_vault: g.maintenance_vault,
        maintenance_authority: g.maintenance_authority,
        crank_reward_account: *crank_reward_account,
        stake_program: STAKE_PROGRAM_ID,
        token_program: SPL_TOKEN_ID,
        event_authority: controller_event_authority_pda(),
        program: fusion_stake_controller::ID,
    }
    .to_account_metas(None);
    accounts.extend_from_slice(tail);
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts,
        data: fusion_stake_controller::instruction::PlanDirectedBatch {}.data(),
    }
}

/// `tail`: N writable `ValidatorRecord`s for consecutive planned list ordinals starting at
/// `neutral_cursor`.
pub fn ctrl_plan_neutral_batch_ix(
    g: &PoolGenesis,
    crank_reward_account: &Pubkey,
    tail: &[AccountMeta],
) -> Instruction {
    let mut accounts = fusion_stake_controller::accounts::PlanNeutralBatch {
        config: g.config,
        epoch_state: g.epoch_state,
        maintenance_vault: g.maintenance_vault,
        maintenance_authority: g.maintenance_authority,
        crank_reward_account: *crank_reward_account,
        token_program: SPL_TOKEN_ID,
        event_authority: controller_event_authority_pda(),
        program: fusion_stake_controller::ID,
    }
    .to_account_metas(None);
    accounts.extend_from_slice(tail);
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts,
        data: fusion_stake_controller::instruction::PlanNeutralBatch {}.data(),
    }
}

pub fn ctrl_finalize_plan_ix(g: &PoolGenesis, crank_reward_account: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::FinalizePlan {
            config: g.config,
            epoch_state: g.epoch_state,
            stake_pool: g.stake_pool,
            pool_authority: g.pool_authority,
            validator_list: g.validator_list,
            stake_pool_program: STAKE_POOL_FORK_ID,
            maintenance_vault: g.maintenance_vault,
            maintenance_authority: g.maintenance_authority,
            crank_reward_account: *crank_reward_account,
            token_program: SPL_TOKEN_ID,
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::FinalizePlan {}.data(),
    }
}

/// `vote` must be the deterministic selection for the current rebalance slot (or a pending
/// admission add); the stake/transient accounts are the upstream-derived pair for it.
pub fn ctrl_execute_next_action_ix(
    g: &PoolGenesis,
    vote: &Pubkey,
    validator_stake_account: &Pubkey,
    transient_stake_account: &Pubkey,
    crank_reward_account: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::ExecuteNextAction {
            config: g.config,
            epoch_state: g.epoch_state,
            stake_pool: g.stake_pool,
            pool_authority: g.pool_authority,
            pool_withdraw_authority: g.pool_withdraw_authority,
            validator_list: g.validator_list,
            reserve_stake: g.reserve_stake,
            vote_account: *vote,
            validator_record: validator_record_pda(vote),
            validator_stake_account: *validator_stake_account,
            transient_stake_account: *transient_stake_account,
            clock: solana_sdk::sysvar::clock::ID,
            rent: rent::ID,
            stake_history: solana_sdk::sysvar::stake_history::ID,
            stake_config: STAKE_CONFIG_ID,
            stake_program: STAKE_PROGRAM_ID,
            stake_pool_program: STAKE_POOL_FORK_ID,
            maintenance_vault: g.maintenance_vault,
            maintenance_authority: g.maintenance_authority,
            crank_reward_account: *crank_reward_account,
            token_program: SPL_TOKEN_ID,
            system_program: system_program::ID,
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::ExecuteNextAction {}.data(),
    }
}

pub fn ctrl_finish_epoch_ix(g: &PoolGenesis, crank_reward_account: &Pubkey) -> Instruction {
    Instruction {
        program_id: fusion_stake_controller::ID,
        accounts: fusion_stake_controller::accounts::FinishEpoch {
            config: g.config,
            epoch_state: g.epoch_state,
            maintenance_vault: g.maintenance_vault,
            maintenance_authority: g.maintenance_authority,
            crank_reward_account: *crank_reward_account,
            stake_program: STAKE_PROGRAM_ID,
            token_program: SPL_TOKEN_ID,
            event_authority: controller_event_authority_pda(),
            program: fusion_stake_controller::ID,
        }
        .to_account_metas(None),
        data: fusion_stake_controller::instruction::FinishEpoch {}.data(),
    }
}

// ============================ controller: pool genesis fixture ============================

/// Pre-created `StakePool` account size. The REAL processor `try_from_slice_unchecked`-reads the
/// zeroed account (`AccountType::Uninitialized`) and later `borsh::to_writer`s the initialized
/// struct back (≤ ~470 bytes with this authority set — every `FutureEpoch`/`Option` small);
/// A generous upper bound: the max borsh size of the vendored `StakePool` (every `Option` Some)
/// field-sums to 611, and the processor imposes no exact size — only rent-exemption and room to
/// serialize (`scripts/bootstrap-fusol.ts` allocates the exact 611). 656 kept here, generous on
/// both sides.
pub const STAKE_POOL_ACCOUNT_SIZE: usize = 656;
/// Pre-created `ValidatorList` size: 5-byte header + 4-byte vec length + 73 bytes per entry.
/// `Initialize` requires `calculate_max_validators(len) == MAX_VALIDATORS` EXACTLY.
pub const VALIDATOR_LIST_ACCOUNT_SIZE: usize = 5 + 4 + (MAX_VALIDATORS as usize) * 73;
/// Lamports the genesis reserve stake holds ABOVE its rent floor. The REAL `Initialize` counts
/// everything above rent as pre-existing pool value: it mints exactly this many pool tokens to
/// the manager fee account (the maintenance vault), so the pool starts at `total_lamports ==
/// pool_token_supply == RESERVE_BOOTSTRAP_LAMPORTS` (rate 1) with a funded crank-reward vault.
pub const RESERVE_BOOTSTRAP_LAMPORTS: u64 = 1_000_000_000; // 1 SOL

/// Every address of a genesis'd fuSOL pool stack (controller PDAs + stake-pool-side accounts).
#[derive(Clone, Debug)]
pub struct PoolGenesis {
    /// `[b"controller"]` — the `ControllerConfig` PDA.
    pub config: Pubkey,
    /// `[b"epoch_state"]` — the `EpochState` PDA.
    pub epoch_state: Pubkey,
    /// `[b"pool_authority"]` — the pool's manager + staker.
    pub pool_authority: Pubkey,
    /// `[b"deposit_authority"]` — the pool's SOL + stake deposit authority.
    pub deposit_authority: Pubkey,
    /// `[b"maintenance"]` — the maintenance vault's token authority.
    pub maintenance_authority: Pubkey,
    /// The initialized `StakePool` account (fork-owned).
    pub stake_pool: Pubkey,
    /// The initialized `ValidatorList` account (fork-owned, exactly `MAX_VALIDATORS` capacity).
    pub validator_list: Pubkey,
    /// The pool reserve stake account (REAL stake program, Initialized, withdraw-PDA authorities).
    pub reserve_stake: Pubkey,
    /// The fuSOL mint (9 dec, mint authority = the pool withdraw-authority PDA, freeze None).
    pub fusol_mint: Pubkey,
    /// The maintenance vault (fuSOL token account, authority = the maintenance PDA; ALSO the
    /// pool's manager fee account).
    pub maintenance_vault: Pubkey,
    /// `[stake_pool, b"withdraw"]` under the FORK program id.
    pub pool_withdraw_authority: Pubkey,
}

/// Build the WHOLE fuSOL pool stack for real against the loaded programs ([`new_svm_full`]):
///
/// 1. repoint the controller's upgrade authority to `payer` (the `initialize_controller` gate);
/// 2. create the fuSOL mint (9 dec, mint authority = the pool withdraw-authority PDA, freeze
///    None) and the maintenance vault (fuSOL account owned by the maintenance PDA);
/// 3. pre-create the zeroed, rent-exempt, fork-owned `StakePool` + `ValidatorList` accounts;
/// 4. create + initialize the reserve stake via the REAL stake program (staker + withdrawer =
///    the withdraw-authority PDA, no lockup), funded [`RESERVE_BOOTSTRAP_LAMPORTS`] above rent;
/// 5. `initialize_controller` (records the address set) then `initialize_pool` (the one-time
///    stake-pool `Initialize` CPI + seal), asserting the resulting on-chain authority graph.
///
/// `payer` funds everything (needs ≈ 3 SOL rent — the 74 KiB validator list dominates — plus
/// the reserve bootstrap); airdrop before calling.
pub fn pool_genesis(svm: &mut LiteSVM, payer: &Keypair) -> PoolGenesis {
    // (0) The genesis payer must hold the controller's upgrade authority (front-run gate),
    // mirroring the fusd-core `set_program_upgrade_authority` → `init_protocol` flow.
    set_upgrade_authority_at(svm, &fusion_stake_controller::ID, &payer.pubkey());

    let stake_pool_kp = Keypair::new();
    let validator_list_kp = Keypair::new();
    let reserve_kp = Keypair::new();
    let mint_kp = Keypair::new();
    let vault_kp = Keypair::new();

    let g = PoolGenesis {
        config: controller_config_pda(),
        epoch_state: controller_epoch_state_pda(),
        pool_authority: pool_authority_pda(),
        deposit_authority: deposit_authority_pda(),
        maintenance_authority: maintenance_authority_pda(),
        stake_pool: stake_pool_kp.pubkey(),
        validator_list: validator_list_kp.pubkey(),
        reserve_stake: reserve_kp.pubkey(),
        fusol_mint: mint_kp.pubkey(),
        maintenance_vault: vault_kp.pubkey(),
        pool_withdraw_authority: pool_withdraw_authority_pda(&stake_pool_kp.pubkey()),
    };

    // (a) fuSOL mint — spec §12.2: legacy SPL, 9 decimals, freeze None, mint authority = the
    // pool withdraw-authority PDA (only the stake-pool program can ever mint).
    create_mint(svm, payer, &mint_kp, 9, &g.pool_withdraw_authority, false);

    // (b) maintenance vault — plain SPL token account whose authority is the maintenance PDA
    // (a PDA can't sign an ATA-idempotent path here; a keyed account via initialize_account3
    // needs no owner signature at all).
    let vault_rent = Rent::default().minimum_balance(spl_token::state::Account::LEN);
    let create_vault = system_instruction::create_account(
        &payer.pubkey(),
        &g.maintenance_vault,
        vault_rent,
        spl_token::state::Account::LEN as u64,
        &SPL_TOKEN_ID,
    );
    let init_vault = spl_token::instruction::initialize_account3(
        &SPL_TOKEN_ID,
        &g.maintenance_vault,
        &g.fusol_mint,
        &g.maintenance_authority,
    )
    .unwrap();
    send(svm, &[create_vault, init_vault], payer, &[&vault_kp])
        .expect("create maintenance vault");

    // (c) pre-created, zeroed, rent-exempt, FORK-owned StakePool + ValidatorList accounts (the
    // real Initialize validates the list size yields exactly MAX_VALIDATORS).
    let create_pool = system_instruction::create_account(
        &payer.pubkey(),
        &g.stake_pool,
        Rent::default().minimum_balance(STAKE_POOL_ACCOUNT_SIZE),
        STAKE_POOL_ACCOUNT_SIZE as u64,
        &STAKE_POOL_FORK_ID,
    );
    let create_list = system_instruction::create_account(
        &payer.pubkey(),
        &g.validator_list,
        Rent::default().minimum_balance(VALIDATOR_LIST_ACCOUNT_SIZE),
        VALIDATOR_LIST_ACCOUNT_SIZE as u64,
        &STAKE_POOL_FORK_ID,
    );
    send(svm, &[create_pool, create_list], payer, &[&stake_pool_kp, &validator_list_kp])
        .expect("create StakePool + ValidatorList accounts");

    // (d) reserve stake via the REAL stake program: create (space 200, owner = stake program,
    // funded above rent) + initialize with staker+withdrawer = the withdraw PDA, no lockup.
    let stake_rent = Rent::default().minimum_balance(STAKE_ACCOUNT_SPACE);
    let create_reserve = system_instruction::create_account(
        &payer.pubkey(),
        &g.reserve_stake,
        stake_rent + RESERVE_BOOTSTRAP_LAMPORTS,
        STAKE_ACCOUNT_SPACE as u64,
        &STAKE_PROGRAM_ID,
    );
    let init_reserve = solana_sdk::stake::instruction::initialize(
        &g.reserve_stake,
        &solana_sdk::stake::state::Authorized {
            staker: g.pool_withdraw_authority,
            withdrawer: g.pool_withdraw_authority,
        },
        &solana_sdk::stake::state::Lockup::default(),
    );
    send(svm, &[create_reserve, init_reserve], payer, &[&reserve_kp])
        .expect("create + initialize reserve stake (real stake program)");

    // (e) genesis: record the address set, then the one-time stake-pool Initialize CPI + seal.
    send(svm, &[ctrl_initialize_controller_ix(&payer.pubkey(), &g)], payer, &[])
        .expect("initialize_controller");
    send(svm, &[ctrl_initialize_pool_ix(&payer.pubkey(), &g)], payer, &[])
        .expect("initialize_pool (real stake-pool Initialize CPI)");

    // Assert the authority graph the REAL processor just wrote (spec §4.1 / §17.2 row 1).
    let pool = read_fork_stake_pool(svm, &g.stake_pool);
    assert_eq!(pool.manager, g.pool_authority.to_bytes(), "manager = pool-authority PDA");
    assert_eq!(pool.staker, g.pool_authority.to_bytes(), "staker = pool-authority PDA");
    assert_eq!(
        pool.stake_deposit_authority,
        g.deposit_authority.to_bytes(),
        "stake deposit authority = deposit-authority PDA"
    );
    assert_eq!(
        pool.manager_fee_account,
        g.maintenance_vault.to_bytes(),
        "manager fee account = maintenance vault"
    );
    assert_eq!(pool.pool_mint, g.fusol_mint.to_bytes(), "pool mint = fuSOL mint");
    assert_eq!(pool.validator_list, g.validator_list.to_bytes());
    assert_eq!(pool.reserve_stake, g.reserve_stake.to_bytes());
    let config = read_controller_config(svm);
    assert!(config.sealed, "initialize_pool must seal the controller");
    assert_eq!(config.pool_withdraw_authority, g.pool_withdraw_authority);

    g
}

/// Create a REAL vote account via the native vote program builtin: system create (space = the
/// CURRENT `VoteState` size — litesvm runs `FeatureSet::all_enabled()`, so the vote-latency
/// layout is required) + `InitializeAccount` with `node` as identity/voter/withdrawer. Funds the
/// node with 10 SOL first (it pays and signs). Returns the vote account pubkey; the account
/// parses under `fusion_stake_view::vote_state` (tag 2 / V3) and passes `register_validator`.
pub fn create_vote_account(
    svm: &mut LiteSVM,
    node: &Keypair,
    vote: &Keypair,
    commission: u8,
) -> Pubkey {
    use solana_sdk::vote::instruction::{self as vote_instruction, CreateVoteAccountConfig};
    use solana_sdk::vote::state::{VoteInit, VoteStateVersions};

    airdrop_sol(svm, &node.pubkey(), 10);
    let space = VoteStateVersions::vote_state_size_of(true) as u64;
    let lamports = Rent::default().minimum_balance(space as usize);
    let ixs = vote_instruction::create_account_with_config(
        &node.pubkey(),
        &vote.pubkey(),
        &VoteInit {
            node_pubkey: node.pubkey(),
            authorized_voter: node.pubkey(),
            authorized_withdrawer: node.pubkey(),
            commission,
        },
        lamports,
        CreateVoteAccountConfig { space, with_seed: None },
    );
    send(svm, &ixs, node, &[vote]).expect("create vote account (real vote program)");
    vote.pubkey()
}

// ============================ controller: shared crank/fixture helpers ============================
//
// Shared by the fuSOL scenario files (deposit-stake, allocation, epoch machine). Each helper
// mirrors an on-chain derivation the handler re-checks, so a drift fails the test loudly.

/// Rewrite a REAL vote account (created by [`create_vote_account`]) into a "healthy"
/// observation through the reference serializer: positive epoch-credit growth for epochs 0..32
/// and a far-future freshness slot (saturates to fresh in the observation policy). Synthesized
/// because litesvm runs no leader schedule, so real votes can never land.
pub fn make_vote_healthy(svm: &mut LiteSVM, vote: &Pubkey) {
    use solana_sdk::vote::state::{BlockTimestamp, VoteState, VoteStateVersions};
    let mut acct = svm.get_account(vote).expect("vote account exists");
    let mut state = VoteState::deserialize(&acct.data).expect("real vote account parses");
    state.epoch_credits = (0u64..32).map(|e| (e, 100 * (e + 1), 100 * e)).collect();
    state.last_timestamp = BlockTimestamp { slot: 1_000_000, timestamp: 1 };
    let mut data = vec![0u8; VoteStateVersions::vote_state_size_of(true)];
    VoteState::serialize(&VoteStateVersions::new_current(state), &mut data)
        .expect("serialize vote state");
    acct.data = data;
    svm.set_account(*vote, acct).unwrap();
}

/// One registered, healthy-observing real validator (vote account + `ValidatorRecord`).
pub fn register_healthy_validator(svm: &mut LiteSVM, payer: &Keypair, commission: u8) -> Pubkey {
    let node = Keypair::new();
    let vote_kp = Keypair::new();
    let vote = create_vote_account(svm, &node, &vote_kp, commission);
    make_vote_healthy(svm, &vote);
    send(svm, &[ctrl_register_validator_ix(&payer.pubkey(), &vote)], payer, &[])
        .expect("register_validator");
    vote
}

/// The current validator-list entry at `i` (fusion-stake-view parse over the live fork bytes).
pub fn fork_list_entry(
    svm: &LiteSVM,
    validator_list: &Pubkey,
    i: u32,
) -> fusion_stake_view::validator_list::ValidatorEntry {
    let list = svm.get_account(validator_list).expect("validator list exists");
    fusion_stake_view::validator_list::entry_at(&list.data, i).expect("list entry")
}

/// Build the 4N reconcile quads for consecutive list indices `[start, start+n)` from the LIVE
/// validator list (the exact derivation `reconcile_batch` re-checks on-chain).
pub fn reconcile_quads(svm: &LiteSVM, g: &PoolGenesis, start: u32, n: u32) -> Vec<AccountMeta> {
    use fusion_stake_controller::spl_cpi;
    let mut tail = Vec::new();
    for i in start..start + n {
        let entry = fork_list_entry(svm, &g.validator_list, i);
        let vote = Pubkey::new_from_array(entry.vote_account_address);
        let vstake =
            spl_cpi::derive_validator_stake(&vote, &g.stake_pool, entry.validator_seed_suffix);
        let tstake =
            spl_cpi::derive_transient_stake(&vote, &g.stake_pool, entry.transient_seed_suffix);
        tail.push(AccountMeta::new(vstake, false));
        tail.push(AccountMeta::new(tstake, false));
        tail.push(AccountMeta::new(validator_record_pda(&vote), false));
        tail.push(AccountMeta::new_readonly(vote, false));
    }
    tail
}

/// `(validator_record [w], vote [])` pairs for `plan_directed_batch`.
pub fn plan_pairs(votes: &[Pubkey]) -> Vec<AccountMeta> {
    votes
        .iter()
        .flat_map(|v| {
            [
                AccountMeta::new(validator_record_pda(v), false),
                AccountMeta::new_readonly(*v, false),
            ]
        })
        .collect()
}

/// Writable `ValidatorRecord`s for `plan_neutral_batch`.
pub fn neutral_records(votes: &[Pubkey]) -> Vec<AccountMeta> {
    votes.iter().map(|v| AccountMeta::new(validator_record_pda(v), false)).collect()
}

/// Warp to the preference-window deadline and close the window (PREFERENCES → PLAN-DIRECTED).
pub fn ctrl_close_window_at_deadline(svm: &mut LiteSVM, payer: &Keypair) {
    let target = read_epoch_state(svm).preference_window_close_slot;
    let cur = current_slot(svm);
    if cur < target {
        warp_slots(svm, target - cur);
    }
    send(svm, &[ctrl_close_preference_window_ix()], payer, &[]).expect("close_preference_window");
}

/// Overwrite a program-owned account's data with a re-serialized Anchor account value (state
/// synthesis for fixtures/defensive probes; same lamports/owner, only the bytes change).
pub fn overwrite_anchor_account<T: AccountSerialize>(svm: &mut LiteSVM, addr: Pubkey, value: &T) {
    let mut acct = svm.get_account(&addr).expect("account exists");
    let mut data = Vec::with_capacity(acct.data.len());
    value.try_serialize(&mut data).expect("serialize account");
    assert!(data.len() <= acct.data.len(), "serialized form fits the allocation");
    data.resize(acct.data.len(), 0);
    acct.data = data;
    svm.set_account(addr, acct).expect("set_account");
}

/// Execute the pending ADMISSION add for `vote` (a planned Candidate without a list slot):
/// `AddValidatorToPool` at the seed-0 derived pair. Cursor-independent per the engine's
/// admission mode. Returns the tx metadata for event asserts.
pub fn execute_admission_add(
    svm: &mut LiteSVM,
    payer: &Keypair,
    g: &PoolGenesis,
    crank: &Pubkey,
    vote: &Pubkey,
) -> litesvm::types::TransactionMetadata {
    use fusion_stake_controller::spl_cpi;
    let vstake = spl_cpi::derive_validator_stake(vote, &g.stake_pool, 0);
    let tstake = spl_cpi::derive_transient_stake(vote, &g.stake_pool, 0);
    svm.expire_blockhash();
    send(svm, &[ctrl_execute_next_action_ix(g, vote, &vstake, &tstake, crank)], payer, &[])
        .expect("execute_next_action (admission AddValidatorToPool)")
}

/// Drive the REBALANCE walk to completion by mirroring the engine's own deterministic slot
/// selection (`fusion_stake_controller::logic::rebalance_slot` over the live list), executing
/// one action per call. Returns `(action_tag, lamports, vote)` per executed slot — the caller
/// asserts the choices. Panics if any step fails (the mirror and the engine must agree).
pub fn run_rebalance_walk(
    svm: &mut LiteSVM,
    payer: &Keypair,
    g: &PoolGenesis,
    crank: &Pubkey,
) -> Vec<(u8, u64, Pubkey)> {
    use fusion_stake_controller::spl_cpi;
    let mut out = Vec::new();
    loop {
        let es = read_epoch_state(svm);
        let Some(slot) = fusion_stake_controller::logic::rebalance_slot(
            es.rebalance_cursor,
            es.plan_directed_cursor,
            es.controller_epoch,
        ) else {
            break;
        };
        let entry = fork_list_entry(svm, &g.validator_list, slot.index as u32);
        let vote = Pubkey::new_from_array(entry.vote_account_address);
        let vstake =
            spl_cpi::derive_validator_stake(&vote, &g.stake_pool, entry.validator_seed_suffix);
        let tstake =
            spl_cpi::derive_transient_stake(&vote, &g.stake_pool, entry.transient_seed_suffix);
        svm.expire_blockhash();
        let meta = send(
            svm,
            &[ctrl_execute_next_action_ix(g, &vote, &vstake, &tstake, crank)],
            payer,
            &[],
        )
        .expect("execute_next_action (walk step at the mirrored slot)");
        let ev: fusion_stake_controller::events::RebalanceActionExecuted = single_event(&meta);
        out.push((ev.action, ev.lamports, vote));
    }
    out
}
