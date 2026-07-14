/**
 * fuSOL stake-pool crank — the permissionless keeper that drives the Allocation Controller's
 * epoch state machine (programs/fusion-stake-controller) around its cycle:
 *
 *   IDLE → RECONCILE → FINALIZE → PREFERENCES → PLAN-DIRECTED → PLAN-NEUTRAL → PLAN-FINALIZE →
 *   REBALANCE → IDLE
 *
 * One tickSecs heartbeat, one nonReentrant sweep. Each sweep reads EpochState + the cluster
 * epoch/slot and executes the ONE leg the machine demands (`nextAction`), chaining legs while
 * progress is possible:
 *   cluster epoch > controller epoch  → start_epoch (preempts ANY phase — the on-chain
 *                                       `* → RECONCILE` recovery edge)
 *   RECONCILE      → reconcile_batch loops: quads (validator stake, transient stake, record,
 *                    vote) for consecutive validator-list indices from `reconcile_cursor`. Every
 *                    address is re-derived on-chain; this keeper derives the same ones (upstream
 *                    stake-pool PDA seeds) and creates NOTHING.
 *   FINALIZE       → finalize_pool (canonical totals + NAV snapshot; opens the preference window)
 *   PREFERENCES    → wait until `preference_window_close_slot`, then close_preference_window.
 *                    The keeper does NOT submit preference snapshots (snapshot_preference) —
 *                    frontends/indexers own that; an omitted position just stays neutral.
 *   PLAN-DIRECTED  → plan_directed_batch loops: (record, vote) pairs in canonical list order;
 *                    the batch that exhausts the list also carries the ADMISSION EXTRAS —
 *                    registered-but-unadmitted records discovered via getProgramAccounts (or the
 *                    validatorRecordsWatch fallback) — because the phase transitions the moment
 *                    the cursor reaches the list length and no later call can land.
 *   PLAN-NEUTRAL   → plan_neutral_batch loops: writable records for consecutive planned ordinals
 *                    from `neutral_cursor` (capacity rounds re-walk the same ordinals).
 *   PLAN-FINALIZE  → finalize_plan (conservation proof + the preferred-withdraw CPI — the
 *                    stake_pool/pool_authority/validator_list accounts it CPIs with are all
 *                    fixed struct accounts, no extra discovery needed).
 *   REBALANCE      → admission adds for planned Candidates without a list slot (cursor-
 *                    independent), then execute_next_action with EXACTLY the record the cursor
 *                    demands — the two-pass rotated walk is `rebalanceSlot` below, a faithful
 *                    port of logic::rebalance_slot / targets::rotation_start — and finally
 *                    finish_epoch once the walk completes or the churn budget is exhausted.
 *
 * Every leg is error-isolated: a failed transaction logs one ✗ line and the sweep retries next
 * tick (competing permissionless crankers racing us surface as benign WrongPhase/cursor errors).
 * Crank rewards (bounded fuSOL from the maintenance vault) land in the keeper wallet's fuSOL
 * ATA (auto-created at startup; override with `rewardAta`).
 *
 * USAGE
 *   ANCHOR_PROVIDER_URL=<rpc> ANCHOR_WALLET=~/.config/solana/id.json \
 *     npx ts-node keepers/stake-pool-crank.ts [config.json]
 *   No config arg → STAKE_CRANK_CONFIG env (the systemd path, same pattern as CRANK_CONFIG),
 *   else the built-in defaults below.
 */
import * as anchor from "@coral-xyz/anchor";
import * as fs from "fs";
import { PublicKey, Connection, Pk, pda, seed, bi, log, loadWallet, ensureAta, errLine, priorityIxs, priorityFeeMicroLamports, redactUrl, nonReentrant } from "./common";

/** The pinned stake-pool FORK program (constants.rs FUSION_STAKE_POOL_PROGRAM_ID). */
export const FUSION_STAKE_POOL_PROGRAM = new PublicKey("3pYHXui7Zk21TKE6oqivqbVJWRXt74wdDkqsnb3Q8mMi");

// --- controller constants mirrored from programs/fusion-stake-controller/src/constants.rs ----
/** Crank phases (state/epoch_state.rs PHASE_*). */
export const PHASE_IDLE = 0, PHASE_RECONCILE = 1, PHASE_FINALIZE = 2, PHASE_PREFERENCES = 3,
  PHASE_PLAN_DIRECTED = 4, PHASE_PLAN_NEUTRAL = 5, PHASE_PLAN_FINALIZE = 6, PHASE_REBALANCE = 7;
