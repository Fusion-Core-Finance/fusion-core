//! Pure byte-parsing of a fusd-core `Position` account (the user CDP) for the fuSOL Allocation
//! Controller's Preference sync.
//!
//! The Preference sync needs to know, per position: who owns it, which collateral market it is
//! in, how much collateral (`ink`) it holds, and the monotonic `ink_nonce` collateral-change
//! counter (which prevents fungible-share validator-direction reuse). We read those four fields
//! at fixed offsets instead of depending on fusd-core at runtime — this crate stays `no_std` and
//! dependency-free, and the offsets are PINNED by a dev-dependency round-trip test that
//! serializes the real `fusd_core::state::Position` (with its Anchor discriminator) and asserts
//! every exposed field reads back. If fusd-core ever moves a field, that test breaks loudly.
//!
//! Layout: `Position` is a Borsh-serialized Anchor account — an 8-byte discriminator
//! (`sha256("account:Position")[..8]`, pinned as [`POSITION_DISCRIMINATOR`]) followed by the
//! fields in declaration order (borsh emits no padding). Running-sum derivation from
//! fusd-core's `state/position.rs`:
//!
//! | offset | size | field |
//! |--------|------|-------|
//! | 0      | 8    | Anchor discriminator |
//! | 8      | 32   | `owner` |
//! | 40     | 32   | `collateral_mint` |
//! | 72     | 8    | `ink` (u64 LE) — locked collateral, native units |
//! | 80     | 16   | `recorded_debt` (u128) |
//! | 96     | 2    | `user_rate_bps` (u16) |
//! | 98     | 1    | `bump` |
//! | 99     | 8    | `last_debt_update` (i64) |
//! | 107    | 16   | `stake` (u128) |
//! | 123    | 16   | `redist_l_coll_snapshot` (u128) |
//! | 139    | 16   | `redist_l_art_snapshot` (u128) |
//! | 155    | 8    | `reserve_lamports` (u64) |
//! | 163    | 2    | `bucket` (u16) |
//! | 165    | 8    | `coll_surplus` (u64) |
//! | 173    | 8    | `last_rate_adjust_ts` (i64) |
//! | 181    | 8    | `ink_nonce` (u64 LE) — bumps whenever `ink` changes |
//! | 189    | 24   | `_reserved` |
//!
//! Total 213 == `Position::SPACE`. New fields are carved from the HEAD of `_reserved` (per the
//! fusd-core convention), so every offset above is stable for the account's lifetime; the
//! round-trip test also pins the total size, so even a reserved-tail resize is caught.
//!
//! The runtime owner check (`account.owner == fusd-core program id`) is the caller's job.

/// Anchor account discriminator: `sha256("account:Position")[..8]`. Pinned against
/// `fusd_core::state::Position::DISCRIMINATOR` in the round-trip test.
pub const POSITION_DISCRIMINATOR: [u8; 8] = [170, 188, 143, 228, 122, 64, 247, 208];

const OWNER_OFFSET: usize = 8;
const COLLATERAL_MINT_OFFSET: usize = 40;
const INK_OFFSET: usize = 72;
const INK_NONCE_OFFSET: usize = 181;

/// Full `Position::SPACE`. We require the whole account (not just through `ink_nonce`): real
/// accounts are allocated at exactly this size and Anchor realloc only ever grows, so anything
/// shorter is not a genuine `Position`.
pub const POSITION_MIN_LEN: usize = 213;

/// Parse failure. Every variant degrades (the position is skipped by the sync), never panics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionError {
    /// Data shorter than `Position::SPACE` (uninitialized / truncated / wrong account).
    TooShort,
    /// The 8-byte Anchor discriminator is not `Position`'s (wrong account type).
    BadDiscriminator,
}

/// The Preference-sync view of a `Position`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PositionView {
    /// Raw key bytes; compare with `Pubkey::to_bytes()`.
    pub owner: [u8; 32],
    /// The collateral market this position belongs to (part of its PDA seeds).
    pub collateral_mint: [u8; 32],
    /// Locked collateral, native units.
    pub ink: u64,
    /// Monotonic collateral-change nonce: bumps whenever `ink` CHANGES for any reason. The
    /// Preference sync compares it to its stored value to detect collateral movement and
    /// prevent fungible-share validator-direction reuse.
    pub ink_nonce: u64,
}

