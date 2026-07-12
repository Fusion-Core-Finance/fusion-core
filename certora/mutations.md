# Certora acceptance: the mutation matrix

A passing rule proves nothing unless it **fails against broken code** (the Certora `rule_sanity`/vacuity
check is the in-prover half, this table is the end-to-end half). For each invariant, this records the
production-path mutation that MUST fail the runnable suite, and separately the class of mutation that
flips the CVLR rule. A rule/suite that still passes against the mutated program is vacuous — fix it
(usually an over-strong `cvlr_assume!`), don't ship it.

**Mutation class** (every mutation cell is tagged):

- **PROD-FN** — a mutation inside a production function in the rule's cone (a shared
  `supply_transition` TRANSITION fn's algebra, the `bucket` helpers, `recovery::absorb`,
  `fusd_oracle::aggregate`). Flips the Certora rule AND the runnable suite — the highest assurance
  class. NOT in this class: `impl SupplyNum for u128` (the checked-arithmetic trait methods the
  transitions call in production) — the rules monomorphize the NativeInt impl, so a u128-impl
  mutation is invisible to every rule and is HANDLER-class coverage (the module's unit tests pin
  each checked-op edge; litesvm covers the composed behavior).
- **HANDLER** — a mutation in handler-only code, at the shared-transition call site (dropping the
  call, or the assignment of the returned post-state), or inside `impl SupplyNum for u128`. Flips
  ONLY the runnable suite; every Certora rule stays green by construction — the documented residual
  gap.
- **IN-RULE** — an edit to the rule's own model. Validates only that the assertion is non-trivial;
  zero production coverage. Recorded for history, never counted as coverage.

The **Certora** column is ticked only where the CVLR rule was run on the cloud and confirmed to flip from
VERIFIED to VIOLATED under a **PROD-FN** mutation — a HANDLER mutation must never tick it. The
**Runnable-verified** column is the same discipline at the litesvm layer (apply the mutation to the
program, rebuild the `.so`, confirm the named suite FAILs, then revert). Rows left ☐ are honestly
not-yet-run, not assumed.

The `S*`/`B*`/`L*`/`R*` IDs are stable, **order-independent** labels, not a positional order: the
`rule` array in each `.conf` is unordered, so an ID's number need not match where its rule is defined
(e.g. `settle_bad_debt` is `S5` here but is the seventh of the eight rules in `supply.conf` /
`certora.rs`).

