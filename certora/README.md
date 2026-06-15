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
| **Certora** | the CVLR `#[rule]`s in `programs/fusd-core/src/certora.rs` — inductive preservation over symbolic pre-states (the infinite tx space the fuzz only samples). | Certora cloud (license + `CERTORAKEY`) | ✅ 4 invariants VERIFIED + mutation-checked |

The runnable layer is the **mutation oracle** for the Certora layer: every rule has a production-path
break that must fail it (`mutations.md`). A rule that still passes under its mutation is vacuous.

## Verified rules

All four drive the **real production code** (not a re-statement), VERIFY on the cloud, and flip to
VIOLATED under their mutation (`rule_sanity: "basic"` is on; `mutations.md` records each break):

| Invariant | Rule(s) | Conf | Mutation → VIOLATED |
|---|---|---|---|
| **#1 supply** — `circulating == agg_recorded_debt − unminted_interest + bad_debt` | `supply_preserved_by_borrow_ghost` | `supply.conf` | drop `agg_recorded_debt = new_agg` in `borrow.rs` |
| **#2a bitmap** — `words ⟺ counts` coupling (bit `k` set iff `counts[k] > 0`) | `bitmap_coupling_preserved_by_add_member` / `_remove_member` | `bitmap_helper.conf` | drop `rb::set`/`rb::clear` in `bucket.rs` |
| **#3 liquidation** — full debt conserved across all 5 loss-absorption tiers | `absorb_conserves_debt` | `absorb.conf` | `let unhomed = 0;` in `recovery.rs` |
| **#3 liquidation** — strict tier ordering (a tier fires only after higher ones drain) | `absorb_unhomed_iff_no_tier_covers` (+ `absorb_unhomed_reachable` witness) | `absorb.conf` | reorder the global tier ahead of the local buffer |

**Invariant #4 (Reactor-Pool P/S realizability) is deferred from the Certora pass on purpose:** the
pool's `bnum` U256 division is intractable for the SMT backend (it times out). It stays covered by Kani +
proptest + `integration-tests` (`litesvm_reactor_realizability`). The obligation is recorded here, not
silently dropped.

The English/pseudocode specifications of all four invariants live in `specs/*.rs`.

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
certoraSolanaProver certora/supply.conf          # invariant #1
certoraSolanaProver certora/bitmap_helper.conf   # invariant #2a
certoraSolanaProver certora/absorb.conf          # invariant #3
```

`round_trip.conf` is the trivial pipeline smoke; the other three are the verified invariant confs.

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

The supply rule also sidesteps the SPL-token CPI mock entirely (which would need a workspace-global
`[patch.crates-io] spl-token` that corrupts the deployable `.so`): it models `circulating` (the SPL mint
supply) as a pure `NativeInt` ghost and replays the handler's accounting delta. The same ghost pattern
extends to the remaining supply rows (`repay`/`refresh_market`/`liquidate`/`settle`).

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
- `specs/*.rs` — the English/pseudocode specification of each invariant.
- `supply.conf` · `bitmap_helper.conf` · `absorb.conf` · `round_trip.conf` — the run configs (build +
  rules + flags). `bitmap.conf` — the superseded characterized-frontier attempt.
- `cvlr_inlining.txt` · `cvlr_summaries.txt` — the Solana memory-model inlining/summary directives.
- `mutations.md` — the non-vacuity acceptance matrix (the production break each rule must fail on).
- `../scripts/check-no-certora.sh` — the isolation gate. `../.github/workflows/certora.yml` — the cloud lane.
