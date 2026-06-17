//! PDA seeds and (eventually) the compile-time governance clamps.
//! Keeping seeds in one place avoids drift between instructions and the SDK.
//! See fusion-docs.md (account table and bounded params).

use anchor_lang::prelude::{pubkey, Pubkey};

/// `[b"config"]`
pub const CONFIG_SEED: &[u8] = b"config";
/// `[b"market", collateral_mint]`
pub const MARKET_SEED: &[u8] = b"market";
/// `[b"position", collateral_mint, owner]`
pub const POSITION_SEED: &[u8] = b"position";
/// `[b"reactor", collateral_mint]`
pub const REACTOR_POOL_SEED: &[u8] = b"reactor";
/// `[b"reactor_dep", collateral_mint, owner]`
pub const REACTOR_DEPOSIT_SEED: &[u8] = b"reactor_dep";
/// `[b"reactor_fusd", collateral_mint]` — the RP's fUSD deposit vault (authority = RP PDA).
pub const REACTOR_FUSD_VAULT_SEED: &[u8] = b"reactor_fusd";
/// `[b"reactor_coll", collateral_mint]` — the RP's seized-collateral vault (authority = RP PDA).
pub const REACTOR_COLL_VAULT_SEED: &[u8] = b"reactor_coll";
/// `[b"ess", collateral_mint]` — the bounded epoch→scale→sum grid (zero-copy).
pub const ESS_SEED: &[u8] = b"ess";
/// `[b"buffer", collateral_mint]` — the per-market insurance buffer (protocol first-loss fUSD reserve;
/// the third loss-absorption tier, fusion-docs.md). Authority of its fUSD vault.
pub const BUFFER_SEED: &[u8] = b"buffer";
/// `[b"buffer_fusd", collateral_mint]` — the insurance buffer's fUSD reserve vault (authority = buffer PDA).
pub const BUFFER_FUSD_VAULT_SEED: &[u8] = b"buffer_fusd";

/// Bounded `EpochToScaleToSum` grid dimensions (milestone sizing; production tunes larger
/// via client pre-allocation + the `zero` constraint). `scale` is the stride.
pub const REACTOR_MAX_EPOCHS: u64 = 32;
pub const REACTOR_MAX_SCALES: u64 = 16;
pub const REACTOR_GRID_LEN: usize = (REACTOR_MAX_EPOCHS * REACTOR_MAX_SCALES) as usize; // 512 u128 = 8 KiB
/// `[b"twap", collateral_mint]`
pub const DEX_TWAP_SEED: &[u8] = b"twap";
/// `[b"coll_vault", collateral_mint]` — program-owned escrow holding a market's collateral.
pub const COLLATERAL_VAULT_SEED: &[u8] = b"coll_vault";
/// `[b"ratelimit", collateral_mint]`
pub const RATE_LIMITER_SEED: &[u8] = b"ratelimit";
/// `[b"registry"]`
pub const REGISTRY_SEED: &[u8] = b"registry";
/// `[b"gov_gate"]`
pub const GOV_GATE_SEED: &[u8] = b"gov_gate";
/// `[b"timelock"]`
pub const TIMELOCK_SEED: &[u8] = b"timelock";
/// `[b"gtimelock"]` — a queued GLOBAL (backstop) param change (distinct from per-market `TIMELOCK_SEED`).
pub const GLOBAL_TIMELOCK_SEED: &[u8] = b"gtimelock";
/// `[b"backstop"]` — the system-wide Global Backstop Reserve (second-loss capital).
pub const BACKSTOP_SEED: &[u8] = b"backstop";
/// `[b"backstop_fusd"]` — the global backstop's fUSD reserve vault (authority = the backstop PDA).
pub const BACKSTOP_FUSD_VAULT_SEED: &[u8] = b"backstop_fusd";
/// `[b"mint_authority"]` — the only signer that may mint/burn fUSD.
pub const MINT_AUTHORITY_SEED: &[u8] = b"mint_authority";
/// `[b"fusd_mint"]` — the fUSD mint PDA (freeze authority None).
pub const FUSD_MINT_SEED: &[u8] = b"fusd_mint";

