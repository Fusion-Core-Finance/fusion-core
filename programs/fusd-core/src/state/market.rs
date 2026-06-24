use anchor_lang::prelude::*;

use crate::constants::{LIQ_RESUME_GRACE_SLOTS, MAX_PRICE_STALENESS_SLOTS};

/// Per-collateral isolated market. PDA `[b"market", collateral_mint]`.
///
/// A hot account in the per-collateral write lane (fusion-docs.md). Migrates to
/// `#[account(zero_copy)]` when the redemption rate-bucket bitmap and Reactor
/// Pool land. Debt accounting is the Liquity-v2 / BOLD **weighted-debt-sum** model:
/// each position carries its own `user_rate_bps`, and the market
/// accrues interest in O(1) off the two aggregates below.
#[account]
#[derive(Debug)]
pub struct Market {
    pub collateral_mint: Pubkey,
    /// Program-owned escrow token account holding this market's collateral.
    pub collateral_vault: Pubkey,
    /// Σ recorded (present-value) debt across the market's positions, in fUSD-native units. Kept exact
    /// at every touch (the aggregate interest folds into it in `accrual::accrue`; per-position
    /// realizations only move debt between positions, never the total). BOLD `aggRecordedDebt`.
    pub agg_recorded_debt: u128,
    /// Σ `recorded_debt_i · user_rate_bps_i` (bps scale) — the weighted-debt sum that makes interest
    /// accrual O(1): pending interest over `dt` = `agg_weighted_debt_sum · dt / (SECONDS_PER_YEAR ·
    /// 10_000)`. Maintained by an add-then-subtract delta on every position touch. BOLD
    /// `aggWeightedDebtSum`; bps scale keeps it in `u128` with vast headroom.
    pub agg_weighted_debt_sum: u128,
    /// Interest folded into `agg_recorded_debt` but not yet **minted** into existence. Accumulates on
    /// every accrual touch; `refresh_market` mints it as fUSD into the insurance buffer and zeroes it
    /// (the lazy mint seam — keeps the buffer-vault + mint off the hot path). The supply invariant is
    /// `circulating fUSD == agg_recorded_debt − unminted_interest + bad_debt`.
    pub unminted_interest: u128,
    /// Unix timestamp of the last aggregate-interest accrual (BOLD `lastAggUpdateTime`).
    pub last_update_ts: i64,
    /// Cached collateral price: RAY-scaled fUSD-native per 1 native collateral unit
    /// (OSM-style cache). 0 until set. Populated by the oracle crank; until then the
    /// dev-only `dev_set_price` (feature `dev-oracle`) sets it for tests, so production
    /// builds simply cannot borrow until the real oracle is wired.
    pub spot: u128,
    pub spot_updated_slot: u64,
    /// Cached HIGH (debt) collateral price: RAY-scaled fUSD-native per 1 native collateral unit,
    /// = `price + k·σ` (the asymmetric `debt_price`). **Liquidation eligibility and the
    /// seize conversion price off this, never `spot`** (which a trunk refactor had collapsed onto):
    /// under price uncertainty a position is liquidated only when it is underwater at the OPTIMISTIC
    /// valuation, so a wide confidence band can't drive a destructive, irreversible liquidation on
    /// noise. `borrow`/`withdraw` (LTV), redemption payout, and the CCR/SCR gauges keep using the
    /// conservative LOW `spot` — pessimism is protective for *extending* or *winding down* risk,
    /// optimism is protective for *destroying* a position. Written together with `spot` by
    /// [`Market::commit_fresh_spot`] (shares its freshness clock). 0 until first priced; a market
    /// priced before this field existed reads 0 and is fail-closed un-liquidatable until re-cranked
    /// (the `0 = unset` sentinel). Carved from `_reserved`.
    pub debt_spot: u128,
    /// Minimum collateral ratio, bps (e.g. 12_000 = 120%).
    pub mcr_bps: u16,
    /// Debt ceiling in fUSD-native units.
    pub debt_ceiling: u64,
    pub collateral_decimals: u8,
    pub bump: u8,
    pub vault_bump: u8,

