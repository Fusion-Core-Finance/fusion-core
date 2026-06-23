# Fusion (FUSD) — Technical Reference

Fusion is an overcollateralized, multi-collateral CDP stablecoin protocol in the Liquity lineage, built natively for Solana with Anchor and intended to be governed — within hard, code-enforced bounds — by MetaDAO futarchy via the FUSION ownership token. Users lock approved collateral in isolated, per-collateral positions and mint the Fusion Dollar (FUSD) against it; the protocol holds no custody, runs no off-chain key, and exposes no privileged lever over user funds. Its $1 peg is held by four overlapping economic loops (permissionless redemption floor, repay arbitrage, the Reactor Pool, and user-set borrow rates), and its solvency rests on overcollateralization plus fast permissionless liquidation plus a pre-funded buffer — never on reflexive token dilution. This document is the canonical source-of-truth technical reference for Fusion, covering both the components already built and tested on-chain and those still designed but unbuilt. Where the two diverge, the divergence is stated explicitly rather than smoothed over.

**Status legend:**

- **[Built]** = implemented + tested on-chain
- **[Partial]** = scaffold / logic present, not fully wired
- **[Planned]** = designed, not yet built

*Reflects: current `master`.*

---

## Table of Contents

1. [Overview & Design Philosophy](#1-overview--design-philosophy)
2. [The CDP Core](#2-the-cdp-core)
3. [The Reactor Pool](#3-the-reactor-pool)
4. [Liquidation & Stability Mechanics](#4-liquidation--stability-mechanics)
5. [Redemption (the Peg Floor)](#5-redemption-the-peg-floor)
6. [Oracle System](#6-oracle-system)
7. [Governance & Risk Parameters](#7-governance--risk-parameters)
8. [Security Model & Invariants](#8-security-model--invariants)
9. [Prior Art & Competitive Landscape](#9-prior-art--competitive-landscape)
10. [Compliance & Regulatory Posture](#10-compliance--regulatory-posture)
11. [Reference, Build & Roadmap](#11-reference-build--roadmap)
12. [Component Status Summary](#12-component-status-summary)
13. [Document Conventions & Caveats](#13-document-conventions--caveats)

---

## 1. Overview & Design Philosophy

### 1.1 What FUSD is

FUSD is an **overcollateralized CDP stablecoin** in the Liquity lineage, built natively for Solana with Anchor and intended to be governed — within hard, code-enforced bounds — by MetaDAO futarchy. A user locks approved collateral in a *position* (a Maker "vault" / Liquity "trove") and mints FUSD against it; the protocol holds no custody, runs no off-chain key, and exposes no privileged lever over user funds. It is single-mint (one FUSD token), multi-collateral, and isolated per collateral.

The thesis is **"unstoppable cash": credible neutrality through code, not policy.** Every "we won't do X" is a thing the program *cannot* do, verifiable by reading the bytecode and the mint's authorities on-chain — not a promise. Concretely, that means:

- **No issuer custody.** Collateral sits in program-owned escrow PDAs; FUSD is minted/burned only by a program PDA (`[b"mint_authority"]`) inside `borrow`/`repay` under the `art*rate` solvency rules. There is no off-chain mint key.
- **No freeze authority.** The FUSD mint is **legacy SPL Token** with `freeze_authority = None`, set irreversibly at `InitializeMint` (no later instruction can add one). Legacy is chosen deliberately because it *physically lacks* Token-2022's censorship extensions (`DefaultAccountState`, `PermanentDelegate`, `TransferHook`, `Pausable`, `ConfidentialTransfer`, `MintCloseAuthority`), and so transfers never serialize behind a hook or pause.
- **No privileged redemption gate.** Anyone may redeem FUSD for face-value collateral at any time; redemption ordering is oracle-*independent* (it keys on the borrower's own chosen rate), so even an oracle outage cannot freeze the floor.
- **Fully permissionless liquidation.** Any position below its market's MCR is a valid, locally-checkable liquidation target for anyone. No keeper whitelist exists anywhere.
- **Bounded governance.** Futarchy may only tune risk parameters inside compile-time `[min,max]` clamps, may mark only registry/config accounts writable, and can never seize, freeze, mint outside the rules, or retroactively change existing positions' terms. The locks are structural, not procedural.

### 1.2 The four reinforcing peg loops

FUSD's $1 peg is held by four overlapping economic loops rather than any single mechanism. Three are **[Built]** and verified in litesvm; the fourth is built in its core but carries a **[Planned]** sub-component.

| # | Loop | What it does | Status |
|---|---|---|---|
| 1 | **Permissionless redemption floor** | Burn FUSD for $1 of collateral at face value (minus a flat fee). Sets a hard price floor: FUSD below $1 is a profitable arbitrage to redeem. Drains the lowest-rate debt first via the on-chain rate-bucket bitmap. | [Built] |
| 2 | **Repay arbitrage** | Buy sub-$1 FUSD, `repay` $1 of debt with it. Any holder of debt can profit, contracting supply when FUSD is weak. | [Built] |
| 3 | **Reactor Pool (RP)** | A standing pool of FUSD that buys liquidated collateral at a discount, burning FUSD as it absorbs bad positions — a buyer of last resort that contracts supply during stress. | [Built] |
| 4 | **User-set borrow rates** | Each borrower picks their own rate (Liquity v2 / BOLD); higher-rate debt is redeemed first ("debt-in-front"). Supply self-clears: rates rise until borrowers willing to bear redemption risk fill demand. | [Built] (rate field + bucketing + `adjust_rate` wired; the dynamic base-rate fee that discourages rate-gaming is [Planned]) |

Solvency (distinct from the peg) rests on overcollateralization + fast permissionless liquidation + a pre-funded surplus/insurance buffer — never on reflexive dilution of a governance or sister token.

### 1.3 Locked foundational decisions

Four decisions are locked and treated as constitutional. They drive everything downstream and are not open for casual revision.

| Decision | Choice | Why |
|---|---|---|
| **Collateral model** | Multi-collateral, **isolated markets** | Each approved collateral is its own `Market` with independent MCR/CCR/SCR, debt ceiling, oracle config, rate bounds, Reactor Pool, and surplus — all sharing one FUSD mint. Wins twice: risk isolation (no shared-basket contagion) and Sealevel parallelism (distinct collaterals never contend on a write-lock). |
| **Peg** | Liquity-style **hard redemptions** + repay arbitrage + Reactor Pool | Face-value redemption is the trustless, ungated floor; explicitly rejects any discretionary peg-defense gate. |
| **Interest rates** | **User-set** (Liquity v2 / BOLD), debt-in-front | Each borrower picks their rate; redemptions hit lowest-rate debt first. Futarchy sets only the min/max bounds, never an individual's rate. |
| **Framework** | **Anchor (Rust)**, zero-copy hot accounts | Hot/large state (the Reactor Pool grid, the redemption bitmap) uses `#[account(zero_copy)]` + `AccountLoader` + bytemuck `Pod` to stay within compute and account-size limits. |

A fifth and sixth decision are likewise locked: redemption targeting uses an **on-chain rate-bucket bitmap** (a monotone off-chain ordering proof was found provably unsound for rate-ordering), and **Recovery Mode is rejected** in favor of the BOLD per-market breaker set (liquidation eligibility is the static rule `health < MCR` regardless of system state). Both are **[Built]** in their v1 form.

### 1.4 System architecture: one program, isolation from the account layout

FUSD is **one on-chain Anchor program, `fusd-core`** (program ID `FuSiontgYvCc2N2Cinvo5gxSuxt2UfGxKMcbzkB67kud` on dev; regenerate for any real deploy). All instructions and account types live in it.

The critical design insight: **isolation and parallelism come from the *account* layout, not from splitting into many programs.** Sealevel parallelizes transactions by their **write-lock set** — two transactions run concurrently iff they don't both write the same account — and the binding scalability limit is the *per-writable-account* compute cap (~12M CU/block today), not the block budget. So FUSD shards state instead of sharding programs:

- **Hot-path ops (`open_position`/`deposit`/`withdraw`/`borrow`/`repay`) write only `Position` + `Market`.** Distinct users never contend (each owns a per-user `Position` PDA); distinct collaterals never contend (each has its own `Market`).
- This is the explicit rejection of Maker's single global `Vat`, which would serialize every state change behind one write-lock and one CU lane — fatal on Solana.
- `ProtocolConfig` is **read-only on the hot path** (read by every op, written almost never), so it never serializes.

Splitting peripherals (oracle/params) into a separately-upgradeable program is an **open** question, deferred until the immutability lockdown — the default is one program now.

### 1.5 Shared logic as compiled-in Rust crates (no CPI hop)

Reusable math and oracle logic live in **plain Rust crates compiled *into* `fusd-core`**, not as separate on-chain programs. The win is concrete: **no CPI hop, no extra write-lock, lower CU.** Two crates exist today:

| Crate | Role | Status |
|---|---|---|
| `fusd-math` | The fixed-point money-math core: U256-backed `mul_div` (floor/ceil), WAD/RAY mul-div, `ray_pow`, `present_debt` (the `art*rate` realization), `apply_bps`; the `reactor_pool` P/S product-sum, `redistribution` reward-per-unit accumulators, `rate_bucket` bitmap math, and the `oracle_scale` price scaling (`px_to_ray`, `usd_ray_to_spot`, `sqrt_price_q64_to_ray`). | [Built] (**113 host tests** + 25 Kani formal-verification harnesses; SBF-clean) |
| `fusd-oracle` | Pure validation/aggregation logic: `PriceView`, conf-ratio, asymmetric collateral/debt pricing, `aggregate` (the sole mode path, with the `fresh`-feed flag), and the `ObservationRing` DEX-TWAP. Now wired to `market.spot` by the `update_price`/`sample_twap` cranks (§6.4). | [Built] (33 tests; live feed parsing wired) |

Both crates are `no_std` (`fusd-math` is `#![cfg_attr(not(test), no_std)]`) and dependency-light, so they drop straight into the program with no host-only baggage on-chain.

### 1.6 Repository / crate layout

An Anchor workspace (`Cargo.toml` members: `programs/*`, `crates/*`, `integration-tests`; resolver 2):

| Path | Contents | Status |
|---|---|---|
| `programs/fusd-core/` | The CDP engine — all instructions + account types (`accrual`, `bucket`, `cdp`, `clmm`, `redist`, `reactor`, `reconcile`, `state`, `instructions`, `events`, `constants`, `errors` modules). | [Built] |
| `crates/fusd-math/` | WAD/RAY/RAD fixed-point, `ray_pow`, P/S product-sum, redistribution, rate-bucket bitmap; U256-backed. | [Built] |
| `crates/fusd-oracle/` | Pyth+Switchboard+DEX-TWAP validation, asymmetric pricing, freeze modes, TWAP ring; wired to `spot` by the live cranks. | [Built] |
| `integration-tests/` | In-process **litesvm** tests (host-only, never deployed) — shared harness + CDP/liquidation/redemption scenarios. Isolated from the program crate so its `dev-oracle` dep can't leak `dev_set_price` into the IDL/`.so`. | [Built] |
| `sdk/`, `keepers/`, `tests/`, `scripts/` | TS client + PDA derivation; permissionless bots; anchor-mocha e2e; deploy/init/verified-build/release-gate scripts. | [Partial]/[Planned] (`scripts/check-no-dev-oracle.sh` release gate is [Built]) |
| `docs/` | This technical reference + the CLMM pool-layout spec (`clmm-pool-layouts.md`). | [Built] |

A guarded build discipline runs throughout: `dev_set_price` (the test-only price setter) is gated behind a `dev-oracle` feature **excluded from production builds and the IDL**, enforced by `scripts/check-no-dev-oracle.sh` as a release gate. Production markets start frozen (`mint_frozen = true`, `spot = 0`), so they *cannot borrow* until the real `update_price` crank has run with healthy feeds — a safe default.

### 1.7 Dimensional fixed-point discipline

All money math uses dimensioned fixed-point with U256 intermediates (`crates/fusd-math/src/lib.rs`).

| Unit | Scale | Dimension |
|---|---|---|
| **WAD** | `1e18` | token quantities (`ink`, `art`) |
| **RAY** | `1e27` | rates / prices / the per-market interest accumulator (`rate`) |
| **RAD** | `1e45` (= WAD·RAY) | internal balances, so `debt[rad] = art[wad] * rate[ray]` is exact |

This makes the Maker-style accounting exact: present debt = `present_debt(art, rate)` = `ray_mul_up(art, rate)`, rounded **up** against the borrower. WAD and RAY are defined `pub const` in `fusd-math`; **RAD is currently a documented convention, not yet a defined constant** (it appears only in doc comments — exact RAD-scale balances aren't materialized in the v1 accounting, which works in WAD/RAY).

The non-negotiable rules:

- **U256 intermediates everywhere.** Any product of two WAD/RAY values overflows `u128` (e.g. `RAY*RAY = 1e54`), so every multiply/divide goes through an exact 256-bit `bnum::U256` (`mul_div_floor`/`mul_div_ceil`). A result that doesn't fit `u128` returns `None` rather than truncating — **no native bignum, no silent wrap.**
- **Multiply-before-divide**, to preserve precision.
- **Round against the protocol**: debt-increasing results round **up** (`*_up`), collateral-credit / debt-decreasing results round **down** (floor). The convention is documented per function.
- **Checked arithmetic in release.** `[profile.release]` sets `overflow-checks = true` (release mode otherwise drops them); verifiable builds additionally pin `lto = "fat"` + `codegen-units = 1`.
- **No floats**, ever.
- **`rpow` is CU-heavy**, so the interest accumulator caps the exponent by forcing a `refresh_market` at least every N slots (`MAX_PRICE_STALENESS_SLOTS = 250`, ~100s) and requests extra compute.

### 1.8 Instruction surface (current)

The deployed program exposes the following (production IDL, `dev_set_price` excluded). Borrow/withdraw price off the cached `Market.spot` (OSM-style), now populated by the permissionless `update_price` crank (which bundles a fresh Pyth post-update + Switchboard quote in the same transaction); `dev_set_price` sets it directly only in dev/test builds. (The full instruction and account reference is in §11.)

| Instruction | Status | Note |
|---|---|---|
| `init_protocol` | [Built] | Creates `ProtocolConfig` + the FUSD mint (legacy SPL, freeze=None, authority=PDA). |
| `init_market` / `init_market_oracle` | [Built] | Per-collateral `Market`, escrow vault, RP, redemption bitmap; onboarding allowlist rejects mints with a live freeze authority. `init_market_oracle` binds the Pyth/Switchboard feeds + Orca/Raydium pools + quote-mint decimals and creates the `DexTwap` ring. |
| `sample_twap` / `update_price` | [Built] | The permissionless oracle cranks: sample a CLMM pool into the `DexTwap` ring; aggregate Pyth + Switchboard + the TWAP into `Market.spot` + `mint_frozen` (§6.4). |
| `set_oracle_program_ids` / `rebind_market_oracle_feeds` | [Built] | Governance: rebind the bounded oracle PROGRAM IDs (Pyth receiver / Switchboard) and a market's feed SOURCES so the ~2026-07-31 Pyth core migration needs no redeploy (§6.4). |
| `refresh_market` | [Built] | Permissionless accrual of the BOLD weighted-debt-sum aggregate; mints `unminted_interest` into the insurance buffer (rewarded crank). Keeps the shared `Market` write to ~once/slot. |
| `open_position`/`close_position`/`deposit`/`withdraw`/`borrow`/`repay` | [Built] | The CDP hot path; writes only `Position` + `Market`. Posts/refunds the per-position SOL reserve bond. |
| `adjust_rate` | [Built] | Moves a position's rate bucket; the BOLD premature-rate-change fee + cooldown (`rate_adjust_cooldown_secs`, governable, default-off) is [Built]. |
| `redeem` | [Built] | Find-first-set the lowest non-empty rate bucket; CR-sort within it; face value minus a flat clamped fee. A candidate closed mid-flight is skipped, not a whole-batch revert. The hard floor. |
| `claim_coll_surplus` | [Built] | Withdraw the collateral the liquidation bonus collar returned (`Position.coll_surplus`); safe in shutdown. |
| `init_reactor_pool`/`open_reactor_deposit`/`provide_to_reactor`/`withdraw_from_reactor`/`claim_reactor_gains` | [Built] | The RP with lazy P/S gain realization. |
| `init_insurance_buffer` / `fund_buffer` | [Built] | The per-market fUSD insurance buffer (tier-3 loss absorber) + permissionless funding. |
| `liquidate` | [Built] | Permissionless **5-tier waterfall** (RP offset → redistribution → insurance buffer → global backstop → un-homed bad debt → shutdown); bonus collar + claimable surplus; SOL-bond + collateral gas-comp make it gas-positive. |
| `init_governance_gate` / `migrate_inbound_authority` / `accept_inbound_authority` / `queue_param_change` / `execute_param_change` / `cancel_param_change` | [Built] | The bounded `GovernanceGate` + FUSD-owned timelock: a **two-step** inbound-authority handoff (propose → the successor signs to accept), then queue a clamped param change and permissionless execute after the delay. Relational config bounds (collar fundability, RP-solvency, MCR ≥ SCR) re-checked at queue **and** execute; an MCR raise arms the liquidation grace window. |
| `init_global_backstop` / `fund_backstop` / `withdraw_backstop_excess` / `queue_global_param_change` / `execute_global_param_change` / `cancel_global_param_change` | [Built] | The bounded **Global Backstop Reserve** (shared second-loss capital): ships inert (params 0/off); permissionless funding; gov-gated above-cap excess withdrawal; clamped global-param changes behind the same FUSD-owned timelock (queue / permissionless execute / cancel). |
| `migrate_gov_authority` / `accept_gov_authority` | [Built] | Two-step propose/accept rotation of the bootstrap/admin `ProtocolConfig.gov_authority` (the key the Phase-3 roadmap migrates to the MetaDAO Squads vault). |
| `guardian_derisk` / `set_guardian` | [Built] | The independent de-risk guardian: a per-market auto-expiring borrow pause + gov-gated guardian rotation/revocation. |
| `shutdown` / `urgent_redeem` | [Built] | Terminal per-market wind-down at SCR breach / oracle failure → unordered 0-fee urgent redemptions. |
| `withdraw_surplus` / `sweep_protocol_collateral` / `settle_bad_debt` | [Built] | The value-recovery trio (recap): move protocol-owned redemption-fee surplus / un-homed collateral, and burn recovered fUSD against `bad_debt`. Gov-gated; never touches position-backing value. |

---

## 2. The CDP Core

The CDP core is the heart of FUSD: three account types and the lifecycle instructions implement an overcollateralized, isolated-per-collateral stablecoin in the Liquity lineage, with **Liquity-v2 / BOLD per-position interest accounting** (each borrower accrues at its own `user_rate_bps` in O(1) off the market's weighted-debt-sum aggregates — §2.2). It is the only part of the protocol that mints and burns FUSD, and the only part that holds borrower collateral. Everything covered here is **[Built]** — implemented in `fusd-core` and exercised by the integration tests — except where explicitly noted.

The design rule is structural neutrality: each safety property is something the program *cannot* do, not something it promises not to do. Collateral lives in a program-owned escrow that only the position's own owner can withdraw from (and only above MCR); FUSD can only be minted by `borrow` under the collateralization rule; and the conservative rounding convention means every fractional unit favors the protocol, never the borrower.

### 2.1 Core accounts

Three PDAs partition the state so distinct users and distinct collaterals never share a write-lock. This is the Solana-specific reason FUSD avoids Maker's single global `Vat`, which would serialize every state change behind one compute lane.

#### `ProtocolConfig` — `[b"config"]` [Built]

The global, read-mostly config. Passed read-only on the hot path so unlimited concurrent operations share it without serializing, and written almost never.

| Field | Type | Purpose |
|---|---|---|
| `gov_authority` | `Pubkey` | The bootstrap/admin authority that runs the `init_*` setup instructions (`init_market`, `init_market_oracle`, `init_reactor_pool`, `init_insurance_buffer`, `init_governance_gate`, `set_guardian`, `dev_set_price`). Rotated via the two-step `migrate_gov_authority`/`accept_gov_authority`. Distinct from `GovernanceGate.inbound_authority`, the migratable param-tuning authority that queues timelocked changes. |
| `guardian` | `Pubkey` | De-risk-only role, independent of futarchy/Squads, so a frozen DAO cannot freeze FUSD's emergency response. [Built] — consumed by `guardian_derisk` (per-market borrow pause); rotatable/revocable by `gov_authority` via `set_guardian`. |
| `deployer` | `Pubkey` | Whoever ran `init_protocol` (informational). |
| `fusd_mint` | `Pubkey` | The FUSD mint (legacy SPL Token, freeze authority `None`, mint authority = PDA). |
| `bump` | `u8` | Canonical bump. |
| `pending_gov_authority` | `Pubkey` | The proposed successor for the two-step `gov_authority` handoff. `Pubkey::default()` = none in flight; the live authority only moves when the pending key itself signs `accept_gov_authority`, so a typo'd / unheld proposal can never strand the admin role. Carved from `_reserved`. |
| `_reserved` | `[u8; 32]` | Forward-compat padding (registry pointer, future flows) so `SPACE` stays stable across early upgrades. The Pyth/Switchboard oracle program IDs are no longer reserved bytes — they are now real fields (`pyth_receiver_program_id`, `pyth_receiver_program_id_alt`, `switchboard_program_id`), bounded-updatable via `set_oracle_program_ids` for the Pyth core migration. There is deliberately **no global emergency flag** — a dead `emergency: bool` was removed pre-launch so "no global kill switch" is grep-verifiable; the only emergency levers are per-market and rule-based (guardian pause-new-debt, permissionless `shutdown`). |

`SPACE` = 8 discriminator + 8 `Pubkey` + `bump` + 32 reserved.

#### `Market` — `[b"market", collateral_mint]` [Built]

The per-collateral isolated market: parameters, accounting accumulators, the cached price, and the escrow vault pointer. A hot account in the per-collateral write lane. Fields cluster into four groups.

**Accounting & params** (this section's focus):

| Field | Type | Purpose |
|---|---|---|
| `collateral_mint` | `Pubkey` | The collateral asset for this market. |
| `collateral_vault` | `Pubkey` | Program-owned escrow token account holding all of this market's collateral. |
| `agg_recorded_debt` | `u128` | Σ recorded (present-value) debt across the market's positions, FUSD-native. Kept exact at every touch. BOLD `aggRecordedDebt`. |
| `agg_weighted_debt_sum` | `u128` | Σ `recorded_debt_i · user_rate_bps_i` (bps scale) — the aggregate that makes per-position interest O(1): pending interest over `dt` = `agg_weighted_debt_sum · dt / (SECONDS_PER_YEAR · 10_000)`. BOLD `aggWeightedDebtSum`. |
| `unminted_interest` | `u128` | Interest folded into `agg_recorded_debt` but not yet **minted**; `refresh_market` mints it as FUSD into the insurance buffer and zeroes it (the lazy mint seam). Supply invariant: `circulating == agg_recorded_debt − unminted_interest + bad_debt`. |
| `last_update_ts` | `i64` | Unix timestamp of the last aggregate-interest accrual. |
| `spot` | `u128` | Cached collateral price: RAY-scaled FUSD-native per 1 native collateral unit (OSM-style cache). `0` until set. |
| `spot_updated_slot` | `u64` | Slot the cached price was written, for staleness checks. |
| `mcr_bps` | `u16` | Minimum collateral ratio, bps (e.g. `12_000` = 120%). |
| `debt_ceiling` | `u64` | Debt ceiling in FUSD-native units. |
| `collateral_decimals` | `u8` | Cached from the mint at `init_market`. |
| `bump` / `vault_bump` | `u8` | Canonical bumps for the market PDA and its vault. |

**Liquidation redistribution** (`l_coll`, `l_art`, `last_coll_redist_error`, `last_art_redist_error`, `total_stakes`, `total_collateral`, `total_stakes_snapshot`, `total_collateral_snapshot`) and **liquidation incentives** (`reserve_lamports`, `liq_gas_comp_bps`) back the liquidation engine — see §4. Note one invariant the CDP core maintains here: `total_collateral` tracks the vault's token balance exactly and stays `>= Σ position.ink`, with floor dust retained as protocol-favoring over-collateralization. **Redemption** fields (`bucket_width_bps`, `redemption_fee_bps`, `surplus_collateral`) back the rate-bucket bitmap — see §5. The CDP core touches `total_collateral` on every `deposit`/`withdraw` and folds in pending redistribution on every position touch via `redist::realize`/`touch`.

#### `Position` (CDP / trove) — `[b"position", collateral_mint, owner]` [Built]

A user's CDP. Only the owner's transactions write it (`has_one = owner`), so all distinct users' operations run in parallel under Sealevel.

| Field | Type | Purpose |
|---|---|---|
| `owner` | `Pubkey` | The borrower; the only signer that may adjust this position. |
| `collateral_mint` | `Pubkey` | The market this position belongs to. |
| `ink` | `u64` | Locked collateral, native units. |
| `recorded_debt` | `u128` | Present-value debt in FUSD-native units, as of `last_debt_update`. Stored directly (no `art*rate` normalization — the BOLD model). |
| `user_rate_bps` | `u16` | Borrower-chosen interest rate (clamped `[MIN, MAX]_USER_RATE_BPS`). [Built] — drives **both** the redemption rate-bucket placement AND per-position interest accrual (each borrower accrues at its own rate in O(1)). |
| `last_debt_update` | `i64` | Unix timestamp of this position's last interest realization (the per-position accrual clock). |
| `bump` | `u8` | Canonical bump. |
| `stake`, `redist_l_coll_snapshot`, `redist_l_art_snapshot` | `u128` | Liquidation-redistribution stake + per-position reward snapshots (see §4). |
| `reserve_lamports` | `u64` | The SOL liquidation bond actually posted, fixed at open from the market's then-current `reserve_lamports` so a later governance change can't alter it retroactively. |
| `bucket` | `u16` | The redemption rate-bucket this position is counted in — valid iff `recorded_debt > 0`; stored explicitly (not re-derived) so a `bucket_width_bps` change can't mis-target the decrement. The `ZOMBIE_BUCKET` sentinel parks a collateral-exhausted / sub-`min_debt` position out of the ordering. |
| `coll_surplus` | `u64` | Collateral the liquidation bonus collar returned to this owner (native), held in the vault, withdrawn via `claim_coll_surplus`. |
| `last_rate_adjust_ts` | `i64` | Last `adjust_rate` time; drives the BOLD premature-rate-change fee/cooldown. |
| `_reserved` | `[u8; 32]` | Forward-compat padding (widened 6→32 pre-launch for additive-upgrade headroom). |

### 2.2 Per-position interest accounting (Liquity-v2 / BOLD weighted-debt-sum) [Built]

Each borrower picks its **own** interest rate (`Position.user_rate_bps`), so FUSD uses the BOLD weighted-debt-sum model rather than Maker's single-`rate` accumulator. A `Position` stores its **`recorded_debt`** (present-value debt, FUSD-native, as of `last_debt_update`) directly — no `art * rate` normalization. The market still accrues interest in **O(1)** off two aggregates (BOLD `ActivePool`).

**The two aggregates.**
- `agg_recorded_debt` = Σ `recorded_debt_i` — kept exact at every touch.
- `agg_weighted_debt_sum` = Σ `recorded_debt_i · user_rate_bps_i` (bps scale; fits `u128` with vast headroom).

```
on accrue (every position touch + refresh_market):              # O(1), linear between touches
    dt      = now − last_update_ts
    pending = ceil(agg_weighted_debt_sum · dt / (SECONDS_PER_YEAR · 10_000))   # rounds UP
    agg_recorded_debt += pending ;  unminted_interest += pending ;  last_update_ts = now
on a position's realize-on-touch:
    accrued = floor(recorded_debt · user_rate_bps · (now − last_debt_update) / (…))  # rounds DOWN
    recorded_debt += accrued (+ pending redistribution) ;  last_debt_update = now
    agg_weighted_debt_sum += new_weighted − old_weighted              # add-then-subtract delta
```

Interest **compounds across touches** (each realize capitalizes it into `recorded_debt`) but is **linear within** an inter-touch interval. The **aggregate-ceil / per-position-floor** rounding guarantees the minted aggregate interest is never short of Σ per-position interest (solvency-by-rounding; Kani-proven in `fusd-math::interest`, and re-fuzzed wide in the B8 proptest suite).

**The mint seam.** Accrued interest is **minted into existence** and routed to the per-market **insurance buffer**, matched one-for-one by the `agg_recorded_debt` growth that booked it. Minting is **lazy**: every touch only accumulates `Market.unminted_interest`; the permissionless `refresh_market` mints it into the buffer's FUSD vault (off the hot path), paying the cranker a governable `keeper_reward_bps` cut. The **upfront borrowing fee** (`borrow_fee_bps`, BOLD-sweep C7; governable, default-off) rides the SAME seam: a borrow grows the debt by `amount + fee` but mints only `amount` to the borrower, booking the `fee` into `unminted_interest` so it funds the buffer exactly like accrued interest. It is the primary redemption-evasion deterrent and is checked against MCR on the post-fee debt. The exact supply invariant (litesvm-tested):

> `circulating FUSD == agg_recorded_debt − unminted_interest + bad_debt` per market.

**Lazy realization + redistribution.** A position is re-priced only when its owner touches it (`borrow`/`repay`/`withdraw`/`deposit`/`adjust_rate`/`liquidate`/`redeem` all realize first). Tier-2 redistributed debt (`Market.l_art`) is parked **non-interest-bearing** until a recipient folds it into `recorded_debt` and re-weights it at its **own** rate on its next touch. Rates are clamped at `borrow`/`adjust_rate` to `[MIN_USER_RATE_BPS, MAX_USER_RATE_BPS]` = `[50, 2550]` bps (the upper bound pinned to the 256-bucket × 10 bps redemption layout). A premature rate change (within `rate_adjust_cooldown_secs`) costs an upfront fee capitalized into `recorded_debt`.

#### Conservative rounding

The rounding convention is "always against the borrower, never against the protocol", implemented exactly in `fusd-math`:

| Operation | Direction | Where | Rounding |
|---|---|---|---|
| Aggregate interest accrual | debt ↑ | `accrue` (`pending`) | **up** (ceil) |
| Per-position interest realize | debt ↑ | `realize` (`accrued`) | **down** (floor) — so `agg_recorded_debt ≥ Σ recorded_debt_i` (solvency-by-rounding) |
| Borrow → add debt | debt ↑ | `recorded_debt += amount` | exact (FUSD-native, no normalization) |
| Repay → remove debt | debt ↓ | `recorded_debt −= amount` | exact, capped at current debt |
| Collateral value | value ↓ | `collateral_value` = `ray_mul(ink, spot)` | **down** (floor) |
| Max debt at MCR | debt cap ↓ | `max_debt` (`mul_div_floor`) | **down** (floor) |

So the minted aggregate interest is never short of the sum of per-position interest (the aggregate-ceil / per-position-floor rule, Kani-proven), a borrower is never over-credited on repayment, and is never given credit for fractional collateral value. All money math goes through an exact 256-bit (`bnum::U256`) intermediate and returns `None` rather than truncating on overflow — wrapping is never tolerated. `repay` caps the burn at the position's current present debt; a full repay zeroes `recorded_debt` exactly (no dust).

### 2.3 Lifecycle instructions

| Instruction | Caller | Price needed? | Mints/burns? | Effect |
|---|---|---|---|---|
| `init_protocol` | deployer (once) | — | — | Create `ProtocolConfig` + the FUSD mint (freeze=None, mint auth=PDA). |
| `init_market` | `gov_authority` | — | — | Create a collateral's `Market`, escrow vault, and redemption bitmap; validate params. |
| `open_position` | borrower | — | — | Create an empty CDP; post the SOL liquidation bond. |
| `deposit` | borrower | no | — | Move collateral into escrow; increase `ink`; top up the bond. |
| `withdraw` | borrower | only if `recorded_debt > 0` | — | Move collateral out of escrow; must stay ≥ MCR if any debt remains. |
| `borrow` | borrower | **yes** | mint | Mint FUSD up to MCR and the debt ceiling. |
| `repay` | borrower / arbitrageur | no | burn | Burn FUSD, reduce `recorded_debt` (capped at current debt). |
| `close_position` | borrower | — | — | Close an empty CDP; reclaim rent + remaining bond. |
| `refresh_market` | anyone | — | — | Advance the interest accumulator to now. |

**`init_protocol`** creates `ProtocolConfig` and the FUSD mint in one transaction. The mint is created with `mint::authority` = the `[b"mint_authority"]` PDA and **no `freeze::authority`** — omitting it makes freeze authority `None` irreversibly (there is no later instruction to remove it), the core censorship-resistance guarantee. It is legacy SPL Token, not Token-2022, so the mint physically lacks the censorship extensions. FUSD has 6 decimals.

**`init_market`** is gated on `config.gov_authority` and creates the `Market`, the program-owned `collateral_vault` (token authority = the market PDA), and the zero-copy `RedemptionBitmap`. It rejects any collateral mint carrying a freeze authority — and, because every collateral account is typed against **legacy SPL Token** (`Account<token::Mint>` under `Program<Token>`), a Token-2022 mint fails account validation before the handler runs. That is the **complete** onboarding extension gate for the locked legacy-SPL-only stance (the earlier "full TLV-extension scan" was deleted, not deferred — a legacy mint physically cannot carry fee/hook/pausable/delegate extensions; §9). Parameters are sanity-checked: `mcr_bps` in `[MIN, MAX]_MCR_BPS`, the liquidation/redemption knobs within their clamps, **and the relational bounds** (`validate_market_config`: collar fundability `10000 + liq_bonus ≤ mcr`, the RP-solvency product, and `mcr ≥ scr`). The market opens with the BOLD aggregates at 0, `mint_frozen = true`, and `spot = 0`.

**`open_position`** initializes an empty CDP (`ink = 0`, `recorded_debt = 0`, `last_debt_update = now`), records the borrower's `user_rate_bps`, snapshots the market's redistribution accumulators (so the position only earns redistributions that happen after it opens), and posts the SOL liquidation bond fixed at the market's current `reserve_lamports`. Opening is collateral- and price-free.

**`deposit`** transfers collateral into the escrow vault, folds in any pending redistribution (`redist::realize`), increases `ink` and `market.total_collateral` in lockstep, recomputes the position's stake, and tops up the liquidation bond if the position is under-bonded (e.g. a position reused after a liquidation consumed its bond). It needs no price — adding collateral only reduces risk. Finally it reconciles redemption-bucket membership in case the realized redistribution took the position's `art` from 0 to positive.

**`withdraw`** first `accrue`s the market, then realizes pending redistribution. If the position still has debt afterward, it requires a fresh price: `spot > 0` (else `OracleUnavailable`), the cached price no older than `MAX_PRICE_STALENESS_SLOTS` (250 slots, ~100s; else `StalePrice`), and that the position stays at/above MCR after the withdrawal (`cdp::is_healthy`, else `BelowMinCollateralRatio`). When the CCR band is enabled (`ccr_bps > 0`) and the price is fresh, it also reverts `CcrRestricted` if the post-withdrawal market TCR would be below CCR — for every position incl. debt-free, but fail-open on a stale price. Collateral leaves the vault signed by the market PDA. If the position has no debt, no price is needed.

**`borrow`** is the only mint path. It `accrue`s the market and folds redistribution (`redist::touch`), then enforces a live, usable, non-frozen, non-paused market: not `shutdown` (`MarketShutdown` — the terminal breaker), `spot > 0` (`OracleUnavailable`), not `mint_frozen` (`MintFrozen` — the mint-freeze gate `update_price` sets when the aggregate degrades), not guardian-paused (`GuardianPaused` — the emergency brake), and the cache not stale (`StalePrice`). All five close NEW MINTS only — repay/withdraw/liquidation/redemption (or `urgent_redeem` once shut down) ignore them. The borrow simply adds `amount` to the realized `recorded_debt`; it checks the *new* position is healthy at MCR (`is_healthy(ink, new_debt, spot, mcr)`) and the *market* debt ceiling against the new aggregate (`agg_recorded_debt + amount <= debt_ceiling`, else `DebtCeilingExceeded`). When the CCR band is enabled (`ccr_bps > 0`), it reverts `CcrRestricted` if the post-borrow market TCR would be below CCR. When the net-outflow rate limiter is enabled (`rl_cap > 0`), it then consumes `amount` of bucket capacity (else `RateLimitExceeded`) — checked before any state change, so an over-cap borrow mints nothing. Only after all checks pass does it commit `recorded_debt`/`agg_recorded_debt` (re-weighting `agg_weighted_debt_sum`) and `mint_to` the borrower's ATA via `invoke_signed` from the `[b"mint_authority"]` PDA, then reconcile the bucket (the position joins a rate bucket on its first debt).

**`repay`** burns FUSD to reduce debt — the repay-arbitrage peg loop, callable by any FUSD holder against their own position. It realizes interest + redistribution, returns early if debt is zero, then burns `min(amount, recorded_debt)` and subtracts it exactly (a full repay zeroes `recorded_debt`, no dust). It needs no price (repaying only reduces risk) and reconciles the bucket (the position leaves its bucket on full repay).

**`close_position`** requires the CDP be empty (`art == 0` and `ink == 0`, else `PositionNotEmpty`) and closes it with `close = owner`, refunding all lamports — rent plus any remaining SOL bond (the bond is gone if the position was liquidated, still present if voluntarily wound down).

**`refresh_market`** is the permissionless `accrue` entry point: anyone may advance a market's interest accumulator to the current timestamp, keeping the shared write to ~once per slot.

### 2.4 Cached price (`spot`) and the oracle gate

`Market.spot` is an OSM-style *cached* price: RAY-scaled FUSD-native per 1 native collateral unit, written by a separate crank rather than read inline. Every debt-taking or debt-holding operation (`borrow`, and `withdraw` when debt remains) requires `spot > 0` and freshness within `MAX_PRICE_STALENESS_SLOTS`. Because `collateral_value` floors and `max_debt` floors, an unset price (`spot == 0`) yields zero collateral value, so any debt is unhealthy — you cannot borrow without a price.

The real oracle (`fusd-oracle` + Pyth/Switchboard + DEX-TWAP validation, see §6) that populates `spot` is now **[Built]** — the `update_price`/`sample_twap` cranks (§6.4). A dev-only **`dev_set_price`** instruction still sets `spot` directly for tests: it is compiled only under the `dev-oracle` Cargo feature (`#![cfg(feature = "dev-oracle")]` on both the handler module and its `#[program]` entry), so it is **excluded from production builds and the IDL** (the `check-no-dev-oracle` release gate enforces this; the integration-tests crate carries the `dev-oracle` dep in isolation). Production borrowing is gated on a real crank having run: markets start `mint_frozen = true` with `spot = 0`, and `borrow` requires `spot > 0`, `!mint_frozen`, and a non-stale `spot_updated_slot`.

---

## 3. The Reactor Pool

The Reactor Pool (RP) is FUSD's first-line liquidation backstop and a structural pillar of the peg. Depositors park FUSD in a per-market pool; when a position falls below MCR, the liquidator burns pool FUSD equal to the offset debt and routes the seized collateral to depositors pro-rata — at a discount, since they absorb $1 of debt for more than $1 of collateral (the liquidation bonus). This makes the RP the standing buyer of liquidated collateral (peg loop 3) and lets liquidations clear in **O(1)** — two scalar writes per liquidation, no iteration over depositors regardless of pool size.

The pool is **per-market / single-collateral** (PDA `[b"reactor", collateral_mint]`), so a depositor explicitly chooses which collateral risk to underwrite. Liquidation uses the RP as tier 1; any debt beyond pool size spills to tier-2 redistribution (covered in §4). All of this — the math crate, the on-chain accounts, and all five instructions — is **[Built]** and tested on-chain.

### 3.1 The product-sum (P/S) algorithm

The accounting is Liquity's product-sum offset, ported to Solana's bounded storage in `fusd-math`'s `reactor_pool` module. The problem it solves: a single liquidation must (a) shrink every depositor's balance by the same proportion (their share of the burned debt) and (b) credit every depositor a share of the seized collateral — without touching each depositor's account. Two running aggregates make this O(1):

- **`P` (running product, `u128`, 1e18-scaled).** Starts at `DECIMAL_PRECISION` (1e18). Each offset multiplies `P` by a `product_factor = (1 − lossPerUnit)`, where `lossPerUnit` is the debt burned per unit of deposit. A depositor's *compounded deposit* is then `initialDeposit · P_now / P_snapshot` — the proportional shrinkage falls out of the ratio of products, computed only when that depositor next interacts.
- **The `S` grid (cumulative collateral-gain-per-unit-staked).** Each offset accumulates a marginal `collGainPerUnit · P` into the current grid cell. A depositor's accrued collateral gain is `initialDeposit · (S_now − S_snapshot) / P_snapshot / 1e18`.

`P` is monotonically decreasing within an epoch, so it would eventually underflow to zero and destroy precision. Two mechanisms prevent this:

- **Scale.** When multiplying would drop `P` below `SCALE_FACTOR` (1e9), `update_product` rescales `P` up by 1e9 and increments `scale`. The arithmetic runs in 256 bits (`bnum::U256`) so the `P · factor` intermediate (up to ~1e36) and the rescale never overflow. Crossing a scale boundary means a depositor's compounded value picks up a `/ SCALE_FACTOR` correction; the gain math reads both the snapshot's scale cell and the next one (the "first portion" / "second portion ÷ 1e9" split) to span a single-scale crossing.
- **Epoch.** When an offset would empty the pool exactly (`debt_to_offset == total_deposits`), every compounded deposit goes to zero. Rather than divide by zero, `offset` resets `P` to `DECIMAL_PRECISION`, sets `scale = 0`, and increments `epoch`. Any depositor whose snapshot predates the current epoch has a compounded deposit of exactly 0 (`compounded_deposit` returns 0 when `snap.epoch < st.epoch`) but can still claim collateral gains accrued up to the drain.

This yields the **epoch → scale → sum grid**: collateral sums are indexed by `(epoch, scale)`. Within a `(epoch, scale)` pair the cumulative `S` only grows; rollover starts a fresh, zero-valued cell.

#### Error-feedback (no drift)

Both per-unit quantities are floor divisions, so each offset leaves a residual. Liquity's fix — carried here verbatim — feeds the residual back into the next offset:

- `coll_gain_per_unit = (coll_to_add · 1e18 + last_coll_error) / total`; `last_coll_error` is updated to the new remainder.
- `loss_numerator = debt_to_offset · 1e18 − last_loss_error`; `last_loss_error` is updated likewise.

Without feedback, a stream of small liquidations would systematically under-distribute collateral (the Kudelski "precision loss" finding). The `error_feedback_no_drift_over_many_offsets` test runs 1000 tiny offsets and asserts the sole depositor still recovers ~all collateral (≥2998 of 3000), confirming no systematic drift.

#### The solvency margin (+1)

The loss-per-unit is deliberately rounded *up*:

```rust
let loss_per_unit = loss_numerator / total + 1;   // +1: never under-count the loss
product_factor = DECIMAL_PRECISION - loss_per_unit;
```

The `+1` guarantees the pool never *over*-counts what depositors keep: each compounded deposit rounds *down*, so the sum of compounded deposits is always `≤ total_deposits`, with the dust difference (≤ ~1–2 units across the pool) retained in the pool as a protocol-favoring buffer. This is the RP analogue of the protocol's general "round in the system's favor" invariant. The `partial_offset_one_depositor` and `two_depositors_share_pro_rata` tests assert exactly this: `compounded ≤ total_deposits` and the deficit is dust.

#### Realize-on-interaction

Depositor state is never touched by a liquidation — only `P`, `scale`, `epoch`, and one `S` cell move. A depositor's gains and shrinkage are *realized lazily* whenever they next interact (`provide_to_reactor`, `withdraw_from_reactor`, `claim_reactor_gains`). On each such call, `reactor::realize` folds the accrued collateral gain into `pending_collateral_gain`, recomputes the compounded FUSD deposit, and `reactor::set_snapshot` rolls the depositor's `{p, s, scale, epoch}` snapshot forward to the pool's current point.

### 3.2 Bounded storage and the no-wraparound guarantee

On Solana the grid is a fixed zero-copy array, not an unbounded map. `EpochToScaleToSum` is `[u128; REACTOR_GRID_LEN]` with `REACTOR_GRID_LEN = REACTOR_MAX_EPOCHS · REACTOR_MAX_SCALES = 32 · 16 = 512` cells (8 KiB), addressed by direct indexing `epoch · REACTOR_MAX_SCALES + scale`. The recommended dimensions in `fusd-math` (`MAX_EPOCHS = 128`, `MAX_SCALES = 64`) are larger; the on-chain account pins the tighter 32×16, and the math is dimension-agnostic (the stride is passed in).

The critical design choice: **cells are written once-monotonically and never wrap.** If an offset would push `scale` or `epoch` past the grid, `offset` returns `ScaleOverflow` / `EpochOverflow` rather than reusing a cell — which would silently corrupt a depositor's still-computable gain. Reaching the edge is astronomically unlikely (each scale bump needs a ~1e9× product collapse in a single liquidation; each epoch needs a full pool drain) and is a known migration trigger, not a loss event. On-chain these map to `FusdError::ReactorGridExhausted`.

### 3.3 Accounts

| Account | PDA seeds | Layout | Role |
|---|---|---|---|
| `ReactorPool` | `[b"reactor", collateral_mint]` | `#[account]` | Scalar pool state: `p`, `epoch`, `scale`, `total_deposits`, `last_coll_error`, `last_loss_error`, vault pointers (`fusd_vault`, `coll_vault`), `epoch_to_scale_to_sum`. Mirrors `PoolState`. |
| `EpochToScaleToSum` | `[b"ess", collateral_mint]` | `#[account(zero_copy)] #[repr(C)]` | The `[u128; 512]` `S` grid. Zero-copy (loaded via `AccountLoader`) so the 8 KiB array never hits the stack. |
| `ReactorDeposit` | `[b"reactor_dep", collateral_mint, owner]` | `#[account]` | Per-depositor stake: `deposited_fusd` (Liquity's `initialDeposit`), snapshot `{snapshot_p, snapshot_s, snapshot_scale, snapshot_epoch}`, and realized-but-unclaimed `pending_collateral_gain` (native collateral units). |

Two SPL token vaults are owned by the `ReactorPool` PDA: `reactor_fusd_vault` (`[b"reactor_fusd", …]`, holds deposits, burned during offset) and `reactor_coll_vault` (`[b"reactor_coll", …]`, holds seized collateral awaiting claims). The `reactor.rs` glue module bridges these accounts to the pure `fusd-math` types: `pool_state` / `write_back` marshal `ReactorPool ↔ PoolState`, and `realize` / `set_snapshot` drive the snapshot lifecycle.

### 3.4 Instructions

| Instruction | Auth | Effect |
|---|---|---|
| `init_reactor_pool` | `gov_authority` | Creates the `ReactorPool`, the zero-copy grid (zero-initialized via `load_init`), and both vaults for a market. `p = DECIMAL_PRECISION`, `epoch = scale = 0`. One per market. |
| `open_reactor_deposit` | depositor | Creates an empty `ReactorDeposit` (`deposited_fusd = 0`) snapshotted at the pool's current point. With zero deposit the gain math is a no-op until the first `provide_to_reactor`. |
| `provide_to_reactor(amount)` | depositor | Realizes accrued gain, re-snapshots, transfers `amount` FUSD into `reactor_fusd_vault`, and sets `deposited_fusd = compounded + amount`; bumps `total_deposits`. |
| `withdraw_from_reactor(amount)` | depositor | Realizes gain, re-snapshots, then transfers `min(amount, compounded)` FUSD out (signed by the RP PDA) and records the remaining compounded balance; decrements `total_deposits`. The cap means a request larger than the (post-liquidation) compounded deposit cleanly withdraws everything. |
| `claim_reactor_gains` | depositor | Realizes the latest gain, re-snapshots and re-compounds `deposited_fusd` (no fUSD moves — only collateral is paid), then pays the full `pending_collateral_gain` from `reactor_coll_vault` and zeroes it. |

The matching liquidation call sits in `liquidate.rs`: it computes the RP-coverable share `offset_present = min(debt, total_deposits)` and the pro-rata collateral `coll_sp`, calls `rpm::offset(&mut ps, &mut grid.data, REACTOR_MAX_SCALES, offset_present, coll_sp)`, then burns `offset_present` FUSD from `reactor_fusd_vault` and moves `coll_sp` collateral from the market escrow into `reactor_coll_vault`. Any uncovered remainder (`debt − offset_present`) falls through to redistribution. `offset` enforces its own caller invariant (`debt_to_offset ≤ total_deposits`, else `DebtExceedsDeposits`), and an empty pool returns `NoDeposits` — both surfaced on-chain as `ReactorPoolTooSmall`.

### 3.5 Test coverage

The `reactor_pool` module ships with unit tests covering the load-bearing paths: fresh-pool init, single- and two-depositor pro-rata offsets, full-pool drain (epoch rollover + deposit wipe + gain still claimable), scale bump on near-total offset, the 1000-round no-drift error-feedback test, and rejection of over-sized / empty-pool offsets. The scale/epoch rollover is the classic precision bug, so it is hard property/fuzz tested: the **B8 proptest suite [Built]** adds a stateful random-offset-sequence model crossing scale/epoch boundaries plus a depositor-snapshot round-trip carried across scale bumps and the epoch roll — alongside the Kani harnesses (the full `fusd-math` layer is 113 host tests + 25 Kani harnesses; §11.3, `PROOF_STRENGTH.md`).

---

## 4. Liquidation & Stability Mechanics

FUSD liquidations are **permissionless, no-auction, and O(1)**. Anyone may liquidate any position that falls strictly below its market's Minimum Collateral Ratio (MCR), priced against a fresh oracle. There are no keepers, no bidding, and no per-depositor iteration — the design extends Liquity's offset/redistribute model into a **five-tier absorption waterfall** (RP offset → redistribution → insurance buffer → global backstop reserve → un-homed bad debt + shutdown; §4.5), deliberately rejecting the auction mechanism that produced Maker's Black Thursday failure (1,462 zero-bid wins, 5.67M DAI of bad debt under congestion + oracle lag). The whole flow lives in one tight instruction, `liquidate` (in `liquidate.rs`), backed by independently fuzzed math crates: `fusd-math::reactor_pool` (the `P`/`S` product-sum), `fusd-math::redistribution` (the stake-based reward-per-unit accumulators), and `fusd-math::recovery` (the waterfall conservation `reactor + redist + buffer + global + unhomed == debt`, Kani-proven).

### 4.1 Eligibility — the static MCR rule [Built]

A position is liquidatable iff `cdp::is_healthy(ink, recorded_debt, debt_spot, mcr_bps)` returns false — i.e. `recorded_debt > collateral_value(ink, debt_spot) / MCR`, priced against the HIGH (debt) price `Market.debt_spot` (= price + k·σ), not the LOW `spot`. This is a **static** rule: eligibility depends only on the position's own health versus its market's `mcr_bps`, never on system-wide state. There is no condition under which a solvent position becomes liquidatable (see *No Recovery Mode* below).

`liquidate` gates on a **fresh price** before anything else:

| Guard | Check | Error |
|---|---|---|
| Oracle present | `spot > 0` | `OracleUnavailable` |
| Price fresh | `slot − spot_updated_slot ≤ MAX_PRICE_STALENESS_SLOTS` (250 slots, ~100s) | `StalePrice` |
| Has debt | `present_debt > 0` | `PositionHealthy` |
| Below MCR | `!is_healthy(...)` | `PositionHealthy` |

Critically, the handler first calls `accrue` (advancing the `rate` accumulator so interest is current) and then `accrual::realize` to fold any **pending redistributed debt** into the victim's recorded debt before evaluating health — a position can be pushed under MCR purely by debt redistributed onto it from an earlier liquidation, and the eligibility check must see that realized debt.

### 4.2 Tier 1 — Reactor Pool offset [Built]

The Reactor Pool (RP) is the first-loss buyer of seized collateral. `liquidate` computes the RP's share as `offset_present = split.reactor = min(recorded_debt, reactor_pool.total_deposits)` (from `recovery::absorb`) and the remainder `redistribute_present = recorded_debt − offset_present`. For the offset portion it calls `reactor_pool::offset`, which in **O(1)** (two scalars updated, no depositor loop):

- Burns `offset_present` FUSD from the RP's `fusd_vault` (signed by the RP PDA).
- Moves the RP's proportional slice of seized collateral (`coll_sp`) from the market collateral escrow to the RP's `coll_vault` (signed by the market PDA). Depositors claim it later via `claim_reactor_gains`.
- Decrements `agg_recorded_debt` by the extinguished native present-value debt `offset_present` (recorded debt is fUSD-native, so the RP's share is `split.reactor` directly — no `art*rate` conversion).

The product-sum accounting (`fusd-math::reactor_pool`, mirroring Liquity + Hubble/USDH) tracks a running product `P` (1e18-scaled, starts at `DECIMAL_PRECISION`) multiplied by `(1 − lossPerUnit)` per offset, and a per-`(epoch, scale)` cumulative collateral-gain sum `S` stored in the zero-copy `EpochToScaleToSum` grid (the full algorithm is detailed in §3). `P` rescales by `SCALE_FACTOR` (1e9) and bumps `scale` when it would underflow; a full-pool drain (`debt == total_deposits`) zeroes everyone's compounded deposit and rolls `epoch`. Each depositor holds an `{p, s, scale, epoch}` snapshot; gains realize on every interaction and the snapshot rolls forward. Floor residuals are carried in `last_coll_error`/`last_loss_error` so repeated small offsets don't drift (the Kudelski precision-loss remediation), and the loss-per-unit `+1` rounding keeps the pool's compounded deposits `≤ total_deposits` — the dust stays as a solvency buffer. Grid exhaustion (`scale`/`epoch` past the bounded `REACTOR_MAX_SCALES`/`REACTOR_MAX_EPOCHS` = 16×32) **reverts** (`ReactorGridExhausted`) rather than wrapping — an astronomically unlikely migration trigger, never a silent loss of a depositor's unrealized gain.

### 4.3 Tier 2 — Redistribution fallback [Built]

When the RP is too small (`redistribute_present > 0`), the uncovered debt **and** its collateral are spread across the market's *other* positions via the Liquity stake-based reward-per-unit algorithm (`fusd-math::redistribution` + `redist.rs`). This is also **O(1)** — two market-level accumulators bumped per liquidation, applied **lazily** to each position the next time it's touched. Liquidations never stall on pool size.

The model:

- Two cumulative reward-per-unit-staked accumulators, **`l_coll`** and **`l_art`** (1e18-scaled, Liquity `L_ETH`/`L_LUSDDebt`). Each redistribution adds `redistributed · 1e18 / total_stakes`, carrying the floor residual in `last_coll_redist_error`/`last_art_redist_error`. The `* 1e18` numerator is computed in 256 bits (`U256`) so it never overflows; accumulator growth past `u128` **reverts** (`RedistributionAccumulatorOverflow`) rather than wrapping.
- A position holds a `stake` and an `{l_coll, l_art}` snapshot. Its pending gains are `stake · (L_now − L_snapshot) / 1e18`, floored. On `realize` they fold into recorded `ink`/`art` and the snapshot rolls forward (Liquity `applyPendingRewards`).
- **`compute_stake`**: `stake = ink · total_stakes_snapshot / total_collateral_snapshot` (or `stake = ink` before the first liquidation, when the snapshot is zero). The system snapshots are recaptured after every liquidation. This is what keeps **`Σ stake == total_stakes`** exact as redistribution grows positions' collateral, so reward-per-unit never over- or under-distributes.

Mechanically, `liquidate` removes the victim from `total_stakes` and `total_collateral` **before** redistributing (so the split targets only the other positions), requires `total_stakes > 0` (else revert, below), bumps `l_coll`/`l_art` via `redistribution::redistribute`, then adds the redistributed collateral `coll_r` back into `total_collateral`. The redistributed collateral physically stays in the market vault (no token move — it now backs the other positions); `split.redist` stays in `agg_recorded_debt` (now owed by them, **not** extinguished). Finally it recaptures `total_stakes_snapshot`/`total_collateral_snapshot` (Liquity `_updateSystemSnapshots_excludeCollRemainder`).

**Floor-dust direction is protocol-favoring.** Each position realizes a *floored* share, so the residual stays in the aggregates: `total_collateral ≥ Σ position.ink` and `agg_recorded_debt ≥ Σ position.recorded_debt` (extra collateralization / debt counted, never a shortfall). What stays **exact** is `total_collateral == collateral-vault balance` — the dust is real tokens sitting in the vault, simply not yet owned by any position. The lazy-application invariant is enforced by every state-touching instruction (`deposit`/`withdraw`/`borrow`/`repay`/`redeem`): each calls `redist::realize` (or `touch` = realize + `set_stake`) *first*, so every health and ceiling check sees fully-realized debt.

### 4.4 The gas-positive incentive layer [Built]

Two governance-tunable (within compile-time clamps), per-market incentives make fully-permissionless liquidation profitable even when Solana priority fees spike:

| Incentive | Mechanism | Clamp / default |
|---|---|---|
| SOL reserve bond | A fixed lamport bond held on the `Position` account *on top of rent*. Posted at `open_position`, paid to the liquidator on liquidation, refunded (with rent) on `close_position`. | `MAX_RESERVE_LAMPORTS` = 1 SOL; default 0.02 SOL; 0 disables |
| Collateral gas-comp | `liq_gas_comp_bps` of seized `ink`, skimmed to the liquidator's collateral ATA *before* the RP/redistribution split. | `MAX_LIQ_GAS_COMP_BPS` = 1000 (10%); default 50 (0.5%) |

The bond is denominated in **SOL, not FUSD** — liquidation cost is a SOL tx fee, so SOL directly covers it (an explicit correction to the design doc's original FUSD-bond wording). It is **fixed per position at open-time** so a later governance change can't retroactively alter a posted bond. Because a position PDA can be *reused* after a liquidation consumes its bond, `deposit` **re-posts** the bond: if the position is below the market's current `reserve_lamports`, it tops up to that value (never silently lowers an already-higher bond). Without this, a reused position would borrow bond-free. Payout uses checked `sub_lamports`/`add_lamports` on the program-owned account (reverts cleanly rather than panicking; the bond sits above rent so it never under-funds rent-exemption). `close_position` requires an empty position (`art == 0 && ink == 0`, else `PositionNotEmpty`) and refunds rent + any remaining bond via `close = owner`.

The gas-comp is skimmed over `distributable = ink − gas_comp` and the RP/redistribution collateral split is computed over `distributable`, so `gas_comp + coll_sp + coll_r == ink` exactly — `total_collateral == vault` is preserved. The liquidator's ATA is pinned `token::authority = liquidator` precisely so it can't alias a program vault (which would make the skim a no-op self-transfer and desync `total_collateral` from the vault). Both are set at `init_market`, and the **collateral gas-comp (`liq_gas_comp_bps`) is now governance-tunable via the bounded gate** (`queue_param_change` → timelock → permissionless `execute_param_change`, re-clamped). The per-position **SOL reserve bond is deliberately *not* gate-tunable** (it is fixed per position at open-time; changing it would risk retroactivity), so it stays init-time-only. A per-position gas-comp absolute *cap* (beyond the bps clamp) is deferred.

#### The core invariant

`vault_balance == total_collateral + surplus_collateral + total_coll_surplus + protocol_collateral`. The collateral vault token balance equals the sum of position-owned collateral plus unowned dust (counted in `total_collateral`) plus the redemption-fee surplus buffer (`surplus_collateral`) plus the liquidation-collar surplus owed back to liquidated borrowers (`total_coll_surplus`) plus un-homed retained collateral (`protocol_collateral`). Every liquidation path — gas-comp skim, RP seizure, redistribution (no token move) — is constructed to preserve this exactly. The corresponding debt invariant is `agg_recorded_debt ≥ Σ position.recorded_debt`.

### 4.5 The absorption waterfall — insurance buffer + un-homed bad debt [Built]

Liquidation absorbs the debt through a **five-tier waterfall** (`fusd_math::recovery::absorb`, conservation `reactor + redist + buffer + global + unhomed == debt` Kani-proven): **(1)** Reactor Pool offset, **(2)** redistribution to other positions, **(3)** the per-market **insurance buffer** — a pre-funded `InsuranceBuffer` PDA whose FUSD vault is funded by `fund_buffer` + the lazy interest mint (§2.2) — burns what's left, **(3.5)** the shared **global backstop reserve** (bounded second-loss capital, drawn up to a per-market hybrid cap when the local buffer is exhausted), **(4)** any remainder that still has no home is booked as `Market.bad_debt`, its offsetting collateral retained as `Market.protocol_collateral`, and the market is tripped into `shutdown` (`SHUTDOWN_REASON_UNHOMED_BAD_DEBT`). So liquidation never stalls and never silently wraps an accumulator. The un-homed loss is later recovered through the value-recovery trio (`sweep_protocol_collateral` → off-chain sale → `settle_bad_debt` burns recovered FUSD against `bad_debt`). The buffer **funding-source policy** (target size, fee share, exhaustion) is the only [Planned] remainder.

### 4.6 Circuit breakers

#### No Recovery Mode [Built — by deliberate absence]

FUSD has **no Recovery Mode**, global or per-market. Any liquidation-*expanding* RM is rejected as **reflexive**: expanding the liquidatable set during a crash force-liquidates still-solvent positions and deepens the very undercollateralization it claims to cure. The evidence is direct — 61% of Liquity v1's liquidations in the 19-May-2021 crash were RM-driven; Liquity v2/BOLD deleted RM; Hubble removed it 2022-04-19. Market isolation does **not** cure this, because the death spiral is *within* a single market. FUSD's eligibility rule is therefore the static `health < MCR` regardless of system state — which is realized as the simple `is_healthy` gate above, with no TCR computation in the liquidation path (also sidestepping the Liquity TCR-miscomputation bug class, GHSA-xh2p-7p87-fhgh).

#### The BOLD minimal-plus breaker set

In place of RM, the design specifies milder, non-reflexive, per-market, permissionless controls. The terminal **`shutdown` breaker, the net-outflow rate limiter, the CCR borrow-restriction band, and the on-resume liquidation grace window are now wired**; the debt-ceiling auto-line and oracle-divergence gate are still [Planned]:

| Breaker | What it does | Status |
|---|---|---|
| Per-market `shutdown()` | At SCR breach (TCR < `scr_bps`, fresh price) or sustained oracle failure, sets the terminal `Market.shutdown` flag → closes `borrow` + ordered `redeem`, opens `urgent_redeem` (unordered, **0-fee**, face value at the last price). Permissionless + condition-gated only; per-market; **never halts other markets**. | [Built] |
| Net-outflow rate-limiter | A per-market **leaky bucket on net FUSD issuance** in the `Market` PDA: `borrow` consumes, `repay` restores, refills over 24h. Cap = governable `MarketParam::RateLimitCap` (0 = disabled). Liquidation / redemption / urgent_redeem are **hard-exempt** (never touch the bucket). | [Built] (cap default 0 pending calibration) |
| Staleness halt + on-resume grace | A Solana halt forces price staleness → the staleness-pause *is* the outage breaker; an on-resume grace window delays re-enabling liquidations so a stale-then-fresh price can't trigger a liquidation cascade at the resume trough — borrowers who couldn't act during the outage get a window to cure. | [Built] — the staleness *gate* is live in `liquidate` (`StalePrice` on `MAX_PRICE_STALENESS_SLOTS`); the **on-resume grace** is armed by `Market::commit_fresh_spot` (the shared freshness-clock writer) on a stall→resume, and `liquidate` requires `slot >= liq_grace_until` (`LiquidationGracePeriod`, `LIQ_RESUME_GRACE_SLOTS` ≈ 5 min). Gates `liquidate` ONLY — redemption/`urgent_redeem` are never frozen. |
| CCR borrow-restriction band | When market TCR < `ccr_bps`, block only risk-*increasing* ops (`borrow` + `withdraw`, on the post-op TCR); **never** expands the liquidatable set (`liquidate` is untouched); de-risking ops + the floor stay open; **fails open** on no-fresh-price (anti-grief). Cap = governable `MarketParam::Ccr` (0 = disabled). | [Built] (default 0 pending calibration) |
| Oracle-divergence gate | Blocks liquidations while a fresh primary grossly disagrees with a present secondary (`slot >= liq_divergence_until`, `OracleDivergent`); **never** blocks redemptions/`urgent_redeem`/`repay`. Per-market `liq_max_divergence_bps` (0 = disabled). | [Built] (default 0 pending calibration) |

The v1 minimum target is MCR-only liquidation + per-market `shutdown()` + redemption floor + net-outflow limiter + staleness/grace; of these, MCR-only liquidation, the redemption floor, `shutdown`, the net-outflow limiter, the CCR band, **and the staleness halt + on-resume grace window are [Built]** (the limiter cap and CCR both default to 0 = disabled, calibrated + enabled by governance after the fast-crash simulation), with the auto-line following. The urgent-redemption bonus was resolved at **0% (face value)** — so no urgent-redemption amount check or collateral-surplus path is needed; the remaining BOLD-derived constants to pin (base-rate decay, RP penalty ~5%, redistribution penalty ~10–20%) are independent of `shutdown`.

---

## 5. Redemption (the Peg Floor)

Redemption is FUSD's hard price floor. Any holder may burn FUSD and receive **face-value** collateral from a CDP at the cached oracle `spot` — $1 of debt cancelled per $1 of collateral handed over, minus a flat fee. When FUSD trades below `(1 − fee)·$1` on the secondary market, this is a riskless arbitrage: buy FUSD cheap, redeem it for a dollar of collateral, repeat until the discount closes. The mechanism is trustless and permissionless — no redemption gate, no keeper whitelist, no oracle-conditioned pause — which is what lets the floor hold even when minting is frozen. **[Built]** as the `redeem` instruction (`programs/fusd-core/src/instructions/redeem.rs`).

The hard problem redemption solves is *targeting*: which CDP pays? FUSD borrowers set their own interest rate (Liquity v2 / BOLD model), and redemptions must hit the **lowest-rate debt first** — "debt-in-front." A borrower who pays a higher rate is buying redemption protection; a borrower who underpays should be redeemed first. The party being protected (the high-rate borrower) is never the transacting party, so no per-fill local check can defend them, and the sound enforcement of this ordering is the novel part of the design.

### 5.1 Why a bitmap, and why the obvious alternative is unsound

The tempting design is an off-chain sorted list: an indexer submits candidate positions in rate order, and the program verifies the *submitted list is internally monotone*. That proof is **unsound**. Monotonicity of a submitted list proves the list is sorted; it cannot prove that no lower-rate position was *omitted*. A redeemer colluding with (or simply ignorant of) a low-rate position can submit a monotone list that skips it, inverting the entire risk model — the under-payer escapes, the over-payer is hit. There is no on-chain witness for "nothing lower exists" in a list-of-submitted-things scheme.

The sound structure is an **on-chain rate-bucket bitmap**. Quantize each position's borrower rate into a fixed number of buckets and maintain, per market, a bitmap whose bit `k` is set iff bucket `k` currently holds at least one position with debt. `redeem` then proves it starts at the global minimum non-empty bucket by **find-first-set-bit** over the bitmap words — a positive on-chain witness that no lower non-empty bucket exists. It is structurally incapable of skipping a lower bucket. This mirrors Uniswap v3's `TickBitmap` and Liquity v1's NICR-keyed list, but keyed on borrower rate, which (to our knowledge) no deployed Solana protocol does — Hubble/Kamino order by collateral ratio, CLOBs use global sorted trees, Fluid's bitmap is CR-keyed for liquidation.

The bucket key is **price-independent and oracle-independent**: a position's bucket derives only from its own `user_rate_bps`, so it changes only when the borrower calls `adjust_rate`, never on an oracle move. This is the load-bearing property. It means the one shared, write-contended account — the `Market` / bitmap — churns minimally (a bit flips only on a bucket's empty↔non-empty transition, not on every price tick), and it means **redemption ordering survives oracle disagreement or staleness**: redemptions stay alive and correctly targeted even when new mints are frozen.

### 5.2 The bucket math — `fusd-math::rate_bucket` [Built]

The pure bit math lives in `crates/fusd-math/src/rate_bucket.rs`, operating over a caller-owned `&[u64]` word array (the on-chain account owns the storage). Notable functions:

| Function | Role |
|---|---|
| `bucket_of(rate_bps, width_bps, num_buckets)` | Quantize a rate into `[0, num_buckets)`; rates at/above the top clamp into the last bucket |
| `set` / `clear` / `is_set` | Flip / test bucket `k`'s bit (`words[k>>6]` ± `1<<(k&63)`) |
| `first_set` | Lowest non-empty bucket, or `None` — the find-first-set `redeem` starts from |
| `first_set_from` | Lowest non-empty bucket `>= from` (masks sub-`from` bits in the start word) |
| `cmp_collateral_ratio(ink_a, art_a, ink_b, art_b)` | Order two positions by CR ascending via a `U256` cross-multiply (`ink_a·art_b` vs `ink_b·art_a`) — no division, `spot`/`rate` cancel as common factors |

Defaults (from `constants.rs`): `NUM_RATE_BUCKETS = 256` → a `[u64; 4]` (`BITMAP_WORDS = 4`) bitmap; `DEFAULT_BUCKET_WIDTH_BPS = 10` (0.10%), so 256 buckets span 0–25.5%. The width is a governance-clamped param, bounded `MIN_BUCKET_WIDTH_BPS = 1` (0.01%) to `MAX_BUCKET_WIDTH_BPS = 100` (1.00%). The module carries 9 unit tests (quantize/clamp, set/clear roundtrip, word boundaries, find-first-set, the CR cross-multiply, and a full 256-bucket strict-ascending drain).

### 5.3 State: `RedemptionBitmap` [Built]

The per-market account (`state/redemption_bitmap.rs`, PDA `[b"redeem_bitmap", collateral_mint]`) is zero-copy:

| Field | Type | Meaning |
|---|---|---|
| `words` | `[u64; 4]` | The bitmap — bit `k` set iff bucket `k` is non-empty |
| `counts` | `[u32; 256]` | Per-bucket member count; the bit flips only on the `0↔1` member transition |
| `zombie_count` | `u64` | Members of the **zombie pen** — debt-bearing positions parked OUT of the buckets (collateral-exhausted `ink == 0`, or sub-`min_debt` dust) so they can never wedge or clog the find-first-set ordering (Liquity `lastZombie` analog) |

`SPACE = 1072` bytes (layout const-asserted). The `counts` array is what makes empty↔non-empty transitions exact: a bit is set only when its count goes `0→1` and cleared only when it returns to `0`. Per-position state lives on `Position`: `recorded_debt` (present-value debt, **used in per-position accrual**), `ink` (collateral), `user_rate_bps` (the borrower-chosen rate — drives both bucketing AND accrual), and `bucket` (the bucket the position is counted in, **valid iff `recorded_debt > 0`**, or the `ZOMBIE_BUCKET` sentinel).

`Position.bucket` is **stored, not re-derived**. This is deliberate: if governance later changes `bucket_width_bps`, a position must decrement the count of the bucket it *actually joined*, not the one its rate would map to under the new width. Storing the bucket makes a width change inert for existing positions — it cannot desync the counts or retroactively reorder anyone's redemption priority (a hard constraint from the adversarial review).

### 5.4 Membership maintenance — `bucket.rs` [Built]

Membership means "has debt." The glue in `programs/fusd-core/src/bucket.rs` threads four operations through every position touch:

| Op | When | Effect |
|---|---|---|
| `join` | debt `0→+` (first borrow, or a realize that revives debt) | compute bucket from current rate, set `position.bucket`, `add_member` |
| `leave` | debt `+→0` (full repay / redeemed-to-zero / liquidation) | `remove_member` on the recorded `position.bucket` |
| `move_bucket` | `adjust_rate` on a debt-bearing position | `remove_member(old)` then `add_member(new)`, only if the bucket actually changes |
| `reconcile` | end of any touch | compares `art_before` vs `art_after`, dispatches join/leave (also catches a `touch` that realized redistributed debt `0→+`) |

This reconciliation is wired into `borrow`, `repay`, `deposit`, `withdraw`, `liquidate`, and `adjust_rate` — every instruction that can change a position's debt calls `bucket::reconcile` (or the explicit join/leave/move) at the end, after all `art` mutations. The maintenance is invisible to callers: the bitmap account is threaded through the harness account builders.

### 5.5 `adjust_rate` [Built]

`adjust_rate` (`instructions/adjust_rate.rs`) is how a borrower changes their rate and thus their redemption priority. It realizes the position, writes the new `user_rate_bps` and re-weights `agg_weighted_debt_sum`, then reconciles bucket membership. The **anti-gaming upfront fee + cooldown** — the mechanism that stops a borrower from front-running a redemption by briefly dropping then restoring their rate — is **[Built]** (the BOLD premature-rate-change fee: a change within `Market.rate_adjust_cooldown_secs`, governable/default-off, costs `cooldown`-seconds of interest at the new rate, capitalized into `recorded_debt`).

### 5.6 `redeem` — the redemption flow [Built]

`redeem(amount)` executes the floor:

1. **Accrue and price.** Reverts `MarketShutdown` if the market is shut down (the wind-down uses `urgent_redeem` instead — §8). Otherwise fold the rate accumulator (`accrue`), then require a fresh oracle: `spot > 0` (`OracleUnavailable`) and `slot − spot_updated_slot <= MAX_PRICE_STALENESS_SLOTS` (`StalePrice`). Redemption pays face value against a *fresh* price — unlike borrow's conservative cache, the floor must track the real market.
2. **Find the lowest bucket.** `rb::first_set(&words)` → the lowest non-empty **normal** bucket, or `NothingToRedeem` if there is nothing to redeem. Redemption *must* start here; it provably cannot target a higher bucket while a lower one is non-empty. Zombie-pen members sit outside this ordering (so a drained stub can't wedge the floor) but stay independently drainable.
3. **Validate candidates.** The redeemer passes candidate `Position` accounts as `remaining_accounts` (capped at `MAX_REDEMPTION_CANDIDATES = 20`, the >64-account DoS guard). For each: it must carry the right `collateral_mint`, have `recorded_debt > 0`, and be in the lowest normal bucket (or the zombie pen); duplicates are rejected (`DuplicateRedemptionTarget`). A candidate **closed mid-flight** (repaid + `close_position` between tx build and execution) is **skipped, not a whole-batch revert** — but a present-but-wrong account (wrong market, or a non-Position) still hard-reverts.
4. **Realize each candidate (mandatory).** `redist::realize` folds any pending tier-2 redistribution into each candidate *before* anything else. This is critical: a candidate carrying pending redistributed debt, if redeemed-to-zero on its *stale recorded* `art`, would resurrect that debt on its next touch — out of its bucket, untargetable, with `agg_art` carrying debt for which no FUSD was burned. The adversarial review flagged the original omission of this step as a **CRITICAL** bug (fixed and regression-tested). Redeem is a position touch like any other and must realize first.
5. **Sort lowest-CR-first among the submitted.** The candidates are sorted by `cmp_collateral_ratio` ascending; the program ignores the submitted order.
6. **Redeem each.** For each candidate until `amount` is exhausted: present `debt = recorded_debt` (post-realize) and `coll_value = ray_mul(ink, spot)`, then `redeem_amt = min(remaining, debt, coll_value)`. The `coll_value` cap means redemption **never creates bad debt** on an underwater position. Collateral removed at face value is `mul_div_floor(redeem_amt, RAY, spot)` (floored against the redeemer), capped at `ink`. The flat fee `fee_coll = coll_total · redemption_fee_bps / 10_000` is retained in `Market.surplus_collateral`; the redeemer receives `coll_total − fee_coll`. `recorded_debt`/`ink`, `agg_recorded_debt`/`total_collateral`, and the stake are updated; a position drained to `ink == 0` with residual debt moves to the zombie pen.
7. **Burn and pay.** Burn exactly the redeemed FUSD from the redeemer, then transfer the net collateral out of the vault (signed by the `Market` PDA). The 4-term vault invariant `vault == total_collateral + surplus_collateral + total_coll_surplus + protocol_collateral` holds exactly across the operation (and is asserted as `==` by the litesvm suite) — while the on-chain handler's last step asserts the load-bearing **sufficiency** bound `vault >= Σ tracked` (a permissionless donation is tolerated; the dangerous under-funded direction hard-reverts) via `reconcile::assert_collateral_vault_sufficiency`.

### 5.7 In-bucket fairness: the disclosed compromise

Within the lowest bucket, members are rate-fungible only to bucket *coarseness* — they all sit within one `width_bps` band. The program enforces **lowest-CR-first among the submitted candidates**, an on-chain tiebreak that exists for an adversarial reason: without it, a redeemer who supplies the in-bucket members could still hand-pick which one to hit. But it is bucket-level, not strict per-position, fairness — a redeemer chooses *which* in-bucket members to submit, and the program only guarantees the right *bucket* and the CR ordering *within the submitted set*. This is disclosed honestly: debt-in-front is bucket-granular, never marketed as full per-position rate priority. Bucket width sets the strength of the guarantee — narrower buckets, finer priority.

### 5.8 Concurrency

The "last member leaves a bucket" flip — two simultaneous redemptions must never disagree on the lowest non-empty bucket — is currently **serialized by the single per-market `Market` write-lock**: every `redeem` / `borrow` / `repay` / `adjust_rate` writes the shared market account, so Sealevel serializes them and no two observers see an inconsistent bitmap. A finer lazy-reconciliation crank is a future option. The bitmap math itself is now fuzzed by the B8 proptest **stateful BTreeSet model** (random set/clear sequences cross-checked after every op); the bitmap's `words ⟺ counts` coupling is now also proven inductively by the Certora #2a invariant (the concurrent-flip itself is the write-lock's job, not Certora's).

### 5.9 Testing

litesvm integration tests across `litesvm_buckets.rs` (count bookkeeping, width-change inertness), `litesvm_redemption.rs` (find-first-set targeting, the realize-before-redeem regression, the bad-debt cap, fee→surplus, the vault invariant), `litesvm_zombie_bucket.rs` (the collateral-exhausted wedge fix + dust pen), and `litesvm_redeem_closed_candidates.rs` (the skip-not-revert guard). Plus the `rate_bucket` unit + B8 proptest properties (`cmp_collateral_ratio` order laws + the bitmap model). Full workspace green; the production IDL exposes `redeem` and `adjust_rate` with no `dev_set_price` leak.

### 5.10 Deferred refinements

One genuinely deferred follow-on remains; the rest of the original deferral list has since shipped (see the rows below):

| Refinement | What it adds |
|---|---|
| `MIN_DEBT` + zombie bucket | An always-drained-first bucket (Liquity `lastZombie` analog) so sub-minimum dust positions can't clog the lowest bucket |
| `adjust_rate` anti-gaming | Upfront fee + cooldown on a rate decrease, to stop gaming the redemption queue |
| `MAX_REDEMPTION_CANDIDATES` **[Built]** | An account-count cap (20) on `remaining_accounts` (the >64-account DoS) |
| Dynamic base-rate | Liquity's decaying base-rate fee replacing today's flat clamped `redemption_fee_bps` |
| Fuzz + Certora **[Built]** | B8 proptest (bitmap `BTreeSet` model + `cmp_collateral_ratio` order laws) plus the Certora #2a bitmap-coupling invariant (`words ⟺ counts`), VERIFIED on the cloud and mutation-checked |

(Shutdown urgent redemption — 0-fee, unordered, last-price — is now **[Built]** as `urgent_redeem`; see §8.)

---

## 6. Oracle System

Oracle failure is FUSD's **#1 systemic risk** — a known Liquity finding (a single feed failure halted redemptions across all branches) is the design's organizing constraint. The system is layered, per-collateral, and fail-safe: when feeds degrade, the protocol freezes only the *risk-increasing* action (new mints) while keeping the peg-defending floor — repay, conservative liquidation, redemption — fully priced and open. Nothing in this subsystem is a discretionary admin pause; every mode transition is a deterministic rule over feed health.

The system splits cleanly into two layers, **both now [Built]**: a **pure, host-tested validation core** (`fusd-oracle` + `fusd_math::oracle_scale`), and the **on-chain integration layer** — the config accounts, the `DexTwap` ring, and the live `sample_twap` + `update_price` cranks that write `Market.spot` + `Market.mint_frozen` (§6.4). The dev-only `dev_set_price` (feature-gated, never in mainnet builds) remains as a test shortcut, but production now reaches `spot` through the real cranks.

### 6.1 Pure validation core (`fusd-oracle`) — [Built]

`fusd-oracle` is a dependency-free crate of pure integer logic, oracle-agnostic by design and exhaustively host-tested. It knows nothing about Pyth or Switchboard account formats; the on-chain feed adapters normalize raw feeds into a common view and hand that view to this core. All inputs must share one scale/exponent — the adapter normalizes before calling.

#### `PriceView` — the oracle-agnostic input

The normalized internal view: `price` (`u128`), `conf` (the σ confidence interval), `expo` (`i32`, so `value = price · 10^expo`), and `publish_ts` (unix-seconds, a `Timestamp` typedef shared with the TWAP ring; may migrate to slots later). Pyth's `publish_time` maps directly; Switchboard's slot-based freshness is converted in the SDK-wiring layer. Two derived checks: `conf_ratio_bps()` (σ/μ in bps, returning `u128::MAX` when price is zero) and `is_stale(now, max_age_secs)`.

#### Asymmetric pricing (`collateral_price` / `debt_price`) — [Built]

Uncertainty always works **against** the borrower. Following Pyth/MarginFi best practice, the core values:

- **collateral** at `price − k·σ` (`collateral_price`) — used for mint/LTV checks, redemption payout, and the CCR/SCR gauges,
- **debt** at `price + k·σ` (`debt_price`) — used for liquidation eligibility and the seize price.

`k_bps` expresses the confidence multiplier in bps of σ (default `21_200` ≈ 2.12σ ≈ 95%). The invariant `collateral_price ≤ debt_price` holds in **every** case — including price/conf extremes and `u128::MAX` — and is asserted unconditionally in a grid-sweep test (`aggregate_invariant_holds_everywhere`). A wider σ automatically widens the conservative spread, which is exactly the degraded behavior wanted: a noisy feed self-penalizes.

`update_price` caches **both** prices on the market — `Market.spot` (the LOW collateral price) and `Market.debt_spot` (the HIGH debt price) — and **liquidation reads `debt_spot`** ([Built]): a position is liquidated only when underwater at the OPTIMISTIC valuation, so a wide confidence band cannot drive a destructive, irreversible liquidation on noise. The asymmetry's principle: pessimism (LOW) protects *extending* and *winding-down* risk; optimism (HIGH) protects *destroying* a position.

#### `aggregate` — cross-oracle validation policy — [Built]

`aggregate(pyth, switchboard, dex_twap, canonical, now, cfg) -> OracleResult` is the heart of the layered design. It folds **Pyth (primary) + Switchboard (secondary) + the self-maintained DEX-TWAP corridor (sanity bound)** into one validated asymmetric price plus an `OracleMode`. Two decisions are made *independently*:

1. **Price selection** (always succeeds): the best available view — Pyth if fresh, else Switchboard if fresh, else the freshest non-zero of the two, falling back to Pyth. A conservative stale price beats *no* price, because staleness has already frozen mints. An `OracleResult` (collateral price, debt price, mode) is **always returned**, even with every feed degraded — this is the peg-floor guarantee in code: the peg-defending floor never loses its price.

2. **Mint permission** (`OracleMode::Ok`) requires *all* of:

| Gate | Condition |
|---|---|
| Pyth fresh | `now − publish_ts ≤ max_age_secs`, non-zero price |
| Pyth confidence | `conf/price ≤ max_conf_bps` |
| Switchboard agree | present, fresh, **same exponent**, within `max_deviation_bps` of Pyth |
| TWAP corridor | present, within `twap_max_divergence_bps` of Pyth |
| Plausibility band (C6) | the MID price is inside `[band_lower_ray, band_upper_ray]` (0 = bound disabled) |

Anything missing, stale, divergent, with mismatched exponents, or **implausible** degrades to `OracleMode::MintFrozen` — never a hard stop. `aggregate` also returns two booleans the crank consumes ([Built]): **`plausible`** — the MID price (pre-`k·σ`) sits inside the coarse 10^k-scale band, a guard against an absurdly-mis-scaled but self-consistent aggregate the divergence checks can't catch (the `update_price` crank WITHHOLDS the spot commit when `!plausible`, letting the cache age into the staleness machinery); and **`liq_divergent`** — a fresh primary grossly disagrees with a *present* secondary beyond `liq_max_divergence_bps` (looser than the mint thresholds), which the crank caches as a liquidation-only pause (`Market.liq_divergence_until`, with a post-convergence grace) — **never** gating redemption/repay. The cross-oracle deviation is measured against the *smaller* value (conservative: the larger denominator would understate the gap), and either side being zero yields `u128::MAX` (a zero TWAP can never agree). The exponent-equality check guards against comparing un-rescaled feeds, where a raw deviation would be garbage.

#### C1 — LST canonical-rate leg — [Built]

For **liquid-staking-token (LST) collateral**, `aggregate` takes a fourth input, `canonical: Option<u128>` — the trustless on-chain valuation `sol_usd · (total_lamports / pool_token_supply)` (RAY USD per whole LST), read from the SPL stake pool by the crank. When present, the **collateral** (mint/LTV) price is capped at `MIN(market, canonical)` *before* the −k·σ haircut, so an upward-manipulated market feed cannot inflate borrowing power past the stake-pool reality — the BOLD-08 upward-manip→over-mint→depeg defense, mirroring BOLD's `MIN(LST-USD, ETH-USD·exchange_rate)`. The **debt** price (liquidation/redemption) is left on the raw market view — the worst case is not forced on redeemers. `OracleConfig.canonical_required` (set for LST markets) makes a healthy canonical *mandatory to mint*: an absent/stale/degenerate stake-pool rate freezes mints (the defense can't be verified) but still serves a conservative price off the market (the peg floor stays alive). A canonical *above* the market is a no-op (the leg only ever lowers collateral). Non-LST markets pass `None` + `false` and are byte-identical to the pre-C1 behavior. The stake-pool rate is read directly from on-chain state (`programs/fusd-core/src/stake_pool.rs`, verified byte offsets), **never** a swap/DEX.

#### Oracle-degradation freeze semantics — [Built]

`OracleMode` has exactly two variants — `Ok` and `MintFrozen` — encoding the central rule:

> Degraded feeds freeze **NEW MINTS ONLY**. Repay, conservative liquidation, and redemption never lose their price.

This is enforced both by the always-returned price and by the redemption design (§5): redemption *ordering* is oracle-independent (the bucket key is the borrower's own `user_rate`); only the *price paid out* depends on the oracle. A Solana halt forces staleness, so the staleness-pause *is* the outage breaker; on recovery an **on-resume grace window** delays re-enabling liquidations **[Built]** — `Market::commit_fresh_spot` arms `liq_grace_until = now + LIQ_RESUME_GRACE_SLOTS` whenever a fresh price recovers from a stall, and `liquidate` requires `slot >= liq_grace_until`.

A second liquidation-only pause covers the **divergence** tail rather than the staleness tail ([Built]): when a fresh primary grossly disagrees with a present secondary, `update_price` arms `Market.liq_divergence_until` (with a post-convergence grace, like the on-resume window), and `liquidate` additionally requires `slot >= liq_divergence_until`. This is the one case a fresh-but-manipulated primary — within its own conf band, so the freeze gate alone wouldn't catch it — could otherwise cascade liquidations the protocol's own secondaries reject. Like every breaker here, it pauses liquidation only; **redemption and repay always clear**.

### 6.2 The DEX-TWAP corridor (`twap`) — [Built]

Solana CLMMs (Orca Whirlpool, Raydium CLMM) expose **no** Uniswap-style on-chain cumulative-price accumulator, so FUSD maintains its own. The TWAP is **never a primary price** — only a divergence corridor that a few-block price pump cannot move. This is the explicit *Mango lesson*: every observation is weighted by the time it was in effect, and the average must span a full window, so a single fresh print cannot dominate.

#### `ObservationRing<N>` — the manipulation-resistant ring

A fixed-capacity, `repr(C)`, heap-free ring of `{price, ts}` observations, oldest overwritten first. Key properties:

- **Refuses, does not extrapolate.** `twap(now, window, cfg)` returns `None` — never a partial or extrapolated average — unless *all* hold: `window > 0` and `now ≥ newest.ts`; at least `min_samples` retained; `now − newest.ts ≤ max_staleness`; the retained samples **span** the window (`oldest.ts ≤ now − window`); and no arithmetic overflows (it refuses rather than wraps). A ring the crank stopped feeding is no bound.
- **Step-function weighting.** Each observation's price holds from its `ts` until the next observation's `ts`; the newest holds until `now`. Coverage is exact, so total weight equals the window; the result **floors** (documented, deterministic, and feeding a *symmetric* divergence check so neither direction favors the protocol).
- **Strictly increasing timestamps.** `push` rejects equal-or-older `ts`, so a crank replaying or racing itself cannot re-weight history.
- **The Mango bound, tested.** `fresh_spike_cannot_dominate` proves an hour of honest 1,000-prints followed by a 100× spike 5 seconds before `now` moves the hour-TWAP by < 14% — its weight is only 5/3600 of the window.

`TwapConfig` (`min_samples`, `max_staleness`) lives in per-collateral params, not in the ring account itself, so it stays futarchy-tunable within clamps.

#### Layout discipline for zero-copy

The ring stores **parallel** `prices`/`ts` arrays (not `[Observation; N]`) because `{u128, i64}` carries 8 bytes of tail padding wherever `u128` is 16-aligned, and padding is incompatible with `bytemuck::Pod`. The `pod` feature unsafely implements `Pod`/`Zeroable` on the basis that the layout is padding-free **when `N` is even** (total `24·N + 16` is then a multiple of the 16-byte `u128` alignment). `new()` asserts `N` is even (and non-zero) at compile time, monomorphized per `N`. The even-`N` check deliberately avoids `N.is_multiple_of(2)` (stabilized only in Rust 1.87) so the crate builds under the SBF toolchain's cargo 1.84.

### 6.3 On-chain integration layer (`fusd-core`)

#### `MarketOracle` — feed bindings + clamped thresholds — [Built]

A per-market config account (PDA `[b"oracle", collateral_mint]`), read-only on the hot path. It binds the feeds and stores the validation thresholds that mirror `fusd_oracle::OracleConfig` and `TwapConfig`:

| Field group | Fields |
|---|---|
| Feed bindings | `pyth_feed_id` (32-byte feed **id**, not an address — bound via `get_price_unchecked`, recency deferred to `aggregate`), `switchboard_feed`, `orca_pool`, `raydium_pool` (`Pubkey::default()` = not configured) |
| Aggregate thresholds | `max_conf_bps` (u16), `max_deviation_bps` (u16), `twap_max_divergence_bps` (u16), `max_age_secs` (i64), `k_bps` (u16) |
| TWAP guards | `twap_window_secs`, `twap_min_samples`, `twap_max_staleness_secs` |

Thresholds are stored as the smallest sufficient int (bps fit `u16`; the clamps guarantee it). `MarketOracle` also binds **`lst_stake_pool`** — the SPL stake-pool account for the C1 canonical-rate leg (`Pubkey::default()` = a non-LST market, leg off); init requires a 9-decimal collateral mint when it is set. The remaining `_reserved` is 30 bytes (was 62; `lst_stake_pool` carved 32) for forward-compat.

#### `DexTwap` — zero-copy ring account — [Built]

A per-market zero-copy account (PDA `[b"twap", collateral_mint]`) that **mirrors** `ObservationRing<TWAP_RING_CAPACITY>` field-for-field: `prices: [u128; 64]`, `ts: [i64; 64]`, `next: u64`, `count: u64` (`TWAP_RING_CAPACITY = 64`). Embedding the const-generic ring directly would force an `anchor-lang` `IdlBuild` dependency into the pure crate, so instead `ring()`/`ring_mut()` reinterpret the account as the tested ring via a **`bytemuck` Pod cast**. Soundness rests on two guards that must never be removed:

- a **compile-time size assert** — `size_of::<DexTwap>() == size_of::<ObservationRing<64>>()` (plus a re-asserted even-`N`), and
- a **round-trip behavior test** (`mirror_layout_round_trips`) proving pushes through `ring_mut()` land in the mirror fields at the right offsets, the TWAP math reads them back, and non-monotonic rejection still fires through the cast.

`SPACE` is `8 + size_of::<DexTwap>()` (1560 bytes). All-zero is the valid empty ring.

#### `init_market_oracle` — [Built]

Governance-gated (`gov_authority`) instruction that creates both `MarketOracle` and `DexTwap` for an existing market. It validates feed bindings (a zero feed id or default pubkey can never verify; **at least one** DEX pool is required because the TWAP corridor is load-bearing for mint-mode), enforces the compile-time clamps on every threshold, and zero-initializes the ring via `load_init()`.

Clamps (`constants.rs` — futarchy-tunable *within* these hard bounds; defaults pending a backtesting pass):

| Param | Min | Max | Default |
|---|---|---|---|
| `max_conf_bps` | > 0 | 500 (5%) | 200 (2%) |
| `max_deviation_bps` | > 0 | 500 | 100 (1%) |
| `twap_max_divergence_bps` | > 0 | 1_000 | 500 (5%, wider than the feed band — the TWAP lags by construction) |
| `max_age_secs` | > 0 | 300 | 60 |
| `k_bps` | 10_000 | 30_000 | 21_200 (2.12σ) |
| `twap_window_secs` | 300 | 86_400 | 1_800 (30 min) |
| `twap_min_samples` | 3 | 64 (`TWAP_RING_CAPACITY`) | 10 |
| `twap_max_staleness_secs` | > 0 | 3_600 | 300 |

The TWAP window floor of 300s is itself a manipulation defense (long enough that a few-block pump can't move the average).

#### SDK dependencies — [Built]

`fusd-core` pins `pyth-solana-receiver-sdk = 1.2.0` and `switchboard-on-demand = 0.12.1` (versions chosen for anchor 0.32 / solana 2.3 / SBF). Both account types are read via `UncheckedAccount` + **manual parse**, not `Account<T>`: neither SDK ships an `idl-build` feature, so `Account<PriceUpdateV2>` would break `anchor build`'s IDL-generation pass. Pyth is verified by runtime owner == the receiver program, anchor discriminator, `VerificationLevel::Full`, and `feed_id` binding (recency is deferred to `aggregate`, so a stale feed degrades rather than hard-errors). Switchboard is verified by owner + configured-key, then `PullFeedAccountData::parse`; the median `result.value` (i128, 1e18-scaled) normalizes to `usd_ray`. Oracle program IDs live in `ProtocolConfig` as bounded-updatable (so Pyth's ~2026-07-31 core migration won't force a redeploy) — [Built]; see §6.4.

### 6.4 The live cranks — [Built]

Two permissionless instructions complete the system; both normalize **everything to `usd_ray`** (RAY-scaled USD per whole collateral token) before `aggregate`, so the corridor compares like-with-like and the output converts directly to `Market.spot`.

- **`update_price`** — parse the Pyth `PriceUpdateV2` (Full verification + `feed_id` bound + `price > 0`) and an **optional** Switchboard `PullFeedAccountData` into `usd_ray` `PriceView`s, read the `DexTwap` corridor, call `aggregate`, then write `Market.spot = usd_ray_to_spot(collateral_price, …)`, **`Market.debt_spot`** (the HIGH liquidation price), `spot_updated_slot`, and **`Market.mint_frozen`** (`= mode != Ok`). The cache advances **only when a fresh feed backed the price, the conservative valuation is nonzero, AND the price is plausible** — a stale, catastrophically-wide, or absurdly-mis-scaled aggregate leaves it to age out, so the staleness gate pauses the safety paths too (the Solana-halt breaker) and a keeper can't re-post an old signed update to keep liquidation "fresh." It also arms `Market.liq_divergence_until` when the aggregate is `liq_divergent` (the liquidation-only divergence pause). **For an LST market** (`market_oracle.lst_stake_pool` set), the crank additionally takes two optional accounts — the **SOL/USD** Pyth `PriceUpdateV2` (bound to the shared `constants::PYTH_SOL_USD_FEED_ID`) and the **SPL stake pool** — computes the canonical `sol_usd · total_lamports/pool_token_supply` (floored), and passes it to `aggregate` as the C1 collateral cap; a wrong owner/key/feed reverts (`InvalidStakePool`), while an absent/stale/epoch-lagged/degenerate input degrades to `None` (→ mints freeze, never a revert). The Pyth/Switchboard **program IDs the feed-account owner checks use come from `ProtocolConfig`**, not compile-time constants, so the 2026-07-31 Pyth core migration is absorbed by `set_oracle_program_ids` / `rebind_market_oracle_feeds` rather than a redeploy. The migration preserves the `PriceUpdateV2` format + feed IDs (only the owner program changes), so a Pyth update is accepted if owned by **either** of two configured receivers (`pyth_receiver_program_id` or `pyth_receiver_program_id_alt`) — the second seeded at genesis to the upgraded receiver `HDw2E7P8X1SkCyjvoGsfBGAVUutKcj874bXjHrpVYrVL`, making the cutover a zero-downtime, zero-gov-action non-event (the on-chain analog of Pyth's dual-fetch guidance). `dev_set_price` (feature `dev-oracle`, never in mainnet) still exists for tests, setting `spot`/`debt_spot` and clearing `mint_frozen`. A production build **cannot borrow until a real crank runs** (markets start `mint_frozen = true` with `spot = 0`).
- **`sample_twap`** — structurally validate the configured Orca/Raydium pool on **every** call (runtime owner == venue program, discriminator, min-length, `sqrt_price` bounds, and the `{collateral, quote}` mint pair), decode the spot price, scale to `usd_ray` (inverting when the collateral is the pool's quote leg), and `push` it onto the `DexTwap` ring (strictly-increasing ts). A misconfigured pool yields no sample, never a bad price.

The byte-exact pool layouts are hand-parsed in `programs/fusd-core/src/clmm.rs` (no orca/raydium crate deps; offsets verified in `docs/clmm-pool-layouts.md` — decoded SOL/USDC prices from both venues agreed within ~0.3%): Orca `Whirlpool.sqrt_price` (Q64.64) at offset 65 with mints at 101/181; Raydium `PoolState.sqrt_price_x64` at offset 253 with mints at 73/105 and decimals at 233/234 (cross-checked against config). Both prices are `sqrt_price² >> 128` — up to ~256 bits, so squaring goes through U256 (`fusd_math::oracle_scale`), never plain `u128`. `MarketOracle` carries `quote_mint` + `collateral_decimals` + `quote_decimals` (bound from the real mints at `init_market_oracle`) so `sample_twap` needs no Market/Mint account.

Tested by 5 host CLMM-parse unit tests + 29 litesvm crank tests (Orca/Raydium sampling, the invert path, every guard rejection, the happy-path `update_price` → borrow, the freeze cases each still serving `spot`, the anti-replay/no-brick safety regressions, and positive coverage that a frozen market still repays and liquidates). The numeric core is `fusd_math::oracle_scale` (`px_to_ray`, `usd_ray_to_spot`, `sqrt_price_q64_to_ray`), host-tested incl. a verified Whirlpool decode proof.

---

## 7. Governance & Risk Parameters

FUSD's governance design is the load-bearing realization of the project's founding rule: *credible neutrality through code, not policy*. Governance does not custody, freeze, seize, or mint — not because operators promise not to, but because the program offers no instruction that can. What governance *can* do is narrow by construction: write a small set of risk-parameter fields, only within hard-coded bounds, never retroactively. Everything in this section is downstream of that constraint.

A crucial accuracy note up front: **the bounded `GovernanceGate` + FUSD-owned timelock are now [Built]** and govern market-parameter changes through a two-speed flow (`queue_param_change` → delay → permissionless `execute_param_change`), with compile-time clamps enforced at both queue and execute. The decision-#2 localnet PoC that gated this work is **done and passing** (a Squads vault PDA can queue a change through the gate; see §9.3). The de-risk **`guardian_derisk` + `set_guardian`** and the terminal **`shutdown` + `urgent_redeem`** are now [Built] (§7.2, §8). The inbound-authority handoff is **two-step** (propose → the successor signs to accept), and every executed change re-checks the per-field clamps **plus** the relational `validate_market_config` bounds (collar fundability, RP-solvency, `mcr ≥ scr`) and emits a forensic `prev_value`→`value` trail; an MCR raise additionally arms the liquidation grace window. What remains **[Planned]**: the broader `RiskParamRegistry` (the gate today applies the **eleven `MarketParam`s** — `Mcr`, `DebtCeiling`, `RedemptionFee`, `LiqGasComp`, `RateLimitCap`, `Ccr`, `LiqBonus`, `MinDebt`, `RateAdjustCooldown`, `KeeperReward`, `BorrowFee` — directly to the `Market`; oracle thresholds + `scr_bps` are still init-time-only). `config.gov_authority` is the bootstrap/admin authority (creates markets, oracles, SPs, and the gate; itself rotated via the two-step `migrate_gov_authority`/`accept_gov_authority`); the gate's own **migratable `inbound_authority`** is the param-tuning authority that queues timelocked changes.

### 7.1 The futarchy authority model (MetaDAO) [Planned]

FUSD is to be governed by **MetaDAO on-chain futarchy**: a proposal is an encoded SVM instruction; PASS/FAIL conditional markets trade for a window (default 24h minimum; `pass_threshold_bps` default 3%, clamp 10%); the proposal executes only if the manipulation-resistant TWAP of PASS beats FAIL by the threshold. The TWAP is a lagging observation clamped to a max change per ≥60s update — the core manipulation defense. Futarchy *selects parameters within bounds*; it is never a soft multisig over funds.

The integration mechanism was resolved by source investigation (**Branch (a) confirmed** against `metaDAOproject/programs` v0.6.0 + Squads V4): a passing futarchy decision *can* CPI an arbitrary external instruction signed by the DAO's Squads vault PDA. The execution chain on pass is:

```
futarchy finalize_proposal  → Squads proposal_approve (DAO PDA, threshold 1)
                            → vault_transaction_execute
                            → invoke_signed → FUSD setter, signed by the Squads VAULT PDA
```

The Squads `VaultTransaction` (created before the futarchy proposal) carries the FUSD setter instruction; on execute, the **vault PDA** — seeds `[b"multisig", multisig, b"vault", 0]` where `multisig = [b"multisig", b"multisig", dao]` — signs the CPI. The SDK's `squadsProposalCreateTx({instructions})` wraps any instruction array with no `program_id` filter (futarchy already CPIs Meteora DAMM this way), so an FUSD setter is structurally identical to instructions the system already executes.

#### The `GovernanceGate` and the migratable inbound authority [Built]

The FUSD-owned **`GovernanceGate` PDA** (`[b"gov_gate"]`) is the **sole** authority on the market-parameter setter. It stores a *single migratable inbound authority* (`inbound_authority`) that every `queue_param_change` `require_keys_eq!`s against, plus the `timelock_secs` delay and a `queue_nonce`. That one indirection is the whole point: `migrate_inbound_authority` (gated on the *current* inbound authority) repoints it from a **guarded-launch Squads vault** (Phase 1) to the **MetaDAO DAO** (Phase 3) without redeploying or touching any other code path — exactly the handoff the decision-#2 PoC exercised.

The field already exists in built state. `ProtocolConfig.gov_authority` (a `Pubkey`) is documented in-source as "the MetaDAO DAO's Squads vault PDA … the authority [the gate] checks," and today every init-time governance instruction (`init_market`, `init_market_oracle`, `init_reactor_pool`, and the dev-only `dev_set_price`) gates on it via `require_keys_eq!(authority, config.gov_authority)`. When the gate lands, this same field migrates to being the gate's checked inbound authority.

**Honest framing (from the investigation, preserved here):** interposing the gate buys *migratability*, not a smaller end-to-end trust surface. FUSD still transitively trusts futarchy v0.6 + Squads V4 (both upgradeable). The gate validates *authority + clamps + timelock*; it cannot validate decision integrity. Integration safety is verified by pinning a release tag and checking the on-chain program hash (`solana-verify get-program-hash`), not by trusting an audit-commit label. Real upstream audits: MetaDAO — Neodyme (2024), Offside Labs (2025), Accretion (2025); Squads V4 — OtterSec/Certora/Neodyme/Trail-of-Bits.

#### FUSD owns its own timelock [Built]

The Squads config futarchy creates has `threshold = 1` and `time_lock = 0` — so a passing decision is **immediately executable** with no enforced delay. FUSD therefore supplies its own. Each queued change is a **`TimelockedParam`** PDA (`[b"timelock", nonce_le]`, one per op) carrying its `eta = now + timelock_secs`, target market, param, and value. The setters split as **`queue_param_change`** (inbound authority; clamps fail-fast) → (wait `timelock_secs`) → **`execute_param_change`** (permissionless once `now ≥ eta`; re-validates clamps, applies, closes the op), with **`cancel_param_change`** to withdraw a queued op. This is the *slow lane only* — there is **no immediate in-gate fast-path** (a misclassification there is a governance-bypass, the Beanstalk class), so every param change pays the delay, giving users an exit window before it binds. The delay is itself a bounded gov param (`[MIN, MAX]_GOV_TIMELOCK_SECS`, 0–30 days). Surplus/insurance outflow changes are designed to ride the same slow lane with a per-epoch hard cap (still [Planned]).

### 7.2 The de-risk-only guardian [Built]

Emergencies route through a **guardian role that is independent of futarchy/Squads** — so a frozen or buggy MetaDAO DAO (cf. the OtterSec reserves-to-1 freeze class) cannot also freeze FUSD's emergency response. `ProtocolConfig.guardian: Pubkey` is set at `init_protocol` and consumed by **`guardian_derisk` [Built]**, which is **monotonic de-risk only**. The envelope is constitutional:

| The guardian MAY | The guardian may NEVER |
|---|---|
| pause *new* debt (per-market, auto-expiring) | seize collateral |
| (auto-lifts; `pause_secs = 0` lifts early) | freeze user funds |
| — | mint outside the `art*rate` rule |
| — | ratchet the protocol permanently shut |

**Implementation.** `guardian_derisk(pause_secs)` is gated on `config.guardian`, clamps `pause_secs` to `[0, GUARDIAN_MAX_PAUSE_SECS]` (7 days), and sets `Market.guardian_paused_until = now + pause_secs`. `borrow` reverts `GuardianPaused` while `now < guardian_paused_until`; **repay, withdraw, liquidation, and redemption never read the flag** — the guardian can only block *new* borrowing, never touch existing positions, funds, or the peg floor. Because the pause is an absolute deadline, it **auto-lifts** — it cannot hold a market closed; it buys time, not control. The 7-day cap is ≥ the 48h governance timelock (so governance can coordinate a real fix while paused) yet bounds a captured guardian, whose worst case is merely "new borrows stay paused."

**Scope decision.** Of the three originally-sketched levers, only **pause-new-debt** is implemented. Lowering a debt ceiling stays with the bounded `GovernanceGate` (§7.1). "Raise a fee" was **dropped** *as a guardian de-risk lever*: raising the *redemption* fee would impede the peg floor (anti-de-risk), and the upfront borrow fee (`borrow_fee_bps`, C7) is a timelocked governance param, not a fast guardian knob — so there is no fee the guardian can raise to de-risk.

**Rotation/revocation.** `set_guardian(new_guardian)` [Built] lets `gov_authority` rotate or revoke the guardian (immediate, no timelock — fast revocation of a compromised key; `Pubkey::default()` disables it). This does not weaken independence: a *frozen* `gov_authority` cannot rotate the guardian either, so the brake keeps working precisely when governance can't act.

(Note: there is **no global emergency flag**. An inert `ProtocolConfig.emergency` bool existed early on and was deliberately removed pre-launch: a dormant flag is exactly the surface a future kill-switch setter would colonize, and its absence makes the "no global kill switch" claim grep-verifiable. Emergency response is strictly per-market and rule-based: the guardian's auto-expiring borrow pause and the permissionless, condition-gated `shutdown`.)

### 7.3 The bounded-parameter model — what makes it trustless

Three structural properties, not policies, are what reduce futarchy to a bounded tuner:

1. **Compile-time clamps [Built].** Every governable parameter has a hard-coded `[min, max]` (or single-sided cap) in `constants.rs`. A setter rejects any out-of-bounds value with `FusdError::ParamOutOfBounds`. The clamps are *program constants, never themselves governance-settable* — a captured governance still cannot breach them. These checks are live **today** in `init_market` and `init_market_oracle` (see the parameter reference below); the planned setters must re-apply the identical clamps.
2. **Account-level confinement [Built/structural].** Governance instructions may mark **only** parameter accounts (the planned `RiskParamRegistry` / `Market` param fields / `MarketOracle`) writable — never a `Position`/`Market` *balance* field, the mint, or any escrow vault. "No seize/freeze/mint-outside-rules" is enforced by which accounts the instruction is even allowed to declare mutable.
3. **Non-retroactivity [Built where it matters].** Governance may not change *existing positions' terms* retroactively. This is already honored by the parameters that could otherwise be weaponized: the per-position SOL reserve bond is **fixed at open-time** (a later bond change never re-prices a posted bond); `bucket_width_bps` is non-retroactive because each `Position.bucket` is **stored, not re-derived** from current width (a width change cannot silently reorder existing borrowers' redemption priority — the constitutional rate-floor invariant). A raised rate floor is designed to apply only to new/adjusted positions, or to ride the slow timelock with a user exit window.

What futarchy may set: collateral onboarding, debt ceilings (+ auto-line gap/ttl), MCR/CCR/SCR/liq penalty (within clamps), interest-rate min/max bounds, oracle config (program IDs, feed IDs, thresholds within clamps), surplus/insurance allocation, rate-bucket width. What it may **never** do: custody, freeze, seize, mint outside `art*rate`, change existing positions' terms retroactively, or (post-lockdown) upgrade the core to malicious logic.

### 7.4 Parameter reference

All defaults and clamps below are the literal constants in `programs/fusd-core/src/constants.rs`. The fixed-point scales are RAY = 1e27 and WAD = 1e18; FUSD has 6 decimals. "FUSD-native units" = base units (1 FUSD = 1e6).

#### Market parameters [Built]

**Eleven `MarketParam`s are gate-tunable** (timelocked `queue_param_change` → `execute_param_change`, clamps re-checked at both, plus the relational `validate_market_config` bounds): `Mcr`, `DebtCeiling`, `RedemptionFee`, `LiqGasComp`, `RateLimitCap`, `Ccr`, `LiqBonus`, `MinDebt`, `RateAdjustCooldown`, `KeeperReward`, `BorrowFee`. The reserve bond, rate-bucket width, and `scr_bps` stay **init-time-only** (deliberately non-retroactive — the bond is fixed per-position at open and `Position.bucket` is stored).

| Parameter | Field / `MarketParam` | Default const | Clamp | Tunable |
|---|---|---|---|---|
| Min collateral ratio | `mcr_bps` / `Mcr` | — (caller-supplied) | `[MIN_MCR_BPS = 10_000, MAX_MCR_BPS = 30_000]`; + relational `mcr ≥ scr` and `10000 + bonus ≤ mcr` | gate (a raise arms the liquidation grace) |
| Debt ceiling | `debt_ceiling` / `DebtCeiling` | — (caller-supplied) | unclamped (0 pauses new debt) | gate |
| Liquidation reserve bond | `reserve_lamports` | `DEFAULT_RESERVE_LAMPORTS` = 0.02 SOL | `<= MAX_RESERVE_LAMPORTS` = 1 SOL (0 disables) | init-time; fixed per-position at open |
| Liquidator gas-comp | `liq_gas_comp_bps` / `LiqGasComp` | `DEFAULT_LIQ_GAS_COMP_BPS` = 50 (0.5%) | `<= MAX_LIQ_GAS_COMP_BPS` = 1000 (10%); + RP-solvency product | gate |
| Liquidation bonus collar | `liq_bonus_bps` / `LiqBonus` | `DEFAULT_LIQ_BONUS_BPS` = 1000 (10%) | `<= MAX_LIQ_BONUS_BPS` = 2000 (20%); 0 = collar off | gate |
| Rate-bucket width | `bucket_width_bps` | `DEFAULT_BUCKET_WIDTH_BPS` = 10 (0.10%) | `[MIN = 1, MAX = 100]` (0.01–1.00%) | init-time; non-retroactive |
| Redemption fee (flat) | `redemption_fee_bps` / `RedemptionFee` | `DEFAULT_REDEMPTION_FEE_BPS` = 50 (0.50%) | `<= MAX_REDEMPTION_FEE_BPS` = 500 (5%) | gate |
| Upfront borrowing fee (C7) | `borrow_fee_bps` / `BorrowFee` | 0 (disabled) | `<= MAX_BORROW_FEE_BPS` = 500 (5%) | gate |
| Min debt (dust floor) | `min_debt` / `MinDebt` | 0 (disabled) | `<= MAX_MIN_DEBT` | gate |
| Rate-adjust cooldown/fee | `rate_adjust_cooldown_secs` / `RateAdjustCooldown` | 0 (disabled) | `<= MAX_RATE_ADJUST_COOLDOWN_SECS` | gate |
| Keeper reward (refresh crank) | `keeper_reward_bps` / `KeeperReward` | 0 (disabled) | `<= MAX_KEEPER_REWARD_BPS` | gate |

`NUM_RATE_BUCKETS` = 256 and `BITMAP_WORDS` = 4 are **fixed structural constants** (the bitmap is `[u64; 4]`), not governable: width × 256 sets the addressable rate range (default 0–25.5%).

#### Oracle parameters (set at `init_market_oracle`, per-market `MarketOracle`) [Built — init-time-only]

| Parameter | Field | Default const | Clamp (const) | Status |
|---|---|---|---|---|
| Pyth conf cap | `max_conf_bps` | `DEFAULT_ORACLE_CONF_BPS` = 200 (2%) | `(0, MAX_ORACLE_CONF_BPS = 500]` (≤5%) | [Built] init-time |
| Pyth↔Switchboard band | `max_deviation_bps` | `DEFAULT_ORACLE_DEVIATION_BPS` = 100 (1%) | `(0, MAX_ORACLE_DEVIATION_BPS = 500]` | [Built] init-time |
| DEX-TWAP divergence corridor | `twap_max_divergence_bps` | `DEFAULT_TWAP_DIVERGENCE_BPS` = 500 (5%) | `(0, MAX_TWAP_DIVERGENCE_BPS = 1000]` | [Built] init-time |
| Feed staleness cutoff | `max_age_secs` | `DEFAULT_ORACLE_MAX_AGE_SECS` = 60 | `(0, MAX_ORACLE_MAX_AGE_SECS = 300]` | [Built] init-time |
| Asymmetry factor *k* | `k_bps` | `DEFAULT_ORACLE_K_BPS` = 21_200 (2.12σ) | `[MIN_ORACLE_K_BPS = 10_000 (1σ), MAX_ORACLE_K_BPS = 30_000 (3σ)]` | [Built] init-time |
| TWAP window | `twap_window_secs` | `DEFAULT_TWAP_WINDOW_SECS` = 1_800 (30m) | `[MIN = 300 (5m), MAX = 86_400 (24h)]` | [Built] init-time |
| TWAP min samples | `twap_min_samples` | `DEFAULT_TWAP_MIN_SAMPLES` = 10 | `[MIN_TWAP_MIN_SAMPLES = 3, TWAP_RING_CAPACITY = 64]` (a target the 64-slot ring can never reach would freeze mints forever) | [Built] init-time |
| TWAP ring staleness | `twap_max_staleness_secs` | `DEFAULT_TWAP_STALENESS_SECS` = 300 | `(0, MAX_TWAP_STALENESS_SECS = 3_600]` | [Built] init-time |

Related structural constants (not governable): `TWAP_RING_CAPACITY` = 64 (must be even — `ObservationRing` Pod layout); `MAX_PRICE_STALENESS_SLOTS` = 250 (placeholder cached-price staleness). Defaults here are explicitly **conservative placeholders pending the backtesting pass**; the constants comment marks them so.

#### Planned parameters (designed, not yet built)

| Parameter | Intended owner | Status / note |
|---|---|---|
| SCR (shutdown ratio) | per-market `Market.scr_bps` | [Built] — stored (default 110%, init-time-only), drives the `shutdown` TCR-breach trigger. |
| CCR (borrow-restriction band) | `Market.ccr_bps` (`MarketParam`) | [Built] — governable, default 0 = disabled, clamped 100%–300%; blocks risk-increasing ops when TCR < CCR. Likely ~150–160% once calibrated. |
| Interest-rate min/max bounds | per-market | [Planned] — bounds on user-set `user_rate`; no clamp constants yet. The `// TODO(params milestone)` block in `constants.rs` reserves these. |
| Redemption dynamic base-rate + decay | per-market | [Planned] — the Liquity base-rate (with ~35-day decay) replaces today's flat `redemption_fee_bps`. Constants to pin remain open. |
| Liquidation penalty (RP / redistribution) | per-market | [Planned] — pinned constants pending BOLD-audit port (~5% RP, ~10–20% redistribution). |
| Net-outflow rate-limit cap | `Market.rl_cap` (`MarketParam`) | [Built] — leaky bucket on net FUSD issuance; cap default 0 (disabled); liquidation/redemption/urgent_redeem hard-exempt. Window is the 24h constant `RATELIMIT_WINDOW_SECS`. |
| Debt-ceiling auto-line (`gap`/`ttl`) | per-market `RateLimiter` account | [Planned] — Maker DC-IAM auto-line with a fast loosen-path. |
| Oracle program IDs | global `ProtocolConfig` | [Built] — bounded-updatable via `set_oracle_program_ids` so Pyth's ~2026-07-31 core migration needs no core redeploy; `ProtocolConfig` carries `pyth_receiver_program_id` (+ `_alt` for the dual-running window) and `switchboard_program_id`, and `update_price` accepts an update owned by EITHER receiver. |
| Oracle-divergence gate / asymmetric `debt_price` / plausibility band | per-market `MarketOracle` | [Built] — `liquidate` pauses on gross feed divergence (`liq_max_divergence_bps`, 0 = off; never redemption); a stored `Market.debt_spot` (price + kσ) prices liquidation eligibility; an absolute price band (`price_band_lower/upper_ray`, ≥ `MIN_PRICE_BAND_RATIO` wide) is enforced on the spot commit. |
| Surplus/insurance allocation | per-market `Surplus` | [Planned] — rides the slow lane with a per-epoch hard cap. |

---

## 8. Security Model & Invariants

FUSD's security thesis is *credible neutrality through code, not policy*: every "the protocol will never do X" is something the program **physically cannot** do, verifiable by reading the bytecode and the mint's on-chain authorities. There is no admin key with custody, no privileged redemption gate, no pause-of-user-funds, no upgrade path that can retroactively rewrite a borrower's terms. This section enumerates the constitutional invariants (the things enforced *in-program*), the conservation invariants the test suite asserts, the adversarial-review process that hardened each subsystem, and the global-supply auditability plan. It then specifies the FUSD token itself.

### 8.1 Constitutional invariants (enforced in-program)

These are the non-negotiable, structural guarantees. They are not configuration — most are set once at initialization or hard-coded, and several are physically irreversible.

| Invariant | Enforcement | Status |
|---|---|---|
| FUSD mint **freeze authority = None**, set at mint creation; there is no instruction to add one later | `init_protocol` creates the mint via Anchor's `mint::authority` constraint and **omits** `mint::freeze_authority` ⇒ `None`. Irreversible by construction (no later "set freeze authority" path exists). | [Built] |
| FUSD **mint authority = a program PDA** (`[b"mint_authority"]`), never a keypair | `init_protocol` sets `mint::authority = mint_authority` (the PDA). Minting only via `invoke_signed` inside `borrow` (and `refresh_market`'s lazy interest mint); burning inside `repay`/`liquidate`/`redeem`/`urgent_redeem`/`settle_bad_debt`. | [Built] |
| **Legacy SPL Token**, not Token-2022 — physically lacks the censorship extensions | The mint is created under `Program<Token>` (legacy `Tokenkeg…`). Legacy SPL has no `PermanentDelegate`, `TransferHook`, `Pausable`, `ConfidentialTransfer`, `DefaultAccountState`, or `MintCloseAuthority` — they cannot be retrofitted. | [Built] |
| **No** admin freeze / seize / pause-of-user-funds instruction anywhere | No such instruction exists in the program. The only "emergency" surface is the *guardian de-risk* — `guardian_derisk` pauses **only new borrowing** (auto-expiring, per-market), never repay/withdraw/liquidation/redemption or any fund. Per-market `shutdown` (opens `urgent_redeem`, never seizes) is [Built]. | [Built] for the absence + `guardian_derisk` + `shutdown` |
| Liquidation and redemption are **permissionless** (no keeper whitelist) | `liquidate` and `redeem` take `anyone` as caller; any position below MCR is a locally-checkable liquidation target, and `redeem` is callable by any signer with FUSD. No allowlist account is consulted. | [Built] |
| Governance is **bounded, non-retroactive, and cannot mint/move/freeze/seize** | Designed: setters reject values outside compile-time clamps; governance instructions may mark **only** registry/config accounts writable (never a `Position`/`Market` balance, the mint, or escrow); a raised rate floor applies only to new/adjusted positions and never reorders existing borrowers' redemption priority. The clamp constants (e.g. `MAX_RESERVE_LAMPORTS`, `MAX_LIQ_GAS_COMP_BPS`, `MIN/MAX_BUCKET_WIDTH_BPS`, `MAX_REDEMPTION_FEE_BPS`) exist, and the `GovernanceGate` + FUSD-owned timelock now carry the market-param setters (clamps enforced at queue + execute), and the de-risk `guardian_derisk` + `set_guardian` are built; the broader `RiskParamRegistry` is not. | [Partial] (gate/timelock + guardian [Built]; `RiskParamRegistry` [Planned]) |
| Oracle degradation **freezes new mints only** | `update_price` writes `Market.mint_frozen` from `aggregate`'s mode on stale/divergent/wide-confidence feeds; `borrow` reverts (`MintFrozen`) while repay, liquidation, and redemption ignore the flag and keep using the conservatively-priced `Market.spot`. (A genuinely stale aggregate further ages the `spot` cache out, so those paths also pause via the staleness gate — the Solana-halt breaker.) | [Built] (aggregation + the live `update_price`/`sample_twap` cranks; 29 litesvm crank tests incl. positive coverage) |
| Solvency **never depends on a governance/sister token** | Recapitalization draws from a **pre-funded insurance buffer** (`InsuranceBuffer` PDA, per-market, fUSD-denominated) + the collateral-denominated `Market.surplus_collateral`/`protocol_collateral`, never reflexive token dilution. The buffer account + the absorb/haircut waterfall + the value-recovery trio (`sweep_protocol_collateral`/`settle_bad_debt`) are built; only the buffer funding-*source* policy is open. | [Built] (funding-source policy [Planned]) |

#### The hard solvency invariant (always enforced)

The one solvency check that is **always** enforced on every debt-affecting operation is per-vault, oracle-priced:

```
recorded_debt  ≤  max_debt(collateral_value(ink, spot), MCR)
```

where `collateral_value = ink · spot / RAY` (floored), `max_debt = value / MCR` (floored), and `recorded_debt` is the position's present-value debt (the BOLD model — stored directly, no `art·rate` normalization; interest + pending redistribution are folded in by `accrual::realize` before the check) — every rounding direction favors the protocol. This is `cdp::is_healthy(ink, recorded_debt, spot, mcr_bps)`, checked in `borrow`, `withdraw`, and (as the eligibility rule, inverted, against the HIGH `debt_spot`) `liquidate`. It depends on no global counter and no governance state, so it can never be relaxed by a parameter change below its compile-time MCR floor. **[Built]** (5 unit tests in `cdp.rs`; exercised across the litesvm suites).

#### 8.1.1 The lever audit — every mutable gate, and proof none can block a solvency or exit path

The constitutional claim "no toggle can disable a solvency defense or a user exit" made grep-verifiable (klend's own `emergency_mode` freezes ~35 instructions *including repay and liquidation*, and is checked even on its dead-letter recovery path; this table is the structural proof Fusion cannot express that). One row per mutable lever; the **alive-path proof** column names the litesvm test pinning that the lever leaves the protected set open. The cross-product (`litesvm_lever_matrix.rs`) additionally flips *every* restrictive lever to its worst case simultaneously and proves the protected set — full repay-to-zero, liquidation of an unhealthy target, ordered redemption, `urgent_redeem`, RP withdrawal, gain claims, surplus claims, `close_position` — still runs.

| Lever | Writer → authority (clamp) | Read by | Alive-path proof |
|---|---|---|---|
| `mint_frozen` | `update_price` crank only, rule-based (oracle disagreement); **no setter exists** | `borrow` ONLY | `litesvm_oracle_matrix::mint_frozen_row_blocks_only_borrow` |
| `guardian_paused_until` | `guardian_derisk` → guardian (≤ `GUARDIAN_MAX_PAUSE_SECS`, auto-expires) | `borrow` ONLY | `litesvm_guardian` de-risk-only proof; lever matrix |
| `shutdown` | `shutdown` crank only, permissionless + condition-gated (SCR breach / oracle failure); **no setter exists**; irreversible | `borrow`, ordered `redeem` (close); `urgent_redeem` (open) | `litesvm_shutdown::repay_deposit_withdraw_stay_open_after_shutdown` + `liquidation_stays_open_after_shutdown` |
| `liq_grace_until` | `commit_fresh_spot` (stall→resume) + MCR-raise execute; both monotone-max, rule-based | `liquidate` ONLY | `litesvm_mcr_grace::redeem_shutdown_urgent_redeem_unaffected_by_armed_grace` |
| `rl_cap` | gate (timelocked; **unclamped — disclosed**: 0=off, larger=looser; the fast loosen-path) | `borrow` consume / `repay` restore-never-fails | `litesvm_ratelimit` exemption tests; lever matrix |
| `ccr_bps` | gate (timelocked; `[MIN,MAX_CCR_BPS]`, 0=off) | `borrow` + `withdraw` (fails OPEN on stale price; **skipped in shutdown** so it can never strand a repaid borrower's collateral in a terminal market) | `litesvm_ccr` + `litesvm_lever_matrix::scene_b` |
| `mcr_bps` | gate (timelocked; `[MIN,MAX_MCR_BPS]` + relational bounds; raise arms the grace) | `borrow`/`withdraw`/`liquidate` health rule | `litesvm_mcr_grace`; **disclosed**: an in-clamp raise prospectively affects existing positions' eligibility — it rides the timelock + the machine-enforced ~1h grace |
| `min_debt` | gate (timelocked; ≤ `MAX_MIN_DEBT`, 0=off) | `borrow`/`repay` ("0 or ≥ floor" — full repay always allowed) | lever matrix scene A (repay-to-zero at the $10k max floor) |
| `redemption_fee_bps`, `rate_adjust_cooldown_secs`, `liq_bonus_bps`, `liq_gas_comp_bps`, `debt_ceiling` (**unclamped — disclosed**; 0 pauses new debt only), `keeper_reward_bps`, `reserve_lamports` (init/open-time), `bucket_width_bps` (init-time) | gate (timelocked, clamped except as disclosed) | fee/penalty/incentive math only — none gates an instruction's availability | full suites |

Negative rows (grep-verifiable absences): **no setter exists** for `shutdown` or `mint_frozen` outside their rule-based writers; `urgent_redeem`'s ONLY gates are `shutdown == true` and `spot > 0` — deliberately **no staleness gate** (the wind-down proceeds on a dead oracle; `litesvm_oracle_matrix::shutdown_row`); **no program denylist** of any kind (vs klend's hardcoded `RESTRICTED_PROGRAMS` co-residency list); **no global emergency flag** (the dead field was removed — §7.2 note). The two disclosed deterministic exceptions: **staleness** pauses the price-consuming paths (liquidate, ordered redeem, debt-bearing withdraw) per invariant 5's conservative rule — rule-based, never an admin bit; the **CCR band** when enabled also blocks debt-free collateral withdrawal in a live stressed market (aggregate-TCR keyed, fails open on stale price, skipped in shutdown). Litmus test for every future lever: *can this bit strand value? does it replace an on-chain-measurable condition with discretion?*

### 8.2 Conservation invariants (asserted by the test suite)

Beyond the per-vault check, the integration tests assert system-wide conservation after every state-changing flow. These are the properties an auditor checks for "money can't appear or vanish."

| Invariant | Meaning | Where asserted |
|---|---|---|
| `vault == total_collateral + surplus_collateral` (**exact**, redemption-only case; the full 4-term invariant adds `total_coll_surplus + protocol_collateral`) | The on-chain collateral escrow balance equals tracked collateral plus retained redemption-fee surplus — no leak, no phantom collateral. | `litesvm_redemption.rs` (exact, fee retained as surplus) |
| `Market.total_collateral == collateral-vault balance` (**exact**) | Holds through liquidation's gas-comp skim and the RP/redistribution split (`gas_comp + coll_sp + coll_r == ink`). | `litesvm_redistribution.rs`, `litesvm_reserve_gascomp.rs` |
| `Σ stake == total_stakes` (**exact**) | The redistribution stake bookkeeping (`stake = ink · total_stakes_snapshot / total_collateral_snapshot`) conserves across liquidations — verified with coprime stakes (7/11/13) to stress the division. | `litesvm_redistribution.rs` |
| `agg_recorded_debt ≥ Σ present recorded_debt` and `total_collateral ≥ Σ ink` (**protocol-favoring**) | Floor dust from per-position realization accumulates *in the protocol's favor* (aggregates are never less than the sum of positions — the reverse would be a solvency hole). Gap bounded by ~number of recipients (asserted `≤ 3`). | `litesvm_redistribution.rs` (the coprime-stake dust case) |

The distinction between **exact** conservation (collateral, stakes) and **protocol-favoring inequality** (debt/ink dust) is deliberate and load-bearing: the Reactor-Pool `P`/`S` math and the redistribution accumulators round residuals so they can only ever leave the protocol *over*-collateralized, never short. The adversarial review fuzzed the solvency inequalities (`present_debt(art_sp) ≤ offset_present`, `offset_present + present_debt(art_r) ≥ debt`, `Σ realized ≤ distributed`) across 2M+ trials clean.

### 8.3 Adversarial-review process

Every major subsystem passed through a dedicated multi-agent adversarial review (multiple independent auditor agents plus a synthesis pass) before being locked in. The reviews found, and the codebase fixed, two **critical** bugs and one **high-severity** gap:

- **The redeem-skips-realize resurrection (CRITICAL).** `redeem` originally skipped `redist::realize` on its candidates. A position carrying pending *redistributed* debt, redeemed to zero against its stale recorded `art`, would **resurrect** that debt on its next touch — outside its rate bucket, untargetable, with `agg_art` carrying un-burned debt. Fixed by realizing every candidate before redeeming (now mandatory, like every position touch); regression-tested.
- **The reuse-bond gap (HIGH).** A `Position` reused after liquidation (re-opened on a still-allocated account) ran **bond-free**, since the SOL liquidation reserve was only posted at `open_position`. Fixed by re-posting the bond in `deposit` when a reused/under-bonded position sits below the market's current bond; regression-tested.

Other reviews (Reactor Pool P/S, two-tier liquidation, reserve/gas-comp accounting) returned **SOUND, ship-ready** verdicts, with non-blocking findings (test-hardening, doc corrections, a redundant `set_stake` removed) folded in before commit. The reviews also correctly *dismissed* false alarms (e.g. a Liquity `stake == 0` dust position flagged as a bug). Exact-value test assertions were independently derived from the Liquity P/S math first, then confirmed against the program — not read off the program's own output.

**Still owed before launch:** the redemption mechanism is *novel on Solana* (no deployed protocol does rate-keyed redemption), so it owes bespoke fuzzing + Certora formal verification — especially the concurrent "last member leaves a bucket" bitmap flip, currently serialized by the single per-market `Market` write-lock.

### 8.4 Global-supply auditability

The strong global invariant `FUSD minted == Σ market debt` **cannot** be a single hot counter without serializing every `borrow` behind one write-lock (the failure mode that killed Maker's global `Vat` on Solana). The plan is **sharded per-market debt counters** (`Market.agg_recorded_debt` per collateral) plus a periodic **permissionless reconciliation crank** that re-derives total supply against the mint, with monitoring on the residual. The always-enforced per-vault hard solvency check (above) is what guarantees individual solvency in the interim; the reconciliation crank is an *auditability* layer, not a solvency dependency. **[Planned]**.

### 8.5 The FUSD Token

| Property | Value | Source / Status |
|---|---|---|
| Token program | **Legacy SPL Token** (`Tokenkeg…`) | `init_protocol` under `Program<Token>` · [Built] |
| Decimals | **6** (`FUSD_DECIMALS`, USDC/USDH convention) | `init_protocol.rs` · [Built] |
| Mint address | PDA `[b"fusd_mint"]` | `FUSD_MINT_SEED` · [Built] |
| Mint authority | PDA `[b"mint_authority"]` (`MINT_AUTHORITY_SEED`) — never a keypair | `init_protocol.rs` · [Built] |
| Freeze authority | **None** (omitted at creation; irreversible) | `init_protocol.rs` · [Built] |

#### Supply model

FUSD supply is **fully demand-driven and debt-backed**. There is no premine, no founder allocation, no fixed cap — every unit in circulation corresponds to outstanding CDP debt:

- **Mint:** `borrow` mints FUSD (via `token::mint_to` signed by the `[b"mint_authority"]` PDA / `invoke_signed`, up to the position's MCR-bounded `max_debt` and the market's debt ceiling), and `refresh_market` mints accrued interest (`unminted_interest`) into the insurance buffer. No off-chain key can mint; minting outside the collateralization / accrued-interest rules is structurally impossible.
- **Burn:** `repay` burns FUSD (`token::burn`) to reduce a position's `recorded_debt`; `liquidate` burns the Reactor Pool's FUSD when offsetting a liquidation; `redeem`/`urgent_redeem` burn FUSD in exchange for face-value collateral; `settle_bad_debt` burns recovered FUSD against `bad_debt`. Every reduction in debt is matched by a burn.
- **Decimals (6) vs internal precision.** The token's 6 decimals are the *external* representation; internal accounting uses the WAD/RAY/RAD fixed-point discipline (per-position `recorded_debt` in fUSD-native units + a `user_rate_bps` rate — the BOLD weighted-debt-sum model, no `art·rate` normalization) in `fusd-math`, with rounding always against the protocol.

Because supply is sharded across per-market `agg_art` aggregates and the mint carries no freeze authority and a PDA-only mint authority, the token is "unstoppable cash" by construction: no issuer can freeze a holder's balance, no admin can mint outside the collateralization rules, and the total supply is reconcilable on-chain against aggregate debt.

---

## 9. Prior Art & Competitive Landscape

This section positions FUSD against what already exists on Solana. The short version: FUSD is **"Liquity v2 / BOLD, brought to Solana."** The closest *existing* product is **Hubble Protocol (USDH)** — but it is a Liquity-v1-generation design and is effectively defunct, and **no live Solana product replicates FUSD's BOLD-style, user-set-rate, rate-bucket-redemption CDP.**

> **Caveat.** The status and TVL figures below reflect a point-in-time landscape survey (mid-2026) of a fast-moving market. The *structural* conclusions (Hubble is the closest analog; nothing live matches the BOLD shape) are robust; specific figures for smaller/newer names should be re-verified before external publication.

### 9.1 Closest existing product — Hubble Protocol (USDH)

Hubble is the nearest neighbour by a clear margin, because it is the only shipped Solana product that implemented the combination that defines FUSD's engine:

- A genuine **per-user overcollateralized CDP** (lock SOL/BTC/ETH/LSTs → mint USDH), not a tranche or pooled-reserve approximation.
- A canonical **Liquity Stability Pool** (the "USDH Vault") that burns the stablecoin to absorb liquidated debt and pays out seized collateral to depositors.
- The same **two-tier liquidation cascade** FUSD uses: Reactor-Pool offset first, then pro-rata redistribution across remaining borrowers when the pool is drained (Liquity's `L_ETH`/`L_LUSDDebt` → FUSD's `l_coll`/`l_art`).

No other Solana product matches FUSD on *(real per-user CDP) + (real Reactor Pool) + (offset-then-redistribution liquidation)* simultaneously.

**The decisive differences (why FUSD is not just "Hubble again"):**

- **Generation.** Hubble is **Liquity v1**-lineage; FUSD is **v2/BOLD**-shaped. Hubble has no user-set per-borrower rates, no rate-bucket "debt-in-front" redemption, and keeps Recovery-Mode framing.
- **Peg tool.** Hubble's dominant peg mechanism was a Maker-style **USDC PSM** (1:1 USDC↔USDH swaps), which makes the stablecoin partly USDC-backed. FUSD has **no PSM and is not USDC-backed**; its hard peg floor is permissionless **par redemption for collateral**, targeted at the lowest-borrower-rate debt.
- **Governance.** Hubble is a conventional HBB token DAO that can change fees (it raised its stability fee 10%→12.5% in March 2024 to defend the peg). FUSD's intended model is **MetaDAO futarchy** via Squads V4, bounded by construction so governance can never seize/freeze/mint (see §7).
- **Collateral isolation.** Hubble used a shared multi-asset basket (shared-contagion risk). FUSD uses **isolated per-collateral `Market` PDAs**, each with its own RP/MCR/oracle — for both risk isolation and Sealevel write-lock parallelism (see §1.4).
- **Status.** Hubble is **deprecated/moribund** as of mid-2026: USDH was wound down (≈ \$2.7M), HBB fell ≈ 99.9%, the team migrated to **Kamino**, and "Hubble 2.0" never shipped. So the closest comparable is itself a cautionary, dead one.

### 9.2 Closest *live* analog — Hylo (hyUSD / xSOL)

Hylo is the closest thing currently growing on mainnet and shares real DNA — SOL/LST overcollateralization, a reactor-pool-spirited backstop (`sHYUSD`), and permissionless par-style redemption. But it is **architecturally a different animal**: a single pooled **dual-token reserve** (hyUSD senior + xSOL junior leverage tranche), *not* per-user CDP positions, and it is **liquidation-free** — volatility is absorbed by the xSOL tranche, so there is no RP-offset, no redistribution cascade, and no troves. It is closer to Frax/f(x) in spirit than to BOLD, and is genuinely *further* from FUSD than Hubble on the axes that matter most (per-user CDP, RP-offset liquidation).

### 9.3 The landscape at a glance

| | Per-user CDP | Liquity RP | Offset + redistribution | User-set rates / rate-bucket redemption | Status (mid-2026) |
|---|---|---|---|---|---|
| **FUSD** | ✅ (`recorded_debt`/`ink`, BOLD per-position accounting) | ✅ bounded zero-copy P/S | ✅ 5-tier waterfall, no Recovery Mode | ✅ **the defining feature** | core built, pre-audit |
| **Hubble (USDH)** | ✅ | ✅ | ✅ | ❌ (zero-interest + gov fee; USDC PSM) | **deprecated** |
| **Hylo (hyUSD)** | ❌ (dual-token pool) | ~ (system-CR backstop) | ❌ liquidation-free | ❌ | **live** |
| **Ratio Finance (USDr)** | ✅ | ❌ | ❌ | ❌ | dormant |
| **Parrot (PAI)** | ✅ | ~ (keeper pool, not RP) | ❌ | ❌ (USDC PSM) | defunct (2023) |
| **Zero Interest Protocol (ROKS)** | ✅ (design) | ✅ (design) | ✅ (design) | ❌ (pre-v2) | **never launched** |
| **Sky / Maker USDS** | on Ethereum | ❌ (auctions) | ❌ | ❌ | bridged token only |

Notes: *Ratio* is a real Solana CDP but collateralized mostly by stablecoin LP tokens, with no Liquity RP. *Parrot* had a keeper "liquidation pool" (not a stablecoin-burning RP) and leaned on a USDC PSM. *Zero Interest Protocol* was announced as a near-literal Liquity-**v1** port — the most on-paper match for the trove + RP + redistribution skeleton — but never shipped, and predates v2 so it lacks every distinctive FUSD economic feature. *Sky/Maker USDS* on Solana is a Wormhole-bridged token whose CDP machinery lives on Ethereum (Maker uses collateral auctions, which FUSD explicitly rejects — see §4.2).

### 9.4 What has no Solana analog at all

Several FUSD design choices have **no deployed Solana equivalent** — these are the genuinely novel surface, and the right emphasis for external positioning:

- **On-chain rate-bucket bitmap for redemption targeting** (`RedemptionBitmap`: `words[u64;4]` + `counts[u32;256]`, find-first-set on the lowest non-empty bucket). Existing Solana protocols order redemption/liquidation by *collateral* ratio; none key on borrower-chosen *interest rate*. The "debt-in-front by user rate" (BOLD) ordering has no deployed Solana analog (see §5).
- **User-set per-borrower interest rates** as a peg loop. Every surveyed Solana CDP uses protocol- or governance-set rates.
- **Price-independent redemption targeting** — the bucket key derives only from `user_rate_bps`, so redemptions stay correctly targeted even during an oracle outage while new mints freeze.
- **A self-maintained DEX-TWAP corridor** (`ObservationRing`), required because Solana CLMMs (Orca Whirlpool, Raydium CLMM) expose no Uniswap-v3-style on-chain cumulative-price accumulator (see §6.2).
- **Legacy SPL Token as a censorship-resistance feature** — chosen precisely because it *lacks* Token-2022's extensions, the inverse of the prevailing "more token features" instinct (see §1.1, §8.5).
- **MetaDAO futarchy as the intended risk-parameter authority** for a CDP stablecoin — no live Solana stablecoin governs its parameters this way (see §7.1).

The one-line external framing: **"Liquity v2 / BOLD, brought to Solana,"** with Hubble as the v1 predecessor that proved CDP-stablecoin demand existed on Solana but whose v1 design and PSM dependence did not hold the peg.

---

## 10. Compliance & Regulatory Posture

This section states how FUSD relates to AML/sanctions/regulatory expectations **without** compromising the credible-neutrality thesis (§1.1, §8.1). The organizing principle is simple and load-bearing: **compliance flows strictly downhill — never in the immutable core, always at the bypassable periphery and in the social/legal layer.** Most of what follows is design posture and recommendation, not yet-built mechanism; it is tagged accordingly.

> **Disclaimer.** This is engineering/strategy documentation, not legal advice. The regulatory characterizations below (GENIUS Act / MiCA / FinCEN non-applicability, the *Van Loon* reading) are interpretive arguments that require qualified counsel before any public launch. Specific case/statute facts reflect a point-in-time survey and should be re-verified.

### 10.1 Two different risks — solvency vs. reputational/legal

The recurring worry — *"a hacker deposits stolen funds into this permissionless protocol and mints a lot of FUSD; doesn't that make us riskier?"* — conflates two distinct risks. Separating them resolves most of the question.

- **Protocol solvency risk — unaffected.** FUSD is overcollateralized. A hacker minting against stolen collateral must lock value worth **more** than the FUSD minted (MCR 120–150%), posts the per-position SOL reserve bond, and is subject to permissionless liquidation (§4) and the always-on redemption floor (§5). If the actor abandons the position, the over-collateral backs the debt and liquidators/redeemers make the system whole. The protocol takes on **zero bad-debt exposure** from this scenario — unlike an undercollateralized lender. The peg and the books are not threatened. *(Launch caveat: production markets start frozen and cannot borrow until the `update_price` crank has run with healthy feeds — §6.4 — so this is a launch-state consideration, not a current one.)*
- **Reputational / legal / adoption risk — real, and downstream of the mint.** What the scenario actually produces is **layering**: an identifiable "dirty" asset is converted into freshly-minted, more-fungible FUSD. The harm is therefore (a) reputational (FUSD perceived as a laundering rail), (b) legal exposure for any *controllable* periphery and for the development team's *conduct*, and (c) downstream taint — the minted FUSD can be risk-scored and declined by exchanges/VASPs. None of this is a solvency or peg problem.

The practical consequence: **resist engineering an on-chain "fix" for a financial problem that does not exist.** An invasive in-core screen would forfeit the thesis (and the legal shield, §10.2) to address a risk that lives entirely at the periphery.

### 10.2 Why a protocol-level freeze is rejected — thesis and legal shield

A mechanism to freeze a hacker's FUSD would moot the "cannot be frozen" guarantee outright. This is not a close call, for three compounding reasons:

1. **It negates the value proposition.** "Unstoppable cash" (§1.1) *is* the differentiator versus USDC. A freeze hook makes FUSD a worse USDC.
2. **It forfeits the legal shield.** The strongest protection for an immutable protocol is precisely its *un*-controllability: in *Van Loon v. Treasury* (5th Cir., 2024) immutable smart contracts were held not to be sanctionable "property" because no person — including the developers — could control their operation; Treasury subsequently delisted the contracts (2025). A freeze function is the textbook "controllable property" that moves FUSD from the protected category into the "mutable contract + controlling person" bucket. The load-bearing facts are therefore `freeze_authority = None` set irreversibly at `InitializeMint`, **on a token program (legacy SPL) that has no permanent-delegate / pausable extension to re-enable one** (§8.1). USDC demonstrates that a legacy-SPL mint *with* a live freeze authority is freezable — so the absence is the asset.
3. **On Solana, discretionary "stop-loss" levers backfire.** The community's "stop hackers" instinct resolves in practice to *external capital* (e.g. the Wormhole backstop) and *criminal law*, not base-layer reversal or protocol freezes. Discretionary interventions have been reputational disasters (a lending protocol's emergency vote to seize a whale wallet, reversed within ~24h amid revolt) or have failed as legal defenses (the "the code/DAO permitted it" argument losing at trial). A freeze lever is a liability magnet and a coercible governance surface, not a safety feature.

**This closure must hold at every layer, including governance.** It is an affirmative constitutional invariant (§8.1), not an accident of the current feature set.

### 10.3 The layered model — where compliance legitimately lives

| Layer | Hosts compliance logic? | Contents |
|---|---|---|
| **Immutable core** | **Never** | the FUSD mint (`freeze_authority = None`, legacy SPL, PDA-only mint authority bound to `art·rate`); the absence of any seize/pause/denylist; permissionless `borrow`/`repay`/`deposit`/`withdraw`/`liquidate`/`redeem` that consult **no** allowlist account; compile-time clamps; non-retroactivity |
| **Periphery** (fully bypassable) | **Yes** | hosted front-ends (address screening, geofencing, ToS); the governance-gated **collateral-onboarding allowlist** (`init_market`); gov-set risk-param throttles within clamps and the de-risk-only guardian; SDK / RPC / indexer |
| **Social / legal** | **Yes** | legal entity / foundation; "software, not an MSB/issuer" positioning; third-party audits; proof-of-reserves; public messaging by legitimate function |

The periphery being **bypassable** (anyone may call the program directly) is the feature, not the bug: it lets a regulated participant comply *without the protocol acquiring a censorship power*. This is the Liquity front-end-operator model, and the posture LUSD/RAI established.

### 10.4 Reasonable compliance levers

Scored against four axes — preserves neutrality? helps institutional adoption? helps community acceptance? cost/downside — and a recommendation.

**Adopt (cheap, neutrality-preserving):**

- **Front-end address screening** (Chainalysis / TRM / Elliptic) on any team-affiliated UI — `[Planned, periphery]`. The remedy to a specific actor is screening the **output** FUSD downstream, not freezing it. The framing to lead with: FUSD is **screenable-but-unfreezable**, like ETH/BTC — Chainalysis/TRM already cover all Solana SPL tokens, so it is *policeable* the moment it has volume, with no freeze hook. Keep any screened UI as **one of several** independent front-ends, not the sole controllable nexus.
- **Geofencing + Terms of Service** on hosted front-ends — `[Planned, periphery]`. Standard operator hygiene; protects the operator, not a fund-censorship lever.
- **Collateral-onboarding allowlist as a governance control** (`init_market`) — `[Built — rejects freeze-authority-carrying mints; legacy-SPL-only typing structurally rejects all Token-2022 mints, so no TLV scan is needed]`. The **strongest legitimate in-protocol lever**, because it screens **assets, not identities**. It also keeps freezable / issuer-controlled assets (USDC, RWA) out of the neutral core, so FUSD does not import a third party's freeze power transitively. Make onboarding a governed, documented process with per-asset debt ceilings.
- **Bounded `GovernanceGate` + FUSD-owned timelock** — `[Built]`: param changes queue behind a clamped delay and execute permissionlessly (§7.1). The **de-risk-only `guardian_derisk`** (§7.2) is also `[Built]`: a monotonic, per-market, auto-expiring pause of **new** debt only — never touches user funds, repay, liquidation, or redemption — with gov-gated `set_guardian` rotation. The guardian scope is resolved: pause-new-debt only (lower-ceiling stays with the gate; "raise fee" dropped as anti-de-risk for the floor).
- **Proof-of-reserves (supply-reconciliation crank) + third-party audit** — `[Planned]`. Table stakes for any serious institution or listing; currently only internal adversarial review exists, and supply is sharded across per-market `agg_art`. Non-negotiable before public launch.
- **Legal entity / "software, not an MSB/issuer" positioning** — `[Social/legal]`. **Highest leverage of all**, because this is where the team's *personal* exposure sits (developer/operator *conduct* liability survives even when the protocol is immune). Run **no** team-operated flagship front-end and no fee-skimming relayer the team controls; obtain a counsel-signed GENIUS-Act / MiCA / FinCEN non-applicability memo (no issuer, no fiat redemption, overcollateralized crypto-backed CDP). Describe FUSD by its CDP/stablecoin function — **never** by censorship-evasion.

**Defer:**

- **Permissioned / KYC'd venue for institutions** (Aave-Arc pattern) — `[Planned, Phase 4+]`. If real institutional demand emerges, offer an opt-in, **segregated** venue layered *on* the permissionless core — **never** a gate on the base program. The track record is weak (Aave Arc emptied and was retired) and demand is unproven; the thesis stands without it.

**Reject (categorically):**

- **Token-2022 freeze / permanent-delegate / pausable on the mint.** Direct negation of the thesis and the *Van Loon* shield; reclassifies FUSD as issuer-led, which there is no entity to be.
- **An on-chain protocol denylist / sanctions-PDA gate** on `borrow`/`deposit`/`redeem`. Puts a coercible censor in the core, inherits sanctions-oracle lag (false negatives on the exact fresh-hacker case) and taint over-blocking (false positives), and solves a solvency problem FUSD does not have.
- **Using the collateral allowlist or a futarchy vote to target a specific actor's deposit.** De-listing a blue-chip asset to stop one actor is collateral-blunt and ineffective (they use an already-listed liquid asset); targeting an individual is the binary censorship decision futarchy is structurally wrong for, and a liability magnet. The allowlist is for **asset-class curation, never for targeting individuals.**
- **A single team-operated flagship front-end / fee-skimming relayer.** A single controllable access point is the nexus regulators actually squeeze. Encourage a plurality of independent front-ends, each carrying its own OFAC/KYC.
- **Accepting freezable, issuer-controlled collateral (USDC, RWA) into the neutral core.** Imports a third party's freeze power transitively and pulls FUSD toward the RWA-forces-a-freeze slippery slope.

### 10.5 Two meta-decisions to lock toward neutrality

Credible neutrality is only *legally* protected if it is real and provable, not merely marketed. Two open decisions should be resolved decisively on the neutral side:

- **Token program → lock to legacy SPL, `freeze_authority = None`.** Everything built already treats this as settled; make it a formal, closed invariant.
- **Immutability endgame → move the BPF upgrade authority toward renouncement or a permanent governance-lock.** This is the one that is easy to miss: **the upgrade authority transcends every in-program invariant** — until it is renounced or permanently locked, whoever holds it could insert arbitrary compliance (or any other) logic, which means the immutability is "current" rather than "credible," and the *Van Loon* protection is correspondingly weaker. This is the meta-lever that must land on the neutral side.

### 10.6 Residual risk — accept and disclose

Some risk cannot (and should not) be engineered away:

- FUSD will be **screenable-but-unfreezable**, like ETH/BTC.
- **Some US-regulated venues** that only handle freeze-capable "payment stablecoins" may decline to list it — so the go-to-market is sized for DeFi-native and non-US venues plus a plurality of independent front-ends.
- It **cannot** prevent a determined actor from minting against legitimately-onboarded collateral. That is by design; the protocol self-insures via the insurance buffer (§4.5, `[Built]`; funding-source policy `[Planned]`) and competes where unfreezability is a feature.

**Bottom line:** holding the core absolutely neutral and pushing all compliance to the periphery and the social/legal layer is not a compromise of the thesis — it is the mature execution of it. The hacker-mints-FUSD scenario does not increase protocol risk; it increases reputational/legal risk, which is managed downstream of the mint, never inside it.

---

## 11. Reference, Build & Roadmap

This section is the cross-check between the design and the bytecode: the exact instruction surface and account set the program exposes today, how it is built/tested/gated, and what remains. Every row is verified against `programs/fusd-core/src/lib.rs` (the `#[program]` block), the `state/` modules, `constants.rs`, `errors.rs`, the `Cargo.toml` manifests, and `scripts/`.

### 11.1 Instruction reference

The `#[program]` block in `fusd-core` exports **46 production instructions** plus one dev/test-only instruction (`dev_set_price`, compiled only under `#[cfg(feature = "dev-oracle")]` and **excluded from every production build and from the IDL**, enforced by a release gate). Init-time setup instructions and the admin `set_guardian` authorize via `require_keys_eq!` against `ProtocolConfig.gov_authority` (itself rotated via the two-step `migrate_gov_authority`/`accept_gov_authority`). **Market-parameter changes flow through the bounded `GovernanceGate` + FUSD-owned timelock [Built]:** `queue_param_change` (gated on the gate's migratable inbound authority, clamped + relationally validated) → after the delay → permissionless `execute_param_change` (with the two-step `migrate_inbound_authority`/`accept_inbound_authority` and `cancel_param_change`). The independent **`guardian_derisk`** (gated on `config.guardian`) is [Built]; the broader `RiskParamRegistry` remains **[Planned]**. All events ride the Anchor `#[event_cpi]` self-CPI transport (immune to RPC log truncation).

| Instruction | Caller | Effect (one line) | Status |
|---|---|---|---|
| `init_protocol` | deployer (once) | Create `ProtocolConfig` + the FUSD mint (legacy SPL, `freeze=None`, mint authority = `[b"mint_authority"]` PDA). | [Built] |
| `init_market` | governance | Onboard a collateral as an isolated `Market` + escrow `collateral_vault` + `RedemptionBitmap`; freeze-authority + legacy-SPL-typing gate + relational config bounds. | [Built] |
| `init_market_oracle` | governance | Bind a market's Pyth/Switchboard/DEX feeds + quote-mint decimals + clamped thresholds; create its `DexTwap` ring. | [Built] |
| `sample_twap` | anyone | Parse + guard an Orca/Raydium CLMM pool and append a `usd_ray` observation to the `DexTwap` ring. | [Built] |
| `update_price` | anyone | Parse a Full-verified Pyth `PriceUpdateV2` + optional Switchboard, run `fusd_oracle::aggregate` against the TWAP corridor, write `Market.spot` + `mint_frozen`. | [Built] |
| `refresh_market` | anyone (rewarded) | Accrue the aggregate interest and mint `unminted_interest` into the insurance buffer; pay the cranker a `keeper_reward_bps` cut. | [Built] |
| `set_oracle_program_ids` | governance | Update the bounded-updatable oracle PROGRAM IDs (Pyth receiver + alt + Switchboard) so the Pyth core migration (~2026-07-31) needs no redeploy. | [Built] |
| `rebind_market_oracle_feeds` | governance | Rebind a market's oracle feed SOURCES (Pyth feed id / Switchboard account / DEX-TWAP pools) for a feed migration. | [Built] |
| `init_governance_gate` | `gov_authority` | Create the gate (migratable inbound authority + clamped timelock delay). | [Built] |
| `migrate_inbound_authority` / `accept_inbound_authority` | inbound auth / successor | **Two-step** repoint of the gate's inbound authority (propose → the successor signs to accept). | [Built] |
| `queue_param_change` / `execute_param_change` / `cancel_param_change` | inbound auth / anyone / inbound auth | Queue a clamped + relationally-validated `MarketParam` change; permissionless execute after the delay (emits the `prev_value`→`value` trail; an MCR raise arms the grace); or cancel. | [Built] |
| `init_global_backstop` / `fund_backstop` / `withdraw_backstop_excess` | governance / anyone / gate (inbound auth) | Create the Global Backstop Reserve (ships inert, every param 0/off) / permissionlessly fund it / withdraw ABOVE-CAP excess (never below `reserve_cap`). | [Built] |
| `queue_global_param_change` / `execute_global_param_change` / `cancel_global_param_change` | inbound auth / anyone / inbound auth | Queue a clamped GLOBAL backstop-param change behind the timelock; permissionless execute after the delay; or cancel. | [Built] |
| `guardian_derisk` | guardian | Per-market auto-expiring pause of NEW debt only (clamped 7 days; `0` lifts early). | [Built] |
| `set_guardian` | gov_authority | Rotate/revoke the guardian (immediate; `Pubkey::default()` disables it). | [Built] |
| `migrate_gov_authority` / `accept_gov_authority` | gov_authority / successor | **Two-step** rotation of the bootstrap/admin authority. | [Built] |
| `open_position` / `close_position` | borrower | Open an empty CDP + post the SOL bond / close an empty CDP + reclaim rent + remaining bond. | [Built] |
| `claim_coll_surplus` | position owner | Withdraw the collateral the liquidation bonus collar returned (`Position.coll_surplus`); safe in shutdown. | [Built] |
| `deposit` / `withdraw` | borrower | Add `ink` (re-post the bond if under-bonded) / remove `ink`, holding ≥ MCR if it carries debt. | [Built] |
| `borrow` / `repay` | borrower / arbitrageur | Mint FUSD up to MCR + the debt ceiling against `Market.spot` / burn FUSD to reduce `recorded_debt`. | [Built] |
| `adjust_rate` | borrower | Change `user_rate_bps` (re-weight + re-bucket); BOLD premature-rate fee within the cooldown. | [Built] |
| `redeem` | anyone | Burn FUSD for face-value collateral, draining the lowest non-empty rate bucket; candidates via `remaining_accounts`, realized then lowest-CR-first; closed candidates skipped. | [Built] |
| `shutdown` / `urgent_redeem` | permissionless / anyone | Terminal wind-down at SCR breach / oracle failure; then unordered 0-fee face-value redemption from any position. | [Built] |
| `init_reactor_pool` | governance | Create the RP: FUSD vault, seized-collateral vault, bounded `EpochToScaleToSum` grid. | [Built] |
| `init_insurance_buffer` / `fund_buffer` | governance / anyone | Create the per-market fUSD insurance buffer / permissionlessly fund it. | [Built] |
| `open_reactor_deposit` / `provide_to_reactor` / `withdraw_from_reactor` / `claim_reactor_gains` | RP depositor | Open / deposit / withdraw (capped at compounded) / claim realized seized-collateral gains. | [Built] |
| `liquidate` | anyone | Liquidate an under-MCR position via the 5-tier waterfall (gas-comp skim, RP offset, redistribution, insurance buffer, global backstop, un-homed→shutdown); bonus collar; SOL bond to the caller. | [Built] |
| `withdraw_surplus` / `sweep_protocol_collateral` / `settle_bad_debt` | gate (inbound auth) | The value-recovery trio: move protocol-owned redemption-fee surplus / un-homed collateral; burn recovered FUSD against `bad_debt`. | [Built] |
| `dev_set_price` | dev/test only | Set `Market.spot` directly. `#[cfg(feature="dev-oracle")]`; **never** in production builds or IDL. | [Built] (dev-gated) |

### 11.2 Account reference

**Fourteen** on-chain account types exist today. The three fixed-array grid accounts (`EpochToScaleToSum`, `RedemptionBitmap`, `DexTwap`) are `#[account(zero_copy)]` (`repr(C)` + `AccountLoader`, layout `const_assert`-pinned); the eleven Borsh `#[account]`s carve new fields from `_reserved` and pin their `SPACE` via the `state::layout_tests` serialized-length test.

#### Built / Partial accounts

| Account | PDA seeds | Purpose | Zero-copy? | Status |
|---|---|---|---|---|
| `ProtocolConfig` | `[b"config"]` | Migratable `gov_authority` (+ `pending_gov_authority`), `guardian`, `deployer`, `fusd_mint`, the bounded-updatable oracle program IDs (`pyth_receiver_program_id` + `_alt` + `switchboard_program_id`); reserved tail. **No global emergency flag** (removed pre-launch — emergency levers are per-market and rule-based only). Read-only on the hot path. | No | [Built] |
| `Market` | `[b"market", collateral_mint]` | The per-collateral lane: BOLD aggregates (`agg_recorded_debt`, `agg_weighted_debt_sum`, `unminted_interest`), cached `spot`/`mint_frozen`, `mcr_bps`/`scr_bps`/`ccr_bps`/`debt_ceiling`; redistribution accumulators (`l_coll`/`l_art` + error terms + stakes + snapshots); the four vault buckets (`total_collateral`/`surplus_collateral`/`total_coll_surplus`/`protocol_collateral`); the liquidation incentives, rate-limiter, `shutdown`/`bad_debt`, and the redemption knobs. `_reserved [u8; 64]`. | No (Borsh) | [Built] |
| `Position` | `[b"position", collateral_mint, owner]` | A user's CDP: `ink`, `recorded_debt`, `user_rate_bps`, `last_debt_update`, redistribution `stake` + snapshots, the posted `reserve_lamports` bond, the stored `bucket`, `coll_surplus`, `last_rate_adjust_ts`. `_reserved [u8; 32]`. | No | [Built] |
| `MarketOracle` | `[b"oracle", collateral_mint]` | Feed bindings (`pyth_feed_id`, `switchboard_feed`, `orca_pool`, `raydium_pool`, quote-mint decimals) + clamped validation thresholds. **Read by `update_price`/`sample_twap`.** | No | [Built] |
| `DexTwap` | `[b"twap", collateral_mint]` | Self-maintained observation ring; field-for-field mirror of `fusd_oracle::ObservationRing<64>`, reinterpreted via a Pod cast (layout asserted at compile time). **Written by `sample_twap`.** | **Yes** | [Built] |
| `ReactorPool` | `[b"reactor", collateral_mint]` | Mirrors `fusd_math::reactor_pool::PoolState`: `p`, `epoch`/`scale`, `total_deposits`, error terms; pointers to its FUSD vault, collateral vault, and grid. | No | [Built] |
| `EpochToScaleToSum` | `[b"ess", collateral_mint]` | The bounded `[u128; 512]` (32×16) gain-per-unit grid; direct-indexed, reverts on exhaustion (`ReactorGridExhausted`) rather than wrapping. | **Yes** | [Built] |
| `ReactorDeposit` | `[b"reactor_dep", collateral_mint, owner]` | A depositor's `{p,s,scale,epoch}` snapshot + recorded `deposited_fusd` + realized `pending_collateral_gain`. | No | [Built] |
| `RedemptionBitmap` | `[b"redeem_bitmap", collateral_mint]` | The `[u64; 4]` non-empty-bucket bitmap + `[u32; 256]` per-bucket member counts + `zombie_count` (the out-of-ordering pen); a bit flips only on a bucket's empty↔non-empty transition. `SPACE = 1072`. | **Yes** | [Built] |
| `GovernanceGate` | `[b"gov_gate"]` | Sole authority on the param setters: the migratable + pending inbound authority, the timelock delay, the queue nonce. | No | [Built] |
| `TimelockedParam` | `[b"timelock", nonce_le]` | One queued param change (eta, market, param, value); created on queue, closed on execute/cancel. | No | [Built] |
| `InsuranceBuffer` | `[b"buffer", collateral_mint]` | The per-market fUSD loss-absorber (tier-3 of the liquidation waterfall); funded by `fund_buffer` + the lazy interest mint. | No | [Built] |
| `GlobalBackstopReserve` | `[b"backstop"]` | The system-wide bounded second-loss reserve (waterfall tier ~3.5): a protocol-owned fUSD vault funded by a minority cut of each market's interest; ships inert (every param 0/off). | No | [Built] |
| `TimelockedGlobalParam` | `[b"gtimelock", nonce_le]` | One queued GLOBAL (backstop) param change; created on queue, closed on execute/cancel (distinct from the per-market `[b"timelock"]`). | No | [Built] |

Associated token vaults (program-owned escrow) are created alongside their owners: `collateral_vault` `[b"coll_vault", mint]` (per `Market`), and the RP's `[b"reactor_fusd", mint]` / `[b"reactor_coll", mint]` vaults. The FUSD mint is a PDA at `[b"fusd_mint"]`; its mint authority is `[b"mint_authority"]`.

The owner-claimable liquidation surplus is **not** a separate account — it is a `Position.coll_surplus` field withdrawn via the [Built] `claim_coll_surplus`. The net-outflow rate limiter lives on `Market` ([Built], §8).

#### Planned accounts (designed, not built)

| Account | PDA seeds | Purpose | Status |
|---|---|---|---|
| `RiskParamRegistry` | `[b"registry"]` | The broader clamped param set beyond the eleven `MarketParam`s (oracle thresholds, `scr_bps`, etc.); written only via the gate. | [Planned] |
| `RateLimiter` (auto-line) | `[b"ratelimit", collateral_mint]` | The Maker DC-IAM debt-ceiling auto-line account (`gap`/`ttl`) — distinct from the [Built] net-outflow limiter on `Market`. | [Planned] |

### 11.3 Build, test & deployment

#### Toolchain

| Component | Version | Why pinned |
|---|---|---|
| Anchor | 0.32.1 | Workspace dependency + `[toolchain] anchor_version`. |
| Solana | 2.3.13 | Anchor 0.32 resolves `solana-program` ^2; oracle SDKs verified to co-resolve to a single `solana-program 2.3.0`. |
| SBF platform-tools | 1.84.1 | The on-chain Rust is **1.84** — the source of the edition2024/MSRV-1.85 pin discipline below. |
| Host Rust | 1.93 | Runs `cargo test` for the crates + litesvm suites. |
| Node / Yarn | ≥20 / 1.22 | `@solana/codecs` requires Node ≥20; the TS e2e harness. |

#### Critical SBF dependency pins

The SBF cargo (1.84) **rejects** `edition2024`/MSRV-1.85 crates that the host cargo (1.93) would otherwise select, so the build depends on holding several transitive deps back in `Cargo.lock`. The known-load-bearing pins are `proc-macro-crate 3.2.0`, `indexmap 2.9.0`, `unicode-segmentation 1.12.0`, and `blake3 1.5.5` (the last dodges `anchor-spl`'s edition2024 `digest 0.11`/`block-buffer 0.12` chain). The two oracle SDKs are the sharpest edge: `pyth-solana-receiver-sdk 1.2.0` and `switchboard-on-demand 0.12.1` (`default-features=false, features=["anchor"]` — `client` is host-only and must stay off on-chain). Both declare **open-ended** anchor/solana version requirements; the current lockfile constrains them to the single `anchor-lang 0.32.1` / `solana-program 2.3.0`, but a fresh resolve or broad `cargo update` will pull a **second** `anchor-lang 1.x` + `solana-program 4.x` and break the SBF build. Recovery is `cargo update -p <crate>@<bad> --precise <good>`. The clean long-term fix is a platform-tools upgrade to Rust ≥1.85. Switchboard's host-side `[build]` deps (prost-build → getrandom 0.4, edition2024) compile on the host cargo only and are absent under `cargo tree -e normal` — not an SBF violation.

#### Test architecture

Tests live in three places, deliberately separated so the dev oracle cannot leak into production artifacts:

- **Crate unit tests** (`crates/fusd-math`, `crates/fusd-oracle`) — pure, dependency-free, tests-first: the U256 WAD/RAY math, the P/S reactor-pool product-sum, the redistribution accumulators, the rate-bucket bitmap, the oracle scaling + TWAP ring + `aggregate`. `fusd-math` carries **113 host tests** (unit + the B8 fuzz-pass proptest properties — the stateful BTreeSet bitmap model, RP scale/epoch round-trips, conservation + fail-closed) plus **25 Kani formal-verification harnesses** (run under the Kani solver, not `cargo test`; see `PROOF_STRENGTH.md`); `fusd-oracle` carries **33**.
- **The isolated `integration-tests` crate** — a **non-program** workspace member running **in-process litesvm** (`litesvm = "=0.7.1"`, the last release on the Solana 2.3 line). It loads the real `.so` and exercises full instruction flows with exact balance/state assertions and revert-code checks. A shared harness (`src/lib.rs`) holds the PDA derivations, every instruction builder, typed account readers, and scenario helpers. The suite is now **~230 litesvm tests** across 35 files (CDP, liquidation, redistribution, reserve/gas-comp, buckets, redemption, zombie-pen, closed-candidate-skip, oracle-crank, oracle-pause-matrix, governance, gov-authority, guardian, shutdown, rate-limit, CCR, MCR-grace, buffer, interest, events, value-recovery, vault-reconcile, lever-matrix, …) — putting the workspace at roughly **400 tests** (~230 litesvm + ~165 host).
- **TS/anchor-mocha e2e** (`tests/`) — wired via `Anchor.toml`, but `solana-test-validator` is killed by this sandbox; the in-process litesvm path is the load-bearing one here.

**The `dev-oracle` feature is needed in two places at once**: in the `.so` at runtime (`anchor build -- --features dev-oracle`) *and* on the `integration-tests` crate's dependency on `fusd-core` (so the generated `instruction::*`/`accounts::*` client modules, including `DevSetPrice`, are importable). Because `integration-tests` is not a program and is excluded from `anchor build`, its `dev-oracle` dependency can **never** unify into the deployed program or IDL — that isolation is the whole reason the tests do not live in the program crate.

#### Release gates (`scripts/`)

| Gate | What it does |
|---|---|
| `verifiable-build.sh` | Deterministic Docker build via `solana-verify` (release profile adds `lto="fat"`, `codegen-units=1` on top of the always-on `overflow-checks=true`); emits the on-chain-comparable program hash. |
| `check-no-dev-oracle.sh` | Production `anchor build`, then fails if `dev_set_price` appears as an IDL instruction (jq) or as a `DevSetPrice` symbol in the `.so`. |
| `check-no-certora.sh` | `cargo tree -p fusd-core -e normal` proves the verification-only `cvlr` dep never reaches `fusd-core`, plus a best-effort `.so` scan for a `cvlr`/`certora` string; mirrors `check-no-dev-oracle.sh`. |
| `check-stack-offsets.sh` | Greps `anchor build` output for `Stack offset of` and turns that **warning** into a hard failure; run for both the production and `dev-oracle` configurations. |

The stack-offset gate exists because the 4 KB SBF stack frame is a real, **build-environment-dependent** hazard: `anchor build` only warns and still emits a `.so` that then corrupts at runtime ("Access violation"). The fix pattern is to **box every large `Account` payload** in an instruction's `Accounts` struct — already applied to `init_market`, `init_reactor_pool`, and `liquidate` (whose unboxed `try_accounts` frame, 56 bytes over on one toolchain build, broke every liquidate call). Any instruction approaching ~13 inline `Account`s is a candidate.

### 11.4 Roadmap & status

The phased decentralization plan gates each step on hard exit criteria; the table below states where the code actually is against it.

| Phase | Scope | Status |
|---|---|---|
| **Phase 1 — Guarded launch / core** | CDP flow + per-position BOLD interest + Reactor Pool + the 5-tier liquidation waterfall with SOL-bond + gas-comp incentives + the rate-bucket redemption floor, on 1–2 collaterals; authority = Squads multisig + timelock via the gate. | **Core mechanisms [Built]** and litesvm-tested + adversarially reviewed; the **`GovernanceGate` + timelock**, the **live oracle cranks**, the **de-risk guardian**, the terminal **`shutdown`/`urgent_redeem`** breaker, the net-outflow **rate limiter**, the **CCR band**, the **on-resume grace**, the **insurance buffer + value-recovery**, per-position interest, the klend-review hardening (vault assert, config bounds, MCR-grace, event-CPI, two-step authorities), and the **B8 fuzz pass** are all **[Built]**. The oracle layout-freeze window (B3 divergence gate, B5 `debt_price`, C6 plausibility band, D3 Pyth-migration-updatable IDs) and the **Certora cross-instruction core-invariant pass** have since landed; the remaining launch-gating work is external **audits** (≥2), the **bitmap concurrent-flip Certora** pass, a **verified build**, and parameter calibration — so the milestone is **functionally complete, not launch-complete**. |
| **Phase 2 — Permissionless & hardened** | Maker auto-line + oracle-divergence gate as bounded params; more collaterals; competing keepers; bitmap concurrent-flip fuzzed + Certora-clean. (The CCR borrow-restriction band landed early in Phase 1, [Built].) | [Planned] |
| **Phase 3 — Governance-minimized** | Point the gate's inbound authority at the MetaDAO futarchy authority behind FUSD's timelock; every param has enforced compile-time clamps. | [Planned] |
| **Phase 4 — Credible neutrality** | Renounce or permanently governance-lock core upgrade authority; multiple frontends/oracles; optional opt-in sFUSD/bounded AMO. | [Planned] |

**Explicitly deferred / planned, by subsystem:**

- **Oracle (the remaining launch-gating window):** the live `update_price` + `sample_twap` cranks are **[Built]** (the borrow blocker is cleared). The delegated oracle window has now landed: the **oracle-divergence gate** (B3), the asymmetric **`debt_price`** for liquidation (B5, `Market.debt_spot`), the absolute **plausibility band** on the spot commit (C6), and the **bounded-updatable oracle program IDs in `ProtocolConfig`** ahead of Pyth's ~2026-07-31 core migration (D3) are all **[Built]**.
- **Governance:** the `GovernanceGate` + FUSD-owned timelock and their setters are **[Built]** — the **eleven `MarketParam`s** are governance-tunable behind the clamped delay (with the relational `validate_market_config` bounds + the forensic `prev_value` trail; two-step authority handoffs; decision-#2 PoC passing). The independent **`guardian_derisk` + `set_guardian` are [Built]** (§7.2). Still **[Planned]:** the `RiskParamRegistry` for the broader param set and oracle-threshold setters (oracle thresholds + `scr_bps` + `reserve_lamports` + `bucket_width_bps` remain init-time-only).
- **Liquidation/redemption polish:** per-position gas-comp cap; surplus-buffer absorption of un-homed bad debt (currently `NoRedistributionRecipients` reverts); `MIN_DEBT` + the always-drained-first zombie bucket for dust; an `adjust_rate` anti-gaming fee + cooldown; a `MAX_REDEMPTION_CANDIDATES` account cap (the >64 DoS); the dynamic Liquity base-rate (vs the current flat clamped fee).
- **Circuit breakers:** the debt-ceiling auto-line (`RateLimiter` account). (Per-market `shutdown`/`urgent_redeem`, the net-outflow rate limiter, the CCR borrow-restriction band, the slot-staleness halt, the on-resume liquidation grace window, and the oracle-divergence gate are [Built].)
- **Other:** `CollSurplus` + `claim_coll_surplus`; the surplus/insurance buffer; the global supply-invariant reconciliation crank; deferred CDP litesvm cases (debt-ceiling 6006). Before launch, the **novel-on-Solana** redemption bitmap still owes bespoke fuzz + Certora — especially the concurrent "last member leaves a bucket" flip, currently serialized only by the per-market `Market` write-lock.

---

## 12. Component Status Summary

Every component from each section's per-section status table, grouped by area.

| Component | Area | Status | Notes |
|---|---|---|---|
| Single-program design (`fusd-core`) | Architecture | [Built] | One program; isolation from the account/write-lock layout |
| Isolated per-collateral markets | Architecture | [Built] | `Market` per collateral mint; Sealevel-parallel hot path |
| Four peg loops (redeem floor / repay-arb / RP / user-set rates) | Architecture | [Built] | All four wired & litesvm-tested; dynamic base-rate fee [Planned] |
| `fusd-math` (WAD/RAY, U256, ray_pow, P/S, redist, rate-bucket) | Architecture | [Built] | Compiled-in crate; **113 host tests** + 25 Kani harnesses, SBF-clean |
| `fusd-oracle` (validation + TWAP ring) | Architecture | [Built] | Aggregation/TWAP logic + the live `update_price`/`sample_twap` cranks wired to `spot` |
| WAD/RAY fixed-point discipline | Architecture | [Built] | U256 intermediates, multiply-before-divide, round-against-protocol, checked release math |
| RAD (1e45) balances | Architecture | [Partial] | Documented convention; not yet a defined constant — v1 accounting works in WAD/RAY |
| Legacy SPL FUSD mint (freeze=None, PDA mint auth) | Architecture | [Built] | Set irreversibly at init; legacy chosen to lack censorship extensions |
| `ProtocolConfig` | CDP Core | [Built] | `guardian` now consumed by `guardian_derisk` + rotatable via `set_guardian`; the inert global `emergency` flag was removed (no global kill switch, grep-verifiable) |
| `Market` (accounting fields) | CDP Core | [Built] | BOLD weighted-debt-sum aggregates; 4-term vault buckets; on-chain vault-sufficiency assert |
| `Position` | CDP Core | [Built] | `user_rate_bps` drives **both** bucket placement and per-position accrual |
| Per-position interest (BOLD weighted-debt-sum) | CDP Core | [Built] | each borrower accrues at its own rate in O(1); minted to the insurance buffer |
| Interest accrual (aggregate-ceil / position-floor) | CDP Core | [Built] | solvency-by-rounding, Kani-proven + B8-fuzzed |
| Conservative rounding (`fusd-math`) | CDP Core | [Built] | debt up, collateral down; exact 256-bit intermediates |
| `init_protocol` | CDP Core | [Built] | mint freeze=None, mint auth=PDA, legacy SPL Token; two-step `gov_authority` rotation |
| `init_market` | CDP Core | [Built] | legacy-SPL-only collateral gate (T22 structurally rejected) + relational config bounds |
| `open_position` / `deposit` / `withdraw` | CDP Core | [Built] | bond posting/top-up; MCR check on debt-bearing withdraw |
| `borrow` / `repay` | CDP Core | [Built] | MCR + debt-ceiling enforced; only mint/burn path |
| `close_position` | CDP Core | [Built] | empty-only; refunds rent + bond |
| `refresh_market` | CDP Core | [Built] | permissionless accrual |
| Cached `spot` (OSM-style) | CDP Core | [Built] | the cache field + staleness gate |
| Per-position user rates in accrual | CDP Core | [Built] | BOLD weighted-debt-sum; each borrower accrues at its own rate, minted to the buffer |
| On-chain vault-sufficiency assert (klend C1) | CDP Core | [Built] | `vault >= 4-term tracked sum`, last step of 8 collateral-moving handlers |
| `fusd-math` `reactor_pool` (P/S, scale/epoch, error-feedback, +1 margin) | Reactor Pool | [Built] | Pure math, unit-tested incl. drift & rollover |
| `ReactorPool` / `EpochToScaleToSum` / `ReactorDeposit` accounts | Reactor Pool | [Built] | Per-market, single-collateral; zero-copy 32×16 grid |
| `sp.rs` glue (`pool_state`/`write_back`/`realize`/`set_snapshot`) | Reactor Pool | [Built] | Bridges accounts ↔ math, drives realize-on-interaction |
| `init_reactor_pool` | Reactor Pool | [Built] | Gov-gated; one RP + grid + two vaults per market |
| `open_reactor_deposit` / `provide_to_reactor` / `withdraw_from_reactor` / `claim_reactor_gains` | Reactor Pool | [Built] | Full deposit lifecycle, realize-on-interaction |
| Liquidation tier-1 offset wiring (`liquidate.rs`) | Reactor Pool | [Built] | `min(debt, deposits)` offset, burn + collateral routing |
| Property/fuzz harness for P/S rollover | Reactor Pool | [Built] | B8 proptest: stateful offset sequences + depositor round-trip across scale/epoch |
| Eligibility (static MCR rule, fresh-price-gated) | Liquidation | [Built] | `is_healthy`; `OracleUnavailable`/`StalePrice`/`PositionHealthy` guards |
| Tier 1 — RP offset (`reactor_pool::offset`, P/S) | Liquidation | [Built] | O(1) burn + seize; bounded grid reverts on exhaustion |
| Tier 2 — redistribution (`l_coll`/`l_art`, `compute_stake`) | Liquidation | [Built] | Lazy-on-touch; `Σ stake == total_stakes`; protocol-favoring floor dust |
| SOL reserve bond | Liquidation | [Built] | Open-posted, re-posted on `deposit`, paid on liq, refunded on close; gov clamp |
| Collateral gas-comp | Liquidation | [Built] | Skimmed pre-split; pinned liquidator ATA; bps clamp |
| Governance setter for gas-comp (via the gate) | Liquidation | [Built] | `liq_gas_comp_bps` tunable via `queue_param_change`/`execute_param_change`; SOL bond stays init-time-only (non-retroactive) |
| 4-term `vault` invariant + on-chain assert | Liquidation | [Built] | `vault == total_collateral + surplus + coll_surplus + protocol_collateral`; asserted on-chain |
| Insurance buffer (tier-3) + un-homed→shutdown (tier-4) | Liquidation | [Built] | `InsuranceBuffer` PDA absorbs; remainder → `bad_debt`/`protocol_collateral` + shutdown; `recovery::absorb` Kani-proven |
| Value-recovery trio (`sweep_protocol_collateral`/`settle_bad_debt`/`withdraw_surplus`) | Liquidation | [Built] | recap: move protocol-owned value; burn recovered FUSD against `bad_debt` |
| No Recovery Mode | Liquidation | [Built] | By deliberate absence; static eligibility |
| Per-market `shutdown()` + `urgent_redeem` | Liquidation | [Built] | Terminal flag (TCR<SCR / oracle-failure trigger) → unordered 0-fee last-price wind-down; 13 litesvm tests |
| Net-outflow rate-limiter | Liquidation | [Built] | Leaky bucket on net FUSD issuance in `Market`; governable cap (default 0); liquidation/redemption exempt; 8 litesvm tests |
| Staleness halt + on-resume grace | Liquidation | [Built] | Staleness gate (`StalePrice`) + on-resume grace (`liq_grace_until`, armed by `commit_fresh_spot`, `LiquidationGracePeriod`, ~5 min); gates `liquidate` only; 7 litesvm tests + prod-crank arming + adversarially reviewed |
| CCR borrow-restriction band | Liquidation | [Built] | Blocks borrow+withdraw when TCR < `ccr_bps` (governable, default 0); never expands liquidation; fail-open on stale; 8 litesvm tests |
| Oracle-divergence gate | Liquidation | [Built] | `MarketOracle.liq_max_divergence_bps` (0=off) arms `Market.liq_divergence_until` in `update_price`; `liquidate` pauses (`OracleDivergent`) with a post-convergence grace; redeem/repay never gate; litesvm-tested |
| `redeem` (find-first-set, realize, CR-sort, face-value, bad-debt cap, fee→surplus) | Redemption | [Built] | `instructions/redeem.rs`; 6 litesvm tests; critical realize bug fixed in review |
| `fusd-math::rate_bucket` (bitmap math + CR-compare) | Redemption | [Built] | unit + B8 proptest (`cmp_collateral_ratio` order laws + bitmap model); `U256` cross-multiply |
| `RedemptionBitmap` (zero-copy words + counts + zombie pen) | Redemption | [Built] | `[u64;4]` + `[u32;256]` + `zombie_count`, per market |
| `bucket.rs` membership (zombie-aware `reconcile`) | Redemption | [Built] | one reconcile entry point; threaded through every touch instruction |
| `adjust_rate` + BOLD premature-rate fee/cooldown | Redemption | [Built] | bucket move on rate change + the anti-gaming fee |
| Price- & oracle-independent ordering | Redemption | [Built] | bucket key = `user_rate` only; survives oracle freeze |
| `min_debt` dust floor + zombie pen | Redemption | [Built] | drained/dust positions parked out of ordering, can't wedge the floor |
| `MAX_REDEMPTION_CANDIDATES` cap + closed-candidate skip | Redemption | [Built] | =20, the >64-account DoS guard; closed candidates skipped not reverted |
| Concurrent bitmap-flip safety | Redemption | [Partial] | serialized by the `Market` write-lock; B8 proptest model built; concurrent-flip Certora pending |
| Dynamic base-rate fee | Redemption | [Planned] | would replace the flat clamped fee |
| Shutdown 0-fee urgent redemption | Redemption | [Built] | `urgent_redeem`, unordered per-market wind-down |
| `PriceView` / `conf_ratio_bps` / `is_stale` | Oracle | [Built] | Oracle-agnostic normalized view; host-tested |
| `collateral_price` / `debt_price` (asymmetric `price ∓ k·σ`) | Oracle | [Built] | `collateral ≤ debt` invariant grid-tested |
| `aggregate` (Pyth + Switchboard + TWAP corridor) | Oracle | [Built] | Always returns a price; `Ok` vs `MintFrozen` |
| `OracleMode` / freeze semantics | Oracle | [Built] | Degraded feeds freeze new mints only |
| `evaluate_mode` (single-feed rule) | Oracle | [Built] | Stale / wide-conf → freeze |
| `ObservationRing` / `twap` (DEX-TWAP ring math) | Oracle | [Built] | Refuses-not-extrapolates; Mango-spike bound tested |
| `MarketOracle` (feed bindings + clamped thresholds) | Oracle | [Built] | PDA `[b"oracle", mint]` |
| `DexTwap` (zero-copy ring, Pod-cast mirror) | Oracle | [Built] | Account built (size assert + round-trip test guard the cast); written by `sample_twap` |
| `oracle_scale` (px_to_ray / usd_ray_to_spot / sqrt_price_q64_to_ray) | Oracle | [Built] | `fusd-math`; host-tested incl. verified Whirlpool decode proof |
| `clmm` (Orca/Raydium pool byte-parser) | Oracle | [Built] | `fusd-core`; 5 host parse tests; guard set per `clmm-pool-layouts.md` |
| `init_market_oracle` | Oracle | [Built] | Gov-gated; clamps all thresholds; binds quote-mint decimals; inits ring |
| Pyth / Switchboard SDK deps | Oracle | [Built] | Pinned (`1.2.0` / `0.12.1`); wired via `UncheckedAccount` + manual parse |
| `dev_set_price` (dev-only `spot` setter) | Oracle | [Built] | Feature `dev-oracle`; never in mainnet builds |
| `update_price` (live parse → `aggregate` → `spot` + `mint_frozen`) | Oracle | [Built] | The real oracle crank; cache advances only on a fresh, nonzero price (anti-replay) |
| `sample_twap` (permissionless CLMM sampler) | Oracle | [Built] | Full guard set; usd_ray scale + invert; 29 litesvm crank tests cover both |
| `Market.mint_frozen` (mint-freeze gate) | Oracle | [Built] | borrow reverts; repay/redeem/liquidate ignore it (repay/redeem keep `spot`; liquidate prices off the HIGH `debt_spot`) |
| Asymmetric `debt_price` for liquidation (`Market.debt_spot`) | Oracle | [Built] | liquidation eligibility + seize conversion price off the HIGH `debt_spot` (`price + k·σ`); borrow/withdraw/redeem keep the LOW `spot`; litesvm-tested |
| On-resume liquidation grace window | Oracle | [Built] | Solana-halt breaker — `liq_grace_until` armed on stall→resume by `Market::commit_fresh_spot`; `liquidate` requires `slot >= liq_grace_until` (`LIQ_RESUME_GRACE_SLOTS` ≈ 5 min) |
| Oracle program IDs as bounded-updatable in `ProtocolConfig` | Oracle | [Planned] | hardcoded for v1; eases Pyth's ~2026-07-31 core migration |
| Compile-time parameter clamps (`constants.rs`) | Governance | [Built] | enforced at init by `init_market` / `init_market_oracle`; reject with `ParamOutOfBounds` |
| Market params (the eleven `MarketParam`s) | Governance | [Built] | `Mcr`/`DebtCeiling`/`RedemptionFee`/`LiqGasComp`/`RateLimitCap`/`Ccr`/`LiqBonus`/`MinDebt`/`RateAdjustCooldown`/`KeeperReward`/`BorrowFee` gate-tunable via timelock; reserve/bucket-width/scr init-time-only (non-retroactive) |
| Config relational bounds + forensic trail (klend C2/C5/C8) | Governance | [Built] | `validate_market_config` (collar/RP-solvency/`mcr≥scr`) at init+queue+execute; `prev_value` event; MarketParam append-only tombstone |
| Two-step authority handoffs (gate inbound + `gov_authority`) | Governance | [Built] | propose → successor signs to accept; a typo'd key can't strand the role |
| MCR-raise liquidation grace (klend C7) | Governance | [Built] | an executed MCR raise arms `liq_grace_until`, machine-enforcing the exit window even at timelock 0 |
| Oracle params (conf, deviation, TWAP corridor, age, k, window, samples, staleness) | Governance | [Built] | init-time-only; per-market `MarketOracle`; defaults are placeholders |
| `gov_authority` / `guardian` fields | Governance | [Built] | stored in `ProtocolConfig`; gate the init-time setters today |
| `GovernanceGate` PDA + migratable inbound authority | Governance | [Built] | sole market-param setter authority; `migrate_inbound_authority` repoints it; 7 litesvm tests |
| Bounded param setters (`queue_param_change` / `execute_param_change` / `cancel_param_change`) | Governance | [Built] | queue (clamped) → delay → permissionless execute; replaces the immediate setter |
| FUSD-owned timelock (`TimelockedParam`) | Governance | [Built] | Squads is threshold-1/`time_lock=0`; FUSD enforces its own delay (queue→eta→execute) |
| `RiskParamRegistry` (broader param set) | Governance | [Planned] | gate applies the eleven `MarketParam`s directly; oracle thresholds + `scr_bps` still init-time-only |
| De-risk-only guardian (`guardian_derisk` / `set_guardian`) | Governance | [Built] | per-market auto-expiring pause of NEW debt only; gov-gated rotation; never seize/freeze/mint; 9 litesvm tests |
| MetaDAO futarchy wiring (Branch a, localnet PoC) | Governance | [Planned] | direct external CPI confirmed; PoC gates the gate code |
| SCR (shutdown ratio) | Governance | [Built] | `Market.scr_bps` (default 110%, clamp constants exist; init-time-only) |
| CCR (band threshold) | Governance | [Built] | `Market.ccr_bps` (`MarketParam`, default 0; clamp 100%–300%) |
| Rate min/max, redemption base-rate, auto-line | Governance | [Planned] | no clamp constants yet; pending simulation + BOLD-audit pin |
| FUSD mint: freeze=None, PDA mint authority, 6 decimals, legacy SPL | Security | [Built] | `init_protocol`; freeze authority irreversibly absent |
| No admin freeze/seize/pause-of-user-funds instruction | Security | [Built] | Absence is the guarantee; `guardian_derisk` pauses only NEW borrowing and `shutdown` only opens `urgent_redeem` — neither touches user funds |
| Permissionless `liquidate` / `redeem` (no whitelist) | Security | [Built] | Any sub-MCR position liquidatable; redeem callable by any FUSD holder |
| Hard per-vault solvency invariant (`debt ≤ ink·spot / MCR`) | Security | [Built] | `cdp::is_healthy`; always enforced, oracle-priced, rounds against protocol |
| Conservation invariants (`vault == total_collateral + surplus`; `Σstake == total_stakes`; aggregates ≥ Σ positions) | Security | [Built] | Asserted across litesvm redistribution/redemption/reserve suites |
| Protocol-favoring floor dust | Security | [Built] | Fuzzed clean 2M+ trials; aggregates never less than Σ positions |
| Adversarial multi-agent review (per subsystem) | Security | [Built] | Fixed redeem-skips-realize (CRITICAL) + reuse-bond gap (HIGH) |
| Governance bounded / non-retroactive / can't mint-move-freeze-seize | Security | [Partial] | `GovernanceGate` + timelock + `guardian_derisk`/`set_guardian` [Built] (clamps enforced at queue+execute; market-param scope); `RiskParamRegistry` [Planned] |
| Oracle degradation freezes new mints only | Security | [Built] | live `update_price`/`sample_twap` cranks + the invariant-5 pause matrix (mint-freeze on disagreement; staleness pauses price-consuming paths) |
| Event-CPI transport (klend C12) | Security | [Built] | all events on `#[event_cpi]` self-CPI — survive RPC log truncation (the BadDebt pager) |
| Insurance-buffer recapitalization (never reflexive dilution) | Security | [Built] | `InsuranceBuffer` + absorb/haircut + value-recovery trio; funding-source policy open |
| Global-supply auditability (sharded counters + reconciliation crank) | Security | [Planned] | Per-market `agg_art` shards exist; reconciliation crank not built |
| Front-end address screening (Chainalysis / TRM / Elliptic) | Compliance | [Planned] | Periphery; off-chain at hosted UIs; "screenable-but-unfreezable" |
| Geofencing + Terms of Service (hosted front-ends) | Compliance | [Planned] | Periphery; standard operator hygiene |
| Collateral-onboarding gate (asset curation) | Compliance | [Built] | legacy-SPL-only locked: T22 structurally rejected by Anchor typing + freeze-authority reject (T22-rejection regression) |
| Proof-of-reserves + third-party audit / formal verification | Compliance | [Planned] | Only internal adversarial review today; table stakes pre-launch |
| Permissioned / KYC'd institutional venue (Aave-Arc pattern) | Compliance | [Planned] | Phase 4+; segregated, opt-in; never gates the core |
| Legal entity / "software, not an MSB/issuer" positioning | Compliance | [Planned] | Social/legal; highest-leverage; no team-run flagship front-end |
| Protocol-level freeze / denylist / clawback | Compliance | [Rejected by design] | Would forfeit the thesis and the *Van Loon* shield; closed at every layer |
| `#[program]` instruction surface (46 prod) | Reference/Build | [Built] | Verified against `lib.rs`; production IDL has no `dev_set_price` |
| `dev_set_price` (`dev-oracle`) | Reference/Build | [Built] (dev-gated) | Excluded from production IDL/`.so` |
| `update_price` / `sample_twap` | Reference/Build | [Built] | Live feed parsing + CLMM sampler; the borrow blocker cleared |
| Governance + recap + guardian instructions (in `lib.rs`) | Reference/Build | [Built] | gate/timelock, two-step authorities, guardian, shutdown, value-recovery trio, buffer, `claim_coll_surplus` — all [Built] |
| `ProtocolConfig`, `Market`, `Position` | Reference/Build | [Built] | Core hot-path accounts (Borsh) |
| `ReactorPool`, `EpochToScaleToSum`, `ReactorDeposit` | Reference/Build | [Built] | RP P/S accounting; grid is zero-copy |
| `RedemptionBitmap` | Reference/Build | [Built] | Zero-copy `[u64;4]` + `[u32;256]` + zombie pen |
| `MarketOracle` | Reference/Build | [Built] | Read by `update_price`/`sample_twap` |
| `DexTwap` ring | Reference/Build | [Built] | Written by `sample_twap` |
| `GovernanceGate`, `TimelockedParam`, `InsuranceBuffer` | Reference/Build | [Built] | Gate + per-op timelock + per-market buffer |
| `GlobalBackstopReserve` + `init_global_backstop`/`fund_backstop`/`withdraw_backstop_excess` + global timelock (`queue/execute/cancel_global_param_change`, `GlobalParam`/`TimelockedGlobalParam`) | Reference/Build | [Built] | Bounded shared second-loss fUSD reserve drawn as liquidation tier-3.5 (after the local buffer, before un-homed bad debt); ships inert (every param 0/off); litesvm-tested |
| `RiskParamRegistry`, `RateLimiter` (auto-line account) | Reference/Build | [Planned] | Seeds reserved in `constants.rs` (the net-outflow limiter itself is [Built] on `Market`) |
| Account layout discipline (klend C10) | Reference/Build | [Built] | const-assert size pins + Borsh SPACE-pin test; pre-launch reserve widening |
| Toolchain + SBF pins | Reference/Build | [Built] | Anchor 0.32.1 / Solana 2.3 / platform-tools 1.84.1 |
| Test architecture (litesvm + crate units) | Reference/Build | [Built] | ~400 tests (~230 litesvm + ~165 host incl. 113 `fusd-math`); isolated `integration-tests` crate |
| Release gates (3 scripts) | Reference/Build | [Built] | verifiable-build, check-no-dev-oracle, check-stack-offsets |
| Phase 1 core | Reference/Build | [Built] | Functionally complete; launch wiring Planned |
| Phases 2–4 | Reference/Build | [Planned] | Hardening → governance-min → immutability |

---

## 13. Document Conventions & Caveats

A few standing caveats apply to everything above:

- **[Planned] items are designs, not guarantees.** Anything tagged [Planned] (and the design-only half of anything [Partial]) describes intended behavior that is not yet implemented on-chain. It reflects current intent and may change as the remaining subsystems are built and reviewed; do not read it as a commitment or as code that exists today. Where a built component and its eventual design diverge, the built behavior is authoritative.
- **Bounded-parameter defaults are placeholders pending backtesting.** The default values for governable risk parameters — oracle thresholds (`max_conf_bps`, `max_deviation_bps`, `twap_max_divergence_bps`, `max_age_secs`, `k_bps`, the TWAP window/samples/staleness), the liquidation incentives, bucket width, and the redemption fee — are deliberately conservative placeholders. They will be re-derived from a fast-crash simulation / backtesting pass before launch. The *clamps* are hard program constants; only the *defaults within them* are provisional.
- **The redemption and oracle mechanisms still owe fuzz + Certora before launch.** The rate-keyed redemption bitmap is novel on Solana — especially the concurrent "last member leaves a bucket" flip, today serialized only by the per-market `Market` write-lock — and the oracle aggregation/validation path are the two designated gating correctness experiments. Both owe bespoke fuzzing and Certora formal verification before any mainnet launch, regardless of their current [Built] tags on the happy path.
