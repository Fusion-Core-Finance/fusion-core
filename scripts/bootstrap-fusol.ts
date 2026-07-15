/**
 * fuSOL genesis orchestrator — brings a freshly-deployed fusion-stake-controller (plus the
 * pinned SPL stake-pool FORK) to a live fuSOL pool.
 *
 * Creates the predeclared stake-pool-side account set, then runs the controller's two-step
 * genesis, idempotently (re-running VERIFIES whatever already exists — existence alone is
 * never trusted — and skips it), for any cluster:
 *
 *   a. the fuSOL mint            legacy SPL, 9 decimals, mint authority = the pool
 *                                withdraw-authority PDA, freeze authority None
 *   b. the maintenance vault     a plain fuSOL token account (create_account +
 *                                InitializeAccount3, NOT an ATA), token authority = the
 *                                controller's [b"maintenance"] PDA — becomes the pool's
 *                                manager fee account
 *   c. the reserve stake account space-200 stake account, staker + withdrawer = the pool
 *                                withdraw-authority PDA, funded above rent (the surplus becomes
 *                                the genesis pool value — the Initialize CPI mints exactly that
 *                                many fuSOL to the maintenance vault, seeding crank rewards and
 *                                anchoring the 1:1 share price)
 *   d. initialize_controller     records the address set in ControllerConfig — BEFORE any
 *                                fork-owned account exists on-chain
 *   e. StakePool + ValidatorList fresh keypair accounts owned by the FORK program (rent-exempt,
 *      + initialize_pool         zeroed; the list sized to EXACTLY MAX_VALIDATORS = 1024),
 *                                created in the SAME atomic transaction as the controller's
 *                                one-time initialize_pool (the stake-pool Initialize CPI +
 *                                seal), so they are claimed the instant they exist
 *
 * ORDERING IS SECURITY-CRITICAL. The fork's `Initialize` is publicly callable and the
 * StakePool account is NOT a signer of it: a fork-owned, zeroed, rent-exempt StakePool (or
 * ValidatorList) left on-chain between transactions can be observed and initialized by ANYONE
 * with their own manager/staker/mint/list — permanently burning the predeclared address (an
 * initialized pool can never be reset). Hence (d) records the addresses before anything
 * claimable exists, and (e) creates both fork-owned accounts inside the very transaction whose
 * initialize_pool claims them — there is no cross-transaction window in which a bare account
 * is hijackable.
 *
 * The five predeclared addresses are Keypairs persisted under keys/fusol/ (keys/ is gitignored)
 * so a partial run resumes with the SAME addresses. Once ControllerConfig exists on-chain, ITS
 * recorded addresses are the source of truth and local keypairs are only used for signing.
 *
 * PREREQUISITES
 *   - BOTH programs are deployed + executable on the target cluster: the controller
 *     (Fz3z1yh21PQ59smsPjmjeyK6ngh8KoK6PiPxUgCgspFq) and the stake-pool FORK
 *     (3pYHXui7Zk21TKE6oqivqbVJWRXt74wdDkqsnb3Q8mMi).
 *   - A funded wallet that is the CONTROLLER'S UPGRADE AUTHORITY: initialize_controller is
 *     gated to it via the ProgramData account (front-run protection; nothing is recorded).
 *
 * USAGE
 *   ANCHOR_PROVIDER_URL=http://127.0.0.1:8899 ANCHOR_WALLET=~/.config/solana/id.json \
 *     npx ts-node scripts/bootstrap-fusol.ts [config.json]
 *
 *   With no config arg it uses the defaults below. The optional JSON may override
 *   `keypairDir` (where the five genesis keypairs live) and `reserveExtraLamports` (lamports
 *   funded above the reserve's rent floor; MUST be > 0 or the config default applies).
 */
import * as anchor from "@coral-xyz/anchor";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import {
  CONTROLLER_PROGRAM_ID,
  STAKE_POOL_FORK_ID,
  getControllerProgram,
  controllerConfig,
  epochState,
  poolAuthority,
  depositAuthority,
  maintenanceAuthority,
  poolWithdrawAuthority,
} from "../sdk/src/stake-pool";

