use anchor_lang::prelude::*;

/// Global, read-mostly config. PDA `[b"config"]`.
///
/// Passed read-only on the hot path so unlimited concurrent ops share it without
/// serializing (fusion-docs.md). Written almost never.
#[account]
#[derive(Debug)]
pub struct ProtocolConfig {
    /// Migratable inbound governance authority — the MetaDAO DAO's Squads vault PDA
    /// (`[b"multisig", multisig, b"vault", 0]`). The `GovernanceGate` is the sole
    /// writer of bounded params; this is the authority it checks. fusion-docs.md.
    pub gov_authority: Pubkey,
    /// Guardian: de-risk-only and INDEPENDENT of futarchy/Squads, so a frozen DAO
    /// cannot freeze fUSD's emergency response. fusion-docs.md.
    pub guardian: Pubkey,
    /// Deployer that ran `init_protocol` (informational).
    pub deployer: Pubkey,
    /// The fUSD mint (legacy SPL Token, freeze authority = None, mint authority = PDA).
    pub fusd_mint: Pubkey,
    /// Canonical bump.
    pub bump: u8,
    /// Pending successor for `gov_authority` (two-step propose/accept handoff, mirroring
    /// `GovernanceGate.pending_inbound_authority`). `Pubkey::default()` = no handoff in
    /// flight. The live `gov_authority` moves ONLY when the pending key itself signs
    /// `accept_gov_authority`, so a typo'd / unheld proposal can never brick the admin role
    /// (it just can't be accepted; the current authority re-proposes). Carved from the head
    /// of `_reserved` (offsets of all prior fields unchanged; SPACE unchanged).
    pub pending_gov_authority: Pubkey,
    /// Pyth receiver program that must own every `PriceUpdateV2` account `update_price` parses.
    /// Bounded-updatable by `gov_authority` via `set_oracle_program_ids` so the Pyth
    /// **core program migration (~2026-07-31)** — which changes the receiver program ID — can be
    /// absorbed WITHOUT redeploying an immutable program. Seeded at `init_protocol` from the
    /// compile-time `PYTH_RECEIVER_PROGRAM_ID` (the genesis default). NEVER `Pubkey::default()`
    /// (a zero program ID would brick the crank). Carved from `_reserved`.
    pub pyth_receiver_program_id: Pubkey,
    /// A SECOND accepted Pyth receiver program (Pyth core upgrade). `update_price` accepts
    /// a `PriceUpdateV2` owned by EITHER `pyth_receiver_program_id` OR this — the on-chain analog of
    /// Pyth's "dual-fetch" guidance, giving a ZERO-DOWNTIME cutover across the dual-running window
    /// (and making it irrelevant whether we launch before, during, or after the 2026-07-31 migration).
    /// Seeded at `init_protocol` to `PYTH_RECEIVER_PROGRAM_ID_UPGRADED` so the cutover needs no gov
    /// action at all. `Pubkey::default()` = disabled (only the primary is accepted). After the old
    /// contract is fully deprecated, gov promotes (primary = upgraded) and clears this for
    /// defense-in-depth. Carved from `_reserved`.
    pub pyth_receiver_program_id_alt: Pubkey,
    /// Switchboard On-Demand program that must own every `PullFeedAccountData` account.
    /// Same bounded-updatable rationale as `pyth_receiver_program_id`; seeded from the compile-time
    /// `SWITCHBOARD_ON_DEMAND_PROGRAM_ID`. (Switchboard is NOT part of the Pyth core migration.)
    /// Carved from `_reserved`.
    pub switchboard_program_id: Pubkey,
    /// Reserved for fields added as flows land (registry pointer, DEX program IDs, ...).
    /// fusion-docs.md.
    ///
    /// NOTE: there is deliberately NO global emergency flag. The only emergency levers are
    /// the per-market, rule-based ones (guardian pause-new-debt, permissionless `shutdown`).
    /// A dead `emergency: bool` lived here until 2026-06-12; it was removed so that
    /// "no global kill switch" is grep-verifiable — a dormant flag is exactly
    /// the surface a future setter would colonize.
    pub _reserved: [u8; 32],
}

impl ProtocolConfig {
    /// 8 discriminator + 8 * Pubkey (gov_authority, guardian, deployer, fusd_mint,
    /// pending_gov_authority, pyth_receiver_program_id, pyth_receiver_program_id_alt,
    /// switchboard_program_id) + bump + reserved. Grown pre-launch (no deployed accounts): the three
    /// program-ID Pubkeys are carved from the old `_reserved` (97) with the reserve re-widened to 32
    /// for post-freeze headroom (layout-freeze checklist).
    pub const SPACE: usize = 8 + (32 * 8) + 1 + 32;
}
