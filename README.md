# Fusion

**Fusion** is a trustless, permissionless protocol issuing the **Fusion Dollar (FUSD)** —
a **Solana-native overcollateralized CDP stablecoin** in the Liquity lineage, governed by
**MetaDAO futarchy** (via the **FUSION** ownership token) within hard, code-enforced bounds.
Goal: "unstoppable cash" — no issuer custody, no freeze authority, no privileged
redemption gate, fully permissionless liquidations; governance tunes only *bounded* risk
parameters and can never touch user funds.

> **Status: Phase-1 core functionally complete, pre-audit.** The CDP engine, per-position
> interest, the liquidation waterfall + insurance buffer, rate-bucket redemption, the
> oracle stack, and the bounded GovernanceGate are built and tested. See
> [`docs/fusion-docs.md`](docs/fusion-docs.md) for the full technical reference (design,
> invariants, and component status).

## Layout

```
programs/fusd-core   the CDP engine (Anchor program)
crates/fusd-math     fixed-point money math (WAD/RAY/RAD), bps, mul-div
crates/fusd-oracle   Pyth + Switchboard + DEX-TWAP validation, asymmetric pricing, freeze modes
sdk                  TypeScript client (PDA derivation, decoders, ix builders)
keepers              permissionless off-chain bots (liquidator, redeemer, oracle-poster, ...)
integration-tests    in-process litesvm test suite (the primary tests)
tests                TS e2e (surfpool mainnet-fork, Squads PoC)
scripts              deploy / init / verifiable-build / governance
migrations           Anchor deploy migration
runbooks             operational runbooks
docs                 the canonical technical reference (fusion-docs.md) + the CLMM pool-layout spec
```

## Toolchain

Anchor 0.32.1 · Solana 2.3 · Rust (host 1.93, SBF platform-tools 1.84) · Node 18 · Yarn.

## Build & test

```bash
# Rust unit tests (host) — fixed-point math + oracle logic
cargo test -p fusd-math -p fusd-oracle

# Build the on-chain program (SBF) + generate the IDL (dev-oracle = the test feature)
anchor build -- --features dev-oracle

# In-process integration tests (litesvm)
cargo test -p fusd-integration-tests

# The full release gate (everything CI runs)
./scripts/ci-checks.sh

# Deterministic verifiable build (release; needs solana-verify)
./scripts/verifiable-build.sh
```

## Key invariants (enforced in code, not policy)

FUSD mint freeze authority = `None`; mint authority = a program PDA (legacy SPL Token).
No admin freeze/seize/pause-of-funds instruction exists. Liquidation & redemption are
permissionless. Governance writes only bounded params within compile-time clamps.
See [`docs/fusion-docs.md`](docs/fusion-docs.md) §8 (Security Model & Invariants).

## Security

Found a vulnerability? Please report it privately — see [`SECURITY.md`](SECURITY.md).

## License

Licensed under the [Apache License 2.0](LICENSE).
