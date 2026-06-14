use anchor_lang::prelude::*;

use crate::constants::{BITMAP_WORDS, NUM_RATE_BUCKETS};

/// Per-market redemption-targeting bitmap (zero-copy). `words` marks non-empty rate buckets — bit
/// `k` is set iff bucket `k` holds ≥1 position with debt — and `redeem` finds the lowest non-empty
/// bucket over it via find-first-set. `counts[k]` is bucket `k`'s member count; the bit flips only
/// on a bucket's empty↔non-empty transition. PDA `[b"redeem_bitmap", collateral_mint]`.
/// fusion-docs.md; math in `fusd_math::rate_bucket`.
#[account(zero_copy)]
#[repr(C)]
pub struct RedemptionBitmap {
    pub words: [u64; BITMAP_WORDS],
    pub counts: [u32; NUM_RATE_BUCKETS],
    /// Member count of the redemption **zombie pen**: positions that carry debt but are no
    /// longer normal redemption targets — collateral-exhausted (`ink == 0`, unredeemable) or sub-
    /// `min_debt` dust. Parked OUTSIDE `words`/`counts` so they can never wedge or clog the normal
    /// find-first-set ordering; a pen member carries `Position.bucket = ZOMBIE_BUCKET` and rejoins a
    /// real bucket when a touch restores its health. `u64` (not `u32`) keeps the zero-copy struct
    /// free of tail padding (`Pod`-clean: words 32 + counts 1024 + 8 stays 8-aligned).
    pub zombie_count: u64,
}

impl RedemptionBitmap {
    pub const SPACE: usize = 8 + BITMAP_WORDS * 8 + NUM_RATE_BUCKETS * 4 + 8; // 8 + 32 + 1024 + 8 = 1072
}

// Layout pins: any drift between the hand-summed SPACE and the actual
// zero-copy layout — a field-type change, reordering, or accidental padding — becomes a COMPILE
// error instead of a first-load runtime corruption. The %8 pin keeps the struct Pod-clean
// (no tail padding) for bytemuck.
const _: () = assert!(RedemptionBitmap::SPACE == 8 + core::mem::size_of::<RedemptionBitmap>());
const _: () = assert!(core::mem::size_of::<RedemptionBitmap>() % 8 == 0);
