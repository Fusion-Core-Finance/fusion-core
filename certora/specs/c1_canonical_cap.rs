//! Certora/CVLR spec — **Invariant C1: the LST canonical-rate cap**.
//!
//! For LST collateral, `fusd_oracle::aggregate` is handed a canonical valuation
//! `canonical = sol_usd · (stake_pool.total_lamports / stake_pool.pool_token_supply)` (RAY USD per
//! whole LST), read by `update_price` from the trustless on-chain SPL stake pool. The collateral
//! (mint/LTV) price is then capped at `MIN(market, canonical)` *before* the −k·σ haircut. The
//! property:
//!
//!   ∀ inputs:  canonical = Some(c)  ⟹  aggregate(..).collateral_price ≤ c          (C1-CAP)
//!   ∀ inputs:  aggregate(.., Some(c), ..).collateral_price
//!                ≤ aggregate(.., None, ..).collateral_price                        (C1-MONOTONE)
//!
//! C1-CAP is the BOLD-08 over-mint defense: an upward-manipulated market feed can never lift
//! borrowing power past the stake-pool reality. C1-MONOTONE states the leg is *purely conservative*
//! — enabling it can only ever LOWER a borrower's mint power, never raise it (so it can never be
//! weaponized into inflating collateral). The DEBT price (liquidation/redemption) is deliberately
//! left on the raw market view, so the cap is collateral-only.
//!
//! The proving rules live in `programs/fusd-core/src/certora.rs`
//! (`c1_canonical_caps_collateral`, `c1_canonical_never_raises_collateral`); they drive the REAL
//! `aggregate` with full-range symbolic u128 `price`/`c`, in the pure-min regime (`k_bps = 0` folds
//! the orthogonal haircut to 0, keeping the proof off the u128 mul/div prover frontier — README
//! §"u128 checked-arith blocker"). Conf: `certora/c1_canonical.conf`.
//!
//! Runnable counterpart (the mutation oracle): the host unit tests in `crates/fusd-oracle/src/lib.rs`
//! (`canonical_caps_collateral_but_not_debt`, `canonical_above_market_does_not_raise_collateral`,
//! `canonical_invariant_holds_under_sweep`) and the litesvm end-to-end suite
//! `integration-tests/tests/litesvm_c1_lst_canonical.rs`.
//!
//! Non-vacuity (see `certora/mutations.md` row C1) — TWO DISTINCT mutations in `aggregate`, each
//! killing one rule (one mutation does NOT break both):
//!   (a) drop the cap (`Some(c) => chosen.price.min(c)` → `Some(c) => chosen.price`) breaks C1-CAP
//!       when `price > c`. It does NOT break C1-MONOTONE — both legs then collapse to `price`.
//!   (b) flip `.min` to `.max` breaks C1-MONOTONE when `c > price` (`max(price,c) > price`), and also
//!       re-breaks C1-CAP when `price > c`.
//!
//! ┌─ STATUS ───────────────────────────────────────────────────────────────────────────────────┐
//! │ Authored to the cloud-verified recipe (pure-arithmetic regime, identical shape to the        │
//! │ VERIFIED `absorb_*` rules). Compiles under `cargo check -p fusd-core --features certora`.     │
//! │ NOT yet run on the Certora cloud (needs CERTORAKEY) — pending a VERIFY like the other confs.  │
//! └──────────────────────────────────────────────────────────────────────────────────────────────┘
#![cfg(feature = "certora")]
#![allow(unused)]

// The proof obligations are the two `#[rule]`s in `programs/fusd-core/src/certora.rs`. This file is
// the English/pseudocode specification only (the `specs/*.rs` convention), mirroring
// `supply_invariant.rs` / `liquidation_terminates.rs`.
