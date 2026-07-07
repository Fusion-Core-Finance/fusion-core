/**
 * fUSD oracle crank — the permissionless keeper that keeps a market PRICED and MINTABLE. The oracle
 * aggregate's Ok mode fail-closes unless ALL THREE legs are fresh (fusd-oracle::aggregate):
 *   1. Switchboard: on-demand feeds only update when a consumer cranks them — nobody else cranks
 *      ours (the mainnet dry run found the SOL/USD feed 566s stale, mints frozen), so this keeper
 *      pulls a signed update every sbIntervalSecs (default 2/3 of the oracle's max_age_secs).
 *   2. DEX TWAP: sample_twap every sampleIntervalSecs (default sits well inside
 *      twap_max_staleness_secs while still spanning the window with ≥ min_samples) so
 *      update_price's TWAP corridor stays live — a stale ring makes twap() return None, which
 *      drops the aggregate out of Ok mode and freezes mints.
 *   3. update_price itself: recommits Market.spot every priceIntervalSecs (default 60s) so borrow's
 *      MAX_PRICE_STALENESS_SLOTS (250-slot ≈ 100s) gate stays open.
 * Plus the interest leg: refresh_market every refreshIntervalSecs (default 300s) folds the aggregate
 * interest accumulator into the insurance buffer (+ the global backstop cut when the reserve exists)
 * and pays the cranker its keeper_reward_bps cut to the wallet's fUSD ATA (auto-created at startup).
 *
 * Everything per-market is read from the on-chain MarketOracle — the Switchboard feed, the CLMM
 * pool, the LST canonical leg, and the Pyth account (derived: the push-oracle sponsored feed is the
 * PDA [shard_u16_le, feed_id]). Config is just the market list + optional interval overrides.
 *
 * COST: base+priority fees ≈ 0.035 SOL/market/day at defaults — but the SB leg DOMINATES: the
 * payer funds each signing oracle (~0.0009 SOL/sig/update measured 2026-07-06). At the defaults
 * (1 signature, 90%-of-max_age cadence ≈ 320 updates/day) that is ≈ 0.3 SOL/day/market; the
 * original 3-sig/200s config burned ≈ 1.2 SOL/day and drained the keeper wallet in a day.
 * Knobs: sbNumSignatures, sbIntervalSecs (hard ceiling: the oracle max_age_secs, 300).
 *
 * USAGE
 *   ANCHOR_PROVIDER_URL=<rpc> ANCHOR_WALLET=~/.config/solana/id.json \
 *     npx ts-node keepers/oracle-crank.ts [config.json]
 *   No config arg → the built-in WSOL default below.
 */
import * as anchor from "@coral-xyz/anchor";
import * as fs from "fs";
import { getDefaultQueue, PullFeed } from "@switchboard-xyz/on-demand";
import { PublicKey, Pk, pda, seed, bundle, log, makeProgram, ensureAta, TOKEN_PROGRAM, errLine, priorityIxs, priorityFeeMicroLamports , redactUrl, nonReentrant } from "./common";

const PYTH_PUSH_ORACLE = new PublicKey("pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT");
const SOL_USD_FEED_ID = Buffer.from("ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d", "hex");
const ZERO = PublicKey.default;
// The on-chain DexTwap observation-ring capacity (programs/fusd-core/src/constants.rs:382). The
// sample_twap anti-flood floor (sample_twap.rs) is ceil(twap_window_secs / (TWAP_RING_CAPACITY-1)).
const TWAP_RING_CAPACITY = 64;

// Priority fee on EVERY send path (anchor .rpc()s and the SB v0 tx) via common's priorityIxs: congestion
// is exactly when the crank must land (volatility ⇒ fee spikes), and a zero-fee tx is dropped first.
// update_price parses Pyth + Switchboard + the TWAP ring (+ the LST leg), and the SB update carries
// oracle signature verification — both can brush the 200k-CU default, so they get explicit headroom.
// The other legs (sample_twap, refresh_market) fit the default comfortably.
const CU_LIMIT_UPDATE_PRICE = 400_000;
const CU_LIMIT_SB_UPDATE = 400_000;

