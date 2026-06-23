//! Anchor events — the off-chain observability layer.
//!
//! Every value-moving / risk-relevant state change emits one event so indexers, the public risk
//! dashboard (fusion-docs.md Phase-1 exit), liquidation/redemption keepers, and proof-of-reserves
//! monitoring get a *historical* stream instead of having to poll-and-diff accounts (account state
//! shows only the present; events show what happened and who did it). One-time account-creation
//! (`init_market_oracle`, `init_reactor_pool`, `init_insurance_buffer`, `open_reactor_deposit`) is
//! fully observable from the created account itself and intentionally emits none. Events ride the Anchor
//! `#[event_cpi]` self-CPI transport: each is an inner instruction
//! (`EVENT_IX_TAG ++ discriminator ++ borsh`), preserved in transaction metadata and immune to
//! the RPC log truncation that drops `Program data:` lines once a tx exceeds the log budget —
//! exactly the fat Pyth-post + liquidate + DEX-sell bundles where the BadDebt pager matters.
//! Purely informational, never load-bearing: no on-chain logic reads them (including the event
//! self-CPI, which Anchor's dispatcher no-ops), and the account-level invariants (supply, vault,
//! weighted-sum) remain the source of truth.
//!
//! Conventions:
//! - Every event carries the `collateral_mint` (the market key) so one subscription can demux
//!   per-market streams. Protocol-wide events (governance, guardian rotation) carry their own keys.
//! - Position/RP "lifecycle" ops share one event shape with a `op: u8` tag (`POSITION_OP_*` /
//!   `REACTOR_OP_*` constants below) — smaller IDL surface, single decode path for indexers.
//! - Amounts are the native units the instruction moved (collateral-native or fUSD-native u64;
//!   u128 for the debt-side accounting quantities).

use anchor_lang::prelude::*;

use crate::state::{GlobalParam, MarketParam};

// ---- PositionUpdated op tags --------------------------------------------------------------------
pub const POSITION_OP_OPEN: u8 = 0;
pub const POSITION_OP_DEPOSIT: u8 = 1;
pub const POSITION_OP_WITHDRAW: u8 = 2;
pub const POSITION_OP_BORROW: u8 = 3;
pub const POSITION_OP_REPAY: u8 = 4;
pub const POSITION_OP_ADJUST_RATE: u8 = 5;
pub const POSITION_OP_CLOSE: u8 = 6;

// ---- ReactorDepositUpdated op tags -------------------------------------------------------------------
pub const REACTOR_OP_PROVIDE: u8 = 0;
pub const REACTOR_OP_WITHDRAW: u8 = 1;
pub const REACTOR_OP_CLAIM: u8 = 2;

// NOTE: the former `emit_position_updated` helper fn was removed with the event-CPI migration:
// `emit_cpi!` requires a literal `ctx` in scope (it references
// `ctx.accounts.event_authority` / `ctx.bumps`), so the seven PositionUpdated call sites construct
// the event inline. Events remain non-load-bearing; transport is the stock Anchor self-CPI.

/// One-time protocol genesis (`init_protocol`).
#[event]
pub struct ProtocolInitialized {
    pub gov_authority: Pubkey,
    pub guardian: Pubkey,
    pub fusd_mint: Pubkey,
}

/// A new isolated market (`init_market`).
#[event]
pub struct MarketInitialized {
    pub collateral_mint: Pubkey,
    pub mcr_bps: u16,
    pub debt_ceiling: u64,
    pub bucket_width_bps: u16,
    pub liq_bonus_bps: u16,
}

/// A position-touching user op (open/deposit/withdraw/borrow/repay/adjust_rate/close — `op` is the
/// `POSITION_OP_*` tag). `amount` is the op-specific quantity (collateral or fUSD moved; the new
/// rate for adjust_rate; 0 for open/close); the remaining fields are the position's POST-op state,
/// so a liquidation keeper can track health without fetching the account.
#[event]
pub struct PositionUpdated {
    pub collateral_mint: Pubkey,
    pub owner: Pubkey,
    pub op: u8,
    pub amount: u64,
    pub ink: u64,
    pub recorded_debt: u128,
    pub user_rate_bps: u16,
    pub bucket: u16,
}

