//! Self-maintained DEX TWAP observation ring (fusion-docs.md).
//!
//! Solana CLMMs (Orca Whirlpool, Raydium CLMM) expose no Uniswap-style on-chain
//! cumulative-price accumulator, so fUSD maintains its own ring of `{price, ts}`
//! observations, appended by a permissionless sampler crank (`sample_twap`). The TWAP is
//! **never a primary price** — only a divergence corridor that a few-block price pump
//! (the Mango attack) cannot move, because every price is weighted by the time it was
//! in effect and the average must span a full window.
//!
//! Pure host-testable logic: plain integers, fixed-capacity array, `repr(C)`, no heap —
//! drops into a zero-copy Solana account (`DexTwap`) later.
//!
//! Rounding: the TWAP floors (rounds down). It feeds a *symmetric* divergence check
//! (both above and below the corridor freeze mints), so neither direction favors the
//! protocol; floor is chosen for determinism and documented here.

/// Unix-seconds for now; may switch to slots later — keep all call sites on the typedef.
pub type Timestamp = i64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TwapError {
    /// `push` requires strictly increasing timestamps.
    NonMonotonicTs,
}

/// One sampled observation: the pool price at time `ts`. A VIEW composed on read — the
/// ring stores parallel `prices`/`ts` arrays, not `[Observation; N]`, because
/// `{u128, i64}` carries 8 bytes of tail padding wherever `u128` is 16-aligned (host
/// x86-64/aarch64) and padding is incompatible with `bytemuck::Pod`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Observation {
    pub price: u128,
    pub ts: Timestamp,
}

/// Guards on `twap()`; lives in per-collateral params (futarchy-tunable within clamps),
/// not in the ring account itself.
#[derive(Clone, Copy, Debug)]
pub struct TwapConfig {
    /// Minimum number of retained observations required to compute a TWAP.
    pub min_samples: u32,
    /// Maximum allowed `now - newest.ts`; a ring the crank stopped feeding is no bound.
    pub max_staleness: i64,
}

/// Fixed-capacity ring of observations, oldest overwritten first.
///
/// Layout (`repr(C)`, parallel arrays): `prices` (16·N) + `ts` (8·N) + `next`/`count`
/// (8+8) — padding-free on every target **when `N` is even** (total `24·N + 16` must be
/// a multiple of the 16-byte `u128` alignment). The `pod` feature unsafely implements
/// `bytemuck::Pod/Zeroable` on that basis so the ring can sit inside an Anchor
/// `zero_copy` account; instantiate with even `N` only (compile-time asserted in
/// `new()`, and worth re-asserting with a `size_of` const check at the embedding site).
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ObservationRing<const N: usize> {
    prices: [u128; N],
    ts: [Timestamp; N],
    /// Index of the next write slot.
    next: u64,
    /// Number of valid observations; saturates at `N`.
    count: u64,
}

// SAFETY: `repr(C)`; every field is a primitive-integer array or integer (each Pod);
// all-zeroes is the valid empty ring; no padding for even `N` (see layout doc above).
// The even-`N` requirement is enforced at compile time via the assert in `new()`
// (monomorphized per `N`).
#[cfg(feature = "pod")]
unsafe impl<const N: usize> bytemuck::Zeroable for ObservationRing<N> {}
#[cfg(feature = "pod")]
unsafe impl<const N: usize> bytemuck::Pod for ObservationRing<N> {}

impl<const N: usize> Default for ObservationRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> ObservationRing<N> {
    pub fn new() -> Self {
        assert!(N > 0, "ObservationRing capacity must be non-zero");
        // NOT `N.is_multiple_of(2)` (clippy's suggestion): that stabilized in Rust 1.87
        // and this crate must build under the SBF toolchain's cargo 1.84.
        #[allow(clippy::manual_is_multiple_of)]
        {
            assert!(N % 2 == 0, "ObservationRing capacity must be even (Pod layout, see above)");
        }
        Self { prices: [0; N], ts: [0; N], next: 0, count: 0 }
    }

