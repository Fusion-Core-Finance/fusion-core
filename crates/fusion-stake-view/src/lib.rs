//! Read-only byte-offset views of the EXTERNAL accounts the fuSOL Allocation Controller reads.
//!
//! Same house pattern as fusd-core's `stake_pool.rs` offset parser:
//!
//! - **Verified fixed offsets** instead of pulling the upstream crates (no runtime deps, no
//!   version-lock churn, no zero-copy alignment surprises). Each module's doc carries the offset
//!   table and the field-order derivation it was verified against.
//! - **Errors degrade, never panic**: every slice access is bounds-checked and all arithmetic is
//!   checked, so adversarial / truncated / wrong-account data returns `Err`/`None`. A momentarily
//!   unreadable account can only fail toward "validator or position ineligible", never abort a
//!   crank transaction.
//! - **The caller owns the runtime owner check** (`account.owner == <expected program>`); these
//!   modules see raw bytes only, which keeps them `no_std`, dependency-free, and host-testable.
//!
//! Layout pins:
//! - [`stake_pool`] + [`validator_list`]: spl-stake-pool v2.0.3 (pinned upstream
//!   solana-program/stake-pool @ a27629b, `program/src/state.rs`), the same verified offsets as
//!   fusd-core's parser (cross-checked against it in tests).
//! - [`vote_state`]: the agave reference deserializer — see that module's doc for the exact
//!   crate files verified.
//! - [`stake_account`]: the native stake program's `StakeStateV2` (solana-stake-interface, the
//!   version pinned by the vendored fork), round-trip-pinned against the real type serialized
//!   with the stake program's own codec.
//! - [`position`]: fusd-core's `state/position.rs`, pinned by a round-trip test that serializes
//!   the real `Position` type (dev-dependency) and reads it back through the fixed offsets.

#![cfg_attr(not(test), no_std)]

pub mod position;
pub mod stake_account;
pub mod stake_pool;
pub mod validator_list;
pub mod vote_state;

mod bytes;
