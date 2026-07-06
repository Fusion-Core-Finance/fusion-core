//! Instruction handlers. One module per instruction (fusion-docs.md table).
//!
//! These are glob re-exports (not selective) on purpose: Anchor's `#[program]` macro
//! resolves each instruction's generated `__client_accounts_*` / `__cpi_client_accounts_*`
//! modules through the crate root, so they must be re-exported here. Each module also
//! exposes a `handler` fn, which makes `instructions::handler` an ambiguous name — but we
//! always call handlers by full path (`instructions::withdraw::handler`), so it's benign.
#![allow(ambiguous_glob_reexports)]

pub mod adjust_rate;
pub mod borrow;
pub mod claim_coll_surplus;
pub mod claim_reactor_gains;
pub mod close_position;
pub mod create_fusd_metadata;
pub mod debt_ceiling_line;
pub mod deposit;
pub mod fund_buffer;
pub mod global_backstop;
pub mod governance;
pub mod guardian_derisk;
pub mod init_insurance_buffer;
pub mod init_market;
pub mod init_market_oracle;
pub mod init_protocol;
pub mod init_reactor_pool;
pub mod liquidate;
pub mod migrate_gov_authority;
pub mod open_position;
pub mod open_reactor_deposit;
pub mod oracle_admin;
pub mod provide_to_reactor;
pub mod redeem;
pub mod refresh_market;
pub mod repay;
pub mod sample_twap;
pub mod set_guardian;
pub mod shutdown;
pub mod settle_bad_debt;
pub mod supply_reconciliation;
pub mod sweep_protocol_collateral;
pub mod update_price;
pub mod urgent_redeem;
pub mod withdraw;
pub mod withdraw_from_reactor;
pub mod withdraw_surplus;

pub use adjust_rate::*;
pub use borrow::*;
pub use claim_coll_surplus::*;
pub use claim_reactor_gains::*;
pub use close_position::*;
pub use create_fusd_metadata::*;
pub use debt_ceiling_line::*;
pub use deposit::*;
pub use fund_buffer::*;
pub use global_backstop::*;
pub use governance::*;
pub use guardian_derisk::*;
pub use init_insurance_buffer::*;
pub use init_market::*;
pub use init_market_oracle::*;
pub use init_protocol::*;
pub use init_reactor_pool::*;
pub use liquidate::*;
pub use migrate_gov_authority::*;
pub use open_position::*;
pub use oracle_admin::*;
pub use open_reactor_deposit::*;
pub use provide_to_reactor::*;
pub use redeem::*;
pub use refresh_market::*;
pub use repay::*;
pub use sample_twap::*;
pub use set_guardian::*;
pub use shutdown::*;
pub use settle_bad_debt::*;
pub use supply_reconciliation::*;
pub use sweep_protocol_collateral::*;
pub use update_price::*;
pub use urgent_redeem::*;
pub use withdraw::*;
pub use withdraw_from_reactor::*;
pub use withdraw_surplus::*;

#[cfg(feature = "dev-oracle")]
pub mod dev_set_price;
#[cfg(feature = "dev-oracle")]
pub use dev_set_price::*;
