//! Pure byte-parsing of an SPL Stake Pool `StakePool` account for the C1 LST canonical-rate leg.
//!
//! For LST collateral, `update_price` reads the trustless on-chain stake-pool exchange rate
//! (`total_lamports / pool_token_supply` = SOL per pool token) and serves the collateral price at
//! `MIN(market, sol_usd · rate)`, so an upward-manipulated market feed can't inflate borrowing
//! power past the stake-pool reality (the BOLD-08 over-mint→depeg defense). We read the few fields
//! we need at **verified byte offsets** rather than pulling the `spl-stake-pool` crate (no extra
//! deps, no version-lock churn, no zero-copy alignment surprises) — exactly as `clmm.rs` does.
//!
//! Layout: `spl_stake_pool::state::StakePool` is borsh-serialized by a NATIVE program, so there is
//! **no 8-byte Anchor discriminator** — byte 0 is the `AccountType` enum (`1 = StakePool`), and the
//! rest is a plain running sum (borsh emits no padding). The fields up to the two balances:
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
//!
//! The canonical rate is `total_lamports / pool_token_supply` (SOL-lamports per LST-smallest-unit;
//! SPL pool mints are 9-decimal like SOL, so this is the whole-token SOL/LST rate directly). The
//! runtime **owner** check (`account.owner == SPL_STAKE_POOL_PROGRAM_ID`) is the caller's job
//! (`update_price`); everything here operates on raw bytes and is fully host-testable.
//!
//! NB: like the CLMM offsets, these should be cross-checked against a live mainnet stake pool via
//! the surfpool harness before launch (`StakePool` is a stable, long-frozen layout, but verify).

/// `AccountType::StakePool` discriminant (borsh enum variant index at byte 0).
const ACCOUNT_TYPE_STAKE_POOL: u8 = 1;

const POOL_MINT_OFFSET: usize = 162; // 32 bytes — the LST mint this pool issues
const TOTAL_LAMPORTS_OFFSET: usize = 258; // u64 LE
const POOL_TOKEN_SUPPLY_OFFSET: usize = 266; // u64 LE
const LAST_UPDATE_EPOCH_OFFSET: usize = 274; // u64 LE
const STAKE_POOL_MIN_LEN: usize = LAST_UPDATE_EPOCH_OFFSET + 8; // 282

/// Parse failure. The `update_price` handler treats every variant as "canonical leg unavailable"
/// (degrade to `None` → freeze mints on an LST market), never a hard revert — a momentarily
/// unreadable stake pool must not brick the permissionless crank.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StakePoolError {
    /// Data shorter than the highest field offset we read (uninitialized / wrong account).
    TooShort,
    /// Byte 0 is not `AccountType::StakePool` (wrong account type).
    NotStakePool,
    /// `pool_token_supply == 0` (uninitialized / would divide by zero) or `total_lamports == 0`.
    DegenerateRate,
}

/// The fields the canonical leg needs out of a `StakePool` account.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StakePoolSample {
    /// The LST mint this pool issues. The caller binds it to the market's `collateral_mint` so a
    /// gov-misconfigured pool (correct key, wrong underlying asset) can't price the wrong LST — the
    /// stake-pool analog of the CLMM leg's per-crank mint-pair check (audit #21). Raw bytes to keep
    /// this module dependency-free / host-testable; compare against `collateral_mint.to_bytes()`.
    pub pool_mint: [u8; 32],
    /// Total SOL (lamports) backing the pool.
    pub total_lamports: u64,
    /// Total LST tokens minted (smallest unit).
    pub pool_token_supply: u64,
    /// Epoch the pool balance was last cranked (`UpdateStakePoolBalance`). The caller compares it to
    /// the current epoch to reject a stale (un-updated-this-epoch) rate.
    pub last_update_epoch: u64,
}

fn read_u64_le(data: &[u8], off: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data[off..off + 8]);
    u64::from_le_bytes(buf)
}

