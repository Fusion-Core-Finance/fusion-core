//! THE stake-pool CPI allowlist — the controller's ONLY doorway into the pinned fork.
//!
//! Instruction data is hand-built borsh (1-byte enum discriminant + fields, `Fee` serialized
//! **denominator first**) and every account list is verified field-by-field against the pinned
//! `vendor/spl-stake-pool/program/src/instruction.rs` (spl-stake-pool v2.0.3 @ a27629b). No
//! upstream crate dependency: the fork is on the newer solana-* crate line, and the house style
//! is byte-verified interfaces (fusd-core's `stake_pool.rs` / `clmm.rs` precedent). The byte
//! tests below pin every discriminant, argument encoding, and account-meta shape.
//!
//! ## The allowlist (everything the controller can ever sign)
//!
//! | disc | instruction                        | signer PDA          | purpose |
//! |------|------------------------------------|---------------------|---------|
//! | 0    | `Initialize`                       | pool + deposit auth | one-time pool creation with fixed fees |
//! | 1    | `AddValidatorToPool`               | pool authority      | admit a validator (plan outcome) |
//! | 2    | `RemoveValidatorFromPool`          | pool authority      | remove a drained validator |
//! | 4    | `IncreaseValidatorStake`           | pool authority      | reserve → transient increase |
//! | 5    | `SetPreferredValidator` (Withdraw) | pool authority      | deterministic withdrawal source |
//! | 6    | `UpdateValidatorListBalance`       | none (permissionless) | reconcile balances / merge transients |
//! | 7    | `UpdateStakePoolBalance`           | none (permissionless) | finalize totals + epoch fee |
//! | 8    | `CleanupRemovedValidatorEntries`   | none (permissionless) | drop ReadyForRemoval entries |
//! | 9    | `DepositStake`                     | deposit authority   | user stake deposit |
//! | 14   | `DepositSol`                       | deposit authority   | user SOL deposit |
//! | 21   | `DecreaseValidatorStakeWithReserve`| pool authority      | validator → transient decrease |
//!
//! NOTHING ELSE. There is no `SetFee` / `SetManager` / `SetStaker` / `SetFundingAuthority` /
//! metadata / `Withdraw*` builder in this module, so the manager-abuse surface (instant
//! deposit-fee rug, SolWithdraw funding-authority freeze, fee-account swap) is STRUCTURALLY
//! absent: an instruction the controller cannot construct is an instruction its PDAs can never
//! sign. Withdrawals are direct user↔stake-pool operations (never gated — `sol_withdraw_authority`
//! is never set and stake withdrawals cannot be authority-gated at all).
//!
//! Deliberately excluded rebalance variants: `IncreaseAdditionalValidatorStake` (19) and
//! `DecreaseAdditionalValidatorStake` (20), the ephemeral-account paths that stack a second
//! move onto a live transient. ONE transient per validator per epoch is the intended churn
//! discipline — existing transient activity blocks another move for that validator (spec
//! hysteresis/churn rules), which is exactly the base variants' behavior
//! (`TransientAccountInUse`), so the Additional variants would only widen the audit surface to
//! enable moves the plan must refuse anyway.
//!
//! `SetPreferredValidator` is exposed for `PreferredValidatorType::Withdraw` ONLY (the
//! deterministic anti-cherry-picking withdrawal source). The Deposit variant is never built:
//! forcing all stake deposits through one validator would break per-validator directed
//! deposits.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::{system_program, sysvar};

use crate::constants::{FUSION_STAKE_POOL_PROGRAM_ID, STAKE_CONFIG_ID, STAKE_PROGRAM_ID};

// --- Allowlisted discriminants (borsh enum variant indices, byte 0 of instruction data) -----
pub const IX_INITIALIZE: u8 = 0;
pub const IX_ADD_VALIDATOR_TO_POOL: u8 = 1;
pub const IX_REMOVE_VALIDATOR_FROM_POOL: u8 = 2;
pub const IX_INCREASE_VALIDATOR_STAKE: u8 = 4;
pub const IX_SET_PREFERRED_VALIDATOR: u8 = 5;
pub const IX_UPDATE_VALIDATOR_LIST_BALANCE: u8 = 6;
pub const IX_UPDATE_STAKE_POOL_BALANCE: u8 = 7;
pub const IX_CLEANUP_REMOVED_VALIDATOR_ENTRIES: u8 = 8;
pub const IX_DEPOSIT_STAKE: u8 = 9;
pub const IX_DEPOSIT_SOL: u8 = 14;
pub const IX_DECREASE_VALIDATOR_STAKE_WITH_RESERVE: u8 = 21;

/// `PreferredValidatorType::Withdraw` (borsh variant 1). The Deposit variant (0) is
/// deliberately not represented — see the module doc.
pub const PREFERRED_VALIDATOR_TYPE_WITHDRAW: u8 = 1;

// --- Upstream stake-account PDA derivations (vendor `program/src/lib.rs:85-156`) -------------

