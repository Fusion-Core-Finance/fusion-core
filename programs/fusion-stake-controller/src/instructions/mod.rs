//! Instruction handlers. One module per instruction (fusd-core conventions).
//!
//! Glob re-exports (not selective) on purpose: Anchor's `#[program]` macro resolves each
//! instruction's generated `__client_accounts_*` / `__cpi_client_accounts_*` modules through
//! the crate root, so they must be re-exported here. Each module also exposes a `handler` fn,
//! which makes `instructions::handler` an ambiguous name — but we always call handlers by
//! full path (`instructions::deposit_sol::handler`), so it's benign.
#![allow(ambiguous_glob_reexports)]

pub mod close_preference;
pub mod close_preference_window;
pub mod deposit_sol;
pub mod deposit_stake;
pub mod execute_next_action;
pub mod finalize_plan;
pub mod finalize_pool;
pub mod finish_epoch;
pub mod initialize_controller;
pub mod initialize_pool;
pub mod plan_directed_batch;
pub mod plan_neutral_batch;
pub mod reconcile_batch;
pub mod register_validator;
pub mod set_preference;
pub mod snapshot_preference;
pub mod start_epoch;
pub mod sync_preference;

pub use close_preference::*;
pub use close_preference_window::*;
pub use deposit_sol::*;
pub use deposit_stake::*;
pub use execute_next_action::*;
pub use finalize_plan::*;
pub use finalize_pool::*;
pub use finish_epoch::*;
pub use initialize_controller::*;
pub use initialize_pool::*;
pub use plan_directed_batch::*;
pub use plan_neutral_batch::*;
pub use reconcile_batch::*;
pub use register_validator::*;
pub use set_preference::*;
pub use snapshot_preference::*;
pub use start_epoch::*;
pub use sync_preference::*;