/// A liquidation, with the full five-tier waterfall breakdown (`reactor_offset + redistributed +
/// buffer_absorbed + backstop_absorbed + unhomed == debt` — the absorb conservation).
#[event]
pub struct LiquidationEvent {
    pub collateral_mint: Pubkey,
    pub position: Pubkey,
    pub owner: Pubkey,
    pub liquidator: Pubkey,
    /// Realized present-value debt extinguished/redistributed (fUSD-native).
    pub debt: u128,
    /// Collateral seized (native), inclusive of the gas-comp skim and the RP/redist shares.
    pub seized_collateral: u64,
    /// Liquidator's collateral gas-comp skim (native).
    pub gas_comp: u64,
    /// Collateral the bonus collar returned to the borrower as a claimable surplus (native).
    pub coll_surplus: u64,
    pub reactor_offset: u128,
    pub redistributed: u128,
    pub buffer_absorbed: u128,
    /// Absorbed by the global backstop reserve (tier 3.5).
    pub backstop_absorbed: u128,
    pub unhomed: u128,
    /// The cached oracle price the health check + seizure priced against.
    pub spot: u128,
}

/// Un-homed bad debt was booked (the tier-4 terminal loss) — the alert PoR monitoring pages on.
/// Emitted alongside [`LiquidationEvent`] when `unhomed > 0`.
#[event]
pub struct BadDebtEvent {
    pub collateral_mint: Pubkey,
    pub position: Pubkey,
    pub amount: u128,
    pub total_bad_debt: u128,
}

/// The market's terminal wind-down tripped (`shutdown`, or a tier-4 liquidation). Emitted exactly
/// once per market — on the false→true transition. `reason` is the `SHUTDOWN_REASON_*` constant.
#[event]
pub struct ShutdownEvent {
    pub collateral_mint: Pubkey,
    pub reason: u8,
}

/// An ordered (bitmap-targeted) redemption (`redeem`): totals across the batch.
#[event]
pub struct RedemptionEvent {
    pub collateral_mint: Pubkey,
    pub redeemer: Pubkey,
    pub fusd_burned: u64,
    /// Collateral paid to the redeemer (native, net of fee).
    pub collateral_paid: u64,
    /// Fee collateral retained in `Market.surplus_collateral` (native).
    pub fee_collateral: u64,
    /// The lowest non-empty bucket the batch drained.
    pub bucket: u16,
    /// Candidates submitted (validated, before in-bucket skips).
    pub candidates: u8,
}

/// A shutdown wind-down redemption (`urgent_redeem`): unordered, 0-fee, last price.
#[event]
pub struct UrgentRedemptionEvent {
    pub collateral_mint: Pubkey,
    pub redeemer: Pubkey,
    pub fusd_burned: u64,
    pub collateral_paid: u64,
}

/// A Reactor-Pool depositor op (`op` is the `REACTOR_OP_*` tag). `fusd_amount` is the fUSD moved
/// (provide/withdraw; 0 for claim); `collateral_paid` is the seized-collateral gain paid out
/// (claim; 0 otherwise); `deposited_fusd` is the POST-op compounded deposit.
#[event]
pub struct ReactorDepositUpdated {
    pub collateral_mint: Pubkey,
    pub owner: Pubkey,
    pub op: u8,
    pub fusd_amount: u64,
    pub collateral_paid: u64,
    pub deposited_fusd: u64,
}

/// A borrower claimed the collateral surplus a collared liquidation returned (`claim_coll_surplus`).
#[event]
pub struct CollSurplusClaimed {
    pub collateral_mint: Pubkey,
    pub owner: Pubkey,
    pub amount: u64,
}

