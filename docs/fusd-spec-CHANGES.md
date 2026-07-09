# fusd-spec — Changelog

Version history for [`docs/fusd-spec-v1.md`](fusd-spec-v1.md), the formal specification of
fusion-core (FUSD). The spec is versioned independently of the protocol: a `MAJOR` bump means a
breaking change to an action's guards/formula or an account's layout; `MINOR` adds an action,
account, or invariant; `PATCH` corrects a citation, formula, or wording without changing what the
code does. Every entry notes the `master` commit the spec was pinned to.

## v1.0.0

Initial formal specification. Covers the 52 production instructions and 16 persisted account types
on `master`. Structure: Notation (fixed-point scales + symbol table) → Stages (per-account) →
Actions (per-instruction preconditions/formula/guards/post-state) → Invariants (constitutional +
conservation + solvency, each cross-linked to its Kani/Certora/mutation artifact) → Rejected
Alternatives. Every field, formula, guard, and invariant is cross-linked to source at `file:line`.

Pins the 2026-07-08 redemption change: `redeem` and `urgent_redeem` pay collateral at
`mid = (spot + debt_spot) / 2`, and the C9 dynamic redemption fee is charged post-bump.
