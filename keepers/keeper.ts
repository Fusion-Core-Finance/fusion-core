/**
 * fUSD keeper — the minimum-viable permissionless crank loop that keeps a market usable.
 *
 * SUPERSEDED for production cranking by oracle-crank.ts, which adds the mandatory Switchboard
 * update leg and derives cadences from the on-chain MarketOracle. Do NOT run this alongside
 * oracle-crank.ts: it double-cranks update_price, and its default 15s sample_twap bounces off the
 * on-chain anti-flood minimum. keeper.ts remains the dev/fork harness.
 *
 * One process, three cranks per market on independent intervals (Solana has no native cron):
 *   • sample_twap    — append a DEX (Orca/Raydium CLMM) observation to the per-market TWAP ring.
 *                      Borrow needs >= twap_min_samples spanning twap_window_secs, so this runs often.
 *   • update_price   — re-aggregate Pyth + Switchboard + TWAP into Market.spot/debt_spot. Without a
 *                      FRESH price the market self-freezes (spot ages past MAX_PRICE_STALENESS_SLOTS,
 *                      ~100s), pausing borrow/liquidate/ordered-redeem. So this runs every ~20-30s.
 *   • refresh_market — fold the aggregate interest accumulator and mint it into the insurance buffer
 *                      (pays the cranker a keeper_reward_bps cut when configured). Lower cadence.
 *
 * PRICE FRESHNESS — update_price READS a Pyth PriceUpdateV2 account (it does NOT post one itself).
 * Two modes per market:
 *   • "persistent" — point at a continuously-updated PriceUpdateV2 account (a sponsored mainnet feed,
 *      e.g. Pyth's SOL/USD pusher account). Anchor-only, node-18 OK. On a STATIC fork this account's
 *      publish_time is frozen at fork time and ages out — run surfpool with account refresh, or use:
 *   • "post" — Hermes-fetch a fresh update and post it via the Pyth receiver in the same tx as
 *      update_price (cluster-agnostic; the robust path). Lazily imports @pythnetwork/pyth-solana-receiver
 *      + @pythnetwork/hermes-client (needs node >= 20).
 *
 * Switchboard is OPTIONAL and pass-through here (the configured feed account is read; cranking it fresh
 * is a documented follow-on). NOTE: update_price's aggregate FREEZES MINTS when the secondary is absent
 * or stale, so for `borrow` to be enabled the Switchboard feed must be present + fresh and the TWAP
 * corridor satisfied — same requirement the surfpool harness documents.
 *
 * USAGE
 *   ANCHOR_PROVIDER_URL=<rpc> ANCHOR_WALLET=~/.config/solana/id.json \
 *     npx ts-node keepers/keeper.ts [config.json]
 *   No config arg → the built-in WSOL/USDC fork defaults below.
 */
import * as anchor from "@coral-xyz/anchor";
import * as fs from "fs";
import * as os from "os";
import { loadIdl } from "./common";

