use anchor_lang::prelude::*;

/// One position's validator direction. PDA `[b"preference", fusion_position]` — the seed
/// includes the position address, so duplicate preference accounts cannot exist.
///
/// The `(observed_ink_nonce, observed_ink, owner)` triple is the anti-reuse belt-and-braces:
/// any collateral change bumps the position's ink nonce, desynchronizing this record until a
/// resync — which only becomes eligible NEXT epoch — so the same fungible shares can never
/// direct stake twice in one epoch through different positions. Losing countability affects
/// direction only; funds, solvency operations and fuSOL rewards never depend on this account.
#[account]
#[derive(Debug)]
pub struct Preference {
    /// Layout version byte (1).
    pub version: u8,
    /// The fusd-core `Position` this preference belongs to (also a PDA seed).
    pub fusion_position: Pubkey,
    /// Position owner at the last (re)sync.
    pub owner: Pubkey,
    /// The selected validator's vote account.
    pub vote_account: Pubkey,
    /// `Position.ink_nonce` observed at the last (re)sync.
    pub observed_ink_nonce: u64,
    /// `Position.ink` observed at the last (re)sync (informational; the snapshot re-reads the
    /// live ink).
    pub observed_ink: u64,
    /// First epoch this preference may count (sync-epoch + 1 on every nonce/validator change).
    pub eligible_from_epoch: u64,
    /// Last epoch this preference was counted (one count per epoch).
    pub last_counted_epoch: u64,
    /// Last epoch the OWNER changed the selected validator (at most one change per epoch).
    pub change_epoch: u64,
    pub bump: u8,
    /// Forward-compat reserve (carve from the HEAD).
    pub _reserved: [u8; 16],
}

impl Preference {
    pub const SPACE: usize = 8 // discriminator
        + 1                    // version
        + 32 * 3               // fusion_position, owner, vote_account
        + 8 * 5                // nonce, ink, eligible_from, last_counted, change_epoch
        + 1                    // bump
        + 16; // _reserved
}
