# fusd-spec — Changelog

Version history for [`docs/fusd-spec-v1.md`](fusd-spec-v1.md), the formal specification of
fusion-core (FUSD). The spec is versioned independently of the protocol: a `MAJOR` bump means a
breaking change to an action's guards/formula or an account's layout; `MINOR` adds an action,
account, or invariant; `PATCH` corrects a citation, formula, or wording without changing what the
code does. Every entry notes the `master` commit the spec was pinned to.

## v1.3.0

Pins the 2026-07-14 canonical-primary oracle mode (the implementing `master` commit) — the fuSOL
pricing mode: a market whose collateral has NO external market feed and is priced as
`sol_usd × stake_pool_rate`.

- **New `MarketOracle` fields** `canonical_primary: u8` + `liquidity_haircut_bps: u16`, carved
  from the HEAD of `_reserved` (30 → 27; total SPACE unchanged). Both init-only; zeroed bytes on
  every existing account decode as mode off + no haircut — byte-identical prior behavior.
- **`init_market_oracle`**: two new args (`canonical_primary`, `liquidity_haircut_bps`, appended)
  and a mode split — mode 1 requires the SOL/USD Pyth feed id, a bound FORK stake pool, NO DEX
  pools, and a haircut in `[1, MAX_LIQUIDITY_HAIRCUT_BPS]`; mode 0 keeps the ≥1-TWAP-venue rule
  and requires the haircut be 0.
- **`update_price`**: in mode 1 both parsed views (SOL/USD legs) are scaled by the bound pool's
  `total_lamports / pool_token_supply` BEFORE aggregation (conf scaled too — ratios and the
  Pyth↔SB deviation corridor are scale-invariant), so `spot` AND `debt_spot` track pool NAV and a
  negative-NAV finalization reaches the liquidation path on the next crank. The collateral leg
  then takes the mandatory liquidity haircut (the debt leg deliberately does not — it wants the
  conservative HIGH side). An unavailable pool rate (absent / parse failure / epoch lag /
  pool-mint mismatch) freezes mints AND WITHHOLDS the commit — there is no market feed to fall
  back on, so the cache ages into the staleness machinery. A pool owned by the wrong deployment
  hard-reverts (`InvalidStakePool`); the expected owner is `FUSION_STAKE_POOL_PROGRAM_ID` in
  mode 1 vs `SPL_STAKE_POOL_PROGRAM_ID` for the C1 min-cap leg (both via the shared
  `parse_bound_stake_pool`).
- **`fusd_oracle::aggregate`**: new `OracleConfig.twap_corridor_optional` — mint mode no longer
  requires a PRESENT TWAP when set (no fuSOL venue exists pre-listing); a present-but-divergent
  TWAP still freezes mints. `false` everywhere else — existing markets unchanged.
- **New constants** `FUSION_STAKE_POOL_PROGRAM_ID` (the pinned fork deployment) and
  `MAX_LIQUIDITY_HAIRCUT_BPS` (2 000).

Versioning: **MINOR** — new account fields from documented reserved padding + new args + a new
mode gated entirely on an opt-in flag; every existing market's behavior is bit-for-bit unchanged
(mode 0 paths are untouched; the aggregate change is behind a flag no existing market can set).

## v1.2.0

Pins the 2026-07-14 fuSOL groundwork change (the implementing `master` commit): the
`Position.ink_nonce` collateral-change nonce.

- **New `Position` field** `ink_nonce: u64`, carved from the HEAD of `_reserved` (32 → 24; total
  SPACE unchanged): a monotonic nonce that bumps whenever `ink` CHANGES for any reason — deposit,
  withdrawal, redemption drain, liquidation seize, and the lazy tier-2 redistribution fold on any
  touch (including debt-only borrow/repay/adjust_rate). A no-op re-write of the same value (e.g.
  re-zeroing an already-drained zombie) does not bump.
- **New sole mutator** `Position::set_ink` — every `ink` write routes through it (open_position's
  fresh-account field init excepted), so the nonce can never silently miss a collateral change.
- **Purely informational**: no fusd-core solvency, debt, oracle, or liquidation path reads the
  field. It exists for the stake-pool Allocation Controller (fuSOL native stake pool, in
  development), which reads it to invalidate validator-direction preferences when position
  collateral moves, preventing fungible-share direction reuse. Zeroed bytes on pre-carve accounts
  decode as `0` ("never changed"), the correct grandfather sentinel.

Versioning: **MINOR** — adds an account field carved from documented reserved padding; every
existing account and action behaves identically.

## v1.1.0

Pins the 2026-07-11 audit-L-02 change (the implementing `master` commit): the liquidation-infra
borrow gate.

- **New `Market` field** `liq_infra_flags: u8`, carved from the HEAD of `_reserved` (10 → 9;
  total SPACE unchanged): bit 0 written at `init_market` (born gated), bit 1 OR'd in by
  `init_reactor_pool`, bit 2 by `init_insurance_buffer`.
- **New `borrow` guard**: `require!(flags == 0 || (flags & LIQ_INFRA_READY_MASK) ==
  LIQ_INFRA_READY_MASK, LiquidationInfraNotReady)` (error 6048, appended) — `liquidate`
  hard-requires the ReactorPool + InsuranceBuffer accounts, so debt is never mintable before they
  exist. `init_reactor_pool`/`init_insurance_buffer` take the `market` writable.
- **New invariant**: debt exists ⇒ the market's liquidation infrastructure exists (inductive via
  gating `borrow`, the only principal-debt creator).

Versioning: **MINOR**, not MAJOR — the file's MAJOR rule (breaking change to an action's guards or
an account's layout) was considered and judged to target breaking changes to existing documented
behavior. This change adds an action-guard, an account field, and an invariant while every
existing account's behavior is identical: the live market's zeroed reserve byte decodes as the
`0` grandfather sentinel, so its `borrow` path is bit-for-bit unchanged, and the layout carve
came out of documented reserved padding.

## v1.0.0

Initial formal specification. Covers the 52 production instructions and 16 persisted account types
on `master`. Structure: Notation (fixed-point scales + symbol table) → Stages (per-account) →
Actions (per-instruction preconditions/formula/guards/post-state) → Invariants (constitutional +
conservation + solvency, each cross-linked to its Kani/Certora/mutation artifact) → Rejected
Alternatives. Every field, formula, guard, and invariant is cross-linked to source at `file:line`.

Pins the 2026-07-08 redemption change: `redeem` and `urgent_redeem` pay collateral at
`mid = (spot + debt_spot) / 2`, and the C9 dynamic redemption fee is charged post-bump.
