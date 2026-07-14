// Unit checks for the stake-pool crank's pure logic — the config gate, the phase→action map,
// the batch-slicing math, the ValidatorList byte parse, the stake/transient PDA derivations
// (pinned against the Rust spl_cpi derivations), and the TS port of the deterministic rebalance
// walk (vectors copied from the Rust tests in programs/fusion-stake-controller/src/logic.rs).
// Run via `npm run test:sdk`.
import assert from "node:assert";
import {
  validateConfig, nextAction, batchSlice, rotationStart, rebalanceSlot, parseValidatorList,
  validatorStakeAddress, transientStakeAddress, isAdmissionExtra, isAdmissionAddDue,
  PHASE_IDLE, PHASE_RECONCILE, PHASE_FINALIZE, PHASE_PREFERENCES, PHASE_PLAN_DIRECTED,
  PHASE_PLAN_NEUTRAL, PHASE_PLAN_FINALIZE, PHASE_REBALANCE,
  VALIDATOR_LIST_INDEX_UNSET, UPSTREAM_MINIMUM_DELEGATION,
  EpochView, RecordView,
} from "./stake-pool-crank";
import { PublicKey, Pk } from "./common";

const VOTE = new PublicKey("Vote111111111111111111111111111111111111111");
const POOL = new PublicKey("So11111111111111111111111111111111111111112");

