/**
 * fUSD monitoring dashboard — a read-only process that polls on-chain state for the configured
 * markets and serves (a) a live HTML dashboard and (b) a /metrics.json feed for alerting pipelines.
 *
 * It needs NO private key (a throwaway wallet is used purely to satisfy Anchor's provider) and sends
 * NO transactions. Everything is derived from account reads + token balances, so it sidesteps both
 * getProgramAccounts (which surfpool / many RPCs don't serve) and the emit_cpi event-decode path —
 * the protocol's own cumulative counters (Market aggregates, buffer/backstop total_funded/absorbed)
 * already expose the flow metrics.
 *
 * THE SOLVENCY CHECK is the hero: circulating fUSD == Σ(agg_recorded_debt − unminted_interest +
 * bad_debt) across markets (the global supply invariant). When every market is in the config this is
 * exact; with a partial config the backing sum is a lower bound, so we only alarm if backing EXCEEDS
 * circulating (debt with no matching supply — never legitimate). The reads are not slot-atomic, so a
 * mint/burn landing mid-collection would fake a violation: the supply is read again after the market
 * sweep, and a tick whose supply moved is flagged torn and skipped (checked again next poll).
 *
 * PEG: fUSD has no on-chain USD oracle (its peg IS the redemption floor), so fUSD/$1 deviation is not
 * computable here — wire an off-chain price source into your alerting if you need it. What IS shown is
 * the collateral oracle's health (spot/debt_spot freshness + the mint-freeze / divergence flags).
 *
 * USAGE
 *   ANCHOR_PROVIDER_URL=<rpc> npx ts-node keepers/monitor.ts [config.json]
 *   then open http://127.0.0.1:8787
 *   No config arg → the built-in WSOL default (port 8787, 15s poll).
 */
import * as anchor from "@coral-xyz/anchor";
import * as fs from "fs";
import * as http from "http";
import {
  PublicKey, Pk, Keypair, pda, seed, bundle, makeProgram, bi,
  RAY, BPS, FUSD_DECIMALS, MAX_PRICE_STALENESS_SLOTS, SHUTDOWN_ORACLE_STALENESS_SLOTS, log, errLine, redactUrl,
} from "./common";

const STALE_SLOTS = Number(MAX_PRICE_STALENESS_SLOTS); // 250
// Past HALF the shutdown fuse the stale-price warn ESCALATES to critical (pages the webhook):
// at SHUTDOWN_ORACLE_STALENESS_SLOTS (~1h) anyone can trip the irreversible market shutdown —
// a drained keeper wallet burned 46 of those 60 minutes as a mere warn on 2026-07-06.
const FUSE_SLOTS = Number(SHUTDOWN_ORACLE_STALENESS_SLOTS); // 9000
const FUSE_ESCALATE_SLOTS = FUSE_SLOTS / 2;
const SUPPLY_EPSILON_USD = 0.01; // rounding tolerance for the supply identity
// On-chain tcr_breach (shutdown.rs / cdp::tcr_below) has NO dust floor: 1 native unit of interest
// dust with zero collateral and a fresh price IS permissionlessly, irreversibly shutdown-eligible.
// So the SCR critical must fire on dust too (it mirrors the chain exactly) — the floor below only
// softens the CCR-band WARN (reversible borrow-restriction) and tags the dust case in the message.
// Disarm the dust state operationally: deposit any collateral (value>0 defeats tcr_below) before
// cranking the price fresh, or repay the dust.
export const TCR_DUST_DEBT = 1_000_000n; // fUSD-native (6dp) = $1

interface MonitorCfg {
  port?: number;
  host?: string;
  pollIntervalSecs?: number;
  markets: string[]; // collateral mints
  bufferTargetBps?: number; // optional: per-market buffer target = bps of that market's agg_recorded_debt
  debtCeilingWarnPct?: number; // default 90
  /** Ops wallets to balance-watch (e.g. the crank fee payer): CRITICAL below minSol. */
  watchWallets?: { label: string; pubkey: string; minSol: number }[];
}
const DEFAULT_CFG: MonitorCfg = {
  port: 8787, host: "127.0.0.1", pollIntervalSecs: 15,
  markets: ["So11111111111111111111111111111111111111112"], // WSOL
  debtCeilingWarnPct: 90,
};