const PHASE_NAMES = ["IDLE", "RECONCILE", "FINALIZE", "PREFERENCES", "PLAN-DIRECTED", "PLAN-NEUTRAL", "PLAN-FINALIZE", "REBALANCE"];
/** finish_epoch's budget-exhaustion bound (constants.rs UPSTREAM_MINIMUM_DELEGATION). */
export const UPSTREAM_MINIMUM_DELEGATION = 1_000_000n;
/** Admission floor for the raw directed target (constants.rs MIN_ACTIVATION_TARGET_LAMPORTS). */
export const MIN_ACTIVATION_TARGET_LAMPORTS = 500_000_000_000n;
/** ValidatorRecord.validator_list_index sentinel: registered but not in the pool list. */
export const VALIDATOR_LIST_INDEX_UNSET = 0xffffffff;
const STATUS_CANDIDATE = 1; // fusion_stake_math::lifecycle::ValidatorStatus::Candidate

// Compute-budget headroom: the batch legs CPI/parse per remaining-accounts entry (reconcile's
// UpdateValidatorListBalance merges transients; plan-directed parses a vote account per pair) so
// they scale with batchSize and get near the transaction ceiling. The single-action legs
// (finalize_pool's two CPIs, finalize_plan's preferred-withdraw CPI, execute_next_action's one
// stake-pool CPI) fit comfortably under 600k. Trivial transitions ride the 200k default.
const CU_LIMIT_BATCH = 1_400_000;
const CU_LIMIT_ACTION = 600_000;

interface CrankCfg {
  tickSecs?: number; // sweep heartbeat (default 30 — the machine is epoch-paced; the tick only bounds reaction time to phase edges)
  batchSize?: number; // validator-list entries per reconcile tx (4-account quads, default 3). Plan legs scale it by their per-entry account cost: ×2 pairs (plan-directed), ×4 singles (plan-neutral) — same byte budget per tx.
  controllerProgramId?: string; // override the controller program id (default: the IDL's address)
  validatorRecordsWatch?: string[]; // VOTE accounts whose ValidatorRecords to track when the RPC has no getProgramAccounts (throttled/disabled/surfpool) — the scanPositions watch-list pattern
  rewardAta?: string; // crank-reward fuSOL token account (default: the wallet's fuSOL ATA, auto-created)
}
const DEFAULT_CFG: CrankCfg = {};

export function validateConfig(cfg: CrankCfg): void {
  const bail = (m: string): never => { throw new Error(`config: ${m}`); };
  if (cfg.tickSecs !== undefined && (typeof cfg.tickSecs !== "number" || !Number.isFinite(cfg.tickSecs) || cfg.tickSecs <= 0))
    bail(`tickSecs must be a positive number (got ${cfg.tickSecs})`);
  if (cfg.batchSize !== undefined && (!Number.isInteger(cfg.batchSize) || cfg.batchSize < 1 || cfg.batchSize > 3))
    bail(`batchSize must be an integer in 1..3 (got ${cfg.batchSize})`); // reconcile is the binding leg: 4 quads (16 keys on top of its 18 fixed) serialize to ~1253 bytes > the 1232-byte legacy tx limit
  for (const k of ["controllerProgramId", "rewardAta"] as const) {
    const v = cfg[k];
    if (v !== undefined) { try { new PublicKey(v); } catch { bail(`${k} is not a valid pubkey: ${v}`); } }
  }
  if (cfg.validatorRecordsWatch !== undefined) {
    if (!Array.isArray(cfg.validatorRecordsWatch)) bail("validatorRecordsWatch must be an array of vote-account pubkeys");
    cfg.validatorRecordsWatch.forEach((v, i) => { try { new PublicKey(v); } catch { bail(`validatorRecordsWatch[${i}] is not a valid pubkey: ${v}`); } });
  }
}

// ── pure helpers (unit-tested in stake-pool-crank.spec.ts) ─────────────────────────────────────

/** Port of fusion_stake_math::targets::rotation_start — the epoch-rotating walk start index. */
export const rotationStart = (epoch: bigint, n: bigint): bigint => (n === 0n ? 0n : epoch % n);

/** One slot of the deterministic rebalance walk — a faithful port of logic::rebalance_slot:
 * two full passes over the planned ordinals (pass 0 = Draining decreases/removals, pass 1 =
 * ordinary moves), each starting from the epoch-rotating index and wrapping. `null` once the
 * walk is complete (or nothing was planned). */
