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
 * circulating (debt with no matching supply — never legitimate).
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
import { PublicKey, Pk, Keypair, Connection, pda, seed, bundle, RAY, BPS, MAX_PRICE_STALENESS_SLOTS, log, errLine } from "./common";

const STALE_SLOTS = Number(MAX_PRICE_STALENESS_SLOTS); // 250
const SUPPLY_EPSILON_USD = 0.01; // rounding tolerance for the supply identity
const FUSD_DECIMALS = 6;

interface MonitorCfg {
  port?: number;
  host?: string;
  pollIntervalSecs?: number;
  markets: string[]; // collateral mints
  bufferTargetBps?: number; // optional: per-market buffer target = bps of that market's agg_recorded_debt
  debtCeilingWarnPct?: number; // default 90
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
}

// ── pure helpers (unit-tested in monitor.spec.ts) ──────────────────────────────────────────────
const bi = (v: any): bigint => BigInt(v.toString());
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
    govAuthority: string; guardian: string; pendingGovAuthority: string | null;
    backstop: { balanceUsd: number; cutBps: number; reserveCapUsd: number; contributedUsd: number; absorbedUsd: number; withdrawnUsd: number; solvencyOk: boolean } | null;
  };
  markets: MarketMetrics[];
  thresholds: { staleSlots: number; ceilingWarnPct: number };
  alerts: Alert[];
}

/** Derive alerts from a metrics snapshot — the operational logic, kept pure for testing. */
export function computeAlerts(m: Metrics): Alert[] {
  const a: Alert[] = [];
  const push = (severity: Severity, scope: string, message: string) => a.push({ severity, scope, message });
  const f = (n: number) => n.toLocaleString("en-US", { maximumFractionDigits: 2 });

  if (m.global.sumBackingUsd - m.global.fusdSupplyUsd > SUPPLY_EPSILON_USD)
    push("critical", "global", `supply identity broken: backing $${f(m.global.sumBackingUsd)} exceeds circulating $${f(m.global.fusdSupplyUsd)} (Δ $${f(m.global.supplyDeltaUsd * -1)})`);
  if (m.global.backstop && !m.global.backstop.solvencyOk)
    push("critical", "global", "global backstop solvency mismatch (vault ≠ contributed − absorbed − withdrawn)");

  for (const mk of m.markets) {
    const s = mk.tag;
    if (!mk.exists) { push("warn", s, `market not readable: ${mk.error ?? "missing"}`); continue; }
    if (mk.shutdown) push("critical", s, `market SHUT DOWN (reason ${mk.shutdownReason})`);
    if (mk.badDebtUsd > 0) push("critical", s, `un-homed bad debt $${f(mk.badDebtUsd)}`);
    if (mk.scrBps > 0 && mk.tcrBps !== null && mk.tcrBps < mk.scrBps && !mk.shutdown)
      push("critical", s, `TCR ${mk.tcrBps}bps below SCR ${mk.scrBps}bps — shutdown-eligible`);
    if (mk.mintFrozen) push("warn", s, "mint frozen (oracle degraded) — borrowing blocked");
    if (mk.slotsSincePrice > m.thresholds.staleSlots) push("warn", s, `price stale: ${mk.slotsSincePrice} slots since update (> ${m.thresholds.staleSlots})`);
    if (mk.liqDivergenceActive) push("warn", s, "liquidation paused — oracle divergence");
    if (mk.liqGraceActive) push("warn", s, "liquidation grace window active (post-staleness resume)");
    if (mk.guardianPauseActive) push("warn", s, "guardian pause active — new borrowing blocked");
    if (mk.bufferTargetUsd !== null && mk.bufferUsd < mk.bufferTargetUsd) push("warn", s, `insurance buffer $${f(mk.bufferUsd)} below target $${f(mk.bufferTargetUsd)}`);
    if (mk.ceilingUsedPct >= m.thresholds.ceilingWarnPct) push("warn", s, `debt ceiling ${mk.ceilingUsedPct.toFixed(1)}% used`);
    if (mk.ccrBps > 0 && mk.tcrBps !== null && mk.tcrBps < mk.ccrBps && (mk.scrBps === 0 || mk.tcrBps >= mk.scrBps))
      push("warn", s, `TCR ${mk.tcrBps}bps below CCR band ${mk.ccrBps}bps — borrow/withdraw restricted`);
    if (mk.rlUsedPct >= 90) push("info", s, `net-outflow rate limiter ${mk.rlUsedPct.toFixed(0)}% utilized`);
  }
  return a;
}

// ── on-chain collection ────────────────────────────────────────────────────────────────────────
function makeReadonlyProgram(): { program: any; conn: any; pid: Pk; url: string } {
  const url = process.env.ANCHOR_PROVIDER_URL || "http://127.0.0.1:8899";
  const conn = new Connection(url, "confirmed");
  const provider = new anchor.AnchorProvider(conn, new anchor.Wallet(Keypair.generate()), { commitment: "confirmed" });
  const idl = JSON.parse(fs.readFileSync(`${__dirname}/../target/idl/fusd_core.json`, "utf8"));
  const program: any = new anchor.Program(idl as anchor.Idl, provider);
  return { program, conn, pid: program.programId as Pk, url };
}