/// Max age (slots) of the cached collateral price accepted when taking on / holding debt.
/// Placeholder until the real oracle defines per-collateral staleness; ~100s at 400ms.
///
/// PER-INSTRUCTION ORACLE REQUIREMENT TIERS (the per-instruction oracle-requirement policy table;
/// the klend `PriceStatusFlags` tiers, expressed as Fusion's three gate sets and
/// pinned end-to-end by `integration-tests/tests/litesvm_oracle_matrix.rs`):
///
/// | Tier        | Requirement                                        | Instructions (gate site) |
/// |-------------|----------------------------------------------------|---------------------------|
/// | ALL         | live market + spot>0 + !mint_frozen + !guardian    | `borrow` (borrow.rs ~68-75; |
/// |             | + fresh cache (+ CCR/limiter when enabled)         |  the ONLY mint path)      |
/// | FRESH-ONLY  | spot>0 + cache within MAX_PRICE_STALENESS_SLOTS;   | `liquidate` (liquidate.rs ~85-98, |
/// |             | deliberately IGNORES mint_frozen — disagreement    |  + liq_grace_until), ordered `redeem` |
/// |             | freezes mints only, these survive a degraded       |  (redeem.rs ~60-72, + !shutdown), |
/// |             | secondary                                          | debt-bearing `withdraw` (withdraw.rs ~62-75), |
/// |             |                                                    | the premature-fee branch of `adjust_rate` |
/// |             |                                                    | (adjust_rate.rs) |
/// | NONE        | no oracle state read at all                        | `repay`, pure `deposit`, zero-debt |
/// |             |                                                    | `withdraw`, no-fee `adjust_rate`, RP ops, |
/// |             |                                                    | `claim_coll_surplus`, `close_position` |
/// | (special)   | shutdown==true + spot>0, NO staleness gate (the    | `urgent_redeem` (urgent_redeem.rs ~58-72) |
/// |             | wind-down must proceed on a dead oracle)           |                           |
///
/// Inverted strictness is structurally impossible: the freshness clock advances on a strictly
/// weaker condition (any fresh feed) than mint_allowed (full agreement), so a divergence can
/// never gate FRESH-ONLY paths harder than the ALL path. Redemption ORDERING never reads the
/// oracle (bucket key = borrower user_rate); only the payout price does.
pub const MAX_PRICE_STALENESS_SLOTS: u64 = 250;

/// On-resume liquidation grace window (slots). After the cached price recovers from a staleness halt
/// (the prior gap exceeded `MAX_PRICE_STALENESS_SLOTS` — a Solana halt or a sustained feed outage),
/// `liquidate` stays paused for this many slots. Borrowers who could not act while the chain/feed was
/// down get a fair window to cure before a stale-then-fresh price can trigger a liquidation cascade at
/// the resume trough (fusion-docs.md; the Solana-halt breaker, mirroring the Chainlink-L2
/// sequencer-uptime-feed grace pattern). ~5 min at 400 ms/slot — 3× the staleness gate. Placeholder
/// pending fast-crash-sim + futarchy calibration: it trades borrower fairness against the bad debt
/// that can accrue while genuinely-underwater positions wait out the window.
pub const LIQ_RESUME_GRACE_SLOTS: u64 = 750;

/// Liquidation grace window armed when governance RAISES a market's MCR (slots). An executed MCR
/// raise instantly expands the liquidatable set over LIVE positions — the retroactive-worsening
/// vector hard invariant 4 forbids — and with `MIN_GOV_TIMELOCK_SECS = 0` (permitted for guarded
/// launch) the timelock alone gives no machine-enforced cure window. So `execute_param_change`
/// arms `liq_grace_until = max(current, now + this)` on a raise: borrowers get a
/// window to cure at the OLD eligibility before the new MCR bites; `liquidate` is the ONLY reader
/// (never redeem/shutdown/urgent_redeem — gating those would breach invariant 5 / remove the
/// backstop). Deliberately longer than `LIQ_RESUME_GRACE_SLOTS` (~1h vs ~5min): a policy change
/// borrowers must react to, not an outage recovery. Compile-time constant, NEVER governance-
/// settable (a tunable grace would be a discretionary liquidation-pause knob). Placeholder pending
/// the same fast-crash-sim calibration as the other grace constants.
pub const MCR_RAISE_GRACE_SLOTS: u64 = 9_000;

/// On-divergence liquidation pause grace window (slots). When `update_price` observes a
/// FRESH primary grossly disagreeing with a PRESENT secondary, `liquidate` is paused until
/// `now + this`, re-armed (monotone `max`) on every divergent crank — so when the feeds re-converge,
/// liquidation stays paused for this grace beyond the LAST divergent observation, and a divergent→
/// converged snap-back can't instantly cascade. Mirrors the `LIQ_RESUME_GRACE_SLOTS` on-resume
/// pattern (~5 min at 400 ms/slot). Liquidation-ONLY (never redeem/urgent_redeem/repay).
/// Compile-time, NOT governance-settable (a tunable pause would be a discretionary liquidation knob);
/// the per-market `MarketOracle.liq_max_divergence_bps` (0 = off) is what enables/calibrates the gate.
/// Placeholder pending the same fast-crash-sim calibration as the other grace constants.
pub const LIQ_DIVERGENCE_GRACE_SLOTS: u64 = 750;

