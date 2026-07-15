//! Pure byte-parsing of a native stake-program account (`StakeStateV2`) — the minimal
//! delegation-voter view the Allocation Controller needs to bind a deposited stake account to
//! the validator record it was cap-checked against. The caller owns the runtime owner check
//! (`account.owner == Stake11111111111111111111111111111111111111`).
//!
//! Layout from the stake interface pinned by the vendored fork (solana-stake-interface 4.2.0;
//! byte-identical in every published version — the crate hand-writes the `StakeStateV2`
//! borsh impl to match the stake program's bincode layout: a **4-byte u32 LE** enum tag, then
//! packed little-endian fields, no padding). Cross-checked against the vendored pool's own
//! reads (`vendor/spl-stake-pool/program/src/processor.rs` `get_stake_state` →
//! `stake.delegation.voter_pubkey`) and round-trip-pinned in the tests below against the real
//! `StakeStateV2` type serialized with the stake program's own codec (bincode).
//!
//! | offset | size | field |
//! |--------|------|-------|
//! | 0      | 4    | state tag (u32 LE): 0 Uninitialized, 1 Initialized, 2 **Stake**, 3 RewardsPool |
//! | 4      | 8    | `Meta.rent_exempt_reserve` (u64 LE) |
//! | 12     | 32   | `Meta.authorized.staker` |
//! | 44     | 32   | `Meta.authorized.withdrawer` |
//! | 76     | 8    | `Meta.lockup.unix_timestamp` (i64 LE) |
//! | 84     | 8    | `Meta.lockup.epoch` (u64 LE) |
//! | 92     | 32   | `Meta.lockup.custodian` |
//! | 124    | 32   | `Stake.delegation.voter_pubkey` |
//! | 156    | 8    | `Stake.delegation.stake` (u64 LE) |
//! | 164    | 8    | `Stake.delegation.activation_epoch` (u64 LE) |
//! | 172    | 8    | `Stake.delegation.deactivation_epoch` (u64 LE) |
//! | 180    | 8    | reserved (formerly the warmup_cooldown_rate 64-bit float) |
//! | 188    | 8    | `Stake.credits_observed` (u64 LE) |
//! | 196    | 1    | `StakeFlags` bits |
//!
//! Rows 4..124 exist for tags 1 and 2; rows 124.. only for tag 2. The account is allocated to
//! 200 bytes (`StakeStateV2::size_of()`); the serialized `Stake` arm is 197.

/// `StakeStateV2::Stake` state tag (u32 LE at offset 0) — the only state this view accepts.
pub const STAKE_STATE_TAG_STAKE: u32 = 2;

const STATE_TAG_OFFSET: usize = 0;
const DELEGATION_VOTER_OFFSET: usize = 124;

