// Pure unit tests for the fuSOL stake-pool SDK module: PDA derivers (controller seeds mirrored
// from programs/fusion-stake-controller/src/constants.rs, fork-side seeds from
// vendor/spl-stake-pool/program/src/lib.rs) and the BigInt share math (ported from the vendored
// StakePool conversions + Fee::apply). No chain / no validator — deterministic, run via
// `npm run test:sdk` (root ts-mocha).
//
// The deriver block recomputes each PDA from RAW seed-string literals under RAW program-id
// literals and asserts it equals the deriver, so a drift in SEEDS, a deriver, or either pinned
// program id (vs the on-chain sources pinned here) fails loudly.
import { expect } from "chai";
import { PublicKey } from "@solana/web3.js";
import * as sp from "../src/stake-pool";

const CONTROLLER = new PublicKey("Fz3z1yh21PQ59smsPjmjeyK6ngh8KoK6PiPxUgCgspFq");
const FORK = new PublicKey("3pYHXui7Zk21TKE6oqivqbVJWRXt74wdDkqsnb3Q8mMi");
const VOTE = new PublicKey("So11111111111111111111111111111111111111112"); // any pubkey
const POSITION = new PublicKey("11111111111111111111111111111111"); // any pubkey
const POOL = new PublicKey("SysvarC1ock11111111111111111111111111111111"); // any pubkey
const at = (seeds: (Buffer | Uint8Array)[], pid: PublicKey) =>
  PublicKey.findProgramAddressSync(seeds, pid)[0].toBase58();
const s = (x: string) => Buffer.from(x);
const u32le = (n: number) => {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(n);
  return b;
};
const u64le = (n: bigint) => {
  const b = Buffer.alloc(8);
  b.writeBigUInt64LE(n);
  return b;
};

describe("stake-pool SDK program ids (pinned)", () => {
  it("controller + fork ids match the on-chain declare_id!s", () => {
    expect(sp.CONTROLLER_PROGRAM_ID.toBase58()).to.equal(CONTROLLER.toBase58());
    expect(sp.STAKE_POOL_FORK_ID.toBase58()).to.equal(FORK.toBase58());
    expect((sp.CONTROLLER_IDL as any).address).to.equal(CONTROLLER.toBase58());
  });
});

describe("stake-pool SDK PDA derivers (seeds pinned to constants.rs)", () => {
  it("controller singleton PDAs derive from the documented seeds", () => {
    expect(sp.controllerConfig().toBase58()).to.equal(at([s("controller")], CONTROLLER));
    expect(sp.epochState().toBase58()).to.equal(at([s("epoch_state")], CONTROLLER));
    expect(sp.poolAuthority().toBase58()).to.equal(at([s("pool_authority")], CONTROLLER));
    expect(sp.depositAuthority().toBase58()).to.equal(at([s("deposit_authority")], CONTROLLER));
    expect(sp.maintenanceAuthority().toBase58()).to.equal(at([s("maintenance")], CONTROLLER));
  });

  it("keyed controller PDAs derive from seed + key", () => {
    expect(sp.validatorRecord(VOTE).toBase58()).to.equal(
      at([s("validator"), VOTE.toBuffer()], CONTROLLER)
    );
    expect(sp.preference(POSITION).toBase58()).to.equal(
      at([s("preference"), POSITION.toBuffer()], CONTROLLER)
    );
  });

  it("poolWithdrawAuthority is [stake_pool, b\"withdraw\"] under the FORK id", () => {
    expect(sp.poolWithdrawAuthority(POOL).toBase58()).to.equal(
      at([POOL.toBuffer(), s("withdraw")], FORK)
    );
  });

  it("validatorStake appends the u32 LE seed ONLY when non-zero (upstream Option<NonZeroU32>)", () => {
    const noSeed = at([VOTE.toBuffer(), POOL.toBuffer()], FORK);
    expect(sp.validatorStake(VOTE, POOL).toBase58()).to.equal(noSeed);
    expect(sp.validatorStake(VOTE, POOL, 0).toBase58()).to.equal(noSeed); // 0 == None
    expect(sp.validatorStake(VOTE, POOL, 7).toBase58()).to.equal(
      at([VOTE.toBuffer(), POOL.toBuffer(), u32le(7)], FORK)
    );
    expect(sp.validatorStake(VOTE, POOL, 7).toBase58()).to.not.equal(noSeed);
  });

  it("transientStake is [b\"transient\", vote, pool, u64 LE] — the seed is ALWAYS appended", () => {
    expect(sp.transientStake(VOTE, POOL, 0n).toBase58()).to.equal(
      at([s("transient"), VOTE.toBuffer(), POOL.toBuffer(), u64le(0n)], FORK)
    );
    expect(sp.transientStake(VOTE, POOL, 2n ** 33n).toBase58()).to.equal(
      at([s("transient"), VOTE.toBuffer(), POOL.toBuffer(), u64le(2n ** 33n)], FORK)
    );
  });
});

