// Unit checks for the monitor's pure logic — the metric math and the alert rules (the parts that
// decide whether the dashboard screams). Run via `npm run test:sdk` (ts-mocha globs keepers/**/*.spec.ts).
import assert from "node:assert";
import { avgRateBps, pct, tcrBps, computeAlerts, Metrics, MarketMetrics } from "./monitor";

const RAY = 10n ** 27n;

function baseMarket(over: Partial<MarketMetrics> = {}): MarketMetrics {
  return {
    mint: "So11111111111111111111111111111111111111112", tag: "So1111", exists: true,
    shutdown: false, shutdownReason: 0, mintFrozen: false,
    aggDebtUsd: 1000, unmintedUsd: 0, badDebtUsd: 0, avgRateBps: 500,
    debtCeilingUsd: 10000, ceilingUsedPct: 10,
    tvlUsd: 2000, tcrBps: 20000, mcrBps: 15000, scrBps: 11000, ccrBps: 0,
    spotUsd: 100, debtSpotUsd: 101, slotsSincePrice: 5, collDecimals: 9,
    liqGraceActive: false, liqDivergenceActive: false, guardianPauseActive: false,
    bufferUsd: 100, bufferTargetUsd: null, bufferFundedUsd: 100, bufferAbsorbedUsd: 0,
    rpDepositsUsd: 500, rpSeizedColl: 0, rlCapUsd: 0, rlUsedPct: 0,
    protocolCollateral: 0, globalContributedUsd: 0, globalDrawnUsd: 0, ...over,
  };
}
function baseMetrics(globalOver: any = {}, marketOver: Partial<MarketMetrics> = {}): Metrics {
  return {
    ts: Date.now(), slot: 1000, rpc: "x",
    global: { fusdSupplyUsd: 1000, sumBackingUsd: 1000, supplyDeltaUsd: 0, govAuthority: "g", guardian: "u", pendingGovAuthority: null, backstop: null, ...globalOver },
    markets: [baseMarket(marketOver)], thresholds: { staleSlots: 250, ceilingWarnPct: 90 }, alerts: [],
  };
}
const fire = (m: Metrics, sub: string) => computeAlerts(m).filter((a) => a.message.includes(sub));

describe("monitor metric math", () => {
  it("avgRateBps = Σ(debt·rate) / Σ debt", () => {
    // two equal debts at 500 and 1500 bps ⇒ 1000 bps average.
    assert.equal(avgRateBps(1000n * 500n + 1000n * 1500n, 2000n), 1000);
    assert.equal(avgRateBps(0n, 0n), 0); // no debt
  });
  it("pct rounds to 2dp and is 0 over a zero denominator", () => {
    assert.equal(pct(900n, 1000n), 90);
    assert.equal(pct(12345n, 100000n), 12.34); // floor at 2dp
    assert.equal(pct(5n, 0n), 0);
  });
  it("tcrBps = collateral value / debt; null when debtless", () => {
    // 10 coll @ spot 100 ⇒ value 1000; debt 500 ⇒ 200% = 20000 bps.
    assert.equal(tcrBps(10n, 100n * RAY, 500n), 20000);
    assert.equal(tcrBps(10n, 100n * RAY, 0n), null);
  });
});

describe("monitor alert rules", () => {
  it("healthy + solvent ⇒ no alerts", () => {
    assert.equal(computeAlerts(baseMetrics()).length, 0);
  });
  it("backing exceeding circulating ⇒ critical supply alert", () => {
    const a = fire(baseMetrics({ fusdSupplyUsd: 1000, sumBackingUsd: 1001, supplyDeltaUsd: -1 }), "supply identity broken");
    assert.equal(a.length, 1);
    assert.equal(a[0].severity, "critical");
    // the inverse (more circulating than backing — unmonitored markets) must NOT alarm.
    assert.equal(fire(baseMetrics({ fusdSupplyUsd: 2000, sumBackingUsd: 1000, supplyDeltaUsd: 1000 }), "supply identity").length, 0);
  });
  it("shutdown / bad debt / TCR<SCR are critical", () => {
    assert.equal(fire(baseMetrics({}, { shutdown: true, shutdownReason: 2 }), "SHUT DOWN")[0].severity, "critical");
    assert.equal(fire(baseMetrics({}, { badDebtUsd: 5 }), "bad debt")[0].severity, "critical");
    assert.equal(fire(baseMetrics({}, { tcrBps: 10000, scrBps: 11000 }), "below SCR")[0].severity, "critical");
  });
  it("mint freeze / stale price / ceiling / buffer-below-target are warnings", () => {
    assert.equal(fire(baseMetrics({}, { mintFrozen: true }), "mint frozen")[0].severity, "warn");
    assert.equal(fire(baseMetrics({}, { slotsSincePrice: 300 }), "stale")[0].severity, "warn");
    assert.equal(fire(baseMetrics({}, { ceilingUsedPct: 95 }), "ceiling")[0].severity, "warn");
    assert.equal(fire(baseMetrics({}, { bufferUsd: 50, bufferTargetUsd: 100 }), "below target")[0].severity, "warn");
    // a target met (or none) ⇒ no buffer alert.
    assert.equal(fire(baseMetrics({}, { bufferUsd: 150, bufferTargetUsd: 100 }), "below target").length, 0);
  });
  it("CCR band warns only when TCR is between SCR and CCR (no double-fire with SCR)", () => {
    const a = computeAlerts(baseMetrics({}, { tcrBps: 12000, scrBps: 11000, ccrBps: 13000 }));
    assert.equal(a.filter((x) => x.message.includes("CCR band")).length, 1);
    assert.equal(a.filter((x) => x.message.includes("below SCR")).length, 0);
  });
  it("backstop solvency mismatch ⇒ critical", () => {
    const m = baseMetrics({ backstop: { balanceUsd: 5, cutBps: 0, reserveCapUsd: 0, contributedUsd: 10, absorbedUsd: 0, withdrawnUsd: 0, solvencyOk: false } });
    assert.equal(fire(m, "backstop solvency")[0].severity, "critical");
  });
});