/// The delegation's voter pubkey, iff the data is a `StakeStateV2::Stake` account. `None` for
/// every other state tag (Uninitialized / Initialized / RewardsPool / unknown) and for
/// short or malformed data — fail closed, never guess.
pub fn delegation_voter(data: &[u8]) -> Option<[u8; 32]> {
    if crate::bytes::u32_le(data, STATE_TAG_OFFSET)? != STAKE_STATE_TAG_STAKE {
        return None;
    }
    crate::bytes::array32(data, DELEGATION_VOTER_OFFSET)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// A 200-byte `StakeStateV2::Stake` fixture built BY HAND per the offset table (never
    /// through a serializer), with a distinct recognizable voter.
    fn stake_fixture(voter: [u8; 32]) -> [u8; 200] {
        let mut d = [0u8; 200];
        d[0..4].copy_from_slice(&2u32.to_le_bytes()); // tag: Stake
        d[4..12].copy_from_slice(&2_282_880u64.to_le_bytes()); // rent_exempt_reserve
        d[12..44].copy_from_slice(&[0xAA; 32]); // staker
        d[44..76].copy_from_slice(&[0xBB; 32]); // withdrawer
        d[76..84].copy_from_slice(&(-1i64).to_le_bytes()); // lockup.unix_timestamp
        d[84..92].copy_from_slice(&7u64.to_le_bytes()); // lockup.epoch
        d[92..124].copy_from_slice(&[0xCC; 32]); // lockup.custodian
        d[124..156].copy_from_slice(&voter); // delegation.voter_pubkey
        d[156..164].copy_from_slice(&5_000_000_000u64.to_le_bytes()); // delegation.stake
        d[164..172].copy_from_slice(&300u64.to_le_bytes()); // activation_epoch
        d[172..180].copy_from_slice(&u64::MAX.to_le_bytes()); // deactivation_epoch
        d
    }

    #[test]
    fn stake_state_yields_the_voter() {
        let voter = [0x42; 32];
        assert_eq!(delegation_voter(&stake_fixture(voter)), Some(voter));
    }

    /// Every non-Stake tag fails closed — including Initialized, whose Meta bytes are present
    /// and would otherwise "parse".
    #[test]
    fn non_stake_states_are_rejected() {
        for tag in [0u32, 1, 3, 4, u32::MAX] {
            let mut d = stake_fixture([0x42; 32]);
            d[0..4].copy_from_slice(&tag.to_le_bytes());
            assert_eq!(delegation_voter(&d), None, "tag {tag}");
        }
    }

    #[test]
    fn short_data_is_rejected() {
        let d = stake_fixture([0x42; 32]);
        assert_eq!(delegation_voter(&d[..155]), None); // voter truncated
        assert_eq!(delegation_voter(&d[..4]), None);
        assert_eq!(delegation_voter(&[]), None);
        // Exactly enough bytes for the voter read still parses.
        assert_eq!(delegation_voter(&d[..156]), Some([0x42; 32]));
    }

    /// Round-trip PIN against the real upstream type serialized with the stake program's own
    /// codec (bincode = the on-chain account encoding; the interface's borsh impl is
    /// hand-written to match it byte-for-byte).
    #[test]
    fn round_trip_pins_the_real_stake_state_v2_layout() {
        use solana_stake_interface::stake_flags::StakeFlags;
        use solana_stake_interface::state::{
            Authorized, Delegation, Lockup, Meta, Stake, StakeStateV2,
        };

        let voter = solana_pubkey::Pubkey::new_from_array([0x42; 32]);
        #[allow(deprecated)] // warmup_cooldown_rate / rent_exempt_reserve: layout, not use
        let state = StakeStateV2::Stake(
            Meta {
                rent_exempt_reserve: 2_282_880,
                authorized: Authorized {
                    staker: solana_pubkey::Pubkey::new_from_array([0xAA; 32]),
                    withdrawer: solana_pubkey::Pubkey::new_from_array([0xBB; 32]),
                },
                lockup: Lockup {
                    unix_timestamp: -1,
                    epoch: 7,
                    custodian: solana_pubkey::Pubkey::new_from_array([0xCC; 32]),
                },
            },
            Stake {
                delegation: Delegation {
                    voter_pubkey: voter,
                    stake: 5_000_000_000,
                    activation_epoch: 300,
                    deactivation_epoch: u64::MAX,
                    warmup_cooldown_rate: 0.0,
                },
                credits_observed: 12_345,
            },
            StakeFlags::empty(),
        );
        // The stake program writes bincode into the fixed 200-byte account buffer.
        let mut account_data = [0u8; 200];
        bincode::serialize_into(&mut account_data[..], &state).unwrap();
        assert_eq!(delegation_voter(&account_data), Some([0x42; 32]));

        // And the non-Stake arms fail closed through the same real serializer.
        let initialized = StakeStateV2::Initialized(Meta::default());
        let mut account_data = [0u8; 200];
        bincode::serialize_into(&mut account_data[..], &initialized).unwrap();
        assert_eq!(delegation_voter(&account_data), None);
    }

    proptest! {
        /// Arbitrary bytes never panic.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = delegation_voter(&data);
        }
    }
}
