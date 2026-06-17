// Unit tests for the keeper's config validation (pure, no I/O). The crank loop itself is I/O-bound
// (RPC + anchor) and out of unit-test scope; importing keeper.ts does NOT start it (the entrypoint is
// guarded by `require.main === module`). Run via `npm test` (root ts-mocha).
import { expect } from "chai";
import { validateConfig } from "./keeper";

// A known-good config (the DEFAULT_CFG shape, valid base58 + 64-hex feed id).
const good = () => ({
  twapIntervalSecs: 15,
  priceIntervalSecs: 25,
  refreshIntervalSecs: 300,
  markets: [
    {
      collateralMint: "So11111111111111111111111111111111111111112",
      clmmPool: "Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE",
      pythMode: "persistent" as const,
      pythFeedIdHex: "ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d",
      pythAccount: "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE",
      switchboardFeed: "4Hmd6PdjVA9auCoScE12iaBogfwS4ZXQ6VZoBeqanwWW",
    },
  ],
});

describe("keeper validateConfig", () => {
  it("accepts a valid config", () => {
    expect(() => validateConfig(good())).to.not.throw();
  });

  it("rejects a non-positive interval (the setInterval busy-loop footgun)", () => {
    expect(() => validateConfig({ ...good(), priceIntervalSecs: 0 })).to.throw(/priceIntervalSecs/);
    expect(() => validateConfig({ ...good(), twapIntervalSecs: -5 })).to.throw(/twapIntervalSecs/);
    expect(() => validateConfig({ ...good(), refreshIntervalSecs: NaN })).to.throw(/refreshIntervalSecs/);
  });

  it("rejects empty markets", () => {
    expect(() => validateConfig({ ...good(), markets: [] })).to.throw(/markets/);
  });

  it("rejects an invalid base58 field", () => {
    const c = good();
    c.markets[0].collateralMint = "not-base58!";
    expect(() => validateConfig(c)).to.throw(/collateralMint/);
  });

  it("rejects an unknown pythMode (the silent mis-route footgun)", () => {
    const c: any = good();
    c.markets[0].pythMode = "persistant"; // typo
    expect(() => validateConfig(c)).to.throw(/pythMode/);
  });

  it("rejects persistent mode without pythAccount", () => {
    const c: any = good();
    delete c.markets[0].pythAccount;
    expect(() => validateConfig(c)).to.throw(/pythAccount/);
  });

  it("rejects a malformed pythFeedIdHex", () => {
    const c = good();
    c.markets[0].pythFeedIdHex = "xyz";
    expect(() => validateConfig(c)).to.throw(/pythFeedIdHex/);
  });
});
