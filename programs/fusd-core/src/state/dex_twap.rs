use anchor_lang::prelude::*;
use fusd_oracle::ObservationRing;

use crate::constants::TWAP_RING_CAPACITY;

/// Per-market self-maintained DEX TWAP observation ring (zero-copy). Appended by the
/// permissionless `sample_twap` crank; read by `update_price` as the manipulation-resistance
/// corridor (NEVER a primary price). PDA `[b"twap", collateral_mint]`. See fusion-docs.md;
/// ring math + tests in `fusd_oracle::twap`.
///
/// Fields MIRROR `ObservationRing<TWAP_RING_CAPACITY>` field-for-field (both `repr(C)`):
/// embedding the const-generic type directly would force an anchor-lang `IdlBuild` dep
/// into the pure fusd-oracle crate, so instead `ring()`/`ring_mut()` reinterpret this
/// account as the ring via a Pod cast — sound iff the layouts are identical, which the
/// `const` size assert plus the behavior round-trip test below enforce. Do not reorder
/// or resize fields without updating `ObservationRing` (and vice versa).
#[account(zero_copy)]
#[repr(C)]
pub struct DexTwap {
    pub prices: [u128; TWAP_RING_CAPACITY],
    pub ts: [i64; TWAP_RING_CAPACITY],
    pub next: u64,
    pub count: u64,
}

// Layout identity: same size (both repr(C), same field order ⇒ same offsets). The ring's
// Pod impl additionally requires even N (padding-free layout) — re-asserted here.
const _: () = assert!(TWAP_RING_CAPACITY % 2 == 0);
const _: () = assert!(
    core::mem::size_of::<DexTwap>()
        == core::mem::size_of::<ObservationRing<TWAP_RING_CAPACITY>>()
);
// %8 alignment pin for uniformity with the other zero-copy accounts.
const _: () = assert!(core::mem::size_of::<DexTwap>() % 8 == 0);
// Field-offset pins (audit #18): a size-neutral swap of two equal-width fields (e.g. next<->count,
// both u64) would pass the size asserts + the mirror test yet silently remap bytes; pin the offsets
// so any reorder is a compile error. (The mirror round-trip test already pins these behaviorally;
// this makes the guarantee static + uniform across the zero-copy accounts.)
const _: () = assert!(core::mem::offset_of!(DexTwap, prices) == 0);
const _: () = assert!(core::mem::offset_of!(DexTwap, ts) == TWAP_RING_CAPACITY * 16);
const _: () =
    assert!(core::mem::offset_of!(DexTwap, next) == TWAP_RING_CAPACITY * 16 + TWAP_RING_CAPACITY * 8);
const _: () = assert!(
    core::mem::offset_of!(DexTwap, count) == TWAP_RING_CAPACITY * 16 + TWAP_RING_CAPACITY * 8 + 8
);

impl DexTwap {
    pub const SPACE: usize = 8 + core::mem::size_of::<DexTwap>(); // 8 + 1552 = 1560

    /// View this account as the tested ring type (zero-copy reinterpret; see layout note).
    pub fn ring(&self) -> &ObservationRing<TWAP_RING_CAPACITY> {
        bytemuck::cast_ref(self)
    }

    pub fn ring_mut(&mut self) -> &mut ObservationRing<TWAP_RING_CAPACITY> {
        bytemuck::cast_mut(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fusd_oracle::TwapConfig;

    /// The cast is layout-sound: pushes through `ring_mut()` land in the mirror fields
    /// exactly where the raw struct expects them, and the TWAP math reads them back.
    #[test]
    fn mirror_layout_round_trips() {
        let mut dt = DexTwap {
            prices: [0; TWAP_RING_CAPACITY],
            ts: [0; TWAP_RING_CAPACITY],
            next: 0,
            count: 0,
        };
        dt.ring_mut().push(100, 10).unwrap();
        dt.ring_mut().push(200, 20).unwrap();
        // Raw mirror fields observe the ring's writes at the same offsets.
        assert_eq!(dt.prices[0], 100);
        assert_eq!(dt.prices[1], 200);
        assert_eq!(dt.ts[0], 10);
        assert_eq!(dt.ts[1], 20);
        assert_eq!(dt.next, 2);
        assert_eq!(dt.count, 2);
        // And the ring view computes over them.
        let cfg = TwapConfig { min_samples: 1, max_staleness: i64::MAX };
        assert_eq!(dt.ring().twap(30, 20, &cfg), Some(150));
        // Non-monotonic still rejected through the cast.
        assert!(dt.ring_mut().push(300, 20).is_err());
    }
}
