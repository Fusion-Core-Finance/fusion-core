# Surfpool mainnet-fork oracle test

Highest-fidelity oracle coverage: runs `fusd_core` inside a [Surfpool](https://surfpool.run)
**mainnet fork**, so it exercises the on-chain path against **real, live Solana accounts**
(fetched just-in-time from mainnet) instead of synthetic fixtures.

```
./tests/surfpool/run.sh
```

## What it verifies (and why it's worth a fork)

`integration-tests/tests/surfpool_oracle.rs` →
`sample_twap` against the **real Orca SOL/USDC whirlpool**
(`HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ`). This runs our hand-rolled `clmm.rs`
byte-offset parser on **production account bytes**, catching any drift between our pinned
layout ([`docs/clmm-pool-layouts.md`](../../docs/clmm-pool-layouts.md)) and mainnet reality
that a fixture cannot. Asserts a sane live SOL price lands in the ring.

The aggregation/mode logic (`update_price` → Pyth + Switchboard + TWAP → `aggregate` →
`mode`/`spot`/`debt_spot`) is already covered **hermetically** by the litesvm suite,
including the `mode == Ok` path via a self-signed Switchboard quote
(`integration-tests/tests/litesvm_oracle.rs`). So this fork test deliberately focuses on the
one thing litesvm can't give: real mainnet account layouts.

## Why it's not in CI

It forks from a mainnet RPC (network-dependent, non-hermetic) → kept out of the per-commit
gate in `.github/workflows/ci.yml`. Run it manually before a release, or wire it as a
scheduled job.

## Program-id note

`run.sh` runs `anchor build`, then asserts the committed `declare_id!` (`FuSiont…`) matches
`target/deploy/fusd_core-keypair.json` (Anchor's entrypoint enforces `program_id ==
declare_id`). In this repo they are already aligned, so no `anchor keys sync` is needed; the
runner bails if they ever drift rather than silently rotating the id.

## Extension: the real-signature Switchboard leg (JS gateway)

`run.sh`/`surfpool_oracle.rs` exercise the DEX-TWAP sampler against live data but do **not**
post a fresh Pyth update or a gateway-signed Switchboard quote (those need the off-chain
Switchboard crossbar/gateway + the JS SDKs — not expressible from Rust). To drive the full
`update_price` → `mode == Ok` path against **real oracle signatures** on the fork:

1. Keep surfpool running (it forks the real Pyth receiver + Switchboard queue/program).
2. In a TS harness (`@switchboard-xyz/on-demand`, `@pythnetwork/pyth-solana-receiver`,
   `@coral-xyz/anchor`):
   - `PullFeed`/crossbar → fetch a fresh signed SOL/USD quote + its Ed25519 instruction.
   - Pyth Hermes → fetch + post a fresh `PriceUpdateV2` for feed id
     `ef0d8b6fcd0104e3e75096912fc8e1e432893da4f18faedaacca7e5875da620f` (SOL/USD).
   - Send `[ed25519_quote_ix, update_price_ix(sb_quote_ix_index=0, …)]`.
   - Assert `Market.oracle_mode == 1` (Ok) and a borrow succeeds; a divergent quote freezes.

Real mainnet addresses for the bindings:

| binding | address |
|---|---|
| collateral (WSOL) | `So11111111111111111111111111111111111111112` |
| quote (USDC) | `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` |
| Orca SOL/USDC whirlpool | `HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ` |
| Pyth receiver | `rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ` |
| Switchboard On-Demand | `SBondMDrcV3K4kxZR1HNVT7osZxAHVHgYXL5Ze1oMUv` |