// --- Governance timelock (the fUSD-owned two-speed; Squads runs time_lock=0) -----------------
// The `GovernanceGate`'s inbound authority QUEUES a clamped param change; after `timelock_secs`
// elapse, ANYONE may EXECUTE it (permissionless execution after the delay). The delay is itself a
// bounded gov param, fixed within these clamps at gate init: it can never be set to lock changes
// forever (MAX bounds it), and the queue→execute split gives users an exit window before any
// change lands (the non-retroactivity intent). See fusion-docs.md. An earlier PoC proved the
// MetaDAO→Squads→fUSD path that QUEUES through this gate.
pub const MIN_GOV_TIMELOCK_SECS: i64 = 0; // 0 permitted for a trusted guarded launch / tests
pub const MAX_GOV_TIMELOCK_SECS: i64 = 2_592_000; // 30 days — bounds the maximum enforced delay
pub const DEFAULT_GOV_TIMELOCK_SECS: i64 = 172_800; // 48h recommended for production

// --- Guardian de-risk (the independent emergency brake; fusion-docs §7.2) ----
// `guardian_derisk` (gated on `ProtocolConfig.guardian`, INDEPENDENT of futarchy/Squads) pauses NEW
// borrowing on a market for at most this long, then it auto-lifts. The single power is the harmless
// borrow pause — it never touches existing positions, user funds, repay, liquidation, or redemption.
// The cap is the "buys time, not control" bound: ≥ the 48h gov timelock so governance can coordinate
// a real fix while paused, yet short enough that a captured guardian can never hold the market shut
// (re-asserting only re-pauses NEW borrows — the least-harmful possible lever). `pause_secs = 0` lifts.
pub const GUARDIAN_MAX_PAUSE_SECS: i64 = 604_800; // 7 days

// --- Per-market shutdown (the terminal circuit breaker; fusion-docs §4.x) -----
// Permissionless `shutdown` winds a market down terminally when it is genuinely failing — TCR < SCR
// (aggregate undercollateralization, fresh price) OR a sustained oracle failure — and opens
// `urgent_redeem` (unordered, 0-fee, face value). It is irreversible and condition-gated only (no
// discretionary trigger): the only emergency lever that can permanently close a market must not be
// a key anyone holds. SCR is the shutdown collateral ratio (the aggregate analog of a position's MCR).

/// Shutdown collateral ratio (bps): shut the market down when its TCR drops below this. ~110%
/// (per-asset calibration pending). Init-time-only for now (set at `init_market`).
pub const DEFAULT_SCR_BPS: u16 = 11_000; // 110%
pub const MIN_SCR_BPS: u16 = 10_500; // 105% — never below ~full collateralization
pub const MAX_SCR_BPS: u16 = 15_000; // 150%
/// Oracle-failure trigger: the cached price has gone stale for longer than this many slots (a real
/// outage, NOT a brief blip — far longer than the borrow-staleness gate `MAX_PRICE_STALENESS_SLOTS`).
/// ~1h at 400ms/slot. Placeholder pending calibration. A never-priced market (`spot == 0`) is never
/// "failed" (it's pre-launch), so a fresh market cannot be griefed into a terminal shutdown.
pub const SHUTDOWN_ORACLE_STALENESS_SLOTS: u64 = 9_000;

/// Recorded reason for a per-market `shutdown` (`Market.shutdown_reason`) — the named terminal-recovery
/// reason. 0 = not shut down.
pub const SHUTDOWN_REASON_NONE: u8 = 0;
pub const SHUTDOWN_REASON_SCR: u8 = 1; // TCR fell below SCR
pub const SHUTDOWN_REASON_ORACLE_FAILURE: u8 = 2; // sustained oracle staleness
pub const SHUTDOWN_REASON_UNHOMED_BAD_DEBT: u8 = 3; // a liquidation could not be fully absorbed (RP+redist+buffer)