    // --- liquidation tier-2 redistribution (fusion-docs.md; `fusd_math::redistribution`) ---
    /// Cumulative redistributed-collateral per unit staked (1e18-scaled). Liquity `L_ETH`.
    pub l_coll: u128,
    /// Cumulative redistributed **recorded (present-value) debt** per unit staked (1e18-scaled). Liquity
    /// v2 `L_boldDebt`. (Under the weighted-debt model the redistributed quantity is recorded debt, not
    /// normalized art; the `fusd_math::redistribution` math is unit-agnostic, so only the unit changed.)
    /// Redistributed debt is **parked non-interest-bearing** here until a recipient's next touch folds it
    /// into `recorded_debt` and re-weights it at the recipient's OWN rate (BOLD).
    pub l_art: u128,
    /// Floor-division residuals carried between redistributions, so repeated liquidations don't drift.
    pub last_coll_redist_error: u128,
    pub last_art_redist_error: u128,
    /// Σ of all positions' stakes — the redistribution denominator.
    pub total_stakes: u128,
    /// Tracks the collateral vault's token balance **exactly** (the invariant the tests check).
    /// It is `>= Σ position.ink`: floor dust from redistribution stays here as protocol-favoring
    /// over-collateralization until a position realizes its share.
    pub total_collateral: u128,
    /// System snapshots captured after each liquidation, feeding the `compute_stake` ratio that
    /// keeps `Σ stake == total_stakes` exact across redistributions.
    pub total_stakes_snapshot: u128,
    pub total_collateral_snapshot: u128,

    // --- liquidation incentives (governance-adjustable within compile-time clamps) ---
    /// Per-position SOL bond posted at open, paid to the liquidator on liquidation, refunded on
    /// close. Bounded by `MAX_RESERVE_LAMPORTS`; 0 disables. Fixed per position at open-time.
    pub reserve_lamports: u64,
    /// Liquidator collateral gas-comp (bps of seized collateral), skimmed before the
    /// RP/redistribution split. Bounded by `MAX_LIQ_GAS_COMP_BPS`; 0 disables.
    pub liq_gas_comp_bps: u16,
    /// Liquidation **bonus collar** (bps): a liquidation seizes collateral worth at most
    /// `debt · (1 + liq_bonus_bps/10000)`; the surplus above that is returned to the borrower as
    /// `Position.coll_surplus`. Governance-adjustable (`MarketParam::LiqBonus`) within
    /// `[0, MAX_LIQ_BONUS_BPS]`; **0 = collar OFF** (seize the whole position).
    pub liq_bonus_bps: u16,
    /// Σ of all positions' `coll_surplus` (native collateral) — the liquidation surpluses held in the
    /// collateral vault but owed to borrowers (not backing any position). Makes the vault invariant
    /// `vault == total_collateral + surplus_collateral + total_coll_surplus + protocol_collateral`
    /// checkable from `Market` alone (proof-of-reserves). Bumped on a collared liquidation; drained by
    /// `claim_coll_surplus`.
    pub total_coll_surplus: u64,

    // --- redemption (fusion-docs.md) ---
    /// Rate-bucket width (bps) — quantizes `user_rate` into the bitmap's 256 buckets. Bounded by
    /// `MIN/MAX_BUCKET_WIDTH_BPS`.
    pub bucket_width_bps: u16,
    /// Flat redemption fee (bps), governance-adjustable within `MAX_REDEMPTION_FEE_BPS`; 0 disables.
    pub redemption_fee_bps: u16,
    /// Collateral retained from redemption fees, held in the collateral vault but not backing any
    /// position. Part of the 4-term vault invariant `vault == total_collateral + surplus_collateral
    /// + total_coll_surplus + protocol_collateral` (see `reconcile.rs`). Governance-withdrawable via
    /// `withdraw_surplus`. Surplus buffer.
    pub surplus_collateral: u64,

