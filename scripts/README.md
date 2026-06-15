# scripts

Operational scripts (deploy, init, verified builds, governance param changes).

## Deploy & bootstrap a cluster

```sh
# 1. build + deploy the program (the deploy keypair under keys/ → target/deploy/, matching declare_id!)
anchor build
solana program deploy target/deploy/fusd_core.so \
  --program-id target/deploy/fusd_core-keypair.json --url <cluster>

# 2. initialize protocol + markets (idempotent: re-running skips what exists)
ANCHOR_PROVIDER_URL=<rpc> ANCHOR_WALLET=~/.config/solana/id.json \
  npx ts-node scripts/bootstrap.ts [config.json]
```

`bootstrap.ts` runs the on-chain-enforced init order — `init_protocol` → `init_governance_gate` →
per-market `init_market` → `init_market_oracle` → `init_reactor_pool` → `init_insurance_buffer`. With no
config arg it uses the built-in WSOL/USDC defaults (mainnet-fork values); pass a JSON file (same shape as
`DEFAULT_CFG` in the script) to override authorities + per-market params/oracle feeds for devnet/mainnet.
The wallet must be the program's **upgrade authority** (`init_protocol` is gated to it). For a real demo,
run against a **surfpool mainnet-fork** so the oracle feed accounts (Pyth/Switchboard/Orca) exist.
> Node ≥ 20 is required for the TS toolchain (a transitive `@solana/web3.js` v2 codec needs it).

- `ci-checks.sh` — **the aggregate release gate**: runs every check below in the correct
  order (pure-crate tests → fusd-oracle clippy `-D warnings` → dev-oracle build → litesvm integration
  tests → Kani `--gate` → dev_set_price isolation → stack-frame gate ×2). This is exactly what `.github/workflows/ci.yml`
  runs; run it locally before any deploy / PR. `FAST=1` is reserved for future use.
- `verifiable-build.sh` — deterministic build via `solana-verify` (fusion-docs.md).
- `check-no-dev-oracle.sh` — release gate: a production build/IDL must not expose `dev_set_price`.
- `check-no-certora.sh` — release gate: the verification-only `cvlr`/`certora` deps must not reach the production build (`cargo tree -e normal` clean + `.so` string scan). Mirrors `check-no-dev-oracle.sh`; self-tests its detector. The Certora prover itself runs in a separate cloud lane (`certora/README.md`).
- `check-stack-offsets.sh` — release gate: no >4 KB SBF stack frames (anchor only *warns*; the `.so` then corrupts at runtime). Run for both the production and `dev-oracle` configurations.
- `kani-audit.sh` — isolated Kani runner + **merge gate** for `fusd-math`: runs each `#[kani::proof]` one at a time (cbmc killed between, so an orphaned solver can't poison the next run), regenerates the tracked artifact `crates/fusd-math/kani_audit.tsv`, and fails on any non-PASS / TIMEOUT / untagged / `VACUOUS` harness. `--gate` does a fast tag+artifact check with no Kani run. Strength tags + rationale: `crates/fusd-math/PROOF_STRENGTH.md`.
- `bootstrap.ts` — the idempotent init orchestrator (protocol + markets); see "Deploy & bootstrap" above.
- *(planned)* `set-param.ts` (governance param queue/execute), `keys-sync.ts`.
