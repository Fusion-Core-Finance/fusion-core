//! On-chain account types. One file per account (fusd-core conventions): explicit version
//! bytes, reserved tails, hand-summed `SPACE` constants pinned by the layout tests below, and
//! compile-time size/offset asserts on the zero-copy account (`EpochState`).

pub mod controller_config;
pub mod epoch_state;
pub mod preference;
pub mod validator_record;

pub use controller_config::*;
pub use epoch_state::*;
pub use preference::*;
pub use validator_record::*;

/// Borsh SPACE pins (fusd-core `state/mod.rs` pattern): the zero-copy `EpochState` gets
/// compile-time `const _` asserts in its own file; Borsh accounts can't (serialized size isn't
/// a const), so this test makes every hand-summed `SPACE` constant drift-proof at
/// `cargo test -p fusion-stake-controller` time.
#[cfg(test)]
mod layout_tests {
    use super::*;
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
        // a valid value for every field, so `zeroed()` builds a representative instance.
        unsafe {
            assert_space(
                &std::mem::zeroed::<ControllerConfig>(),
                ControllerConfig::SPACE,
                "ControllerConfig",
            );
            assert_space(
                &std::mem::zeroed::<ValidatorRecord>(),
                ValidatorRecord::SPACE,
                "ValidatorRecord",
            );
            assert_space(&std::mem::zeroed::<Preference>(), Preference::SPACE, "Preference");
        }
    }
}
