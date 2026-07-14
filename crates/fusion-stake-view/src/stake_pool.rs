//! Pure byte-parsing of an SPL Stake Pool `StakePool` account for the fuSOL Allocation
//! Controller.
//!
//! The Allocation Controller binds the pool's `validator_list` / `reserve_stake` / `pool_mint`
//! keys, checks `last_update_epoch` freshness, and reads the `total_lamports /
//! pool_token_supply` exchange rate. We read those fields at **verified byte offsets** rather
//! than pulling the `spl-stake-pool` crate (no extra deps, no version-lock churn, no zero-copy
//! alignment surprises) — exactly as fusd-core's `stake_pool.rs` does; the shared offsets are
//! cross-checked against that parser in tests.
//!
//! Layout: `spl_stake_pool::state::StakePool` is borsh-serialized by a NATIVE program, so there
//! is **no 8-byte Anchor discriminator** — byte 0 is the `AccountType` enum (`1 = StakePool`)
//! and the rest is a plain running sum (borsh emits no padding). Field-order derivation from the
//! pinned upstream (solana-program/stake-pool @ a27629b = spl-stake-pool v2.0.3,
//! `program/src/state.rs`):
//!
//! | offset | size | field |
//! |--------|------|-------|
//! | 0      | 1    | `account_type` (`0`=Uninitialized, `1`=StakePool, `2`=ValidatorList) |
//! | 1      | 32   | `manager` |
//! | 33     | 32   | `staker` |
//! | 65     | 32   | `stake_deposit_authority` |
//! | 97     | 1    | `stake_withdraw_bump_seed` |
//! | 98     | 32   | `validator_list` |
//! | 130    | 32   | `reserve_stake` |
//! | 162    | 32   | `pool_mint` |
//! | 194    | 32   | `manager_fee_account` |
//! | 226    | 32   | `token_program_id` |
//! | 258    | 8    | `total_lamports` (u64 LE) — total SOL backing the pool |
//! | 266    | 8    | `pool_token_supply` (u64 LE) — total LST tokens minted |
//! | 274    | 8    | `last_update_epoch` (u64 LE) — epoch the balance was last cranked |
//! | 282    | 48   | `lockup` (`unix_timestamp` i64 @282, `epoch` u64 @290, `custodian` @298) |
//! | 330    | 16   | `epoch_fee` (`Fee { denominator u64 @330, numerator u64 @338 }`) |
//!
//! **HARD STOP at offset 346.** The next field, `next_epoch_fee`, is a `FutureEpoch<Fee>` — a
//! borsh **enum whose width depends on its tag byte**, so every offset past 346 is
//! variable and fixed-offset reads there are invalid. [`parse`] never reads at or beyond
//! [`STAKE_POOL_FIXED_LEN`]; a property test pins that (parsing is unchanged by arbitrary tail
//! bytes).
//!
//! The runtime **owner** check (`account.owner == SPL_STAKE_POOL_PROGRAM_ID`) is the caller's
//! job; everything here operates on raw bytes and is fully host-testable. Unlike fusd-core's
//! oracle-leg parser this view does NOT reject a zero-balance pool — it exposes the raw fields
//! and leaves rate/degeneracy policy to the eligibility layer.

/// `AccountType::StakePool` discriminant (borsh enum variant index at byte 0).
pub const ACCOUNT_TYPE_STAKE_POOL: u8 = 1;

const MANAGER_OFFSET: usize = 1;
const STAKER_OFFSET: usize = 33;
const STAKE_DEPOSIT_AUTHORITY_OFFSET: usize = 65;
const STAKE_WITHDRAW_BUMP_OFFSET: usize = 97;
const VALIDATOR_LIST_OFFSET: usize = 98;
const RESERVE_STAKE_OFFSET: usize = 130;
const POOL_MINT_OFFSET: usize = 162;
const MANAGER_FEE_ACCOUNT_OFFSET: usize = 194;
const TOKEN_PROGRAM_OFFSET: usize = 226;
const TOTAL_LAMPORTS_OFFSET: usize = 258;
const POOL_TOKEN_SUPPLY_OFFSET: usize = 266;
const LAST_UPDATE_EPOCH_OFFSET: usize = 274;
const LOCKUP_UNIX_TIMESTAMP_OFFSET: usize = 282;
const LOCKUP_EPOCH_OFFSET: usize = 290;
const LOCKUP_CUSTODIAN_OFFSET: usize = 298;
const EPOCH_FEE_DENOMINATOR_OFFSET: usize = 330;
const EPOCH_FEE_NUMERATOR_OFFSET: usize = 338;

/// End of the fixed-offset region (== minimum account length we accept). The field after this,
/// `next_epoch_fee: FutureEpoch<Fee>`, is variable-width — NEVER read at or past this offset.
pub const STAKE_POOL_FIXED_LEN: usize = 346;