/// The validator-stake PDA: seeds `[vote, pool]` plus the u32 seed suffix LE when nonzero
/// (`validator_seed_suffix == 0` in the list entry means "no seed component" — upstream stores
/// `Option<NonZeroU32>` as a plain u32), under the FORK program id.
pub fn derive_validator_stake(vote: &Pubkey, pool: &Pubkey, seed: u32) -> Pubkey {
    let seed_bytes = seed.to_le_bytes();
    let mut seeds: Vec<&[u8]> = vec![vote.as_ref(), pool.as_ref()];
    if seed != 0 {
        seeds.push(&seed_bytes);
    }
    Pubkey::find_program_address(&seeds, &FUSION_STAKE_POOL_PROGRAM_ID).0
}

/// The transient-stake PDA: seeds `[b"transient", vote, pool, u64 seed LE]` under the FORK
/// program id (the u64 suffix is ALWAYS present, unlike the validator-stake u32).
pub fn derive_transient_stake(vote: &Pubkey, pool: &Pubkey, seed: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[b"transient", vote.as_ref(), pool.as_ref(), &seed.to_le_bytes()],
        &FUSION_STAKE_POOL_PROGRAM_ID,
    )
    .0
}

/// Upstream `Fee { denominator: u64, numerator: u64 }` — borsh serializes **denominator
/// FIRST** (state.rs:926-931). `denominator == 0` means zero fee upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fee {
    pub denominator: u64,
    pub numerator: u64,
}

impl Fee {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.denominator.to_le_bytes());
        out.extend_from_slice(&self.numerator.to_le_bytes());
    }
}

/// `Initialize` (0). Accounts (instruction.rs:53-81 + builder :739-784):
/// 0 `[w]` new StakePool (pre-created, program-owned, uninitialized); 1 `[s]` manager;
/// 2 `[]` staker; 3 `[]` withdraw authority PDA; 4 `[w]` uninitialized ValidatorList;
/// 5 `[]` reserve stake; 6 `[w]` pool mint; 7 `[w]` manager fee token account;
/// 8 `[]` token program; 9 `[s]` deposit authority — sets BOTH the stake AND sol deposit
/// authorities (processor.rs:745-755). Marked signer (the upstream client convention); our
/// `invoke_signed` provides the PDA signature.
#[allow(clippy::too_many_arguments)]
pub fn initialize(
    stake_pool: &Pubkey,
    manager: &Pubkey,
    staker: &Pubkey,
    withdraw_authority: &Pubkey,
    validator_list: &Pubkey,
    reserve_stake: &Pubkey,
    pool_mint: &Pubkey,
    manager_fee_account: &Pubkey,
    token_program: &Pubkey,
    deposit_authority: &Pubkey,
    epoch_fee: Fee,
    withdrawal_fee: Fee,
    deposit_fee: Fee,
    referral_fee: u8,
    max_validators: u32,
) -> Instruction {
    let mut data = Vec::with_capacity(1 + 16 * 3 + 1 + 4);
    data.push(IX_INITIALIZE);
    epoch_fee.write(&mut data);
    withdrawal_fee.write(&mut data);
    deposit_fee.write(&mut data);
    data.push(referral_fee);
    data.extend_from_slice(&max_validators.to_le_bytes());
    Instruction {
        program_id: FUSION_STAKE_POOL_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*stake_pool, false),
            AccountMeta::new_readonly(*manager, true),
            AccountMeta::new_readonly(*staker, false),
            AccountMeta::new_readonly(*withdraw_authority, false),
            AccountMeta::new(*validator_list, false),
            AccountMeta::new_readonly(*reserve_stake, false),
            AccountMeta::new(*pool_mint, false),
            AccountMeta::new(*manager_fee_account, false),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(*deposit_authority, true),
        ],
        data,
    }
}

/// `AddValidatorToPool` (1). Accounts (instruction.rs:83-126 + builder :788-825):
/// 0 `[w]` stake pool; 1 `[s]` staker; 2 `[w]` reserve stake; 3 `[]` withdraw authority;
/// 4 `[w]` validator list; 5 `[w]` validator stake account to create; 6 `[]` vote account;
/// 7 `[]` rent; 8 `[]` clock; 9 `[]` stake history; 10 `[]` stake config; 11 `[]` system
/// program; 12 `[]` stake program. `seed` 0 = no seed component (upstream Option<NonZeroU32>).
#[allow(clippy::too_many_arguments)]
pub fn add_validator_to_pool(
    stake_pool: &Pubkey,
    staker: &Pubkey,
    reserve_stake: &Pubkey,
    withdraw_authority: &Pubkey,
    validator_list: &Pubkey,
    validator_stake: &Pubkey,
    vote_account: &Pubkey,
    seed: u32,
) -> Instruction {
    let mut data = Vec::with_capacity(1 + 4);
    data.push(IX_ADD_VALIDATOR_TO_POOL);
    data.extend_from_slice(&seed.to_le_bytes());
    Instruction {
        program_id: FUSION_STAKE_POOL_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*stake_pool, false),
            AccountMeta::new_readonly(*staker, true),
            AccountMeta::new(*reserve_stake, false),
            AccountMeta::new_readonly(*withdraw_authority, false),
            AccountMeta::new(*validator_list, false),
            AccountMeta::new(*validator_stake, false),
            AccountMeta::new_readonly(*vote_account, false),
            AccountMeta::new_readonly(sysvar::rent::ID, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(sysvar::stake_history::ID, false),
            AccountMeta::new_readonly(STAKE_CONFIG_ID, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(STAKE_PROGRAM_ID, false),
        ],
        data,
    }
}