export function validateConfig(cfg: MonitorCfg): void {
  const bail = (m: string): never => { throw new Error(`config: ${m}`); };
  const posNum = (v: any, name: string) => {
    if (v !== undefined && (typeof v !== "number" || !Number.isFinite(v) || v <= 0)) bail(`${name} must be a positive number (got ${v})`);
  };
  posNum(cfg.port, "port"); posNum(cfg.pollIntervalSecs, "pollIntervalSecs");
  if (cfg.bufferTargetBps !== undefined && (typeof cfg.bufferTargetBps !== "number" || cfg.bufferTargetBps < 0)) bail("bufferTargetBps must be a non-negative number");
  if (cfg.debtCeilingWarnPct !== undefined && (typeof cfg.debtCeilingWarnPct !== "number" || cfg.debtCeilingWarnPct <= 0 || cfg.debtCeilingWarnPct > 100)) bail("debtCeilingWarnPct must be in (0, 100]");
  if (!Array.isArray(cfg.markets) || cfg.markets.length === 0) bail("markets must be a non-empty array");
  cfg.markets.forEach((m, i) => { try { new PublicKey(m); } catch { bail(`markets[${i}] is not a valid pubkey: ${m}`); } });
  (cfg.watchWallets ?? []).forEach((w, i) => {
    if (!w.label) bail(`watchWallets[${i}].label required`);
    try { new PublicKey(w.pubkey); } catch { bail(`watchWallets[${i}].pubkey invalid: ${w.pubkey}`); }
    if (typeof w.minSol !== "number" || !(w.minSol >= 0)) bail(`watchWallets[${i}].minSol must be >= 0`);
  });
}

// ── pure helpers (unit-tested in monitor.spec.ts) ──────────────────────────────────────────────
const usd = (native: bigint): number => Number(native) / 10 ** FUSD_DECIMALS;

/** Debt-weighted average borrow rate (bps), 2-dp. `agg_weighted_debt_sum / agg_recorded_debt`. */
export function avgRateBps(weightedSum: bigint, aggDebt: bigint): number {
  return aggDebt === 0n ? 0 : Number((weightedSum * 100n) / aggDebt) / 100;
}
/** `part/whole` as a percent (2-dp); 0 when whole is 0. */
export function pct(part: bigint, whole: bigint): number {
  return whole === 0n ? 0 : Number((part * 10000n) / whole) / 100;
}
/** Market collateral ratio (bps): collateral value / agg debt; null when there is no debt. */
export function tcrBps(totalCollateral: bigint, spot: bigint, aggDebt: bigint): number | null {
  if (aggDebt === 0n) return null;
  const value = (totalCollateral * spot) / RAY; // fUSD-native
  return Number((value * BPS) / aggDebt);
}

/**
 * Global-backstop solvency. The on-chain invariant is `vault ≥ total_contributed − total_absorbed −
 * total_withdrawn`, NOT equality: the vault is a plain SPL account, so anyone can donate fUSD straight
 * in (bypassing `fund_backstop`, the only inflow that bumps `total_contributed`), and
 * `withdraw_backstop_excess` reads the LIVE balance and lets gov recover any surplus above `reserve_cap`
 * without reconciling donations into the counters — so the balance can only drift UP. Only a genuine
 * shortfall (`bal < counters`, an unaccounted outflow) is the real break. See
 * programs/fusd-core/src/instructions/global_backstop.rs (fund / withdraw_excess) +
 * state/global_backstop.rs.
 */
export function backstopSolvent(contributed: bigint, absorbed: bigint, withdrawn: bigint, bal: bigint): boolean {
  return bal >= contributed - absorbed - withdrawn;
}

export type Severity = "critical" | "warn" | "info";
export interface Alert { severity: Severity; scope: string; message: string; }