export function rebalanceSlot(cursor: bigint, plannedLen: bigint, epoch: bigint): { pass: number; index: bigint } | null {
  if (plannedLen === 0n || cursor >= plannedLen * 2n) return null;
  const pass = Number(cursor / plannedLen);
  const ordinal = cursor % plannedLen;
  const start = rotationStart(epoch, plannedLen);
  return { pass, index: ((start % plannedLen) + ordinal) % plannedLen };
}

/** The next contiguous batch a monotonic cursor demands: up to `batchSize` items of
 * `[cursor, total)`. `last` marks the batch that exhausts the range (count 0 when nothing —
 * or nothing MORE — remains, which for reconcile/plan legs is the empty transition call). */
export function batchSlice(cursor: bigint, total: bigint, batchSize: number): { start: bigint; count: number; last: boolean } {
  const remaining = total > cursor ? total - cursor : 0n;
  const count = Number(remaining < BigInt(batchSize) ? remaining : BigInt(batchSize));
  return { start: cursor, count, last: cursor + BigInt(count) >= total };
}

/** What the sweep reads each pass — EpochState + cluster clock, BigInt-normalized. */
export interface EpochView {
  clusterEpoch: bigint;
  controllerEpoch: bigint;
  phase: number;
  currentSlot: bigint;
  preferenceWindowCloseSlot: bigint;
  rebalanceCursor: bigint;
  plannedLen: bigint; // EpochState.plan_directed_cursor — the planned ordinal count once PLAN-DIRECTED completes
  churnBudgetRemaining: bigint;
}
export type CrankAction =
  | { kind: "start_epoch" }
  | { kind: "reconcile_batch" }
  | { kind: "finalize_pool" }
  | { kind: "wait_preference_window"; closeSlot: bigint }
  | { kind: "close_preference_window" }
  | { kind: "plan_directed_batch" }
  | { kind: "plan_neutral_batch" }
  | { kind: "finalize_plan" }
  | { kind: "execute_next_action" }
  | { kind: "finish_epoch" }
  | { kind: "idle" };

/** THE phase→action map. Epoch preemption first (start_epoch fires from ANY phase — including
 * a corrupt one — exactly like the on-chain `* → RECONCILE` edge); PREFERENCES gates on the
 * close slot; REBALANCE finishes when the walk is complete or the churn budget can no longer
 * fund one minimum action (finish_epoch's own completion proof). */
export function nextAction(v: EpochView): CrankAction {
  if (v.clusterEpoch > v.controllerEpoch) return { kind: "start_epoch" };
  switch (v.phase) {
    case PHASE_RECONCILE: return { kind: "reconcile_batch" };
    case PHASE_FINALIZE: return { kind: "finalize_pool" };
    case PHASE_PREFERENCES:
      return v.currentSlot >= v.preferenceWindowCloseSlot
        ? { kind: "close_preference_window" }
        : { kind: "wait_preference_window", closeSlot: v.preferenceWindowCloseSlot };
    case PHASE_PLAN_DIRECTED: return { kind: "plan_directed_batch" };
    case PHASE_PLAN_NEUTRAL: return { kind: "plan_neutral_batch" };
    case PHASE_PLAN_FINALIZE: return { kind: "finalize_plan" };
    case PHASE_REBALANCE:
      return v.rebalanceCursor >= v.plannedLen * 2n || v.churnBudgetRemaining < UPSTREAM_MINIMUM_DELEGATION
        ? { kind: "finish_epoch" }
        : { kind: "execute_next_action" };
    default: return { kind: "idle" }; // IDLE, or a corrupt phase only start_epoch can recover
  }
}

/** One parsed ValidatorList entry (fusion-stake-view validator_list.rs layout). */
export interface ValidatorListEntry {
  voteAccount: Pk;
  activeStakeLamports: bigint;
  transientStakeLamports: bigint;
  lastUpdateEpoch: bigint;
  transientSeedSuffix: bigint;
  validatorSeedSuffix: number;
  status: number;
}
/** Parse the stake pool's ValidatorList account: header `{account_type u8 == 2,
 * max_validators u32, vec len u32}` then 73-byte borsh entries; trailing pre-allocated
 * capacity bytes are expected and ignored; a `len` past the buffer fails closed. */
