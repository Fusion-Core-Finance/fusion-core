//! Pure byte-parsing of Orca Whirlpool / Raydium CLMM pool accounts for `sample_twap`.
//!
//! Solana CLMMs expose no on-chain TWAP accumulator, so fUSD samples their spot `sqrt_price` into
//! its own observation ring. We read the few fields we need at **verified byte offsets** rather
//! than pulling the orca/raydium crates (no extra deps, no version-lock churn, and no zero-copy
//! alignment surprises). Layouts + the mandatory guard set are documented in
//! [`docs/clmm-pool-layouts.md`] — source-walked AND verified against live mainnet
//! accounts. Re-verified 2026-06-09 (surfpool fork PoC): these exact offsets decode the real Orca
//! Whirlpool `Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE` and Raydium CLMM
//! `3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv` SOL/USDC pools — owner / discriminator / mint-pair /
//! (Raydium) decimals all correct, and the two venues' decoded SOL/USDC prices agreed to <0.01%.
//!
//! The runtime **owner** check (`account.owner == <venue program id>`) is the caller's job
//! (`sample_twap`); everything here operates on the raw account bytes and is fully host-testable.

use anchor_lang::prelude::Pubkey;

/// The two CLMM venues fUSD samples. Selected by which configured pool the passed account matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Venue {
    Orca,
    Raydium,
}

/// Whirlpool global `sqrt_price` bounds (Q64.64); the same tick domain bounds Raydium. Checked
/// BEFORE squaring (guard 4) so a corrupt / zero / uninitialized pool can never yield a price.
pub const SQRT_PRICE_MIN: u128 = 4_295_048_016;
pub const SQRT_PRICE_MAX: u128 = 79_226_673_515_401_279_992_447_579_055;

// --- Orca Whirlpool: borsh `#[account]`, sha256("account:Whirlpool")[..8] ---
const WHIRLPOOL_DISCRIMINATOR: [u8; 8] = [63, 149, 209, 12, 225, 128, 99, 9];
const WHIRLPOOL_SQRT_PRICE_OFFSET: usize = 65; // u128
const WHIRLPOOL_MINT_A_OFFSET: usize = 101; // Pubkey
const WHIRLPOOL_MINT_B_OFFSET: usize = 181; // Pubkey
const WHIRLPOOL_MIN_LEN: usize = WHIRLPOOL_MINT_B_OFFSET + 32; // 213

// --- Raydium CLMM: `#[repr(C, packed)]` zero-copy, sha256("account:PoolState")[..8] ---
const RAYDIUM_DISCRIMINATOR: [u8; 8] = [247, 237, 227, 245, 215, 195, 222, 70];
const RAYDIUM_MINT_0_OFFSET: usize = 73; // Pubkey
const RAYDIUM_MINT_1_OFFSET: usize = 105; // Pubkey
const RAYDIUM_DECIMALS_0_OFFSET: usize = 233; // u8
const RAYDIUM_DECIMALS_1_OFFSET: usize = 234; // u8
const RAYDIUM_SQRT_PRICE_OFFSET: usize = 253; // u128
const RAYDIUM_MIN_LEN: usize = RAYDIUM_SQRT_PRICE_OFFSET + 16; // 269

/// Parse failure. The `sample_twap` handler maps every variant to `FusdError::InvalidClmmPool`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClmmError {
    /// First 8 bytes are not the venue's account discriminator (wrong account type).
    BadDiscriminator,
    /// Data shorter than the highest field offset we read (never assert exact length — Raydium's
    /// tail has grown across versions; we only require a lower bound).
    TooShort,
    /// `sqrt_price` outside the global tick bounds (or zero) — refused before squaring.
    SqrtPriceOutOfBounds,
}

/// The fields `sample_twap` needs out of a pool account.
///
/// `mint_a`/`mint_b` are the pool's token pair in the venue's canonical order (Orca a/b; Raydium
/// 0/1, which Raydium enforces as `mint_0 < mint_1`). `sqrt_price` is Q64.64 of
/// `sqrt(mint_b-native / mint_a-native)`. `dec_a`/`dec_b` are present for Raydium (decimals live
/// in the account) and `None` for Whirlpool (which stores none — decimals come from `MarketOracle`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolSample {
    pub sqrt_price: u128,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub dec_a: Option<u8>,
    pub dec_b: Option<u8>,
}