export interface MarketMetrics {
  mint: string; tag: string; exists: boolean; error?: string;
  shutdown: boolean; shutdownReason: number; mintFrozen: boolean;
  aggDebtUsd: number; unmintedUsd: number; badDebtUsd: number; avgRateBps: number;
  debtCeilingUsd: number; ceilingUsedPct: number;
  tvlUsd: number; tcrBps: number | null; mcrBps: number; scrBps: number; ccrBps: number;
  spotUsd: number; debtSpotUsd: number; slotsSincePrice: number; collDecimals: number;
  liqGraceActive: boolean; liqDivergenceActive: boolean; guardianPauseActive: boolean;
  bufferUsd: number; bufferTargetUsd: number | null;
  bufferFundedUsd: number; bufferAbsorbedUsd: number;
  rpDepositsUsd: number; rpSeizedColl: number; rlCapUsd: number; rlUsedPct: number;
  protocolCollateral: number; globalContributedUsd: number; globalDrawnUsd: number;
}
export interface Metrics {
  ts: number; slot: number; rpc: string;
  global: {
    fusdSupplyUsd: number; sumBackingUsd: number; supplyDeltaUsd: number;
    supplySnapshotTorn: boolean; // supply moved during collection — identity check unreliable this tick
    govAuthority: string; guardian: string; pendingGovAuthority: string | null;
    backstop: { balanceUsd: number; cutBps: number; reserveCapUsd: number; contributedUsd: number; absorbedUsd: number; withdrawnUsd: number; solvencyOk: boolean } | null;
  };
  markets: MarketMetrics[];
  wallets: { label: string; pubkey: string; sol: number; minSol: number }[];
  thresholds: { staleSlots: number; ceilingWarnPct: number };
  alerts: Alert[];
}

/** Derive alerts from a metrics snapshot — the operational logic, kept pure for testing. */
export function computeAlerts(m: Metrics): Alert[] {
  const a: Alert[] = [];
  const push = (severity: Severity, scope: string, message: string) => a.push({ severity, scope, message });
  const f = (n: number) => n.toLocaleString("en-US", { maximumFractionDigits: 2 });

  if (m.global.sumBackingUsd - m.global.fusdSupplyUsd > SUPPLY_EPSILON_USD) {
    // A mint/burn that landed mid-collection makes the two sides incomparable — don't cry wolf on
    // the one alert that matters; a REAL violation persists and fires on the next (untorn) tick.
    if (m.global.supplySnapshotTorn)
      push("info", "global", "supply moved during collection — identity check skipped this tick");
    else
      push("critical", "global", `supply identity broken: backing $${f(m.global.sumBackingUsd)} exceeds circulating $${f(m.global.fusdSupplyUsd)} (Δ $${f(m.global.supplyDeltaUsd * -1)})`);
  }
  if (m.global.backstop && !m.global.backstop.solvencyOk)
    push("critical", "global", "global backstop shortfall: vault < contributed − absorbed − withdrawn (unaccounted outflow)");

  for (const mk of m.markets) {
    const s = mk.tag;
    if (!mk.exists) { push("warn", s, `market not readable: ${mk.error ?? "missing"}`); continue; }
    const tcrJudgeable = mk.aggDebtUsd >= usd(TCR_DUST_DEBT); // see TCR_DUST_DEBT
    if (mk.shutdown) push("critical", s, `market SHUT DOWN (reason ${mk.shutdownReason})`);
    if (mk.badDebtUsd > 0) push("critical", s, `un-homed bad debt $${f(mk.badDebtUsd)}`);
    // No dust gate here: on-chain tcr_breach has none, so this mirrors real shutdown eligibility.
    if (mk.scrBps > 0 && mk.tcrBps !== null && mk.tcrBps < mk.scrBps && !mk.shutdown)
      push("critical", s, `TCR ${mk.tcrBps}bps below SCR ${mk.scrBps}bps — shutdown-eligible${
        tcrJudgeable ? "" : " (interest dust — disarm: deposit any collateral or repay the dust)"}`);
    if (mk.mintFrozen) push("warn", s, "mint frozen (oracle degraded) — borrowing blocked");
    if (mk.slotsSincePrice > FUSE_ESCALATE_SLOTS && !mk.shutdown)
      push("critical", s, `price stale ${mk.slotsSincePrice} slots — SHUTDOWN FUSE ${Math.round((mk.slotsSincePrice / FUSE_SLOTS) * 100)}% (permissionless irreversible shutdown at ${FUSE_SLOTS}); fix the crank NOW`);
    else if (mk.slotsSincePrice > m.thresholds.staleSlots) push("warn", s, `price stale: ${mk.slotsSincePrice} slots since update (> ${m.thresholds.staleSlots})`);
    if (mk.liqDivergenceActive) push("warn", s, "liquidation paused — oracle divergence");
    if (mk.liqGraceActive) push("warn", s, "liquidation grace window active (post-staleness resume)");
    if (mk.guardianPauseActive) push("warn", s, "guardian pause active — new borrowing blocked");
    if (mk.bufferTargetUsd !== null && mk.bufferUsd < mk.bufferTargetUsd) push("warn", s, `insurance buffer $${f(mk.bufferUsd)} below target $${f(mk.bufferTargetUsd)}`);
    if (mk.debtCeilingUsd === 0 && mk.aggDebtUsd > 0) push("warn", s, "debt ceiling 0 — new debt paused with debt outstanding");
    else if (mk.ceilingUsedPct >= m.thresholds.ceilingWarnPct) push("warn", s, `debt ceiling ${mk.ceilingUsedPct.toFixed(1)}% used`);
    if (tcrJudgeable && mk.ccrBps > 0 && mk.tcrBps !== null && mk.tcrBps < mk.ccrBps && (mk.scrBps === 0 || mk.tcrBps >= mk.scrBps))
      push("warn", s, `TCR ${mk.tcrBps}bps below CCR band ${mk.ccrBps}bps — borrow/withdraw restricted`);
    if (mk.rlUsedPct >= 90) push("info", s, `net-outflow rate limiter ${mk.rlUsedPct.toFixed(0)}% utilized`);
  }
  for (const w of m.wallets) {
    if (w.sol < w.minSol)
      push("critical", "wallet", `${w.label} wallet ${w.pubkey.slice(0, 6)}… at ${w.sol.toFixed(4)} SOL (< ${w.minSol}) — cranks fail when it hits 0; top up`);
  }
  return a;
}

