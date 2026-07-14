# keepers

Permissionless off-chain bots. Fusion never relies on a keeper whitelist — these are
reference implementations that anyone can run, kept profitable by liquidation bonuses,
redemption fees, and small crank rewards (fusion-docs.md).

`keeper.ts` (TS) — the **MVP crank loop**, the three cranks a market needs to stay usable, one
process, config-driven, each on its own interval with per-tick error isolation:
- **twap-sampler** — `sample_twap` an Orca/Raydium pool into the per-market `DexTwap` ring.
- **oracle-poster** — `update_price` (re-aggregate into `Market.spot`). Two Pyth modes per market:
  `persistent` (read a continuously-updated `PriceUpdateV2` account — anchor-only, node 18 OK) or
  `post` (Hermes-fetch + post via `@pythnetwork/pyth-solana-receiver` in the same tx — cluster-agnostic,
  needs **node ≥ 20**). Switchboard is read-through/optional (cranking it fresh is a follow-on).
- **refresher** — `refresh_market` to fold the interest accumulator + mint it into the buffer.

```sh
ANCHOR_PROVIDER_URL=<rpc> ANCHOR_WALLET=~/.config/solana/id.json \
  npx ts-node keepers/keeper.ts [config.json]   # no arg → built-in WSOL/USDC fork defaults
```
> Run against a **surfpool mainnet-fork** (or a cluster where the Pyth/Switchboard/Orca accounts
> exist + stay fresh). For `borrow` to be ENABLED the aggregate must be `Ok` — Pyth + a present/fresh
> Switchboard + a satisfied TWAP corridor (≥ `twap_min_samples` over `twap_window_secs`); a missing or
> stale secondary freezes mints by design (repay/liquidate/redeem stay open).

Still **planned**:
- **liquidator** — scan positions, liquidate any below MCR (RP offset → redistribution).
- **redeemer** — arbitrage the peg floor; supply lowest-rate-bucket members for `redeem`.

## stake-pool crank (`stake-pool-crank.ts`)

Drives the fuSOL Allocation Controller's permissionless epoch state machine
(`programs/fusion-stake-controller`) around its cycle — IDLE → RECONCILE → FINALIZE →
PREFERENCES → PLAN-DIRECTED → PLAN-NEUTRAL → PLAN-FINALIZE → REBALANCE → IDLE — one `tickSecs`
heartbeat, one non-reentrant sweep, every leg error-isolated (a failed transaction logs one `✗`
line and retries next tick). Anyone can run it; the on-chain program pays bounded fuSOL crank
rewards from the maintenance vault into the keeper wallet's fuSOL ATA (auto-created; override
with `rewardAta`).

```sh
ANCHOR_PROVIDER_URL=<rpc> ANCHOR_WALLET=~/.config/solana/id.json \
  npx ts-node keepers/stake-pool-crank.ts [config.json]
# no arg → STAKE_CRANK_CONFIG env (the systemd path), else built-in defaults
```

Config (all optional): `tickSecs` (default 30), `batchSize` (validator-list entries per
reconcile transaction, default 3 — the max whose 4-account quads fit a legacy transaction;
the plan legs scale it by their per-entry account cost), `controllerProgramId`,
`validatorRecordsWatch`, `rewardAta`.

What each sweep does, per the on-chain phase:
- cluster epoch ahead of the controller → `start_epoch` (preempts any phase — the program's
  `* → RECONCILE` recovery edge).
- **RECONCILE** — `reconcile_batch` loops of (validator stake, transient stake, record, vote)
  quads in validator-list order; all addresses derived, nothing created.
- **FINALIZE** — `finalize_pool` (canonical totals + NAV snapshot; opens the preference window).
- **PREFERENCES** — waits for the close slot, then `close_preference_window`. The keeper does
  **not** submit preference snapshots (`snapshot_preference`) — frontends/indexers own that;
  an omitted position simply stays in neutral allocation for the epoch (never a loss).
- **PLAN-DIRECTED / PLAN-NEUTRAL / PLAN-FINALIZE** — the record batch loops, with the
  list-exhausting plan-directed batch also carrying admission extras (see discovery below),
  then `finalize_plan`.
- **REBALANCE** — admission adds for planned Candidates without a list slot, then
  `execute_next_action` following the controller's deterministic two-pass rotated walk
  (the keeper ports `logic::rebalance_slot` and supplies exactly the record the cursor
  demands), and `finish_epoch` once the walk completes or the churn budget is spent.

**Account discovery**: validator-list members come from the list itself; the
registered-but-unadmitted `ValidatorRecord`s (admission extras/adds) are found via
`getProgramAccounts`. RPCs that throttle or disable gPA (most providers, surfpool) make that
scan fail — configure `validatorRecordsWatch` (an array of **vote account** addresses; their
record PDAs are fetched directly, the `scanPositions` watch-list pattern) and admission keeps
working. Without either, admission extras are skipped fail-safe: a validator's admission is
delayed to a later epoch, nothing else is affected.
