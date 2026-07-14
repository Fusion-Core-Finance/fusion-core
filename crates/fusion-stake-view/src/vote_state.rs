//! Pure byte-parsing of a native Vote account for validator eligibility.
//!
//! Exposes exactly what the Allocation Controller's eligibility check needs — [`VoteSample`]:
//! the `commission`, whether a given (prior completed) epoch had positive credit growth in
//! `epoch_credits`, and a freshness slot (the latest landed-vote slot, falling back to the
//! `last_timestamp` slot when the vote tower is empty). Everything else in the account is
//! bounds-check-walked and skipped.
//!
//! ## Verified against
//!
//! The layout below was verified against the REAL agave sources vendored in this workspace's
//! Cargo.lock (`solana-vote-interface 2.2.6`, the solana 2.3 line used by anchor-lang 0.32 /
//! solana-program 2.3.0):
//!
//! - `src/state/vote_state_versions.rs` — enum variant order, hence the bincode u32 LE version
//!   tag at offset 0: `0 = V0_23_5`, `1 = V1_14_11`, `2 = Current` (a.k.a. V3).
//! - `src/state/vote_state_v3.rs` (`deserialize_into_ptr` + `mod vote_state_deserialize`) — the
//!   reference field walk this parser mirrors, including the single V1_14_11/V3 difference:
//!   vote entries are 12-byte `Lockout`s vs 13-byte `LandedVote`s (a leading `latency` byte).
//! - `src/state/mod.rs` — `Lockout { slot u64, confirmation_count u32 }`, `LandedVote { latency
//!   u8, lockout Lockout }`, `CircBuf` (`MAX_ITEMS = 32`, plus `idx usize` and `is_empty bool`),
//!   `BlockTimestamp { slot u64, timestamp i64 }`.
//! - `src/authorized_voters.rs` — `BTreeMap<Epoch, Pubkey>`, bincode-serialized as a u64 count
//!   followed by `(epoch u64, pubkey 32)` pairs.
//! - `solana-serialize-utils 2.2.1` `src/cursor.rs` `read_option_u64` — the `Option<u64>` tag
//!   byte must be exactly 0 or 1; anything else is invalid account data (mirrored here).
//!
//! The account payload is bincode (little-endian, fixed-int, u64 collection lengths, u32 enum
//! tags), zero-padded to the fixed account size (3731 bytes for V1_14_11, 3762 for V3); this
//! parser reads only the payload prefix, so trailing padding is ignored.
//!
//! ## Byte walk (offsets after the 4-byte version tag are fixed only through `commission`)
//!
//! | offset | size | field |
//! |--------|------|-------|
//! | 0      | 4    | version tag (u32 LE) — only 1 (V1_14_11) and 2 (V3) accepted |
//! | 4      | 32   | `node_pubkey` (skipped) |
//! | 36     | 32   | `authorized_withdrawer` (skipped) |
//! | 68     | 1    | `commission` |
//! | 69     | 8+   | `votes`: u64 count, then count × (12 or 13)-byte entries, oldest→newest; the slot of the LAST entry is the latest landed vote |
//! | …      | 1/9  | `root_slot`: `Option<u64>` (tag byte strictly 0 or 1) |
//! | …      | 8+   | `authorized_voters`: u64 count, then count × 40-byte `(epoch u64, pubkey)` |
//! | …      | 1545 | `prior_voters`: FIXED block — 32 × `(pubkey 32, epoch_start u64, epoch_end u64)` + `idx` u64 + `is_empty` u8 |
//! | …      | 8+   | `epoch_credits`: u64 count, then count × 24-byte `(epoch u64, credits u64, prev_credits u64)` |
//! | …      | 16   | `last_timestamp`: `{ slot u64, timestamp i64 }` |
//!
//! ## Fail-closed versioning
//!
//! Any other version tag returns [`VoteStateError::UnsupportedVersion`] — the validator simply
//! reads as ineligible. That covers `0` (V0_23_5, extinct on mainnet and rejected by agave's own
//! BPF-side deserializer) and `3` (the VoteStateV4 tag agave 3.x introduces for SIMD-0185).
//! **Follow-up:** when the cluster migrates vote accounts to V4, add its walk here; until then
//! failing closed is the safe behavior, not a parse bug.
//!
//! The runtime owner check (`account.owner == solana_vote_program`) is the caller's job.

/// Bincode version tag for `VoteStateVersions::V1_14_11` (12-byte vote entries, no latency).
pub const VERSION_TAG_V1_14_11: u32 = 1;
/// Bincode version tag for `VoteStateVersions::Current`, a.k.a. V3 (13-byte vote entries).
pub const VERSION_TAG_V3: u32 = 2;