const { PublicKey, Keypair, Connection } = anchor.web3;
type Pk = anchor.web3.PublicKey;
const TOKEN_PROGRAM = new PublicKey("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

interface MarketCfg {
  collateralMint: string;
  clmmPool: string;            // Orca/Raydium pool for sample_twap (the on-chain CLMM the parser reads)
  pythMode: "persistent" | "post";
  pythFeedIdHex: string;       // 32-byte feed id (hex) — used by both modes
  pythAccount?: string;        // "persistent" mode: the continuously-updated PriceUpdateV2 account
  switchboardFeed?: string;    // optional secondary; read-through (omit → mints freeze by design)
}
interface KeeperCfg {
  hermesUrl?: string;
  twapIntervalSecs?: number;
  priceIntervalSecs?: number;
  refreshIntervalSecs?: number;
  markets: MarketCfg[];
}

const DEFAULT_CFG: KeeperCfg = {
  hermesUrl: "https://hermes.pyth.network",
  twapIntervalSecs: 15,
  priceIntervalSecs: 25,
  refreshIntervalSecs: 300,
  markets: [
    {
      collateralMint: "So11111111111111111111111111111111111111112",
      clmmPool: "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE", // Orca WSOL/USDC
      pythMode: "persistent",
      pythFeedIdHex: "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d", // SOL/USD
      pythAccount: "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE",
      switchboardFeed: "4Hmd6PdjVA9auCoScE12iaBogfwS4ZXQ6VZoBeqanwWW",
    },
  ],
};

const seed = (s: string) => Buffer.from(s);
function pda(seeds: (Buffer | Pk)[], pid: Pk): Pk {
  return PublicKey.findProgramAddressSync(seeds.map((s) => (s instanceof PublicKey ? s.toBuffer() : s)), pid)[0];
}
function loadWallet(): anchor.Wallet {
  const path = process.env.ANCHOR_WALLET || `${os.homedir()}/.config/solana/id.json`;
  return new anchor.Wallet(Keypair.fromSecretKey(Uint8Array.from(JSON.parse(fs.readFileSync(path, "utf8")))));
}
const log = (m: string) => console.log(`${new Date().toISOString()} ${m}`);

// Per-market PDA bundle (derived once).
function marketPdas(pid: Pk, coll: Pk) {
  return {
    coll,
    market: pda([seed("market"), coll], pid),
    marketOracle: pda([seed("oracle"), coll], pid),
    dexTwap: pda([seed("twap"), coll], pid),
    fusdMint: pda([seed("fusd_mint")], pid),
    mintAuthority: pda([seed("mint_authority")], pid),
    buffer: pda([seed("buffer"), coll], pid),
    bufferFusdVault: pda([seed("buffer_fusd"), coll], pid),
    config: pda([seed("config")], pid),
  };
}

async function sampleTwap(program: any, p: ReturnType<typeof marketPdas>, me: Pk, clmmPool: Pk) {
  await program.methods.sampleTwap().accounts({
    cranker: me, collateralMint: p.coll, marketOracle: p.marketOracle, dexTwap: p.dexTwap, clmmPool,
  }).rpc();
}

async function refreshMarket(program: any, p: ReturnType<typeof marketPdas>) {
  await program.methods.refreshMarket().accounts({
    collateralMint: p.coll, market: p.market, fusdMint: p.fusdMint, mintAuthority: p.mintAuthority,
    insuranceBuffer: p.buffer, bufferFusdVault: p.bufferFusdVault,
    crankerFusdAta: null, backstop: null, backstopFusdVault: null, tokenProgram: TOKEN_PROGRAM,
  }).rpc();
}

// "persistent" mode: update_price reading a pre-existing, continuously-updated PriceUpdateV2 account.
async function updatePricePersistent(program: any, p: ReturnType<typeof marketPdas>, me: Pk, m: MarketCfg) {
  await program.methods.updatePrice().accounts({
    cranker: me, config: p.config, collateralMint: p.coll, market: p.market, marketOracle: p.marketOracle,
    pythPriceUpdate: new PublicKey(m.pythAccount!),
    switchboardFeed: m.switchboardFeed ? new PublicKey(m.switchboardFeed) : null,
    dexTwap: p.dexTwap,
  }).rpc();
}

// "post" mode: Hermes-fetch a fresh update, post it via the Pyth receiver, append update_price in the
// SAME tx (the receiver builds the post txs + an ephemeral PriceUpdateV2 the consumer ix references).
async function updatePricePost(
  program: any, p: ReturnType<typeof marketPdas>, m: MarketCfg, provider: anchor.AnchorProvider, hermesUrl: string,
) {
  const { PythSolanaReceiver } = await import("@pythnetwork/pyth-solana-receiver");
  const { HermesClient } = await import("@pythnetwork/hermes-client");
  const hermes = new HermesClient(hermesUrl);
  const receiver = new PythSolanaReceiver({ connection: provider.connection, wallet: provider.wallet as any });
  const feed = "0x" + m.pythFeedIdHex;

  const updates = await hermes.getLatestPriceUpdates([feed]);
  const builder = receiver.newTransactionBuilder({ closeUpdateAccounts: true });
  await builder.addPostPriceUpdates(updates.binary.data as string[]);
  await builder.addPriceConsumerInstructions(async (getPriceUpdateAccount: (id: string) => Pk) => {
    const ix = await program.methods.updatePrice().accounts({
      cranker: provider.wallet.publicKey, config: p.config, collateralMint: p.coll, market: p.market,
      marketOracle: p.marketOracle, pythPriceUpdate: getPriceUpdateAccount(feed),
      switchboardFeed: m.switchboardFeed ? new PublicKey(m.switchboardFeed) : null, dexTwap: p.dexTwap,
    }).instruction();
    return [{ instruction: ix, signers: [] }];
  });
  const txs = await builder.buildVersionedTransactions({ computeUnitPriceMicroLamports: 50_000 });
  await receiver.provider.sendAll(txs as any);
}

// Run a crank on an interval, isolating failures so one bad tick never kills the loop.
function every(label: string, secs: number, fn: () => Promise<void>) {
  const tick = async () => {
    try { await fn(); log(`✓ ${label}`); }
    catch (e: any) { log(`✗ ${label}: ${(e?.message || String(e)).split("\n")[0]}`); }
  };
  tick();
  return setInterval(tick, secs * 1000);
}

// Fail fast on a malformed config.json. A 0/negative/NaN interval would busy-loop setInterval
// (machine-gunning the RPC + draining the keeper wallet), and a missing/typo'd per-market field
// would otherwise throw cryptically mid-crank — or, for a pythMode typo, silently mis-route to
// "persistent" and log a per-tick error forever while the market self-freezes. One clear startup
// error beats all three.
export function validateConfig(cfg: KeeperCfg): void {
  const bail = (m: string): never => {
    throw new Error(`config: ${m}`);
  };
  const pk = (field: string, v: string | undefined) => {
    if (!v) return bail(`${field} missing`);
    try {
      new PublicKey(v);
    } catch {
      bail(`${field} is not a valid base58 pubkey: ${v}`);
    }
  };
  for (const k of ["twapIntervalSecs", "priceIntervalSecs", "refreshIntervalSecs"] as const) {
    const v = cfg[k];
    if (v !== undefined && (typeof v !== "number" || !Number.isFinite(v) || v <= 0))
      bail(`${k} must be a positive number (got ${v})`);
  }
  if (!Array.isArray(cfg.markets) || cfg.markets.length === 0)
    bail("markets must be a non-empty array");
  cfg.markets.forEach((m, i) => {
    const at = `markets[${i}]`;
    pk(`${at}.collateralMint`, m.collateralMint);
    pk(`${at}.clmmPool`, m.clmmPool);
    if (m.pythMode !== "persistent" && m.pythMode !== "post")
      bail(`${at}.pythMode must be "persistent" or "post" (got ${m.pythMode})`);
    if (!/^[0-9a-fA-F]{64}$/.test(m.pythFeedIdHex ?? ""))
      bail(`${at}.pythFeedIdHex must be 64 hex chars`);
    if (m.pythMode === "persistent") pk(`${at}.pythAccount`, m.pythAccount);
    if (m.switchboardFeed) pk(`${at}.switchboardFeed`, m.switchboardFeed);
  });
}

async function main() {
  const cfgPath = process.argv[2];
  const cfg: KeeperCfg = cfgPath ? JSON.parse(fs.readFileSync(cfgPath, "utf8")) : DEFAULT_CFG;
  validateConfig(cfg);
  const url = process.env.ANCHOR_PROVIDER_URL || "http://127.0.0.1:8899";
  const wallet = loadWallet();
  const provider = new anchor.AnchorProvider(new Connection(url, "confirmed"), wallet, { commitment: "confirmed" });
  anchor.setProvider(provider);
  const program: any = new anchor.Program(loadIdl(), provider);
  const pid: Pk = program.programId;
  const me = wallet.publicKey;
  const hermesUrl = cfg.hermesUrl || "https://hermes.pyth.network";

  log(`keeper up — program ${pid.toBase58()}, wallet ${me.toBase58()}, RPC ${url}`);
  log(`markets: ${cfg.markets.map((m) => m.collateralMint).join(", ")}`);

  for (const m of cfg.markets) {
    const p = marketPdas(pid, new PublicKey(m.collateralMint));
    const clmm = new PublicKey(m.clmmPool);
    every(`sample_twap   ${m.collateralMint.slice(0, 6)}`, cfg.twapIntervalSecs ?? 15, () =>
      sampleTwap(program, p, me, clmm));
    every(`update_price  ${m.collateralMint.slice(0, 6)}`, cfg.priceIntervalSecs ?? 25, () =>
      m.pythMode === "post" ? updatePricePost(program, p, m, provider, hermesUrl)
        : updatePricePersistent(program, p, me, m));
    every(`refresh_mkt   ${m.collateralMint.slice(0, 6)}`, cfg.refreshIntervalSecs ?? 300, () =>
      refreshMarket(program, p));
  }
}

// Guard the entrypoint so the module is importable (e.g. by tests for `validateConfig`) without
// kicking off the crank loop; only run when invoked directly (`ts-node keepers/keeper.ts`).
if (require.main === module) {
  main().catch((e) => { console.error(e); process.exit(1); });
}