export function parseValidatorList(data: Buffer): { maxValidators: number; entries: ValidatorListEntry[] } {
  if (data.length < 9) throw new Error("validator list: shorter than the 9-byte header");
  if (data[0] !== 2) throw new Error(`validator list: account_type ${data[0]} (expected 2)`);
  const maxValidators = data.readUInt32LE(1);
  const len = data.readUInt32LE(5);
  if (9 + len * 73 > data.length) throw new Error(`validator list: len ${len} exceeds the buffer`);
  const entries: ValidatorListEntry[] = [];
  for (let i = 0; i < len; i++) {
    const o = 9 + i * 73;
    entries.push({
      activeStakeLamports: data.readBigUInt64LE(o),
      transientStakeLamports: data.readBigUInt64LE(o + 8),
      lastUpdateEpoch: data.readBigUInt64LE(o + 16),
      transientSeedSuffix: data.readBigUInt64LE(o + 24),
      validatorSeedSuffix: data.readUInt32LE(o + 36), // +32 is upstream's unused u32
      status: data[o + 40],
      voteAccount: new PublicKey(data.subarray(o + 41, o + 73)),
    });
  }
  return { maxValidators, entries };
}

/** The validator-stake PDA under the FORK: seeds `[vote, pool]` + u32 LE seed ONLY when
 * nonzero (spl_cpi::derive_validator_stake). */
export function validatorStakeAddress(vote: Pk, pool: Pk, seedSuffix: number): Pk {
  const seeds: Buffer[] = [vote.toBuffer(), pool.toBuffer()];
  if (seedSuffix !== 0) { const b = Buffer.alloc(4); b.writeUInt32LE(seedSuffix); seeds.push(b); }
  return PublicKey.findProgramAddressSync(seeds, FUSION_STAKE_POOL_PROGRAM)[0];
}
/** The transient-stake PDA: seeds `[b"transient", vote, pool, u64 LE seed]` — the u64 suffix
 * is ALWAYS present, unlike the validator-stake u32 (spl_cpi::derive_transient_stake). */
export function transientStakeAddress(vote: Pk, pool: Pk, seedSuffix: bigint): Pk {
  const b = Buffer.alloc(8); b.writeBigUInt64LE(seedSuffix);
  return PublicKey.findProgramAddressSync([Buffer.from("transient"), vote.toBuffer(), pool.toBuffer(), b], FUSION_STAKE_POOL_PROGRAM)[0];
}
const validatorRecordAddress = (pid: Pk, vote: Pk): Pk => pda([seed("validator"), vote], pid);

/** A scanned ValidatorRecord (BigInt-normalized), for the admission discovery paths. */
export interface RecordView {
  vote: Pk; listIndex: number; status: number; planEpoch: bigint;
  observedEpoch: bigint; observedOk: boolean; directedSharesEpoch: bigint; directedShares: bigint;
}
/** PLAN-DIRECTED admission-extra filter: not in the pool list, not yet planned this epoch, and
 * plausibly admittable (already a Candidate needing a re-plan for its add, or carrying
 * current-epoch directed shares that could clear the activation floor). Extras are fail-safe —
 * omitting one only delays that validator's admission to a later epoch. */
export function isAdmissionExtra(r: RecordView, controllerEpoch: bigint): boolean {
  return r.listIndex === VALIDATOR_LIST_INDEX_UNSET && r.planEpoch !== controllerEpoch &&
    (r.status === STATUS_CANDIDATE || r.directedSharesEpoch === controllerEpoch);
}
/** REBALANCE admission-add filter — mirrors execute_next_action's admission-mode requires
 * exactly (planned Candidate, no list slot, healthy current observation, raw directed target
 * `floor(P·d/S)` at/above the activation minimum) so we never submit a doomed add. */
export function isAdmissionAddDue(r: RecordView, controllerEpoch: bigint, productive: bigint, supply: bigint): boolean {
  if (r.listIndex !== VALIDATOR_LIST_INDEX_UNSET || r.status !== STATUS_CANDIDATE) return false;
  if (r.planEpoch !== controllerEpoch || r.observedEpoch !== controllerEpoch || !r.observedOk) return false;
  if (supply === 0n) return false;
  const shares = r.directedSharesEpoch === controllerEpoch ? r.directedShares : 0n;
  return (productive * shares) / supply >= MIN_ACTIVATION_TARGET_LAMPORTS;
}

// ── crank context + on-chain reads ─────────────────────────────────────────────────────────────
interface Ctx {
  program: any; conn: anchor.web3.Connection; pid: Pk;
  epochState: Pk; stakePool: Pk; validatorList: Pk; reserveStake: Pk;
  poolWithdrawAuthority: Pk; fusolMint: Pk; maintenanceVault: Pk; rewardAta: Pk;
  batchSize: number; watch: Pk[]; // vote accounts (validatorRecordsWatch)
}

/** The controller IDL: a local anchor build (target/idl) wins; a build-less checkout falls back
 * to the committed production copy in sdk/src/idl (kept current via `yarn --cwd sdk sync-idl`). */