const COMMISSION_OFFSET: usize = 68; // 4 (tag) + 32 (node_pubkey) + 32 (authorized_withdrawer)
const VOTES_COUNT_OFFSET: usize = 69;
const LOCKOUT_BYTES: u64 = 12; // { slot u64, confirmation_count u32 }
const LANDED_VOTE_BYTES: u64 = 13; // { latency u8 } + Lockout
const AUTHORIZED_VOTER_ENTRY_BYTES: u64 = 40; // (epoch u64, pubkey 32)
const PRIOR_VOTERS_BYTES: u64 = 32 * 48 + 8 + 1; // 32 × (pubkey, u64, u64) + idx u64 + is_empty u8
const EPOCH_CREDITS_ENTRY_BYTES: u64 = 24; // (epoch u64, credits u64, prev_credits u64)
const LAST_TIMESTAMP_BYTES: u64 = 16; // { slot u64, timestamp i64 }

/// Parse failure. Every variant means "this validator is ineligible this pass" (fail-closed) —
/// never a panic, never a hard revert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VoteStateError {
    /// The buffer ended before a field the walk needed (truncated / not a vote account).
    TooShort,
    /// Version tag other than V1_14_11 / V3 — uninitialized (all-zero ⇒ tag 0), the extinct
    /// V0_23_5, or a future version (V4 = tag 3) this parser doesn't know. Fail-closed.
    UnsupportedVersion,
    /// Structurally invalid claims: an `Option` tag byte other than 0/1, or collection lengths
    /// whose byte size overflows (adversarial data — real accounts are ≤ a few KB).
    Malformed,
}

/// The eligibility-relevant view of a vote account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoteSample {
    /// Rewards commission percentage (0–100 on well-formed accounts; not clamped here).
    pub commission: u8,
    /// `true` iff `epoch_credits` holds an entry for the queried epoch with
    /// `credits > prev_credits` — i.e. the validator actually earned credits that epoch. A
    /// missing entry reads as `false` (no participation ⇒ ineligible), per fail-closed policy.
    pub prior_epoch_credit_growth: bool,
    /// Freshness signal: the slot of the latest landed vote (the LAST tower entry — agave
    /// appends new votes at the back), or `last_timestamp.slot` when the tower is empty (both
    /// are compared via `max`, so a stale-but-present tower can't mask a newer timestamp).
    /// The caller compares this against the current slot to reject dormant validators.
    pub freshness_slot: u64,
}

/// `u64`-cursor wrappers over the crate's bounds-checked readers (the walk is done in `u64` so
/// adversarial collection counts can't overflow `usize` arithmetic on any target).
fn u8_at(data: &[u8], off: u64) -> Option<u8> {
    crate::bytes::u8_at(data, usize::try_from(off).ok()?)
}

fn u64_at(data: &[u8], off: u64) -> Option<u64> {
    crate::bytes::u64_le(data, usize::try_from(off).ok()?)
}

/// `Ok` iff the first `end` bytes exist.
fn ensure_within(data: &[u8], end: u64) -> Result<(), VoteStateError> {
    if end <= data.len() as u64 {
        Ok(())
    } else {
        Err(VoteStateError::TooShort)
    }
}

/// Skip a bincode collection at `cur` (u64 LE count, then `entry_bytes`-wide entries), returning
/// the offset past it. Checked arithmetic throughout: a count like `u64::MAX` fails as
/// `Malformed`/`TooShort`, never overflows.
fn skip_counted(data: &[u8], cur: u64, entry_bytes: u64) -> Result<u64, VoteStateError> {
    let count = u64_at(data, cur).ok_or(VoteStateError::TooShort)?;
    let entries_start = cur.checked_add(8).ok_or(VoteStateError::Malformed)?;
    let total = count.checked_mul(entry_bytes).ok_or(VoteStateError::Malformed)?;
    let end = entries_start.checked_add(total).ok_or(VoteStateError::Malformed)?;
    ensure_within(data, end)?;
    Ok(end)
}

