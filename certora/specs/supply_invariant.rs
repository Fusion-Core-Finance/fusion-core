//! Certora/CVLR spec — **Invariant #1: global supply** (the highest-value rule; start here).
//!
//!   circulating fUSD == agg_recorded_debt − unminted_interest + bad_debt    (per market)
//!
//! where `circulating` is the **SPL mint supply** (NOT a `Market` field), so each rule must model the
//! `mint_to` / `burn` CPI's effect on the mint account, not merely the `Market` aggregates. This is the
//! part most likely to need Certora-support / the Solana examples (CPI-to-token-program modeling); see
//! `certora/README.md` → "Modeling the token CPI".
//!
//! Runnable counterpart (the mutation oracle for this rule): `integration-tests` →
//! `assert_supply_invariant` (`src/lib.rs`), asserted after every tx by `litesvm_invariants_fuzz.rs`.
//! Mutation that must break BOTH this rule and that suite: drop `agg_recorded_debt = new_agg` in
//! `borrow` (verified at the runnable layer — see `certora/mutations.md` row S1).
//!
//! ┌─ STATUS: spec scaffold ────────────────────────────────────────────────────────────────────┐
//! │ The CVLR API used here is CONFIRMED against docs.certora.com (Solana speclanguage + usage,     │
//! │ 2026-06): `cvlr::prelude::*`, `cvlr_assert!`/`cvlr_assume!`/`cvlr_satisfy!`, `#[rule]`,         │
//! │ `nondet()`, `clog!`. The remaining `// CONFIRM` markers are the HARNESS GLUE only — the         │
//! │ `*_context_nondet()` builders + the Anchor `handler(ctx: Context<…>)` invocation — which the    │
//! │ Certora Solana spec template supplies (README §"Bring-up" step 4). Trust the proof obligation   │
//! │ in each rule's doc comment; the glue symbols are placeholders for the template's scaffolding.   │
//! │ Round-trip-first: get a trivial `assert(true)` rule green BEFORE porting these.                 │
//! └────────────────────────────────────────────────────────────────────────────────────────────┘
#![cfg(feature = "certora")]
#![allow(unused)]

use cvlr::prelude::*; // confirmed: cvlr_assert!/cvlr_assume!/cvlr_satisfy!/nondet/clog! (cvlr 0.6)

// ─────────────────────────────────────────────────────────────────────────────────────────────────
// Helpers: the invariant predicate + a minimal, JUSTIFIED pre-state. Over-assuming here is the
// vacuity footgun (README §"Vacuity"): the ONLY admissible pre-state assumptions are (a) the invariant
// itself and (b) account-validity the program already enforces (canonical bumps / the position belongs
// to this market). Anything else risks a vacuously-green rule. Run rule_sanity on every rule.
// ─────────────────────────────────────────────────────────────────────────────────────────────────

/// circulating fUSD (the SPL mint `supply`). CONFIRM: how the Solana prover exposes the mint account's
/// supply — likely via a modeled token-program account read (`cvlr_solana` account helpers).
fn circulating(mint: &SplMint) -> u128 { mint.supply as u128 } // CONFIRM: SplMint type + field

/// The supply invariant predicate over a market + the fUSD mint.
fn supply_holds(m: &Market, mint: &SplMint) -> bool {
    circulating(mint) == m.agg_recorded_debt - m.unminted_interest + m.bad_debt
}

// ─────────────────────────────────────────────────────────────────────────────────────────────────
// One rule per supply-touching instruction. Pattern (inductive preservation):
//   1. cvlr_assume!(supply_holds(pre))         — the invariant in an arbitrary symbolic pre-state
//   2. <execute the instruction with nondet args + accounts>
//   3. cvlr_assert!(supply_holds(post))        — the invariant still holds
// If every instruction preserves it and init establishes it, it holds over every reachable tx.
// ─────────────────────────────────────────────────────────────────────────────────────────────────

/// borrow MINTS `amount`: circulating += amount, agg_recorded_debt += amount ⇒ invariant preserved.
#[rule]
pub fn supply_preserved_by_borrow() {
    let mut cx = borrow_context_nondet();              // CONFIRM: harness ctor for the Borrow ix accounts
    cvlr_assume!(supply_holds(&cx.market, &cx.fusd_mint));
    cvlr_assume!(account_valid(&cx));                  // canonical bumps / position∈market (program-enforced)
    let amount: u64 = nondet();
    let _ = borrow_handler(&mut cx, amount);           // CONFIRM: how to invoke the real handler symbolically
    cvlr_assert!(supply_holds(&cx.market, &cx.fusd_mint));
}

