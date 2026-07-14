use anchor_lang::prelude::*;

/// Program error codes (6000 + ordinal). APPEND-ONLY: new variants MUST go at the END so
/// existing Anchor error codes never shift (the litesvm `E_*` constants pin them) — the same
/// discipline as fusd-core's `FusdError`.
#[error_code]
pub enum ControllerError {
    #[msg("Controller is already sealed: initialize_pool may run exactly once")]
    AlreadySealed,
    #[msg("A predeclared address passed to initialize_controller is the default pubkey")]
    InvalidConfigAddress,
    #[msg("Supplied account does not match the ControllerConfig-recorded address")]
    AddressMismatch,
    #[msg("fuSOL mint must be legacy SPL, 9 decimals, freeze authority None, zero supply, mint authority = the stake-pool withdraw-authority PDA")]
    InvalidFusolMint,
    #[msg("Maintenance vault must be a fuSOL token account whose authority is the maintenance PDA")]
    InvalidMaintenanceVault,
    #[msg("Vote account is not owned by the Vote program or does not parse under a supported VoteState version")]
    InvalidVoteAccount,
    #[msg("Amount must be non-zero")]
    ZeroAmount,
    #[msg("Arithmetic overflow")]
    MathOverflow,
    #[msg("Pool is not initialized yet: initialize_pool must seal the controller before deposits")]
    PoolNotInitialized,
    #[msg("Validator is not admitted to the stake-pool validator list (or is draining) — deposit SOL or choose another validator")]
    ValidatorNotInPool,
    #[msg("Stake deposit would push the validator past its lifecycle cap — deposit SOL or choose another eligible validator")]
    ValidatorCapExceeded,
    #[msg("Validator record's lifecycle status byte is corrupt (fail closed)")]
    CorruptValidatorStatus,
    #[msg("Stake-pool account failed to parse (wrong account or truncated data)")]
    InvalidStakePoolAccount,
    #[msg("Validator-list entry missing or inconsistent with the validator record")]
    InvalidValidatorListEntry,
    // NOTE: kept solely to keep the 6000+ordinal codes stable — no reachable path returns it
    // (every handler landed in stage B). Append new variants AFTER the last one below.
    #[msg("Instruction not yet implemented")]
    NotYetImplemented,
    #[msg("Crank instruction called in the wrong phase")]
    WrongPhase,
    #[msg("Cluster epoch has not advanced past the controller epoch")]
    EpochNotAdvanced,
    #[msg("remaining_accounts malformed: wrong count, order, duplicate, or account for the cursor slice")]
    InvalidRemainingAccounts,
    #[msg("Validator record was not covered by this epoch's reconcile pass (stale observations)")]
    StaleValidatorRecord,
    #[msg("Validator record already received a current-epoch plan result")]
    RecordAlreadyPlanned,
    #[msg("The preference window is not open at the current slot")]
    PreferenceWindowClosed,
    #[msg("The preference window deadline has not passed yet")]
    PreferenceWindowStillOpen,
    #[msg("Preference is not countable this epoch (mint/owner/nonce/delay/already-counted)")]
    PreferenceNotCountable,
    #[msg("Preference already changed this epoch (at most one change per epoch)")]
    PreferenceChangeLimit,
    #[msg("Signer is not the position owner recorded on the live Position")]
    PreferenceOwnerMismatch,
    #[msg("Fusion position account failed the fusd-core owner check or does not parse as a Position")]
    InvalidPositionAccount,
    #[msg("Preference targets a Draining/Removable validator — choose an eligible validator")]
    ValidatorNotEligibleForPreference,
    #[msg("Plan rejected: total eligible directed shares exceed the finalized fuSOL supply")]
    DirectedSharesExceedSupply,
    #[msg("Plan conservation violated: directed + neutral + shortfall != productive lamports")]
    PlanConservationViolated,
    #[msg("Neutral capacity round incomplete or inconsistent with the unsaturated count")]
    NeutralRoundInconsistent,
    #[msg("Rebalance walk already complete — call finish_epoch")]
    RebalanceComplete,
    #[msg("Passed validator is not the deterministic selection for the current rebalance slot")]
    WrongActionTarget,
    #[msg("Rebalance walk incomplete and churn budget not exhausted — the epoch is not finished")]
    EpochNotFinished,
    #[msg("Position still exists: only the preference owner may close this preference")]
    PositionStillOpen,
    #[msg("Rent recipient must be the recorded preference owner")]
    InvalidRentRecipient,
    #[msg("Crank reward recipient must not be the maintenance vault itself")]
    InvalidRewardRecipient,
    #[msg("Preference already changed this epoch (close+recreate does not reset the limit)")]
    PreferenceChangeLimit2,
}