/// Parse a vote account and answer the eligibility questions for `prior_epoch` (the caller
/// passes the last COMPLETED epoch, i.e. `Clock::epoch.saturating_sub(1)` — this parser has no
/// clock and takes the target epoch explicitly).
///
/// Walks the variable-width fields with checked arithmetic and bounds checks only; adversarial
/// lengths and truncations return `Err`, never panic or overflow. Trailing bytes after
/// `last_timestamp` (the account's zero padding) are ignored.
pub fn parse(data: &[u8], prior_epoch: u64) -> Result<VoteSample, VoteStateError> {
    let tag = crate::bytes::u32_le(data, 0).ok_or(VoteStateError::TooShort)?;
    let has_latency = match tag {
        VERSION_TAG_V1_14_11 => false,
        VERSION_TAG_V3 => true,
        _ => return Err(VoteStateError::UnsupportedVersion),
    };

    let commission =
        crate::bytes::u8_at(data, COMMISSION_OFFSET).ok_or(VoteStateError::TooShort)?;

    // -- votes: u64 count + fixed-width entries; the LAST entry is the newest landed vote --
    let vote_count = u64_at(data, VOTES_COUNT_OFFSET as u64).ok_or(VoteStateError::TooShort)?;
    let entry_bytes = if has_latency { LANDED_VOTE_BYTES } else { LOCKOUT_BYTES };
    let votes_start = (VOTES_COUNT_OFFSET as u64).checked_add(8).ok_or(VoteStateError::Malformed)?;
    let votes_total = vote_count.checked_mul(entry_bytes).ok_or(VoteStateError::Malformed)?;
    let votes_end = votes_start.checked_add(votes_total).ok_or(VoteStateError::Malformed)?;
    ensure_within(data, votes_end)?;
    let last_vote_slot = if vote_count > 0 {
        // Last entry starts at votes_end - entry_bytes (≥ votes_start since count ≥ 1); in a
        // V3 LandedVote the slot sits after the 1-byte latency.
        let slot_off = votes_end
            .checked_sub(entry_bytes)
            .and_then(|o| o.checked_add(if has_latency { 1 } else { 0 }))
            .ok_or(VoteStateError::Malformed)?;
        Some(u64_at(data, slot_off).ok_or(VoteStateError::TooShort)?)
    } else {
        None
    };
    let mut cur = votes_end;

    // -- root_slot: Option<u64>; tag byte strictly 0 or 1 (agave's read_option_u64) --
    let opt_tag = u8_at(data, cur).ok_or(VoteStateError::TooShort)?;
    cur = cur.checked_add(1).ok_or(VoteStateError::Malformed)?;
    match opt_tag {
        0 => {}
        1 => {
            cur = cur.checked_add(8).ok_or(VoteStateError::Malformed)?;
            ensure_within(data, cur)?;
        }
        _ => return Err(VoteStateError::Malformed),
    }

    // -- authorized_voters: u64 count + (epoch, pubkey) entries; skipped --
    cur = skip_counted(data, cur, AUTHORIZED_VOTER_ENTRY_BYTES)?;

    // -- prior_voters: FIXED-width circular buffer block; skipped --
    cur = cur.checked_add(PRIOR_VOTERS_BYTES).ok_or(VoteStateError::Malformed)?;
    ensure_within(data, cur)?;

    // -- epoch_credits: u64 count + (epoch, credits, prev_credits) entries --
    let credit_count = u64_at(data, cur).ok_or(VoteStateError::TooShort)?;
    cur = cur.checked_add(8).ok_or(VoteStateError::Malformed)?;
    let credits_total =
        credit_count.checked_mul(EPOCH_CREDITS_ENTRY_BYTES).ok_or(VoteStateError::Malformed)?;
    let credits_end = cur.checked_add(credits_total).ok_or(VoteStateError::Malformed)?;
    ensure_within(data, credits_end)?;
    // The runtime keeps entries ascending by unique epoch (≤ 64 of them), so at most one entry
    // can match; scanning all of them makes no ordering assumption (a duplicated epoch — which
    // the runtime never produces — would resolve to the last entry's answer).
    let mut prior_epoch_credit_growth = false;
    let mut entry = cur;
    while entry < credits_end {
        let epoch = u64_at(data, entry).ok_or(VoteStateError::TooShort)?;
        if epoch == prior_epoch {
            let credits = u64_at(data, entry.checked_add(8).ok_or(VoteStateError::Malformed)?)
                .ok_or(VoteStateError::TooShort)?;
            let prev_credits =
                u64_at(data, entry.checked_add(16).ok_or(VoteStateError::Malformed)?)
                    .ok_or(VoteStateError::TooShort)?;
            prior_epoch_credit_growth = credits > prev_credits;
        }
        entry = entry.checked_add(EPOCH_CREDITS_ENTRY_BYTES).ok_or(VoteStateError::Malformed)?;
    }
    cur = credits_end;

    // -- last_timestamp: { slot u64, timestamp i64 } — the walk must fully fit the buffer --
    let last_timestamp_slot = u64_at(data, cur).ok_or(VoteStateError::TooShort)?;
    ensure_within(data, cur.checked_add(LAST_TIMESTAMP_BYTES).ok_or(VoteStateError::Malformed)?)?;

    let freshness_slot = match last_vote_slot {
        Some(slot) => slot.max(last_timestamp_slot),
        None => last_timestamp_slot,
    };
    Ok(VoteSample { commission, prior_epoch_credit_growth, freshness_slot })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use solana_pubkey::Pubkey;
    use solana_vote_interface::authorized_voters::AuthorizedVoters;
    use solana_vote_interface::state::{
        BlockTimestamp, LandedVote, Lockout, VoteState1_14_11, VoteStateV3, VoteStateVersions,
    };

    /// A realistic V3 state built from the REAL agave types (this is the whole point of the
    /// dev-dependency: a fixture serialized by the reference implementation, not by our own
    /// mental model of it).
    fn v3_state(
        commission: u8,
        vote_slots: &[u64],
        epoch_credits: &[(u64, u64, u64)],
        ts_slot: u64,
    ) -> VoteStateV3 {
        VoteStateV3 {
            commission,
            votes: vote_slots
                .iter()
                .enumerate()
                .map(|(i, &slot)| LandedVote {
                    latency: 2,
                    lockout: Lockout::new_with_confirmation_count(
                        slot,
                        (vote_slots.len() as u32).saturating_sub(i as u32),
                    ),
                })
                .collect(),
            root_slot: Some(4_242),
            authorized_voters: AuthorizedVoters::new(7, Pubkey::new_from_array([3u8; 32])),
            epoch_credits: epoch_credits.to_vec(),
            last_timestamp: BlockTimestamp { slot: ts_slot, timestamp: 1_700_000_000 },
            ..VoteStateV3::default()
        }
    }

    /// Exact bincode payload (no trailing padding).
    fn serialize_exact(versioned: &VoteStateVersions) -> Vec<u8> {
        bincode::serialize(versioned).unwrap()
    }

    /// Account-realistic buffer: fixed size (V3's 3762 covers both versions), zero-padded past
    /// the payload — exactly how the vote program stores it.
    fn serialize_account(versioned: &VoteStateVersions) -> Vec<u8> {
        let mut buf = vec![0u8; VoteStateV3::size_of()];
        VoteStateV3::serialize(versioned, &mut buf).unwrap();
        buf
    }

    fn v3_fixture() -> VoteStateVersions {
        VoteStateVersions::new_current(v3_state(
            5,
            &[100, 200, 300],
            &[(6, 90, 80), (7, 100, 90), (8, 100, 100)],
            290,
        ))
    }

    #[test]
    fn v3_fixture_fields() {
        let buf = serialize_account(&v3_fixture());
        let s = parse(&buf, 7).unwrap();
        assert_eq!(s.commission, 5);
        assert!(s.prior_epoch_credit_growth); // (7, 100, 90): 100 > 90
        assert_eq!(s.freshness_slot, 300); // last landed vote beats last_timestamp.slot 290

        // Zero credit growth (credits == prev_credits) is NOT growth.
        assert!(!parse(&buf, 8).unwrap().prior_epoch_credit_growth);
        // An epoch with no entry at all reads as no growth (fail-closed).
        assert!(!parse(&buf, 5).unwrap().prior_epoch_credit_growth);
    }

    /// The V1_14_11 walk (12-byte entries, no latency byte) — built via agave's own
    /// V3→V1_14_11 conversion, so the fixture is the reference layout again.
    #[test]
    fn v1_14_11_fixture_fields() {
        let v1: VoteState1_14_11 = v3_state(
            9,
            &[100, 200, 355],
            &[(41, 500, 480), (42, 500, 500)],
            310,
        )
        .into();
        let buf = serialize_account(&VoteStateVersions::V1_14_11(Box::new(v1)));
        assert_eq!(crate::bytes::u32_le(&buf, 0), Some(VERSION_TAG_V1_14_11));
        let s = parse(&buf, 41).unwrap();
        assert_eq!(s.commission, 9);
        assert!(s.prior_epoch_credit_growth);
        assert_eq!(s.freshness_slot, 355);
        assert!(!parse(&buf, 42).unwrap().prior_epoch_credit_growth);
    }

    /// Empty tower (e.g. a vote account that has never voted): freshness falls back to
    /// `last_timestamp.slot`.
    #[test]
    fn empty_votes_fall_back_to_last_timestamp_slot() {
        let buf = serialize_account(&VoteStateVersions::new_current(v3_state(
            0,
            &[],
            &[(7, 10, 5)],
            999,
        )));
        assert_eq!(parse(&buf, 7).unwrap().freshness_slot, 999);
    }

    /// The exact payload and the zero-padded account buffer parse identically — the walk stops
    /// at `last_timestamp` and never depends on padding.
    #[test]
    fn padding_is_ignored() {
        let versioned = v3_fixture();
        assert_eq!(
            parse(&serialize_exact(&versioned), 7),
            parse(&serialize_account(&versioned), 7)
        );
    }

    /// Documented fixed offsets hold against the reference serialization: version tag @0,
    /// commission @68, votes count @69.
    #[test]
    fn documented_fixed_offsets_match_reference_serialization() {
        let exact = serialize_exact(&v3_fixture());
        assert_eq!(&exact[0..4], &VERSION_TAG_V3.to_le_bytes());
        assert_eq!(exact[COMMISSION_OFFSET], 5);
        assert_eq!(&exact[VOTES_COUNT_OFFSET..VOTES_COUNT_OFFSET + 8], &3u64.to_le_bytes());
    }

    /// Fail-closed versioning: tag 0 (V0_23_5 / all-zero uninitialized data), tag 3 (agave 3.x
    /// VoteStateV4), and garbage tags all Err cleanly.
    #[test]
    fn unsupported_version_tags_err() {
        let mut buf = vec![0u8; 3762]; // all zeros ⇒ tag 0 (V0_23_5 / uninitialized)
        assert_eq!(parse(&buf, 7), Err(VoteStateError::UnsupportedVersion));
        buf[0..4].copy_from_slice(&3u32.to_le_bytes()); // the future V4 tag
        assert_eq!(parse(&buf, 7), Err(VoteStateError::UnsupportedVersion));
        buf[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(parse(&buf, 7), Err(VoteStateError::UnsupportedVersion));
    }

    /// EVERY strict prefix of a valid payload errs (and never panics): the walk verifies that
    /// the whole structure, through `last_timestamp`, fits the buffer.
    #[test]
    fn every_truncation_errs() {
        let exact = serialize_exact(&v3_fixture());
        assert!(parse(&exact, 7).is_ok());
        for len in 0..exact.len() {
            assert!(parse(&exact[..len], 7).is_err(), "prefix of {len} bytes must not parse");
        }
    }

    /// Adversarial collection counts and option tags: Err, never overflow/panic/wrap.
    #[test]
    fn adversarial_lengths_and_tags_err() {
        // votes count u64::MAX — count*13 must not overflow.
        let mut buf = serialize_account(&v3_fixture());
        buf[VOTES_COUNT_OFFSET..VOTES_COUNT_OFFSET + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(parse(&buf, 7).is_err());

        // Rebuild with an empty tower so downstream offsets are easy: root_slot tag follows
        // immediately after the count.
        let empty_tower = VoteStateVersions::new_current(v3_state(1, &[], &[(7, 2, 1)], 50));
        let exact = serialize_exact(&empty_tower);
        let root_tag_off = VOTES_COUNT_OFFSET + 8;
        assert_eq!(exact[root_tag_off], 1); // Some(root_slot) in the fixture

        let mut bad_opt = exact.clone();
        bad_opt[root_tag_off] = 2; // invalid Option tag
        assert_eq!(parse(&bad_opt, 7), Err(VoteStateError::Malformed));

        // authorized_voters count u64::MAX (count*40 must not overflow).
        let voters_count_off = root_tag_off + 9; // tag byte + Some(u64)
        let mut bad_voters = exact.clone();
        bad_voters[voters_count_off..voters_count_off + 8]
            .copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(parse(&bad_voters, 7).is_err());

        // epoch_credits count over-claiming the buffer.
        let credits_count_off = voters_count_off + 8 + 40 + (32 * 48 + 8 + 1);
        let mut bad_credits = exact;
        bad_credits[credits_count_off..credits_count_off + 8]
            .copy_from_slice(&1_000_000u64.to_le_bytes());
        assert!(parse(&bad_credits, 7).is_err());
    }

    proptest! {
        /// Arbitrary bytes never panic — the walk degrades to Err on anything malformed.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..4096),
                                       epoch in any::<u64>()) {
            let _ = parse(&data, epoch);
        }

        /// Single-byte corruption of a real account buffer never panics.
        #[test]
        fn corrupted_fixture_never_panics(idx in 0usize..3762, byte in any::<u8>()) {
            let mut buf = serialize_account(&v3_fixture());
            buf[idx] = byte;
            let _ = parse(&buf, 7);
        }
    }
}
