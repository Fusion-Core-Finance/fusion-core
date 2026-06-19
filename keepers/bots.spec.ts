// Unit checks for the liquidator/redeemer money math in common.ts (the pure BigInt ports of the
// on-chain accrual + cdp::is_healthy). Run via `npm run test:sdk` (ts-mocha picks up keepers/**/*.spec.ts).
import assert from "node:assert";
import { currentDebt, isLiquidatable, SECONDS_PER_YEAR } from "./common";

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
