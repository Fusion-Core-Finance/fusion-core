//! Reactor-Pool product-sum (`P` / `S`) accounting — the Liquity offset algorithm,
//! adapted to Solana bounded storage. Models how stablecoin depositors absorb liquidated
//! debt and earn the seized collateral at a discount, in **O(1)** per liquidation (no
//! iteration over depositors). fusion-docs.md; precedent: Hubble/USDH (see
//! the Hubble IDL — `StabilityPoolState`/`EpochToScaleToSum`).
//!
//! ## Representation
//! - `P` (`u128`): running product, starts at `DECIMAL_PRECISION` (1e18). Each liquidation
//!   multiplies it by `(1 − lossPerUnit)`; when it would drop below `SCALE_FACTOR` (1e9)
//!   it is rescaled up and `scale` increments; a full-pool offset resets it and `epoch`
//!   increments.
//! - `S` grid: per-`(epoch, scale)` cumulative collateral-gain-per-unit-staked, stored as
//!   a **flat `&[u128]`** the caller owns (one `u128` per cell — fUSD pools are isolated /
//!   single-collateral). Addressed `epoch * scales_per_epoch + scale`. **Direct indexing,
//!   no wraparound:** a cell is written once-monotonically and never overwritten, so a
//!   depositor's unrealized gain is *always* computable (no silent loss). If `epoch` or
//!   `scale` would exceed the grid the offset returns an error (astronomically unlikely —
//!   each needs a full drain / a 1e9× product collapse — and is a known migration trigger).
//! - Per depositor: an `{p, s, scale, epoch}` [`Snapshot`] + a realized pending-gain
//!   balance the program keeps; gains are realized **on every interaction** and the
//!   snapshot rolled forward, mirroring Hubble.
//!
//! ## Error feedback
//! Floor-division residuals are carried in `last_coll_error` / `last_loss_error` exactly
//! as Liquity's `lastETHError_Offset` / `lastLUSDLossError_Offset`, so repeated small
//! liquidations don't drift (the Kudelski "precision loss" remediation).
//!
//! ## Overflow envelope (BOLD-sweep C4 — fUSD is `u128` / 6-decimal, not Liquity's `uint256` / 18)
//! BOLD (`StabilityPool.sol`, README "known issue 15") notes its 36-digit `P` overflows `uint256` near a
//! ~1e24 BOLD deposit, deems ~1e23 acceptable, and EXPLICITLY tells forks with a different precision OR
//! supply to redo the analysis. fUSD changed BOTH axes: `P` is `u128` (≈3.40e38 max) at 1e18 precision,
//! and amounts are in **6-decimal native units** ($1 = 1e6). Every overflow REVERTS
//! (`ReactorError::Math` / `ScaleOverflow` / `EpochOverflow`) — never wraps — so the worst case is a bricked
//! offset (a known migration trigger), never a silent loss of a depositor's unrealized gain. The bounds,
//! all far above any plausible per-market fUSD supply:
//! - **Absolute (per offset).** `coll_to_add · 1e18`, `debt_to_offset · 1e18`, and `loss_per_unit · total`
//!   (`loss_per_unit ≤ 1e18`) each fit `u128` while `coll_to_add, debt_to_offset, total_deposits ≤
//!   u128::MAX / 1e18 ≈ 3.40e20` native = **$3.40e14 (≈ $340 trillion)** at 6 decimals. A $10B market is
//!   1e16 native (~34,000× margin); a $1T market — larger than every stablecoin combined — is 1e18 native
//!   (~340× margin).
//! - **Ratio (the marginal).** `marginal = coll_gain_per_unit · P` (≈ `coll_to_add · 1e36 / total`,
//!   `P ≤ 1e18`) fits `u128` while `coll_to_add / total_deposits ≤ u128::MAX / 1e36 ≈ 340`. The realized
//!   ratio for a 9-decimal, ~$150 SOL/LST collateral is ≈ 6.7 (seizing $X of collateral moves ~`X/150·1e9`
//!   native units against ~`X·1e6` native fUSD deposits) — a ~50× margin. High-decimal / low-unit-value or
//!   fee-on-transfer / rebasing collateral could erode this; that is one more reason onboarding is
//!   legacy-SPL only and the seized amount is bounded by the value of the debt it backs.
//! - **Grid.** `scale` bumps ≤ 4 per offset and `epoch` +1 per full-pool drain; both are addressed against
//!   the `[MAX_EPOCHS=128 × MAX_SCALES=64]` grid and REVERT if exceeded. Each bump needs a ~1e9× product
//!   collapse / a full drain (each a catastrophic event), so the grid absorbs 127 epoch rolls before the
//!   migration trigger fires. Pinned by `large_realistic_market_with_lst_ratio_never_overflows`,
//!   `epoch_grid_absorbs_max_rolls_then_reverts`, and `offset_reverts_not_wraps_beyond_envelope`.

use crate::mul_div_floor;
// `bnum` backs the production `update_product` rescale; under `cfg(kani)` that path is swapped for the
// u128-native `update_product_u128` twin, so the import would be unused there.
#[cfg(not(kani))]
use bnum::types::U256;

/// 1e18 — the product/precision scale (Liquity `DECIMAL_PRECISION`).
pub const DECIMAL_PRECISION: u128 = 1_000_000_000_000_000_000;
/// 1e9 — rescale threshold/factor for `P` (Liquity `SCALE_FACTOR`).
pub const SCALE_FACTOR: u128 = 1_000_000_000;