interface CrankCfg {
  markets: string[]; // collateral mints
  tickSecs?: number; // dueness check cadence (default 15)
  priceIntervalSecs?: number; // update_price cadence (default 60)
  sbIntervalSecs?: number; // Switchboard update cadence (default derived from max_age_secs)
  sbNumSignatures?: number; // oracle signatures per SB update (default 1 — EACH signing oracle is PAID per update)
  sampleIntervalSecs?: number; // sample_twap cadence (default from twap_max_staleness_secs + window/min_samples)
  refreshIntervalSecs?: number; // refresh_market cadence (default 300)
  pythShard?: number; // sponsored-feed shard (default 0)
}
const DEFAULT_CFG: CrankCfg = { markets: ["So11111111111111111111111111111111111111112"] };

export function validateConfig(cfg: CrankCfg): void {
  const bail = (m: string): never => { throw new Error(`config: ${m}`); };
  if (!Array.isArray(cfg.markets) || cfg.markets.length === 0) bail("markets must be a non-empty array");
  cfg.markets.forEach((m, i) => { try { new PublicKey(m); } catch { bail(`markets[${i}] is not a valid pubkey: ${m}`); } });
  for (const k of ["tickSecs", "priceIntervalSecs", "sbIntervalSecs", "sampleIntervalSecs", "refreshIntervalSecs"] as const) {
    const v = cfg[k];
    if (v !== undefined && (typeof v !== "number" || !Number.isFinite(v) || v <= 0)) bail(`${k} must be a positive number (got ${v})`);
  }
  if (cfg.sbNumSignatures !== undefined && (!Number.isInteger(cfg.sbNumSignatures) || cfg.sbNumSignatures < 1 || cfg.sbNumSignatures > 8))
    bail(`sbNumSignatures must be an integer in 1..8 (got ${cfg.sbNumSignatures})`);
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
export function intervalsFrom(
  o: { maxAgeSecs: number; twapWindowSecs: number; twapMinSamples: number; twapMaxStalenessSecs: number },
  cfg: CrankCfg,
) {
  // The BINDING sample constraint is twap()'s staleness gate (fusd-oracle::twap): once
  // now-newest > twap_max_staleness_secs the corridor returns None, dropping the aggregate out of
  // Ok mode and freezing mints (update_price.rs). So sample well INSIDE max_staleness (2/3), while
  // also (a) keeping ≥ min_samples spanning the window (≤ ceil(window/(min_samples−1))) and
  // (b) staying at/above sample_twap's on-chain anti-flood floor ceil(window/(TWAP_RING_CAPACITY−1))
  // (sample_twap.rs) — sampling faster than that both bounces the tx (TwapSampleRejected) and shrinks
  // a full ring below the window. SB likewise lands well inside max_age or Ok mode drops.
  const antiFloodFloor = Math.ceil(o.twapWindowSecs / (TWAP_RING_CAPACITY - 1));
  const staleBound = Math.floor((o.twapMaxStalenessSecs * 2) / 3);
  const minSampleBound = Math.ceil(o.twapWindowSecs / Math.max(1, o.twapMinSamples - 1));
  const sample = Math.max(antiFloodFloor, Math.min(staleBound, minSampleBound));
  return {
    sample: cfg.sampleIntervalSecs ?? sample,
    sb: cfg.sbIntervalSecs ?? Math.max(30, Math.floor((o.maxAgeSecs * 9) / 10) - 10),
    price: cfg.priceIntervalSecs ?? 60,
    refresh: cfg.refreshIntervalSecs ?? 300,
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
  fusdMint: Pk; mintAuthority: Pk; buffer: Pk; bufferFusdVault: Pk; crankerFusdAta: Pk;
  intervals: { sample: number; sb: number; price: number; refresh: number };
  lastSample: number; lastSb: number; lastPrice: number; lastRefresh: number;
}

async function loadMarket(program: any, queue: any, pid: Pk, coll: Pk, cfg: CrankCfg, crankerFusdAta: Pk): Promise<MarketCrank> {
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
    {
      maxAgeSecs: Number(o.maxAgeSecs),
      twapWindowSecs: Number(o.twapWindowSecs),
      twapMinSamples: Number(o.twapMinSamples),
      twapMaxStalenessSecs: Number(o.twapMaxStalenessSecs),
    },
    cfg,
  );
  log(`${tag}: pool ${clmmPool.toBase58().slice(0, 6)}…, sb ${sbFeed ? sbFeed.toBase58().slice(0, 6) + "…" : "none"}, pyth ${pythPriceUpdate.toBase58().slice(0, 6)}…${lst ? ", LST leg on" : ""} | intervals sample=${intervals.sample}s sb=${intervals.sb}s price=${intervals.price}s refresh=${intervals.refresh}s`);
  const b = bundle(pid, coll);
  return {
    coll, tag, market: b.market, marketOracle, dexTwap: pda([seed("twap"), coll], pid),
    clmmPool, sbFeed, pullFeed: sbFeed && queue ? new PullFeed(queue.program, sbFeed) : null,
    pythPriceUpdate, solUsdPythUpdate: lst ? pythFeedAccount(shard, SOL_USD_FEED_ID) : null,
    lstStakePool: lst ? o.lstStakePool : null,
    fusdMint: b.fusdMint, mintAuthority: b.mintAuthority, buffer: b.buffer, bufferFusdVault: b.bufferFusdVault, crankerFusdAta,
    intervals, lastSample: 0, lastSb: 0, lastPrice: 0, lastRefresh: 0,
  };
}

async function crankSb(program: any, me: Pk, wallet: anchor.Wallet, mc: MarketCrank, numSignatures: number): Promise<void> {
  // payer MUST be our wallet — omitted, the ix binds the SDK's dummy provider as a signer.
  // numSignatures default 1: every signing oracle is PAID by the payer per update (the burn that
  // drained the keeper wallet in a day at 3 sigs / 200s); the feed itself medians whatever arrives,
  // and the on-chain leg only checks freshness. Raise via sbNumSignatures if stronger quorum wanted.
  const [ixs, responses, , luts] = await mc.pullFeed.fetchUpdateIx({ numSignatures, payer: me });
  if (!ixs || ixs.length === 0)
    throw new Error(`no SB update ix (${JSON.stringify(responses?.map((r: any) => r?.error ?? "ok"))})`);
  // APPEND, never prepend: the SB update bundles a sig-verify precompile whose offset descriptors
  // reference instruction index 0 — prepending shifts it and EVERY SB tx fails. Compute-budget ixs
  // are position-independent (the runtime scans the whole message), so the tail is safe.
  ixs.push(...priorityIxs(CU_LIMIT_SB_UPDATE));
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

async function tick(program: any, me: Pk, wallet: anchor.Wallet, mc: MarketCrank, sbNumSignatures: number): Promise<void> {
  const now = Math.floor(Date.now() / 1000);

  if (mc.pullFeed && due(now, mc.lastSb, mc.intervals.sb)) {
    try { await crankSb(program, me, wallet, mc, sbNumSignatures); mc.lastSb = now; }
    catch (e: any) { log(`  ✗ ${mc.tag} sb: ${errLine(e)}`); } // retried next tick
  }

  if (due(now, mc.lastSample, mc.intervals.sample)) {
    try {
      const sig = await program.methods.sampleTwap()
        .accounts({ cranker: me, collateralMint: mc.coll, marketOracle: mc.marketOracle, dexTwap: mc.dexTwap, clmmPool: mc.clmmPool })
        .preInstructions(priorityIxs()).rpc();
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
      }).preInstructions(priorityIxs(CU_LIMIT_UPDATE_PRICE)).rpc();
      mc.lastPrice = now;
      log(`  ✓ ${mc.tag} update_price (${sig.slice(0, 16)}…)`);
    } catch (e: any) {
      log(`  ✗ ${mc.tag} update_price: ${errLine(e)}`); // retried next tick
    }
  }

  if (due(now, mc.lastRefresh, mc.intervals.refresh)) {
    try {
      const m: any = await program.account.market.fetch(mc.market);
      if (m.shutdown) return log(`  · ${mc.tag} market shut down — not refreshing`);
      // Backstop routing is optional on-chain: pass the reserve + its vault when it exists so the
      // cut_bps slice reaches it; absent, the whole post-keeper interest funds the local buffer.
      const backstop = pda([seed("backstop")], program.programId);
      const bs: any = await program.account.globalBackstopReserve.fetchNullable(backstop);
      const sig = await program.methods.refreshMarket().accounts({
        collateralMint: mc.coll, market: mc.market, fusdMint: mc.fusdMint, mintAuthority: mc.mintAuthority,
        insuranceBuffer: mc.buffer, bufferFusdVault: mc.bufferFusdVault, crankerFusdAta: mc.crankerFusdAta,
        backstop: bs ? backstop : null, backstopFusdVault: bs ? bs.fusdVault : null, tokenProgram: TOKEN_PROGRAM,
      }).preInstructions(priorityIxs()).rpc();
      mc.lastRefresh = now;
      log(`  ✓ ${mc.tag} refresh_market (${sig.slice(0, 16)}…)`);
    } catch (e: any) {
      log(`  ✗ ${mc.tag} refresh_market: ${errLine(e)}`); // retried next tick
    }
  }
}

