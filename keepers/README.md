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
