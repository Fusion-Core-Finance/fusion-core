//! fUSD oracle validation.
//!
//! Layered Pyth (primary) + Switchboard (secondary) + self-maintained DEX TWAP, with
//! asymmetric pricing and rule-based freeze modes (fusion-docs.md). Oracle failure
//! freezes NEW MINTS ONLY — repay, conservative liquidation, and redemption stay open
//! (the BOLD lesson). Nothing here is a discretionary admin pause.
//!
//! Pure, dependency-free, host-tested logic against the oracle-agnostic [`PriceView`]:
//! the [`twap`] observation ring (sampled by a permissionless crank), [`aggregate`]
//! (cross-oracle validation policy), and the `price ∓ k·σ` asymmetric valuations.
//! The feed parsing (pyth-solana-receiver-sdk, switchboard-on-demand) and the on-chain
//! `DexTwap` account wiring live in fusd-core's oracle instructions (`update_price` /
//! `sample_twap`), which normalize every feed to a usd_ray [`PriceView`] before calling
//! [`aggregate`].

pub mod twap;

pub use twap::{Observation, ObservationRing, Timestamp, TwapConfig, TwapError};

/// Actions currently permitted, derived purely from oracle health.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OracleMode {
    /// Feeds fresh and confidence tight: minting allowed.
    Ok,
    /// Stale / confidence too wide / feeds diverge: freeze NEW MINTS only. Repay,
    /// conservative liquidation, and redemption remain open.
    MintFrozen,
}

/// Normalized internal price view (oracle-agnostic). `value = price * 10^expo`.
///
/// `publish_ts` is unix-seconds (the [`Timestamp`] typedef, shared with the TWAP ring;
/// may switch to slots later). The feed adapters normalize to this: Pyth's
/// `publish_time` is already seconds; Switchboard's slot-based freshness maps in the
/// SDK-wiring layer.
#[derive(Clone, Copy, Debug)]
pub struct PriceView {
    pub price: u128,
    pub conf: u128,
    pub expo: i32,
    pub publish_ts: Timestamp,
}

/// Basis-point denominator.
pub const BPS: u128 = 10_000;

impl PriceView {
    /// Confidence ratio σ/μ in bps. Wide ⇒ freeze new mints. fusion-docs.md.
    pub fn conf_ratio_bps(&self) -> u128 {
        if self.price == 0 {
            return u128::MAX;
        }
        self.conf.saturating_mul(BPS) / self.price
    }

    pub fn is_stale(&self, now: Timestamp, max_age_secs: i64) -> bool {
        now.saturating_sub(self.publish_ts) > max_age_secs
    }
}

/// Collateral valuation for mint/LTV: `price - k*conf` (μ − k·σ). Uncertainty works
/// against the borrower. `k_bps` e.g. 21_200 ≈ 2.12σ (95%). fusion-docs.md.
pub fn collateral_price(p: &PriceView, k_bps: u128) -> u128 {
    let haircut = p.conf.saturating_mul(k_bps) / BPS;
    p.price.saturating_sub(haircut)
}

/// Debt valuation for liquidation: `price + k*conf` (μ + k·σ).
pub fn debt_price(p: &PriceView, k_bps: u128) -> u128 {
    let markup = p.conf.saturating_mul(k_bps) / BPS;
    p.price.saturating_add(markup)
}