const { PublicKey, SystemProgram, Keypair, Connection, Transaction, TransactionInstruction } =
  anchor.web3;
const { StakeProgram, Authorized } = anchor.web3;
type Pk = anchor.web3.PublicKey;
const TOKEN_PROGRAM = new PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"); // legacy SPL
const BPF_LOADER_UPGRADEABLE = new PublicKey("BPFLoaderUpgradeab1e11111111111111111111111");

// --- account sizes (pinned to the vendored fork / SPL Token) --------------------------------------
const MINT_SPACE = 82; // spl_token::state::Mint::LEN
const TOKEN_ACCOUNT_SPACE = 165; // spl_token::state::Account::LEN
// borsh get_packed_len::<StakePool>() at the pinned fork — the MAX size (all Options Some,
// FutureEpoch two-variant), so later SetFee-shaped growth can never overflow the account.
const STAKE_POOL_SPACE = 611;
// ValidatorListHeader::LEN (5) + BigVec u32 len (4) + MAX_VALIDATORS * ValidatorStakeInfo::LEN (73).
// The Initialize CPI requires calculate_max_validators(len) == 1024 EXACTLY.
const MAX_VALIDATORS = 1024;
const VALIDATOR_LIST_SPACE = 9 + MAX_VALIDATORS * 73;
const STAKE_ACCOUNT_SPACE = 200; // constants::STAKE_ACCOUNT_SPACE (size_of::<StakeStateV2>())

// --- config shape ----------------------------------------------------------------------------------
interface FusolBootstrapCfg {
  /** Directory holding the five genesis keypairs (created if absent). Default: keys/fusol. */
  keypairDir?: string;
  /**
   * Lamports funded ABOVE the reserve stake account's rent floor. The stake-pool Initialize
   * counts them as genesis pool value and mints exactly that many fuSOL to the maintenance
   * vault. Must be > 0 (the reserve must be funded above rent). Default: 1 SOL.
   */
  reserveExtraLamports?: string;
}

const DEFAULT_CFG: FusolBootstrapCfg = {
  reserveExtraLamports: "1000000000", // 1 SOL -> 1 fuSOL of genesis crank-reward budget
};

// --- helpers ----------------------------------------------------------------------------------------
function loadWallet(): anchor.Wallet {
  const p = process.env.ANCHOR_WALLET || `${os.homedir()}/.config/solana/id.json`;
  return new anchor.Wallet(Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(p, "utf8")))));
}

/** Load a persisted genesis keypair, or generate + persist one (0600, solana JSON format). */
function loadOrCreateKeypair(dir: string, name: string): anchor.web3.Keypair {
  const file = path.join(dir, `${name}.json`);
  if (fs.existsSync(file)) {
    return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(file, "utf8"))));
  }
  const kp = Keypair.generate();
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(file, JSON.stringify(Array.from(kp.secretKey)), { mode: 0o600 });
  return kp;
}

// SPL Token InitializeMint2 (tag 20): decimals, mint authority, freeze authority = COption::None.
function ixInitializeMint2(mint: Pk, decimals: number, mintAuthority: Pk) {
  const data = Buffer.alloc(35); // tag(1) + decimals(1) + authority(32) + freeze COption tag(1)=0
  data[0] = 20;
  data[1] = decimals;
  mintAuthority.toBuffer().copy(data, 2);
  return new TransactionInstruction({
    programId: TOKEN_PROGRAM,
    keys: [{ pubkey: mint, isSigner: false, isWritable: true }],
    data,
  });
}

