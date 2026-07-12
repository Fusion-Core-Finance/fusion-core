# Certora / CVLR — cross-instruction core invariants

This directory runs the **Certora Solana Prover** (CVLR — Certora Verification Language for Rust) against
`fusd-core` to prove the cross-instruction invariants that bounded model checking (Kani) and property
tests (proptest) structurally cannot reach — properties that hold only across *sequences of transactions
over the on-chain accounts*.

The Certora Solana Prover is a paid cloud service (license + API key). This lane is optional and never
blocks the local/PR gate; the always-on isolation gate (`scripts/check-no-certora.sh`) guarantees the
verification-only `cvlr` deps never reach the deployable program.

## Two-layer design

| Layer | What | Runs | Status |
|---|---|---|---|
| **Runnable** | `integration-tests/` litesvm fuzz — random/scripted transaction sequences over the live program, asserting the invariants after every tx. | locally (`cargo test`) | ✅ verified (with the `mutations.md` oracle) |
| **Certora** | the CVLR `#[rule]`s in `programs/fusd-core/src/certora.rs` — inductive preservation over symbolic pre-states (the infinite tx space the fuzz only samples). | Certora cloud (license + `CERTORAKEY`) | per-conf ledger below — 6 rules cloud-VERIFIED, 10 authored/pending cloud, 1 retained-failing artifact |

The runnable layer is the **mutation oracle** for the Certora layer: every invariant has a
production-path break that must fail the runnable suite, and every Certora rule is additionally
flipped by a mutation inside a production function in its cone — the shared supply-transition fns,
the `bucket` helpers, `recovery::absorb`, `fusd_oracle::aggregate` (`mutations.md`, class PROD-FN).
A handler CALL-SITE bypass is invisible to the rules and is caught ONLY by the runnable layer
(class HANDLER) — see §"What a green run proves" below. A rule that still passes under its PROD-FN
mutation is vacuous.

## Verified rules

Every implemented rule **executes real code**: the bitmap and absorb rules call the exact production
functions the handlers call, and the supply rules call the shared supply-transition functions
extracted in the M-01 fix (`programs/fusd-core/src/supply_transition.rs`) — the same bodies the
handlers run at `u128`, monomorphized to `NativeInt`. So a mutation inside a covered function flips
both the Certora rule and the litesvm suite. What no rule covers: the handlers' CALL SITES — a
handler that skips or bypasses the covered function leaves every rule green and is caught only by
the litesvm mutation checks (`mutations.md`, class HANDLER).

Cloud-VERIFIED, with `rule_sanity: "basic"` on and the PROD-FN flip confirmed (`mutations.md`):

| Invariant | Rule(s) | Conf | Mutation → VIOLATED |
|---|---|---|---|
| **#2a bitmap** — `words ⟺ counts` coupling (bit `k` set iff `counts[k] > 0`) | `bitmap_coupling_preserved_by_add_member` / `_remove_member` | `bitmap_helper.conf` | drop `rb::set`/`rb::clear` in `bucket.rs` |
| **#3 liquidation** — full debt conserved across all 5 loss-absorption tiers | `absorb_conserves_debt` | `absorb.conf` | `let unhomed = 0;` in `recovery.rs` |
| **#3 liquidation** — strict tier ordering (a tier fires only after higher ones drain) | `absorb_unhomed_iff_no_tier_covers` (+ `absorb_unhomed_reachable` witness) | `absorb.conf` | reorder the global tier ahead of the local buffer |

**Invariant #4 (Reactor-Pool P/S realizability) is deferred from the Certora pass on purpose:** the
pool's `bnum` U256 division is intractable for the SMT backend (it times out). It stays covered by Kani +
proptest + `integration-tests` (`litesvm_reactor_realizability`). The obligation is recorded here, not
silently dropped.

The English/pseudocode specifications of the invariants live in `specs/*.rs` — spec-only pseudocode,
never compiled or run (see §"What a green run proves").

### Authored, pending cloud run

| Invariant | Rule(s) | Conf | Mutation → VIOLATED |
|---|---|---|---|
| **#1 supply** — `circulating == agg_recorded_debt − unminted_interest + bad_debt` | all eight `supply_preserved_by_{borrow,repay,refresh_market,liquidate,redeem,urgent_redeem,settle_bad_debt,book_interest}_ghost` | `supply.conf` | break the shared transition fn (e.g. `new_agg ← agg0` in `supply_transition::borrow`) — flips the rule AND the litesvm suite; a handler call-site drop flips litesvm only (`mutations.md` S1–S8) |
| **C1 LST canonical cap** — `collateral_price ≤ canonical`, and the leg never RAISES collateral | `c1_canonical_caps_collateral` / `c1_canonical_never_raises_collateral` | `c1_canonical.conf` | drop the cap in `fusd_oracle::aggregate` (`Some(c) => chosen.price.min(c)` → `chosen.price`) |

