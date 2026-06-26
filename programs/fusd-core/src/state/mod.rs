//! On-chain account types. One file per account; see fusion-docs.md for the
//! full partitioning rationale (per-collateral / per-user sharding for Sealevel
//! parallelism). Hot/large accounts (`Market`, `ReactorPool`) migrate to
//! `#[account(zero_copy)]` as their flows are implemented.

pub mod debt_ceiling_line;
pub mod dex_twap;
pub mod global_backstop;
pub mod governance;
pub mod insurance_buffer;
pub mod market;
pub mod market_oracle;
pub mod position;
pub mod protocol_config;
pub mod redemption_bitmap;
pub mod reactor_pool;
pub mod supply_reconciliation;

pub use debt_ceiling_line::*;
pub use dex_twap::*;
pub use global_backstop::*;
pub use governance::*;
pub use insurance_buffer::*;
pub use market::*;
pub use market_oracle::*;
pub use position::*;
pub use protocol_config::*;
pub use redemption_bitmap::*;
pub use reactor_pool::*;
pub use supply_reconciliation::*;

/// Borsh SPACE pins. The zero-copy accounts get compile-time `const _`
/// asserts in their own files; Borsh accounts can't (serialized size isn't a const), so this
/// test makes every hand-summed `SPACE` constant drift-proof at `cargo test -p fusd-core` time
/// (wired into ci-checks step 1). Before this, a missed SPACE update was only caught if a
/// litesvm test happened to init that account type.
#[cfg(test)]
mod layout_tests {
    use super::*;
    use anchor_lang::prelude::Pubkey;
    use anchor_lang::AnchorSerialize;

    #[track_caller]
    fn assert_space<T: AnchorSerialize>(value: &T, space: usize, name: &str) {
        let serialized = 8 + value.try_to_vec().unwrap().len();
        assert_eq!(
            serialized, space,
            "{name}::SPACE ({space}) != 8 + serialized len ({serialized}) — update the constant"
        );
    }

    #[test]
    fn borsh_space_constants_match_serialized_layouts() {
        // These accounts hold only ints/Pubkeys/bools/byte-arrays — the all-zero bit pattern is
        // a valid value for every field, so `zeroed()` builds a representative instance without
        // hand-writing ~40 fields each. Types containing an ENUM are constructed explicitly
        // below with a valid variant instead (never zeroed).
        unsafe {
            assert_space(&std::mem::zeroed::<ProtocolConfig>(), ProtocolConfig::SPACE, "ProtocolConfig");
            assert_space(&std::mem::zeroed::<Market>(), Market::SPACE, "Market");
            assert_space(&std::mem::zeroed::<Position>(), Position::SPACE, "Position");
            assert_space(&std::mem::zeroed::<ReactorPool>(), ReactorPool::SPACE, "ReactorPool");
            assert_space(&std::mem::zeroed::<ReactorDeposit>(), ReactorDeposit::SPACE, "ReactorDeposit");
            assert_space(&std::mem::zeroed::<GovernanceGate>(), GovernanceGate::SPACE, "GovernanceGate");
            assert_space(&std::mem::zeroed::<MarketOracle>(), MarketOracle::SPACE, "MarketOracle");
            assert_space(&std::mem::zeroed::<InsuranceBuffer>(), InsuranceBuffer::SPACE, "InsuranceBuffer");
            assert_space(&std::mem::zeroed::<GlobalBackstopReserve>(), GlobalBackstopReserve::SPACE, "GlobalBackstopReserve");
        }
        // TimelockedParam carries a MarketParam enum — explicit valid variant.
        let tl = TimelockedParam {
            nonce: 0,
            eta: 0,
            market: Pubkey::default(),
            param: MarketParam::Mcr,
            value: 0,
            bump: 0,
            _reserved: [0u8; 16],
        };
        assert_space(&tl, TimelockedParam::SPACE, "TimelockedParam");
        // TimelockedGlobalParam carries a GlobalParam enum — explicit valid variant.
        let tlg = TimelockedGlobalParam {
            nonce: 0,
            eta: 0,
            param: GlobalParam::Cut,
            value: 0,
            bump: 0,
            _reserved: [0u8; 16],
        };
        assert_space(&tlg, TimelockedGlobalParam::SPACE, "TimelockedGlobalParam");
    }
}
