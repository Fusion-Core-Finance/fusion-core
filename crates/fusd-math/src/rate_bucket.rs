//! Rate-bucket **bitmap** for redemption targeting (fusion-docs.md).
//!
//! Redemption must hit the **lowest borrower-set rate first** ("debt-in-front"). Quantizing each
//! position's `user_rate` into fixed-width buckets and marking non-empty buckets in a bitmap lets
//! `redeem` prove — via **find-first-set-bit** — that it starts at the lowest non-empty bucket and
//! can't skip a lower one (a pure off-chain "is this list sorted?" proof is unsound: it can't show a
//! lower-rate position wasn't *skipped*). The bucket key is **price-independent** (it changes only
//! on `adjust_rate`, never on an oracle move) — the property that keeps churn on the one shared
//! `Market` write minimal, mirroring Uniswap-v3's `TickBitmap` and Liquity v1's NICR-keyed list.
//!
//! This module is the pure bit math over a caller-owned `&[u64]` word array (the on-chain account
//! owns the storage, as with the Reactor-Pool grid). `num_buckets == words.len() * 64`. A bit is
//! set iff its bucket holds ≥1 position with debt; the program flips it only on a bucket's
//! empty↔non-empty transition (tracked via per-bucket member counts), so writes are rare.

use bnum::types::U256;

/// Map a borrower rate (bps) to a bucket index in `[0, num_buckets)`. `width_bps` is the bucket
/// width (a clamped governance param); rates at/above the top bucket clamp into the last one.
#[inline]
pub fn bucket_of(rate_bps: u16, width_bps: u16, num_buckets: usize) -> usize {
    debug_assert!(width_bps > 0 && num_buckets > 0);
    let raw = (rate_bps / width_bps.max(1)) as usize;
    raw.min(num_buckets - 1)
}

/// `words.len() * 64` — the number of representable buckets.
#[inline]
pub fn num_buckets(words: &[u64]) -> usize {
    words.len() * 64
}

/// Mark bucket `b` non-empty. Precondition: `b < num_buckets(words)` (callers derive `b` via
/// [`bucket_of`], which clamps).
#[inline]
pub fn set(words: &mut [u64], b: usize) {
    words[b >> 6] |= 1u64 << (b & 63);
}

/// Mark bucket `b` empty.
#[inline]
pub fn clear(words: &mut [u64], b: usize) {
    words[b >> 6] &= !(1u64 << (b & 63));
}

/// Is bucket `b` non-empty?
#[inline]
pub fn is_set(words: &[u64], b: usize) -> bool {
    words[b >> 6] & (1u64 << (b & 63)) != 0
}

/// The lowest non-empty bucket, or `None` if every bucket is empty (the **find-first-set** that
/// `redeem` starts from — it cannot target a higher bucket while a lower one is non-empty).
#[inline]
pub fn first_set(words: &[u64]) -> Option<usize> {
    for (i, &w) in words.iter().enumerate() {
        if w != 0 {
            return Some(i * 64 + w.trailing_zeros() as usize);
        }
    }
    None
}

/// The lowest non-empty bucket `>= from`, or `None`. (`from >= num_buckets` ⇒ `None`.)
#[inline]
pub fn first_set_from(words: &[u64], from: usize) -> Option<usize> {
    let n = num_buckets(words);
    if from >= n {
        return None;
    }
    let start_word = from >> 6;
    // Mask off the bits below `from` in the starting word.
    let first_word = words[start_word] & (u64::MAX << (from & 63));
    if first_word != 0 {
        return Some(start_word * 64 + first_word.trailing_zeros() as usize);
    }
    for i in (start_word + 1)..words.len() {
        if words[i] != 0 {
            return Some(i * 64 + words[i].trailing_zeros() as usize);
        }
    }
    None
}

/// Count of non-empty buckets (popcount). Diagnostic / test helper.
#[inline]
pub fn count_set(words: &[u64]) -> u32 {
    words.iter().map(|w| w.count_ones()).sum()
}

