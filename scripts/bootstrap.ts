/**
 * Bootstrap orchestrator — brings a freshly-deployed fUSD program to life.
 *
 * Runs the on-chain-enforced init sequence in order, idempotently (re-running skips whatever already
 * exists), for any cluster: a surfpool mainnet-fork, devnet, or mainnet.
 *
 *   1. init_protocol            (creates ProtocolConfig, the fUSD mint PDA, mint-authority PDA)
 *   2. init_governance_gate     (the bounded param-setter authority + timelock)
 *   then, per collateral market:
 *   3. init_market              (Market + collateral vault + redemption bitmap)
 *   4. init_market_oracle       (Pyth feed id + Switchboard feed + Orca/Raydium pool + thresholds)
 *   5. init_reactor_pool        (RP deposit/coll vaults + the P/S grid)
 *   6. init_insurance_buffer    (the tier-3 fUSD loss-absorption reserve)
 *
 * The global backstop (init_global_backstop) is OPTIONAL and intentionally NOT run here — it ships
 * inert and is a later, protocol-wide step.
 *
 * PREREQUISITES
 *   - The program is deployed + executable on the target cluster (scripts/deploy.ts or surfpool).
 *   - The IDL exists at target/idl/fusd_core.json (anchor build).
 *   - A funded wallet that is the program's UPGRADE AUTHORITY (init_protocol is gated to it).
 *
 * USAGE
 *   ANCHOR_PROVIDER_URL=http://127.0.0.1:8899 ANCHOR_WALLET=~/.config/solana/id.json \
 *     npx ts-node scripts/bootstrap.ts [config.json]
 *
 *   With no config arg it uses the built-in WSOL defaults below (the surfpool-harness known-good set),
 *   which target a mainnet-fork. For devnet/mainnet, pass a JSON file overriding `markets[].oracle`
 *   (real feed accounts for that cluster) and the authorities.
 */
import * as anchor from "@coral-xyz/anchor";
import * as fs from "fs";
import * as os from "os";

const { PublicKey, SystemProgram, Keypair, Connection } = anchor.web3;
type Pk = anchor.web3.PublicKey;
const BN = anchor.BN;
const TOKEN_PROGRAM = new PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"); // legacy SPL

// --- config shape -------------------------------------------------------------------------------
interface OracleCfg {
  pythFeedIdHex: string;       // 32-byte Pyth feed id, hex (no 0x)
  switchboardFeed: string;     // Switchboard On-Demand PullFeed account (or "" / default)
  orcaPool: string;            // Orca Whirlpool for the collateral/quote pair (or "" = none)
  raydiumPool: string;         // Raydium CLMM pool (or "" = none)
  maxConfBps: number;
  maxDeviationBps: number;
  twapMaxDivergenceBps: number;
  maxAgeSecs: number;
  kBps: number;                // asymmetric-pricing haircut (k·sigma)
  twapWindowSecs: number;      // >= 300 (clamp)
  twapMinSamples: number;      // >= 3 (clamp)
  twapMaxStalenessSecs: number;
}
interface MarketCfg {
  collateralMint: string;
  quoteMint: string;
  params: {
    mcrBps: number; debtCeiling: string; reserveLamports: string;
    liqGasCompBps: number; bucketWidthBps: number; redemptionFeeBps: number; liqBonusBps: number;
  };
  oracle: OracleCfg;
}
interface BootstrapCfg {
  // Authorities — default to the deploying wallet for a single-key demo; point at a Squads vault for prod.
  govAuthority?: string;
  guardian?: string;
  inboundAuthority?: string;
  timelockSecs?: number;
  markets: MarketCfg[];
}

// Built-in default: WSOL on a mainnet-fork (the surfpool-harness known-good values).
const DEFAULT_CFG: BootstrapCfg = {
  timelockSecs: 0, // 0 permitted for a guarded/demo launch (MIN_GOV_TIMELOCK_SECS)
  markets: [
    {
      collateralMint: "So11111111111111111111111111111111111111112",
      quoteMint: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
      params: {
        mcrBps: 15000, debtCeiling: "1000000000000", reserveLamports: "0",
        liqGasCompBps: 0, bucketWidthBps: 10, redemptionFeeBps: 0, liqBonusBps: 1000,
      },
      oracle: {
        pythFeedIdHex: "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d",
        switchboardFeed: "4Hmd6PdjVA9auCoScE12iaBogfwS4ZXQ6VZoBeqanwWW",
        orcaPool: "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE",
        raydiumPool: "",
        maxConfBps: 200, maxDeviationBps: 500, twapMaxDivergenceBps: 1000,
        maxAgeSecs: 300, kBps: 21200,
        twapWindowSecs: 300, twapMinSamples: 3, twapMaxStalenessSecs: 3600,
      },
    },
  ],
};