// --- Net-outflow rate limiter (the bank-run / mint-exploit damper; fusion-docs.md) ---------
// A per-market leaky bucket on NET fUSD issuance: `borrow` consumes capacity, `repay` restores it,
// and the bucket refills fully over `RATELIMIT_WINDOW_SECS`. The cap (`Market.rl_cap`) is a
// governable risk param defaulting to 0 = DISABLED — the mechanism ships now; governance enables a
// calibrated cap post-launch (the architecture defers these "after simulation"). Liquidation,
// redemption, and urgent_redeem are HARD-EXEMPT (never touch the bucket) — limiting the peg floor
// would accelerate insolvency. A higher cap is the fast loosen-path; 0 is fully off.
pub const RATELIMIT_WINDOW_SECS: i64 = 86_400; // 24h refill

// --- Minimum collateral ratio (the per-position liquidation threshold; fusion-docs.md) ------
// MCR is read LIVE by `liquidate` (via `cdp::is_healthy`), so a governance change to it is
// retroactive to existing positions' liquidation eligibility. The compile-time UPPER clamp is the
// constitutional bound that stops a captured/buggy governance from raising MCR far enough to make
// solvent positions liquidatable (the "never expand the liquidatable set" invariant):
// it caps how aggressive the threshold can become regardless of who controls the gate. The FLOOR is
// full collateralization (100%) — below that a position could mint more than its collateral backs.
// 300% leaves headroom for per-asset calibration (the black-swan sim wants ~200% for SOL)
// without permitting the old `u16::MAX` (655%) footgun. The timelock gives borrowers an exit window;
// this clamp ensures even the timelocked landing point stays inside a sane band.
pub const MIN_MCR_BPS: u16 = 10_000; // 100%
pub const MAX_MCR_BPS: u16 = 30_000; // 300%

// --- CCR borrow-restriction band (the mild, non-reflexive RM alternative; fusion-docs.md) ---
// When a market's aggregate TCR is below its CCR, the band blocks only RISK-INCREASING ops (new
// debt, collateral withdrawal) — it NEVER expands the liquidatable set (that is the Recovery-Mode
// death spiral fUSD rejects). De-risking ops (deposit, repay) and the peg floor
// (liquidation, redemption) always stay open, and the band fails OPEN on a stale price (so a dead
// oracle can't grief-freeze the market). `Market.ccr_bps` is a governable `MarketParam`, default
// 0 = DISABLED (it's an inline hot-path gate; governance enables a calibrated CCR after simulation).
pub const MIN_CCR_BPS: u16 = 10_000; // 100% — a non-zero CCR is at least full collateralization
pub const MAX_CCR_BPS: u16 = 30_000; // 300%

// --- Liquidation incentive bounds (fusion-docs.md) -------------------------------------
// The per-position liquidation reserve (a SOL bond) and the collateral gas-comp are stored per
// market (governance-adjustable WITHIN these compile-time clamps — never settable outside them,
// and fixed per position at open-time so a later change can't retroactively alter posted bonds).

/// Upper clamp on the per-position SOL reserve bond: 1 SOL. (0 disables the reserve.)
pub const MAX_RESERVE_LAMPORTS: u64 = 1_000_000_000;
/// Recommended default reserve bond: 0.02 SOL — comfortably covers a liquidation tx under
/// congestion. Deploy scripts / `init_market` pass this; governance retunes within the clamp.
pub const DEFAULT_RESERVE_LAMPORTS: u64 = 20_000_000;

/// Upper clamp on the liquidator collateral gas-comp: 10% (1000 bps).
pub const MAX_LIQ_GAS_COMP_BPS: u16 = 1_000;
/// Recommended default collateral gas-comp: 0.5% (Liquity).
pub const DEFAULT_LIQ_GAS_COMP_BPS: u16 = 50;

/// Liquidation **bonus collar** (bps): a liquidation seizes collateral worth at most
/// `debt · (1 + liq_bonus_bps/10000)`; the rest is returned to the borrower as claimable
/// `Position.coll_surplus`. This bonus IS the borrower's MAX liquidation penalty AND the RP +
/// liquidator's reward (the gas-comp is taken WITHIN it). Governance-adjustable (`MarketParam::LiqBonus`)
/// within `[0, MAX_LIQ_BONUS_BPS]`; **0 = collar OFF** (seize the whole position, no surplus return —
/// the `0 = feature off` convention).
pub const DEFAULT_LIQ_BONUS_BPS: u16 = 1_000; // 10%
pub const MAX_LIQ_BONUS_BPS: u16 = 2_000; // 20% — the clamp ceiling