// ── on-chain collection ────────────────────────────────────────────────────────────────────────
async function collect(program: any, conn: any, pid: Pk, cfg: MonitorCfg): Promise<Metrics> {
  const tokenBal = async (acc: Pk): Promise<bigint> => {
    try { return BigInt((await conn.getTokenAccountBalance(acc)).value.amount); } catch { return 0n; }
  };
  const cfgPda = pda([seed("config")], pid);
  const [slot, pc] = await Promise.all([conn.getSlot(), program.account.protocolConfig.fetch(cfgPda)]) as [number, any];
  const fusdSupply = BigInt((await conn.getTokenSupply(pc.fusdMint)).value.amount);

  const readBackstop = async (): Promise<Metrics["global"]["backstop"]> => {
    try {
      const b: any = await program.account.globalBackstopReserve.fetch(pda([seed("backstop")], pid));
      const bal = await tokenBal(b.fusdVault);
      const contributed = bi(b.totalContributed), absorbed = bi(b.totalAbsorbed), withdrawn = bi(b.totalWithdrawn);
      return {
        balanceUsd: usd(bal), cutBps: Number(b.cutBps), reserveCapUsd: usd(bi(b.reserveCap)),
        contributedUsd: usd(contributed), absorbedUsd: usd(absorbed), withdrawnUsd: usd(withdrawn),
        solvencyOk: backstopSolvent(contributed, absorbed, withdrawn, bal),
      };
    } catch { return null; } // backstop not initialized
  };

  const readMarket = async (mint: string): Promise<{ metric: MarketMetrics; backing: bigint }> => {
    const coll = new PublicKey(mint);
    const tag = mint.slice(0, 6);
    const p = bundle(pid, coll);
    try {
      const m: any = await program.account.market.fetch(p.market);
      const aggDebt = bi(m.aggRecordedDebt), unminted = bi(m.unmintedInterest), badDebt = bi(m.badDebt);
      const spot = bi(m.spot), debtSpot = bi(m.debtSpot), totalColl = bi(m.totalCollateral);
      const dec = Number(m.collateralDecimals);
      const debtCeiling = bi(m.debtCeiling);
      const [bufferBal, rpFusd, rpColl, buf] = await Promise.all([
        tokenBal(p.bufferFusdVault), tokenBal(p.reactorFusdVault), tokenBal(p.reactorCollVault),
        program.account.insuranceBuffer.fetch(p.buffer).catch(() => null),
      ]);
      const tvlUsd = Number((totalColl * spot) / RAY) / 10 ** FUSD_DECIMALS;
      const bufferTargetUsd = cfg.bufferTargetBps !== undefined ? usd((aggDebt * BigInt(Math.round(cfg.bufferTargetBps))) / BPS) : null;
      const rlCap = bi(m.rlCap), rlAccrued = bi(m.rlAccrued);
      const metric: MarketMetrics = {
        mint, tag, exists: true,
        shutdown: !!m.shutdown, shutdownReason: Number(m.shutdownReason), mintFrozen: !!m.mintFrozen,
        aggDebtUsd: usd(aggDebt), unmintedUsd: usd(unminted), badDebtUsd: usd(badDebt),
        avgRateBps: avgRateBps(bi(m.aggWeightedDebtSum), aggDebt),
        // ceiling 0 = new debt paused: with debt outstanding that is a FULLY-used ceiling, not 0%.
        debtCeilingUsd: usd(debtCeiling), ceilingUsedPct: debtCeiling === 0n ? (aggDebt > 0n ? 100 : 0) : pct(aggDebt, debtCeiling),
        tvlUsd, tcrBps: tcrBps(totalColl, spot, aggDebt),
        mcrBps: Number(m.mcrBps), scrBps: Number(m.scrBps), ccrBps: Number(m.ccrBps),
        spotUsd: spotToUsd(spot, dec), debtSpotUsd: spotToUsd(debtSpot, dec),
        slotsSincePrice: spot === 0n ? Number.MAX_SAFE_INTEGER : Math.max(0, slot - Number(m.spotUpdatedSlot)),
        collDecimals: dec,
        liqGraceActive: slot < Number(m.liqGraceUntil), liqDivergenceActive: slot < Number(m.liqDivergenceUntil),
        guardianPauseActive: Math.floor(Date.now() / 1000) < Number(m.guardianPausedUntil),
        bufferUsd: usd(bufferBal), bufferTargetUsd,
        bufferFundedUsd: buf ? usd(bi(buf.totalFunded)) : 0, bufferAbsorbedUsd: buf ? usd(bi(buf.totalAbsorbed)) : 0,
        rpDepositsUsd: usd(rpFusd), rpSeizedColl: Number(rpColl) / 10 ** dec,
        rlCapUsd: usd(rlCap), rlUsedPct: pct(rlAccrued, rlCap),
        protocolCollateral: Number(bi(m.protocolCollateral)) / 10 ** dec,
        globalContributedUsd: usd(bi(m.globalContributed)), globalDrawnUsd: usd(bi(m.globalDrawn)),
      };
      return { metric, backing: aggDebt - unminted + badDebt };
    } catch (e: any) {
      return { metric: { mint, tag, exists: false, error: errLine(e) } as any, backing: 0n };
    }
  };

  // The reads within/across markets are independent — one parallel wave instead of ~6×N round trips
  // (also shrinks the supply-identity torn-snapshot window below).
  const readWallet = async (w: { label: string; pubkey: string; minSol: number }) => ({
    label: w.label, pubkey: w.pubkey, minSol: w.minSol,
    sol: (await conn.getBalance(new PublicKey(w.pubkey)).catch(() => 0)) / 1e9,
  });
  const [backstop, marketReads, wallets] = await Promise.all([
    readBackstop(),
    Promise.all(cfg.markets.map(readMarket)),
    Promise.all((cfg.watchWallets ?? []).map(readWallet)),
  ]);
  const markets = marketReads.map((r) => r.metric);
  const sumBacking = marketReads.reduce((acc, r) => acc + r.backing, 0n);

  // Re-read the supply: if it moved during the market sweep, a mint/burn landed mid-collection and
  // the identity comparison is torn (any agg_debt change WITHOUT a mint/burn preserves the identity,
  // so supply-stable ⇒ the snapshot is comparable even though the reads span slots).
  const fusdSupplyAfter = BigInt((await conn.getTokenSupply(pc.fusdMint)).value.amount);

  const m: Metrics = {
    // origin only — provider urls carry the API key in the query string
    ts: Date.now(), slot, rpc: redactUrl((program.provider.connection as any)._rpcEndpoint ?? ""),
    global: {
      fusdSupplyUsd: usd(fusdSupply), sumBackingUsd: usd(sumBacking), supplyDeltaUsd: usd(fusdSupply - sumBacking),
      supplySnapshotTorn: fusdSupplyAfter !== fusdSupply,
      govAuthority: pc.govAuthority.toBase58(), guardian: pc.guardian.toBase58(),
      pendingGovAuthority: pc.pendingGovAuthority?.equals?.(PublicKey.default) ? null : pc.pendingGovAuthority?.toBase58?.() ?? null,
      backstop,
    },
    markets,
    wallets,
    thresholds: { staleSlots: STALE_SLOTS, ceilingWarnPct: cfg.debtCeilingWarnPct ?? 90 },
    alerts: [],
  };
  m.alerts = computeAlerts(m);
  return m;
}