// --- helpers ------------------------------------------------------------------------------------
const seed = (s: string) => Buffer.from(s);
function pda(seeds: (Buffer | Pk)[], pid: Pk): Pk {
  return PublicKey.findProgramAddressSync(seeds.map((s) => (s instanceof PublicKey ? s.toBuffer() : s)), pid)[0];
}
function loadWallet(): anchor.Wallet {
  const path = process.env.ANCHOR_WALLET || `${os.homedir()}/.config/solana/id.json`;
  return new anchor.Wallet(Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(path, "utf8")))));
}
const opt = (s: string): Pk => (s ? new PublicKey(s) : PublicKey.default);

async function trySend(label: string, fn: () => Promise<string>) {
  try {
    const sig = await fn();
    console.log(`  ✓ ${label}  (${sig.slice(0, 16)}…)`);
  } catch (e: any) {
    const msg = e?.message || String(e);
    // 0x0 = Anchor's "account already in use" from an `init` on an existing PDA.
    if (/already in use|already exists|custom program error: 0x0\b/i.test(msg)) {
      console.log(`  • ${label} — already initialized, skipping`);
    } else {
      throw new Error(`${label} FAILED: ${msg}`);
    }
  }
}

async function main() {
  const cfgPath = process.argv[2];
  const cfg: BootstrapCfg = cfgPath ? JSON.parse(fs.readFileSync(cfgPath, "utf8")) : DEFAULT_CFG;

  const url = process.env.ANCHOR_PROVIDER_URL || "http://127.0.0.1:8899";
  const wallet = loadWallet();
  const provider = new anchor.AnchorProvider(new Connection(url, "confirmed"), wallet, { commitment: "confirmed" });
  anchor.setProvider(provider);

  const idl = JSON.parse(fs.readFileSync(`${__dirname}/../target/idl/fusd_core.json`, "utf8"));
  const program: any = new anchor.Program(idl as anchor.Idl, provider);
  const pid: Pk = program.programId;
  const me = wallet.publicKey;

  const govAuthority = cfg.govAuthority ? new PublicKey(cfg.govAuthority) : me;
  const guardian = cfg.guardian ? new PublicKey(cfg.guardian) : me;
  const inboundAuthority = cfg.inboundAuthority ? new PublicKey(cfg.inboundAuthority) : me;
  const timelockSecs = cfg.timelockSecs ?? 0;

  console.log(`fUSD program: ${pid.toBase58()}`);
  console.log(`wallet:       ${me.toBase58()}  (must be the program's upgrade authority)`);
  console.log(`RPC:          ${url}`);
  console.log(`gov auth:     ${govAuthority.toBase58()}\nguardian:     ${guardian.toBase58()}`);
  console.log(`inbound auth: ${inboundAuthority.toBase58()}  timelock: ${timelockSecs}s\n`);

  const info = await provider.connection.getAccountInfo(pid);
  if (!info?.executable) throw new Error(`program ${pid.toBase58()} is not deployed/executable on ${url}`);

  // Protocol-wide PDAs.
  const config = pda([seed("config")], pid);
  const fusdMint = pda([seed("fusd_mint")], pid);
  const mintAuthority = pda([seed("mint_authority")], pid);
  const govGate = pda([seed("gov_gate")], pid);
  console.log(`fUSD mint (PDA): ${fusdMint.toBase58()}\n`);

  console.log("── protocol ──");
  await trySend("init_protocol", () =>
    program.methods.initProtocol({ govAuthority, guardian }).accounts({
      payer: me, config, mintAuthority, fusdMint, tokenProgram: TOKEN_PROGRAM,
      systemProgram: SystemProgram.programId, rent: anchor.web3.SYSVAR_RENT_PUBKEY,
    }).rpc());
  await trySend("init_governance_gate", () =>
    program.methods.initGovernanceGate(inboundAuthority, new BN(timelockSecs)).accounts({
      authority: me, config, govGate, systemProgram: SystemProgram.programId,
    }).rpc());

  for (const m of cfg.markets) {
    const coll = new PublicKey(m.collateralMint);
    console.log(`\n── market ${coll.toBase58()} ──`);
    const market = pda([seed("market"), coll], pid);
    const collateralVault = pda([seed("coll_vault"), coll], pid);
    const redemptionBitmap = pda([seed("redeem_bitmap"), coll], pid);
    const marketOracle = pda([seed("oracle"), coll], pid);
    const dexTwap = pda([seed("twap"), coll], pid);
    const reactorPool = pda([seed("reactor"), coll], pid);
    const ess = pda([seed("ess"), coll], pid);
    const reactorFusdVault = pda([seed("reactor_fusd"), coll], pid);
    const reactorCollVault = pda([seed("reactor_coll"), coll], pid);
    const buffer = pda([seed("buffer"), coll], pid);
    const bufferFusdVault = pda([seed("buffer_fusd"), coll], pid);

    await trySend("init_market", () =>
      program.methods.initMarket({
        mcrBps: m.params.mcrBps, debtCeiling: new BN(m.params.debtCeiling),
        reserveLamports: new BN(m.params.reserveLamports), liqGasCompBps: m.params.liqGasCompBps,
        bucketWidthBps: m.params.bucketWidthBps, redemptionFeeBps: m.params.redemptionFeeBps,
        liqBonusBps: m.params.liqBonusBps,
      }).accounts({
        authority: me, config, collateralMint: coll, market, collateralVault, redemptionBitmap,
        tokenProgram: TOKEN_PROGRAM, systemProgram: SystemProgram.programId, rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      }).rpc());

    const o = m.oracle;
    await trySend("init_market_oracle", () =>
      program.methods.initMarketOracle({
        pythFeedId: Array.from(Buffer.from(o.pythFeedIdHex, "hex")),
        switchboardFeed: opt(o.switchboardFeed), orcaPool: opt(o.orcaPool), raydiumPool: opt(o.raydiumPool),
        maxConfBps: o.maxConfBps, maxDeviationBps: o.maxDeviationBps, twapMaxDivergenceBps: o.twapMaxDivergenceBps,
        maxAgeSecs: new BN(o.maxAgeSecs), kBps: o.kBps,
        twapWindowSecs: new BN(o.twapWindowSecs), twapMinSamples: o.twapMinSamples,
        twapMaxStalenessSecs: new BN(o.twapMaxStalenessSecs),
      }).accounts({
        authority: me, config, collateralMint: coll, quoteMint: new PublicKey(m.quoteMint),
        market, marketOracle, dexTwap, systemProgram: SystemProgram.programId,
      }).rpc());

    // ORDERING (audit #24 / L-02, now ENFORCED ON-CHAIN): the ReactorPool + InsuranceBuffer below
    // are liquidation PREREQUISITES — `liquidate` requires both as non-optional accounts. Since
    // L-02, `borrow` rejects LiquidationInfraNotReady (6048) until BOTH inits have run
    // (Market.liq_infra_flags), so a mis-sequenced deploy fails safe instead of minting
    // unliquidatable debt. This loop still creates them here: a market cannot borrow until it
    // completes.
    await trySend("init_reactor_pool", () =>
      program.methods.initReactorPool().accounts({
        authority: me, config, collateralMint: coll, fusdMint, market, reactorPool,
        epochToScaleToSum: ess, reactorFusdVault, reactorCollVault, tokenProgram: TOKEN_PROGRAM,
        systemProgram: SystemProgram.programId, rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      }).rpc());

    await trySend("init_insurance_buffer", () =>
      program.methods.initInsuranceBuffer().accounts({
        authority: me, config, collateralMint: coll, fusdMint, market, insuranceBuffer: buffer,
        bufferFusdVault, tokenProgram: TOKEN_PROGRAM, systemProgram: SystemProgram.programId,
        rent: anchor.web3.SYSVAR_RENT_PUBKEY,
      }).rpc());
  }

  console.log("\n✓ bootstrap complete.");
}

main().then(() => process.exit(0)).catch((e) => { console.error(e); process.exit(1); });