/// Recommended on-chain grid dimensions for the `EpochToScaleToSum` account
/// (`[u128; MAX_EPOCHS * MAX_SCALES]`). `scale` rarely exceeds a handful; `epoch` grows
/// only on full-pool drains. Tune per market; the math here is dimension-agnostic.
pub const MAX_SCALES: u64 = 64;
pub const MAX_EPOCHS: u64 = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReactorError {
    /// Offset attempted against an empty pool.
    NoDeposits,
    /// `debt_to_offset` exceeded total deposits (caller invariant violated).
    DebtExceedsDeposits,
    /// `scale` would exceed the grid stride — migration trigger.
    ScaleOverflow,
    /// `epoch` would exceed the grid — migration trigger.
    EpochOverflow,
    /// Arithmetic overflow (should not occur within realistic pool sizes).
    Math,
}

/// A depositor's snapshot of pool state, taken at their last interaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub p: u128,
    pub s: u128,
    pub scale: u64,
    pub epoch: u64,
}

/// The scalar Reactor-Pool state (the `S` grid is owned by the caller as a slice).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolState {
    pub p: u128,
    pub epoch: u64,
    pub scale: u64,
    /// Total stablecoin (fUSD) currently deposited.
    pub total_deposits: u128,
    pub last_coll_error: u128,
    pub last_loss_error: u128,
}

impl Default for PoolState {
    fn default() -> Self {
        Self::new()
    }
}

impl PoolState {
    pub fn new() -> Self {
        PoolState {
            p: DECIMAL_PRECISION,
            epoch: 0,
            scale: 0,
            total_deposits: 0,
            last_coll_error: 0,
            last_loss_error: 0,
        }
    }

    /// Snapshot the pool at the current `(p, S[epoch,scale], scale, epoch)` — taken when a
    /// depositor provides/withdraws/realizes.
    pub fn snapshot(&self, s_grid: &[u128], stride: u64) -> Snapshot {
        let s = s_idx(self.epoch, self.scale, stride, s_grid.len())
            .map(|i| s_grid[i])
            .unwrap_or(0);
        Snapshot { p: self.p, s, scale: self.scale, epoch: self.epoch }
    }
}

/// Flat index into the `S` grid, or `None` if out of range (treated as "no sum yet" = 0).
/// `pub(crate)` so the Kani harnesses can prove its index contract directly.
#[inline]
pub(crate) fn s_idx(epoch: u64, scale: u64, stride: u64, len: usize) -> Option<usize> {
    if scale >= stride {
        return None;
    }
    let i = (epoch as u128).checked_mul(stride as u128)?.checked_add(scale as u128)?;
    let i = usize::try_from(i).ok()?;
    if i < len {
        Some(i)
    } else {
        None
    }
}

/// Apply a liquidation to the pool: burn `debt_to_offset` of deposits and distribute
/// `coll_to_add` of seized collateral to depositors. Updates `st` and the `s_grid`
/// in place. **Caller must ensure `debt_to_offset <= st.total_deposits`** (the pool only
/// absorbs up to its size; any remainder is redistributed to troves by the caller).
///
/// Mirrors Liquity `StabilityPool._computeRewardsPerUnitStaked` +
/// `_updateRewardSumAndProduct`.
pub fn offset(
    st: &mut PoolState,
    s_grid: &mut [u128],
    stride: u64,
    debt_to_offset: u128,
    coll_to_add: u128,
) -> Result<(), ReactorError> {
    let total = st.total_deposits;
    if total == 0 {
        return Err(ReactorError::NoDeposits);
    }
    if debt_to_offset > total {
        return Err(ReactorError::DebtExceedsDeposits);
    }

    // --- collateral gain per unit staked (with error feedback) ---
    let coll_numerator = coll_to_add
        .checked_mul(DECIMAL_PRECISION)
        .and_then(|x| x.checked_add(st.last_coll_error))
        .ok_or(ReactorError::Math)?;
    let coll_gain_per_unit = coll_numerator / total;
    st.last_coll_error = coll_numerator - coll_gain_per_unit * total;

    // --- loss per unit staked + product factor ---
    let product_factor: u128;
    if debt_to_offset == total {
        // Pool fully emptied: everyone's compounded deposit goes to 0; epoch rolls.
        st.last_loss_error = 0;
        product_factor = 0;
    } else {
        let loss_numerator = debt_to_offset
            .checked_mul(DECIMAL_PRECISION)
            .and_then(|x| x.checked_sub(st.last_loss_error))
            .ok_or(ReactorError::Math)?;
        // +1 so the loss is never under-counted (Liquity), keeping the pool solvent.
        let loss_per_unit = loss_numerator / total + 1;
        st.last_loss_error = loss_per_unit
            .checked_mul(total)
            .ok_or(ReactorError::Math)?
            - loss_numerator;
        product_factor = DECIMAL_PRECISION - loss_per_unit;
    }

    // --- accumulate S at the current (epoch, scale): marginal = collGainPerUnit * P ---
    let marginal = coll_gain_per_unit.checked_mul(st.p).ok_or(ReactorError::Math)?;
    let cur_i = s_idx(st.epoch, st.scale, stride, s_grid.len()).ok_or(ReactorError::EpochOverflow)?;
    s_grid[cur_i] = s_grid[cur_i].checked_add(marginal).ok_or(ReactorError::Math)?;

    // --- update P / scale / epoch ---
    if product_factor == 0 {
        let new_epoch = st.epoch.checked_add(1).ok_or(ReactorError::EpochOverflow)?;
        // The new epoch's first cell must be addressable (and is zero — never wrapped).
        s_idx(new_epoch, 0, stride, s_grid.len()).ok_or(ReactorError::EpochOverflow)?;
        st.epoch = new_epoch;
        st.scale = 0;
        st.p = DECIMAL_PRECISION;
    } else {
        let (new_p, bumps) = update_product(st.p, product_factor)?;
        if bumps > 0 {
            let new_scale = st.scale.checked_add(bumps as u64).ok_or(ReactorError::ScaleOverflow)?;
            // The new scale cell must be addressable (and is zero — fresh scale).
            s_idx(st.epoch, new_scale, stride, s_grid.len()).ok_or(ReactorError::ScaleOverflow)?;
            st.scale = new_scale;
        }
        st.p = new_p;
    }

    st.total_deposits = total - debt_to_offset;
    Ok(())
}