The supply family executes the shared transitions extracted by M-01 and covers every
supply-touching writer (the seven mint/burn handlers plus the interest-booking twin
`accrual::accrue` / `adjust_rate`'s fee). The pre-M-01 borrow rule — which replayed a hand-written
delta over `NativeInt` ghosts — was cloud-VERIFIED, but M-01 rewrote every supply rule body to
execute the shared fns, so the whole family (borrow included) awaits a cloud re-run
(`certoraSolanaProver certora/supply.conf`). Note the pre-M-01 rule's cone made a `borrow.rs` flip
impossible: the flip recorded for the old S1 tick validated the in-rule model, not production code
(`mutations.md` S1).

C1 drives the real `fusd_oracle::aggregate` over symbolic u128 prices, in the same pure-arithmetic regime as the VERIFIED `absorb_*` rules (`k_bps = 0` folds the orthogonal −k·σ haircut to 0, keeping the proof off the u128 mul/div frontier). It compiles under `cargo check -p fusd-core --features certora`, and its mutation is confirmed at the runnable layer (`mutations.md` row C1 — the host test `canonical_caps_collateral_but_not_debt` FAILs under it), but it has **not** been run on the Certora cloud yet (needs `CERTORAKEY`). Run `certoraSolanaProver certora/c1_canonical.conf` to promote it to the VERIFIED table.

## What a green run proves — and what it does not

Each artifact in this directory sits at exactly one rung of this ladder; nothing may be presented a
rung above where it sits.

1. **Production-linked function proofs** (the `bucket` helpers, `recovery::absorb`,
   `fusd_oracle::aggregate`/C1, the shared supply transitions): the property holds of the exact
   shipped function body — under the conf's `-solanaOptimistic*`/`-solanaSkipCallRegInst` flags and
   any concrete-witness restrictions documented in the rule (bucket 0, concrete pre-counts,
   `k_bps = 0`).
2. **Handler call sites**: NOT covered by any rule. A handler that never invokes the covered
   function passes every conf. litesvm-only (`mutations.md`, class HANDLER).
3. **Spec-only pseudocode** (`certora/specs/*.rs`): zero proof assurance — documentation of intent,
   never compiled into any build, never run.
4. **Characterized-failing artifacts** (`bitmap.conf`): never coverage; retained for the frontier
   writeup only.
5. **Pending cloud**: authored rules with no (or no post-rewrite) cloud run — `supply.conf`'s eight
   rules and `c1_canonical.conf`'s two.

A green dashboard means the listed rules hold of the code in their cones under these caveats — it
does not mean the program is verified.

## The working recipe

- **Toolchain:** `certoraSolanaProver` (from `pip install certora-cli`) + the `cargo certora-sbf` build
  plugin (`cargo install cargo-certora-sbf`). Put both on `PATH`.
- **`cargo_tools_version: "v1.53"` is mandatory in every `.conf`** — the default platform-tools (v1.41 /
  rustc 1.75) are too old for the Solana 2.3 line and cascade through lockfile-v4 → edition2024 → MSRV
  failures. v1.53 (rustc 1.89) clears the whole cascade with no dependency pins.
- **Conf flags:** `rule_sanity: "basic"`, `wait_for_results: "all"` (without it the CLI returns async with
  only a URL), `precise_bitwise_ops: true` (bitmap only). `prover_args`: the `-solanaOptimistic*` set +
  `-solanaSkipCallRegInst true` + `-solanaTACMathInt true`; the bitmap conf adds
  `-solanaCvtNondetAccountInfo true` (the Anchor account-summary flag).
- **Build wiring:** `programs/fusd-core/Cargo.toml` has the optional `certora` feature + the `cvlr` deps +
  `[package.metadata.certora]` (the source globs + `cvlr_inlining.txt` + `cvlr_summaries.txt`).
  `cvlr_inlining.txt` carries the memcpy/memmove/memset/memcmp + `__rust_alloc*` + `CVT_*` directives the
  Solana memory model needs — an empty inlining file yields a generic "prover error / no nodes".

### Running

```bash
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
export CERTORAKEY=<your-key>          # never commit it; it's a CI secret only
certoraSolanaProver certora/round_trip.conf      # toolchain/pipeline smoke (must VERIFY first)
certoraSolanaProver certora/supply.conf          # invariant #1 (8 rules, pending post-M-01 re-run)
certoraSolanaProver certora/bitmap_helper.conf   # invariant #2a
certoraSolanaProver certora/absorb.conf          # invariant #3
certoraSolanaProver certora/c1_canonical.conf    # invariant C1 (promotes it from pending)
```

`round_trip.conf` is the trivial pipeline smoke; `bitmap_helper.conf` and `absorb.conf` are fully
cloud-VERIFIED; `supply.conf`'s eight rules execute the shared supply transitions and await their
post-M-01 cloud run.

## Two prover frontiers, characterized and defeated

These two findings are the load-bearing lessons for extending the rule set, and apply to any
Solana/CVLR project using `?`-based error handling or `u128` money math:

- **A skipped indirect `callx` havocs account memory.** `…ok_or(SomeError)?` lowers the `Err` arm to an
  indirect `callx` (the `From`/`core::fmt` conversion). `-solanaSkipCallRegInst true` translates a
  *skipped* `callx` to an empty TAC block, which leaves the result register / account memory **havoced** —
  producing a *spurious* counterexample that looks like a bitwise-modeling bug but isn't. **Fix:** make the
  error path provably dead so the slicer drops it — e.g. a *concrete* pre-state that constant-folds the
  `checked_add`/`checked_sub` (a `cvlr_assume!` on a projected account field is not enough; the slicer's
  scalar domain doesn't track it). The real state-mutating code still runs, so the mutation stays live.
- **Raw `u128` checked arithmetic is mis-modeled.** The prover mis-handles SBF's 128-bit compiler-rt limb
  lowering (`u128` isn't a native register type), so a true `u128` identity can report a spurious
  violation even with fully in-domain arithmetic. **Fix:** express `u128`-valued invariants in
  `cvlr::mathint::NativeInt` (`NativeInt::from(nondet::<u128>())` ranges the full `u128` domain and is
  reasoned over exactly). A diagnostic confirmed the identical algebra VERIFIES at native `u64` width and
  in `NativeInt`, but FAILS in raw `u128 checked_*`.

The supply rules also sidestep the SPL-token CPI mock entirely (which would need a workspace-global
`[patch.crates-io] spl-token` that corrupts the deployable `.so`): each models `circulating` (the SPL
mint supply) as a pure `NativeInt` ghost and EXECUTES the shared supply-transition fn the handler
itself runs at `u128` (`programs/fusd-core/src/supply_transition.rs`, M-01). The residual gap is the
handler call site — dropping the call or the assignment of the returned post-state is invisible to
these rules and is caught only by the litesvm layer (`mutations.md`, class HANDLER).

> The `bitmap_coherence_preserved_by_reconcile` rule + `bitmap.conf` are kept as the **characterized
> first attempt** that drove `reconcile` end-to-end with a symbolic-index store — it still reports the
> spurious counterexample described above and is **superseded by `bitmap_helper.conf`**. It is not a
> passing conf; it is retained for the writeup.

## Isolation: `cvlr` must never reach mainnet bytecode

The `certora` feature and its `cvlr` deps are enabled only by the Certora cloud build, never by `default`.
`scripts/check-no-certora.sh` enforces this structurally (`cvlr` absent from `cargo tree -e normal` plus a
best-effort `.so` string scan; it self-tests its own detector) and runs in `scripts/ci-checks.sh` next to
`check-no-dev-oracle.sh`. The `bucket::add_member`/`remove_member` visibility change (`pub(crate)` +
`#[cfg_attr(feature = "certora", inline(always))]`) is verification-only and behavior-neutral.

## A counterexample is a FINDING, not a fix

If the prover produces a real counterexample on `fusd-core`, **stop** and surface it with the trace — it
may be an on-chain bug with funds implications. Do not weaken the rule to make it pass.

## Files

- `programs/fusd-core/src/certora.rs` — the CVLR `#[rule]`s (compiled only under `--features certora`).
- `specs/*.rs` — the English/pseudocode specification of each invariant (spec-only pseudocode; never
  compiled or run).
- `supply.conf` · `bitmap_helper.conf` · `absorb.conf` · `round_trip.conf` — the run configs (build +
  rules + flags). `c1_canonical.conf` — the C1 LST-cap rules (authored, pending cloud). `bitmap.conf`
  — the superseded characterized-frontier attempt.
- `cvlr_inlining.txt` · `cvlr_summaries.txt` — the Solana memory-model inlining/summary directives.
- `mutations.md` — the non-vacuity acceptance matrix (the production break each rule must fail on).
- `../scripts/check-no-certora.sh` — the isolation gate. `../.github/workflows/certora.yml` — the cloud lane.
