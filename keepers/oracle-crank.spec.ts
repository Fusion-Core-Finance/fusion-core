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
    // sample is bounded BELOW twap_max_staleness (2/3 of it) so the TWAP corridor never ages out and
    // freezes mints, while spanning the window with >= min_samples and staying >= the anti-flood floor
    // ceil(window/(RING-1))=ceil(window/63). sb = 90% of max_age − 10s (fee-driven: each SB update PAYS
    // the signing oracles), floored at 30s. For window 300 / min_samples 3 / max_staleness 300:
    // antiFlood ceil(300/63)=5, staleBound floor(300*2/3)=200, minSampleBound ceil(300/2)=150 ->
    // sample max(5,min(200,150))=150; sb floor(300*9/10)-10=260.
    const i = intervalsFrom({ maxAgeSecs: 300, twapWindowSecs: 300, twapMinSamples: 3, twapMaxStalenessSecs: 300 }, { markets: [] });
    assert.deepEqual(i, { sample: 150, sb: 260, price: 60, refresh: 300 });
    // explicit config wins.
    const o = intervalsFrom({ maxAgeSecs: 300, twapWindowSecs: 300, twapMinSamples: 3, twapMaxStalenessSecs: 300 },
      { markets: [], sampleIntervalSecs: 42, sbIntervalSecs: 99, priceIntervalSecs: 7, refreshIntervalSecs: 600 });
    assert.deepEqual(o, { sample: 42, sb: 99, price: 7, refresh: 600 });
    // a tight max_staleness BINDS the sample cadence below the window/min_samples target (the #8 bug:
    // a large window with the min sample count must still sample often enough to stay fresh).
    const s = intervalsFrom({ maxAgeSecs: 300, twapWindowSecs: 1800, twapMinSamples: 3, twapMaxStalenessSecs: 300 }, { markets: [] });
    assert.equal(s.sample, 200); // staleBound floor(300*2/3)=200 < minSampleBound ceil(1800/2)=900
    // degenerate min_samples never divides by zero; sb never below the 30s floor.
    const d = intervalsFrom({ maxAgeSecs: 30, twapWindowSecs: 300, twapMinSamples: 1, twapMaxStalenessSecs: 3600 }, { markets: [] });
    assert.equal(d.sample, 300); // minSampleBound ceil(300/1)=300 < staleBound floor(3600*2/3)=2400
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
    assert.throws(() => validateConfig({ markets: ["So11111111111111111111111111111111111111112"], sbNumSignatures: 0 }), /1\.\.8/);
    assert.throws(() => validateConfig({ markets: ["So11111111111111111111111111111111111111112"], sbNumSignatures: 2.5 }), /1\.\.8/);
    validateConfig({ markets: ["So11111111111111111111111111111111111111112"], sbNumSignatures: 3 }); // ok
  });
});