/** RAY-scaled fUSD-native-per-native-collateral spot → USD per whole collateral token. */
function spotToUsd(spot: bigint, collDecimals: number): number {
  if (spot === 0n) return 0;
  // spot = usd * 10^FUSD_DEC * RAY / 10^collDec  ⇒  usd = spot * 10^collDec / (RAY * 10^FUSD_DEC)
  const scale = (RAY * 10n ** BigInt(FUSD_DECIMALS)) / 10n ** BigInt(collDecimals);
  return Number((spot * 1000n) / scale) / 1000;
}

// ── HTTP server ──────────────────────────────────────────────────────────────────────────────
async function main() {
  const cfgPath = process.argv[2] || process.env.MONITOR_CONFIG; // env: config without touching the systemd unit
  const cfg: MonitorCfg = cfgPath ? { ...DEFAULT_CFG, ...JSON.parse(fs.readFileSync(cfgPath, "utf8")) } : DEFAULT_CFG;
  validateConfig(cfg);
  // Read-only: a throwaway wallet satisfies Anchor's provider without needing a key file.
  const { program, provider, pid, url } = makeProgram(new anchor.Wallet(Keypair.generate()));
  const conn = provider.connection;
  log(`monitor — program ${pid.toBase58()}, RPC ${redactUrl(url)}, markets ${cfg.markets.map((s) => s.slice(0, 6)).join(", ")}`);

  let latest: Metrics | null = null;
  let lastError: string | null = null;
  let lastGoodAt = Date.now(); // start of grace window; bumped on every successful collect
  const refresh = async () => {
    try { latest = await collect(program, conn, pid, cfg); lastError = null; lastGoodAt = Date.now(); }
    catch (e: any) { lastError = errLine(e); log(`✗ refresh: ${lastError}`); }
  };
  await refresh();
  const pollMs = (cfg.pollIntervalSecs ?? 15) * 1000;
  setInterval(refresh, pollMs);

  const page = fs.readFileSync(`${__dirname}/monitor.html`, "utf8")
    .replace("__POLL_MS__", String((cfg.pollIntervalSecs ?? 15) * 1000));
  const server = http.createServer((req, res) => {
    const path = (req.url || "/").split("?")[0];
    if (path === "/" || path === "/index.html") { res.writeHead(200, { "content-type": "text/html; charset=utf-8" }); res.end(page); return; }
    if (path === "/healthz") {
      // A blind-but-up monitor must page: 503 once no snapshot has landed for 3 poll intervals
      // (persistent RPC failure), so the webhook's liveness branch fires instead of staying green.
      const fresh = Date.now() - lastGoodAt <= 3 * pollMs;
      res.writeHead(fresh ? 200 : 503, { "content-type": "text/plain" });
      res.end(fresh ? (latest ? "ok" : "starting") : `stale: ${lastError ?? "no snapshot"}`);
      return;
    }
    if (path === "/metrics.json") {
      if (!latest) { res.writeHead(503, { "content-type": "application/json" }); res.end(JSON.stringify({ error: lastError || "starting" })); return; }
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ ...latest, lastError }));
      return;
    }
    res.writeHead(404, { "content-type": "text/plain" }); res.end("not found");
  });
  server.listen(cfg.port ?? 8787, cfg.host ?? "127.0.0.1", () =>
    log(`dashboard → http://${cfg.host ?? "127.0.0.1"}:${cfg.port ?? 8787}  (poll ${cfg.pollIntervalSecs ?? 15}s)`));
}

if (require.main === module) {
  main().catch((e) => { console.error(e); process.exit(1); });
}