/// Parse failure. The Allocation Controller treats every variant as "pool unreadable" (the
/// affected eligibility/sync step degrades), never a hard revert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StakePoolError {
    /// Data shorter than the fixed-offset region (uninitialized / wrong account).
    TooShort,
    /// Byte 0 is not `AccountType::StakePool` (wrong account type).
    NotStakePool,
}

/// `spl_stake_pool::state::Lockup` (fixed 48 bytes at offset 282).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lockup {
    pub unix_timestamp: i64,
    pub epoch: u64,
    /// Raw key bytes (this crate is Pubkey-type-free); compare with `Pubkey::to_bytes()`.
    pub custodian: [u8; 32],
}

/// `spl_stake_pool::state::Fee` — a numerator/denominator pair (fee = num/den).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fee {
    pub denominator: u64,
    pub numerator: u64,
}

/// Every field of the `StakePool` fixed-offset region. All keys are raw 32-byte arrays so this
/// module stays dependency-free; the caller compares them against `Pubkey::to_bytes()` when
/// binding accounts (e.g. the passed validator-list account key == `validator_list`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StakePoolView {
    pub manager: [u8; 32],
    pub staker: [u8; 32],
    pub stake_deposit_authority: [u8; 32],
    pub stake_withdraw_bump_seed: u8,
    /// The `ValidatorList` account this pool owns — bind it before trusting a validator list.
    pub validator_list: [u8; 32],
    pub reserve_stake: [u8; 32],
    /// The LST mint this pool issues — bind it to the market's collateral mint.
    pub pool_mint: [u8; 32],
    pub manager_fee_account: [u8; 32],
    pub token_program: [u8; 32],
    /// Total SOL (lamports) backing the pool.
    pub total_lamports: u64,
    /// Total LST tokens minted (smallest unit).
    pub pool_token_supply: u64,
    /// Epoch the pool balance was last cranked (`UpdateStakePoolBalance`); compare with the
    /// current epoch to reject a stale rate.
    pub last_update_epoch: u64,
    pub lockup: Lockup,
    pub epoch_fee: Fee,
}