/// New `P` and the number of `scale` bumps. `factor` ∈ (0, 1e18]; over the real domain
/// `p` ∈ [SCALE_FACTOR, 1e18] so every intermediate is ≤ 1e36 < `u128::MAX` (the precision-preserving
/// `* SCALE_FACTOR` rescale only runs while `num < 1e27`). Two implementations, identical in result
/// over that domain (pinned by `update_product_matches_bnum`):
/// - **production** (`cfg(not(kani))`): `bnum` U256 — the trusted, defensively-wide shipped path.
/// - **`cfg(kani)`**: the u128-native twin [`update_product_u128`] — CBMC-friendly (`bnum`'s 256-bit
///   division is intractable for CBMC); exact here because the whole computation fits `u128`.
///
/// `pub(crate)` so the rescale-invariant Kani harness can drive the loop directly (an `offset` with
/// `u8` inputs never bumps `scale`, so only the direct harness exercises the rescale path).
pub(crate) fn update_product(p: u128, factor: u128) -> Result<(u128, u32), ReactorError> {
    #[cfg(not(kani))]
    {
        let prec = U256::from(DECIMAL_PRECISION);
        let sf = U256::from(SCALE_FACTOR);
        let threshold = U256::from(SCALE_FACTOR);
        let mut num = U256::from(p) * U256::from(factor); // ≤ 1e36
        let mut bumps: u32 = 0;
        loop {
            let new_p = num / prec;
            if new_p >= threshold {
                return Ok((u128::try_from(new_p).map_err(|_| ReactorError::Math)?, bumps));
            }
            // Rescale up (preserves precision) and bump scale. `factor > 0` so `num > 0`,
            // and each step multiplies by 1e9, so this terminates in a couple of iterations.
            num *= sf;
            bumps += 1;
            if bumps > 4 {
                // Pathological tiny factor; treat as a vanishing product (caller-bounded MCR
                // makes this unreachable in practice).
                return Err(ReactorError::Math);
            }
        }
    }
    #[cfg(kani)]
    {
        update_product_u128(p, factor)
    }
}

/// The u128-native twin of [`update_product`]'s core (the `cfg(kani)`/test path). Exact over the real
/// domain because every intermediate fits `u128` (`p*factor ≤ 1e36`; the ×1e9 rescale only runs while
/// `num < 1e27`). Validated ≡ the `bnum` path by `update_product_matches_bnum`.
#[cfg(any(kani, test))]
fn update_product_u128(p: u128, factor: u128) -> Result<(u128, u32), ReactorError> {
    let mut num = p.checked_mul(factor).ok_or(ReactorError::Math)?; // ≤ 1e36 < u128::MAX
    let mut bumps: u32 = 0;
    loop {
        let new_p = num / DECIMAL_PRECISION;
        if new_p >= SCALE_FACTOR {
            return Ok((new_p, bumps));
        }
        num = num.checked_mul(SCALE_FACTOR).ok_or(ReactorError::Math)?;
        bumps += 1;
        if bumps > 4 {
            return Err(ReactorError::Math);
        }
    }
}

/// A depositor's current compounded deposit, given their snapshot.
/// Mirrors Liquity `_getCompoundedStakeFromSnapshots`.
pub fn compounded_deposit(st: &PoolState, initial_deposit: u128, snap: &Snapshot) -> u128 {
    if initial_deposit == 0 || snap.epoch < st.epoch {
        return 0; // a full-pool drain since the snapshot wiped this deposit
    }
    let scale_diff = st.scale - snap.scale;
    let compounded = if scale_diff == 0 {
        mul_div_floor(initial_deposit, st.p, snap.p).unwrap_or(0)
    } else if scale_diff == 1 {
        mul_div_floor(initial_deposit, st.p, snap.p).unwrap_or(0) / SCALE_FACTOR
    } else {
        0 // ≥ 2 scale changes: negligible, zeroed (Liquity)
    };
    // Liquity dust guard: drop deposits that have shrunk below 1e-9 of the original.
    if compounded < initial_deposit / 1_000_000_000 {
        0
    } else {
        compounded
    }
}

