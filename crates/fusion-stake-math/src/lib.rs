//! fuSOL stake-pool Allocation Controller — pure math.
//!
//! Scope: the reserve split, validator lifecycle state machine, directed/neutral
//! target allocation (equal capacity rounds), churn/hysteresis limits, preference
//! countability, and crank reward payouts. Consumed by the fusion-stake-controller
//! program; **no accounts, no syscalls**, no clock reads, no randomness — every
//! function is a deterministic map from integer inputs to integer outputs, so the
//! same logic is host-testable, proptest-hammered, and Kani-provable.
//!
//! Conventions:
//! - All lamport and fuSOL-share quantities are `u64`.
//! - Every product goes through a `u128` intermediate (`u64 × u64` always fits
//!   `u128`, so no wider integer type is needed).
//! - Division is floor division; **no floats anywhere**.
//! - Checked/saturating arithmetic only; where saturation is used, the direction
//!   is documented and either provably unreachable or fail-safe.

#![cfg_attr(not(test), no_std)]

pub mod churn;
pub mod lifecycle;
pub mod preference;
pub mod reserve;
pub mod rewards;
pub mod targets;

/// Kani formal-verification harnesses (bounded model checking). Compiled ONLY under `cargo kani`
/// (the `kani` cfg) — excluded from every normal/test/SBF build, so it can never affect production.
#[cfg(kani)]
mod kani_proofs;

/// Basis-point denominator.
pub const BPS_DENOMINATOR: u64 = 10_000;

/// `floor(amount * bps / 10_000)` over a `u128` intermediate (never overflows: `u64 × u64`
/// always fits `u128`). The shared primitive for every bps-derived quantity in this crate
/// (reserve target, lifecycle caps, churn budget, per-validator move cap, hysteresis).
///
/// Saturates to `u64::MAX` if `bps > 10_000` pushes the quotient past `u64` — an over-unity
/// bps parameter means "larger than the whole pool", i.e. effectively uncapped, so saturating
/// UP is the fail-safe direction for a cap/budget. All spec parameters are well below 10_000.
#[inline]
pub fn bps_of(amount: u64, bps: u64) -> u64 {
    let q = u128::from(amount) * u128::from(bps) / u128::from(BPS_DENOMINATOR);
    u64::try_from(q).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bps_of_hand_vectors() {
        assert_eq!(bps_of(10_000, 200), 200); // 2%
        assert_eq!(bps_of(10_000, 0), 0);
        assert_eq!(bps_of(0, 5_000), 0);
        assert_eq!(bps_of(999, 1), 0); // floors: 999/10_000 = 0
        assert_eq!(bps_of(1_000_000_000_000, 5), 500_000_000); // 5 bps of 1000 SOL = 0.5 SOL
        assert_eq!(bps_of(u64::MAX, 10_000), u64::MAX); // 100% is the identity
        assert_eq!(bps_of(u64::MAX, 20_000), u64::MAX); // over-unity saturates (uncapped)
    }

    #[test]
    fn bps_of_at_most_amount_for_sub_unity_bps() {
        for amount in [0u64, 1, 7, 10_000, u64::MAX] {
            for bps in [0u64, 1, 200, 9_999, 10_000] {
                assert!(bps_of(amount, bps) <= amount);
            }
        }
    }
}