/// `RemoveValidatorFromPool` (2). Accounts (instruction.rs:83-126 + builder :828-852):
/// 0 `[w]` stake pool; 1 `[s]` staker; 2 `[]` withdraw authority; 3 `[w]` validator list;
/// 4 `[w]` validator stake account; 5 `[w]` transient stake account; 6 `[]` clock;
/// 7 `[]` stake program.
pub fn remove_validator_from_pool(
    stake_pool: &Pubkey,
    staker: &Pubkey,
    withdraw_authority: &Pubkey,
    validator_list: &Pubkey,
    validator_stake: &Pubkey,
    transient_stake: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: FUSION_STAKE_POOL_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*stake_pool, false),
            AccountMeta::new_readonly(*staker, true),
            AccountMeta::new_readonly(*withdraw_authority, false),
            AccountMeta::new(*validator_list, false),
            AccountMeta::new(*validator_stake, false),
            AccountMeta::new(*transient_stake, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(STAKE_PROGRAM_ID, false),
        ],
        data: vec![IX_REMOVE_VALIDATOR_FROM_POOL],
    }
}

/// `IncreaseValidatorStake` (4). Accounts (instruction.rs:165-204 + builder :976-1015):
/// 0 `[]` stake pool; 1 `[s]` staker; 2 `[]` withdraw authority; 3 `[w]` validator list;
/// 4 `[w]` reserve stake; 5 `[w]` transient stake; 6 `[]` validator stake; 7 `[]` vote
/// account; 8 `[]` clock; 9 `[]` rent; 10 `[]` stake history; 11 `[]` stake config;
/// 12 `[]` system program; 13 `[]` stake program. Fails `TransientAccountInUse` when the
/// validator already has a live transient — the intended one-move-per-epoch discipline.
#[allow(clippy::too_many_arguments)]
pub fn increase_validator_stake(
    stake_pool: &Pubkey,
    staker: &Pubkey,
    withdraw_authority: &Pubkey,
    validator_list: &Pubkey,
    reserve_stake: &Pubkey,
    transient_stake: &Pubkey,
    validator_stake: &Pubkey,
    vote_account: &Pubkey,
    lamports: u64,
    transient_stake_seed: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(1 + 8 + 8);
    data.push(IX_INCREASE_VALIDATOR_STAKE);
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&transient_stake_seed.to_le_bytes());
    Instruction {
        program_id: FUSION_STAKE_POOL_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*stake_pool, false),
            AccountMeta::new_readonly(*staker, true),
            AccountMeta::new_readonly(*withdraw_authority, false),
            AccountMeta::new(*validator_list, false),
            AccountMeta::new(*reserve_stake, false),
            AccountMeta::new(*transient_stake, false),
            AccountMeta::new_readonly(*validator_stake, false),
            AccountMeta::new_readonly(*vote_account, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(sysvar::rent::ID, false),
            AccountMeta::new_readonly(sysvar::stake_history::ID, false),
            AccountMeta::new_readonly(STAKE_CONFIG_ID, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(STAKE_PROGRAM_ID, false),
        ],
        data,
    }
}

/// `SetPreferredValidator` (5) — **Withdraw only** (see module doc). Accounts
/// (instruction.rs:206-224 + builder :1120-1141): 0 `[w]` stake pool; 1 `[s]` staker;
/// 2 `[]` validator list. `vote_account = None` unsets (borsh Option: tag byte 0/1).
pub fn set_preferred_withdraw_validator(
    stake_pool: &Pubkey,
    staker: &Pubkey,
    validator_list: &Pubkey,
    vote_account: Option<&Pubkey>,
) -> Instruction {
    let mut data = Vec::with_capacity(1 + 1 + 1 + 32);
    data.push(IX_SET_PREFERRED_VALIDATOR);
    data.push(PREFERRED_VALIDATOR_TYPE_WITHDRAW);
    match vote_account {
        Some(vote) => {
            data.push(1);
            data.extend_from_slice(vote.as_ref());
        }
        None => data.push(0),
    }
    Instruction {
        program_id: FUSION_STAKE_POOL_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*stake_pool, false),
            AccountMeta::new_readonly(*staker, true),
            AccountMeta::new_readonly(*validator_list, false),
        ],
        data,
    }
}