    /// Oracle-driven mint gate. Written by `update_price` from the `fusd_oracle`
    /// aggregate mode: `true` when the aggregate is degraded (stale / wide-conf / cross-oracle or
    /// TWAP divergence). `borrow` reverts when set; repay, liquidation, and redemption ignore it
    /// (they keep using `spot`, which stays a conservative price even when frozen). Carved from
    /// `_reserved` — no SPACE change.
    pub mint_frozen: bool,

    /// Guardian de-risk: new borrowing is paused while `now < guardian_paused_until` (unix secs).
    /// Set ONLY by `guardian_derisk` (gated on `ProtocolConfig.guardian`), clamped to at most
    /// `now + GUARDIAN_MAX_PAUSE_SECS` so it auto-lifts and can never ratchet the market shut. A
    /// pure de-risk lever — it blocks only `borrow`; repay, withdraw, liquidation, and redemption
    /// ignore it (never touches user funds or the peg floor). fusion-docs §7.2.
    pub guardian_paused_until: i64,

    /// Terminal per-market wind-down flag (fusion-docs §4.x). Set ONCE by the
    /// permissionless `shutdown` when the market is failing (TCR < `scr_bps`, or sustained oracle
    /// failure); **irreversible**. Closes `borrow` and ordered `redeem`; opens `urgent_redeem`
    /// (unordered, 0-fee, face value). Carved from `_reserved`.
    pub shutdown: bool,
    /// Shutdown collateral ratio (bps): the aggregate threshold below which `shutdown` may fire
    /// (the market-level analog of a position's `mcr_bps`). Set at `init_market` to `DEFAULT_SCR_BPS`.
    pub scr_bps: u16,

    // --- net-outflow rate limiter (leaky bucket on NET fUSD issuance; fusion-docs.md) ---
    /// Max net fUSD issuance per `RATELIMIT_WINDOW_SECS` (fUSD-native). Governable (`MarketParam`);
    /// **0 = disabled** (no limit). `borrow` consumes, `repay` restores; the bucket refills over the
    /// window. Liquidation/redemption/urgent_redeem are exempt.
    pub rl_cap: u64,
    /// Current net-outflow pressure in the bucket (fUSD-native), decayed by elapsed time on each touch.
    pub rl_accrued: u64,
    /// Unix timestamp of the last rate-limiter update (for the time-decay).
    pub rl_last_update: i64,

    /// CCR borrow-restriction band threshold (bps; fusion-docs.md). Governable (`MarketParam`);
    /// **0 = disabled**. When `> 0` and the market's aggregate TCR is below it, `borrow` and
    /// `withdraw` revert (`CcrRestricted`) — risk-increasing ops only; liquidation/redemption and
    /// de-risking ops stay open, and the band fails open on a stale price. Never expands liquidation.
    pub ccr_bps: u16,

    /// On-resume liquidation grace deadline (slot). Armed by [`Market::commit_fresh_spot`] when a
    /// fresh price recovers from a staleness halt (prior gap > `MAX_PRICE_STALENESS_SLOTS`): set to
    /// `now + LIQ_RESUME_GRACE_SLOTS`. `liquidate` additionally requires `slot >= liq_grace_until`, so
    /// borrowers who couldn't act during the outage get a window to cure before a stale-then-fresh
    /// price can cascade. 0 = no active grace (default); never re-cleared — a later halt just re-arms a
    /// later deadline. Liquidation-only: redemption / `urgent_redeem` are oracle-independent and are
    /// NEVER gated by it. fusion-docs.md.
    pub liq_grace_until: u64,

