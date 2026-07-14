//! Kani bounded-model-checking harnesses for the allocation/churn/reward math.
//!
//! These prove the conservation and bound contracts the Allocation Controller's epoch
//! plan is built on — the class of bug (a grant exceeding a round's remaining pool, a
//! directed target escaping its cap, an action overdrawing the churn budget, a payout
//! draining past the vault) that would silently misallocate pool stake or shares.
//!
//! HOW TO RUN (manual only — NOT wired into kani-audit.sh or any CI gate):
//!   cargo install --locked kani-verifier && cargo kani setup
//!   cargo kani -p fusion-stake-math
//!
//! TRACTABILITY: same u8-narrowing method as fusd-math's harnesses — CBMC's formula
//! grows with the number of SYMBOLIC INPUT BITS, and every property here (min-clamps,
//! per-round conservation, saturation counting) is scale-independent, so narrow
//! symbolic inputs prove the full logic. There is no wide-divide shim to route through:
//! this crate's only divisions are `u128 / u128` with small operands under these
//! harnesses, which CBMC handles directly.

use crate::churn::action_amount;
use crate::rewards::payout;
use crate::targets::{begin_round, directed_target, step};

/// One full capacity round conserves exactly: grants sum to `remaining - remaining_after`,
/// never exceed `remaining`, and a completed NON-final round saturates at least one
/// validator (the progress guarantee behind the MAX_NEUTRAL_ROUNDS termination bound).
// strength: STRONG — symbolic remaining/epoch/capacities over a full 3-validator round; proves exact per-round conservation, the granted-accumulator identity, remainder exhaustion, and the non-final⇒saturation progress lemma; both final and non-final outcomes covered.
#[kani::proof]
fn neutral_round_conserves_and_saturates() {
    let remaining = kani::any::<u8>() as u64;
    let epoch = kani::any::<u8>() as u64;
    let c0 = kani::any::<u8>() as u64;
    let c1 = kani::any::<u8>() as u64;
    let c2 = kani::any::<u8>() as u64;
    kani::assume(remaining >= 1);
    kani::assume(c0 >= 1 && c1 >= 1 && c2 >= 1); // unsaturated validators have capacity

    let mut round = begin_round(remaining, 3, epoch).unwrap();
    let g0 = step(&mut round, c0);
    let g1 = step(&mut round, c1);
    let g2 = step(&mut round, c2);
    assert!(round.is_complete());

    // Grants are individually capped by capacity.
    assert!(g0 <= c0 && g1 <= c1 && g2 <= c2);
    // Exact conservation: the accumulator equals the grants, and the round consumed
    // exactly (remaining - remaining_after) <= remaining.
    let sum = g0 + g1 + g2; // each <= remaining <= 255: no overflow
    assert_eq!(sum, round.granted);
    assert!(sum <= remaining);
    assert_eq!(round.remaining_after(), remaining - sum);
    // All integer remainders were assigned.
    assert_eq!(round.remainder_used, round.remainder);
    // Progress: a completed round either consumed everything or saturated someone.
    if round.remaining_after() > 0 {
        assert!(round.saturated >= 1);
    }
    // A post-completion step is inert (crank replay cannot double-grant).
    assert_eq!(step(&mut round, 7), 0);
    assert_eq!(round.granted, sum);
    kani::cover!(round.remaining_after() > 0); // a capacity-clipped round is reachable
    kani::cover!(round.remaining_after() == 0); // a fully-consumed round is reachable
    kani::cover!(round.saturated >= 1);
}

/// Directed targets never escape the lifecycle cap, and never exceed productive
/// lamports when directed shares are within supply (the plan-guarded regime).
// strength: STRONG — cap clamp proven unconditionally over fully symbolic u8 inputs (including d > s and s == 0); the productive bound proven over the entire plan-legal d <= s region; clipped and unclipped branches both covered.
#[kani::proof]
fn directed_target_cap_clamp() {
    let p = kani::any::<u8>() as u64;
    let d = kani::any::<u8>() as u64;
    let s = kani::any::<u8>() as u64;
    let cap = kani::any::<u8>() as u64;
    let t = directed_target(p, d, s, cap);
    assert!(t <= cap); // unconditional: holds even for d > s or s == 0
    if d <= s {
        assert!(t <= p); // within the plan-guarded regime the floor is <= productive
    }
    kani::cover!(t == cap && cap > 0); // a real cap clip is reachable
    kani::cover!(d <= s && s > 0 && t < cap); // an unclipped floor is reachable
}

/// Churn actions never exceed any bound, honor the minimum-action floor (full drains
/// exempt), and a folded sequence can never overdraw the global budget.
// strength: STRONG — per-action bounds over fully symbolic inputs incl. the full-drain exemption; the fold invariant (spent + budget == initial, budget never underflows) proven over a 3-action symbolic sequence; binding and zero-action branches covered.
#[kani::proof]
fn churn_budget_bound_over_fold() {
    let initial = kani::any::<u8>() as u64;
    let vcap = kani::any::<u8>() as u64;
    let min_action = kani::any::<u8>() as u64;
    let devs: [u8; 3] = kani::any();
    let srcs: [u8; 3] = kani::any();
    let drains: [bool; 3] = kani::any();

    let mut budget = initial;
    let mut spent = 0u64;
    let mut i = 0;
    while i < 3 {
        let dev = devs[i] as u64;
        let src = srcs[i] as u64;
        let a = action_amount(dev, budget, vcap, src, min_action, drains[i]);
        // Every bound holds on every action.
        assert!(a <= dev && a <= budget && a <= vcap && a <= src);
        // The minimum-action floor holds unless the deviation is a full drain.
        assert!(a == 0 || a >= min_action || drains[i]);
        budget -= a; // a <= budget just proven: cannot underflow
        spent += a; // <= 3 * 255: no overflow
        i += 1;
    }
    // The fold conserves the budget exactly and never exceeds it.
    assert_eq!(spent + budget, initial);
    assert!(spent <= initial);
    kani::cover!(spent > 0); // a paying sequence is reachable
    kani::cover!(spent == initial && initial > 0); // budget exhaustion is reachable
}

/// The crank reward payout never exceeds the task reward, the epoch budget or the
/// vault balance, and always equals one of the three (min-of-three, zero included).
// strength: STRONG — fully symbolic u64 inputs (pure min chain, no divide, tractable at full width); proves all three upper bounds and the equals-one-of-three identity; the unpaid-crank (empty vault) case covered.
#[kani::proof]
fn reward_payout_bound() {
    let r = kani::any::<u64>();
    let b = kani::any::<u64>();
    let v = kani::any::<u64>();
    let p = payout(r, b, v);
    assert!(p <= r && p <= b && p <= v);
    assert!(p == r || p == b || p == v);
    kani::cover!(p == 0 && r > 0); // cranks run unpaid on an empty vault/budget
    kani::cover!(p > 0);
}