async function main() {
  const cfgPath = process.argv[2];
  const cfg: CrankCfg = cfgPath ? JSON.parse(fs.readFileSync(cfgPath, "utf8")) : DEFAULT_CFG;
  validateConfig(cfg);
  const { program, provider, pid, me, url } = makeProgram();
  const wallet = (program.provider as anchor.AnchorProvider).wallet as anchor.Wallet;
  log(`oracle-crank up — program ${pid.toBase58()}, wallet ${me.toBase58()}, RPC ${redactUrl(url)}, priority ${priorityFeeMicroLamports()}µlam/CU`);

  // The refresh leg's keeper_reward_bps cut lands in the cranker's fUSD ATA — create it once up front.
  const crankerFusdAta = await ensureAta(provider, pda([seed("fusd_mint")], pid), me, priorityIxs());

  const needSb = true; // queue is cheap to load and most markets have an SB feed
  const queue = needSb ? await getDefaultQueue(url) : null;
  const markets: MarketCrank[] = [];
  for (const mint of cfg.markets) {
    try {
      markets.push(await loadMarket(program, queue, pid, new PublicKey(mint), cfg, crankerFusdAta));
    } catch (e: any) {
      // Per-market isolation: a misconfigured/not-yet-onboarded market (no CLMM pool, missing Pyth
      // sponsored feed) must NOT stop the healthy markets from being cranked — otherwise one bad entry
      // crash-loops the whole process under Restart=always, and every priced market ages past
      // MAX_PRICE_STALENESS then into permissionless irreversible shutdown (~1h). Skip + warn instead.
      log(`✗ skip market ${mint.slice(0, 6)}: ${errLine(e)} — other markets keep cranking`);
    }
  }
  if (markets.length === 0) throw new Error("no markets loaded — check config + oracle setup (see errors above)");

  const tickSecs = cfg.tickSecs ?? 15;
  const sbNumSignatures = cfg.sbNumSignatures ?? 1;
  // Skip a tick if the previous sweep is still running (slow RPC ⇒ overlapping cranks would double-submit).
  const run = nonReentrant(async () => { for (const mc of markets) { try { await tick(program, me, wallet, mc, sbNumSignatures); } catch (e: any) { log(`✗ ${mc.tag}: ${errLine(e)}`); } } });
  await run();
  setInterval(run, tickSecs * 1000);
}

if (require.main === module) {
  main().catch((e) => { console.error(e); process.exit(1); });
}
