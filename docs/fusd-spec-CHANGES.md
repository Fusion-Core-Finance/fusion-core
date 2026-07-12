# fusd-spec — Changelog

Version history for [`docs/fusd-spec-v1.md`](fusd-spec-v1.md), the formal specification of
fusion-core (FUSD). The spec is versioned independently of the protocol: a `MAJOR` bump means a
breaking change to an action's guards/formula or an account's layout; `MINOR` adds an action,
account, or invariant; `PATCH` corrects a citation, formula, or wording without changing what the
code does. Every entry notes the `master` commit the spec was pinned to.

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