/// `UpdateValidatorListBalance` (6) — permissionless. Accounts (instruction.rs:227-249 +
/// builder :1502-1560): 0 `[]` stake pool; 1 `[]` withdraw authority; 2 `[w]` validator list;
/// 3 `[w]` reserve stake; 4 `[]` clock; 5 `[]` stake history; 6 `[]` stake program;
/// 7..7+2N `[w]` N (validator stake, transient stake) pairs covering
/// `validator_list[start_index..start_index+N]`. The pair addresses MUST be derived from the
/// on-chain validator list (a mismatched pair is silently skipped upstream and stays stale,
/// wedging `UpdateStakePoolBalance`).
pub fn update_validator_list_balance(
    stake_pool: &Pubkey,
    withdraw_authority: &Pubkey,
    validator_list: &Pubkey,
    reserve_stake: &Pubkey,
    stake_pairs: &[(Pubkey, Pubkey)],
    start_index: u32,
    no_merge: bool,
) -> Instruction {
    let mut data = Vec::with_capacity(1 + 4 + 1);
    data.push(IX_UPDATE_VALIDATOR_LIST_BALANCE);
    data.extend_from_slice(&start_index.to_le_bytes());
    data.push(u8::from(no_merge));
    let mut accounts = vec![
        AccountMeta::new_readonly(*stake_pool, false),
        AccountMeta::new_readonly(*withdraw_authority, false),
        AccountMeta::new(*validator_list, false),
        AccountMeta::new(*reserve_stake, false),
        AccountMeta::new_readonly(sysvar::clock::ID, false),
        AccountMeta::new_readonly(sysvar::stake_history::ID, false),
        AccountMeta::new_readonly(STAKE_PROGRAM_ID, false),
    ];
    for (validator_stake, transient_stake) in stake_pairs {
        accounts.push(AccountMeta::new(*validator_stake, false));
        accounts.push(AccountMeta::new(*transient_stake, false));
    }
    Instruction { program_id: FUSION_STAKE_POOL_PROGRAM_ID, accounts, data }
}

/// `UpdateStakePoolBalance` (7) — permissionless. Accounts (instruction.rs:251-261 + builder
/// :1605-1630): 0 `[w]` stake pool; 1 `[]` withdraw authority; 2 `[w]` validator list;
/// 3 `[]` reserve stake; 4 `[w]` manager fee account; 5 `[w]` pool mint; 6 `[]` token program.
/// Mints the epoch fee to the manager fee account (= the maintenance vault).
pub fn update_stake_pool_balance(
    stake_pool: &Pubkey,
    withdraw_authority: &Pubkey,
    validator_list: &Pubkey,
    reserve_stake: &Pubkey,
    manager_fee_account: &Pubkey,
    pool_mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: FUSION_STAKE_POOL_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*stake_pool, false),
            AccountMeta::new_readonly(*withdraw_authority, false),
            AccountMeta::new(*validator_list, false),
            AccountMeta::new_readonly(*reserve_stake, false),
            AccountMeta::new(*manager_fee_account, false),
            AccountMeta::new(*pool_mint, false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![IX_UPDATE_STAKE_POOL_BALANCE],
    }
}

/// `CleanupRemovedValidatorEntries` (8) — permissionless. Accounts (instruction.rs:263-268 +
/// builder :1634-1647): 0 `[w]` stake pool (writable so upstream also auto-resets a dangling
/// preferred validator); 1 `[w]` validator list.
pub fn cleanup_removed_validator_entries(
    stake_pool: &Pubkey,
    validator_list: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: FUSION_STAKE_POOL_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*stake_pool, false),
            AccountMeta::new(*validator_list, false),
        ],
        data: vec![IX_CLEANUP_REMOVED_VALIDATOR_ENTRIES],
    }
}

/// `DepositStake` (9). Accounts (instruction.rs:270-295 + builder :1761-1843):
/// 0 `[w]` stake pool; 1 `[w]` validator list; 2 `[s]` stake deposit authority (OUR custom
/// PDA — signer, provided by `invoke_signed`); 3 `[]` withdraw authority; 4 `[w]` user stake
/// account to absorb (its staker+withdrawer must ALREADY be authorized to the deposit
/// authority); 5 `[w]` validator stake account; 6 `[w]` reserve stake (receives the rent/extra
/// SOL portion); 7 `[w]` user pool-token account; 8 `[w]` manager fee account; 9 `[w]`
/// referrer pool-token account; 10 `[w]` pool mint; 11 `[]` clock; 12 `[]` stake history;
/// 13 `[]` token program; 14 `[]` stake program.
#[allow(clippy::too_many_arguments)]
pub fn deposit_stake(
    stake_pool: &Pubkey,
    validator_list: &Pubkey,
    deposit_authority: &Pubkey,
    withdraw_authority: &Pubkey,
    user_stake: &Pubkey,
    validator_stake: &Pubkey,
    reserve_stake: &Pubkey,
    user_pool_token_account: &Pubkey,
    manager_fee_account: &Pubkey,
    referrer_pool_token_account: &Pubkey,
    pool_mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: FUSION_STAKE_POOL_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*stake_pool, false),
            AccountMeta::new(*validator_list, false),
            AccountMeta::new_readonly(*deposit_authority, true),
            AccountMeta::new_readonly(*withdraw_authority, false),
            AccountMeta::new(*user_stake, false),
            AccountMeta::new(*validator_stake, false),
            AccountMeta::new(*reserve_stake, false),
            AccountMeta::new(*user_pool_token_account, false),
            AccountMeta::new(*manager_fee_account, false),
            AccountMeta::new(*referrer_pool_token_account, false),
            AccountMeta::new(*pool_mint, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(sysvar::stake_history::ID, false),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(STAKE_PROGRAM_ID, false),
        ],
        data: vec![IX_DEPOSIT_STAKE],
    }
}

