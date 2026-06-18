# Certora acceptance: the mutation matrix

A passing rule proves nothing unless it **fails against broken code** (the Certora `rule_sanity`/vacuity
check is the in-prover half, this table is the end-to-end half). For each rule, this records the
deliberate production-path mutation that MUST make it FAIL. A rule that still passes against the mutated
program is vacuous — fix the rule (usually an over-strong `cvlr_assume!`), don't ship it.

The **Certora** column is ticked only where the CVLR rule was run on the cloud and confirmed to flip from
VERIFIED to VIOLATED under the mutation. The **Runnable-verified** column is the same discipline at the
litesvm layer (apply the mutation to the program, rebuild the `.so`, confirm the named suite FAILs, then
revert). Rows left ☐ are honestly not-yet-run, not assumed.

The `S*`/`B*`/`L*`/`R*` IDs are stable, **order-independent** labels, not a positional order: the
`rule` array in each `.conf` is unordered, so an ID's number need not match where its rule is defined
(e.g. `settle_bad_debt` is `S5` here but is defined last in `supply.conf` / `certora.rs`).

| ID | Rule(s) | Mutation (production path) | Must fail | Runnable-verified | Certora |
|----|---------|----------------------------|-----------|:---:|:---:|
| S1 | `supply_preserved_by_borrow_ghost` | In `borrow.rs`, drop `market.agg_recorded_debt = new_agg` (mint without booking the debt). | supply identity: `circulating > agg − unminted + bad` | ✅ `assert_supply_invariant` fired (`circulating 15_000_000_000` vs `0`) | ✅ rule VERIFIED; mutation → VIOLATED |
| S2 | `supply_preserved_by_repay_ghost` | In `repay.rs`, skip the `agg_recorded_debt = checked_sub(repay_amount)` decrement (burn without un-booking). CVLR in-rule: model `agg1 = agg0`. | supply identity (agg too high) | ✅ `assert_supply_invariant` fired in `invariants_fuzz_plain` | ☐ authored (`supply.conf`); pending cloud |
| S3 | `supply_preserved_by_refresh_market_ghost` | In `refresh_market.rs`, mint the interest into the buffer but skip zeroing `unminted_interest`. CVLR in-rule: model `unminted1 = unminted0`. | supply identity (double-counts interest) | ☐ | ☐ authored (`supply.conf`); pending cloud |
| S4 | `supply_preserved_by_liquidate_ghost` | In the un-homed branch, skip `bad_debt += unhomed` (drop the debt instead of booking it). CVLR in-rule: model `bad1 = bad0`. | supply identity (agg dropped, bad not raised) | ☐ (needs an un-homed liquidation — use `litesvm_liquidation.rs` terminal-recovery, not the fuzz) | ☐ authored (`supply.conf`); pending cloud |
| S5 | `supply_preserved_by_settle_bad_debt_ghost` | Burn the fUSD but skip `bad_debt -= amount`. CVLR in-rule: model `bad1 = bad0`. | supply identity | ☐ | ☐ authored (`supply.conf`); pending cloud |
| S6 | `supply_preserved_by_redeem_ghost` | In `redeem.rs`, burn the redeemed face value but skip the `agg_recorded_debt` decrement. CVLR in-rule: model `agg1 = agg0`. | supply identity (agg too high) | ☐ | ☐ authored (`supply.conf`); pending cloud |
| S7 | `supply_preserved_by_urgent_redeem_ghost` | In `urgent_redeem.rs`, burn for collateral but skip the `agg_recorded_debt` decrement. CVLR in-rule: model `agg1 = agg0`. | supply identity (agg too high) | ☐ | ☐ authored (`supply.conf`); pending cloud |
| B1 | `bitmap_preserved_by_borrow` (+ all `bitmap_preserved_by_*`); CVLR: `bitmap_coupling_preserved_by_add_member` | In `borrow.rs`, skip the `bucket::reconcile(...)` call (debtor never joins its bucket); CVLR: drop `rb::set` in `bucket::add_member`. | bitmap coherence: stored `bucket` ≠ classification, or `counts[k]`≠members; CVLR: `words ⟺ counts` coupling false at bucket 0 | ✅ `assert_bitmap_coherent` fired (`stored bucket 0 != classification 10`) | ✅ `bitmap_coupling_preserved_by_add_member` flipped VERIFIED→VIOLATED under the dropped-`rb::set` mutation (`certora/bitmap_helper.conf`) |
| B2 | `bitmap_preserved_by_adjust_rate`; CVLR: `bitmap_coupling_preserved_by_remove_member` | In `adjust_rate.rs`, skip the `bucket::reconcile(...)` call on a rate move (bucket not moved); CVLR: drop `rb::clear` in `bucket::remove_member`. | bitmap coherence (stored `bucket` ≠ new classification); CVLR: `words ⟺ counts` coupling false at bucket 0 | ✅ `assert_bitmap_coherent` fired (`stored bucket 60 != classification 179`) | ✅ `bitmap_coupling_preserved_by_remove_member` flipped VERIFIED→VIOLATED under the dropped-`rb::clear` mutation (`certora/bitmap_helper.conf`) |
| B3 | `redeem_targets_lowest_bucket_and_preserves_coherence` | In `redeem.rs`, accept candidates from an arbitrary bucket instead of `first_set` (skip the lowest-bucket check). | the "starts at lowest non-empty bucket" assertion | ⚠ NOT YET COVERED — the targeting assertion needs a redeem test that submits a non-lowest bucket's members. | ☐ |
| B4 | `bitmap_preserved_by_urgent_redeem` | In `urgent_redeem.rs`, skip a `bucket::reconcile(...)` call. | bitmap coherence | ☐ (runnable: an `urgent_redeem` op exists in the fuzz but only commits post-shutdown — add a shutdown-then-urgent_redeem scenario) | ☐ |
| L1 | `liquidate_partitions_the_full_debt` / `absorb_conserves_debt` | In the waterfall, route only part of the realized debt (e.g. drop the redistribution leg) so the split under-sums. | `reactor + redist + buffer + global + unhomed == debt` (the `recovery::absorb` identity, `recovery.rs` / `liquidate.rs`) | ☐ (supply+vault in the fuzz catch most cases; the explicit split is `recovery::absorb` Kani) | ✅ `absorb_conserves_debt` VERIFIED on the cloud; mutation `let unhomed = 0;` (drop the un-homed remainder) flipped it to **VIOLATED** with a counterexample where `rem > global` |
| L2 | `unhomed_debt_always_trips_shutdown` | In the un-homed branch, book `bad_debt` but skip `market.shutdown = true`. | `unhomed>0 ⟹ shutdown` | ☐ (needs an un-homed liquidation — `litesvm_liquidation.rs` terminal-recovery) | ☐ |
| L3 | `absorb_unhomed_iff_no_tier_covers` (+ `absorb_unhomed_reachable` non-vacuity witness) | In `recovery.rs::absorb`, reorder the GLOBAL tier ahead of the LOCAL buffer (`let global = rem.min(global_available); let rem = rem - global; let buffer = rem.min(buffer_balance); let unhomed = rem - buffer;`). | strict tier order: `global>0 ⟹ buffer==buffer_balance` (a tier fires only after every higher-priority tier is drained), `recovery.rs` | n/a (pure-u128 Certora rule) | ✅ Certora VERIFIED (`certora/absorb.conf`); mutation flipped it to VIOLATED |
| R1 | `rp_solvency_preserved_by_withdraw` | In `withdraw_from_reactor.rs`, pay out `amount` instead of `min(amount, compounded)` (drop the cap, over-pay). | pool solvency / withdraw over-pay → transfer reverts (no-lockout `.expect` fails) | ✅ `litesvm_reactor_realizability` fired ("withdraw_from_reactor must always succeed (realizable)": insufficient funds) | ☐ |
| R2 | `rp_provide_withdraw_round_trips_without_offset` | In `withdraw_from_reactor.rs`, snapshot at the wrong `P` so the no-offset round-trip returns ≠ x. | the round-trip `withdrawn == provided` (no offset) | ☐ | ☐ |
| R3 | `rp_full_drain_preserves_claimable_collateral` | In the offset epoch-roll, reset `P` without crediting the depositor's pre-roll collateral gain. | seized collateral no longer claimable | ☐ | ☐ |
| C1 | `c1_canonical_caps_collateral` / `c1_canonical_never_raises_collateral` | In `crates/fusd-oracle/src/lib.rs::aggregate`, two DISTINCT mutations: **(a)** drop the LST cap `Some(c) => chosen.price.min(c)` → `Some(c) => chosen.price`; **(b)** flip `.min` → `.max`. | **(a)** breaks C1-CAP `collateral_price ≤ canonical` (when `price > c`) — but leaves C1-MONOTONE VERIFIED (both legs collapse to `price`); **(b)** breaks C1-MONOTONE WITH ≤ WITHOUT (when `c > price`) and re-breaks C1-CAP (when `price > c`) | ✅ host test `canonical_caps_collateral_but_not_debt` (`crates/fusd-oracle`) FAILs under mutation **(a)** (verified: `assertion left == right failed: collateral capped at the canonical mid`) | ☐ (authored to recipe; pending cloud — needs CERTORAKEY) |

## How to run a mutation (runnable layer — works today, no cloud)

```bash
# 1. apply the mutation to the named source file (one line)
# 2. rebuild the dev-oracle .so the litesvm harness loads
anchor build -- --features dev-oracle
# 3. the fuzz suite must now FAIL on the named invariant
cargo test -p fusd-integration-tests --test litesvm_invariants_fuzz   # S*/B* rows
cargo test -p fusd-integration-tests --test litesvm_reactor_realizability  # R* rows
# 4. revert
git checkout programs/fusd-core/src/instructions/<file>.rs
anchor build -- --features dev-oracle
```

## How to run a mutation (Certora layer)

```bash
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
export CERTORAKEY=<your-key>
# 1. apply the mutation; 2. run only the affected rule (rule_sanity is on in the conf):
certoraSolanaProver certora/supply.conf --rule supply_preserved_by_borrow_ghost   # must report a VIOLATION
# 3. revert. A mutation that leaves the rule VERIFIED means the rule is vacuous — tighten the
#    pre-state assumptions (usually an over-strong cvlr_assume!) until the rule fails as expected.
```