/// Parse a `StakePool` account's raw data. Guards (in order): minimum length, `account_type`, then
/// a non-degenerate rate (`pool_token_supply > 0 && total_lamports > 0`). Reads only fixed offsets
/// — no allocation, no zero-copy cast, alignment-independent. The caller must already have verified
/// the account's runtime owner == `SPL_STAKE_POOL_PROGRAM_ID`.
pub fn parse(data: &[u8]) -> Result<StakePoolSample, StakePoolError> {
    if data.len() < STAKE_POOL_MIN_LEN {
        return Err(StakePoolError::TooShort);
    }
    if data[0] != ACCOUNT_TYPE_STAKE_POOL {
        return Err(StakePoolError::NotStakePool);
    }
    let mut pool_mint = [0u8; 32];
    pool_mint.copy_from_slice(&data[POOL_MINT_OFFSET..POOL_MINT_OFFSET + 32]);
    let total_lamports = read_u64_le(data, TOTAL_LAMPORTS_OFFSET);
    let pool_token_supply = read_u64_le(data, POOL_TOKEN_SUPPLY_OFFSET);
    let last_update_epoch = read_u64_le(data, LAST_UPDATE_EPOCH_OFFSET);
    if pool_token_supply == 0 || total_lamports == 0 {
        return Err(StakePoolError::DegenerateRate);
    }
    Ok(StakePoolSample { pool_mint, total_lamports, pool_token_supply, last_update_epoch })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_u64(buf: &mut [u8], off: usize, v: u64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// A StakePool buffer with a realistic ~1.18 SOL/jitoSOL rate (total/supply) and a recognizable
    /// `pool_mint`.
    fn stake_pool_buf(total_lamports: u64, pool_token_supply: u64, epoch: u64) -> Vec<u8> {
        let mut buf = vec![0u8; 320]; // past min_len, mimicking the real account's larger tail
        buf[0] = ACCOUNT_TYPE_STAKE_POOL;
        buf[POOL_MINT_OFFSET..POOL_MINT_OFFSET + 32].copy_from_slice(&[9u8; 32]);
        put_u64(&mut buf, TOTAL_LAMPORTS_OFFSET, total_lamports);
        put_u64(&mut buf, POOL_TOKEN_SUPPLY_OFFSET, pool_token_supply);
        put_u64(&mut buf, LAST_UPDATE_EPOCH_OFFSET, epoch);
        buf
    }

    #[test]
    fn parses_balances_and_epoch() {
        // 1_180_000 SOL backing 1_000_000 jitoSOL ⇒ rate 1.18.
        let s = parse(&stake_pool_buf(1_180_000_000_000_000, 1_000_000_000_000_000, 742)).unwrap();
        assert_eq!(s.pool_mint, [9u8; 32]);
        assert_eq!(s.total_lamports, 1_180_000_000_000_000);
        assert_eq!(s.pool_token_supply, 1_000_000_000_000_000);
        assert_eq!(s.last_update_epoch, 742);
    }

    #[test]
    fn rejects_too_short() {
        let mut buf = stake_pool_buf(1_000, 1_000, 1);
        buf.truncate(STAKE_POOL_MIN_LEN - 1);
        assert_eq!(parse(&buf), Err(StakePoolError::TooShort));
    }

    #[test]
    fn rejects_wrong_account_type() {
        // 0 = Uninitialized, 2 = ValidatorList — both rejected (only a StakePool account is valid).
        for t in [0u8, 2, 3, 0xFF] {
            let mut buf = stake_pool_buf(1_000, 1_000, 1);
            buf[0] = t;
            assert_eq!(parse(&buf), Err(StakePoolError::NotStakePool));
        }
    }

    #[test]
    fn rejects_degenerate_rate() {
        // Zero supply (would divide by zero) and zero backing (uninitialized) are both refused.
        assert_eq!(parse(&stake_pool_buf(1_000, 0, 1)), Err(StakePoolError::DegenerateRate));
        assert_eq!(parse(&stake_pool_buf(0, 1_000, 1)), Err(StakePoolError::DegenerateRate));
    }
}