// SPL Token InitializeAccount3 (tag 18): owner in data — no rent sysvar, no owner signature.
function ixInitializeAccount3(account: Pk, mint: Pk, owner: Pk) {
  const data = Buffer.alloc(33); // tag(1) + owner(32)
  data[0] = 18;
  owner.toBuffer().copy(data, 1);
  return new TransactionInstruction({
    programId: TOKEN_PROGRAM,
    keys: [
      { pubkey: account, isSigner: false, isWritable: true },
      { pubkey: mint, isSigner: false, isWritable: false },
    ],
    data,
  });
}

async function main() {
  const cfgPath = process.argv[2];
  const cfg: FusolBootstrapCfg = cfgPath
    ? { ...DEFAULT_CFG, ...JSON.parse(fs.readFileSync(cfgPath, "utf8")) }
    : DEFAULT_CFG;

  const url = process.env.ANCHOR_PROVIDER_URL || "http://127.0.0.1:8899";
  const wallet = loadWallet();
  const connection = new Connection(url, "confirmed");
  const provider = new anchor.AnchorProvider(connection, wallet, { commitment: "confirmed" });
  anchor.setProvider(provider);

  const program: any = getControllerProgram(provider);
  const me = wallet.publicKey;
  const reserveExtra = BigInt(cfg.reserveExtraLamports ?? DEFAULT_CFG.reserveExtraLamports!);
  if (reserveExtra <= 0n) throw new Error("reserveExtraLamports must be > 0 (reserve above rent)");

  console.log(`controller:  ${CONTROLLER_PROGRAM_ID.toBase58()}`);
  console.log(`pool fork:   ${STAKE_POOL_FORK_ID.toBase58()}`);
  console.log(`wallet:      ${me.toBase58()}  (must be the CONTROLLER's upgrade authority)`);
  console.log(`RPC:         ${url}\n`);

  for (const [name, pid] of [
    ["controller", CONTROLLER_PROGRAM_ID],
    ["stake-pool fork", STAKE_POOL_FORK_ID],
  ] as const) {
    const info = await connection.getAccountInfo(pid);
    if (!info?.executable) throw new Error(`${name} ${pid.toBase58()} is not deployed/executable on ${url}`);
  }

  // The five predeclared keypairs (persisted so a partial run resumes at the same addresses).
  const keypairDir = cfg.keypairDir ?? `${__dirname}/../keys/fusol`;
  const kpMint = loadOrCreateKeypair(keypairDir, "fusol_mint");
  const kpVault = loadOrCreateKeypair(keypairDir, "maintenance_vault");
  const kpPool = loadOrCreateKeypair(keypairDir, "stake_pool");
  const kpList = loadOrCreateKeypair(keypairDir, "validator_list");
  const kpReserve = loadOrCreateKeypair(keypairDir, "reserve_stake");

  // Once ControllerConfig exists its recorded addresses are the source of truth.
  const configPda = controllerConfig();
  let addrs = {
    fusolMint: kpMint.publicKey,
    maintenanceVault: kpVault.publicKey,
    stakePool: kpPool.publicKey,
    validatorList: kpList.publicKey,
    reserveStake: kpReserve.publicKey,
  };
  const cfgInfo = await connection.getAccountInfo(configPda);
  if (cfgInfo) {
    const onchain = await program.account.controllerConfig.fetch(configPda);
    const recorded = {
      fusolMint: onchain.fusolMint as Pk,
      maintenanceVault: onchain.maintenanceVault as Pk,
      stakePool: onchain.stakePool as Pk,
      validatorList: onchain.validatorList as Pk,
      reserveStake: onchain.reserveStake as Pk,
    };
    for (const k of Object.keys(addrs) as (keyof typeof addrs)[]) {
      if (!recorded[k].equals(addrs[k])) {
        console.log(`  ! ${k}: on-chain ${recorded[k].toBase58()} != local keypair ${addrs[k].toBase58()} — using on-chain`);
      }
    }
    addrs = recorded;
  }

  const pwAuthority = poolWithdrawAuthority(addrs.stakePool);

  // --- resume-state verifiers -----------------------------------------------------------------
  // Existence is NOT sufficiency: a resumed run must never skip an account that exists at the
  // right address with the WRONG contents (a hijacked / mis-created account exists too). Each
  // verifier returns null when the on-chain state matches this deployment's expectations,
  // otherwise a description of the mismatch — which aborts the bootstrap.
  type AcctInfo = anchor.web3.AccountInfo<Buffer>;
  const pkAt = (data: Buffer, off: number) => new PublicKey(data.subarray(off, off + 32));

  // SPL Mint layout: mint_authority COption (tag u32 @0, key @4), supply u64 @36,
  // decimals u8 @44, is_initialized u8 @45, freeze_authority COption (tag u32 @46, key @50).
  const verifyMint = (info: AcctInfo): string | null => {
    if (!info.owner.equals(TOKEN_PROGRAM)) return `owner ${info.owner.toBase58()} is not the SPL Token program`;
    if (info.data.length !== MINT_SPACE) return `size ${info.data.length} != ${MINT_SPACE}`;
    if (info.data[45] !== 1) return "mint is not initialized";
    if (info.data[44] !== 9) return `decimals ${info.data[44]} != 9`;
    if (info.data.readUInt32LE(0) !== 1 || !pkAt(info.data, 4).equals(pwAuthority))
      return `mint authority is not the pool withdraw-authority PDA ${pwAuthority.toBase58()}`;
    if (info.data.readUInt32LE(46) !== 0) return "freeze authority is set (must be None)";
    return null;
  };

  // SPL Token Account layout: mint @0, owner @32, amount u64 @64, delegate COption (tag u32
  // @72, key @76), state u8 @108, is_native COption<u64> @109, delegated_amount u64 @121,
  // close_authority COption (tag u32 @129, key @133).
  const verifyVault = (info: AcctInfo): string | null => {
    if (!info.owner.equals(TOKEN_PROGRAM)) return `owner ${info.owner.toBase58()} is not the SPL Token program`;
    if (info.data.length !== TOKEN_ACCOUNT_SPACE) return `size ${info.data.length} != ${TOKEN_ACCOUNT_SPACE}`;
    if (info.data[108] !== 1) return "token account is not in the Initialized state";
    if (!pkAt(info.data, 0).equals(addrs.fusolMint)) return "token account mint is not the fuSOL mint";
    if (!pkAt(info.data, 32).equals(maintenanceAuthority()))
      return `token authority is not the maintenance PDA ${maintenanceAuthority().toBase58()}`;
    if (info.data.readUInt32LE(72) !== 0) return "a delegate is set (must be None)";
    if (info.data.readUInt32LE(129) !== 0) return "a close authority is set (must be None)";
    return null;
  };

  // StakeStateV2 (bincode): discriminant u32 @0 (1 = Initialized, 2 = Stake), then Meta
  // { rent_exempt_reserve u64 @4, authorized.staker @12, authorized.withdrawer @44 }.
  const verifyReserve = (info: AcctInfo): string | null => {
    if (!info.owner.equals(StakeProgram.programId)) return `owner ${info.owner.toBase58()} is not the stake program`;
    if (info.data.length !== STAKE_ACCOUNT_SPACE) return `size ${info.data.length} != ${STAKE_ACCOUNT_SPACE}`;
    const tag = info.data.readUInt32LE(0);
    if (tag !== 1 && tag !== 2) return `stake state ${tag} is neither Initialized nor Stake`;
    if (!pkAt(info.data, 12).equals(pwAuthority) || !pkAt(info.data, 44).equals(pwAuthority))
      return `staker/withdrawer is not the pool withdraw-authority PDA ${pwAuthority.toBase58()}`;
    return null;
  };

  // Borsh StakePool head (pinned fork state.rs): account_type u8 @0 (1 = StakePool), manager
  // @1, staker @33, stake_deposit_authority @65, stake_withdraw_bump_seed u8 @97,
  // validator_list @98, reserve_stake @130, pool_mint @162, manager_fee_account @194,
  // token_program_id @226. Together these pin the pool's ENTIRE authority graph to this
  // controller — a pool initialized by anyone else fails here.
  const verifyStakePool = (info: AcctInfo): string | null => {
    if (!info.owner.equals(STAKE_POOL_FORK_ID)) return `owner ${info.owner.toBase58()} is not the stake-pool fork`;
    if (info.data.length !== STAKE_POOL_SPACE) return `size ${info.data.length} != ${STAKE_POOL_SPACE}`;
    if (info.data[0] !== 1) return `account_type ${info.data[0]} != StakePool`;
    const expected: [string, number, Pk][] = [
      ["manager", 1, poolAuthority()],
      ["staker", 33, poolAuthority()],
      ["stake_deposit_authority", 65, depositAuthority()],
      ["validator_list", 98, addrs.validatorList],
      ["reserve_stake", 130, addrs.reserveStake],
      ["pool_mint", 162, addrs.fusolMint],
      ["manager_fee_account", 194, addrs.maintenanceVault],
      ["token_program_id", 226, TOKEN_PROGRAM],
    ];
    for (const [name, off, want] of expected) {
      const got = pkAt(info.data, off);
      if (!got.equals(want)) return `${name} is ${got.toBase58()}, expected ${want.toBase58()}`;
    }
    return null;
  };

  /** Create `address` via `create` if absent; if it already exists, VERIFY it before skipping. */
  const ensure = async (
    label: string,
    address: Pk,
    verify: (info: AcctInfo) => string | null,
    create: () => Promise<string>
  ) => {
    const info = await connection.getAccountInfo(address);
    if (info) {
      const mismatch = verify(info);
      if (mismatch)
        throw new Error(
          `${label} at ${address.toBase58()} already exists but does not match the expected ` +
            `state: ${mismatch} — refusing to resume around a wrong account`
        );
      console.log(`  • ${label} — already exists, state verified, skipping`);
      return;
    }
    const sig = await create();
    console.log(`  ✓ ${label}  (${sig.slice(0, 16)}…)`);
  };
  const rent = (space: number) => connection.getMinimumBalanceForRentExemption(space);

  console.log("── stake-pool-side accounts ──");
  // (a) the fuSOL mint: 9 decimals, mint authority = the pool withdraw-authority PDA, no freeze.
  await ensure("fuSOL mint", addrs.fusolMint, verifyMint, async () =>
    provider.sendAndConfirm(
      new Transaction()
        .add(SystemProgram.createAccount({
          fromPubkey: me, newAccountPubkey: addrs.fusolMint, lamports: await rent(MINT_SPACE),
          space: MINT_SPACE, programId: TOKEN_PROGRAM,
        }))
        .add(ixInitializeMint2(addrs.fusolMint, 9, pwAuthority)),
      [kpMint]
    ));

  // (b) the maintenance vault: plain fuSOL token account, authority = the [b"maintenance"] PDA
  // (InitializeAccount3 leaves delegate + close authority None, as initialize_pool requires).
  await ensure("maintenance vault", addrs.maintenanceVault, verifyVault, async () =>
    provider.sendAndConfirm(
      new Transaction()
        .add(SystemProgram.createAccount({
          fromPubkey: me, newAccountPubkey: addrs.maintenanceVault, lamports: await rent(TOKEN_ACCOUNT_SPACE),
          space: TOKEN_ACCOUNT_SPACE, programId: TOKEN_PROGRAM,
        }))
        .add(ixInitializeAccount3(addrs.maintenanceVault, addrs.fusolMint, maintenanceAuthority())),
      [kpVault]
    ));

  // (c) the reserve stake account: staker + withdrawer = the pool withdraw-authority PDA,
  // funded reserveExtra above rent (counted as genesis pool value by the Initialize CPI).
  await ensure("reserve stake account", addrs.reserveStake, verifyReserve, async () =>
    provider.sendAndConfirm(
      new Transaction()
        .add(SystemProgram.createAccount({
          fromPubkey: me, newAccountPubkey: addrs.reserveStake,
          lamports: Number(BigInt(await rent(STAKE_ACCOUNT_SPACE)) + reserveExtra),
          space: STAKE_ACCOUNT_SPACE, programId: StakeProgram.programId,
        }))
        .add(StakeProgram.initialize({
          stakePubkey: addrs.reserveStake,
          authorized: new Authorized(pwAuthority, pwAuthority),
        })),
      [kpReserve]
    ));

  // (d) initialize_controller — records the address set BEFORE any fork-owned account exists
  // on-chain (nothing claimable may ever precede its controller); gated to the upgrade
  // authority via the canonical ProgramData PDA. The earlier ControllerConfig fetch already
  // proves genuine prior completion — no blind "already in use" skip.
  console.log("\n── controller genesis ──");
  if (cfgInfo) {
    console.log("  • initialize_controller — ControllerConfig already exists (addresses adopted above), skipping");
  } else {
    const programData = PublicKey.findProgramAddressSync(
      [CONTROLLER_PROGRAM_ID.toBuffer()],
      BPF_LOADER_UPGRADEABLE
    )[0];
    const sig = await program.methods.initializeController({
      stakePool: addrs.stakePool, validatorList: addrs.validatorList,
      reserveStake: addrs.reserveStake, fusolMint: addrs.fusolMint,
      maintenanceVault: addrs.maintenanceVault,
    }).accounts({
      payer: me, programData, config: configPda, epochState: epochState(),
      poolAuthority: poolAuthority(), depositAuthority: depositAuthority(),
      maintenanceAuthority: maintenanceAuthority(), systemProgram: SystemProgram.programId,
    }).rpc();
    console.log(`  ✓ initialize_controller  (${sig.slice(0, 16)}…)`);
  }

  // (e) StakePool + ValidatorList creation AND initialize_pool (the one-time stake-pool
  // Initialize CPI; seals the controller) in ONE atomic transaction. The fork's Initialize is
  // publicly callable and the StakePool account is not a signer of it, so the fork-owned
  // zeroed accounts must never exist outside the transaction that claims them (see the header
  // comment). A sealed controller means genesis already completed — the seal is written in the
  // same instruction as the CPI, so sealed is proof the pool was initialized by THIS controller.
  console.log("\n── stake-pool genesis ──");
  const sealed = (await program.account.controllerConfig.fetch(configPda)).sealed as boolean;
  if (sealed) {
    const info = await connection.getAccountInfo(addrs.stakePool);
    const mismatch = info ? verifyStakePool(info) : "account does not exist";
    if (mismatch) throw new Error(`controller is sealed but the StakePool account is wrong: ${mismatch}`);
    console.log("  • initialize_pool — controller already sealed, StakePool state verified, skipping");
  } else {
    // Triage a pre-existing pool/list (a resume of an older, non-atomic run). Initialized data
    // under an UNSEALED controller can only mean someone else ran the fork's public Initialize
    // against the predeclared account — a hijack. Fail loudly; never skip past it.
    const needsCreate = async (label: string, address: Pk, keypair: anchor.web3.Keypair, space: number) => {
      const info = await connection.getAccountInfo(address);
      if (!info) {
        if (!address.equals(keypair.publicKey))
          throw new Error(
            `${label} ${address.toBase58()} is recorded in ControllerConfig but does not exist, ` +
              `and the local keypair does not match — cannot sign its creation`
          );
        return true;
      }
      if (!info.owner.equals(STAKE_POOL_FORK_ID))
        throw new Error(`${label} ${address.toBase58()} exists but is owned by ${info.owner.toBase58()}, not the stake-pool fork`);
      if (info.data.length !== space)
        throw new Error(`${label} ${address.toBase58()} has size ${info.data.length}, expected ${space}`);
      if (info.data[0] !== 0)
        throw new Error(
          `${label} ${address.toBase58()} is ALREADY INITIALIZED while the controller is not sealed — ` +
            `the predeclared account was hijacked via the fork's public Initialize and can never be ` +
            `reused; this deployment needs a fresh address set (new keypairs + fresh controller genesis)`
        );
      console.log(`  • ${label} — exists (fork-owned, still zeroed), claiming it in the atomic init`);
      return false;
    };
    const needPool = await needsCreate("StakePool account", addrs.stakePool, kpPool, STAKE_POOL_SPACE);
    const needList = await needsCreate("ValidatorList account", addrs.validatorList, kpList, VALIDATOR_LIST_SPACE);

    const tx = new Transaction();
    const signers: anchor.web3.Keypair[] = [];
    if (needPool) {
      tx.add(SystemProgram.createAccount({
        fromPubkey: me, newAccountPubkey: addrs.stakePool, lamports: await rent(STAKE_POOL_SPACE),
        space: STAKE_POOL_SPACE, programId: STAKE_POOL_FORK_ID,
      }));
      signers.push(kpPool);
    }
    if (needList) {
      tx.add(SystemProgram.createAccount({
        fromPubkey: me, newAccountPubkey: addrs.validatorList, lamports: await rent(VALIDATOR_LIST_SPACE),
        space: VALIDATOR_LIST_SPACE, programId: STAKE_POOL_FORK_ID,
      }));
      signers.push(kpList);
    }
    tx.add(await program.methods.initializePool().accounts({
      payer: me, config: configPda, stakePool: addrs.stakePool,
      poolAuthority: poolAuthority(), depositAuthority: depositAuthority(),
      poolWithdrawAuthority: pwAuthority, validatorList: addrs.validatorList,
      reserveStake: addrs.reserveStake, fusolMint: addrs.fusolMint,
      maintenanceVault: addrs.maintenanceVault, maintenanceAuthority: maintenanceAuthority(),
      stakePoolProgram: STAKE_POOL_FORK_ID, tokenProgram: TOKEN_PROGRAM,
    }).instruction());
    const steps = [
      ...(needPool ? ["create StakePool"] : []),
      ...(needList ? [`create ValidatorList (${VALIDATOR_LIST_SPACE} bytes = ${MAX_VALIDATORS} entries)`] : []),
      "initialize_pool",
    ];
    const sig = await provider.sendAndConfirm(tx, signers);
    console.log(`  ✓ ${steps.join(" + ")} — one atomic tx  (${sig.slice(0, 16)}…)`);

    // Post-init verification: read the pool back and check the full recorded authority graph.
    const info = await connection.getAccountInfo(addrs.stakePool);
    const mismatch = info ? verifyStakePool(info) : "account does not exist";
    if (mismatch) throw new Error(`initialize_pool landed but StakePool verification failed: ${mismatch}`);
    console.log("  ✓ StakePool state verified (manager/staker/deposit authority/list/reserve/mint/fee vault)");
  }

  // The immutability-checklist manifest: every predeclared / derived address of the deployment.
  console.log("\n── fuSOL genesis manifest ──");
  const manifest: [string, Pk][] = [
    ["controller_program", CONTROLLER_PROGRAM_ID],
    ["stake_pool_program", STAKE_POOL_FORK_ID],
    ["controller_config", configPda],
    ["epoch_state", epochState()],
    ["pool_authority", poolAuthority()],
    ["deposit_authority", depositAuthority()],
    ["maintenance_authority", maintenanceAuthority()],
    ["pool_withdraw_authority", pwAuthority],
    ["fusol_mint", addrs.fusolMint],
    ["maintenance_vault", addrs.maintenanceVault],
    ["stake_pool", addrs.stakePool],
    ["validator_list", addrs.validatorList],
    ["reserve_stake", addrs.reserveStake],
  ];
  for (const [name, addr] of manifest) console.log(`${name.padEnd(24)} ${addr.toBase58()}`);

  console.log("\n✓ fuSOL bootstrap complete.");
}

main().then(() => process.exit(0)).catch((e) => { console.error(e); process.exit(1); });