/// `refresh_market` minted accrued interest: the buffer/keeper split for PoR + keeper
/// monitoring. Emitted only when something was minted.
#[event]
pub struct InterestMinted {
    pub collateral_mint: Pubkey,
    pub amount: u64,
    pub to_buffer: u64,
    /// The cut routed to the Global Backstop Reserve (global second-loss capital). 0 when the backstop
    /// accounts aren't supplied / cut disabled / reserve at cap.
    pub to_backstop: u64,
    /// The slice diverted to retire `bad_debt` (BOLD-sweep C16 auto-paydown). 0 when disabled or no
    /// bad debt outstanding.
    pub to_bad_debt_paydown: u64,
    pub keeper_cut: u64,
    /// Backlog left unminted (non-zero only in the absurd >u64 case).
    pub unminted_remaining: u128,
}

/// An external deposit into the insurance buffer (`fund_buffer`).
#[event]
pub struct BufferFunded {
    pub collateral_mint: Pubkey,
    pub funder: Pubkey,
    pub amount: u64,
    pub total_funded: u128,
}

/// An `update_price` crank ran: the post-aggregate oracle state (the oracle heartbeat — staleness
/// monitors alarm when these stop). `fresh`/`spot` reflect the committed cache; `mint_frozen` the
/// aggregate mode; `plausible` whether the aggregate passed the C6 band (a `fresh && !plausible`
/// run withholds the commit — a monitorable "implausible price observed" alert).
#[event]
pub struct PriceCommitted {
    pub collateral_mint: Pubkey,
    pub spot: u128,
    pub slot: u64,
    pub mint_frozen: bool,
    pub fresh: bool,
    pub plausible: bool,
}

/// Governance updated the bounded-updatable oracle PROGRAM IDs (e.g. the Pyth core
/// migration). A monitorable, rare admin event: the feed-account owner checks now verify against
/// the new IDs.
#[event]
pub struct OracleProgramIdsUpdated {
    pub old_pyth: Pubkey,
    pub new_pyth: Pubkey,
    pub old_pyth_alt: Pubkey,
    pub new_pyth_alt: Pubkey,
    pub old_switchboard: Pubkey,
    pub new_switchboard: Pubkey,
}

/// Governance rebound a market's oracle feed SOURCES (feed id / SB account / DEX pools).
#[event]
pub struct OracleFeedsRebound {
    pub collateral_mint: Pubkey,
    pub pyth_feed_id: [u8; 32],
    pub switchboard_feed: Pubkey,
    pub orca_pool: Pubkey,
    pub raydium_pool: Pubkey,
}

/// A `sample_twap` crank appended a DEX observation (the TWAP-liveness heartbeat).
#[event]
pub struct TwapSampled {
    pub collateral_mint: Pubkey,
    pub usd_ray: u128,
    pub ts: i64,
}

// ---- governance / guardian ----------------------------------------------------------------------

#[event]
pub struct ParamChangeQueued {
    pub market: Pubkey,
    pub nonce: u64,
    pub param: MarketParam,
    pub value: u64,
    pub eta: i64,
}

#[event]
pub struct ParamChangeExecuted {
    pub market: Pubkey,
    pub nonce: u64,
    pub param: MarketParam,
    /// The value the param held immediately before this change applied (the
    /// forensic Prv/New trail; indexers and incident response reconstruct any param's history
    /// from the event stream alone, without replaying program logic).
    pub prev_value: u64,
    pub value: u64,
}

#[event]
pub struct ParamChangeCanceled {
    pub nonce: u64,
}

// ---- global backstop reserve --------------------------------------------------------------------

/// The Global Backstop Reserve was created.
#[event]
pub struct BackstopInitialized {
    pub fusd_vault: Pubkey,
}

/// fUSD flowed INTO the reserve — a permissionless top-up (the per-market interest cut emits its own
/// per-market signal; this is the explicit donation path). `total_contributed` is cumulative.
#[event]
pub struct BackstopFunded {
    pub funder: Pubkey,
    pub amount: u64,
    pub total_contributed: u128,
}

