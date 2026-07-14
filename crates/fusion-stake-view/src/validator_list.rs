//! Pure byte-parsing of an SPL Stake Pool `ValidatorList` account.
//!
//! The Allocation Controller walks this list to see which validators the pool currently stakes
//! to, with how much active/transient stake and in which lifecycle status. The account key must
//! first be bound to `StakePool.validator_list` (see [`crate::stake_pool`]) and the runtime
//! owner checked by the caller.
//!
//! Layout from the pinned upstream (solana-program/stake-pool @ a27629b = spl-stake-pool v2.0.3,
//! `program/src/state.rs`): `ValidatorList { header: ValidatorListHeader, validators:
//! Vec<ValidatorStakeInfo> }`, borsh-serialized by a native program (no Anchor discriminator).
//! The header is `{ account_type: u8, max_validators: u32 }`, then borsh's `Vec` length prefix:
//!
//! | offset | size | field |
//! |--------|------|-------|
//! | 0      | 1    | `account_type` (`2` = ValidatorList) |
//! | 1      | 4    | `max_validators` (u32 LE) — allocated capacity |
//! | 5      | 4    | vec length (u32 LE) — entries currently in the list |
//! | 9      | 73·i | entry `i` (see below) |
//!
//! `ValidatorStakeInfo` is 73 bytes (all integers are byte-aligned `Pod*` little-endian types
//! upstream, so borsh emits them back-to-back with no padding):
//!
//! | rel. offset | size | field |
//! |-------------|------|-------|
//! | +0          | 8    | `active_stake_lamports` (u64 LE) |
//! | +8          | 8    | `transient_stake_lamports` (u64 LE) |
//! | +16         | 8    | `last_update_epoch` (u64 LE) |
//! | +24         | 8    | `transient_seed_suffix` (u64 LE) |
//! | +32         | 4    | `unused` (u32 LE, not exposed) |
//! | +36         | 4    | `validator_seed_suffix` (u32 LE, 0 = none) |
//! | +40         | 1    | `status` (see [`StakeStatus`]) |
//! | +41         | 32   | `vote_account_address` |
//!
//! The account is pre-allocated to `max_validators` capacity, so real data extends well past the
//! `len` in-use entries — trailing capacity bytes are expected and ignored. A `len` that CLAIMS
//! more entries than the buffer holds is malformed and fails closed ([`ValidatorListError::EntriesTruncated`]).

/// `AccountType::ValidatorList` discriminant (borsh enum variant index at byte 0).
pub const ACCOUNT_TYPE_VALIDATOR_LIST: u8 = 2;

const MAX_VALIDATORS_OFFSET: usize = 1;
const VEC_LEN_OFFSET: usize = 5;
const ENTRIES_OFFSET: usize = 9;
const ENTRY_LEN: usize = 73;

/// Parse failure. Every variant degrades (the pool is skipped this pass), never panics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidatorListError {
    /// Data shorter than the 9-byte header.
    TooShort,
    /// Byte 0 is not `AccountType::ValidatorList` (wrong account type).
    NotValidatorList,
    /// The vec length claims more 73-byte entries than the buffer actually holds.
    EntriesTruncated,
}

/// The validated list header: [`len`](Self::len) in-use entries out of
/// [`max_validators`](Self::max_validators) allocated capacity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidatorListHeader {
    /// Allocated capacity (entries the account was sized for).
    pub max_validators: u32,
    /// Entries currently in the list; [`entry_at`] accepts indices `0..len`.
    pub len: u32,
}

/// `ValidatorStakeInfo::status` — the validator's lifecycle inside the pool. Only
/// [`Active`](Self::Active) validators are candidates for new stake direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StakeStatus {
    Active = 0,
    DeactivatingTransient = 1,
    ReadyForRemoval = 2,
    DeactivatingValidator = 3,
    DeactivatingAll = 4,
}

impl StakeStatus {
    /// `None` for a discriminant this version doesn't know — the caller treats the validator as
    /// ineligible (fail-closed) rather than guessing.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Active),
            1 => Some(Self::DeactivatingTransient),
            2 => Some(Self::ReadyForRemoval),
            3 => Some(Self::DeactivatingValidator),
            4 => Some(Self::DeactivatingAll),
            _ => None,
        }
    }
}

/// One `ValidatorStakeInfo` entry (the `unused` field is not exposed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidatorEntry {
    pub active_stake_lamports: u64,
    pub transient_stake_lamports: u64,
    pub last_update_epoch: u64,
    pub transient_seed_suffix: u64,
    /// 0 = default seed; nonzero = non-default validator stake seed.
    pub validator_seed_suffix: u32,
    /// Raw status byte; decode with [`StakeStatus::from_u8`] (unknown values fail closed there,
    /// not here — exposing the raw byte keeps the parse itself total over valid buffers).
    pub status: u8,
    /// Raw key bytes; compare with `Pubkey::to_bytes()`.
    pub vote_account_address: [u8; 32],
}