// --- Redemption rate-bucket bitmap (fusion-docs.md) ----------------------
/// `[b"redeem_bitmap", collateral_mint]` — the per-market zero-copy bitmap + member counts.
pub const REDEMPTION_BITMAP_SEED: &[u8] = b"redeem_bitmap";
/// Fixed bucket count (bitmap = `[u64; NUM_RATE_BUCKETS / 64]`). 256 -> a 32-byte (4-word) bitmap.
pub const NUM_RATE_BUCKETS: usize = 256;
pub const BITMAP_WORDS: usize = NUM_RATE_BUCKETS / 64; // 4
/// Sentinel `Position.bucket` value for a position parked in the redemption **zombie pen**:
/// it carries debt but is no longer a normal redemption target — collateral-exhausted (`ink == 0`,
/// unredeemable) OR sub-`min_debt` dust. One past the last addressable bucket (`rate_bucket::bucket_of`
/// clamps every real rate to `< NUM_RATE_BUCKETS`), so it can never collide with a normal bucket index.
/// Zombies are removed from the normal find-first-set ordering so they can't wedge/clog the lowest
/// bucket; they rejoin their rate bucket when a touch restores health.
pub const ZOMBIE_BUCKET: usize = NUM_RATE_BUCKETS; // 256

/// Bucket width (bps) — the governance-adjustable quantization of `user_rate`, within these clamps.
/// Default 10 bps (0.10%) → 256 buckets span 0–25.6%. Width × count sets the addressable rate range.
pub const DEFAULT_BUCKET_WIDTH_BPS: u16 = 10;
pub const MIN_BUCKET_WIDTH_BPS: u16 = 1; // 0.01%
pub const MAX_BUCKET_WIDTH_BPS: u16 = 100; // 1.00%

/// Flat redemption fee (bps of the redeemed amount), governance-adjustable within this clamp.
/// 0 disables the fee. The dynamic Liquity base-rate is a deferred follow-on.
pub const DEFAULT_REDEMPTION_FEE_BPS: u16 = 50; // 0.50%
pub const MAX_REDEMPTION_FEE_BPS: u16 = 500; // 5%

/// Max number of candidate `Position` accounts a single `redeem` / `urgent_redeem` may take in
/// `remaining_accounts` (the Jupiter-Lend >64-account liquidation DoS, fusion-docs.md).
/// Each candidate costs a realize + reweight + set_stake + an O(n) dup scan, so an unbounded list could
/// blow the per-tx account limit (64 addresses / 128 locks) or the CU ceiling and brick the floor under
/// load. 20 fits comfortably under both with the ~10 fixed accounts, and is ample for batch redemption.
pub const MAX_REDEMPTION_CANDIDATES: usize = 20;

/// Minimum position debt (fUSD-native) — the dust floor (Liquity `MIN_NET_DEBT`). A position
/// must end an op with `recorded_debt == 0` (closed) or `>= min_debt`; `borrow`/`repay` enforce it.
/// Governance-adjustable (`MarketParam::MinDebt`) within `[0, MAX_MIN_DEBT]`; **0 = disabled**. Stops the
/// redemption-griefing vector where an attacker stuffs the lowest rate bucket with sub-cent positions to
/// throttle the redemption floor: a non-zero floor makes each such position lock `>= min_debt · MCR`
/// worth of collateral, pricing the attack out. Non-retroactive (gates only new borrow/repay ops; never
/// force-closes an existing position — it can always fully repay). The redemption-leaves-dust case (the
/// zombie/last-bucket) is a separate deferred refinement.
pub const DEFAULT_MIN_DEBT: u64 = 0; // disabled until governance sets a calibrated floor
pub const MAX_MIN_DEBT: u64 = 10_000 * 1_000_000; // $10,000 cap (6 decimals) — bounds the floor

/// Premature interest-rate-adjustment cooldown (seconds) — the BOLD anti-gaming knob.
/// When `> 0`, an `adjust_rate` within `cooldown` of the position's last change/open is charged an
/// upfront fee = `cooldown`-seconds of interest at the new rate (`fusd_math::premature_adjustment_fee`),
/// so reactive rate-jumping to dodge the redemption queue costs ~that much each time. Governance-adjustable
/// (`MarketParam::RateAdjustCooldown`) within `[0, MAX_RATE_ADJUST_COOLDOWN_SECS]`; **0 = disabled** (no
/// fee, no cooldown). The fee is added to the position's `recorded_debt` + the market aggregate and minted
/// to the insurance buffer lazily (like accrued interest), preserving the supply invariant.
pub const DEFAULT_RATE_ADJUST_COOLDOWN_SECS: i64 = 0; // disabled until governance enables it
pub const MAX_RATE_ADJUST_COOLDOWN_SECS: i64 = 2_592_000; // 30 days — bounds the fee/cooldown