    /// On-divergence liquidation pause deadline (slot). Armed by `update_price` when a FRESH
    /// primary grossly disagrees with a PRESENT secondary (`fusd_oracle` `liq_divergent`): set to
    /// `now + LIQ_DIVERGENCE_GRACE_SLOTS`, re-armed (monotone `max`) on every divergent crank, so when
    /// convergence returns the pause self-clears one grace later (a snap-back can't instantly cascade).
    /// `liquidate` additionally requires `slot >= liq_divergence_until` — a manipulated/briefly-bad
    /// primary the secondaries visibly disagree with cannot drive liquidations. Liquidation-ONLY:
    /// redemption, `urgent_redeem`, and repay are NEVER gated by it (the peg floor must
    /// always clear). 0 = no active pause (default; the gate is off until governance sets a non-zero
    /// `MarketOracle.liq_max_divergence_bps`). Carved from `_reserved`.
    pub liq_divergence_until: u64,

    /// Realized **un-homed bad debt** (present-value fUSD): debt a liquidation could NOT extinguish
    /// because RP + redistribution + the insurance buffer were all insufficient (the waterfall's
    /// `unhomed`; fusion-docs.md). This fUSD remains in circulation with no backing debt — the
    /// loss the buffer/recapitalization must eventually cover. Accumulates as terminal liquidations
    /// occur; surfaced for proof-of-reserves. A non-zero value coincides with `shutdown` (the wind-down).
    /// NOTE: the un-homed position's collateral is retained PROTOCOL-OWNED in the market vault (it backs
    /// the wind-down) and tracked in `protocol_collateral` — so treat `bad_debt` as GROSS
    /// circulating-unbacked fUSD; the offsetting collateral is recovered via `sweep_protocol_collateral`
    /// (deployed against this loss off-chain), not double-counted as freely available.
    pub bad_debt: u128,
    /// Why the market was shut down (`SHUTDOWN_REASON_*`; 0 = not shut down). The named terminal-recovery
    /// reason — set alongside `shutdown` by `shutdown` (SCR / oracle failure) or `liquidate` (un-homed bad debt).
    pub shutdown_reason: u8,

    /// Minimum position debt (fUSD-native) — the dust floor (Liquity `MIN_NET_DEBT`). A
    /// borrow/repay must leave the position at `recorded_debt == 0` or `>= min_debt`. Governable
    /// (`MarketParam::MinDebt`) within `[0, MAX_MIN_DEBT]`; **0 = disabled**. Non-retroactive (gates only
    /// new ops, never force-closes). Prices out the lowest-bucket dust-stuffing redemption grief.
    pub min_debt: u64,

    /// Premature rate-adjustment cooldown (secs) — the BOLD anti-gaming knob. When `> 0`, an
    /// `adjust_rate` within this window of the position's last change/open is charged an upfront fee of
    /// `cooldown`-seconds of interest at the new rate. Governable (`MarketParam::RateAdjustCooldown`)
    /// within `[0, MAX_RATE_ADJUST_COOLDOWN_SECS]`; **0 = disabled**. Stops reactive rate-jumping to dodge
    /// the redemption queue.
    pub rate_adjust_cooldown_secs: i64,

    /// Keeper reward (bps) — the cut of the interest `refresh_market` mints that is paid to the cranker
    /// (rest to the buffer), the self-funding keeper incentive. Governable
    /// (`MarketParam::KeeperReward`) within `[0, MAX_KEEPER_REWARD_BPS]`; **0 = disabled**. Self-funding
    /// (a split of already-minted interest, never a fresh mint) and spam-proof. Carved from `_reserved`.
    pub keeper_reward_bps: u16,

    /// Upfront borrowing fee (bps; BOLD-sweep C7) — a one-time charge added to the position's debt at
    /// `borrow` (the primary redemption-evasion deterrent). Governable (`MarketParam::BorrowFee`)
    /// within `[0, MAX_BORROW_FEE_BPS]`; **0 = disabled** (default). The fee is NOT minted to the
    /// borrower: debt grows by `amount + fee`, only `amount` is minted, and `fee` is booked into
    /// `unminted_interest` so `refresh_market` mints it to the buffer (funds first-loss capital like
    /// accrued interest — supply identity preserved). Carved from `_reserved`.
    pub borrow_fee_bps: u16,

