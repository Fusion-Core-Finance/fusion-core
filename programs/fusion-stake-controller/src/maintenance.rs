//! The bounded crank-reward payout from the maintenance vault.
//!
//! One shared helper so every crank instruction pays through the identical path: the actual
//! amount is `min(task_reward, epoch_budget_remaining, vault_balance)`
//! (`fusion_stake_math::rewards::payout`) — a ZERO payout is a normal success (an empty vault
//! leaves every crank executable unpaid; permissionless liveness never depends on the reward).
//! The transfer is signed by the `[b"maintenance"]` PDA, the ONLY token-moving authority this
//! program holds, and it moves shares exclusively to the caller-chosen fuSOL account for a
//! successful, previously incomplete transition — no generic withdrawal path exists.

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer};

use crate::constants::{CRANK_EPOCH_PAYOUT_BUDGET, MAINTENANCE_AUTHORITY_SEED};

/// Pay a crank reward. Returns the amount actually paid (0 is success, not failure).
///
/// `budget_used` is `EpochState.epoch_payout_budget_used`; it is bumped by exactly the paid
/// amount, so `Σ payouts <= CRANK_EPOCH_PAYOUT_BUDGET` holds by construction each epoch.
/// The vault is reloaded first: several cranks (pool finalization) mint INTO the vault in the
/// same transaction before paying out of it.
pub fn pay_crank_reward<'info>(
    token_program: &Program<'info, Token>,
    vault: &mut Account<'info, TokenAccount>,
    maintenance_authority: &UncheckedAccount<'info>,
    recipient: &Account<'info, TokenAccount>,
    maintenance_authority_bump: u8,
    task_reward: u64,
    budget_used: &mut u64,
) -> Result<u64> {
    // A self-transfer to the vault itself would move nothing while still consuming the epoch
    // payout budget — near-zero-value grief, closed structurally.
    require!(
        recipient.key() != vault.key(),
        crate::errors::ControllerError::InvalidRewardRecipient
    );
    vault.reload()?;
    let remaining = CRANK_EPOCH_PAYOUT_BUDGET.saturating_sub(*budget_used);
    let amount = fusion_stake_math::rewards::payout(task_reward, remaining, vault.amount);
    if amount == 0 {
        return Ok(0);
    }
    token::transfer(
        CpiContext::new_with_signer(
            token_program.to_account_info(),
            Transfer {
                from: vault.to_account_info(),
                to: recipient.to_account_info(),
                authority: maintenance_authority.to_account_info(),
            },
            &[&[MAINTENANCE_AUTHORITY_SEED, &[maintenance_authority_bump]]],
        ),
        amount,
    )?;
    // Exact by the payout bound (`amount <= remaining <= budget`), checked anyway.
    *budget_used =
        budget_used.checked_add(amount).ok_or(crate::errors::ControllerError::MathOverflow)?;
    Ok(amount)
}