impl ValidatorEntry {
    /// The decoded [`StakeStatus`], `None` for an unknown discriminant.
    pub fn stake_status(&self) -> Option<StakeStatus> {
        StakeStatus::from_u8(self.status)
    }
}

/// Validate the header and that the buffer really holds the `len` entries it declares.
/// This is the `len` / `capacity` accessor: [`ValidatorListHeader::len`] and
/// [`ValidatorListHeader::max_validators`].
pub fn parse_header(data: &[u8]) -> Result<ValidatorListHeader, ValidatorListError> {
    if data.len() < ENTRIES_OFFSET {
        return Err(ValidatorListError::TooShort);
    }
    if data[0] != ACCOUNT_TYPE_VALIDATOR_LIST {
        return Err(ValidatorListError::NotValidatorList);
    }
    let max_validators =
        crate::bytes::u32_le(data, MAX_VALIDATORS_OFFSET).ok_or(ValidatorListError::TooShort)?;
    let len = crate::bytes::u32_le(data, VEC_LEN_OFFSET).ok_or(ValidatorListError::TooShort)?;
    // Fail closed on a length that over-claims the buffer (adversarial or truncated account).
    // u64 arithmetic: (u32::MAX as u64) * 73 + 9 cannot overflow, so no wrap is possible.
    let declared_end = (len as u64)
        .checked_mul(ENTRY_LEN as u64)
        .and_then(|b| b.checked_add(ENTRIES_OFFSET as u64))
        .ok_or(ValidatorListError::EntriesTruncated)?;
    if declared_end > data.len() as u64 {
        return Err(ValidatorListError::EntriesTruncated);
    }
    Ok(ValidatorListHeader { max_validators, len })
}