function loadControllerIdl(): anchor.Idl {
  for (const p of [`${__dirname}/../target/idl/fusion_stake_controller.json`, `${__dirname}/../sdk/src/idl/fusion_stake_controller.json`]) {
    if (fs.existsSync(p)) return JSON.parse(fs.readFileSync(p, "utf8"));
  }
  throw new Error("no controller IDL found — run `anchor build` or restore sdk/src/idl/fusion_stake_controller.json");
}

async function readState(c: Ctx): Promise<{ es: any; view: EpochView }> {
  const [es, info] = await Promise.all([c.program.account.epochState.fetch(c.epochState), c.conn.getEpochInfo()]);
  const view: EpochView = {
    clusterEpoch: BigInt(info.epoch),
    controllerEpoch: bi(es.controllerEpoch),
    phase: Number(es.phase),
    currentSlot: BigInt(info.absoluteSlot),
    preferenceWindowCloseSlot: bi(es.preferenceWindowCloseSlot),
    rebalanceCursor: bi(es.rebalanceCursor),
    plannedLen: bi(es.planDirectedCursor),
    churnBudgetRemaining: bi(es.churnBudgetTotal) > bi(es.churnBudgetUsed) ? bi(es.churnBudgetTotal) - bi(es.churnBudgetUsed) : 0n,
  };
  return { es, view };
}

async function fetchValidatorList(c: Ctx): Promise<{ maxValidators: number; entries: ValidatorListEntry[] }> {
  const info = await c.conn.getAccountInfo(c.validatorList);
  if (!info) throw new Error(`validator list ${c.validatorList.toBase58()} does not exist`);
  return parseValidatorList(info.data);
}

/** All ValidatorRecords: the watch-list (vote accounts → record PDAs fetched directly) when
 * configured, else getProgramAccounts — which throttled/gPA-less RPCs (most providers, surfpool)
 * reject, so a failure degrades to "no records discovered" (admission waits, nothing breaks). */
async function scanValidatorRecords(c: Ctx): Promise<RecordView[]> {
  const toView = (a: any): RecordView => ({
    vote: a.voteAccount, listIndex: Number(a.validatorListIndex), status: Number(a.status),
    planEpoch: bi(a.planEpoch), observedEpoch: bi(a.observedEpoch),
    observedOk: Boolean(a.observedCommissionOk) && Boolean(a.observedLivenessOk),
    directedSharesEpoch: bi(a.directedSharesEpoch), directedShares: bi(a.directedShares),
  });
  try {
    if (c.watch.length) {
      const fetched = await Promise.all(c.watch.map((vote) => c.program.account.validatorRecord.fetchNullable(validatorRecordAddress(c.pid, vote))));
      return fetched.filter(Boolean).map(toView);
    }
    const all = await c.program.account.validatorRecord.all();
    return all.map((a: any) => toView(a.account));
  } catch (e: any) {
    log(`  · validator-record scan failed (gPA-less RPC? configure validatorRecordsWatch): ${errLine(e)}`);
    return []; // fail-safe: admission extras/adds wait for a scan-capable pass or the next epoch
  }
}

// ── legs ───────────────────────────────────────────────────────────────────────────────────────

/** Send one leg transaction; a failure logs ✗ and stops the sweep (retried next tick). */
async function submit(label: string, build: () => any): Promise<boolean> {
  try {
    const sig = await build().rpc();
    log(`  ✓ ${label} (${sig.slice(0, 16)}…)`);
    return true;
  } catch (e: any) {
    log(`  ✗ ${label}: ${errLine(e)}`);
    return false;
  }
}

const meta = (pubkey: Pk, isWritable: boolean): anchor.web3.AccountMeta => ({ pubkey, isSigner: false, isWritable });

/** The (validator stake, transient stake, record, vote) quad reconcile_batch demands per entry. */
function reconcileQuad(c: Ctx, e: ValidatorListEntry): anchor.web3.AccountMeta[] {
  return [
    meta(validatorStakeAddress(e.voteAccount, c.stakePool, e.validatorSeedSuffix), true),
    meta(transientStakeAddress(e.voteAccount, c.stakePool, e.transientSeedSuffix), true),
    meta(validatorRecordAddress(c.pid, e.voteAccount), true),
    meta(e.voteAccount, false),
  ];
}

/** RECONCILE: batch quads until the cursor covers the list (the covering batch — possibly the
 * empty one on an empty list — transitions the phase on-chain). */