/// Per-collateral validation thresholds (governance-tunable within compile-time clamps
/// later — see fusion-docs.md).
#[derive(Clone, Copy, Debug)]
pub struct OracleConfig {
    /// Freeze mints if Pyth `conf/price` exceeds this (bps).
    pub max_conf_bps: u128,
    /// Pyth ↔ Switchboard agreement band (bps).
    pub max_deviation_bps: u128,
    /// Corridor vs the DEX TWAP (bps).
    pub twap_max_divergence_bps: u128,
    /// Staleness cutoff for both feeds (seconds).
    pub max_age_secs: i64,
    /// Asymmetry: collateral `price − k·σ`, debt `price + k·σ` (~21_200 ≈ 2.12σ / 95%).
    pub k_bps: u128,
    /// Plausibility band, lower bound (RAY-scaled USD-per-token). `0` disables this
    /// bound. The chosen mid price (PRE-`k·σ`-haircut) must be `>= band_lower_ray` or the aggregate
    /// is IMPLAUSIBLE.
    pub band_lower_ray: u128,
    /// Plausibility band, upper bound (RAY-scaled USD-per-token). `0` disables this bound. The chosen
    /// mid price must be `<= band_upper_ray` or the aggregate is implausible. The band is a coarse
    /// 10^k-scale / absolute-nonsense RAIL (the Sept-2021 Pyth mis-scale class, or a wrong feed rebind
    /// during the Pyth core migration), NEVER a tight price opinion — `init_market_oracle` clamps it
    /// `upper >= lower · MIN_PRICE_BAND_RATIO` so a captured governance can't weaponize it into a
    /// synthetic oracle outage. It catches what the divergence checks can't: a secondary-absent or
    /// all-legs-agreeing-but-absurd price.
    pub band_upper_ray: u128,
    /// Liquidation-divergence threshold (bps). `0` = disabled. When set, a FRESH primary
    /// that disagrees with a PRESENT secondary (Switchboard or DEX-TWAP) by more than this pauses
    /// LIQUIDATIONS (never redemptions/repay — the peg floor). Deliberately LOOSER than the mint
    /// `max_deviation_bps`/`twap_max_divergence_bps`: mints freeze early on mild disagreement, but the
    /// liquidation engine pauses only on GROSS disagreement, so a mildly-noisy secondary doesn't wedge
    /// liquidations during a real crash. A MISSING secondary never trips it (an SB/TWAP outage must
    /// not freeze the liquidation engine — only a present-and-divergent feed does).
    pub liq_max_divergence_bps: u128,
    /// C1 LST canonical-rate leg. `true` for LST markets: minting then REQUIRES a healthy canonical
    /// price (the `canonical` arg `Some`), so a failed/absent on-chain stake-pool rate FREEZES mints
    /// (the BOLD-08 upward-manip→over-mint→depeg defense cannot be verified without it). `false`
    /// (default, non-LST markets) ⇒ the `canonical` arg is ignored for the mode decision. Independent
    /// of this flag, whenever `canonical` is `Some` the COLLATERAL price is capped at
    /// `MIN(market, canonical)` before the −k·σ haircut (debt/redemption pricing is left on the raw
    /// market price — we don't force the worst case on redeemers).
    pub canonical_required: bool,
    /// Canonical-primary (fuSOL) markets: `true` makes the DEX-TWAP corridor OPTIONAL for mint
    /// mode — a market with NO DEX pool bound can still reach `Ok` (there is no venue to sample
    /// pre-listing), while a PRESENT-but-divergent TWAP still freezes mints (a material market
    /// discount blocks new borrowing exactly as before). `false` (default, every pre-existing
    /// market) keeps the corridor load-bearing: an absent TWAP freezes mints. NEVER set for
    /// markets whose price comes from an external market feed — the corridor is their
    /// manipulation rail; a canonical-primary market's rail is the stake-pool rate itself plus
    /// the mandatory liquidity haircut.
    pub twap_corridor_optional: bool,
}

/// Validated, asymmetric price + permitted-actions mode. Always returned — even with
/// every feed degraded — so repay / liquidation / redemption never lose their price
/// (oracle failure must never freeze the peg-defending floor).
#[derive(Clone, Copy, Debug)]
pub struct OracleResult {
    /// For mint / LTV (conservative low: `price − k·σ`).
    pub collateral_price: u128,
    /// For liquidation (conservative high: `price + k·σ`).
    pub debt_price: u128,
    pub mode: OracleMode,
    /// Whether the chosen price came from a **fresh** feed (Pyth or Switchboard within
    /// `max_age_secs`). When `false`, the prices are from a stale fallback view — still
    /// returned (the peg floor never loses its price), but a caller MUST NOT treat them as a fresh quote (e.g. the
    /// on-chain crank must not advance its freshness cache off a stale price, or a keeper
    /// could re-post an old signed update to keep the cache "fresh"). Independent of `mode`:
    /// a fresh feed can still be `MintFrozen` (wide conf / divergence).
    pub fresh: bool,
    /// Whether the chosen mid price lies within the configured plausibility band.
    /// `true` when both band bounds are disabled (the default `(0, 0)`) or the price is inside them.
    /// Folded into `mint_allowed`; the `update_price` crank ALSO gates the spot COMMIT on it, so an
    /// implausible fresh price is WITHHELD (the cache ages into the staleness machinery rather than
    /// committing nonsense as the liquidation/redemption price). Independent of `mode`/`fresh`.
    pub plausible: bool,
    /// Whether a FRESH primary grossly disagrees with a PRESENT secondary beyond
    /// `liq_max_divergence_bps`. `false` when the gate is disabled (`0`), the primary is
    /// stale, or no present secondary diverges. The `update_price` crank caches this as a
    /// pause-until-slot (`Market.liq_divergence_until`, with a post-convergence grace) read ONLY by
    /// `liquidate` — redemptions and repay never gate on it (the peg floor).
    pub liq_divergent: bool,
}