/// A depositor's accrued collateral gain since their snapshot.
/// Mirrors Liquity `_getCollateralGainFromSnapshots`.
pub fn collateral_gain(
    s_grid: &[u128],
    stride: u64,
    initial_deposit: u128,
    snap: &Snapshot,
) -> Result<u128, ReactorError> {
    if initial_deposit == 0 {
        return Ok(0);
    }
    let s_at = s_idx(snap.epoch, snap.scale, stride, s_grid.len())
        .map(|i| s_grid[i])
        .unwrap_or(0);
    let first_portion = s_at.saturating_sub(snap.s);
    let second_portion = s_idx(snap.epoch, snap.scale + 1, stride, s_grid.len())
        .map(|i| s_grid[i])
        .unwrap_or(0)
        / SCALE_FACTOR;
    let sum = first_portion.checked_add(second_portion).ok_or(ReactorError::Math)?;
    // gain = initial * (first + second) / P_snap / 1e18
    let t = mul_div_floor(initial_deposit, sum, snap.p).ok_or(ReactorError::Math)?;
    Ok(t / DECIMAL_PRECISION)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STRIDE: u64 = MAX_SCALES;

    fn grid() -> Vec<u128> {
        vec![0u128; (MAX_EPOCHS * MAX_SCALES) as usize]
    }

    #[test]
    fn fresh_pool() {
        let st = PoolState::new();
        assert_eq!(st.p, DECIMAL_PRECISION);
        assert_eq!(st.epoch, 0);
        assert_eq!(st.scale, 0);
    }

    #[test]
    fn partial_offset_one_depositor() {
        // One depositor with 100 fUSD; a liquidation offsets 40 debt, adds 10 collateral.
        let mut st = PoolState::new();
        st.total_deposits = 100;
        let mut g = grid();
        let snap = st.snapshot(&g, STRIDE);

        offset(&mut st, &mut g, STRIDE, 40, 10).unwrap();

        // Deposits fell to 60; the sole depositor owns the whole pool, so:
        assert_eq!(st.total_deposits, 60);
        let comp = compounded_deposit(&st, 100, &snap);
        // Liquity's loss-per-unit "+1" keeps the pool solvent: compounded rounds DOWN to
        // <= remaining deposits (the dust stays as a buffer), here 59 vs 60.
        assert!(comp <= 60 && 60 - comp <= 1, "compounded within dust of pool: {}", comp);
        let gain = collateral_gain(&g, STRIDE, 100, &snap).unwrap();
        assert_eq!(gain, 10, "sole depositor gets all seized collateral");
    }

    #[test]
    fn two_depositors_share_pro_rata() {
        // 60 + 40 = 100 total; offset 50 debt / 20 coll. Each should keep 50% of deposit
        // and earn collateral pro-rata to their share.
        let mut st = PoolState::new();
        st.total_deposits = 100;
        let mut g = grid();
        let snap = st.snapshot(&g, STRIDE);

        offset(&mut st, &mut g, STRIDE, 50, 20).unwrap();

        assert_eq!(st.total_deposits, 50);
        let a = compounded_deposit(&st, 60, &snap); // ~30
        let b = compounded_deposit(&st, 40, &snap); // ~20
        assert!((a as i128 - 30).abs() <= 1);
        assert!((b as i128 - 20).abs() <= 1);
        // Solvency invariant: compounded deposits never exceed the pool; deficit is dust.
        assert!(a + b <= st.total_deposits, "no over-allocation: {} > {}", a + b, st.total_deposits);
        assert!(st.total_deposits - (a + b) <= 2, "deficit is dust: {}", st.total_deposits - (a + b));

        let ga = collateral_gain(&g, STRIDE, 60, &snap).unwrap(); // ~12
        let gb = collateral_gain(&g, STRIDE, 40, &snap).unwrap(); // ~8
        assert!((ga as i128 - 12).abs() <= 1);
        assert!((gb as i128 - 8).abs() <= 1);
        assert!(ga + gb <= 20 && ga + gb >= 19, "collateral conserved (minus dust): {}", ga + gb);
    }

    #[test]
    fn full_offset_rolls_epoch_and_wipes_deposits() {
        let mut st = PoolState::new();
        st.total_deposits = 100;
        let mut g = grid();
        let snap = st.snapshot(&g, STRIDE);

        offset(&mut st, &mut g, STRIDE, 100, 25).unwrap(); // debt == total -> full drain

        assert_eq!(st.epoch, 1);
        assert_eq!(st.scale, 0);
        assert_eq!(st.p, DECIMAL_PRECISION);
        assert_eq!(st.total_deposits, 0);
        // Depositor from epoch 0 is wiped (compounded -> 0) but still claims their gain.
        assert_eq!(compounded_deposit(&st, 100, &snap), 0);
        assert_eq!(collateral_gain(&g, STRIDE, 100, &snap).unwrap(), 25);
    }

    #[test]
    fn scale_bumps_when_product_underflows() {
        // A near-total offset (99.9999%+) collapses P below SCALE_FACTOR -> scale++.
        let mut st = PoolState::new();
        st.total_deposits = 1_000_000_000_000; // 1e12
        let mut g = grid();
        // offset almost everything: leaves 1 unit of 1e12 -> product_factor ~ 1e-12 * 1e18 = 1e6
        offset(&mut st, &mut g, STRIDE, 1_000_000_000_000 - 1, 1_000_000).unwrap();
        assert!(st.scale >= 1, "scale should bump on product underflow, got {}", st.scale);
        assert!(st.p >= SCALE_FACTOR, "P rescaled back above SCALE_FACTOR: {}", st.p);
        assert_eq!(st.total_deposits, 1);
    }

    #[test]
    fn error_feedback_no_drift_over_many_offsets() {
        // Many tiny offsets should distribute ~all collateral with no systematic drift,
        // thanks to last_coll_error feedback.
        let mut st = PoolState::new();
        st.total_deposits = 1_000_000; // 1e6
        let mut g = grid();
        let snap = st.snapshot(&g, STRIDE);
        let rounds = 1000u128;
        for _ in 0..rounds {
            // offset 1 unit of debt, 3 units of collateral each round
            offset(&mut st, &mut g, STRIDE, 1, 3).unwrap();
        }
        assert_eq!(st.total_deposits, 1_000_000 - rounds);
        let gain = collateral_gain(&g, STRIDE, 1_000_000, &snap).unwrap();
        // total collateral added = 3 * 1000 = 3000; sole depositor should get ~all of it.
        assert!(gain >= 3000 - 2 && gain <= 3000, "no drift: got {} expected ~3000", gain);
    }

    #[test]
    fn rejects_debt_exceeding_deposits() {
        let mut st = PoolState::new();
        st.total_deposits = 10;
        let mut g = grid();
        assert_eq!(offset(&mut st, &mut g, STRIDE, 11, 1), Err(ReactorError::DebtExceedsDeposits));
        assert_eq!(offset(&mut PoolState::new(), &mut g, STRIDE, 1, 1), Err(ReactorError::NoDeposits));
    }

    // ---------------------------------------------------------------------------------------------
    // BOLD-sweep C4 — overflow-envelope regression (the mandatory "redo the analysis for your
    // precision/supply" fork item). The module doc derives the bounds; these pin them in code: a
    // realistically-LARGE market at the realized SOL/LST collateral ratio never overflows, the grid
    // absorbs the maximum epoch rolls then cleanly reverts, and over-envelope inputs revert (never wrap).
    // ---------------------------------------------------------------------------------------------

    /// A $10B market (1e16 native at 6 decimals — ~10× the largest CDP stablecoin) absorbing a long run
    /// of liquidations at the realized SOL/LST collateral-to-deposit RATIO (~7), INCLUDING a near-total
    /// offset that forces scale bumps, never overflows: every offset returns Ok with `P` in range and
    /// `scale`/`epoch` inside the grid. The other proptests bound `coll <= total` (ratio ≤ 1) and so never
    /// exercise the realistic `1 < ratio << 340` regime this does.
    #[test]
    fn large_realistic_market_with_lst_ratio_never_overflows() {
        let mut st = PoolState::new();
        st.total_deposits = 10_000_000_000u128 * 1_000_000; // $10B at 6 decimals = 1e16 native
        let mut g = grid();

        for round in 0u128..30 {
            let total = st.total_deposits;
            // A near-total offset once (round 10) to force a product collapse + scale bump; ordinary
            // ~14% partials otherwise. Seized collateral is ~7× the offset debt (a 9-dec ~$150 LST/SOL
            // ratio), well under the ~340 marginal limit.
            let debt = if round == 10 { total - 1 } else { total / 7 };
            let coll = debt.saturating_mul(7);
            offset(&mut st, &mut g, STRIDE, debt, coll).expect("realistic large offset never overflows");
            assert!(st.p >= SCALE_FACTOR && st.p <= DECIMAL_PRECISION, "P in range: {}", st.p);
            assert!(st.scale < MAX_SCALES, "scale stays inside the grid: {}", st.scale);
            assert!(st.epoch < MAX_EPOCHS, "epoch stays inside the grid: {}", st.epoch);
            // Re-fund toward the original size so later rounds stay large.
            st.total_deposits = st.total_deposits.saturating_add(total / 2);
        }
    }

    /// The epoch grid absorbs the maximum number of full-pool drains (each a catastrophic wipe-everyone
    /// event, +1 epoch), then cleanly REVERTS with `EpochOverflow` — a migration trigger, never a wrap.
    #[test]
    fn epoch_grid_absorbs_max_rolls_then_reverts() {
        let mut st = PoolState::new();
        let mut g = grid();
        // The grid addresses epochs 0..MAX_EPOCHS-1, so it absorbs (MAX_EPOCHS - 1) full-drain rolls.
        for e in 1..MAX_EPOCHS {
            st.total_deposits = 1_000_000; // re-fund the drained pool
            offset(&mut st, &mut g, STRIDE, 1_000_000, 500).expect("full drain within the grid rolls the epoch");
            assert_eq!(st.epoch, e, "epoch rolled to {e}");
        }
        // One more full drain would need an out-of-grid cell at epoch == MAX_EPOCHS -> revert, not wrap.
        st.total_deposits = 1_000_000;
        assert_eq!(
            offset(&mut st, &mut g, STRIDE, 1_000_000, 500),
            Err(ReactorError::EpochOverflow),
            "the (MAX_EPOCHS)th roll is a migration trigger, not a silent wrap"
        );
    }

    /// Inputs BEYOND the u128 envelope REVERT (`ReactorError::Math`), never wrap or panic — exercising all
    /// three multiply sites: `debt·1e18`, `coll·1e18`, and the marginal `coll_gain_per_unit·P` (the
    /// coll/total ratio bound).
    #[test]
    fn offset_reverts_not_wraps_beyond_envelope() {
        let mut g = grid();

        // `debt_to_offset · 1e18` overflows (partial offset, so the loss-numerator path runs).
        let mut st = PoolState::new();
        st.total_deposits = u128::MAX;
        assert_eq!(offset(&mut st, &mut g, STRIDE, u128::MAX / 2, 0), Err(ReactorError::Math));

        // `coll_to_add · 1e18` overflows.
        let mut st = PoolState::new();
        st.total_deposits = u128::MAX;
        assert_eq!(offset(&mut st, &mut g, STRIDE, 1, u128::MAX), Err(ReactorError::Math));

        // The marginal `coll_gain_per_unit · P` overflows when coll/total >> 340 even though `coll·1e18`
        // itself fits (total 1e6, coll 1e12 -> ratio 1e6).
        let mut st = PoolState::new();
        st.total_deposits = 1_000_000;
        assert_eq!(offset(&mut st, &mut g, STRIDE, 1, 1_000_000_000_000), Err(ReactorError::Math));
    }

    /// Differential validation that the `cfg(kani)` `update_product_u128` twin computes EXACTLY what
    /// the production `bnum` `update_product` does, over the real domain
    /// `p ∈ [SCALE_FACTOR, DECIMAL_PRECISION]`, `factor ∈ [1, DECIMAL_PRECISION]`. This keeps the
    /// shipped `bnum` path validated even though Kani verifies the twin. Sweeps the scale-bump
    /// boundaries + randomized values.
    #[test]
    fn update_product_matches_bnum() {
        fn check(p: u128, factor: u128) {
            assert_eq!(
                update_product_u128(p, factor),
                update_product(p, factor),
                "p={p} factor={factor}"
            );
        }
        let ps = [
            SCALE_FACTOR,
            SCALE_FACTOR + 1,
            SCALE_FACTOR * 7,
            DECIMAL_PRECISION / 3,
            DECIMAL_PRECISION - 1,
            DECIMAL_PRECISION,
        ];
        let factors =
            [1u128, 2, SCALE_FACTOR - 1, SCALE_FACTOR, SCALE_FACTOR + 1, DECIMAL_PRECISION - 1, DECIMAL_PRECISION];
        for &p in &ps {
            for &f in &factors {
                check(p, f);
            }
        }
        let mut s: u128 = 0x1234_5678_9ABC_DEF0;
        let mut next = || {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            s
        };
        for _ in 0..20_000 {
            let p = SCALE_FACTOR + next() % (DECIMAL_PRECISION - SCALE_FACTOR + 1);
            let f = 1 + next() % DECIMAL_PRECISION;
            check(p, f);
        }
    }

    // ---------------------------------------------------------------------------------------------
    // Property / fuzz tests (audit task B8). These assert — over WIDE random inputs — the SAME
    // invariants the Kani harnesses (`kani_proofs.rs`) prove exhaustively over tiny inputs:
    //   * `offset_full_drain_rolls_epoch_and_resets`  -> full_drain_rolls_epoch
    //   * `offset_partial_keeps_pool_solvent`         -> partial_offset_stays_solvent
    //   * `offset_partial_conserves_collateral`       -> partial_offset_conserves_collateral
    //   * `update_product_rescales_above_floor`       -> update_product_stays_above_floor
    //   * `s_idx_none_iff_cell_out_of_range`          -> s_idx_matches_reference
    // plus a long stateful offset-sequence walk that crosses scale/epoch boundaries.
    //
    // `proptest`/`std` imports live INSIDE this `#[cfg(test)]` module — `fusd-math` is `no_std`, so a
    // top-level `use proptest` would break the lib build.
    use proptest::prelude::*;

    proptest! {
        // Cheap pure-math properties: hammer them with 10,000 random cases each.
        #![proptest_config(ProptestConfig::with_cases(10_000))]

        // `s_idx` returns `None` EXACTLY when the cell is out of range, else its precise flat index.
        // Independent reference: recompute the flat index in plain u128 and compare.
        #[test]
        fn s_idx_matches_reference(
            epoch in any::<u8>(),
            scale in any::<u8>(),
            stride in any::<u8>(),
            len in any::<u8>(),
        ) {
            let (epoch, scale, stride, len) = (epoch as u64, scale as u64, stride as u64, len as usize);
            let got = s_idx(epoch, scale, stride, len);
            let idx = epoch * stride + scale; // 255*255 + 255 fits u64, no overflow
            if scale >= stride || idx >= len as u64 {
                prop_assert_eq!(got, None);
            } else {
                prop_assert_eq!(got, Some(idx as usize));
            }
        }

        // `update_product` over the REAL domain (`p ∈ [SCALE_FACTOR, DECIMAL_PRECISION]`,
        // `factor ∈ [1, DECIMAL_PRECISION]`): never panics, always rescales `P` back to/above the
        // SCALE_FACTOR floor (depositor-precision guarantee), and terminates in <= 4 bumps.
        #[test]
        fn update_product_stays_above_floor(
            p in SCALE_FACTOR..=DECIMAL_PRECISION,
            factor in 1u128..=DECIMAL_PRECISION,
        ) {
            let (new_p, bumps) = update_product(p, factor).expect("real-domain inputs never error");
            prop_assert!(new_p >= SCALE_FACTOR, "P never rescales below the floor: {}", new_p);
            prop_assert!(new_p <= DECIMAL_PRECISION, "P stays within precision: {}", new_p);
            prop_assert!(bumps <= 4, "rescale loop terminates within the bump guard: {}", bumps);
        }
    }

    proptest! {
        // The `offset`-driven properties divide by the (random) pool size, so a touch heavier — but
        // still cheap; 2,000 cases keeps total runtime small while sampling pool sizes broadly.
        #![proptest_config(ProptestConfig::with_cases(2_000))]

        // PARTIAL offset (0 < debt < total): the pool stays solvent. Deposits fall by EXACTLY the
        // debt; P stays in the valid range [SCALE_FACTOR, DECIMAL_PRECISION]; epoch/scale don't roll
        // backwards; the loss-per-unit "+1" round-up holds; and the whole-pool depositor's compounded
        // deposit never exceeds what remains. Generate `total` first, then `debt`/`coll` within it so
        // the `offset` precondition (debt <= total) is always honored.
        #[test]
        fn partial_offset_stays_solvent(
            total in 2u128..=1_000_000_000_000u128,
            debt_frac in 1u128..=1_000_000u128,
            // Seized collateral is bounded by the value of the debt it backs (~pool-scale); keep
            // `coll <= total` so `coll_gain_per_unit * P` stays in u128 (the production magnitude
            // contract — a 1e30+× coll/total ratio is out-of-contract, not a bug). `coll_frac/256`.
            coll_frac in 0u128..=256u128,
        ) {
            // debt strictly between 1 and total-1 (partial, never a full drain).
            let debt = 1 + (debt_frac % (total - 1));
            let coll = (total.saturating_mul(coll_frac)) / 256;
            prop_assume!(debt < total);

            let mut st = PoolState::new();
            st.total_deposits = total;
            let mut g = grid();
            let snap = st.snapshot(&g, STRIDE);

            offset(&mut st, &mut g, STRIDE, debt, coll).expect("in-contract offset never errors");

            prop_assert_eq!(st.total_deposits, total - debt, "deposits fall by exactly the debt");
            prop_assert!(st.p >= SCALE_FACTOR, "P stays above the floor: {}", st.p);
            prop_assert!(st.p <= DECIMAL_PRECISION, "P stays within precision: {}", st.p);
            prop_assert_eq!(st.epoch, 0, "a partial offset never rolls the epoch");
            prop_assert!(
                st.last_loss_error >= 1 && st.last_loss_error <= total,
                "loss-per-unit '+1' rounds the loss up, never under-counted: {}", st.last_loss_error
            );

            // Solvency: the whole pool as a single depositor can never claim more than remains.
            let comp = compounded_deposit(&st, total, &snap);
            prop_assert!(comp <= st.total_deposits, "compounded {} exceeds pool {}", comp, st.total_deposits);
        }

        // FULL drain (debt == total): the epoch rolls deterministically — epoch +1, scale -> 0,
        // P -> DECIMAL_PRECISION, total_deposits -> 0, loss error cleared. The pre-drain depositor is
        // wiped (compounded -> 0).
        #[test]
        fn full_drain_rolls_epoch(
            total in 1u128..=1_000_000_000_000u128,
            coll_frac in 0u128..=256u128,
        ) {
            let coll = (total.saturating_mul(coll_frac)) / 256;
            let mut st = PoolState::new();
            st.total_deposits = total;
            let mut g = grid();
            let snap = st.snapshot(&g, STRIDE);

            offset(&mut st, &mut g, STRIDE, total, coll).expect("full drain never errors");

            prop_assert_eq!(st.total_deposits, 0, "a full drain empties the pool");
            prop_assert_eq!(st.epoch, 1, "epoch rolls exactly once");
            prop_assert_eq!(st.scale, 0, "scale resets");
            prop_assert_eq!(st.p, DECIMAL_PRECISION, "P resets to 1.0");
            prop_assert_eq!(st.last_loss_error, 0, "loss error cleared on a full drain");
            prop_assert_eq!(compounded_deposit(&st, total, &snap), 0, "pre-drain deposit is wiped");
        }

        // COLLATERAL conservation through a partial offset: the sole depositor (the whole pool) never
        // accrues MORE collateral than was seized — floor rounding favors the system. Independent f64
        // reference: the gain should be approximately `coll` (the sole depositor owns 100%).
        #[test]
        fn partial_offset_conserves_collateral(
            total in 2u128..=1_000_000_000u128,
            debt_frac in 1u128..=1_000_000u128,
            coll_frac in 0u128..=256u128,
        ) {
            let debt = 1 + (debt_frac % (total - 1));
            let coll = (total.saturating_mul(coll_frac)) / 256;
            prop_assume!(debt < total);

            let mut st = PoolState::new();
            st.total_deposits = total;
            let mut g = grid();
            let snap = st.snapshot(&g, STRIDE);

            offset(&mut st, &mut g, STRIDE, debt, coll).expect("in-contract offset never errors");
            let gain = collateral_gain(&g, STRIDE, total, &snap).expect("gain computes");

            prop_assert!(gain <= coll, "depositor gains {} > seized {}", gain, coll);
            // Sole depositor owns 100% of the pool, so the gain is `coll` minus at most one dust unit.
            prop_assert!(coll - gain <= 1, "gain shortfall is at most one dust unit: {}", coll - gain);
        }
    }

    proptest! {
        // STATEFUL: drive a long random SEQUENCE of offsets crossing scale/epoch boundaries, with
        // re-deposits keeping the pool non-empty, asserting the invariants after EVERY step. This is
        // the highest-value test — scale/epoch bugs hide in long sequences. Heavier per case (each is a
        // multi-op walk with cross-checks), so fewer cases.
        #![proptest_config(ProptestConfig::with_cases(400))]

        #[test]
        fn offset_sequence_preserves_invariants(
            // Each op: (debt fraction in bps 1..=10000, coll fraction of pool in /256, re-deposit
            // top-up). `coll` is derived as a fraction of the CURRENT pool so `coll_gain_per_unit * P`
            // stays in u128 (the production magnitude contract).
            ops in prop::collection::vec(
                (1u32..=10_000u32, 0u128..=256u128, 0u128..=1_000_000u128),
                1..40,
            ),
            start in 1_000u128..=1_000_000_000_000u128,
        ) {
            let mut st = PoolState::new();
            st.total_deposits = start;
            let mut g = grid();

            let mut prev_epoch = st.epoch;
            let mut prev_scale_in_epoch = st.scale;

            for (frac_bps, coll_frac, topup) in ops {
                let total = st.total_deposits;
                if total == 0 {
                    // Pool drained on the previous full offset: a depositor must re-fund it first.
                    st.total_deposits = 1 + topup;
                    continue;
                }
                // debt is `frac_bps/10000` of the pool, clamped to [1, total] (== total => full drain).
                let debt = ((total.saturating_mul(frac_bps as u128)) / 10_000).clamp(1, total);
                let coll = (total.saturating_mul(coll_frac)) / 256;

                let before = st.total_deposits;
                offset(&mut st, &mut g, STRIDE, debt, coll)
                    .expect("every generated offset respects debt <= total");

                // Invariant 1: deposits fell by exactly the debt (or zeroed on a full drain).
                prop_assert_eq!(st.total_deposits, before - debt);
                // Invariant 2: P always within the valid range.
                prop_assert!(st.p >= SCALE_FACTOR && st.p <= DECIMAL_PRECISION, "P out of range: {}", st.p);
                // Invariant 3: epoch is monotonic non-decreasing; scale only resets on an epoch roll.
                prop_assert!(st.epoch >= prev_epoch, "epoch went backwards");
                if st.epoch == prev_epoch {
                    prop_assert!(st.scale >= prev_scale_in_epoch, "scale went backwards within an epoch");
                } else {
                    // Epoch rolled: deterministic reset.
                    prop_assert_eq!(st.scale, 0, "scale resets on an epoch roll");
                    prop_assert_eq!(st.p, DECIMAL_PRECISION, "P resets on an epoch roll");
                    prop_assert_eq!(st.total_deposits, 0, "an epoch roll empties the pool");
                }
                prev_epoch = st.epoch;
                prev_scale_in_epoch = st.scale;

                // (The depositor value-conservation invariant — an OLD snapshot carried across scale
                // bumps and epoch rolls — lives in `depositor_snapshot_conserves_across_scale_and_epoch`
                // below. A snapshot taken HERE, at the same instant, is tautological: `floor(d·p/p) = d`.)

                // Re-deposit top-up to keep the pool funded and push toward boundary crossings.
                st.total_deposits = st.total_deposits.saturating_add(topup);
            }
        }

        // A SINGLE depositor owns the whole pool at entry; their snapshot is taken at ENTRY and carried
        // UNCHANGED across every offset — scale bumps AND (until) an epoch roll. This is the Liquity bug
        // site Kani punts to fuzzing: `compounded_deposit`'s scale_diff 0/1/≥2 paths and
        // `collateral_gain`'s second-portion path only fire when an OLD snapshot survives state
        // evolution. Asserts the conservation the old tautological "Invariant 4" lacked: the entry
        // depositor never compounds to MORE than the pool still holds (solvency across the EVOLVED
        // product), their collateral gain never exceeds what was seized and only accrues, and a
        // full-pool drain wipes them to zero (the epoch-roll path).
        #[test]
        fn depositor_snapshot_conserves_across_scale_and_epoch(
            ops in prop::collection::vec((1u32..=10_000u32, 0u128..=256u128), 1..40),
            start in 1_000u128..=1_000_000_000_000u128,
        ) {
            let mut st = PoolState::new();
            st.total_deposits = start;
            let mut g = grid();

            let initial = start;                  // one depositor == the whole pool
            let entry = st.snapshot(&g, STRIDE);  // taken at ENTRY, carried UNCHANGED across all ops
            let mut total_coll = 0u128;
            let mut prev_gain = 0u128;

            for (frac_bps, coll_frac) in ops {
                let total = st.total_deposits;
                if total == 0 {
                    break; // pool already fully drained; the entry depositor is gone
                }
                let debt = ((total.saturating_mul(frac_bps as u128)) / 10_000).clamp(1, total);
                let coll = (total.saturating_mul(coll_frac)) / 256;
                let drained = debt == total;

                offset(&mut st, &mut g, STRIDE, debt, coll).expect("debt <= total by construction");
                total_coll = total_coll.saturating_add(coll);

                let compounded = compounded_deposit(&st, initial, &entry);
                let gain = collateral_gain(&g, STRIDE, initial, &entry).expect("gain computes");

                // Collateral conservation across scale bumps: never MORE than was seized, and gains
                // only accrue (they plateau once the deposit crosses ≥2 scale changes — never reverse).
                prop_assert!(gain <= total_coll, "gain {} exceeds seized {}", gain, total_coll);
                prop_assert!(gain >= prev_gain, "collateral gain went backwards");
                prev_gain = gain;

                if drained {
                    // A full-pool drain rolls the epoch ⇒ the entry snapshot is now a prior epoch ⇒ wiped.
                    prop_assert_eq!(compounded, 0, "a full drain must wipe the entry depositor's deposit");
                    break;
                }
                // Solvency carried across the EVOLVED product/scale: the entry depositor (the whole
                // pool) can never compound to more than the pool still holds — no value created. Real
                // cross-snapshot check: `entry.p` is the genesis product, `st.p` has evolved (not `d<=d`).
                prop_assert!(
                    compounded <= st.total_deposits,
                    "compounded {} exceeds remaining pool {}", compounded, st.total_deposits
                );
            }
        }
    }
}