/// `DepositSol` (14). Accounts (instruction.rs:359-374 + builder :2013-2060):
/// 0 `[w]` stake pool; 1 `[]` withdraw authority; 2 `[w]` reserve stake; 3 `[s,w]` lamports
/// source; 4 `[w]` user pool-token account; 5 `[w]` manager fee account; 6 `[w]` referrer
/// pool-token account; 7 `[w]` pool mint; 8 `[]` system program; 9 `[]` token program;
/// 10 `[s]` sol deposit authority (trailing optional — ALWAYS present for this pool; set at
/// `Initialize`, provided by `invoke_signed`).
#[allow(clippy::too_many_arguments)]
pub fn deposit_sol(
    stake_pool: &Pubkey,
    withdraw_authority: &Pubkey,
    reserve_stake: &Pubkey,
    lamports_from: &Pubkey,
    user_pool_token_account: &Pubkey,
    manager_fee_account: &Pubkey,
    referrer_pool_token_account: &Pubkey,
    pool_mint: &Pubkey,
    token_program: &Pubkey,
    sol_deposit_authority: &Pubkey,
    lamports: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(1 + 8);
    data.push(IX_DEPOSIT_SOL);
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: FUSION_STAKE_POOL_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*stake_pool, false),
            AccountMeta::new_readonly(*withdraw_authority, false),
            AccountMeta::new(*reserve_stake, false),
            AccountMeta::new(*lamports_from, true),
            AccountMeta::new(*user_pool_token_account, false),
            AccountMeta::new(*manager_fee_account, false),
            AccountMeta::new(*referrer_pool_token_account, false),
            AccountMeta::new(*pool_mint, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(*token_program, false),
            AccountMeta::new_readonly(*sol_deposit_authority, true),
        ],
        data,
    }
}

