# keepers

Permissionless off-chain bots. Fusion never relies on a keeper whitelist — these are
reference implementations that anyone can run, kept profitable by liquidation bonuses,
redemption fees, and small crank rewards (fusion-docs.md).

Planned bots:
- **liquidator** — watch positions, liquidate any below MCR (RP offset → redistribution).
- **redeemer** — arbitrage the peg floor; supply lowest-rate-bucket members for `redeem`.
- **oracle-poster** — post fresh Pyth `PriceUpdateV2` / Switchboard quotes in-tx.
- **twap-sampler** — sample Orca/Raydium into the per-market `DexTwap` ring.
- **refresher** — call `refresh_market` to fold the rate accumulator (~once/slot).

Language TBD (Rust with solana-client, or TS) — decided when the first flow ships.