describe("stake-pool SDK share math (mirrors vendored StakePool conversions)", () => {
  it("solToShares bootstraps 1:1 at zero total OR zero supply", () => {
    expect(sp.solToShares(1_000_000_000n, 0n, 0n)).to.equal(1_000_000_000n);
    expect(sp.solToShares(5n, 0n, 100n)).to.equal(5n); // zero total
    expect(sp.solToShares(5n, 100n, 0n)).to.equal(5n); // zero supply
  });

  it("solToShares floors (rounds against the depositor)", () => {
    expect(sp.solToShares(100n, 300n, 100n)).to.equal(33n); // 100·100/300 = 33.33…
    expect(sp.solToShares(2n, 3n, 1n)).to.equal(0n); // floors to 0
    expect(sp.solToShares(100n, 100n, 100n)).to.equal(100n); // 1:1 pool
  });

  it("sharesToSol floors (rounds against the withdrawer)", () => {
    expect(sp.sharesToSol(33n, 300n, 100n)).to.equal(99n); // 33·300/100
    expect(sp.sharesToSol(1n, 3n, 2n)).to.equal(1n); // 1.5 floors to 1
    expect(sp.sharesToSol(1n, 1n, 2n)).to.equal(0n); // numerator < denominator => 0
    expect(sp.sharesToSol(0n, 300n, 100n)).to.equal(0n);
  });

  it("sharesToSol is 0 at zero supply — the upstream asymmetry, NOT the deposit-side 1:1", () => {
    expect(sp.sharesToSol(5n, 100n, 0n)).to.equal(0n);
    expect(sp.sharesToSol(5n, 0n, 0n)).to.equal(0n);
  });

  it("a deposit/withdraw round trip never gains lamports", () => {
    const total = 12_345_678_901n;
    const supply = 11_111_111_111n;
    for (const lamports of [1n, 999n, 1_000_000_000n]) {
      const back = sp.sharesToSol(sp.solToShares(lamports, total, supply), total, supply);
      expect(back <= lamports).to.equal(true);
    }
  });

  it("applyFeeBps ceils exactly like upstream Fee::apply", () => {
    expect(sp.applyFeeBps(10_000n, 5n)).to.equal(5n); // exact: 5 bps of 10_000
    expect(sp.applyFeeBps(1n, 5n)).to.equal(1n); // any non-zero amount pays >= 1
    expect(sp.applyFeeBps(1_999n, 5n)).to.equal(1n); // ceil(0.9995) = 1
    expect(sp.applyFeeBps(10_001n, 5n)).to.equal(6n); // ceil(5.0005) = 6
    expect(sp.applyFeeBps(0n, 5n)).to.equal(0n); // zero amount pays zero
    expect(sp.applyFeeBps(1_000_000n, 0n)).to.equal(0n); // zero fee pays zero
    // The pinned pool fee: 5 bps on a 1-SOL deposit = 500_000 lamports of shares.
    expect(sp.applyFeeBps(1_000_000_000n, 5n)).to.equal(500_000n);
  });
});
