# Vendored SPL Stake Pool — upstream pin manifest

The fuSOL Native Stake Share Pool uses a minimal, upstream-compatible fork of the SPL Stake
Pool program for custody and share accounting, deployed under a Fusion-specific program ID and
controlled exclusively by the immutable Allocation Controller PDAs (`programs/fusion-stake-controller`).

## Pin

| | |
|---|---|
| Upstream repository | `https://github.com/solana-program/stake-pool` |
| Pinned commit | `a27629b1696cc4cb4fb9bad6a48547f444c8e006` (main, 2026-07-13) |
| Crate / version | `spl-stake-pool` v2.0.3 (`program/`) |
| Upstream program ID | `SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy` |
| Fork program ID | `3pYHXui7Zk21TKE6oqivqbVJWRXt74wdDkqsnb3Q8mMi` |
| License | Apache-2.0 (upstream `LICENSE` carried verbatim) |
| Upstream toolchain | rust 1.93.0 (`rust-toolchain.toml`, lint tasks); Solana CLI / platform-tools 3.1.14 (`workspace.metadata.cli`) |

The pinned commit was current upstream `main` when the fuSOL specification was drafted. Per the
spec, the FINAL production pin MUST be re-confirmed before audit and MUST NOT move afterward.

## Complete diff from upstream (audit surface)

Verified by `diff -rq` against a pristine checkout of the pinned commit — the fork changes
exactly two files, one line each:

1. `program/src/lib.rs:158` — `declare_id!` swapped from `SPoo1Ku8…` to the fork ID
   `3pYHXui7Zk21TKE6oqivqbVJWRXt74wdDkqsnb3Q8mMi`. (Program logic is ID-agnostic: every PDA is
   derived from the runtime `program_id` argument, so behavior is byte-identical to upstream.)
2. `program/program-id.md` — same ID swap (docs).

The `Cargo.toml` at this directory's root is NOT part of the upstream program: it is a minimal
standalone-workspace shim replacing the upstream monorepo root (which also carried `clients/cli`),
carrying over verbatim the `[workspace.lints]` table that `program/Cargo.toml` inherits.
`Cargo.lock` is upstream's lockfile re-resolved for the single-member workspace: the PROGRAM's
resolved dependency tree is verified IDENTICAL to upstream's locked tree (`cargo tree --locked
-p spl-stake-pool -e normal,build` diffed clean against the pristine pinned checkout,
2026-07-14); the only pin differences live in pruned cli/dev branches that never ship in the
artifact. Upstream's `rust-toolchain.toml` is deliberately NOT carried (it would repoint every
host cargo invocation in this subtree to a toolchain this repo does not pin); the version is
recorded above instead.

Everything else — `program/src`, `program/tests`, `program/Xargo.toml`, `proptest-regressions` —
is verbatim upstream.

## Building

The deployable `.so` builds with the upstream platform-tools line (Solana CLI ~3.1.14), NOT the
fusion-core anchor 0.32.1 / solana 2.3 toolchain:

```
cd vendor/spl-stake-pool && cargo build-sbf --manifest-path program/Cargo.toml
```

CONFIRMED 2026-07-14: this repo's installed `solana-cargo-build-sbf 2.3.13` (platform-tools
v1.48, cargo 1.84) CANNOT build the fork — the dependency graph requires edition2024
(cargo >= 1.85; first failure is the `wincode` manifest). Building the deployable artifact
requires a side-installed Solana CLI ~3.1.14 (agave) for its platform-tools; this is a
deploy-time (audit/launch) step and deliberately NOT part of `scripts/ci-checks.sh`. Host
`cargo` 1.93 handles the vendored workspace fine for resolution/host checks.

For integration tests, the runtime-equivalent alternative is dumping the deployed upstream
program and loading it at the fork ID (`scripts/fetch-spl-stake-pool.sh` → `fixtures/`): since
the only source change is `declare_id!` (unused at runtime), the mainnet `.so` at the fork
address behaves identically to a from-source fork build. The from-source verifiable build is a
deploy-time requirement, not a test-time one.

## Security / manager posture

The upstream program retains its full manager/staker instruction surface (SetFee, SetManager,
SetFundingAuthority, …). Those instructions are neutralized OPERATIONALLY, not by patching:
the manager and staker authorities are Allocation Controller PDAs, and the controller exposes
no passthrough for any of them (see the controller's CPI allowlist). Patching them out of the
fork was considered and rejected — it would enlarge the audited diff and forfeit
upstream-compatibility guarantees.