    /// Auto bad-debt paydown rate (bps of post-keeper interest; BOLD-sweep C16). When `bad_debt > 0`,
    /// `refresh_market` diverts this fraction of the interest it would mint to the buffer to retire
    /// `bad_debt` instead (automatic recapitalization-from-revenue; supply-preserving — the diverted
    /// slice is not minted while `bad_debt` drops by the same amount). Governable
    /// (`MarketParam::BadDebtPaydown`) within `[0, MAX_BAD_DEBT_PAYDOWN_BPS]`; **0 = disabled**
    /// (default). Carved from `_reserved`.
    pub bad_debt_paydown_bps: u16,

    /// Dynamic redemption base-rate (RAY-scaled fraction; BOLD-sweep C9). The decaying volume-spike
    /// component of the redemption fee: each `redeem` decays it toward 0 (6h half-life) then bumps it
    /// by `(redeemed / market debt) / BETA`, and the effective fee is
    /// `clamp(redemption_fee_bps + min(base_rate_bps, redemption_base_rate_max_bps), floor, MAX)`.
    /// Only `redeem` reads/writes it (never `urgent_redeem`, which is 0-fee). `0` when quiet/disabled.
    /// Carved from `_reserved`.
    pub redemption_base_rate: u128,
    /// Unix-ts of the last `redemption_base_rate` update (the decay anchor). Distinct from
    /// `last_update_ts` (interest accrual), which moves on every touch. Carved from `_reserved`.
    pub redemption_base_rate_ts: i64,
    /// Governable cap (bps) on the dynamic base-rate's ADD over the flat fee floor (C9);
    /// **0 = the dynamic component is DISABLED** (flat-fee-only, pre-C9 behavior). Clamped
    /// (`MarketParam::RedemptionBaseRateMax`) to `<= MAX_REDEMPTION_BASE_RATE_BPS`. Carved from `_reserved`.
    pub redemption_base_rate_max_bps: u16,

    /// Retained PROTOCOL-OWNED collateral (native) — the un-homed liquidation remainder (`coll_r`) that
    /// had no redistribution recipient, so it sits in the collateral vault backing NO position.
    /// Tracked separately from `total_collateral` (which only ever backs live positions + redistribution
    /// dust) so the vault invariant stays exact and the amount is recoverable in O(1): `bad_debt`'s
    /// offsetting collateral. Bumped on a tier-3/4 liquidation; drained by `sweep_protocol_collateral`
    /// (governance, deploys it against `bad_debt`).
    pub protocol_collateral: u64,

    /// Cumulative fUSD this market has CONTRIBUTED to the Global Backstop Reserve (the interest cut).
    /// A *local* write (this market's own lane — parallelism-safe),
    /// it feeds the contribution-weighted arm of the per-market draw cap. Observability + fairness.
    pub global_contributed: u128,
    /// Cumulative fUSD this market has DRAWN from the Global Backstop Reserve (tier-3.5 absorbs). Enforces
    /// the per-market draw cap across repeated draws (a market in slow decline can't re-draw past its bound).
    pub global_drawn: u128,

    /// Forward-compat reserve (RP pointers, oracle refinements, ...). WIDENED 4 → 64 bytes
    /// pre-launch: the carve-from-`_reserved` additive-upgrade path (used for
    /// `mint_frozen`, `shutdown`, `keeper_reward_bps`) must survive the upgradeable Phases 1–3 —
    /// post-launch a Borsh account cannot grow without realloc, so headroom is free now and
    /// impossible later. Carve new fields from the HEAD of this reserve; old accounts' zeroed
    /// bytes must decode as the new field's documented `0 = disabled/none` sentinel.
    pub _reserved: [u8; 10],
}