/// repay BURNS: circulating −= burned, agg_recorded_debt −= burned ⇒ preserved (incl. the cap at debt).
#[rule]
pub fn supply_preserved_by_repay() {
    let mut cx = repay_context_nondet();
    cvlr_assume!(supply_holds(&cx.market, &cx.fusd_mint));
    cvlr_assume!(account_valid(&cx));
    let amount: u64 = nondet();
    let _ = repay_handler(&mut cx, amount);
    cvlr_assert!(supply_holds(&cx.market, &cx.fusd_mint));
}

/// refresh_market MINTS the `unminted_interest` into the buffer: circulating += U, unminted −= U,
/// agg unchanged (the interest was folded into agg at accrual) ⇒ preserved. Also covers the keeper-cut.
#[rule]
pub fn supply_preserved_by_refresh_market() {
    let mut cx = refresh_context_nondet();
    cvlr_assume!(supply_holds(&cx.market, &cx.fusd_mint));
    cvlr_assume!(account_valid(&cx));
    let _ = refresh_market_handler(&mut cx);
    cvlr_assert!(supply_holds(&cx.market, &cx.fusd_mint));
}

/// liquidate: the RP-offset BURN and the buffer BURN reduce circulating; the victim debt leaves
/// `agg_recorded_debt`; un-homed debt moves to `bad_debt` (circulating unchanged, agg −D, bad +D).
/// Every branch of the waterfall must preserve the identity. (The per-case split conservation
/// `sp+redist+buffer+unhomed==debt` is the separate `liquidation_terminates` rule + Kani `recovery::absorb`.)
#[rule]
pub fn supply_preserved_by_liquidate() {
    let mut cx = liquidate_context_nondet();
    cvlr_assume!(supply_holds(&cx.market, &cx.fusd_mint));
    cvlr_assume!(account_valid(&cx));
    let _ = liquidate_handler(&mut cx);
    cvlr_assert!(supply_holds(&cx.market, &cx.fusd_mint));
}

/// redeem BURNS the redeemed face value and reduces target debt by the same ⇒ preserved (the fee is
/// retained collateral, not fUSD, so it does not move the supply identity — see the vault rule).
#[rule]
pub fn supply_preserved_by_redeem() {
    let mut cx = redeem_context_nondet();
    cvlr_assume!(supply_holds(&cx.market, &cx.fusd_mint));
    cvlr_assume!(account_valid(&cx));
    let amount: u64 = nondet();
    let _ = redeem_handler(&mut cx, amount);
    cvlr_assert!(supply_holds(&cx.market, &cx.fusd_mint));
}

/// urgent_redeem (shutdown wind-down): 0-fee burn-for-collateral; same supply algebra as `redeem`.
#[rule]
pub fn supply_preserved_by_urgent_redeem() {
    let mut cx = urgent_redeem_context_nondet();
    cvlr_assume!(supply_holds(&cx.market, &cx.fusd_mint));
    cvlr_assume!(account_valid(&cx));
    let amount: u64 = nondet();
    let _ = urgent_redeem_handler(&mut cx, amount);
    cvlr_assert!(supply_holds(&cx.market, &cx.fusd_mint));
}

/// settle_bad_debt BURNS recovered fUSD and reduces `bad_debt` by the same amount in lockstep
/// (circulating −X, bad −X) ⇒ preserved. The on-chain half of recapitalization.
#[rule]
pub fn supply_preserved_by_settle_bad_debt() {
    let mut cx = settle_bad_debt_context_nondet();
    cvlr_assume!(supply_holds(&cx.market, &cx.fusd_mint));
    cvlr_assume!(account_valid(&cx));
    let amount: u64 = nondet();
    let _ = settle_bad_debt_handler(&mut cx, amount);
    cvlr_assert!(supply_holds(&cx.market, &cx.fusd_mint));
}

// COVERAGE NOTE: the rules above are the COMPLETE set of fUSD mint/burn instructions. The remaining
// fUSD-touching instructions only *transfer* already-circulating fUSD between accounts (no mint, no
// burn), so they trivially preserve the supply identity and need no rule:
//   - fund_buffer / fund_backstop          — move fUSD into a protocol vault
//   - withdraw_backstop_excess             — move above-cap fUSD out of the reserve
//   - provide_to_reactor / withdraw_from_reactor — move fUSD in/out of the RP vault
// Pure collateral ops (deposit/withdraw/claim_*) touch no fUSD at all. All of these are covered by the
// VAULT rule (the 4-term reserve identity), not here. mint_to/burn happen ONLY in the rules above.
