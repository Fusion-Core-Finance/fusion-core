// Unit checks for the oracle crank's pure logic — the Pyth sponsored-feed derivation, the cadence
// derivation from on-chain oracle config, and the dueness predicate. Run via `npm run test:sdk`.
import assert from "node:assert";
import { pythFeedAccount, intervalsFrom, due, validateConfig } from "./oracle-crank";

const SOL_USD = Buffer.from("ef0d8b6fda2ceba41da15d4095d1da392a0d2f8ed0c6c7bc0f4cfac8c280b56d", "hex");

describe("oracle-crank helpers", () => {
  it("pythFeedAccount derives the shard-0 SOL/USD sponsored feed (pinned to the live mainnet account)", () => {
    assert.equal(pythFeedAccount(0, SOL_USD).toBase58(), "7UVimffxr9ow1uXYxsr4LHAcV58mLzhmwaeKvJ1pjLiE");
    assert.notEqual(pythFeedAccount(1, SOL_USD).toBase58(), pythFeedAccount(0, SOL_USD).toBase58()); // shard-distinct
  });

  it("intervalsFrom derives safe cadences from the on-chain oracle config", () => {
    // window 300 / (3-1) = 150s on-chain floor -> 155s with slack; sb = 2/3 of max_age; price default 60;
    // refresh_market default 300 (interest folding — no on-chain derivation, config-only).
    const i = intervalsFrom({ maxAgeSecs: 300, twapWindowSecs: 300, twapMinSamples: 3 }, { markets: [] });
    assert.deepEqual(i, { sample: 155, sb: 200, price: 60, refresh: 300 });
    // explicit config wins.
    const o = intervalsFrom({ maxAgeSecs: 300, twapWindowSecs: 300, twapMinSamples: 3 },
      { markets: [], sampleIntervalSecs: 42, sbIntervalSecs: 99, priceIntervalSecs: 7, refreshIntervalSecs: 600 });
    assert.deepEqual(o, { sample: 42, sb: 99, price: 7, refresh: 600 });
    // degenerate min_samples never divides by zero; sb never below the 30s floor.
    const d = intervalsFrom({ maxAgeSecs: 30, twapWindowSecs: 300, twapMinSamples: 1 }, { markets: [] });
    assert.equal(d.sample, 305);
    assert.equal(d.sb, 30);
  });

  it("due fires immediately when never run, at/after the interval, not before", () => {
    assert.equal(due(1000, 0, 60), true); // never run
    assert.equal(due(1000, 941, 60), false); // 59s ago
    assert.equal(due(1000, 940, 60), true); // exactly 60s
    assert.equal(due(1000, 900, 60), true);
  });

  it("validateConfig rejects bad markets, intervals, and shard", () => {
    assert.throws(() => validateConfig({ markets: [] }), /non-empty/);
    assert.throws(() => validateConfig({ markets: ["not-a-key"] }), /not a valid pubkey/);
    assert.throws(() => validateConfig({ markets: ["So11111111111111111111111111111111111111112"], tickSecs: -5 }), /positive/);
    assert.throws(() => validateConfig({ markets: ["So11111111111111111111111111111111111111112"], refreshIntervalSecs: 0 }), /positive/);
    assert.throws(() => validateConfig({ markets: ["So11111111111111111111111111111111111111112"], refreshIntervalSecs: NaN }), /positive/);
    assert.throws(() => validateConfig({ markets: ["So11111111111111111111111111111111111111112"], pythShard: 70000 }), /u16/);
    validateConfig({ markets: ["So11111111111111111111111111111111111111112"], priceIntervalSecs: 60 }); // ok
    validateConfig({ markets: ["So11111111111111111111111111111111111111112"], refreshIntervalSecs: 300 }); // ok
  });
});