| ID | Rule(s) | Mutation (production path) | Must fail | Runnable-verified | Certora |
|----|---------|----------------------------|-----------|:---:|:---:|
| S1 | `supply_preserved_by_borrow_ghost` | **PROD-FN:** in `supply_transition::borrow`, `new_agg ← agg0` (skip the `cadd` — mint + book the fee without growing agg). **HANDLER:** in `borrow.rs`, drop the transition call or the `agg_recorded_debt = d.new_agg` commit. | supply identity: `circulating > agg − unminted + bad` | ✅ HANDLER: `assert_supply_invariant` fired (`circulating 15_000_000_000` vs `0`). PROD-FN: ☐ (`litesvm_c7_borrow_fee`) | ✅ rule VERIFIED post-M-01 (all 8 supply rules, non-vacuous); PROD-FN flip cloud-confirmed: `new_agg ← agg0` in `supply_transition::borrow` → the rule reports FAIL, reverted. (The pre-M-01 replay-the-delta rule's cone made a `borrow.rs` flip impossible — its recorded flip validated the in-rule model, class IN-RULE.) |
| S2 | `supply_preserved_by_repay_ghost` | **PROD-FN:** in `supply_transition::repay`, `new_agg: agg0` (skip the `csub` — burn without un-booking). **HANDLER:** in `repay.rs`, drop the call or the `d.new_agg` commit. | supply identity (agg too high) | ✅ HANDLER: `assert_supply_invariant` fired in `invariants_fuzz_plain`. PROD-FN: ☐ (`litesvm_interest` / `litesvm_b1_floors` / `litesvm_shutdown`) | ✅ rule VERIFIED (cloud, post-M-01, non-vacuous); PROD-FN flip cloud-confirmed: the shared-fn mutation → rule FAIL, reverted, clean tree re-VERIFIED |
| S3 | `supply_preserved_by_refresh_market_ghost` | **PROD-FN:** in `supply_transition::refresh`, `new_unminted ← pending` (skip the drain — interest double-counted). C16 sub-mutation: `new_bad ← bad0` (divert the paydown without retiring bad debt) → flips the same rule whenever `paydown > 0`; dropping any split term also breaks the rule's destination-sum assert. **HANDLER:** in `refresh_market.rs`, drop the call or a commit. | supply identity (double-counts interest / bad not retired) | ☐ (`litesvm_c7_borrow_fee` / `litesvm_keeper_reward` / `litesvm_interest`; C16 sub-mutation: `litesvm_c16_bad_debt_paydown`) | ✅ rule VERIFIED (cloud, post-M-01, non-vacuous); PROD-FN flip cloud-confirmed: the shared-fn mutation → rule FAIL, reverted, clean tree re-VERIFIED |
| S4 | `supply_preserved_by_liquidate_ghost` | **PROD-FN:** in `supply_transition::liquidate`, `new_bad: bad0` (drop the un-homed booking); secondary: remove `.csub(unhomed)` from the `new_agg` chain → flips the same rule. **HANDLER:** in `liquidate.rs`, drop the call or a commit. | supply identity (agg dropped, bad not raised) | ☐ (needs an un-homed liquidation — `litesvm_value_recovery` / `litesvm_shutdown` / `litesvm_backstop_draw`) | ✅ rule VERIFIED (cloud, post-M-01, non-vacuous); PROD-FN flip cloud-confirmed: the shared-fn mutation → rule FAIL, reverted, clean tree re-VERIFIED |
| S5 | `supply_preserved_by_settle_bad_debt_ghost` | **PROD-FN:** in `supply_transition::settle_bad_debt`, `return Some(bad0)` (burn but skip the retire). **HANDLER:** in `settle_bad_debt.rs`, drop the call or the commit. | supply identity | ☐ (`litesvm_value_recovery` / `litesvm_c16_bad_debt_paydown`) | ✅ rule VERIFIED (cloud, post-M-01, non-vacuous); PROD-FN flip cloud-confirmed: the shared-fn mutation → rule FAIL, reverted, clean tree re-VERIFIED |
| S6 | `supply_preserved_by_redeem_ghost` | **PROD-FN:** in `supply_transition::redeem_step`, `new_agg: agg0` (burn the face value without the debt decrement) — ONE shared step fn (deliberate), so this flips BOTH the redeem AND urgent_redeem rules (S7). **HANDLER:** in `redeem.rs`, drop the call or the commit — the layer that distinguishes the two handlers. | supply identity (agg too high) | ☐ (`litesvm_zombie_bucket` / `litesvm_b1_floors`) | ✅ rule VERIFIED (cloud, post-M-01, non-vacuous); PROD-FN flip cloud-confirmed: the shared-fn mutation → rule FAIL, reverted, clean tree re-VERIFIED |
| S7 | `supply_preserved_by_urgent_redeem_ghost` | **PROD-FN:** the same shared `redeem_step` mutation as S6 (one fn — flips both rules). **HANDLER:** in `urgent_redeem.rs`, drop the call or the commit. | supply identity (agg too high) | ☐ (`litesvm_shutdown`) | ✅ rule VERIFIED (cloud, post-M-01, non-vacuous); PROD-FN flip cloud-confirmed: the shared-fn mutation → rule FAIL, reverted, clean tree re-VERIFIED |
| S8 | `supply_preserved_by_book_interest_ghost` | **PROD-FN:** in `supply_transition::book_interest`, `new_unminted: unminted0` (book into agg only). **HANDLER:** in `accrual.rs` / `adjust_rate.rs`, drop the call or a commit. | supply identity (agg rises with no unminted offset) | ☐ (`litesvm_interest` — the accruing borrow→refresh sequence AND the adjust_rate cooldown-fee path) | ✅ rule VERIFIED (cloud, post-M-01, non-vacuous); PROD-FN flip cloud-confirmed: the shared-fn mutation → rule FAIL, reverted, clean tree re-VERIFIED |
| B1 | `bitmap_preserved_by_borrow` (+ all `bitmap_preserved_by_*`); CVLR: `bitmap_coupling_preserved_by_add_member` | **HANDLER:** in `borrow.rs`, skip the `bucket::reconcile(...)` call (debtor never joins its bucket). **PROD-FN (CVLR):** drop `rb::set` in `bucket::add_member`. | bitmap coherence: stored `bucket` ≠ classification, or `counts[k]`≠members; CVLR: `words ⟺ counts` coupling false at bucket 0 | ✅ `assert_bitmap_coherent` fired (`stored bucket 0 != classification 10`) | ✅ `bitmap_coupling_preserved_by_add_member` flipped VERIFIED→VIOLATED under the dropped-`rb::set` mutation (`certora/bitmap_helper.conf`) |
| B2 | `bitmap_preserved_by_adjust_rate`; CVLR: `bitmap_coupling_preserved_by_remove_member` | **HANDLER:** in `adjust_rate.rs`, skip the `bucket::reconcile(...)` call on a rate move (bucket not moved). **PROD-FN (CVLR):** drop `rb::clear` in `bucket::remove_member`. | bitmap coherence (stored `bucket` ≠ new classification); CVLR: `words ⟺ counts` coupling false at bucket 0 | ✅ `assert_bitmap_coherent` fired (`stored bucket 60 != classification 179`) | ✅ `bitmap_coupling_preserved_by_remove_member` flipped VERIFIED→VIOLATED under the dropped-`rb::clear` mutation (`certora/bitmap_helper.conf`) |
| B3 | `redeem_targets_lowest_bucket_and_preserves_coherence` | **HANDLER:** in `redeem.rs`, accept candidates from an arbitrary bucket instead of `first_set` (skip the lowest-bucket check). | the "starts at lowest non-empty bucket" assertion | ⚠ NOT YET COVERED — the targeting assertion needs a redeem test that submits a non-lowest bucket's members. | ☐ |
| B4 | `bitmap_preserved_by_urgent_redeem` | **HANDLER:** in `urgent_redeem.rs`, skip a `bucket::reconcile(...)` call. | bitmap coherence | ☐ (runnable: an `urgent_redeem` op exists in the fuzz but only commits post-shutdown — add a shutdown-then-urgent_redeem scenario) | ☐ |
| L1 | `liquidate_partitions_the_full_debt` / `absorb_conserves_debt` | **PROD-FN (CVLR):** in `recovery.rs::absorb`, `let unhomed = 0;` (drop the un-homed remainder). **HANDLER:** in `liquidate.rs`'s waterfall, route only part of the realized debt (e.g. drop the redistribution leg) so the split under-sums. | `reactor + redist + buffer + global + unhomed == debt` (the `recovery::absorb` identity, `recovery.rs` / `liquidate.rs`) | ☐ (supply+vault in the fuzz catch most cases; the explicit split is `recovery::absorb` Kani) | ✅ `absorb_conserves_debt` VERIFIED on the cloud; mutation `let unhomed = 0;` (drop the un-homed remainder) flipped it to **VIOLATED** with a counterexample where `rem > global` |
| L2 | `unhomed_debt_always_trips_shutdown` | **HANDLER:** in the un-homed branch, book `bad_debt` but skip `market.shutdown = true`. | `unhomed>0 ⟹ shutdown` | ☐ (needs an un-homed liquidation — `litesvm_liquidation.rs` terminal-recovery) | ☐ |
| L3 | `absorb_unhomed_iff_no_tier_covers` (+ `absorb_unhomed_reachable` non-vacuity witness) | **PROD-FN:** in `recovery.rs::absorb`, reorder the GLOBAL tier ahead of the LOCAL buffer (`let global = rem.min(global_available); let rem = rem - global; let buffer = rem.min(buffer_balance); let unhomed = rem - buffer;`). | strict tier order: `global>0 ⟹ buffer==buffer_balance` (a tier fires only after every higher-priority tier is drained), `recovery.rs` | n/a (pure-u128 Certora rule) | ✅ Certora VERIFIED (`certora/absorb.conf`); mutation flipped it to VIOLATED |
| R1 | `rp_solvency_preserved_by_withdraw` | **HANDLER:** in `withdraw_from_reactor.rs`, pay out `amount` instead of `min(amount, compounded)` (drop the cap, over-pay). | pool solvency / withdraw over-pay → transfer reverts (no-lockout `.expect` fails) | ✅ `litesvm_reactor_realizability` fired ("withdraw_from_reactor must always succeed (realizable)": insufficient funds) | ☐ |
| R2 | `rp_provide_withdraw_round_trips_without_offset` | **HANDLER:** in `withdraw_from_reactor.rs`, snapshot at the wrong `P` so the no-offset round-trip returns ≠ x. | the round-trip `withdrawn == provided` (no offset) | ☐ | ☐ |
| R3 | `rp_full_drain_preserves_claimable_collateral` | **HANDLER:** in the offset epoch-roll, reset `P` without crediting the depositor's pre-roll collateral gain. | seized collateral no longer claimable | ☐ | ☐ |
| C1 | `c1_canonical_caps_collateral` / `c1_canonical_never_raises_collateral` | **PROD-FN:** in `crates/fusd-oracle/src/lib.rs::aggregate`, two DISTINCT mutations: **(a)** drop the LST cap `Some(c) => chosen.price.min(c)` → `Some(c) => chosen.price`; **(b)** flip `.min` → `.max`. | **(a)** breaks C1-CAP `collateral_price ≤ canonical` (when `price > c`) — but leaves C1-MONOTONE VERIFIED (both legs collapse to `price`); **(b)** breaks C1-MONOTONE WITH ≤ WITHOUT (when `c > price`) and re-breaks C1-CAP (when `price > c`) | ✅ host test `canonical_caps_collateral_but_not_debt` (`crates/fusd-oracle`) FAILs under mutation **(a)** (verified: `assertion left == right failed: collateral capped at the canonical mid`) | ✅ both rules VERIFIED (cloud); PROD-FN flip ☐ pending (runnable-layer flip confirmed) |

> A **HANDLER**-class mutation — deleting or bypassing a shared-transition / helper call in a handler
> — does NOT flip any Certora rule; only the litesvm layer catches it. Never tick the Certora column
> for a HANDLER mutation.

## How to run a mutation (runnable layer — works today, no cloud)

```bash
# 1. apply the mutation to the named source file (one line):
#    PROD-FN S rows: programs/fusd-core/src/supply_transition.rs; HANDLER rows: the handler .rs
# 2. rebuild the dev-oracle .so the litesvm harness loads
anchor build -- --features dev-oracle
# 3. the fuzz suite must now FAIL on the named invariant
cargo test -p fusd-integration-tests --test litesvm_invariants_fuzz   # S*/B* rows
cargo test -p fusd-integration-tests --test litesvm_reactor_realizability  # R* rows
# (each S row also names the targeted litesvm suites that catch its PROD-FN break)
# 4. revert
git checkout programs/fusd-core/src/   # instructions/<file>.rs, supply_transition.rs, accrual.rs, …
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
