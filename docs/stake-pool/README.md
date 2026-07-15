# fuSOL — the Fusion Native Stake Share Pool

fuSOL is productive, natively staked SOL as Fusion collateral, without outsourcing validator
selection to an LST operator. Users deposit SOL (or fully active native stake) into a dedicated
stake pool and receive a non-rebasing legacy SPL share token that appreciates against SOL as the
underlying stake earns rewards. fuSOL is onboarded into fusd-core as an ordinary isolated
collateral market. Validator direction is optional and expressed by Fusion collateral owners; all
backing above a small operational reserve is delegated by fixed directed-or-neutral rules — no
manager discretion anywhere.

**Design posture: credible neutrality through constrained code.** The pool has no human manager,
no arbitrary validator-selection authority, no upgrade path after launch sealing, no freeze
authority, and no generic CPI executor.

## Components

| Component | Where | Role |
|---|---|---|
| Stake pool (custody + share accounting) | `vendor/spl-stake-pool` | Pinned, upstream-compatible SPL Stake Pool fork under a Fusion-specific program ID (`3pYHXui7Zk21TKE6oqivqbVJWRXt74wdDkqsnb3Q8mMi`). The complete fork diff is TWO `declare_id` lines — see [`vendor/spl-stake-pool/UPSTREAM.md`](../../vendor/spl-stake-pool/UPSTREAM.md), the audit pin manifest. |
| Allocation Controller | `programs/fusion-stake-controller` | An immutable Anchor program (`Fz3z1yh21PQ59smsPjmjeyK6ngh8KoK6PiPxUgCgspFq`) holding the pool's manager/staker/deposit authorities as PDAs. It can sign exactly an 11-instruction byte-pinned CPI allowlist; `SetFee`/`SetManager`/`SetStaker`/`SetFundingAuthority`/withdraw/metadata builders do not exist in the binary. |
| Pure math | `crates/fusion-stake-math` | Deterministic target allocation (directed floors + equal-capacity neutral rounds as an incremental fold), churn caps, hysteresis, validator lifecycle transitions, the preference-countability predicate, reward bounds. Property-tested and Kani-verified. |
| Read-only parsers | `crates/fusion-stake-view` | Bounds-checked byte parsers for `StakePool`, `ValidatorList`, `VoteState` (fail-closed on unknown versions), and the fusd-core `Position` (layout pinned by a round-trip test). |
| fusd-core integration | `programs/fusd-core` | Two additions: `Position.ink_nonce` (a monotonic collateral-change nonce the controller reads to invalidate stale validator direction) and the **canonical-primary oracle mode** (fuSOL priced as `sol_usd × pool_rate`; see `docs/fusd-spec-v1.md`, MarketOracle + `update_price`). |
| Ops | `keepers/stake-pool-crank.ts`, `scripts/bootstrap-fusol.ts`, `scripts/fetch-spl-stake-pool.sh` | Permissionless reference crank, idempotent genesis orchestrator, and the test-fixture fetcher. |

## Authority graph (the launch-verifiable core)

```
Stake-pool manager authority  = Controller PDA ["pool_authority"]
Stake-pool staker authority   = Controller PDA ["pool_authority"]
SOL + stake deposit authority = Controller PDA ["deposit_authority"]
Stake-pool withdraw authority = Stake Pool program PDA [pool, "withdraw"]
fuSOL mint authority          = the pool withdraw-authority PDA
fuSOL freeze authority        = None
Manager fee account           = the maintenance vault (token authority = Controller PDA ["maintenance"])
Program upgrade authorities   = None after launch sealing
```

Withdrawals (stake or SOL) go DIRECTLY to the stake-pool program and are never authority-gated —
the guaranteed exit. Deposits route through the controller (the deposit-authority PDA co-signs).
Fees are fixed at `initialize_pool` (5 bps on each deposit/withdrawal flavor, 1% of positive
epoch rewards, referral 0) and there is no fee setter.

## How allocation works

Each epoch, permissionless cranks drive a state machine over a single `EpochState` write lane:

```
IDLE → RECONCILE → FINALIZE → PREFERENCES → PLAN-DIRECTED → PLAN-NEUTRAL → PLAN-FINALIZE → REBALANCE → IDLE
```

- **Reconcile** batches `UpdateValidatorListBalance` over the validator list (accounts re-derived
  on-chain — a wrong or duplicate account fails without advancing the cursor) and records
  commission/liveness observations per validator.
- **Finalize** runs `UpdateStakePoolBalance` + cleanup, snapshots NAV and supply, detects
  negative-NAV (emitted as an event; the fusd-core oracle independently picks the lower rate up
  on its next crank), computes the reserve target (2% of pool, min 10 SOL) and the productive
  balance, and opens the preference window (1/32 of an epoch).
- **Preferences**: a fuSOL-backed Fusion position may direct its collateral-weight at ONE
  eligible validator. Countability requires the position's `ink_nonce` to match the preference's
  observed nonce (any collateral change invalidates direction until re-synced, with a one-epoch
  delay — this is what makes fungible-share direction reuse impossible), and each preference
  counts at most once per epoch. Direction is optional; uncounted supply is neutral backing.