/// Keeper reward (bps) — the cut of the interest `refresh_market` mints that is paid to the cranker
/// (the rest funds the insurance buffer), the self-funding keeper incentive. Governance-
/// adjustable (`MarketParam::KeeperReward`) within `[0, MAX_KEEPER_REWARD_BPS]`; **0 = disabled** (the
/// whole interest funds the buffer, no reward). Self-funding (a split of interest the protocol already
/// mints — never a fresh mint outside the rules, so the supply invariant + credible neutrality hold)
/// and spam-proof (calling `refresh_market` twice mints ~0 the second time, since no new interest has
/// accrued). The cap keeps the buffer's share dominant — the reward pays a keeper's tx cost, it is not
/// a profit center; governance sets the live value low.
pub const DEFAULT_KEEPER_REWARD_BPS: u16 = 0; // disabled until governance enables it
pub const MAX_KEEPER_REWARD_BPS: u16 = 1_000; // 10% of minted interest — bounds the keeper's share

// --- Global Backstop Reserve (bounded shared second-loss capital) ----
// A protocol-owned fUSD reserve funded by a MINORITY cut of every market's realized interest. It is
// the waterfall's tier 3.5 (after a market's local buffer, before un-homed): second-loss capital that
// catches the narrow tail where a contained local failure would otherwise surface as a system-wide
// backing shortfall (confidence contagion). Bounded on BOTH axes — a reserve-level cap AND a per-market
// hybrid draw cap — so it is never a general-purpose bailout, and one bad market can never drain it.
// Every param ships **0/off**; governance enables calibrated values (via the TIMELOCKED global-param
// flow) after the fast-crash sim. NEVER moves user collateral between pools (the hard guardrail).

/// Funding cut (bps of a market's post-keeper realized interest) routed to the global reserve; the
/// majority stays in the market's own local buffer. Governable (`GlobalParam::Cut`) within
/// `[0, MAX_BACKSTOP_CUT_BPS]`; **0 = disabled** (all interest stays local — backstop unfunded).
pub const DEFAULT_BACKSTOP_CUT_BPS: u16 = 0;
pub const MAX_BACKSTOP_CUT_BPS: u16 = 3_000; // ≤30% — the local buffer always keeps the majority

/// Reserve-level cap (v1 = ABSOLUTE fUSD; debt-relative is the documented fast-follow once a global
/// reconciliation snapshot exists). Above the cap, the funding cut reverts to the local buffer.
/// `0` = no accumulation (effectively off). Governable (`GlobalParam::ReserveCap`), no compile-time max
/// (the protocol's own sizing choice; the design targets ~0.5–1.5% of system debt once debt-relative).
pub const DEFAULT_BACKSTOP_RESERVE_CAP: u64 = 0;

// --- Per-market hybrid draw cap (the important bound): a market may draw from the reserve only after
//     its local buffer is exhausted, up to min(base + k·contributed, ceiling_share·reserve,
//     debt_share·market_debt) − already_drawn. All default 0 (draws disabled until calibrated).

/// Base draw allowance every active (gov-onboarded) market gets, independent of contribution (fUSD) —
/// so the backstop is useful to NEW markets where confidence support matters most. Safe because
/// `init_market` is gov-permissioned (no throwaway markets can farm it). Governable.
pub const DEFAULT_BACKSTOP_DRAW_BASE: u64 = 0;
/// Contribution multiplier (bps): additional draw access = `draw_k_bps/10_000 · global_contributed`.
/// Governable within `[0, MAX_BACKSTOP_DRAW_K_BPS]`.
pub const DEFAULT_BACKSTOP_DRAW_K_BPS: u64 = 0;
pub const MAX_BACKSTOP_DRAW_K_BPS: u64 = 100_000; // ≤10× of a market's cumulative contribution
/// Hard ceiling: a single failure may draw at most this fraction (bps) of the LIVE reserve, so one
/// market can't drain it. Governable within `[0, 10_000]` (≤100%).
pub const DEFAULT_BACKSTOP_DRAW_CEILING_SHARE_BPS: u16 = 0;
pub const MAX_BACKSTOP_DRAW_CEILING_SHARE_BPS: u16 = 10_000;
/// Hard ceiling: cumulative backstop draws for a market may not exceed this fraction (bps) of that
/// market's own debt, so a small market can't over-draw vs its size. Governable within `[0, 10_000]`.
pub const DEFAULT_BACKSTOP_DRAW_DEBT_SHARE_BPS: u16 = 0;
pub const MAX_BACKSTOP_DRAW_DEBT_SHARE_BPS: u16 = 10_000;