/// Parse a `StakePool` account's raw data. Guards (in order): minimum length, then
/// `account_type`. Reads only fixed offsets strictly below [`STAKE_POOL_FIXED_LEN`] — no
/// allocation, no zero-copy cast, alignment-independent. The caller must already have verified
/// the account's runtime owner == the SPL stake-pool program.
pub fn parse(data: &[u8]) -> Result<StakePoolView, StakePoolError> {
    if data.len() < STAKE_POOL_FIXED_LEN {
        return Err(StakePoolError::TooShort);
    }
    if data[0] != ACCOUNT_TYPE_STAKE_POOL {
        return Err(StakePoolError::NotStakePool);
    }
    // Every offset below is < STAKE_POOL_FIXED_LEN ≤ data.len(), so the bounds-checked readers
    // cannot fail; `TooShort` is kept as the (unreachable) fallback rather than any panic path.
    let read32 = |off| crate::bytes::array32(data, off).ok_or(StakePoolError::TooShort);
    let read_u64 = |off| crate::bytes::u64_le(data, off).ok_or(StakePoolError::TooShort);
    Ok(StakePoolView {
        manager: read32(MANAGER_OFFSET)?,
        staker: read32(STAKER_OFFSET)?,
        stake_deposit_authority: read32(STAKE_DEPOSIT_AUTHORITY_OFFSET)?,
        stake_withdraw_bump_seed: crate::bytes::u8_at(data, STAKE_WITHDRAW_BUMP_OFFSET)
            .ok_or(StakePoolError::TooShort)?,
        validator_list: read32(VALIDATOR_LIST_OFFSET)?,
        reserve_stake: read32(RESERVE_STAKE_OFFSET)?,
        pool_mint: read32(POOL_MINT_OFFSET)?,
        manager_fee_account: read32(MANAGER_FEE_ACCOUNT_OFFSET)?,
        token_program: read32(TOKEN_PROGRAM_OFFSET)?,
        total_lamports: read_u64(TOTAL_LAMPORTS_OFFSET)?,
        pool_token_supply: read_u64(POOL_TOKEN_SUPPLY_OFFSET)?,
        last_update_epoch: read_u64(LAST_UPDATE_EPOCH_OFFSET)?,
        lockup: Lockup {
            unix_timestamp: crate::bytes::i64_le(data, LOCKUP_UNIX_TIMESTAMP_OFFSET)
                .ok_or(StakePoolError::TooShort)?,
            epoch: read_u64(LOCKUP_EPOCH_OFFSET)?,
            custodian: read32(LOCKUP_CUSTODIAN_OFFSET)?,
        },
        epoch_fee: Fee {
            denominator: read_u64(EPOCH_FEE_DENOMINATOR_OFFSET)?,
            numerator: read_u64(EPOCH_FEE_NUMERATOR_OFFSET)?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn put_u64(buf: &mut [u8], off: usize, v: u64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// A StakePool buffer with every fixed-region field set to a distinct recognizable value,
    /// plus `tail` bytes past offset 346 (the variable-width `next_epoch_fee`+ region).
    fn stake_pool_buf(tail: usize) -> Vec<u8> {
        let mut buf = vec![0u8; STAKE_POOL_FIXED_LEN + tail];
        buf[0] = ACCOUNT_TYPE_STAKE_POOL;
        for (off, fill) in [(1usize, 1u8), (33, 2), (65, 3), (98, 4), (130, 5), (162, 6), (194, 7), (226, 8)]
        {
            buf[off..off + 32].copy_from_slice(&[fill; 32]);
        }
        buf[97] = 0xFE; // stake_withdraw_bump_seed
        put_u64(&mut buf, 258, 1_180_000_000_000_000); // total_lamports (rate 1.18)
        put_u64(&mut buf, 266, 1_000_000_000_000_000); // pool_token_supply
        put_u64(&mut buf, 274, 742); // last_update_epoch
        buf[282..290].copy_from_slice(&(-5i64).to_le_bytes()); // lockup.unix_timestamp
        put_u64(&mut buf, 290, 11); // lockup.epoch
        buf[298..330].copy_from_slice(&[9u8; 32]); // lockup.custodian
        put_u64(&mut buf, 330, 100); // epoch_fee.denominator
        put_u64(&mut buf, 338, 3); // epoch_fee.numerator
        buf
    }

    #[test]
    fn parses_every_field() {
        // Tail bytes past 346 set to 0xFF: a variable-width `next_epoch_fee` region full of
        // garbage must not affect (or even be touched by) the fixed-region parse.
        let mut buf = stake_pool_buf(60);
        for b in &mut buf[STAKE_POOL_FIXED_LEN..] {
            *b = 0xFF;
        }
        let v = parse(&buf).unwrap();
        assert_eq!(v.manager, [1u8; 32]);
        assert_eq!(v.staker, [2u8; 32]);
        assert_eq!(v.stake_deposit_authority, [3u8; 32]);
        assert_eq!(v.stake_withdraw_bump_seed, 0xFE);
        assert_eq!(v.validator_list, [4u8; 32]);
        assert_eq!(v.reserve_stake, [5u8; 32]);
        assert_eq!(v.pool_mint, [6u8; 32]);
        assert_eq!(v.manager_fee_account, [7u8; 32]);
        assert_eq!(v.token_program, [8u8; 32]);
        assert_eq!(v.total_lamports, 1_180_000_000_000_000);
        assert_eq!(v.pool_token_supply, 1_000_000_000_000_000);
        assert_eq!(v.last_update_epoch, 742);
        assert_eq!(v.lockup, Lockup { unix_timestamp: -5, epoch: 11, custodian: [9u8; 32] });
        assert_eq!(v.epoch_fee, Fee { denominator: 100, numerator: 3 });
    }

    /// Exactly the fixed region parses; one byte fewer is `TooShort`.
    #[test]
    fn fixed_len_boundary() {
        let buf = stake_pool_buf(0);
        assert_eq!(buf.len(), 346);
        assert!(parse(&buf).is_ok());
        assert_eq!(parse(&buf[..buf.len() - 1]), Err(StakePoolError::TooShort));
        assert_eq!(parse(&[]), Err(StakePoolError::TooShort));
    }

    #[test]
    fn rejects_wrong_account_type() {
        // 0 = Uninitialized, 2 = ValidatorList — both rejected (only a StakePool is valid).
        for t in [0u8, 2, 3, 0xFF] {
            let mut buf = stake_pool_buf(0);
            buf[0] = t;
            assert_eq!(parse(&buf), Err(StakePoolError::NotStakePool));
        }
    }

    /// The offsets shared with fusd-core's oracle-leg parser stay in lockstep: both parsers must
    /// read identical values from the same bytes. Breaks loudly if either side's offsets drift.
    #[test]
    fn agrees_with_fusd_core_parser() {
        let buf = stake_pool_buf(20);
        let ours = parse(&buf).unwrap();
        let house = fusd_core::stake_pool::parse(&buf).unwrap();
        assert_eq!(ours.pool_mint, house.pool_mint);
        assert_eq!(ours.total_lamports, house.total_lamports);
        assert_eq!(ours.pool_token_supply, house.pool_token_supply);
        assert_eq!(ours.last_update_epoch, house.last_update_epoch);
    }

    proptest! {
        /// Parsing NEVER depends on any byte at or past offset 346 (the variable-width region):
        /// the result over the full buffer equals the result over the truncated fixed region.
        #[test]
        fn never_reads_past_fixed_region(head in proptest::collection::vec(any::<u8>(), 346),
                                         tail in proptest::collection::vec(any::<u8>(), 0..256)) {
            let mut full = head.clone();
            full.extend_from_slice(&tail);
            prop_assert_eq!(parse(&full), parse(&head));
        }

        /// Arbitrary bytes never panic — parse degrades to `Err` or a well-formed view.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = parse(&data);
        }
    }
}