/// Parse a `Position` account's raw data. Guards (in order): minimum length, then the Anchor
/// discriminator. Reads only fixed offsets. The caller must already have verified the account's
/// runtime owner == the fusd-core program.
pub fn parse(data: &[u8]) -> Result<PositionView, PositionError> {
    if data.len() < POSITION_MIN_LEN {
        return Err(PositionError::TooShort);
    }
    if data[..8] != POSITION_DISCRIMINATOR {
        return Err(PositionError::BadDiscriminator);
    }
    // Every offset below is < POSITION_MIN_LEN ≤ data.len(); `TooShort` is the (unreachable)
    // fallback rather than any panic path.
    Ok(PositionView {
        owner: crate::bytes::array32(data, OWNER_OFFSET).ok_or(PositionError::TooShort)?,
        collateral_mint: crate::bytes::array32(data, COLLATERAL_MINT_OFFSET)
            .ok_or(PositionError::TooShort)?,
        ink: crate::bytes::u64_le(data, INK_OFFSET).ok_or(PositionError::TooShort)?,
        ink_nonce: crate::bytes::u64_le(data, INK_NONCE_OFFSET).ok_or(PositionError::TooShort)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anchor_lang::{AccountSerialize, Discriminator};
    use proptest::prelude::*;

    /// A `Position` with EVERY field set to a distinct nonzero pattern, so that any
    /// field-reorder / width change in fusd-core shifts the bytes and breaks the assertions
    /// below — this is the layout pin the module doc promises.
    fn realistic_position() -> fusd_core::state::Position {
        fusd_core::state::Position {
            owner: anchor_lang::prelude::Pubkey::new_from_array([0x11; 32]),
            collateral_mint: anchor_lang::prelude::Pubkey::new_from_array([0x22; 32]),
            ink: 0x0102_0304_0506_0708,
            recorded_debt: 0x1112_1314_1516_1718_191A_1B1C_1D1E_1F20,
            user_rate_bps: 0x2122,
            bump: 0x33,
            last_debt_update: -0x4142_4344_4546_4748,
            stake: 0x5152_5354_5556_5758_595A_5B5C_5D5E_5F60,
            redist_l_coll_snapshot: 0x6162_6364_6566_6768_696A_6B6C_6D6E_6F70,
            redist_l_art_snapshot: 0x7172_7374_7576_7778_797A_7B7C_7D7E_7F80,
            reserve_lamports: 0x8182_8384_8586_8788,
            bucket: 0x9192,
            coll_surplus: 0xA1A2_A3A4_A5A6_A7A8,
            last_rate_adjust_ts: -0x31B2_B3B4_B5B6_B7B8,
            ink_nonce: 0xC1C2_C3C4_C5C6_C7C8,
            _reserved: [0xEE; 24],
        }
    }

    fn serialize(p: &fusd_core::state::Position) -> Vec<u8> {
        let mut buf = Vec::new();
        p.try_serialize(&mut buf).unwrap(); // Anchor: discriminator + borsh fields
        buf
    }

    /// THE pin: serialize the real `fusd_core::state::Position` through Anchor and read every
    /// exposed field back through our fixed offsets. Breaks loudly if fusd-core ever moves a
    /// field, resizes one, or changes the account's total SPACE.
    #[test]
    fn round_trips_real_fusd_core_position() {
        let buf = serialize(&realistic_position());
        assert_eq!(buf.len(), fusd_core::state::Position::SPACE);
        assert_eq!(buf.len(), POSITION_MIN_LEN);

        let v = parse(&buf).unwrap();
        assert_eq!(v.owner, [0x11; 32]);
        assert_eq!(v.collateral_mint, [0x22; 32]);
        assert_eq!(v.ink, 0x0102_0304_0506_0708);
        assert_eq!(v.ink_nonce, 0xC1C2_C3C4_C5C6_C7C8);
    }

    /// The hardcoded discriminator equals the one Anchor derives for `Position`.
    #[test]
    fn discriminator_matches_fusd_core() {
        assert_eq!(fusd_core::state::Position::DISCRIMINATOR, &POSITION_DISCRIMINATOR[..]);
    }

    #[test]
    fn rejects_short_data() {
        let buf = serialize(&realistic_position());
        assert_eq!(parse(&buf[..buf.len() - 1]), Err(PositionError::TooShort));
        assert_eq!(parse(&[]), Err(PositionError::TooShort));
    }

    /// Longer-than-SPACE data still parses (Anchor realloc only grows; offsets are stable).
    #[test]
    fn accepts_grown_account() {
        let mut buf = serialize(&realistic_position());
        buf.extend_from_slice(&[0xFF; 32]);
        assert_eq!(parse(&buf).unwrap().ink, 0x0102_0304_0506_0708);
    }

    #[test]
    fn rejects_wrong_discriminator() {
        let mut buf = serialize(&realistic_position());
        buf[0] ^= 0x01;
        assert_eq!(parse(&buf), Err(PositionError::BadDiscriminator));
        // All-zero discriminator (uninitialized account data) is also rejected.
        let zeroed = vec![0u8; POSITION_MIN_LEN];
        assert_eq!(parse(&zeroed), Err(PositionError::BadDiscriminator));
    }

    proptest! {
        /// Arbitrary bytes never panic.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = parse(&data);
        }
    }
}