async function collect(program: any, conn: any, pid: Pk, cfg: MonitorCfg): Promise<Metrics> {
  const slot = await conn.getSlot();
  const tokenBal = async (acc: Pk): Promise<bigint> => {
    try { return BigInt((await conn.getTokenAccountBalance(acc)).value.amount); } catch { return 0n; }
  };
  const cfgPda = pda([seed("config")], pid);
  const pc: any = await program.account.protocolConfig.fetch(cfgPda);
  const fusdSupply = BigInt((await conn.getTokenSupply(pc.fusdMint)).value.amount);

  let backstop: Metrics["global"]["backstop"] = null;
  try {
    const b: any = await program.account.globalBackstopReserve.fetch(pda([seed("backstop")], pid));
    const bal = await tokenBal(b.fusdVault);
    const contributed = bi(b.totalContributed), absorbed = bi(b.totalAbsorbed), withdrawn = bi(b.totalWithdrawn);
    backstop = {
      balanceUsd: usd(bal), cutBps: Number(b.cutBps), reserveCapUsd: usd(bi(b.reserveCap)),
      contributedUsd: usd(contributed), absorbedUsd: usd(absorbed), withdrawnUsd: usd(withdrawn),
      solvencyOk: contributed - absorbed - withdrawn === bal,
    };
  } catch { /* backstop not initialized — leave null */ }

  const markets: MarketMetrics[] = [];
  let sumBacking = 0n;
  for (const mint of cfg.markets) {
    const coll = new PublicKey(mint);
    const tag = mint.slice(0, 6);
    const p = bundle(pid, coll);
    try {
      const m: any = await program.account.market.fetch(p.market);
      const aggDebt = bi(m.aggRecordedDebt), unminted = bi(m.unmintedInterest), badDebt = bi(m.badDebt);
      sumBacking += aggDebt - unminted + badDebt;
      const spot = bi(m.spot), debtSpot = bi(m.debtSpot), totalColl = bi(m.totalCollateral);
      const dec = Number(m.collateralDecimals);
      const debtCeiling = bi(m.debtCeiling);
      const vaultBal = await tokenBal(p.collateralVault);
      const bufferBal = await tokenBal(p.bufferFusdVault);
      const rpFusd = await tokenBal(p.reactorFusdVault);
      const rpColl = await tokenBal(p.reactorCollVault);
      let buf: any = null;
      try { buf = await program.account.insuranceBuffer.fetch(p.buffer); } catch { /* none */ }
      const tvlUsd = Number((totalColl * spot) / RAY) / 10 ** FUSD_DECIMALS;
      const bufferTargetUsd = cfg.bufferTargetBps !== undefined ? usd((aggDebt * BigInt(Math.round(cfg.bufferTargetBps))) / BPS) : null;
      const rlCap = bi(m.rlCap), rlAccrued = bi(m.rlAccrued);
      markets.push({
        mint, tag, exists: true,
        shutdown: !!m.shutdown, shutdownReason: Number(m.shutdownReason), mintFrozen: !!m.mintFrozen,
        aggDebtUsd: usd(aggDebt), unmintedUsd: usd(unminted), badDebtUsd: usd(badDebt),
        avgRateBps: avgRateBps(bi(m.aggWeightedDebtSum), aggDebt),
        debtCeilingUsd: usd(debtCeiling), ceilingUsedPct: pct(aggDebt, debtCeiling),
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
        // vaultBal kept for the proof-of-reserves note (vault should cover total_collateral + surpluses)
        ...( { vaultColl: Number(vaultBal) / 10 ** dec } as any ),
      });
    } catch (e: any) {
      markets.push({ mint, tag, exists: false, error: errLine(e) } as any);
    }
  }

  const m: Metrics = {
    ts: Date.now(), slot, rpc: (program.provider.connection as any)._rpcEndpoint ?? "",
    global: {
      fusdSupplyUsd: usd(fusdSupply), sumBackingUsd: usd(sumBacking), supplyDeltaUsd: usd(fusdSupply - sumBacking),
      govAuthority: pc.govAuthority.toBase58(), guardian: pc.guardian.toBase58(),
      pendingGovAuthority: pc.pendingGovAuthority?.equals?.(PublicKey.default) ? null : pc.pendingGovAuthority?.toBase58?.() ?? null,
      backstop,
    },
    markets,
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
  const cfgPath = process.argv[2];
  const cfg: MonitorCfg = cfgPath ? { ...DEFAULT_CFG, ...JSON.parse(fs.readFileSync(cfgPath, "utf8")) } : DEFAULT_CFG;
  validateConfig(cfg);
  const { program, conn, pid, url } = makeReadonlyProgram();
  log(`monitor — program ${pid.toBase58()}, RPC ${url}, markets ${cfg.markets.map((s) => s.slice(0, 6)).join(", ")}`);

  let latest: Metrics | null = null;
  let lastError: string | null = null;
  const refresh = async () => {
    try { latest = await collect(program, conn, pid, cfg); lastError = null; }
    catch (e: any) { lastError = errLine(e); log(`✗ refresh: ${lastError}`); }
  };
  await refresh();
  setInterval(refresh, (cfg.pollIntervalSecs ?? 15) * 1000);

  const page = fs.readFileSync(`${__dirname}/monitor.html`, "utf8")
    .replace("__POLL_MS__", String((cfg.pollIntervalSecs ?? 15) * 1000));
  const server = http.createServer((req, res) => {
    const path = (req.url || "/").split("?")[0];
    if (path === "/" || path === "/index.html") { res.writeHead(200, { "content-type": "text/html; charset=utf-8" }); res.end(page); return; }
    if (path === "/healthz") { res.writeHead(200, { "content-type": "text/plain" }); res.end(latest ? "ok" : "starting"); return; }
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
