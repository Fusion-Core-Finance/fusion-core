// Unit checks for the governance client's pure helpers — flag parsing, enum resolution, clamp
// guardrails, and PDA seed wiring. Run via `npm run test:sdk` (ts-mocha globs keepers/**/*.spec.ts).
import assert from "node:assert";
import {
  flags, camel, resolveVariant, clampWarning, authorityOf, isOracleParam,
  timelockPda, gtimelockPda, marketPda, MARKET_CLAMPS, GLOBAL_CLAMPS, PublicKey,
} from "./gov-common";

const PID = new PublicKey("FuSiontgYvCc2N2Cinvo5gxSuxt2UfGxKMcbzkB67kud");
const MKT = ["Mcr", "DebtCeiling", "RedemptionFee", "LiqBonus", "KeeperReward"];

describe("gov-common helpers", () => {
  it("flags parses values, bare flags, and positionals", () => {
    const f = flags(["queue", "--param", "mcr", "--value", "15000", "--send"]);
    assert.deepEqual(f._, ["queue"]);
    assert.equal(f.get("param"), "mcr");
    assert.equal(f.get("value"), "15000");
    assert.equal(f.has("send"), true);
    assert.equal(f.has("missing"), false);
    assert.equal(f.get("missing"), undefined);
  });

  it("camel lowercases the first letter only", () => {
    assert.equal(camel("Mcr"), "mcr");
    assert.equal(camel("DebtCeiling"), "debtCeiling");
    assert.equal(camel("RateAdjustCooldown"), "rateAdjustCooldown");
  });

  it("resolveVariant matches case-insensitively and builds the Anchor enum arg", () => {
    assert.deepEqual(resolveVariant(MKT, "liqbonus"), { name: "LiqBonus", arg: { liqBonus: {} } });
    assert.deepEqual(resolveVariant(MKT, "debtCeiling"), { name: "DebtCeiling", arg: { debtCeiling: {} } });
    assert.deepEqual(resolveVariant(MKT, "MCR"), { name: "Mcr", arg: { mcr: {} } });
    assert.throws(() => resolveVariant(MKT, "totallyBogus"), /unknown param/); // not a valid variant
  });

  it("clampWarning flags out-of-range values, passes in-range and unbounded ones", () => {
    assert.match(clampWarning("Mcr", 5000n, MARKET_CLAMPS)!, /< documented min/);
    assert.match(clampWarning("Mcr", 40000n, MARKET_CLAMPS)!, /> documented max/);
    assert.equal(clampWarning("Mcr", 15000n, MARKET_CLAMPS), null);
    assert.equal(clampWarning("DebtCeiling", 999999n, MARKET_CLAMPS), null); // no upper clamp
    assert.equal(clampWarning("Unknown", 1n, MARKET_CLAMPS), null);
    assert.match(clampWarning("Cut", 5000n, GLOBAL_CLAMPS)!, /> documented max/); // backstop cut max 3000
    // RiskParamRegistry additions
    assert.match(clampWarning("Scr", 10000n, MARKET_CLAMPS)!, /< documented min/); // scr min 10500
    assert.match(clampWarning("OracleMaxAge", 3601n, MARKET_CLAMPS)!, /> documented max/); // clamp is 3600 (db57635)
    assert.equal(clampWarning("OracleK", 15000n, MARKET_CLAMPS), null);
  });

  it("isOracleParam matches oracle-registry params in either case, never market ones", () => {
    assert.equal(isOracleParam("OracleMaxConf"), true);
    assert.equal(isOracleParam("oracleTwapStaleness"), true); // camelCase decoded key
    assert.equal(isOracleParam("scr"), false); // Scr writes the Market, not the oracle
    assert.equal(isOracleParam("mcr"), false);
  });

  it("authorityOf returns the wallet, honors --authority in dry-run, and rejects a foreign override in --send", () => {
    const me = PublicKey.default;
    const other = PID; // any pubkey ≠ me
    assert.ok(authorityOf(flags([]), me, false).equals(me));
    assert.ok(authorityOf(flags(["--authority", other.toBase58()]), me, false).equals(other)); // external signer/PDA dry-run
    assert.ok(authorityOf(flags(["--authority", me.toBase58()]), me, true).equals(me)); // matching override + --send ok
    assert.throws(() => authorityOf(flags(["--authority", other.toBase58()]), me, true), /dry-run proposals only/);
  });

  it("timelock PDAs are deterministic, nonce-distinct, and prefix-distinct from the global twin", () => {
    assert.ok(timelockPda(PID, 0n).equals(timelockPda(PID, 0n)));
    assert.ok(!timelockPda(PID, 0n).equals(timelockPda(PID, 1n)));
    assert.ok(!timelockPda(PID, 0n).equals(gtimelockPda(PID, 0n))); // [b"timelock"] vs [b"gtimelock"]
    assert.ok(!marketPda(PID, PID).equals(timelockPda(PID, 0n)));
  });
});