async function reconcileLeg(c: Ctx): Promise<boolean> {
  for (;;) {
    const { es, view } = await readState(c);
    if (view.phase !== PHASE_RECONCILE || view.clusterEpoch > view.controllerEpoch) return true;
    const list = await fetchValidatorList(c);
    const s = batchSlice(bi(es.reconcileCursor), BigInt(list.entries.length), c.batchSize);
    const remaining = list.entries.slice(Number(s.start), Number(s.start) + s.count).flatMap((e) => reconcileQuad(c, e));
    const ok = await submit(`reconcile_batch [${s.start}, ${s.start + BigInt(s.count)})/${list.entries.length}`, () =>
      c.program.methods.reconcileBatch().accounts({
        stakePool: c.stakePool, poolWithdrawAuthority: c.poolWithdrawAuthority, validatorList: c.validatorList,
        reserveStake: c.reserveStake, maintenanceVault: c.maintenanceVault, crankRewardAccount: c.rewardAta,
      }).remainingAccounts(remaining).preInstructions(priorityIxs(CU_LIMIT_BATCH)));
    if (!ok) return false;
  }
}

/** PLAN-DIRECTED: (record, vote) pairs in canonical list order; the batch that exhausts the
 * list also carries the admission extras (the phase transitions the moment the cursor reaches
 * the list length, so extras can never ride a later call). */
async function planDirectedLeg(c: Ctx): Promise<boolean> {
  for (;;) {
    const { es, view } = await readState(c);
    if (view.phase !== PHASE_PLAN_DIRECTED || view.clusterEpoch > view.controllerEpoch) return true;
    const list = await fetchValidatorList(c);
    const pairBudget = c.batchSize * 2; // TOTAL (list + extra) pairs per tx — the reconcile-quad byte budget
    let s = batchSlice(bi(es.planDirectedCursor), BigInt(list.entries.length), pairBudget);
    let extras: RecordView[] = [];
    if (s.last) {
      extras = (await scanValidatorRecords(c)).filter((r) => isAdmissionExtra(r, view.controllerEpoch));
      if (extras.length > 0 && s.count === pairBudget) {
        // A FULL covering slice leaves no room for extras (and extras can only ride the covering
        // call). Submit it one pair short — the next, smaller covering batch carries the extras.
        s = { start: s.start, count: s.count - 1, last: false };
        extras = [];
      }
    }
    const remaining = list.entries.slice(Number(s.start), Number(s.start) + s.count)
      .flatMap((e) => [meta(validatorRecordAddress(c.pid, e.voteAccount), true), meta(e.voteAccount, false)]);
    const taken = extras.slice(0, pairBudget - s.count); // keep the covering tx inside the account budget
    if (extras.length > taken.length) log(`  · ${extras.length - taken.length} admission extras deferred to next epoch (batch full)`);
    for (const r of taken) remaining.push(meta(validatorRecordAddress(c.pid, r.vote), true), meta(r.vote, false));
    const extraCount = taken.length;
    const ok = await submit(`plan_directed_batch [${s.start}, ${s.start + BigInt(s.count)})/${list.entries.length}${extraCount ? ` +${extraCount} extras` : ""}`, () =>
      c.program.methods.planDirectedBatch().accounts({
        validatorList: c.validatorList, maintenanceVault: c.maintenanceVault, crankRewardAccount: c.rewardAta,
      }).remainingAccounts(remaining).preInstructions(priorityIxs(CU_LIMIT_BATCH)));
    if (!ok) return false;
  }
}

/** PLAN-NEUTRAL: writable records for consecutive planned ordinals from `neutral_cursor`
 * (which resets to 0 at each capacity-round start — the loop just follows it). */
async function planNeutralLeg(c: Ctx): Promise<boolean> {
  for (;;) {
    const { es, view } = await readState(c);
    if (view.phase !== PHASE_PLAN_NEUTRAL || view.clusterEpoch > view.controllerEpoch) return true;
    const list = await fetchValidatorList(c);
    const s = batchSlice(bi(es.neutralCursor), view.plannedLen, c.batchSize * 4);
    const remaining = list.entries.slice(Number(s.start), Number(s.start) + s.count)
      .map((e) => meta(validatorRecordAddress(c.pid, e.voteAccount), true));
    const ok = await submit(`plan_neutral_batch round ${es.neutralRoundNumber} [${s.start}, ${s.start + BigInt(s.count)})/${view.plannedLen}`, () =>
      c.program.methods.planNeutralBatch().accounts({
        maintenanceVault: c.maintenanceVault, crankRewardAccount: c.rewardAta,
      }).remainingAccounts(remaining).preInstructions(priorityIxs(CU_LIMIT_BATCH)));
    if (!ok) return false;
  }
}