/// Governance withdrew above-cap excess from the reserve. `total_withdrawn` is cumulative.
#[event]
pub struct BackstopWithdrawn {
    pub recipient: Pubkey,
    pub amount: u64,
    pub total_withdrawn: u128,
}

/// A liquidation drew on the global backstop (tier 3.5) to absorb debt the local buffer couldn't.
#[event]
pub struct BackstopDrawn {
    pub collateral_mint: Pubkey,
    pub amount: u128,
    pub total_absorbed: u128,
}

#[event]
pub struct GlobalParamChangeQueued {
    pub nonce: u64,
    pub param: GlobalParam,
    pub value: u64,
    pub eta: i64,
}

#[event]
pub struct GlobalParamChangeExecuted {
    pub nonce: u64,
    pub param: GlobalParam,
    pub prev_value: u64,
    pub value: u64,
}

#[event]
pub struct GlobalParamChangeCanceled {
    pub nonce: u64,
}

/// Step 1 of the two-step inbound-authority handoff. `pending == Pubkey::default()`
/// means a pending handoff was canceled.
#[event]
pub struct InboundAuthorityProposed {
    pub current: Pubkey,
    pub pending: Pubkey,
}

/// Step 2 — the successor accepted; the live authority moved.
#[event]
pub struct InboundAuthorityMigrated {
    pub previous: Pubkey,
    pub new_authority: Pubkey,
}

/// Step 1 of the two-step `ProtocolConfig.gov_authority` handoff.
/// `pending == Pubkey::default()` means a pending handoff was canceled.
#[event]
pub struct GovAuthorityProposed {
    pub current: Pubkey,
    pub pending: Pubkey,
}

/// Step 2 — the successor accepted; the live admin authority moved.
#[event]
pub struct GovAuthorityMigrated {
    pub previous: Pubkey,
    pub new_authority: Pubkey,
}

/// An executed governance MCR RAISE armed the market-wide liquidation grace window
/// (`liq_grace_until`, monotone max). A silently-armed liquidation pause would
/// be invisible to monitors — this is the alert/forensics hook for the raise-cycling trade-off.
#[event]
pub struct McrRaiseGraceArmed {
    pub collateral_mint: Pubkey,
    pub old_mcr_bps: u16,
    pub new_mcr_bps: u16,
    pub grace_until_slot: u64,
}

/// The guardian (re)set a borrow pause (`guardian_derisk`); `paused_until <= now` is an early lift.
#[event]
pub struct GuardianDerisked {
    pub collateral_mint: Pubkey,
    pub guardian: Pubkey,
    pub paused_until: i64,
}

/// Governance rotated/revoked the guardian (`set_guardian`).
#[event]
pub struct GuardianRotated {
    pub previous: Pubkey,
    pub new_guardian: Pubkey,
}

/// Governance withdrew accrued redemption-fee surplus collateral (`withdraw_surplus`).
#[event]
pub struct SurplusWithdrawn {
    pub collateral_mint: Pubkey,
    pub recipient: Pubkey,
    pub amount: u64,
    pub surplus_remaining: u64,
}

/// Governance swept retained protocol-owned (un-homed) collateral (`sweep_protocol_collateral`).
#[event]
pub struct ProtocolCollateralSwept {
    pub collateral_mint: Pubkey,
    pub recipient: Pubkey,
    pub amount: u64,
    pub protocol_collateral_remaining: u64,
    /// The realized loss this recovery is being deployed against (unchanged by the sweep itself).
    pub bad_debt: u128,
}

/// Governance burned fUSD to retire realized bad debt (`settle_bad_debt`; the recap settlement).
#[event]
pub struct BadDebtSettled {
    pub collateral_mint: Pubkey,
    pub amount: u64,
    pub bad_debt_remaining: u128,
}
