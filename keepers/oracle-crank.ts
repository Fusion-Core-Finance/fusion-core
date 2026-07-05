/**
 * fUSD oracle crank — the permissionless keeper that keeps a market PRICED and MINTABLE. The oracle
 * aggregate's Ok mode fail-closes unless ALL THREE legs are fresh (fusd-oracle::aggregate):
 *   1. Switchboard: on-demand feeds only update when a consumer cranks them — nobody else cranks
 *      ours (the mainnet dry run found the SOL/USD feed 566s stale, mints frozen), so this keeper
 *      pulls a signed update every sbIntervalSecs (default 2/3 of the oracle's max_age_secs).
 *   2. DEX TWAP: sample_twap every sampleIntervalSecs (default the on-chain minimum,
 *      ceil(window/(min_samples−1)), + slack) so the window always holds enough fresh samples.
 *   3. update_price itself: recommits Market.spot every priceIntervalSecs (default 60s) so borrow's
 *      MAX_PRICE_STALENESS_SLOTS (250-slot ≈ 100s) gate stays open.
 *
 * Everything per-market is read from the on-chain MarketOracle — the Switchboard feed, the CLMM
 * pool, the LST canonical leg, and the Pyth account (derived: the push-oracle sponsored feed is the
 * PDA [shard_u16_le, feed_id]). Config is just the market list + optional interval overrides.
 *
 * COST: at defaults ≈ 2,500 txs/market/day ≈ 0.013 SOL base fees (+ any priority fee).
 *
 * USAGE
 *   ANCHOR_PROVIDER_URL=<rpc> ANCHOR_WALLET=~/.config/solana/id.json \
 *     npx ts-node keepers/oracle-crank.ts [config.json]
 *   No config arg → the built-in WSOL default below.
 */
import * as anchor from "@coral-xyz/anchor";
import * as fs from "fs";
import { getDefaultQueue, PullFeed } from "@switchboard-xyz/on-demand";
import { PublicKey, Pk, pda, seed, bundle, log, makeProgram, errLine } from "./common";

const PYTH_PUSH_ORACLE = new PublicKey("pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT");
const SOL_USD_FEED_ID = Buffer.from("ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d", "hex");
const ZERO = PublicKey.default;

interface CrankCfg {
  markets: string[]; // collateral mints
  tickSecs?: number; // dueness check cadence (default 15)
  priceIntervalSecs?: number; // update_price cadence (default 60)
  sbIntervalSecs?: number; // Switchboard update cadence (default derived from max_age_secs)
  sampleIntervalSecs?: number; // sample_twap cadence (default derived from window/min_samples)
  pythShard?: number; // sponsored-feed shard (default 0)
}
const DEFAULT_CFG: CrankCfg = { markets: ["So11111111111111111111111111111111111111112"] };

export function validateConfig(cfg: CrankCfg): void {
  const bail = (m: string): never => { throw new Error(`config: ${m}`); };
  if (!Array.isArray(cfg.markets) || cfg.markets.length === 0) bail("markets must be a non-empty array");
  cfg.markets.forEach((m, i) => { try { new PublicKey(m); } catch { bail(`markets[${i}] is not a valid pubkey: ${m}`); } });
  for (const k of ["tickSecs", "priceIntervalSecs", "sbIntervalSecs", "sampleIntervalSecs"] as const) {
    const v = cfg[k];
    if (v !== undefined && (typeof v !== "number" || !Number.isFinite(v) || v <= 0)) bail(`${k} must be a positive number (got ${v})`);
  }
  if (cfg.pythShard !== undefined && (!Number.isInteger(cfg.pythShard) || cfg.pythShard < 0 || cfg.pythShard > 0xffff))
    bail(`pythShard must be a u16 (got ${cfg.pythShard})`);
}