/// Order two positions by collateral ratio (ascending). Within a redemption bucket the program
/// redeems **lowest-CR-first** (adversarial fix). CR `= ink·spot / (art·rate)`; `spot`/`rate`
/// are common across a market, so it compares `ink_a·art_b` vs `ink_b·art_a` (cross-multiplied in
/// 256 bits, no division). Both `art` must be `> 0` (redemption candidates always carry debt).
#[inline]
pub fn cmp_collateral_ratio(
    ink_a: u64,
    art_a: u128,
    ink_b: u64,
    art_b: u128,
) -> core::cmp::Ordering {
    let lhs = U256::from(ink_a) * U256::from(art_b);
    let rhs = U256::from(ink_b) * U256::from(art_a);
    lhs.cmp(&rhs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    fn grid() -> Vec<u64> {
        vec![0u64; 4] // 256 buckets
    }

    #[test]
    fn bucket_of_quantizes_and_clamps() {
        // 256 buckets @ 0.10% (10 bps) -> 0..25.5%.
        assert_eq!(bucket_of(0, 10, 256), 0);
        assert_eq!(bucket_of(9, 10, 256), 0);
        assert_eq!(bucket_of(10, 10, 256), 1);
        assert_eq!(bucket_of(505, 10, 256), 50); // 5.05% -> bucket 50
        assert_eq!(bucket_of(2_550, 10, 256), 255); // top of range
        assert_eq!(bucket_of(60_000, 10, 256), 255); // clamps into the last bucket
    }

    #[test]
    fn set_clear_is_set_roundtrip() {
        let mut g = grid();
        assert!(!is_set(&g, 100));
        set(&mut g, 100);
        assert!(is_set(&g, 100));
        assert_eq!(count_set(&g), 1);
        // distinct bucket in another word
        set(&mut g, 200);
        assert!(is_set(&g, 200));
        assert_eq!(count_set(&g), 2);
        clear(&mut g, 100);
        assert!(!is_set(&g, 100));
        assert!(is_set(&g, 200));
        assert_eq!(count_set(&g), 1);
    }

    #[test]
    fn boundary_buckets() {
        let mut g = grid();
        set(&mut g, 0);
        set(&mut g, 63); // top of word 0
        set(&mut g, 64); // bottom of word 1
        set(&mut g, 255); // top of the grid
        assert!(is_set(&g, 0) && is_set(&g, 63) && is_set(&g, 64) && is_set(&g, 255));
        assert_eq!(count_set(&g), 4);
    }

    #[test]
    fn first_set_finds_lowest() {
        let mut g = grid();
        assert_eq!(first_set(&g), None);
        set(&mut g, 130);
        set(&mut g, 70);
        set(&mut g, 200);
        assert_eq!(first_set(&g), Some(70), "lowest non-empty bucket");
        clear(&mut g, 70);
        assert_eq!(first_set(&g), Some(130));
        clear(&mut g, 130);
        clear(&mut g, 200);
        assert_eq!(first_set(&g), None);
    }

    #[test]
    fn first_set_lowest_is_zero() {
        let mut g = grid();
        set(&mut g, 0);
        set(&mut g, 5);
        assert_eq!(first_set(&g), Some(0));
    }

    #[test]
    fn first_set_from_resumes() {
        let mut g = grid();
        set(&mut g, 10);
        set(&mut g, 70);
        set(&mut g, 71);
        set(&mut g, 200);
        assert_eq!(first_set_from(&g, 0), Some(10));
        assert_eq!(first_set_from(&g, 10), Some(10)); // inclusive
        assert_eq!(first_set_from(&g, 11), Some(70));
        assert_eq!(first_set_from(&g, 64), Some(70)); // crosses a word boundary
        assert_eq!(first_set_from(&g, 71), Some(71));
        assert_eq!(first_set_from(&g, 72), Some(200));
        assert_eq!(first_set_from(&g, 201), None);
        assert_eq!(first_set_from(&g, 256), None); // out of range
    }

    #[test]
    fn first_set_from_within_word_masks_correctly() {
        let mut g = grid();
        // bits 1 and 40 in word 0
        set(&mut g, 1);
        set(&mut g, 40);
        assert_eq!(first_set_from(&g, 0), Some(1));
        assert_eq!(first_set_from(&g, 2), Some(40));
        assert_eq!(first_set_from(&g, 41), None);
    }

    #[test]
    fn collateral_ratio_ordering() {
        use core::cmp::Ordering;
        // CR_a = 200/100 = 2.0; CR_b = 150/100 = 1.5 -> b is lower.
        assert_eq!(cmp_collateral_ratio(200, 100, 150, 100), Ordering::Greater);
        assert_eq!(cmp_collateral_ratio(150, 100, 200, 100), Ordering::Less);
        // equal CR (same ratio, different scale): 200/100 == 400/200.
        assert_eq!(cmp_collateral_ratio(200, 100, 400, 200), Ordering::Equal);
        // sort a set lowest-CR-first
        let mut v = vec![(300u64, 100u128), (150, 100), (250, 100)];
        v.sort_by(|a, b| cmp_collateral_ratio(a.0, a.1, b.0, b.1));
        assert_eq!(v, vec![(150, 100), (250, 100), (300, 100)]);
        // huge values don't overflow (256-bit intermediate)
        assert_eq!(cmp_collateral_ratio(u64::MAX, u128::MAX, u64::MAX, u128::MAX), Ordering::Equal);
    }

    #[test]
    fn full_grid_drain_order() {
        // Set every bucket, then drain lowest-first via first_set_from, confirming strict ascending.
        let mut g = grid();
        for b in 0..256 {
            set(&mut g, b);
        }
        let mut expected = 0usize;
        let mut cursor = 0usize;
        while let Some(b) = first_set_from(&g, cursor) {
            assert_eq!(b, expected, "buckets drain in strict ascending order");
            clear(&mut g, b);
            expected += 1;
            cursor = b; // re-scan from here; the bit is now cleared
        }
        assert_eq!(expected, 256, "all buckets visited once, in order");
        assert_eq!(first_set(&g), None);
    }

    // The cheap `bucket_of` properties are pure arithmetic, so run a LARGE random sample each — Kani
    // is exhaustive over tiny inputs; proptest hammers wide random inputs (10,000 cases per property).
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        // RULE 1: the answer is always a valid index (0 <= b < num).
        #[test]
        fn bucket_of_always_in_range(
            rate in any::<u16>(),
            width in 1u16..=u16::MAX,   // avoid 0 -> the divide-by-zero guard
            num in 1usize..=1024,
        ) {
            prop_assert!(bucket_of(rate, width, num) < num);
        }

        // RULE 2: it matches the simple independent reference.
        #[test]
        fn bucket_of_matches_reference(
            rate in any::<u16>(),
            width in 1u16..=u16::MAX,
            num in 1usize..=1024,
        ) {
            let reference = ((rate / width) as usize).min(num - 1);
            prop_assert_eq!(bucket_of(rate, width, num), reference);
        }

        // RULE 3: monotonic — a higher rate never gives a lower bucket.
        #[test]
        fn bucket_of_is_monotonic(
            a in any::<u16>(),
            b in any::<u16>(),
            width in 1u16..=u16::MAX,
            num in 1usize..=1024,
        ) {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            prop_assert!(bucket_of(lo, width, num) <= bucket_of(hi, width, num));
        }
    }

    // The stateful bitmap test is heavy per case (each case is a long random op-sequence with full
    // cross-checks after every op), so it runs fewer cases — but 1,000 random 300-op walks is still a
    // massive amount of total coverage (~300k operations, each fully verified).
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1_000))]

        // `set(b)` marks bucket `b` and disturbs NO other bucket — starting from a RANDOM grid (the
        // full "touches only b" property), and it is idempotent on a re-set. `b in 0..256` keeps it
        // inside the 4-word grid's range, so `set`'s precondition is always honored.
        #[test]
        fn set_marks_only_target_bucket(
            initial in any::<[u64; 4]>(),
            b in 0usize..256,
        ) {
            let before = initial;          // [u64; 4] is Copy — a snapshot of the starting grid
            let mut words = initial;
            set(&mut words, b);

            // 1. the target bucket is now set
            prop_assert!(is_set(&words, b));
            // 2. every OTHER bucket is byte-for-byte unchanged from the starting grid
            for other in 0..256usize {
                if other != b {
                    prop_assert_eq!(is_set(&words, other), is_set(&before, other));
                }
            }
            // 3. idempotent: setting an already-set bucket changes nothing
            let mut twice = words;
            set(&mut twice, b);
            prop_assert_eq!(twice, words);
        }

        // `is_set` is a READER, so its meaningful properties cross-check it against an INDEPENDENT
        // computation of the same fact (not a re-derivation of its own bit math). On a random grid:

        // counting the "true" buckets one-by-one via `is_set` must equal `count_set`'s popcount
        // (shift+mask vs hardware `count_ones` — two techniques, same truth).
        #[test]
        fn is_set_count_agrees_with_count_set(words in any::<[u64; 4]>()) {
            let by_probe = (0..256usize).filter(|&b| is_set(&words, b)).count() as u32;
            prop_assert_eq!(by_probe, count_set(&words));
        }

        // the lowest bucket where `is_set` is true must equal `first_set`
        // (linear scan vs `trailing_zeros` — two techniques, same truth). `None` when all empty.
        #[test]
        fn is_set_lowest_agrees_with_first_set(words in any::<[u64; 4]>()) {
            let lowest = (0..256usize).find(|&b| is_set(&words, b));
            prop_assert_eq!(lowest, first_set(&words));
        }

        // `clear(b)` unmarks bucket `b` and disturbs NO other bucket (the mirror of the `set`
        // property), and is idempotent on a re-clear. Random starting grid; `b` kept in range.
        #[test]
        fn clear_unmarks_only_target_bucket(
            initial in any::<[u64; 4]>(),
            b in 0usize..256,
        ) {
            let before = initial;
            let mut words = initial;
            clear(&mut words, b);

            // 1. the target bucket is now clear
            prop_assert!(!is_set(&words, b));
            // 2. every OTHER bucket is unchanged from the starting grid
            for other in 0..256usize {
                if other != b {
                    prop_assert_eq!(is_set(&words, other), is_set(&before, other));
                }
            }
            // 3. idempotent: clearing an already-clear bucket changes nothing
            let mut twice = words;
            clear(&mut twice, b);
            prop_assert_eq!(twice, words);
        }

        // `first_set_from(k)` = the lowest occupied bucket `>= k` — fuzzed over a RANDOM grid AND a
        // RANDOM cursor `k` (in-range, the 256 edge, and out-of-range), against an independent
        // `BTreeSet` reference. This is the dedicated fuzz of the partial-first-word MASKING line (the
        // file's most bug-prone spot); the stateful test only probes a fixed list of cursors.
        #[test]
        fn first_set_from_agrees_with_reference(
            words in any::<[u64; 4]>(),
            k in 0usize..=300,
        ) {
            let model: BTreeSet<usize> = (0..256).filter(|&b| is_set(&words, b)).collect();
            let expected = model.range(k..).next().copied();
            prop_assert_eq!(first_set_from(&words, k), expected);
        }

        #[test]
        fn bitmap_always_matches_btreeset(
            // a random sequence of up to 300 ops: (turn_on?, which_bucket 0..256)
            ops in prop::collection::vec((any::<bool>(), 0usize..256), 0..300)
        ) {
            let mut words = [0u64; 4];        // the REAL bitmap (256 buckets)
            let mut model = BTreeSet::new();  // the DUMB reference

            for (turn_on, b) in ops {
                // apply the same op to both
                if turn_on { set(&mut words, b);   model.insert(b); }
                else       { clear(&mut words, b); model.remove(&b); }

                // after EVERY op, the two must agree on everything:
                prop_assert_eq!(first_set(&words), model.iter().next().copied());
                prop_assert_eq!(count_set(&words) as usize, model.len());

                // membership across the whole range
                for probe in 0..256 {
                    prop_assert_eq!(is_set(&words, probe), model.contains(&probe));
                }
                // first_set_from at the danger spots (word boundaries 63→64, ends)
                for &k in &[0usize, 1, 62, 63, 64, 65, 127, 128, 191, 192, 255, 256] {
                    prop_assert_eq!(first_set_from(&words, k), model.range(k..).next().copied());
                }
            }
        }

        #[test]
        fn set_marks_the_bucket(b in 0usize..256) {
            let mut words = [0u64; 4];   // empty grid
            set(&mut words, b);
            prop_assert!(is_set(&words, b));
            prop_assert_eq!(count_set(&words), 1);   // ONLY b is set
            set(&mut words, b);                       // do it again
            prop_assert_eq!(count_set(&words), 1);   // idempotent: still just one
        }

    }

    // `cmp_collateral_ratio` is the redemption sort comparator (used by `redeem.rs`) and had ZERO
    // formal/fuzz coverage. The doc comment states `art > 0` for redemption candidates, so every
    // position here carries `art >= 1`. A comparator backing a `sort_by` MUST be a valid total order
    // (anti-symmetric + transitive) or it silently corrupts sorts.
    proptest! {
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        // ANTI-SYMMETRY: cmp(a, b) is the reverse of cmp(b, a) for every pair.
        #[test]
        fn cmp_cr_is_anti_symmetric(
            ink_a in any::<u64>(), art_a in 1u128..=u128::MAX,
            ink_b in any::<u64>(), art_b in 1u128..=u128::MAX,
        ) {
            prop_assert_eq!(
                cmp_collateral_ratio(ink_a, art_a, ink_b, art_b),
                cmp_collateral_ratio(ink_b, art_b, ink_a, art_a).reverse(),
            );
        }

        // TRANSITIVITY: over random triples, a <= b and b <= c implies a <= c (total-order law that a
        // sort comparator must obey).
        #[test]
        fn cmp_cr_is_transitive(
            ink_a in any::<u64>(), art_a in 1u128..=u128::MAX,
            ink_b in any::<u64>(), art_b in 1u128..=u128::MAX,
            ink_c in any::<u64>(), art_c in 1u128..=u128::MAX,
        ) {
            use core::cmp::Ordering::Greater;
            let ab = cmp_collateral_ratio(ink_a, art_a, ink_b, art_b);
            let bc = cmp_collateral_ratio(ink_b, art_b, ink_c, art_c);
            if ab != Greater && bc != Greater {
                // a <= b and b <= c  =>  a <= c
                prop_assert_ne!(cmp_collateral_ratio(ink_a, art_a, ink_c, art_c), Greater);
            }
        }

        // SCALE-INVARIANCE: scaling ONE position's (ink, art) by the same positive factor k leaves its
        // CR (and so the ordering vs the other position) unchanged. `k` and `ink` are kept small so the
        // scaled ink stays within u64; art has headroom to spare.
        #[test]
        fn cmp_cr_is_scale_invariant(
            ink_a in 0u64..=u32::MAX as u64, art_a in 1u128..=u64::MAX as u128,
            ink_b in any::<u64>(), art_b in 1u128..=u128::MAX,
            k in 1u64..=1_000,
        ) {
            let scaled_ink = ink_a * k;             // <= u32::MAX * 1000 < u64::MAX
            let scaled_art = art_a * k as u128;     // <= u64::MAX * 1000 < u128::MAX
            prop_assert_eq!(
                cmp_collateral_ratio(ink_a, art_a, ink_b, art_b),
                cmp_collateral_ratio(scaled_ink, scaled_art, ink_b, art_b),
            );
        }

        // AGREEMENT WITH AN INDEPENDENT WIDE-PRECISION REFERENCE: compare against a `u128` cross-
        // multiply computed independently of the production 256-bit path. Constraining `art <= u64::MAX`
        // makes `ink (u64) * art` fit in u128, so the reference can't overflow and needs no bnum.
        #[test]
        fn cmp_cr_matches_u128_reference(
            ink_a in any::<u64>(), art_a in 1u128..=u64::MAX as u128,
            ink_b in any::<u64>(), art_b in 1u128..=u64::MAX as u128,
        ) {
            let lhs = ink_a as u128 * art_b;
            let rhs = ink_b as u128 * art_a;
            prop_assert_eq!(
                cmp_collateral_ratio(ink_a, art_a, ink_b, art_b),
                lhs.cmp(&rhs),
            );
        }
    }

}
