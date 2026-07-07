// Unit checks for the liquidator/redeemer money math in common.ts (the pure BigInt ports of the
// on-chain accrual + cdp::is_healthy). Run via `npm run test:sdk` (ts-mocha picks up keepers/**/*.spec.ts).
import assert from "node:assert";
import { currentDebt, isLiquidatable, pendingRedist, REDIST_PRECISION, SECONDS_PER_YEAR, priorityIxs, nonReentrant } from "./common";

// spot/debt_spot are RAY-scaled fUSD-native per native collateral unit:
//   spot = usd * 10^FUSD_DEC * RAY / 10^COLL_DEC = usd * 1e6 * 1e27 / 1e9 = usd * 1e24  (WSOL, 9-dec coll).
const usdToDebtSpot = (usd: number) => BigInt(usd) * 10n ** 24n;
const fusd = (d: number) => BigInt(d) * 1_000_000n; // fUSD-native (6 dec)

describe("liquidator/redeemer math (common.ts)", () => {
  it("currentDebt accrues interest over the elapsed period", () => {
    // 1000 fUSD at 10% (1000 bps) for exactly one year ⇒ +100 fUSD interest.
    assert.equal(currentDebt(fusd(1000), 1000, 0n, SECONDS_PER_YEAR), fusd(1100));
    // half a year ⇒ +50.
    assert.equal(currentDebt(fusd(1000), 1000, 0n, SECONDS_PER_YEAR / 2n), fusd(1050));
  });

  it("currentDebt does not accrue when now <= last update (clock skew safe)", () => {
    assert.equal(currentDebt(fusd(1000), 2000, 100n, 100n), fusd(1000));
    assert.equal(currentDebt(fusd(1000), 2000, 200n, 100n), fusd(1000));
  });

  it("isLiquidatable: true past the MCR, false within it (priced at debt_spot)", () => {
    // 1 SOL collateral at $45, MCR 150% ⇒ max debt = 45 * 1e4/15000 = $30.
    const ink = 1_000_000_000n; // 1 WSOL (9 dec)
    const debtSpot = usdToDebtSpot(45);
    assert.equal(isLiquidatable(ink, fusd(40), debtSpot, 15000), true, "$40 debt > $30 max ⇒ liquidatable");
    assert.equal(isLiquidatable(ink, fusd(30), debtSpot, 15000), false, "exactly at max ⇒ healthy");
    assert.equal(isLiquidatable(ink, fusd(25), debtSpot, 15000), false, "$25 debt < $30 max ⇒ healthy");
  });

  it("isLiquidatable: degenerate inputs are never liquidatable", () => {
    assert.equal(isLiquidatable(1_000_000_000n, 0n, usdToDebtSpot(45), 15000), false, "zero debt");
    assert.equal(isLiquidatable(1_000_000_000n, fusd(40), 0n, 15000), false, "no price");
    assert.equal(isLiquidatable(1_000_000_000n, fusd(40), usdToDebtSpot(45), 0), false, "mcr 0 (disabled)");
  });
});

describe("priorityIxs (compute-budget send helper, common.ts)", () => {
  const COMPUTE_BUDGET = "ComputeBudget111111111111111111111111111111";
  it("emits a priority-fee price ix, and prepends a CU-limit ix when given", () => {
    const price = priorityIxs();
    assert.equal(price.length, 1);
    assert.equal(price[0].programId.toBase58(), COMPUTE_BUDGET);
    assert.equal(price[0].data[0], 3); // SetComputeUnitPrice discriminator
    const withLimit = priorityIxs(400_000);
    assert.equal(withLimit.length, 2);
    assert.equal(withLimit[0].data[0], 2); // SetComputeUnitLimit prepended (finding: SB precompile safe)
    assert.equal(withLimit[1].data[0], 3); // price second
  });
});

describe("pendingRedist (tier-2 redistribution fold, common.ts)", () => {
  it("ports redist::pending: floor(stake·(l − snapshot)/1e18) per leg", () => {
    // stake 100, snapshot 0; l_coll bumped 0.1·1e18 ⇒ 100·1e17/1e18 = 10 coll; l_art 0.4 ⇒ 40 debt.
    const p = pendingRedist(100n, REDIST_PRECISION / 10n, (REDIST_PRECISION * 4n) / 10n, 0n, 0n);
    assert.deepEqual(p, { coll: 10n, debt: 40n });
  });

  it("floors each leg (protocol-favoring dust), never rounds up", () => {
    // stake 3, delta 0.5·1e18 ⇒ 3·5e17/1e18 = 1.5 → floored to 1.
    assert.equal(pendingRedist(3n, REDIST_PRECISION / 2n, 0n, 0n, 0n).coll, 1n);
  });

  it("no gain when stake is 0 or the accumulator has not passed the snapshot (saturating_sub)", () => {
    assert.deepEqual(pendingRedist(0n, REDIST_PRECISION, REDIST_PRECISION, 0n, 0n), { coll: 0n, debt: 0n });
    assert.deepEqual(pendingRedist(100n, 5n, 5n, 5n, 5n), { coll: 0n, debt: 0n }); // l == snapshot
    assert.deepEqual(pendingRedist(100n, 3n, 3n, 5n, 5n), { coll: 0n, debt: 0n }); // l < snapshot ⇒ delta 0
  });

  it("a prior redistribution can tip an otherwise-healthy position under MCR (the miss this closes)", () => {
    // 1 WSOL @ $45, MCR 150% ⇒ max debt $30. $25 recorded debt is healthy on its own...
    const ink = 1_000_000_000n;
    const debtSpot = usdToDebtSpot(45);
    const baseDebt = fusd(25);
    assert.equal(isLiquidatable(ink, baseDebt, debtSpot, 15000), false, "healthy pre-redistribution");
    // ...but a pending redistribution of +$8 debt tips present debt to $33 > $30.
    const stake = 1_000_000_000n;
    const lArt = (fusd(8) * REDIST_PRECISION) / stake; // craft l_art so stake·l_art/1e18 == $8
    const pend = pendingRedist(stake, 0n, lArt, 0n, 0n);
    assert.equal(pend.debt, fusd(8));
    assert.equal(
      isLiquidatable(ink + pend.coll, baseDebt + pend.debt, debtSpot, 15000), true,
      "liquidatable after folding pending redistribution",
    );
  });
});

describe("nonReentrant (interval re-entrancy guard, common.ts)", () => {
  it("skips a tick while the previous one is still in flight, then resumes", async () => {
    let calls = 0;
    let release!: () => void;
    const gate = new Promise<void>((r) => { release = r; });
    const guarded = nonReentrant(async () => { calls++; await gate; });

    const first = guarded();                 // starts, now in flight (awaiting gate)
    await Promise.resolve();                 // let the wrapped fn reach its await
    assert.equal(calls, 1);
    assert.equal(await guarded(), false, "overlapping tick is skipped");
    assert.equal(calls, 1, "wrapped fn is not re-invoked while in flight");

    release();                               // let the first tick finish (gate now resolved)
    assert.equal(await first, true, "the tick that actually ran returns true");
    assert.equal(await guarded(), true, "the next tick runs once the previous finished");
    assert.equal(calls, 2);
  });

  it("clears the in-flight flag when a tick throws (error still propagates)", async () => {
    let calls = 0;
    const guarded = nonReentrant(async () => { calls++; throw new Error("boom"); });
    await assert.rejects(guarded(), /boom/);       // error reaches the caller's try/catch (its `✗` log)
    await assert.rejects(guarded(), /boom/);       // flag was reset ⇒ the next tick still runs
    assert.equal(calls, 2);
  });
});