/// The `i`-th in-use entry, or `None` if the header is invalid or `i >= len`. Every read is
/// bounds-checked; a malformed account can never panic the caller.
pub fn entry_at(data: &[u8], i: u32) -> Option<ValidatorEntry> {
    let header = parse_header(data).ok()?;
    if i >= header.len {
        return None;
    }
    // In-bounds by the parse_header declared_end check; every read below is still checked.
    let base = ENTRIES_OFFSET.checked_add((i as usize).checked_mul(ENTRY_LEN)?)?;
    Some(ValidatorEntry {
        active_stake_lamports: crate::bytes::u64_le(data, base)?,
        transient_stake_lamports: crate::bytes::u64_le(data, base.checked_add(8)?)?,
        last_update_epoch: crate::bytes::u64_le(data, base.checked_add(16)?)?,
        transient_seed_suffix: crate::bytes::u64_le(data, base.checked_add(24)?)?,
        validator_seed_suffix: crate::bytes::u32_le(data, base.checked_add(36)?)?,
        status: crate::bytes::u8_at(data, base.checked_add(40)?)?,
        vote_account_address: crate::bytes::array32(data, base.checked_add(41)?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// One 73-byte entry with distinct recognizable values derived from `seed`.
    fn entry_bytes(seed: u8, status: u8) -> [u8; ENTRY_LEN] {
        let mut e = [0u8; ENTRY_LEN];
        e[0..8].copy_from_slice(&(1_000 + seed as u64).to_le_bytes()); // active
        e[8..16].copy_from_slice(&(2_000 + seed as u64).to_le_bytes()); // transient
        e[16..24].copy_from_slice(&(700 + seed as u64).to_le_bytes()); // last_update_epoch
        e[24..32].copy_from_slice(&(30 + seed as u64).to_le_bytes()); // transient_seed_suffix
        e[32..36].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes()); // unused (must be ignored)
        e[36..40].copy_from_slice(&(40 + seed as u32).to_le_bytes()); // validator_seed_suffix
        e[40] = status;
        e[41..73].copy_from_slice(&[seed; 32]); // vote_account_address
        e
    }

    /// A ValidatorList buffer: header + `entries`, pre-allocated (zero-filled) out to
    /// `capacity` entries like the real account.
    fn list_buf(capacity: u32, entries: &[[u8; ENTRY_LEN]]) -> Vec<u8> {
        let mut buf = vec![0u8; ENTRIES_OFFSET + capacity as usize * ENTRY_LEN];
        buf[0] = ACCOUNT_TYPE_VALIDATOR_LIST;
        buf[1..5].copy_from_slice(&capacity.to_le_bytes());
        buf[5..9].copy_from_slice(&(entries.len() as u32).to_le_bytes());
        for (i, e) in entries.iter().enumerate() {
            let base = ENTRIES_OFFSET + i * ENTRY_LEN;
            buf[base..base + ENTRY_LEN].copy_from_slice(e);
        }
        buf
    }

    #[test]
    fn header_len_and_capacity() {
        let buf = list_buf(5, &[entry_bytes(1, 0), entry_bytes(2, 1), entry_bytes(3, 4)]);
        let h = parse_header(&buf).unwrap();
        assert_eq!(h.max_validators, 5);
        assert_eq!(h.len, 3);
    }

    #[test]
    fn entries_parse_every_field() {
        let buf = list_buf(5, &[entry_bytes(1, 0), entry_bytes(2, 1), entry_bytes(3, 4)]);
        for (i, seed, status) in [(0u32, 1u8, StakeStatus::Active), (1, 2, StakeStatus::DeactivatingTransient), (2, 3, StakeStatus::DeactivatingAll)]
        {
            let e = entry_at(&buf, i).unwrap();
            assert_eq!(e.active_stake_lamports, 1_000 + seed as u64);
            assert_eq!(e.transient_stake_lamports, 2_000 + seed as u64);
            assert_eq!(e.last_update_epoch, 700 + seed as u64);
            assert_eq!(e.transient_seed_suffix, 30 + seed as u64);
            assert_eq!(e.validator_seed_suffix, 40 + seed as u32);
            assert_eq!(e.stake_status(), Some(status));
            assert_eq!(e.vote_account_address, [seed; 32]);
        }
    }

    /// Indices at/after `len` are `None`, even though zeroed capacity bytes exist there — the
    /// in-use window is `0..len`, never the allocation.
    #[test]
    fn index_past_len_is_none() {
        let buf = list_buf(5, &[entry_bytes(1, 0)]);
        assert!(entry_at(&buf, 0).is_some());
        assert_eq!(entry_at(&buf, 1), None);
        assert_eq!(entry_at(&buf, 4), None); // within capacity, past len
        assert_eq!(entry_at(&buf, u32::MAX), None);
    }

    #[test]
    fn empty_list() {
        let buf = list_buf(4, &[]);
        let h = parse_header(&buf).unwrap();
        assert_eq!((h.len, h.max_validators), (0, 4));
        assert_eq!(entry_at(&buf, 0), None);
    }

    #[test]
    fn rejects_wrong_account_type_and_short_data() {
        let mut buf = list_buf(2, &[entry_bytes(1, 0)]);
        buf[0] = 1; // StakePool, not ValidatorList
        assert_eq!(parse_header(&buf), Err(ValidatorListError::NotValidatorList));
        assert_eq!(entry_at(&buf, 0), None);
        assert_eq!(parse_header(&[2, 0, 0, 0]), Err(ValidatorListError::TooShort));
        assert_eq!(parse_header(&[]), Err(ValidatorListError::TooShort));
    }

    /// A `len` claiming more entries than the buffer holds fails closed — no partial reads of a
    /// truncated tail entry.
    #[test]
    fn rejects_overclaiming_len() {
        let mut buf = list_buf(2, &[entry_bytes(1, 0), entry_bytes(2, 0)]);
        buf[5..9].copy_from_slice(&3u32.to_le_bytes()); // claims 3, holds 2
        assert_eq!(parse_header(&buf), Err(ValidatorListError::EntriesTruncated));
        assert_eq!(entry_at(&buf, 0), None);
        buf[5..9].copy_from_slice(&u32::MAX.to_le_bytes()); // absurd claim: no u64 overflow
        assert_eq!(parse_header(&buf), Err(ValidatorListError::EntriesTruncated));
    }

    /// Unknown status discriminants parse (raw byte exposed) but decode to `None` — the
    /// eligibility layer fails closed on them.
    #[test]
    fn unknown_status_fails_closed_in_decode() {
        let buf = list_buf(1, &[entry_bytes(1, 5)]);
        let e = entry_at(&buf, 0).unwrap();
        assert_eq!(e.status, 5);
        assert_eq!(e.stake_status(), None);
        for v in 0..=4u8 {
            assert!(StakeStatus::from_u8(v).is_some());
        }
        assert_eq!(StakeStatus::from_u8(0xFF), None);
    }

    proptest! {
        /// Arbitrary bytes and indices never panic.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..1024),
                                       i in any::<u32>()) {
            let _ = parse_header(&data);
            let _ = entry_at(&data, i);
        }

        /// On a well-formed list every index below `len` yields an entry and every index at or
        /// above it yields `None`.
        #[test]
        fn entry_at_matches_len(n in 0u32..8, extra_capacity in 0u32..4, probe in 0u32..16) {
            let entries: Vec<[u8; ENTRY_LEN]> =
                (0..n).map(|i| entry_bytes(i as u8 + 1, (i % 5) as u8)).collect();
            let buf = list_buf(n + extra_capacity, &entries);
            prop_assert_eq!(parse_header(&buf).unwrap().len, n);
            prop_assert_eq!(entry_at(&buf, probe).is_some(), probe < n);
        }
    }
}
