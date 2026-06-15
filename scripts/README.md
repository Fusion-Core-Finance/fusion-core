# scripts

Operational scripts (deploy, init, verified builds, governance param changes).

- `ci-checks.sh` — **the aggregate release gate**: runs every check below in the correct
  order (pure-crate tests → fusd-oracle clippy `-D warnings` → dev-oracle build → litesvm integration
  tests → Kani `--gate` → dev_set_price isolation → stack-frame gate ×2). This is exactly what `.github/workflows/ci.yml`
  runs; run it locally before any deploy / PR. `FAST=1` is reserved for future use.
- `verifiable-build.sh` — deterministic build via `solana-verify` (fusion-docs.md).
- `check-no-dev-oracle.sh` — release gate: a production build/IDL must not expose `dev_set_price`.
- `check-no-certora.sh` — release gate: the verification-only `cvlr`/`certora` deps must not reach the production build (`cargo tree -e normal` clean + `.so` string scan). Mirrors `check-no-dev-oracle.sh`; self-tests its detector. The Certora prover itself runs in a separate cloud lane (`certora/README.md`).
- `check-stack-offsets.sh` — release gate: no >4 KB SBF stack frames (anchor only *warns*; the `.so` then corrupts at runtime). Run for both the production and `dev-oracle` configurations.
- `kani-audit.sh` — isolated Kani runner + **merge gate** for `fusd-math`: runs each `#[kani::proof]` one at a time (cbmc killed between, so an orphaned solver can't poison the next run), regenerates the tracked artifact `crates/fusd-math/kani_audit.tsv`, and fails on any non-PASS / TIMEOUT / untagged / `VACUOUS` harness. `--gate` does a fast tag+artifact check with no Kani run. Strength tags + rationale: `crates/fusd-math/PROOF_STRENGTH.md`.
- *(planned)* `init-protocol.ts`, `init-market.ts`, `set-param.ts` (governance), `keys-sync.ts`.