/// Symmetric relative gap in bps, measured against the smaller value (conservative:
/// the larger denominator would understate the gap). Either side zero ⇒ `u128::MAX`.
fn deviation_bps(a: u128, b: u128) -> u128 {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    if lo == 0 {
        return u128::MAX;
    }
    (hi - lo).saturating_mul(BPS) / lo
}

/// Combine Pyth (primary) + Switchboard (secondary) + DEX TWAP (sanity corridor) into
/// one validated asymmetric price and a mode (fusion-docs.md).
///
/// Mode `Ok` (minting allowed) requires ALL of:
/// - Pyth fresh (`now − publish_ts ≤ max_age_secs`) with non-zero price,
/// - Pyth `conf/price ≤ max_conf_bps`,
/// - Switchboard present, fresh, same exponent, within `max_deviation_bps` of Pyth,
/// - DEX TWAP present and within `twap_max_divergence_bps` of Pyth.
///
/// Anything missing / stale / divergent degrades to `MintFrozen` — never a hard stop:
/// conservative prices are still returned from the best available view (Pyth if fresh,
/// else Switchboard if fresh, else the freshest non-zero of the two). All views and the
/// TWAP must share one scale/exponent; the feed adapters normalize before calling this.
///
/// C1 — for LST collateral, `canonical` carries the trustless on-chain stake-pool valuation
/// (`sol_usd · canonical_lst_rate`, RAY-scaled USD per whole LST token) computed by the crank.
/// When `Some`, the COLLATERAL (mint/LTV) price is capped at `MIN(market, canonical)` before the
/// −k·σ haircut so an upward-manipulated market feed can't inflate borrowing power past the
/// stake-pool rate. The DEBT price (liquidation/redemption) is left on the raw market view. When
/// `cfg.canonical_required` (LST markets) and `canonical` is `None` (a stale/zero/absent rate),
/// mints freeze — the over-mint defense can't be verified. Non-LST markets pass `None` + a
/// `false` flag and behave exactly as before.
pub fn aggregate(
    pyth: PriceView,
    switchboard: Option<PriceView>,
    dex_twap: Option<u128>,
    canonical: Option<u128>,
    now: Timestamp,
    cfg: &OracleConfig,
) -> OracleResult {
    let pyth_fresh = !pyth.is_stale(now, cfg.max_age_secs) && pyth.price > 0;
    let sb_fresh =
        switchboard.is_some_and(|s| !s.is_stale(now, cfg.max_age_secs) && s.price > 0);

    // Price selection (independent of the mode decision): best available view.
    let chosen = if pyth_fresh {
        pyth
    } else if sb_fresh {
        switchboard.unwrap()
    } else {
        // Both degraded: freshest non-zero price, falling back to Pyth (the peg floor —
        // a conservative stale price beats no price; staleness already froze mints).
        match switchboard {
            Some(s) if s.price > 0 && (pyth.price == 0 || s.publish_ts > pyth.publish_ts) => s,
            _ => pyth,
        }
    };

    // C6 plausibility band: the chosen MID price (pre-`k·σ`-haircut, so wide confidence can't trip
    // it) must sit within the configured rail. Each bound independently disabled by 0; `(0, 0)` ⇒
    // always plausible (default-off, byte-identical to pre-C6 behavior).
    let plausible = (cfg.band_lower_ray == 0 || chosen.price >= cfg.band_lower_ray)
        && (cfg.band_upper_ray == 0 || chosen.price <= cfg.band_upper_ray);

    // B3 liquidation-divergence: a FRESH primary that grossly disagrees with a PRESENT secondary
    // (looser than the mint deviation thresholds). Disabled by 0; never tripped by a missing or stale
    // secondary (an SB/TWAP outage must not freeze the liquidation engine — only a present, fresh,
    // divergent feed does). Pyth-fresh-gated so an absent primary can't manufacture a false divergence.
    let liq_divergent = cfg.liq_max_divergence_bps > 0
        && pyth_fresh
        && (switchboard.is_some_and(|s| {
            sb_fresh
                && s.expo == pyth.expo
                && deviation_bps(pyth.price, s.price) > cfg.liq_max_divergence_bps
        }) || dex_twap
            .is_some_and(|t| deviation_bps(pyth.price, t) > cfg.liq_max_divergence_bps));

    // C1 canonical leg: cap the COLLATERAL mid at MIN(market, canonical) before the −k·σ haircut,
    // so an upward-manipulated market price can't inflate borrowing power past the trustless
    // stake-pool rate. DEBT pricing stays on the raw market view (don't force the worst case on
    // redeemers). A failed/absent canonical leg on an LST market freezes mints below.
    let coll_mid = match canonical {
        Some(c) => chosen.price.min(c),
        None => chosen.price,
    };
    let coll_haircut = chosen.conf.saturating_mul(cfg.k_bps) / BPS;
    let collateral_price = coll_mid.saturating_sub(coll_haircut);

    let mint_allowed = pyth_fresh
        && pyth.conf_ratio_bps() <= cfg.max_conf_bps
        && switchboard.is_some_and(|s| {
            sb_fresh
                && s.expo == pyth.expo
                && deviation_bps(pyth.price, s.price) <= cfg.max_deviation_bps
        })
        && (if cfg.twap_corridor_optional {
            // Canonical-primary: corridor enforced only when a TWAP is PRESENT (none may exist
            // pre-listing); a present-but-divergent TWAP still freezes mints.
            dex_twap.is_none_or(|t| deviation_bps(pyth.price, t) <= cfg.twap_max_divergence_bps)
        } else {
            dex_twap.is_some_and(|t| deviation_bps(pyth.price, t) <= cfg.twap_max_divergence_bps)
        })
        && plausible
        // C1: an LST market cannot mint without a healthy canonical rate to bound the market price.
        && (!cfg.canonical_required || canonical.is_some());

    OracleResult {
        collateral_price,
        debt_price: debt_price(&chosen, cfg.k_bps),
        mode: if mint_allowed { OracleMode::Ok } else { OracleMode::MintFrozen },
        fresh: pyth_fresh || sb_fresh,
        plausible,
        liq_divergent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pv(price: u128, conf: u128, ts: Timestamp) -> PriceView {
        PriceView { price, conf, expo: -8, publish_ts: ts }
    }

    #[test]
    fn conf_ratio() {
        // conf 1% of price -> 100 bps
        assert_eq!(pv(100_000, 1_000, 0).conf_ratio_bps(), 100);
        assert_eq!(pv(0, 1, 0).conf_ratio_bps(), u128::MAX);
    }

    #[test]
    fn asymmetric_pricing() {
        let p = pv(1_000_000, 10_000, 0); // price 1.0e6, conf 1%
        // k = 2.12σ -> haircut/markup = 21_200 bps of conf = 21_200
        assert_eq!(collateral_price(&p, 21_200), 1_000_000 - 21_200);
        assert_eq!(debt_price(&p, 21_200), 1_000_000 + 21_200);
        // collateral always valued <= debt under uncertainty
        assert!(collateral_price(&p, 21_200) <= debt_price(&p, 21_200));
    }

    const CFG: OracleConfig = OracleConfig {
        max_conf_bps: 500,            // 5%
        max_deviation_bps: 100,       // 1%
        twap_max_divergence_bps: 200, // 2%
        max_age_secs: 60,
        k_bps: 21_200,      // 2.12σ
        band_lower_ray: 0,  // plausibility band disabled by default (byte-identical to pre-C6)
        band_upper_ray: 0,
        liq_max_divergence_bps: 0, // liquidation-divergence gate disabled by default (pre-B3)
        canonical_required: false, // C1 LST canonical leg off by default (non-LST markets)
        twap_corridor_optional: false, // corridor load-bearing by default (pre-fuSOL behavior)
    };
    const NOW: Timestamp = 1_000;

    /// Assert the unconditional invariant and hand the result back.
    fn checked(r: OracleResult) -> OracleResult {
        assert!(
            r.collateral_price <= r.debt_price,
            "collateral_price {} > debt_price {}",
            r.collateral_price,
            r.debt_price
        );
        r
    }

    fn fresh_pyth() -> PriceView {
        pv(1_000_000, 1_000, NOW - 10) // conf 10 bps
    }

    fn fresh_sb() -> PriceView {
        pv(1_001_000, 2_000, NOW - 5) // 10 bps off pyth
    }

    #[test]
    fn aggregate_agree_and_fresh_is_ok() {
        let r = checked(aggregate(fresh_pyth(), Some(fresh_sb()), Some(1_005_000), None, NOW, &CFG));
        assert_eq!(r.mode, OracleMode::Ok);
        assert!(r.fresh);
        // Prices come from Pyth, ∓ 2.12σ.
        assert_eq!(r.collateral_price, 1_000_000 - 2_120);
        assert_eq!(r.debt_price, 1_000_000 + 2_120);
    }

    #[test]
    fn aggregate_deviation_beyond_band_freezes() {
        let sb = pv(1_020_000, 2_000, NOW - 5); // 200 bps off pyth > 100
        let r = checked(aggregate(fresh_pyth(), Some(sb), Some(1_005_000), None, NOW, &CFG));
        assert_eq!(r.mode, OracleMode::MintFrozen);
        assert!(r.fresh, "fresh feed, frozen mode — `fresh` is independent of `mode`");
        assert_eq!(r.collateral_price, 1_000_000 - 2_120); // prices still served (pyth)
    }

    #[test]
    fn aggregate_one_feed_stale_freezes_but_prices_returned() {
        // Switchboard stale -> frozen, priced off Pyth.
        let stale_sb = pv(1_001_000, 2_000, NOW - 61);
        let r = checked(aggregate(fresh_pyth(), Some(stale_sb), Some(1_005_000), None, NOW, &CFG));
        assert_eq!(r.mode, OracleMode::MintFrozen);
        assert_eq!(r.collateral_price, 1_000_000 - 2_120);

        // Pyth stale -> frozen, priced off the fresh Switchboard view.
        let stale_pyth = pv(1_000_000, 1_000, NOW - 61);
        let r = checked(aggregate(stale_pyth, Some(fresh_sb()), Some(1_005_000), None, NOW, &CFG));
        assert_eq!(r.mode, OracleMode::MintFrozen);
        assert_eq!(r.collateral_price, 1_001_000 - 2_000 * 21_200 / BPS);
        assert_eq!(r.debt_price, 1_001_000 + 2_000 * 21_200 / BPS);
    }

    #[test]
    fn aggregate_both_stale_uses_freshest_never_no_price() {
        let stale_pyth = pv(1_000_000, 1_000, NOW - 200);
        let staler_sb = pv(900_000, 2_000, NOW - 300);
        let r = checked(aggregate(stale_pyth, Some(staler_sb), None, None, NOW, &CFG));
        assert_eq!(r.mode, OracleMode::MintFrozen);
        assert!(!r.fresh, "neither feed fresh ⇒ the served price is a stale fallback");
        assert_eq!(r.collateral_price, 1_000_000 - 2_120); // pyth is fresher

        let fresher_sb = pv(900_000, 2_000, NOW - 100);
        let r = checked(aggregate(stale_pyth, Some(fresher_sb), None, None, NOW, &CFG));
        assert_eq!(r.collateral_price, 900_000 - 2_000 * 21_200 / BPS); // sb is fresher

        // Pyth price zero, no switchboard: still a result, conservatively floored.
        let r = checked(aggregate(pv(0, 1_000, NOW - 200), None, None, None, NOW, &CFG));
        assert_eq!(r.mode, OracleMode::MintFrozen);
        assert_eq!(r.collateral_price, 0);
    }

    #[test]
    fn aggregate_twap_corridor_breach_freezes() {
        let r = checked(aggregate(fresh_pyth(), Some(fresh_sb()), Some(1_030_000), None, NOW, &CFG));
        assert_eq!(r.mode, OracleMode::MintFrozen); // 300 bps > 200
        let r = checked(aggregate(fresh_pyth(), Some(fresh_sb()), Some(0), None, NOW, &CFG));
        assert_eq!(r.mode, OracleMode::MintFrozen); // zero TWAP can never agree
    }

    #[test]
    fn aggregate_missing_inputs_freeze() {
        let r = checked(aggregate(fresh_pyth(), None, Some(1_005_000), None, NOW, &CFG));
        assert_eq!(r.mode, OracleMode::MintFrozen); // no switchboard
        let r = checked(aggregate(fresh_pyth(), Some(fresh_sb()), None, None, NOW, &CFG));
        assert_eq!(r.mode, OracleMode::MintFrozen); // no TWAP
    }

    #[test]
    fn aggregate_conf_too_wide_freezes() {
        let wide = pv(1_000_000, 60_000, NOW - 10); // 600 bps > 500
        let r = checked(aggregate(wide, Some(fresh_sb()), Some(1_005_000), None, NOW, &CFG));
        assert_eq!(r.mode, OracleMode::MintFrozen);
        // Wide σ widens the conservative spread — exactly the degraded behavior we want.
        assert_eq!(r.collateral_price, 1_000_000 - 60_000 * 21_200 / BPS);
        assert_eq!(r.debt_price, 1_000_000 + 60_000 * 21_200 / BPS);
    }

    #[test]
    fn aggregate_expo_mismatch_freezes() {
        let mut sb = fresh_sb();
        sb.expo = -6; // not comparable to pyth's -8: raw deviation would be garbage
        let r = checked(aggregate(fresh_pyth(), Some(sb), Some(1_005_000), None, NOW, &CFG));
        assert_eq!(r.mode, OracleMode::MintFrozen);
    }

    #[test]
    fn aggregate_invariant_holds_everywhere() {
        // Grid sweep: collateral_price <= debt_price in EVERY case, including extremes.
        for &price in &[0u128, 1, 1_000_000, u128::MAX] {
            for &conf in &[0u128, 1, 50_000, u128::MAX] {
                for &ts in &[NOW, NOW - 61, -1] {
                    for sb in [None, Some(pv(999_000, 3_000, NOW - 1))] {
                        for twap in [None, Some(1_002_000)] {
                            checked(aggregate(pv(price, conf, ts), sb, twap, None, NOW, &CFG));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn plausibility_band_default_off_is_always_plausible() {
        // (0, 0) band ⇒ plausible regardless of price; pre-C6 behavior is byte-identical.
        let r = checked(aggregate(fresh_pyth(), Some(fresh_sb()), Some(1_005_000), None, NOW, &CFG));
        assert!(r.plausible);
        assert_eq!(r.mode, OracleMode::Ok);
        let absurd = pv(100_000_000, 1_000, NOW - 10); // 100× the others
        assert!(aggregate(absurd, None, None, None, NOW, &CFG).plausible, "disabled band never trips");
    }

    #[test]
    fn plausibility_band_catches_absurd_price() {
        // A coarse rail ~[0.5×, 5×] around the ~1_000_000 fixtures.
        let banded = OracleConfig { band_lower_ray: 500_000, band_upper_ray: 5_000_000, ..CFG };
        // In-band, fresh, agreeing ⇒ Ok + plausible.
        let r = checked(aggregate(fresh_pyth(), Some(fresh_sb()), Some(1_005_000), None, NOW, &banded));
        assert!(r.plausible);
        assert_eq!(r.mode, OracleMode::Ok);

        // A 100× mis-scale (the Sept-2021 Pyth class): fresh + self-consistent across legs, but absurd.
        let hi = pv(100_000_000, 1_000, NOW - 10);
        let r = aggregate(hi, Some(pv(100_100_000, 2_000, NOW - 5)), Some(100_500_000), None, NOW, &banded);
        assert!(!r.plausible, "above the upper band ⇒ implausible");
        assert_eq!(r.mode, OracleMode::MintFrozen, "implausible ⇒ mints freeze (the crank withholds the commit)");
        assert!(r.collateral_price <= r.debt_price, "prices still served");

        // A 1/100 mis-scale: below the lower band.
        let lo = pv(10_000, 10, NOW - 10);
        let r = aggregate(lo, Some(pv(10_010, 20, NOW - 5)), Some(10_050), None, NOW, &banded);
        assert!(!r.plausible, "below the lower band ⇒ implausible");
        assert_eq!(r.mode, OracleMode::MintFrozen);
    }

    #[test]
    fn plausibility_band_is_single_sided_disableable() {
        // Upper-only (lower disabled): tiny price plausible, huge price not.
        let upper_only = OracleConfig { band_lower_ray: 0, band_upper_ray: 5_000_000, ..CFG };
        assert!(aggregate(pv(1, 0, NOW), None, None, None, NOW, &upper_only).plausible);
        assert!(!aggregate(pv(9_000_000, 0, NOW), None, None, None, NOW, &upper_only).plausible);
        // Lower-only (upper disabled): huge price plausible, tiny price not.
        let lower_only = OracleConfig { band_lower_ray: 500_000, band_upper_ray: 0, ..CFG };
        assert!(aggregate(pv(9_000_000, 0, NOW), None, None, None, NOW, &lower_only).plausible);
        assert!(!aggregate(pv(1, 0, NOW), None, None, None, NOW, &lower_only).plausible);
    }

    #[test]
    fn plausibility_band_uses_mid_not_haircut_price() {
        // The band evaluates the MID (pre-k·σ) price, so a WIDE confidence cannot push the haircut
        // collateral_price below a lower bound and manufacture a synthetic outage.
        let banded = OracleConfig { band_lower_ray: 500_000, band_upper_ray: 5_000_000, ..CFG };
        let wide = pv(600_000, 590_000, NOW - 10); // conf ~98% of price ⇒ haircut saturates low
        let r = aggregate(wide, None, None, None, NOW, &banded);
        assert!(r.plausible, "mid 600k is in-band even though the haircut price is far below the lower bound");
        assert!(r.collateral_price < 500_000, "haircut price IS below the band — proves we banded the mid, not the haircut");
    }

    #[test]
    fn liq_divergence_disabled_by_default() {
        // CFG has liq_max_divergence_bps == 0 — even a wild secondary disagreement is not liq-divergent.
        let sb = pv(2_000_000, 2_000, NOW - 5); // 100% off
        assert!(!aggregate(fresh_pyth(), Some(sb), Some(2_000_000), None, NOW, &CFG).liq_divergent);
    }

    #[test]
    fn liq_divergence_trips_only_on_gross_disagreement() {
        let cfg = OracleConfig { liq_max_divergence_bps: 2_000, ..CFG }; // 20%
        // 10% SB disagreement: under the 20% liq threshold ⇒ NOT liq-divergent (mints may still freeze).
        let mild = pv(1_100_000, 2_000, NOW - 5);
        assert!(!aggregate(fresh_pyth(), Some(mild), Some(1_005_000), None, NOW, &cfg).liq_divergent);
        // 30% SB disagreement: over the threshold ⇒ liq-divergent.
        let gross = pv(1_300_000, 2_000, NOW - 5);
        assert!(aggregate(fresh_pyth(), Some(gross), Some(1_005_000), None, NOW, &cfg).liq_divergent);
        // The DEX-TWAP can trip it too (SB absent, TWAP 30% off).
        assert!(aggregate(fresh_pyth(), None, Some(1_300_000), None, NOW, &cfg).liq_divergent);
    }

    #[test]
    fn liq_divergence_missing_or_stale_secondary_never_trips() {
        let cfg = OracleConfig { liq_max_divergence_bps: 2_000, ..CFG };
        // No secondary present ⇒ never divergent (an SB/TWAP outage must not freeze liquidations).
        assert!(!aggregate(fresh_pyth(), None, None, None, NOW, &cfg).liq_divergent);
        // A STALE Switchboard is "not present" for divergence — only a fresh, present feed counts.
        let stale_sb = pv(1_300_000, 2_000, NOW - 61);
        assert!(!aggregate(fresh_pyth(), Some(stale_sb), None, None, NOW, &cfg).liq_divergent);
    }

    #[test]
    fn liq_divergence_requires_fresh_primary() {
        let cfg = OracleConfig { liq_max_divergence_bps: 2_000, ..CFG };
        // Stale primary ⇒ no divergence verdict (liquidation is already staleness-gated; don't pause
        // liquidations on an absent primary).
        let stale_pyth = pv(1_000_000, 1_000, NOW - 61);
        let fresh_div_sb = pv(1_300_000, 2_000, NOW - 5);
        assert!(!aggregate(stale_pyth, Some(fresh_div_sb), None, None, NOW, &cfg).liq_divergent);
    }

    #[test]
    fn liq_divergence_is_looser_than_mint_freeze() {
        // A disagreement in the band (mint_deviation, liq_threshold] freezes mints but does NOT pause
        // liquidations — the whole point of the looser liquidation threshold.
        let cfg = OracleConfig { liq_max_divergence_bps: 2_000, ..CFG }; // mint 1% / liq 20%
        let sb = pv(1_050_000, 2_000, NOW - 5); // 5% off: > 1% (freezes mints), < 20% (no liq pause)
        let r = aggregate(fresh_pyth(), Some(sb), Some(1_005_000), None, NOW, &cfg);
        assert_eq!(r.mode, OracleMode::MintFrozen, "5% disagreement freezes mints");
        assert!(!r.liq_divergent, "but is under the 20% liquidation-divergence threshold");
    }

    // ---- C1: LST canonical-rate leg ----

    #[test]
    fn canonical_caps_collateral_but_not_debt() {
        // LST market: market price 1_000_000, canonical lower at 950_000 (the trustless stake-pool
        // valuation). Collateral is capped at MIN(market, canonical) − k·σ; debt stays on market.
        let cfg = OracleConfig { canonical_required: true, ..CFG };
        let r = checked(aggregate(fresh_pyth(), Some(fresh_sb()), Some(1_005_000), Some(950_000), NOW, &cfg));
        // collateral = 950_000 − (conf 1_000 · 21_200/10_000 = 2_120) = 947_880
        assert_eq!(r.collateral_price, 950_000 - 2_120, "collateral capped at the canonical mid");
        // debt = market 1_000_000 + 2_120 (unchanged — redeemers/liquidation price off the market view)
        assert_eq!(r.debt_price, 1_000_000 + 2_120, "debt is NOT pulled down by the canonical cap");
        assert!(r.collateral_price < r.debt_price);
    }

    #[test]
    fn canonical_above_market_does_not_raise_collateral() {
        // A canonical ABOVE the market price is the safe direction (market not inflated) — MIN keeps
        // the market price, so collateral is unchanged. Defends only against upward market manip.
        let cfg = OracleConfig { canonical_required: true, ..CFG };
        let r = checked(aggregate(fresh_pyth(), Some(fresh_sb()), Some(1_005_000), Some(1_100_000), NOW, &cfg));
        assert_eq!(r.collateral_price, 1_000_000 - 2_120, "MIN keeps the (lower) market price");
        assert_eq!(r.mode, OracleMode::Ok, "fresh + agreeing + canonical present ⇒ Ok");
    }

    #[test]
    fn lst_market_missing_canonical_freezes_mints() {
        // canonical_required (LST) + None canonical (stale/zero/absent stake-pool rate) ⇒ freeze
        // mints (the over-mint defense can't be verified), but prices still serve off the market.
        let cfg = OracleConfig { canonical_required: true, ..CFG };
        let r = checked(aggregate(fresh_pyth(), Some(fresh_sb()), Some(1_005_000), None, NOW, &cfg));
        assert_eq!(r.mode, OracleMode::MintFrozen, "no canonical ⇒ no mint");
        assert!(r.fresh, "the market feed is still fresh");
        assert_eq!(r.collateral_price, 1_000_000 - 2_120, "fall back to the market price, never 0");
    }

    #[test]
    fn lst_market_with_canonical_can_mint() {
        // The positive control: an LST market mints when everything is fresh/agreeing AND a healthy
        // canonical is present (here equal to market, so the cap is a no-op).
        let cfg = OracleConfig { canonical_required: true, ..CFG };
        let r = checked(aggregate(fresh_pyth(), Some(fresh_sb()), Some(1_005_000), Some(1_000_000), NOW, &cfg));
        assert_eq!(r.mode, OracleMode::Ok);
    }

    #[test]
    fn non_lst_market_ignores_canonical_arg() {
        // canonical_required = false (default): a None canonical does NOT freeze, and a Some canonical
        // is still honored as a collateral cap (harmless for non-LST since callers pass None).
        let r = checked(aggregate(fresh_pyth(), Some(fresh_sb()), Some(1_005_000), None, NOW, &CFG));
        assert_eq!(r.mode, OracleMode::Ok, "non-LST market unaffected by the absent canonical");
    }

    #[test]
    fn canonical_invariant_holds_under_sweep() {
        // collateral_price <= debt_price in EVERY canonical configuration, including a canonical of 0
        // (which floors collateral to 0 but must never exceed debt or panic).
        let cfg = OracleConfig { canonical_required: true, ..CFG };
        for canon in [None, Some(0u128), Some(1), Some(500_000), Some(1_000_000), Some(u128::MAX)] {
            for sb in [None, Some(fresh_sb())] {
                for twap in [None, Some(1_005_000u128)] {
                    checked(aggregate(fresh_pyth(), sb, twap, canon, NOW, &cfg));
                }
            }
        }
    }
}