- **Plan**: directed targets are `floor(productive × shares / supply)` clipped by lifecycle caps
  (2% Active / 0.25% Candidate of the pool per vote account); everything else — undirected
  supply, cap clippings, stale or omitted preferences — is distributed EQUALLY across Active
  validators with remaining capacity, in deterministic capacity rounds with an epoch-rotating
  remainder. The plan is rejected if directed shares exceed supply, and `finalize_plan` proves
  conservation on-chain: `directed + neutral grants + recorded shortfall == productive`, exactly.
- **Rebalance**: a monotonic, epoch-rotated two-pass cursor — pass 0 executes only draining
  validators' decreases/removals ("drains first" preserved globally), pass 1 the ordinary
  deficit/surplus moves, each pass walking the planned validator ordinals from an
  epoch-rotating start index. Each action amount is capped by hysteresis (max(50 SOL, 5 bps)),
  a 3% global churn budget, and a 0.5% per-validator move cap. The caller supplies accounts,
  never choices — the record must sit at exactly the cursor's index or the call fails without
  advancing. **Recorded spec deviation:** the spec orders ordinary moves by per-epoch global
  greatest-deficit/surplus first. Verifying a global maximum on-chain requires the full record
  set in one transaction — infeasible at 1024 validators — and any subset-based selection would
  let a caller steer the choice by omission. Cursor order is a deterministic, batchable,
  non-steerable approximation of that intent; it does NOT execute greatest-deficit-first.

Validator admission is objective: any vote account can be registered; explicit directed support
of at least 500 SOL admits it as a Candidate (directed stake only, 0.25% cap); two consecutive
healthy epochs while staked promote it to Active (neutral allocation eligible). A commission
breach (>10%) drains immediately; two consecutive liveness failures drain unless fewer than half
of delegated stake passes the health check (the systemic-event guard). There is no allowlist, no
denylist, and no identity registry.

Cranks pay bounded fuSOL rewards from the maintenance vault (fee-funded): fixed amounts per task
class, per-epoch budget cap, zero for no-ops, and an empty vault never blocks a crank.

## Failure posture

- Missing or stale preferences never idle capital — the backing simply joins neutral allocation.
- If nobody cranks: delegated stake keeps earning, fuSOL keeps transferring; the exchange rate
  goes stale, so fusd-core freezes NEW borrowing (canonical-primary mode withholds price commits
  when the pool's finalization lags more than 2 epochs) while repay, redemption, and liquidation
  continue on the last conservative price under the normal staleness breakers. Any caller can
  restore operation; a stranded mid-cycle epoch is preempted by the next `start_epoch`.
- Fusion debt paths never touch the controller: no fusd-core instruction CPIs into it, and the
  controller only ever READS Position bytes. A controller failure cannot block repayment,
  redemption, or liquidation.

## Genesis + operations

```
# one-time genesis (payer must be the controller's upgrade authority pre-sealing)
ANCHOR_PROVIDER_URL=<rpc> npx ts-node scripts/bootstrap-fusol.ts [config.json]

# the permissionless crank (any wallet; earns fuSOL rewards)
STAKE_CRANK_CONFIG=<cfg.json> npx ts-node keepers/stake-pool-crank.ts

# refresh the litesvm test fixture (gitignored)
bash scripts/fetch-spl-stake-pool.sh
```

Integration tests run the REAL stake-pool program: the mainnet-deployed upstream binary loaded
at the fork address (behaviorally identical — the fork's only source change is unused at
runtime). Building the fork from source requires the upstream platform-tools line (Solana CLI
~3.1.14) and is a deploy-time step; see `vendor/spl-stake-pool/UPSTREAM.md`.

## Launch checklist (condensed; each row needs published evidence)

1. Final upstream pin re-confirmed + full fork diff published (UPSTREAM.md).
2. Deterministic builds reproduced independently; program hashes published.
3. Audits: one focused on the fork diff, one on the controller + Fusion integration.
4. Mainnet configuration manifest verified: every authority, fee, mint property, PDA, and
   program ID (bootstrap-fusol prints the manifest).
5. `Position.ink_nonce` increment coverage proven for every collateral-changing instruction
   (`litesvm_ink_nonce.rs`).
6. VoteStateV4 parsing landed if the deploy-target cluster has migrated vote accounts
   (`fusion-stake-view` currently fails closed on V4 — validators would be ineligible, a
   liveness issue, not a fund-safety one).
7. Aggregate Active-validator capacity ≥ 100% of productive AUM at public opening.
8. Upgrade authorities set to `None` on both programs, independently verified — THEN open
   deposits, not before.
9. External audit (received 2026-07-14): findings remediated in-tree, closure evidence
   published.

**Open spec-amendment item (pending economic review):** the rebalance's epoch-rotated two-pass
cursor order deviates from the spec's per-epoch global greatest-deficit-first priority (see
"How allocation works" above). The deviation is deliberate and recorded; the spec text has not
yet been amended to match the implemented order.