// --- Borrower interest rate (the Liquity-v2 / BOLD user-set rate; fusion-docs.md) -----------
// Each position picks its own annual interest rate (`Position.user_rate_bps`), validated within these
// compile-time clamps at borrow / `adjust_rate`. The rate is BOTH the accrual rate and the redemption
// rate-bucket key. The MAX sits within the default redemption layout (256 buckets × 10 bps =
// 0–25.6%, i.e. NUM_RATE_BUCKETS × DEFAULT_BUCKET_WIDTH_BPS = 2560 ≥ 2550, one bucket of headroom)
// so every valid rate maps to a distinct bucket — the bitmap is fUSD's ordering primitive.
// MIN matches BOLD's 0.5% floor.
pub const MIN_USER_RATE_BPS: u16 = 50; // 0.5%  (BOLD parity)
pub const MAX_USER_RATE_BPS: u16 = 2_550; // 25.5% (≤ NUM_RATE_BUCKETS × DEFAULT_BUCKET_WIDTH_BPS = 2560)

// --- Oracle validation (fusion-docs.md) --------------------------------------------------
// Per-market thresholds live in `MarketOracle`, governance-adjustable WITHIN these clamps.
// Defaults are conservative placeholders until the backtesting pass pins them.

/// `[b"oracle", collateral_mint]` — per-market feed bindings + validation thresholds.
pub const MARKET_ORACLE_SEED: &[u8] = b"oracle";

/// TWAP ring capacity (observations). MUST be even — `ObservationRing`'s Pod layout
/// requires it (asserted at the embedding site). At one sample/minute ≈ a bit over 1h.
pub const TWAP_RING_CAPACITY: usize = 64;

/// Upper clamp on `max_conf_bps`: conf/price is capped at 5%.
pub const MAX_ORACLE_CONF_BPS: u16 = 500;
pub const DEFAULT_ORACLE_CONF_BPS: u16 = 200; // 2%
/// Upper clamp on the Pyth↔Switchboard agreement band.
pub const MAX_ORACLE_DEVIATION_BPS: u16 = 500;
pub const DEFAULT_ORACLE_DEVIATION_BPS: u16 = 100; // 1%
/// Upper clamp on the DEX-TWAP divergence corridor.
pub const MAX_TWAP_DIVERGENCE_BPS: u16 = 1_000;
pub const DEFAULT_TWAP_DIVERGENCE_BPS: u16 = 500; // 5% — wider than the feeds band: the
                                                  // TWAP lags by construction
/// Feed staleness clamp (seconds).
pub const MAX_ORACLE_MAX_AGE_SECS: i64 = 300;
pub const DEFAULT_ORACLE_MAX_AGE_SECS: i64 = 60;
/// Asymmetry factor k (bps of σ): collateral `−kσ`, debt `+kσ`. Clamped to [1σ, 3σ].
pub const MIN_ORACLE_K_BPS: u16 = 10_000;
pub const MAX_ORACLE_K_BPS: u16 = 30_000;
pub const DEFAULT_ORACLE_K_BPS: u16 = 21_200; // 2.12σ ≈ 95%
/// TWAP window (seconds): long enough that a few-block pump can't move it (Mango lesson).
pub const MIN_TWAP_WINDOW_SECS: i64 = 300;
pub const MAX_TWAP_WINDOW_SECS: i64 = 86_400;
pub const DEFAULT_TWAP_WINDOW_SECS: i64 = 1_800; // 30 min
/// TWAP sample-count guard.
pub const MIN_TWAP_MIN_SAMPLES: u32 = 3;
pub const DEFAULT_TWAP_MIN_SAMPLES: u32 = 10;
/// TWAP ring staleness guard (seconds since the newest observation).
pub const MAX_TWAP_STALENESS_SECS: i64 = 3_600;
pub const DEFAULT_TWAP_STALENESS_SECS: i64 = 300;

/// Liquidation-divergence threshold clamp (bps). The per-market
/// `MarketOracle.liq_max_divergence_bps` is bounded `[0, MAX]`; **0 = disabled** (the gate ships now,
/// governance/init enables a calibrated value). The gate pauses LIQUIDATIONS (never the peg floor)
/// when a FRESH primary disagrees with a PRESENT secondary by more than the set value. It must be
/// set LOOSER than the mint deviation thresholds (`MAX_ORACLE_DEVIATION_BPS` 5% /
/// `MAX_TWAP_DIVERGENCE_BPS` 10%): mints freeze early on mild disagreement, liquidations pause only
/// on GROSS disagreement so a mildly-noisy secondary can't wedge the liquidation engine in a crash.
/// The MAX (100%) lets a deployer set any gross threshold; the recommended production value is
/// ~2000–3000 bps (20–30%), set at `init_market_oracle`. Default 0 (off) until calibrated.
pub const MAX_LIQ_DIVERGENCE_BPS: u16 = 10_000; // 100% — bounds the gross-disagreement threshold
pub const DEFAULT_LIQ_DIVERGENCE_BPS: u16 = 0; // disabled until governance/init sets a calibrated value