/// `DecreaseValidatorStakeWithReserve` (21). Accounts (instruction.rs:521-556 + builder
/// :936-973): 0 `[]` stake pool; 1 `[s]` staker; 2 `[]` withdraw authority; 3 `[w]` validator
/// list; 4 `[w]` reserve stake (pre-funds the transient's rent-exemption); 5 `[w]` validator
/// stake account; 6 `[w]` transient stake account; 7 `[]` clock; 8 `[]` stake history;
/// 9 `[]` system program; 10 `[]` stake program.
///
/// Upstream minimums (processor.rs:1296-1320): `lamports >= minimum_delegation` AND the
/// validator account must retain `>= rent + minimum_delegation` — the rebalance layer must
/// reconcile a clipped sub-minimum full drain with these rules (a residual below the minimum
/// can only leave via `RemoveValidatorFromPool`, which deactivates the whole account).
#[allow(clippy::too_many_arguments)]
pub fn decrease_validator_stake_with_reserve(
    stake_pool: &Pubkey,
    staker: &Pubkey,
    withdraw_authority: &Pubkey,
    validator_list: &Pubkey,
    reserve_stake: &Pubkey,
    validator_stake: &Pubkey,
    transient_stake: &Pubkey,
    lamports: u64,
    transient_stake_seed: u64,
) -> Instruction {
    let mut data = Vec::with_capacity(1 + 8 + 8);
    data.push(IX_DECREASE_VALIDATOR_STAKE_WITH_RESERVE);
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&transient_stake_seed.to_le_bytes());
    Instruction {
        program_id: FUSION_STAKE_POOL_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new_readonly(*stake_pool, false),
            AccountMeta::new_readonly(*staker, true),
            AccountMeta::new_readonly(*withdraw_authority, false),
            AccountMeta::new(*validator_list, false),
            AccountMeta::new(*reserve_stake, false),
            AccountMeta::new(*validator_stake, false),
            AccountMeta::new(*transient_stake, false),
            AccountMeta::new_readonly(sysvar::clock::ID, false),
            AccountMeta::new_readonly(sysvar::stake_history::ID, false),
            AccountMeta::new_readonly(system_program::ID, false),
            AccountMeta::new_readonly(STAKE_PROGRAM_ID, false),
        ],
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Distinct byte-pattern fixtures so any transposition or truncation shows in assertions.
    fn key(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    /// Assert one meta's (pubkey, signer, writable) shape.
    #[track_caller]
    fn assert_meta(ix: &Instruction, idx: usize, key: Pubkey, signer: bool, writable: bool) {
        let m = &ix.accounts[idx];
        assert_eq!(m.pubkey, key, "account #{idx} pubkey");
        assert_eq!(m.is_signer, signer, "account #{idx} signer flag");
        assert_eq!(m.is_writable, writable, "account #{idx} writable flag");
    }

    /// Byte-position pins are built BY HAND per the borsh spec (u8 enum tag, LE ints, Fee =
    /// denominator THEN numerator) — never through the same serializer — so encode-path drift
    /// cannot self-verify.
    #[test]
    fn initialize_data_and_metas() {
        let ix = initialize(
            &key(1),
            &key(2),
            &key(3),
            &key(4),
            &key(5),
            &key(6),
            &key(7),
            &key(8),
            &key(9),
            &key(10),
            Fee { denominator: 100, numerator: 1 },
            Fee { denominator: 10_000, numerator: 5 },
            Fee { denominator: 10_000, numerator: 5 },
            0,
            1_024,
        );
        assert_eq!(ix.program_id, FUSION_STAKE_POOL_PROGRAM_ID);
        // 1 disc + 3×16 Fee + 1 referral + 4 max_validators = 54 bytes.
        assert_eq!(ix.data.len(), 54);
        assert_eq!(ix.data[0], 0); // Initialize discriminant
        // epoch fee: denominator FIRST (100), then numerator (1).
        assert_eq!(&ix.data[1..9], &100u64.to_le_bytes());
        assert_eq!(&ix.data[9..17], &1u64.to_le_bytes());
        // withdrawal fee.
        assert_eq!(&ix.data[17..25], &10_000u64.to_le_bytes());
        assert_eq!(&ix.data[25..33], &5u64.to_le_bytes());
        // deposit fee.
        assert_eq!(&ix.data[33..41], &10_000u64.to_le_bytes());
        assert_eq!(&ix.data[41..49], &5u64.to_le_bytes());
        // referral fee, then max_validators u32 LE.
        assert_eq!(ix.data[49], 0);
        assert_eq!(&ix.data[50..54], &1_024u32.to_le_bytes());

        assert_eq!(ix.accounts.len(), 10);
        assert_meta(&ix, 0, key(1), false, true); // stake pool
        assert_meta(&ix, 1, key(2), true, false); // manager signs
        assert_meta(&ix, 2, key(3), false, false); // staker
        assert_meta(&ix, 3, key(4), false, false); // withdraw authority
        assert_meta(&ix, 4, key(5), false, true); // validator list
        assert_meta(&ix, 5, key(6), false, false); // reserve stake
        assert_meta(&ix, 6, key(7), false, true); // pool mint
        assert_meta(&ix, 7, key(8), false, true); // manager fee account
        assert_meta(&ix, 8, key(9), false, false); // token program
        assert_meta(&ix, 9, key(10), true, false); // deposit authority signs
    }

    #[test]
    fn add_validator_to_pool_data_and_metas() {
        let ix = add_validator_to_pool(
            &key(1),
            &key(2),
            &key(3),
            &key(4),
            &key(5),
            &key(6),
            &key(7),
            0x0102_0304,
        );
        assert_eq!(ix.data.len(), 5);
        assert_eq!(ix.data[0], 1);
        assert_eq!(&ix.data[1..5], &[0x04, 0x03, 0x02, 0x01]); // u32 LE
        assert_eq!(ix.accounts.len(), 13);
        assert_meta(&ix, 0, key(1), false, true); // stake pool
        assert_meta(&ix, 1, key(2), true, false); // staker
        assert_meta(&ix, 2, key(3), false, true); // reserve (funds the new account)
        assert_meta(&ix, 3, key(4), false, false); // withdraw authority
        assert_meta(&ix, 4, key(5), false, true); // validator list
        assert_meta(&ix, 5, key(6), false, true); // new validator stake account
        assert_meta(&ix, 6, key(7), false, false); // vote account
        assert_meta(&ix, 7, sysvar::rent::ID, false, false);
        assert_meta(&ix, 8, sysvar::clock::ID, false, false);
        assert_meta(&ix, 9, sysvar::stake_history::ID, false, false);
        assert_meta(&ix, 10, STAKE_CONFIG_ID, false, false);
        assert_meta(&ix, 11, system_program::ID, false, false);
        assert_meta(&ix, 12, STAKE_PROGRAM_ID, false, false);
    }

    #[test]
    fn remove_validator_from_pool_data_and_metas() {
        let ix = remove_validator_from_pool(&key(1), &key(2), &key(3), &key(4), &key(5), &key(6));
        assert_eq!(ix.data, vec![2]);
        assert_eq!(ix.accounts.len(), 8);
        assert_meta(&ix, 0, key(1), false, true); // stake pool
        assert_meta(&ix, 1, key(2), true, false); // staker
        assert_meta(&ix, 2, key(3), false, false); // withdraw authority
        assert_meta(&ix, 3, key(4), false, true); // validator list
        assert_meta(&ix, 4, key(5), false, true); // validator stake
        assert_meta(&ix, 5, key(6), false, true); // transient stake
        assert_meta(&ix, 6, sysvar::clock::ID, false, false);
        assert_meta(&ix, 7, STAKE_PROGRAM_ID, false, false);
    }

    #[test]
    fn increase_validator_stake_data_and_metas() {
        let ix = increase_validator_stake(
            &key(1),
            &key(2),
            &key(3),
            &key(4),
            &key(5),
            &key(6),
            &key(7),
            &key(8),
            0x0102_0304_0506_0708,
            0x1112_1314_1516_1718,
        );
        assert_eq!(ix.data.len(), 17);
        assert_eq!(ix.data[0], 4);
        assert_eq!(&ix.data[1..9], &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]); // lamports LE
        assert_eq!(&ix.data[9..17], &[0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11]); // seed LE
        assert_eq!(ix.accounts.len(), 14);
        assert_meta(&ix, 0, key(1), false, false); // stake pool READ-ONLY (unlike add/remove)
        assert_meta(&ix, 1, key(2), true, false); // staker
        assert_meta(&ix, 2, key(3), false, false); // withdraw authority
        assert_meta(&ix, 3, key(4), false, true); // validator list
        assert_meta(&ix, 4, key(5), false, true); // reserve
        assert_meta(&ix, 5, key(6), false, true); // transient
        assert_meta(&ix, 6, key(7), false, false); // validator stake (read-only on increase)
        assert_meta(&ix, 7, key(8), false, false); // vote account
        assert_meta(&ix, 8, sysvar::clock::ID, false, false);
        assert_meta(&ix, 9, sysvar::rent::ID, false, false);
        assert_meta(&ix, 10, sysvar::stake_history::ID, false, false);
        assert_meta(&ix, 11, STAKE_CONFIG_ID, false, false);
        assert_meta(&ix, 12, system_program::ID, false, false);
        assert_meta(&ix, 13, STAKE_PROGRAM_ID, false, false);
    }

    #[test]
    fn set_preferred_withdraw_validator_data_and_metas() {
        // Set: disc 5, type Withdraw (1), Option tag 1 + 32-byte vote key.
        let vote = key(9);
        let ix = set_preferred_withdraw_validator(&key(1), &key(2), &key(3), Some(&vote));
        assert_eq!(ix.data.len(), 35);
        assert_eq!(ix.data[0], 5);
        assert_eq!(ix.data[1], 1); // PreferredValidatorType::Withdraw — NEVER Deposit (0)
        assert_eq!(ix.data[2], 1); // Option::Some
        assert_eq!(&ix.data[3..35], vote.as_ref());
        assert_eq!(ix.accounts.len(), 3);
        assert_meta(&ix, 0, key(1), false, true); // stake pool
        assert_meta(&ix, 1, key(2), true, false); // staker
        assert_meta(&ix, 2, key(3), false, false); // validator list

        // Unset: Option tag 0, no key bytes.
        let ix = set_preferred_withdraw_validator(&key(1), &key(2), &key(3), None);
        assert_eq!(ix.data, vec![5, 1, 0]);
    }

    #[test]
    fn update_validator_list_balance_data_and_metas() {
        let pairs = [(key(10), key(11)), (key(12), key(13))];
        let ix = update_validator_list_balance(
            &key(1),
            &key(2),
            &key(3),
            &key(4),
            &pairs,
            0x0102_0304,
            true,
        );
        assert_eq!(ix.data.len(), 6);
        assert_eq!(ix.data[0], 6);
        assert_eq!(&ix.data[1..5], &[0x04, 0x03, 0x02, 0x01]); // start_index u32 LE
        assert_eq!(ix.data[5], 1); // no_merge bool
        assert_eq!(ix.accounts.len(), 7 + 4);
        assert_meta(&ix, 0, key(1), false, false); // stake pool read-only
        assert_meta(&ix, 1, key(2), false, false); // withdraw authority
        assert_meta(&ix, 2, key(3), false, true); // validator list
        assert_meta(&ix, 3, key(4), false, true); // reserve
        assert_meta(&ix, 4, sysvar::clock::ID, false, false);
        assert_meta(&ix, 5, sysvar::stake_history::ID, false, false);
        assert_meta(&ix, 6, STAKE_PROGRAM_ID, false, false);
        // Pairs in order, all writable.
        assert_meta(&ix, 7, key(10), false, true);
        assert_meta(&ix, 8, key(11), false, true);
        assert_meta(&ix, 9, key(12), false, true);
        assert_meta(&ix, 10, key(13), false, true);

        // no_merge = false encodes 0.
        let ix = update_validator_list_balance(&key(1), &key(2), &key(3), &key(4), &[], 7, false);
        assert_eq!(ix.data, vec![6, 7, 0, 0, 0, 0]);
        assert_eq!(ix.accounts.len(), 7);
    }

    #[test]
    fn update_stake_pool_balance_data_and_metas() {
        let ix = update_stake_pool_balance(
            &key(1),
            &key(2),
            &key(3),
            &key(4),
            &key(5),
            &key(6),
            &key(7),
        );
        assert_eq!(ix.data, vec![7]);
        assert_eq!(ix.accounts.len(), 7);
        assert_meta(&ix, 0, key(1), false, true); // stake pool
        assert_meta(&ix, 1, key(2), false, false); // withdraw authority
        assert_meta(&ix, 2, key(3), false, true); // validator list
        assert_meta(&ix, 3, key(4), false, false); // reserve READ-ONLY here
        assert_meta(&ix, 4, key(5), false, true); // manager fee account (epoch fee minted)
        assert_meta(&ix, 5, key(6), false, true); // pool mint
        assert_meta(&ix, 6, key(7), false, false); // token program
    }

    #[test]
    fn cleanup_removed_validator_entries_data_and_metas() {
        let ix = cleanup_removed_validator_entries(&key(1), &key(2));
        assert_eq!(ix.data, vec![8]);
        assert_eq!(ix.accounts.len(), 2);
        // Pool passed WRITABLE so upstream also resets a dangling preferred validator.
        assert_meta(&ix, 0, key(1), false, true);
        assert_meta(&ix, 1, key(2), false, true);
    }

    #[test]
    fn deposit_stake_data_and_metas() {
        let ix = deposit_stake(
            &key(1),
            &key(2),
            &key(3),
            &key(4),
            &key(5),
            &key(6),
            &key(7),
            &key(8),
            &key(9),
            &key(10),
            &key(11),
            &key(12),
        );
        assert_eq!(ix.data, vec![9]);
        assert_eq!(ix.accounts.len(), 15);
        assert_meta(&ix, 0, key(1), false, true); // stake pool
        assert_meta(&ix, 1, key(2), false, true); // validator list
        assert_meta(&ix, 2, key(3), true, false); // deposit authority SIGNS (custom authority)
        assert_meta(&ix, 3, key(4), false, false); // withdraw authority
        assert_meta(&ix, 4, key(5), false, true); // user stake account
        assert_meta(&ix, 5, key(6), false, true); // validator stake account
        assert_meta(&ix, 6, key(7), false, true); // reserve
        assert_meta(&ix, 7, key(8), false, true); // user pool-token account
        assert_meta(&ix, 8, key(9), false, true); // manager fee account
        assert_meta(&ix, 9, key(10), false, true); // referrer account
        assert_meta(&ix, 10, key(11), false, true); // pool mint
        assert_meta(&ix, 11, sysvar::clock::ID, false, false);
        assert_meta(&ix, 12, sysvar::stake_history::ID, false, false);
        assert_meta(&ix, 13, key(12), false, false); // token program
        assert_meta(&ix, 14, STAKE_PROGRAM_ID, false, false);
    }

    #[test]
    fn deposit_sol_data_and_metas() {
        let ix = deposit_sol(
            &key(1),
            &key(2),
            &key(3),
            &key(4),
            &key(5),
            &key(6),
            &key(7),
            &key(8),
            &key(9),
            &key(10),
            0x0102_0304_0506_0708,
        );
        assert_eq!(ix.data.len(), 9);
        assert_eq!(ix.data[0], 14);
        assert_eq!(&ix.data[1..9], &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]); // u64 LE
        assert_eq!(ix.accounts.len(), 11);
        assert_meta(&ix, 0, key(1), false, true); // stake pool
        assert_meta(&ix, 1, key(2), false, false); // withdraw authority
        assert_meta(&ix, 2, key(3), false, true); // reserve
        assert_meta(&ix, 3, key(4), true, true); // lamports source signs AND is writable
        assert_meta(&ix, 4, key(5), false, true); // user pool-token account
        assert_meta(&ix, 5, key(6), false, true); // manager fee account
        assert_meta(&ix, 6, key(7), false, true); // referrer account
        assert_meta(&ix, 7, key(8), false, true); // pool mint
        assert_meta(&ix, 8, system_program::ID, false, false);
        assert_meta(&ix, 9, key(9), false, false); // token program
        assert_meta(&ix, 10, key(10), true, false); // sol deposit authority signs (trailing)
    }

    #[test]
    fn decrease_validator_stake_with_reserve_data_and_metas() {
        let ix = decrease_validator_stake_with_reserve(
            &key(1),
            &key(2),
            &key(3),
            &key(4),
            &key(5),
            &key(6),
            &key(7),
            0x0102_0304_0506_0708,
            0x1112_1314_1516_1718,
        );
        assert_eq!(ix.data.len(), 17);
        assert_eq!(ix.data[0], 21);
        assert_eq!(&ix.data[1..9], &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(&ix.data[9..17], &[0x18, 0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11]);
        assert_eq!(ix.accounts.len(), 11);
        assert_meta(&ix, 0, key(1), false, false); // stake pool read-only
        assert_meta(&ix, 1, key(2), true, false); // staker
        assert_meta(&ix, 2, key(3), false, false); // withdraw authority
        assert_meta(&ix, 3, key(4), false, true); // validator list
        assert_meta(&ix, 4, key(5), false, true); // reserve (funds transient rent)
        assert_meta(&ix, 5, key(6), false, true); // validator stake
        assert_meta(&ix, 6, key(7), false, true); // transient stake
        assert_meta(&ix, 7, sysvar::clock::ID, false, false);
        assert_meta(&ix, 8, sysvar::stake_history::ID, false, false);
        assert_meta(&ix, 9, system_program::ID, false, false);
        assert_meta(&ix, 10, STAKE_PROGRAM_ID, false, false);
    }

    /// The allowlist is EXACTLY the eleven pinned discriminants — this module must never grow a
    /// builder without this test (and the audit doc table) changing in the same review.
    #[test]
    fn allowlist_discriminants_pinned() {
        assert_eq!(
            [
                IX_INITIALIZE,
                IX_ADD_VALIDATOR_TO_POOL,
                IX_REMOVE_VALIDATOR_FROM_POOL,
                IX_INCREASE_VALIDATOR_STAKE,
                IX_SET_PREFERRED_VALIDATOR,
                IX_UPDATE_VALIDATOR_LIST_BALANCE,
                IX_UPDATE_STAKE_POOL_BALANCE,
                IX_CLEANUP_REMOVED_VALIDATOR_ENTRIES,
                IX_DEPOSIT_STAKE,
                IX_DEPOSIT_SOL,
                IX_DECREASE_VALIDATOR_STAKE_WITH_RESERVE,
            ],
            [0u8, 1, 2, 4, 5, 6, 7, 8, 9, 14, 21],
        );
    }
}