    pub fn len(&self) -> usize {
        self.count as usize
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Newest observation, if any.
    pub fn last(&self) -> Option<Observation> {
        if self.count == 0 {
            return None;
        }
        let idx = (self.next as usize + N - 1) % N;
        Some(Observation { price: self.prices[idx], ts: self.ts[idx] })
    }

    /// `k`-th retained observation in chronological order (0 = oldest).
    fn nth(&self, k: usize) -> Observation {
        debug_assert!(k < self.count as usize);
        let oldest = (self.next as usize + N - self.count as usize) % N;
        let idx = (oldest + k) % N;
        Observation { price: self.prices[idx], ts: self.ts[idx] }
    }

    /// Append an observation, overwriting the oldest. Timestamps must be strictly
    /// increasing — equal or older `ts` is rejected (a crank replaying or racing
    /// itself must not be able to re-weight history).
    pub fn push(&mut self, price: u128, ts: Timestamp) -> Result<(), TwapError> {
        if let Some(last) = self.last() {
            if ts <= last.ts {
                return Err(TwapError::NonMonotonicTs);
            }
        }
        let idx = self.next as usize;
        self.prices[idx] = price;
        self.ts[idx] = ts;
        self.next = (self.next + 1) % N as u64;
        if self.count < N as u64 {
            self.count += 1;
        }
        Ok(())
    }

    /// Time-weighted average price over the trailing `window` seconds ending at `now`.
    ///
    /// Step-function weighting: each observation's price holds from its `ts` until the
    /// next observation's `ts`; the newest holds until `now`. Returns `None` — never an
    /// extrapolated or partial average — unless ALL of:
    /// - `window > 0` and `now >= newest.ts` (no future samples),
    /// - at least `cfg.min_samples` observations are retained,
    /// - `now - newest.ts <= cfg.max_staleness`,
    /// - the retained observations span the window (`oldest.ts <= now - window`),
    /// - no arithmetic overflow (conservative: refuse rather than wrap).
    ///
    /// Manipulation resistance: a sample pushed just before `now` carries only
    /// `now - its_ts` seconds of weight, so a single fresh print cannot dominate a
    /// window-spanning average.
    pub fn twap(&self, now: Timestamp, window: i64, cfg: &TwapConfig) -> Option<u128> {
        if window <= 0 || self.count < cfg.min_samples as u64 || self.count == 0 {
            return None;
        }
        let newest = self.last()?;
        if now < newest.ts || now - newest.ts > cfg.max_staleness {
            return None;
        }
        let start = now.checked_sub(window)?;
        // Don't extrapolate: the oldest retained sample must predate the window start.
        if self.nth(0).ts > start {
            return None;
        }

        let mut acc: u128 = 0;
        let n = self.count as usize;
        for k in 0..n {
            let o = self.nth(k);
            let seg_start = o.ts.max(start);
            let seg_end = if k + 1 < n { self.nth(k + 1).ts } else { now };
            let seg_end = seg_end.min(now);
            if seg_end <= seg_start {
                continue; // segment entirely before the window
            }
            let dur = (seg_end - seg_start) as u128;
            acc = acc.checked_add(o.price.checked_mul(dur)?)?;
        }
        // Coverage is exact (oldest.ts <= start), so total weight == window. Floor.
        Some(acc / window as u128)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: TwapConfig = TwapConfig { min_samples: 1, max_staleness: i64::MAX };

    fn ring_from<const N: usize>(obs: &[(u128, Timestamp)]) -> ObservationRing<N> {
        let mut r = ObservationRing::<N>::new();
        for &(price, ts) in obs {
            r.push(price, ts).unwrap();
        }
        r
    }

    #[test]
    fn empty_ring_is_none() {
        let r = ObservationRing::<8>::new();
        assert_eq!(r.twap(100, 50, &CFG), None);
        assert!(r.is_empty());
        assert_eq!(r.last(), None);
    }

    #[test]
    fn partial_window_is_none() {
        // Samples start at ts=60 but the window starts at 100-50=50: no extrapolation.
        let r = ring_from::<8>(&[(100, 60), (100, 80)]);
        assert_eq!(r.twap(100, 50, &CFG), None);
        // Exactly spanning (oldest.ts == window start) is allowed.
        let r = ring_from::<8>(&[(100, 50), (100, 80)]);
        assert_eq!(r.twap(100, 50, &CFG), Some(100));
    }

    #[test]
    fn constant_price_is_exact() {
        let r = ring_from::<8>(&[(42, 0), (42, 10), (42, 25), (42, 90)]);
        assert_eq!(r.twap(100, 100, &CFG), Some(42));
        // Also exact over a sub-window.
        assert_eq!(r.twap(100, 30, &CFG), Some(42));
    }

    #[test]
    fn step_change_weights_by_time_in_regime() {
        // price 100 over [0,50), 200 over [50,100]: twap = 150.
        let r = ring_from::<8>(&[(100, 0), (200, 50)]);
        assert_eq!(r.twap(100, 100, &CFG), Some(150));
        // Asymmetric split: 100 over [0,75), 200 over [75,100] -> 125.
        let r = ring_from::<8>(&[(100, 0), (200, 75)]);
        assert_eq!(r.twap(100, 100, &CFG), Some(125));
        // Sub-window clips the first regime: window [60,100] -> 100*[60,75) + 200*[75,100]
        // = (100*15 + 200*25)/40 = 162 (floor of 162.5).
        assert_eq!(r.twap(100, 40, &CFG), Some(162));
    }

    #[test]
    fn wraparound_stays_correct() {
        // N=4; push 6 samples; retained = ts 20,30,40,50.
        let r = ring_from::<4>(&[
            (100, 0),
            (100, 10),
            (100, 20),
            (100, 30),
            (200, 40),
            (200, 50),
        ]);
        assert_eq!(r.len(), 4);
        assert_eq!(r.last().unwrap().ts, 50);
        // window [20,60]: 100 over [20,40), 200 over [40,60] -> 150.
        assert_eq!(r.twap(60, 40, &CFG), Some(150));
        // Window reaching past the oldest retained sample: refuse (history was overwritten).
        assert_eq!(r.twap(60, 50, &CFG), None);
    }

    #[test]
    fn many_pushes_beyond_capacity() {
        let mut r = ObservationRing::<4>::new();
        for i in 0..100i64 {
            r.push(1_000 + i as u128, i * 10).unwrap();
        }
        assert_eq!(r.len(), 4);
        // Retained: (1096,960),(1097,970),(1098,980),(1099,990).
        // window [960,1000]: 1096*10 + 1097*10 + 1098*10 + 1099*10 over 40 -> 1097 (floor of 1097.5).
        assert_eq!(r.twap(1000, 40, &CFG), Some(1097));
    }

    #[test]
    fn stale_ring_is_none() {
        let cfg = TwapConfig { min_samples: 1, max_staleness: 30 };
        let r = ring_from::<8>(&[(100, 0), (100, 50)]);
        // now - newest = 30: ok.
        assert_eq!(r.twap(80, 60, &cfg), Some(100));
        // now - newest = 31: stale.
        assert_eq!(r.twap(81, 60, &cfg), None);
    }

    #[test]
    fn below_min_samples_is_none() {
        let cfg = TwapConfig { min_samples: 3, max_staleness: i64::MAX };
        let r = ring_from::<8>(&[(100, 0), (100, 50)]);
        assert_eq!(r.twap(100, 100, &cfg), None);
        let r = ring_from::<8>(&[(100, 0), (100, 40), (100, 80)]);
        assert_eq!(r.twap(100, 100, &cfg), Some(100));
    }

    #[test]
    fn non_monotonic_ts_rejected() {
        let mut r = ObservationRing::<8>::new();
        r.push(100, 10).unwrap();
        assert_eq!(r.push(100, 10), Err(TwapError::NonMonotonicTs)); // equal
        assert_eq!(r.push(100, 5), Err(TwapError::NonMonotonicTs)); // older
        assert_eq!(r.len(), 1); // rejected pushes leave the ring untouched
        r.push(100, 11).unwrap();
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn future_newest_sample_is_none() {
        let r = ring_from::<8>(&[(100, 0), (100, 50)]);
        assert_eq!(r.twap(49, 20, &CFG), None); // now precedes the newest sample
    }

    #[test]
    fn zero_or_negative_window_is_none() {
        let r = ring_from::<8>(&[(100, 0), (100, 50)]);
        assert_eq!(r.twap(100, 0, &CFG), None);
        assert_eq!(r.twap(100, -10, &CFG), None);
    }

    #[test]
    fn fresh_spike_cannot_dominate() {
        // The Mango scenario: one hour of honest 1_000 prints, then an attacker pushes
        // a 100x print 5 seconds before `now`. Its weight is 5/3600 of the window.
        let mut r = ObservationRing::<64>::new();
        for i in 0..60i64 {
            r.push(1_000, i * 60).unwrap(); // ts 0..3540
        }
        r.push(100_000, 3595).unwrap();
        let twap = r.twap(3600, 3600, &CFG).unwrap();
        // Exact: (1000*3595 + 100000*5)/3600 = 1137 (floor of 1137.5).
        assert_eq!(twap, 1137);
        // The bound: a 100x spike moved the hour TWAP < 14%.
        assert!(twap < 1_150);
    }

    #[test]
    fn overflow_is_refused_not_wrapped() {
        let r = ring_from::<8>(&[(u128::MAX, 0), (u128::MAX, 50)]);
        assert_eq!(r.twap(100, 100, &CFG), None);
    }
}
