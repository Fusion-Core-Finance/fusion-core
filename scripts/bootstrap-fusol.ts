/**
 * fuSOL genesis orchestrator — brings a freshly-deployed fusion-stake-controller (plus the
 * pinned SPL stake-pool FORK) to a live fuSOL pool.
 *
 * Creates the predeclared stake-pool-side account set, then runs the controller's two-step
 * genesis, idempotently (re-running skips whatever already exists), for any cluster:
 *
 *   a. the fuSOL mint            legacy SPL, 9 decimals, mint authority = the pool
 *                                withdraw-authority PDA, freeze authority None
 *   b. the maintenance vault     a plain fuSOL token account (create_account +
 *                                InitializeAccount3, NOT an ATA), token authority = the
 *                                controller's [b"maintenance"] PDA — becomes the pool's
 *                                manager fee account
 *   c. StakePool + ValidatorList fresh keypair accounts owned by the FORK program, rent-exempt
 *                                and zeroed; the list sized to EXACTLY MAX_VALIDATORS = 1024
 *   d. the reserve stake account space-200 stake account, staker + withdrawer = the pool
 *                                withdraw-authority PDA, funded above rent (the surplus becomes
 *                                the genesis pool value — the Initialize CPI mints exactly that
 *                                many fuSOL to the maintenance vault, seeding crank rewards and
 *                                anchoring the 1:1 share price)
 *   e. initialize_controller     records the address set in ControllerConfig
 *   f. initialize_pool           the one-time stake-pool Initialize CPI; seals the controller
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

async function trySend(label: string, fn: () => Promise<string>) {
  try {
    const sig = await fn();
    console.log(`  ✓ ${label}  (${sig.slice(0, 16)}…)`);
  } catch (e: any) {
    const msg = e?.message || String(e);
    // 0x0 = "account already in use" from an `init` on an existing PDA.
    if (/already in use|already exists|custom program error: 0x0\b/i.test(msg)) {
      console.log(`  • ${label} — already initialized, skipping`);
    } else {
      throw new Error(`${label} FAILED: ${msg}`);
    }
  }
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

  const ensure = async (label: string, address: Pk, create: () => Promise<string>) => {
    if (await connection.getAccountInfo(address)) {
      console.log(`  • ${label} — already exists, skipping`);
      return;
    }
    const sig = await create();
    console.log(`  ✓ ${label}  (${sig.slice(0, 16)}…)`);
  };
  const rent = (space: number) => connection.getMinimumBalanceForRentExemption(space);

  console.log("── stake-pool-side accounts ──");
  // (a) the fuSOL mint: 9 decimals, mint authority = the pool withdraw-authority PDA, no freeze.
  await ensure("fuSOL mint", addrs.fusolMint, async () =>
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
  await ensure("maintenance vault", addrs.maintenanceVault, async () =>
    provider.sendAndConfirm(
      new Transaction()
        .add(SystemProgram.createAccount({
          fromPubkey: me, newAccountPubkey: addrs.maintenanceVault, lamports: await rent(TOKEN_ACCOUNT_SPACE),
          space: TOKEN_ACCOUNT_SPACE, programId: TOKEN_PROGRAM,
        }))
        .add(ixInitializeAccount3(addrs.maintenanceVault, addrs.fusolMint, maintenanceAuthority())),
      [kpVault]
    ));

  // (c) StakePool + ValidatorList: fork-owned, rent-exempt, zeroed (the Initialize CPI fills them).
  await ensure("StakePool account", addrs.stakePool, async () =>
    provider.sendAndConfirm(
      new Transaction().add(SystemProgram.createAccount({
        fromPubkey: me, newAccountPubkey: addrs.stakePool, lamports: await rent(STAKE_POOL_SPACE),
        space: STAKE_POOL_SPACE, programId: STAKE_POOL_FORK_ID,
      })),
      [kpPool]
    ));
  await ensure(`ValidatorList account (${VALIDATOR_LIST_SPACE} bytes = ${MAX_VALIDATORS} entries)`, addrs.validatorList, async () =>
    provider.sendAndConfirm(
      new Transaction().add(SystemProgram.createAccount({
        fromPubkey: me, newAccountPubkey: addrs.validatorList, lamports: await rent(VALIDATOR_LIST_SPACE),
        space: VALIDATOR_LIST_SPACE, programId: STAKE_POOL_FORK_ID,
      })),
      [kpList]
    ));

  // (d) the reserve stake account: staker + withdrawer = the pool withdraw-authority PDA,
  // funded reserveExtra above rent (counted as genesis pool value by the Initialize CPI).
  await ensure("reserve stake account", addrs.reserveStake, async () =>
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

  // (e) initialize_controller — records the address set; gated to the upgrade authority via
  // the canonical ProgramData PDA.
  console.log("\n── controller genesis ──");
  const programData = PublicKey.findProgramAddressSync(
    [CONTROLLER_PROGRAM_ID.toBuffer()],
    BPF_LOADER_UPGRADEABLE
  )[0];
  await trySend("initialize_controller", () =>
    program.methods.initializeController({
      stakePool: addrs.stakePool, validatorList: addrs.validatorList,
      reserveStake: addrs.reserveStake, fusolMint: addrs.fusolMint,
      maintenanceVault: addrs.maintenanceVault,
    }).accounts({
      payer: me, programData, config: configPda, epochState: epochState(),
      poolAuthority: poolAuthority(), depositAuthority: depositAuthority(),
      maintenanceAuthority: maintenanceAuthority(), systemProgram: SystemProgram.programId,
    }).rpc());

  // (f) initialize_pool — the one-time stake-pool Initialize CPI; seals the controller. A
  // sealed controller means genesis already completed (re-running would hit AlreadySealed).
  const sealed = (await program.account.controllerConfig.fetch(configPda)).sealed as boolean;
  if (sealed) {
    console.log("  • initialize_pool — controller already sealed, skipping");
  } else {
    await trySend("initialize_pool", () =>
      program.methods.initializePool().accounts({
        payer: me, config: configPda, stakePool: addrs.stakePool,
        poolAuthority: poolAuthority(), depositAuthority: depositAuthority(),
        poolWithdrawAuthority: pwAuthority, validatorList: addrs.validatorList,
        reserveStake: addrs.reserveStake, fusolMint: addrs.fusolMint,
        maintenanceVault: addrs.maintenanceVault, maintenanceAuthority: maintenanceAuthority(),
        stakePoolProgram: STAKE_POOL_FORK_ID, tokenProgram: TOKEN_PROGRAM,
      }).rpc());
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