/// Plausibility-band minimum width ratio. When BOTH band bounds are set (nonzero),
/// `init_market_oracle` requires `upper >= lower · MIN_PRICE_BAND_RATIO`, forcing the band to be a
/// COARSE 10^k-scale / absolute-nonsense rail (≥ 4× wide) and never a tight price opinion. This is
/// the constitutional guard that a captured governance can't weaponize the band into a synthetic
/// oracle outage (an always-breaching tight band → withheld commit → staleness → permissionless
/// shutdown). The band is init-only in v1; a future `MarketParam::PriceBand` setter would have to add
/// a placement sanity check (current fresh spot must lie inside the new band) on top of this width clamp.
pub const MIN_PRICE_BAND_RATIO: u128 = 4;

// --- Oracle / DEX program IDs (the live `update_price` / `sample_twap` cranks) ------
// These are owner-verified at parse time: a price/pool account is trusted only if the runtime
// owner matches (never trust by address alone). Hardcoded (not governance-settable in v1);
// ProtocolConfig will carry bounded-updatable oracle program IDs in a later milestone so a Pyth
// core migration can't force a redeploy. Values cross-checked against fusion-docs.md
// and the pinned SDKs (pyth_solana_receiver_sdk::ID / switchboard_on_demand::ON_DEMAND_MAINNET_PID).

/// Pyth Solana receiver program — owner of every `PriceUpdateV2` account.
pub const PYTH_RECEIVER_PROGRAM_ID: Pubkey = pubkey!("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");
/// The **upgraded** Pyth Solana receiver program for the core migration (cutover **2026-07-31**),
/// from Pyth's official upgrade-contracts page (docs.pyth.network/price-feeds/core/upgrade/contracts).
/// The `PriceUpdateV2` account format + feed IDs are PRESERVED across the upgrade — only the OWNER
/// program changes — so the sole on-chain adaptation is the feed-account owner check. This is seeded
/// into `ProtocolConfig.pyth_receiver_program_id_alt` at `init_protocol` so `update_price` accepts
/// updates owned by EITHER receiver through the dual-running window, making the cutover a non-event
/// (Pyth core upgrade). Gov can promote/clean up via `set_oracle_program_ids` afterward.
pub const PYTH_RECEIVER_PROGRAM_ID_UPGRADED: Pubkey =
    pubkey!("HDw2E7P8X1SkCyjvoGsfBGAVUutKcj874bXjHrpVYrVL");
/// Switchboard On-Demand program (mainnet) — owner of every `PullFeedAccountData` account.
pub const SWITCHBOARD_ON_DEMAND_PROGRAM_ID: Pubkey =
    pubkey!("SBondMDrcV3K4kxZR1HNVT7osZxAHVHgYXL5Ze1oMUv");
/// Orca Whirlpool program — owner of every Whirlpool pool account sampled by `sample_twap`.
pub const ORCA_WHIRLPOOL_PROGRAM_ID: Pubkey = pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
/// Raydium CLMM program — owner of every Raydium `PoolState` account sampled by `sample_twap`.
pub const RAYDIUM_CLMM_PROGRAM_ID: Pubkey = pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");
/// SPL Stake Pool program — owner of the `StakePool` account read for the C1 LST canonical-rate
/// leg (`update_price`). The canonical rate (`total_lamports / pool_token_supply`) is read directly
/// from this trustless on-chain state, never a swap/DEX.
pub const SPL_STAKE_POOL_PROGRAM_ID: Pubkey = pubkey!("SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy");

// Note: `update_price` defers ALL feed staleness to `fusd_oracle::aggregate` (which uses
// `MarketOracle.max_age_secs`), so a stale feed yields a conservative price + a frozen mint —
// never a hard revert.

// TODO(params milestone): compile-time `[min, max]` clamps for every governable
// parameter (MCR/CCR/SCR, debt ceiling, fees, rate bounds, rate-bucket width...).
// These are program constants — never governance-settable. See fusion-docs.md.