// ── pure helpers (unit-tested in oracle-crank.spec.ts) ─────────────────────────────────────────
/** The Pyth push-oracle sponsored-feed account: PDA [shard u16 LE, feed_id]. */
export function pythFeedAccount(shard: number, feedId: Buffer | Uint8Array): Pk {
  const s = Buffer.alloc(2); s.writeUInt16LE(shard);
  return PublicKey.findProgramAddressSync([s, Buffer.from(feedId)], PYTH_PUSH_ORACLE)[0];
}
/** Safe crank cadences from the on-chain oracle config; explicit cfg values win. */
export function intervalsFrom(o: { maxAgeSecs: number; twapWindowSecs: number; twapMinSamples: number }, cfg: CrankCfg) {
  // sample_twap's on-chain anti-flood minimum is ceil(window/(min_samples-1)); add slack so a
  // slightly-early tick never bounces. SB must land well inside max_age or Ok mode drops.
  const sampleFloor = Math.ceil(o.twapWindowSecs / Math.max(1, o.twapMinSamples - 1));
  return {
    sample: cfg.sampleIntervalSecs ?? sampleFloor + 5,
    sb: cfg.sbIntervalSecs ?? Math.max(30, Math.floor((o.maxAgeSecs * 2) / 3)),
    price: cfg.priceIntervalSecs ?? 60,
  };
}
/** Is an action due? `last` 0 = never done. */
export const due = (nowSecs: number, lastSecs: number, intervalSecs: number): boolean =>
  lastSecs === 0 || nowSecs - lastSecs >= intervalSecs;

// ── per-market crank state ─────────────────────────────────────────────────────────────────────
interface MarketCrank {
  coll: Pk; tag: string; market: Pk; marketOracle: Pk; dexTwap: Pk;
  clmmPool: Pk; sbFeed: Pk | null; pullFeed: any | null;
  pythPriceUpdate: Pk; solUsdPythUpdate: Pk | null; lstStakePool: Pk | null;
  intervals: { sample: number; sb: number; price: number };
  lastSample: number; lastSb: number; lastPrice: number;
}

async function loadMarket(program: any, queue: any, pid: Pk, coll: Pk, cfg: CrankCfg): Promise<MarketCrank> {
  const tag = coll.toBase58().slice(0, 6);
  const marketOracle = pda([seed("oracle"), coll], pid);
  const o: any = await program.account.marketOracle.fetch(marketOracle);
  const orca: Pk = o.orcaPool, ray: Pk = o.raydiumPool;
  const clmmPool = !orca.equals(ZERO) ? orca : ray;
  if (clmmPool.equals(ZERO)) throw new Error(`${tag}: no CLMM pool configured — TWAP leg impossible, market can never mint`);
  const sbFeed: Pk | null = o.switchboardFeed.equals(ZERO) ? null : o.switchboardFeed;
  if (!sbFeed) log(`⚠ ${tag}: no Switchboard feed configured — Ok mode is unreachable, cranking price/TWAP only`);
  const shard = cfg.pythShard ?? 0;
  const pythPriceUpdate = pythFeedAccount(shard, Buffer.from(o.pythFeedId));
  if (!(await program.provider.connection.getAccountInfo(pythPriceUpdate)))
    throw new Error(`${tag}: derived Pyth sponsored feed ${pythPriceUpdate.toBase58()} does not exist (shard ${shard}) — check pythShard`);
  // C1 LST leg: when the oracle carries a stake pool, update_price needs the SOL/USD sponsored feed too.
  const lst = !o.lstStakePool.equals(ZERO);
  const intervals = intervalsFrom(
    { maxAgeSecs: Number(o.maxAgeSecs), twapWindowSecs: Number(o.twapWindowSecs), twapMinSamples: Number(o.twapMinSamples) },
    cfg,
  );
  log(`${tag}: pool ${clmmPool.toBase58().slice(0, 6)}…, sb ${sbFeed ? sbFeed.toBase58().slice(0, 6) + "…" : "none"}, pyth ${pythPriceUpdate.toBase58().slice(0, 6)}…${lst ? ", LST leg on" : ""} | intervals sample=${intervals.sample}s sb=${intervals.sb}s price=${intervals.price}s`);
  return {
    coll, tag, market: bundle(pid, coll).market, marketOracle, dexTwap: pda([seed("twap"), coll], pid),
    clmmPool, sbFeed, pullFeed: sbFeed && queue ? new PullFeed(queue.program, sbFeed) : null,
    pythPriceUpdate, solUsdPythUpdate: lst ? pythFeedAccount(shard, SOL_USD_FEED_ID) : null,
    lstStakePool: lst ? o.lstStakePool : null,
    intervals, lastSample: 0, lastSb: 0, lastPrice: 0,
  };
}