fn read_u128_le(data: &[u8], off: usize) -> u128 {
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&data[off..off + 16]);
    u128::from_le_bytes(buf)
}

fn read_pubkey(data: &[u8], off: usize) -> Pubkey {
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&data[off..off + 32]);
    Pubkey::new_from_array(buf)
}

/// Parse a pool account's raw data for the chosen venue. Guards (in order): minimum length,
/// discriminator, then `sqrt_price` bounds. Reads only fixed offsets — no allocation, no zero-copy
/// cast, alignment-independent. The caller must already have verified the account's runtime owner.
pub fn parse(venue: Venue, data: &[u8]) -> Result<PoolSample, ClmmError> {
    let (disc, min_len) = match venue {
        Venue::Orca => (WHIRLPOOL_DISCRIMINATOR, WHIRLPOOL_MIN_LEN),
        Venue::Raydium => (RAYDIUM_DISCRIMINATOR, RAYDIUM_MIN_LEN),
    };
    if data.len() < min_len {
        return Err(ClmmError::TooShort);
    }
    if data[..8] != disc {
        return Err(ClmmError::BadDiscriminator);
    }

    let sample = match venue {
        Venue::Orca => PoolSample {
            sqrt_price: read_u128_le(data, WHIRLPOOL_SQRT_PRICE_OFFSET),
            mint_a: read_pubkey(data, WHIRLPOOL_MINT_A_OFFSET),
            mint_b: read_pubkey(data, WHIRLPOOL_MINT_B_OFFSET),
            dec_a: None,
            dec_b: None,
        },
        Venue::Raydium => PoolSample {
            sqrt_price: read_u128_le(data, RAYDIUM_SQRT_PRICE_OFFSET),
            mint_a: read_pubkey(data, RAYDIUM_MINT_0_OFFSET),
            mint_b: read_pubkey(data, RAYDIUM_MINT_1_OFFSET),
            dec_a: Some(data[RAYDIUM_DECIMALS_0_OFFSET]),
            dec_b: Some(data[RAYDIUM_DECIMALS_1_OFFSET]),
        },
    };

    if sample.sqrt_price < SQRT_PRICE_MIN || sample.sqrt_price > SQRT_PRICE_MAX {
        return Err(ClmmError::SqrtPriceOutOfBounds);
    }
    Ok(sample)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A valid WSOL/USDC-shaped sqrt_price (the verified decode sample from clmm-pool-layouts.md).
    const SQRT_PRICE: u128 = 4_857_170_867_873_581_308;

    fn put_u128(buf: &mut [u8], off: usize, v: u128) {
        buf[off..off + 16].copy_from_slice(&v.to_le_bytes());
    }
    fn put_key(buf: &mut [u8], off: usize, k: &Pubkey) {
        buf[off..off + 32].copy_from_slice(k.as_ref());
    }

    fn whirlpool_buf(sqrt_price: u128, a: &Pubkey, b: &Pubkey) -> Vec<u8> {
        // Sized past BOTH venues' min_len so a cross-venue parse reaches the discriminator check
        // (the length guard fires first) — see `rejects_wrong_discriminator`.
        let mut buf = vec![0u8; 320];
        buf[..8].copy_from_slice(&WHIRLPOOL_DISCRIMINATOR);
        put_u128(&mut buf, WHIRLPOOL_SQRT_PRICE_OFFSET, sqrt_price);
        put_key(&mut buf, WHIRLPOOL_MINT_A_OFFSET, a);
        put_key(&mut buf, WHIRLPOOL_MINT_B_OFFSET, b);
        buf
    }

    fn raydium_buf(sqrt_price: u128, m0: &Pubkey, m1: &Pubkey, d0: u8, d1: u8) -> Vec<u8> {
        let mut buf = vec![0u8; 320]; // a bit past min_len, mimicking the grown tail
        buf[..8].copy_from_slice(&RAYDIUM_DISCRIMINATOR);
        put_key(&mut buf, RAYDIUM_MINT_0_OFFSET, m0);
        put_key(&mut buf, RAYDIUM_MINT_1_OFFSET, m1);
        buf[RAYDIUM_DECIMALS_0_OFFSET] = d0;
        buf[RAYDIUM_DECIMALS_1_OFFSET] = d1;
        put_u128(&mut buf, RAYDIUM_SQRT_PRICE_OFFSET, sqrt_price);
        buf
    }

    #[test]
    fn parses_whirlpool() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        let s = parse(Venue::Orca, &whirlpool_buf(SQRT_PRICE, &a, &b)).unwrap();
        assert_eq!(s.sqrt_price, SQRT_PRICE);
        assert_eq!(s.mint_a, a);
        assert_eq!(s.mint_b, b);
        assert_eq!(s.dec_a, None);
        assert_eq!(s.dec_b, None);
    }

    #[test]
    fn parses_raydium_with_decimals() {
        let m0 = Pubkey::new_unique();
        let m1 = Pubkey::new_unique();
        let s = parse(Venue::Raydium, &raydium_buf(SQRT_PRICE, &m0, &m1, 9, 6)).unwrap();
        assert_eq!(s.sqrt_price, SQRT_PRICE);
        assert_eq!(s.mint_a, m0);
        assert_eq!(s.mint_b, m1);
        assert_eq!(s.dec_a, Some(9));
        assert_eq!(s.dec_b, Some(6));
    }

    #[test]
    fn rejects_wrong_discriminator() {
        // A Raydium buffer fed to the Orca parser (and vice versa) is rejected on discriminator.
        let m0 = Pubkey::new_unique();
        let m1 = Pubkey::new_unique();
        assert_eq!(
            parse(Venue::Orca, &raydium_buf(SQRT_PRICE, &m0, &m1, 9, 6)),
            Err(ClmmError::BadDiscriminator)
        );
        assert_eq!(
            parse(Venue::Raydium, &whirlpool_buf(SQRT_PRICE, &m0, &m1)),
            Err(ClmmError::BadDiscriminator)
        );
        // Garbage discriminator.
        let mut buf = whirlpool_buf(SQRT_PRICE, &m0, &m1);
        buf[0] ^= 0xFF;
        assert_eq!(parse(Venue::Orca, &buf), Err(ClmmError::BadDiscriminator));
    }

    #[test]
    fn rejects_too_short() {
        let mut buf = whirlpool_buf(SQRT_PRICE, &Pubkey::new_unique(), &Pubkey::new_unique());
        buf.truncate(WHIRLPOOL_MIN_LEN - 1);
        assert_eq!(parse(Venue::Orca, &buf), Err(ClmmError::TooShort));

        let mut rbuf =
            raydium_buf(SQRT_PRICE, &Pubkey::new_unique(), &Pubkey::new_unique(), 9, 6);
        rbuf.truncate(RAYDIUM_MIN_LEN - 1);
        assert_eq!(parse(Venue::Raydium, &rbuf), Err(ClmmError::TooShort));
    }

    #[test]
    fn rejects_sqrt_price_out_of_bounds() {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        // Zero is the uninitialized-account sentinel.
        assert_eq!(
            parse(Venue::Orca, &whirlpool_buf(0, &a, &b)),
            Err(ClmmError::SqrtPriceOutOfBounds)
        );
        // Below the global minimum.
        assert_eq!(
            parse(Venue::Orca, &whirlpool_buf(SQRT_PRICE_MIN - 1, &a, &b)),
            Err(ClmmError::SqrtPriceOutOfBounds)
        );
        // Above the global maximum.
        assert_eq!(
            parse(Venue::Orca, &whirlpool_buf(SQRT_PRICE_MAX + 1, &a, &b)),
            Err(ClmmError::SqrtPriceOutOfBounds)
        );
        // Exactly the bounds are accepted.
        assert!(parse(Venue::Orca, &whirlpool_buf(SQRT_PRICE_MIN, &a, &b)).is_ok());
        assert!(parse(Venue::Orca, &whirlpool_buf(SQRT_PRICE_MAX, &a, &b)).is_ok());
    }
}