/** Cursor-independent admission adds — planned Candidates without a list slot. Runs before the
 * walk AND before finish_epoch: the walk can already be complete (empty plan — the genesis /
 * first-validator case, an exhausted budget, or a competing cranker) while adds are still due,
 * and finishing without them would defer admission epoch after epoch. Failures are isolated per
 * add (a full list / rent-poor reserve fails cleanly; the on-chain design calls the deferral
 * fail-safe), so the sweep proceeds to finish_epoch regardless. */
async function admissionAddsLeg(c: Ctx): Promise<void> {
  const { es, view } = await readState(c);
  if (view.phase !== PHASE_REBALANCE) return;
  const adds = (await scanValidatorRecords(c))
    .filter((r) => isAdmissionAddDue(r, view.controllerEpoch, bi(es.productiveLamports), bi(es.navFusolSupply)));
  for (const r of adds) {
    // The new entry always uses seed 0 (execute_next_action admission mode); the transient is
    // an unused rider for adds.
    await submit(`add_validator ${r.vote.toBase58().slice(0, 6)}…`, () =>
      executeNextActionBuilder(c, r.vote, validatorStakeAddress(r.vote, c.stakePool, 0), transientStakeAddress(r.vote, c.stakePool, 0n)));
  }
}

/** REBALANCE: cursor-independent admission adds first, then execute_next_action with exactly
 * the record the walk cursor demands, until the walk (or budget) says finish_epoch. */
async function rebalanceLeg(c: Ctx): Promise<boolean> {
  await admissionAddsLeg(c);
  for (;;) {
    const { es, view } = await readState(c);
    if (view.phase !== PHASE_REBALANCE || view.clusterEpoch > view.controllerEpoch) return true;
    if (nextAction(view).kind !== "execute_next_action") return true; // finish_epoch — the sweep submits it
    const slot = rebalanceSlot(view.rebalanceCursor, view.plannedLen, view.controllerEpoch)!;
    const list = await fetchValidatorList(c);
    const e = list.entries[Number(slot.index)];
    if (!e) { log(`  ✗ rebalance: walk index ${slot.index} beyond the live list (${list.entries.length})`); return false; }
    const ok = await submit(`execute_next_action cursor ${es.rebalanceCursor} pass ${slot.pass} idx ${slot.index} (${e.voteAccount.toBase58().slice(0, 6)}…)`, () =>
      executeNextActionBuilder(c, e.voteAccount,
        validatorStakeAddress(e.voteAccount, c.stakePool, e.validatorSeedSuffix),
        transientStakeAddress(e.voteAccount, c.stakePool, e.transientSeedSuffix)));
    if (!ok) return false;
  }
}

function executeNextActionBuilder(c: Ctx, vote: Pk, validatorStake: Pk, transientStake: Pk): any {
  return c.program.methods.executeNextAction().accounts({
    stakePool: c.stakePool, poolWithdrawAuthority: c.poolWithdrawAuthority, validatorList: c.validatorList,
    reserveStake: c.reserveStake, voteAccount: vote, validatorRecord: validatorRecordAddress(c.pid, vote),
    validatorStakeAccount: validatorStake, transientStakeAccount: transientStake,
    maintenanceVault: c.maintenanceVault, crankRewardAccount: c.rewardAta,
  }).preInstructions(priorityIxs(CU_LIMIT_ACTION));
}

/** One sweep: chain legs while the machine can advance (bounded — a full cycle is at most
 * ~10 legs; the cap keeps a logic bug or a racing crank from spinning inside one tick). */