impl Market {
    /// Commit a fresh asymmetric collateral price pair, advancing the staleness clock — the single
    /// freshness-clock writer shared by `update_price` (prod) and `dev_set_price` (test), so both have
    /// identical staleness + on-resume-grace semantics. `spot` is the LOW (collateral) price; `debt_spot`
    /// is the HIGH (debt/liquidation) price. Caller must pass a positive `spot` (a 0 price
    /// would brick the liquidation/redemption gates that require `spot > 0`); `debt_spot >= spot` by
    /// construction (it is `price + k·σ` vs `price − k·σ`).
    ///
    /// If the prior cached price had gone stale (gap > `MAX_PRICE_STALENESS_SLOTS` — a halt), this
    /// ALSO arms the on-resume liquidation grace window (`liq_grace_until = now + LIQ_RESUME_GRACE_SLOTS`).
    /// The `self.spot > 0` guard skips arming on the very first price a market ever receives (genesis,
    /// where the huge slot gap is not a "resume"). fusion-docs.md.
    ///
    /// MONOTONE `max`: `liq_grace_until` has two writers (this resume path and the governance
    /// MCR-raise arming in `execute_param_change`), so each writer may only EXTEND the window —
    /// an oracle halt-resume during an active MCR-raise grace must not truncate the longer window
    /// down to `now + LIQ_RESUME_GRACE_SLOTS` (cross-writer shortening hole).
    pub fn commit_fresh_spot(&mut self, spot: u128, debt_spot: u128, now_slot: u64) {
        if self.spot > 0 && now_slot.saturating_sub(self.spot_updated_slot) > MAX_PRICE_STALENESS_SLOTS
        {
            self.liq_grace_until =
                self.liq_grace_until.max(now_slot.saturating_add(LIQ_RESUME_GRACE_SLOTS));
        }
        self.spot = spot;
        self.debt_spot = debt_spot;
        self.spot_updated_slot = now_slot;
    }
}

impl Market {
    pub const SPACE: usize = 8      // discriminator
        + 32 + 32                   // collateral_mint, collateral_vault
        + 16 + 16 + 16              // agg_recorded_debt, agg_weighted_debt_sum, unminted_interest
        + 8                         // last_update_ts
        + 16 + 8 + 16               // spot, spot_updated_slot, debt_spot
        + 2 + 8 + 1                 // mcr_bps, debt_ceiling, collateral_decimals
        + 1 + 1                     // bump, vault_bump
        + 16 * 8                    // redistribution: l_coll, l_art, 2 errors, total_stakes,
                                    //   total_collateral, total_stakes_snapshot, total_collateral_snapshot
        + 8 + 2                     // reserve_lamports, liq_gas_comp_bps
        + 2 + 8                     // liq_bonus_bps, total_coll_surplus
        + 2 + 2 + 8                 // bucket_width_bps, redemption_fee_bps, surplus_collateral
        + 1                         // mint_frozen
        + 8                         // guardian_paused_until
        + 1 + 2                     // shutdown, scr_bps
        + 8 + 8 + 8                 // rl_cap, rl_accrued, rl_last_update
        + 2                         // ccr_bps
        + 8                         // liq_grace_until
        + 8                         // liq_divergence_until
        + 16 + 1                    // bad_debt, shutdown_reason
        + 8 + 8                     // min_debt, rate_adjust_cooldown_secs
        + 2                         // keeper_reward_bps (carved from the former 6-byte reserve)
        + 2                         // borrow_fee_bps (C7 — carved from _reserved)
        + 2                         // bad_debt_paydown_bps (C16 — carved from _reserved)
        + 16 + 8 + 2                // redemption_base_rate, _ts, _max_bps (C9 — carved from _reserved)
        + 8                         // protocol_collateral (un-homed retained collateral)
        + 16 + 16                   // global_contributed, global_drawn (global backstop; widen, not carve)
        + 10; // reserved (base 64 − 24 debt_spot+liq_divergence_until − 2 borrow_fee − 2 bad_debt_paydown
              // − 26 C9 base-rate fields; the +32 global-backstop fields are a widen, so net SPACE = base + 32)
}