async function crankSb(program: any, me: Pk, wallet: anchor.Wallet, mc: MarketCrank): Promise<void> {
  // payer MUST be our wallet — omitted, the ix binds the SDK's dummy provider as a signer.
  const [ixs, responses, , luts] = await mc.pullFeed.fetchUpdateIx({ numSignatures: 3, payer: me });
  if (!ixs || ixs.length === 0)
    throw new Error(`no SB update ix (${JSON.stringify(responses?.map((r: any) => r?.error ?? "ok"))})`);
  const conn = program.provider.connection;
  const { blockhash } = await conn.getLatestBlockhash();
  const msg = new anchor.web3.TransactionMessage({ payerKey: me, recentBlockhash: blockhash, instructions: ixs })
    .compileToV0Message(luts ?? []);
  const tx = new anchor.web3.VersionedTransaction(msg);
  tx.sign([wallet.payer]);
  const sig = await conn.sendTransaction(tx);
  await conn.confirmTransaction(sig, "confirmed");
  log(`  ✓ ${mc.tag} switchboard update (${sig.slice(0, 16)}…)`);
}

async function tick(program: any, me: Pk, wallet: anchor.Wallet, mc: MarketCrank): Promise<void> {
  const now = Math.floor(Date.now() / 1000);

  if (mc.pullFeed && due(now, mc.lastSb, mc.intervals.sb)) {
    try { await crankSb(program, me, wallet, mc); mc.lastSb = now; }
    catch (e: any) { log(`  ✗ ${mc.tag} sb: ${errLine(e)}`); } // retried next tick
  }

  if (due(now, mc.lastSample, mc.intervals.sample)) {
    try {
      const sig = await program.methods.sampleTwap()
        .accounts({ cranker: me, collateralMint: mc.coll, marketOracle: mc.marketOracle, dexTwap: mc.dexTwap, clmmPool: mc.clmmPool }).rpc();
      log(`  ✓ ${mc.tag} sample_twap (${sig.slice(0, 16)}…)`);
      mc.lastSample = now;
    } catch (e: any) {
      // A too-soon sample (restart mid-window) is benign — back off a full interval either way.
      mc.lastSample = now;
      log(`  · ${mc.tag} sample_twap skipped: ${errLine(e)}`);
    }
  }

  if (due(now, mc.lastPrice, mc.intervals.price)) {
    try {
      const m: any = await program.account.market.fetch(mc.market);
      if (m.shutdown) return log(`  · ${mc.tag} market shut down — not cranking price`);
      const sig = await program.methods.updatePrice().accounts({
        cranker: me, collateralMint: mc.coll, market: mc.market, marketOracle: mc.marketOracle,
        pythPriceUpdate: mc.pythPriceUpdate, switchboardFeed: mc.sbFeed, dexTwap: mc.dexTwap,
        solUsdPythUpdate: mc.solUsdPythUpdate, lstStakePool: mc.lstStakePool,
      }).rpc();
      mc.lastPrice = now;
      log(`  ✓ ${mc.tag} update_price (${sig.slice(0, 16)}…)`);
    } catch (e: any) {
      log(`  ✗ ${mc.tag} update_price: ${errLine(e)}`); // retried next tick
    }
  }
}

async function main() {
  const cfgPath = process.argv[2];
  const cfg: CrankCfg = cfgPath ? JSON.parse(fs.readFileSync(cfgPath, "utf8")) : DEFAULT_CFG;
  validateConfig(cfg);
  const { program, pid, me, url } = makeProgram();
  const wallet = (program.provider as anchor.AnchorProvider).wallet as anchor.Wallet;
  log(`oracle-crank up — program ${pid.toBase58()}, wallet ${me.toBase58()}, RPC ${url}`);

  const needSb = true; // queue is cheap to load and most markets have an SB feed
  const queue = needSb ? await getDefaultQueue(url) : null;
  const markets: MarketCrank[] = [];
  for (const mint of cfg.markets) markets.push(await loadMarket(program, queue, pid, new PublicKey(mint), cfg));

  const tickSecs = cfg.tickSecs ?? 15;
  const run = async () => { for (const mc of markets) { try { await tick(program, me, wallet, mc); } catch (e: any) { log(`✗ ${mc.tag}: ${errLine(e)}`); } } };
  await run();
  setInterval(run, tickSecs * 1000);
}

if (require.main === module) {
  main().catch((e) => { console.error(e); process.exit(1); });
}