async function sweep(c: Ctx): Promise<void> {
  for (let leg = 0; leg < 12; leg++) {
    const { view } = await readState(c);
    const action = nextAction(view);
    let progressed = false;
    switch (action.kind) {
      case "idle":
        return;
      case "wait_preference_window":
        log(`  · ${PHASE_NAMES[view.phase]}: window closes at slot ${action.closeSlot} (now ${view.currentSlot})`);
        return;
      case "start_epoch":
        progressed = await submit(`start_epoch → ${view.clusterEpoch} (from ${PHASE_NAMES[view.phase] ?? view.phase})`, () =>
          c.program.methods.startEpoch().accounts({}).preInstructions(priorityIxs()));
        break;
      case "reconcile_batch": progressed = await reconcileLeg(c); break;
      case "finalize_pool":
        progressed = await submit("finalize_pool", () =>
          c.program.methods.finalizePool().accounts({
            stakePool: c.stakePool, poolWithdrawAuthority: c.poolWithdrawAuthority, validatorList: c.validatorList,
            reserveStake: c.reserveStake, fusolMint: c.fusolMint, maintenanceVault: c.maintenanceVault,
            crankRewardAccount: c.rewardAta,
          }).preInstructions(priorityIxs(CU_LIMIT_ACTION)));
        break;
      case "close_preference_window":
        progressed = await submit("close_preference_window", () =>
          c.program.methods.closePreferenceWindow().accounts({}).preInstructions(priorityIxs()));
        break;
      case "plan_directed_batch": progressed = await planDirectedLeg(c); break;
      case "plan_neutral_batch": progressed = await planNeutralLeg(c); break;
      case "finalize_plan":
        progressed = await submit("finalize_plan", () =>
          c.program.methods.finalizePlan().accounts({
            stakePool: c.stakePool, validatorList: c.validatorList,
            maintenanceVault: c.maintenanceVault, crankRewardAccount: c.rewardAta,
          }).preInstructions(priorityIxs(CU_LIMIT_ACTION)));
        break;
      case "execute_next_action": progressed = await rebalanceLeg(c); break;
      case "finish_epoch":
        // Pending admission adds are still legal (and due) even when the walk is complete —
        // notably the empty-plan genesis epoch, where finish_epoch is due IMMEDIATELY.
        await admissionAddsLeg(c);
        progressed = await submit(`finish_epoch ${view.controllerEpoch}`, () =>
          c.program.methods.finishEpoch().accounts({
            maintenanceVault: c.maintenanceVault, crankRewardAccount: c.rewardAta,
          }).preInstructions(priorityIxs()));
        break;
    }
    if (!progressed) return;
  }
}

async function main() {
  const cfgPath = process.argv[2] || process.env.STAKE_CRANK_CONFIG; // env: config without touching the systemd unit
  const cfg: CrankCfg = cfgPath ? JSON.parse(fs.readFileSync(cfgPath, "utf8")) : DEFAULT_CFG;
  validateConfig(cfg);

  const url = process.env.ANCHOR_PROVIDER_URL || "http://127.0.0.1:8899";
  const wallet = loadWallet();
  const provider = new anchor.AnchorProvider(new Connection(url, "confirmed"), wallet, { commitment: "confirmed" });
  anchor.setProvider(provider);
  const idl: any = loadControllerIdl();
  if (cfg.controllerProgramId) idl.address = cfg.controllerProgramId;
  const program: any = new anchor.Program(idl, provider);
  const pid: Pk = program.programId;
  const me: Pk = wallet.publicKey;
  log(`stake-pool-crank up — controller ${pid.toBase58()}, wallet ${me.toBase58()}, RPC ${redactUrl(url)}, priority ${priorityFeeMicroLamports()}µlam/CU`);

  // The immutable address book: every pool-side account the legs pass is recorded on-chain.
  const cc: any = await program.account.controllerConfig.fetch(pda([seed("controller")], pid));
  if (!cc.sealed) throw new Error("controller not sealed — run initialize_pool before cranking");
  // Crank rewards are fuSOL from the maintenance vault; default them into our own fuSOL ATA.
  const rewardAta = cfg.rewardAta ? new PublicKey(cfg.rewardAta) : await ensureAta(provider, cc.fusolMint, me, priorityIxs());

  const ctx: Ctx = {
    program, conn: provider.connection, pid,
    epochState: pda([seed("epoch_state")], pid),
    stakePool: cc.stakePool, validatorList: cc.validatorList, reserveStake: cc.reserveStake,
    poolWithdrawAuthority: cc.poolWithdrawAuthority, fusolMint: cc.fusolMint,
    maintenanceVault: cc.maintenanceVault, rewardAta,
    batchSize: cfg.batchSize ?? 3,
    watch: (cfg.validatorRecordsWatch ?? []).map((v) => new PublicKey(v)),
  };
  const tickSecs = cfg.tickSecs ?? 30;
  log(`pool ${ctx.stakePool.toBase58().slice(0, 6)}…, list ${ctx.validatorList.toBase58().slice(0, 6)}…, reward → ${rewardAta.toBase58().slice(0, 6)}… | tick ${tickSecs}s, batch ${ctx.batchSize}${ctx.watch.length ? `, watch ${ctx.watch.length}` : ""}`);

  // Skip a tick if the previous sweep is still running (slow RPC ⇒ overlapping sweeps would double-submit).
  const run = nonReentrant(async () => { try { await sweep(ctx); } catch (e: any) { log(`✗ sweep: ${errLine(e)}`); } });
  await run();
  setInterval(run, tickSecs * 1000);
}

if (require.main === module) {
  main().catch((e) => { console.error(e); process.exit(1); });
}