describe("stake-pool-crank helpers", () => {
  it("validateConfig rejects bad ticks, batch sizes, pubkeys, and watch lists", () => {
    validateConfig({}); // everything optional
    validateConfig({ tickSecs: 30, batchSize: 3, controllerProgramId: POOL.toBase58(), rewardAta: VOTE.toBase58(), validatorRecordsWatch: [VOTE.toBase58()] });
    assert.throws(() => validateConfig({ tickSecs: 0 }), /positive/);
    assert.throws(() => validateConfig({ tickSecs: -5 }), /positive/);
    assert.throws(() => validateConfig({ tickSecs: NaN }), /positive/);
    assert.throws(() => validateConfig({ batchSize: 0 }), /1\.\.3/);
    assert.throws(() => validateConfig({ batchSize: 2.5 }), /1\.\.3/);
    assert.throws(() => validateConfig({ batchSize: 4 }), /1\.\.3/); // 4 quads overflow the 1232-byte legacy tx
    assert.throws(() => validateConfig({ controllerProgramId: "not-a-key" }), /not a valid pubkey/);
    assert.throws(() => validateConfig({ rewardAta: "nope" }), /not a valid pubkey/);
    assert.throws(() => validateConfig({ validatorRecordsWatch: "x" as any }), /array/);
    assert.throws(() => validateConfig({ validatorRecordsWatch: [VOTE.toBase58(), "bad"] }), /validatorRecordsWatch\[1\]/);
  });

  it("nextAction maps every phase to its leg, preempted by an advanced cluster epoch", () => {
    const base: EpochView = {
      clusterEpoch: 10n, controllerEpoch: 10n, phase: PHASE_IDLE, currentSlot: 1_000n,
      preferenceWindowCloseSlot: 2_000n, rebalanceCursor: 0n, plannedLen: 5n, churnBudgetRemaining: 10_000_000n,
    };
    // The forward table (same-epoch): phase → leg.
    assert.equal(nextAction(base).kind, "idle");
    assert.equal(nextAction({ ...base, phase: PHASE_RECONCILE }).kind, "reconcile_batch");
    assert.equal(nextAction({ ...base, phase: PHASE_FINALIZE }).kind, "finalize_pool");
    assert.deepEqual(nextAction({ ...base, phase: PHASE_PREFERENCES }), { kind: "wait_preference_window", closeSlot: 2_000n });
    assert.equal(nextAction({ ...base, phase: PHASE_PREFERENCES, currentSlot: 2_000n }).kind, "close_preference_window"); // at the slot, not only after
    assert.equal(nextAction({ ...base, phase: PHASE_PREFERENCES, currentSlot: 3_000n }).kind, "close_preference_window");
    assert.equal(nextAction({ ...base, phase: PHASE_PLAN_DIRECTED }).kind, "plan_directed_batch");
    assert.equal(nextAction({ ...base, phase: PHASE_PLAN_NEUTRAL }).kind, "plan_neutral_batch");
    assert.equal(nextAction({ ...base, phase: PHASE_PLAN_FINALIZE }).kind, "finalize_plan");
    assert.equal(nextAction({ ...base, phase: PHASE_REBALANCE }).kind, "execute_next_action");
    // REBALANCE completion: full walk (2 × planned) or a churn budget below one minimum action.
    assert.equal(nextAction({ ...base, phase: PHASE_REBALANCE, rebalanceCursor: 9n }).kind, "execute_next_action");
    assert.equal(nextAction({ ...base, phase: PHASE_REBALANCE, rebalanceCursor: 10n }).kind, "finish_epoch");
    assert.equal(nextAction({ ...base, phase: PHASE_REBALANCE, plannedLen: 0n }).kind, "finish_epoch"); // nothing planned
    assert.equal(nextAction({ ...base, phase: PHASE_REBALANCE, churnBudgetRemaining: UPSTREAM_MINIMUM_DELEGATION - 1n }).kind, "finish_epoch");
    assert.equal(nextAction({ ...base, phase: PHASE_REBALANCE, churnBudgetRemaining: UPSTREAM_MINIMUM_DELEGATION }).kind, "execute_next_action");
    // Epoch preemption fires from EVERY phase (the on-chain `* → RECONCILE` edge), corrupt included.
    for (const phase of [PHASE_IDLE, PHASE_RECONCILE, PHASE_FINALIZE, PHASE_PREFERENCES, PHASE_PLAN_DIRECTED, PHASE_PLAN_NEUTRAL, PHASE_PLAN_FINALIZE, PHASE_REBALANCE, 0xff])
      assert.equal(nextAction({ ...base, phase, clusterEpoch: 11n }).kind, "start_epoch", `phase ${phase}`);
    // A corrupt phase without an epoch advance can only wait for the preemption edge.
    assert.equal(nextAction({ ...base, phase: 0xff }).kind, "idle");
  });

  it("batchSlice walks a cursor range in bounded contiguous slices", () => {
    assert.deepEqual(batchSlice(0n, 10n, 4), { start: 0n, count: 4, last: false });
    assert.deepEqual(batchSlice(4n, 10n, 4), { start: 4n, count: 4, last: false });
    assert.deepEqual(batchSlice(8n, 10n, 4), { start: 8n, count: 2, last: true });
    assert.deepEqual(batchSlice(0n, 3n, 4), { start: 0n, count: 3, last: true }); // short list, one batch
    assert.deepEqual(batchSlice(10n, 10n, 4), { start: 10n, count: 0, last: true }); // done → the empty transition call
    assert.deepEqual(batchSlice(0n, 0n, 4), { start: 0n, count: 0, last: true }); // empty list
  });

  it("rebalanceSlot is two rotated passes (vectors from logic.rs rebalance_walk_is_two_rotated_passes)", () => {
    // len 5, epoch 7 → rotation start = 7 % 5 = 2; pass 0 then pass 1, each [2,3,4,0,1].
    assert.equal(rotationStart(7n, 5n), 2n);
    assert.equal(rotationStart(0n, 5n), 0n);
    assert.equal(rotationStart(7n, 0n), 0n); // degenerate n
    const expectedOrder = [2n, 3n, 4n, 0n, 1n];
    for (let cursor = 0; cursor < 10; cursor++) {
      const slot = rebalanceSlot(BigInt(cursor), 5n, 7n)!;
      assert.equal(slot.pass, Math.floor(cursor / 5), `cursor ${cursor} pass`);
      assert.equal(slot.index, expectedOrder[cursor % 5], `cursor ${cursor} index`);
    }
    // Termination and the empty plan.
    assert.equal(rebalanceSlot(10n, 5n, 7n), null);
    assert.equal(rebalanceSlot(2n ** 64n - 1n, 5n, 7n), null); // u64::MAX
    assert.equal(rebalanceSlot(0n, 0n, 7n), null);
  });

  it("rebalanceSlot names exactly one in-bounds index per cursor, deterministically (logic.rs vectors)", () => {
    for (const epoch of [0n, 1n, 3n, 1_000_003n]) {
      for (const len of [1n, 2n, 7n, 1_024n]) {
        const walk = 2n * len < 64n ? 2n * len : 64n;
        const perPass = new Map<number, Set<bigint>>();
        for (let cursor = 0n; cursor < walk; cursor++) {
          const slot = rebalanceSlot(cursor, len, epoch)!;
          assert.ok(slot.index < len, `index ${slot.index} < len ${len}`);
          assert.deepEqual(rebalanceSlot(cursor, len, epoch), slot); // pure determinism
          (perPass.get(slot.pass) ?? perPass.set(slot.pass, new Set()).get(slot.pass)!).add(slot.index);
        }
        // Every index visited exactly once per completed pass — omission is impossible.
        for (const [, visited] of perPass) if (visited.size < Number(len)) assert.ok(walk < 2n * len, "incomplete pass only when truncated");
        if (2n * len <= 64n) for (const [pass, visited] of perPass) assert.equal(visited.size, Number(len), `pass ${pass} full coverage len ${len}`);
      }
    }
  });

  it("parseValidatorList decodes the upstream 73-byte entries and fails closed on malformed buffers", () => {
    // Synthetic account: type 2, capacity 4, len 2, pre-allocated to capacity (trailing zeros ignored).
    const data = Buffer.alloc(9 + 4 * 73);
    data[0] = 2;
    data.writeUInt32LE(4, 1); // max_validators
    data.writeUInt32LE(2, 5); // len
    const writeEntry = (i: number, active: bigint, transient: bigint, lastUpdate: bigint, tSeed: bigint, vSeed: number, status: number, vote: Pk) => {
      const o = 9 + i * 73;
      data.writeBigUInt64LE(active, o);
      data.writeBigUInt64LE(transient, o + 8);
      data.writeBigUInt64LE(lastUpdate, o + 16);
      data.writeBigUInt64LE(tSeed, o + 24);
      data.writeUInt32LE(0xdeadbeef, o + 32); // upstream's unused u32 — must not leak into any field
      data.writeUInt32LE(vSeed, o + 36);
      data[o + 40] = status;
      vote.toBuffer().copy(data, o + 41);
    };
    writeEntry(0, 1_000_000_000n, 5n, 700n, 42n, 0, 0, VOTE);
    writeEntry(1, 2n ** 63n, 0n, 701n, 7n, 9, 3, POOL);
    const list = parseValidatorList(data);
    assert.equal(list.maxValidators, 4);
    assert.equal(list.entries.length, 2);
    assert.deepEqual(list.entries[0], {
      activeStakeLamports: 1_000_000_000n, transientStakeLamports: 5n, lastUpdateEpoch: 700n,
      transientSeedSuffix: 42n, validatorSeedSuffix: 0, status: 0, voteAccount: VOTE,
    });
    assert.deepEqual(list.entries[1], {
      activeStakeLamports: 2n ** 63n, transientStakeLamports: 0n, lastUpdateEpoch: 701n,
      transientSeedSuffix: 7n, validatorSeedSuffix: 9, status: 3, voteAccount: POOL,
    });
    // Malformed: wrong account type, header-short buffer, len claiming past the buffer.
    const wrongType = Buffer.from(data); wrongType[0] = 1;
    assert.throws(() => parseValidatorList(wrongType), /account_type/);
    assert.throws(() => parseValidatorList(data.subarray(0, 8)), /header/);
    const truncated = Buffer.from(data); truncated.writeUInt32LE(5, 5); // claims 5 entries in a 4-capacity buffer
    assert.throws(() => parseValidatorList(truncated), /exceeds/);
  });

  it("stake/transient PDA derivations match the Rust spl_cpi derivations (pinned vectors)", () => {
    // Ground truth generated from spl_cpi::derive_validator_stake / derive_transient_stake's
    // exact seed layouts via solana-pubkey find_program_address (vote=Vote111…, pool=So11…112).
    assert.equal(validatorStakeAddress(VOTE, POOL, 0).toBase58(), "5NZpanjSCdATncAPY4kHgW7UEPGknmADEAgUVNRHHrxa");
    assert.equal(validatorStakeAddress(VOTE, POOL, 7).toBase58(), "FHnqx8Xftcf9o8YSP2M9LUEjkTrv9xnm8XA8Z5iozSbe"); // u32 suffix present only when nonzero
    assert.equal(transientStakeAddress(VOTE, POOL, 0n).toBase58(), "9pGXMCtVxqNXCM5YdMUTGeE2kVRP7utVitpBpUvnRsU7"); // u64 suffix ALWAYS present
    assert.equal(transientStakeAddress(VOTE, POOL, 42n).toBase58(), "9cQFUauJXZUxaBbGZXPycaFyKf8jFdqsd65eGV6gzEv8");
  });

  it("admission filters mirror the on-chain gates", () => {
    const e = 700n;
    const base: RecordView = {
      vote: VOTE, listIndex: VALIDATOR_LIST_INDEX_UNSET, status: 1 /* Candidate */, planEpoch: 0n,
      observedEpoch: e, observedOk: true, directedSharesEpoch: e, directedShares: 0n,
    };
    // PLAN-DIRECTED extras: unadmitted + not yet planned this epoch + plausibly admittable.
    assert.ok(isAdmissionExtra(base, e)); // Candidate needing a re-plan
    assert.ok(isAdmissionExtra({ ...base, status: 0, directedSharesEpoch: e }, e)); // Registered with current shares
    assert.ok(!isAdmissionExtra({ ...base, status: 0, directedSharesEpoch: e - 1n }, e)); // Registered, stale shares — cannot admit
    assert.ok(!isAdmissionExtra({ ...base, planEpoch: e }, e)); // already planned (RecordAlreadyPlanned)
    assert.ok(!isAdmissionExtra({ ...base, listIndex: 3 }, e)); // in the list — the cursor slice covers it
    // REBALANCE adds: execute_next_action's admission-mode requires, re-checked off-chain.
    // productive 10_000 SOL over supply 10_000 fuSOL ⇒ raw target == shares.
    const P = 10_000n * 10n ** 9n, S = 10_000n * 10n ** 9n;
    const due: RecordView = { ...base, planEpoch: e, directedShares: 500n * 10n ** 9n };
    assert.ok(isAdmissionAddDue(due, e, P, S));
    assert.ok(!isAdmissionAddDue({ ...due, directedShares: 500n * 10n ** 9n - 1n }, e, P, S)); // below the activation floor
    assert.ok(!isAdmissionAddDue({ ...due, directedSharesEpoch: e - 1n }, e, P, S)); // stale shares read as zero
    assert.ok(!isAdmissionAddDue({ ...due, planEpoch: e - 1n }, e, P, S)); // not planned this epoch
    assert.ok(!isAdmissionAddDue({ ...due, observedOk: false }, e, P, S)); // failing observation
    assert.ok(!isAdmissionAddDue({ ...due, observedEpoch: e - 1n }, e, P, S)); // stale observation
    assert.ok(!isAdmissionAddDue({ ...due, listIndex: 3 }, e, P, S)); // already in the list
    assert.ok(!isAdmissionAddDue({ ...due, status: 2 }, e, P, S)); // Active is not an admission target
    assert.ok(!isAdmissionAddDue(due, e, P, 0n)); // degenerate supply
  });
});
