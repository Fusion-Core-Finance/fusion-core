//! Create the fUSD mint's Metaplex token-metadata account (`CreateMetadataAccountV3` CPI), so
//! wallets/explorers display "Fusion Dollar / FUSD" instead of "Unknown Token".
//!
//! Why this must be a PROGRAM instruction: the fUSD mint is a PDA (`[b"fusd_mint"]`) whose mint
//! authority is the `[b"mint_authority"]` program PDA, and Metaplex requires the MINT AUTHORITY to
//! sign metadata creation — no off-chain key can ever produce that signature, so the program itself
//! CPIs `CreateMetadataAccountV3` with `invoke_signed` (the fusion-docs.md §"token mint" open item:
//! legacy SPL mint + a separate Metaplex metadata account).
//!
//! Gated on `ProtocolConfig.gov_authority` — the same admin lane as `oracle_admin::set_program_ids`:
//! this is DISPLAY-ONLY infrastructure, not a risk param. It cannot mint/move/freeze/seize funds and
//! never touches the liquidation/redemption math; the mint-authority PDA signs only the metadata
//! creation for its own mint, never a token instruction. So it sits on the `gov_authority` lane,
//! not the timelocked `MarketParam` lane.
//!
//! Supply-chain posture: the CPI is HAND-ROLLED (u8 enum discriminator + borsh args) instead of
//! adding the `mpl-token-metadata` crate — one display-only instruction is not worth a new
//! dependency surface in the verified build (see the `MPL_TOKEN_METADATA_PROGRAM_ID` doc). The
//! wire format is verified against the Metaplex source and pinned by the golden-vector test below.
//!
//! No one-shot flag is needed: Metaplex itself fails `CreateMetadataAccountV3` when the metadata
//! account already exists (the PDA is already initialized), so a second call reverts in the CPI.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::invoke_signed;
use anchor_spl::token::Mint;

use crate::constants::{
    CONFIG_SEED, FUSD_MINT_SEED, MINT_AUTHORITY_SEED, MPL_TOKEN_METADATA_PROGRAM_ID,
};
use crate::errors::FusdError;
use crate::state::ProtocolConfig;

/// Metaplex `MetadataInstruction::CreateMetadataAccountV3` — borsh enum variant **33** (a single
/// u8 discriminator; the token-metadata instruction enum predates Anchor's 8-byte scheme).
/// Verified against metaplex-foundation/mpl-token-metadata
/// `programs/token-metadata/program/src/instruction/mod.rs` (variant position + account list).
const CREATE_METADATA_ACCOUNT_V3_IX: u8 = 33;

/// Metaplex's own on-chain caps (token-metadata `MAX_NAME_LENGTH` / `MAX_SYMBOL_LENGTH` /
/// `MAX_URI_LENGTH`). Enforced here so an oversized arg fails at OUR boundary with a legible
/// `ParamOutOfBounds` instead of an opaque error deep inside the CPI.
const MAX_METADATA_NAME_LEN: usize = 32;
const MAX_METADATA_SYMBOL_LEN: usize = 10;
const MAX_METADATA_URI_LEN: usize = 200;

/// Minimal local mirror of Metaplex `DataV2` — exactly the wire shape, no `mpl-token-metadata`
/// dep. The three trailing `Option`s are ALWAYS `None` here (a stablecoin mint has no
/// creators/collection/uses); borsh encodes `None` as a single `0x00` byte regardless of the
/// payload type, so the unit `()` payload is wire-exact. The golden-vector test pins the bytes.
#[derive(AnchorSerialize, AnchorDeserialize)]
struct DataV2 {
    name: String,
    symbol: String,
    uri: String,
    seller_fee_basis_points: u16,
    creators: Option<()>,
    collection: Option<()>,
    uses: Option<()>,
}

/// Minimal local mirror of Metaplex `CreateMetadataAccountArgsV3` (the variant-33 payload).
#[derive(AnchorSerialize, AnchorDeserialize)]
struct CreateMetadataAccountArgsV3 {
    data: DataV2,
    /// `true` so the update authority (gov) can later fix name/symbol/uri via Metaplex's
    /// `UpdateMetadataAccountV2` — mutability is what keeps the metadata governable.
    is_mutable: bool,
    collection_details: Option<()>,
}

/// Build the full `CreateMetadataAccountV3` instruction data:
/// `u8 discriminator (33) ++ borsh(CreateMetadataAccountArgsV3)`.
fn instruction_data(name: &str, symbol: &str, uri: &str) -> Result<Vec<u8>> {
    let args = CreateMetadataAccountArgsV3 {
        data: DataV2 {
            name: name.to_owned(),
            symbol: symbol.to_owned(),
            uri: uri.to_owned(),
            seller_fee_basis_points: 0,
            creators: None,
            collection: None,
            uses: None,
        },
        is_mutable: true,
        collection_details: None,
    };
    let mut data = vec![CREATE_METADATA_ACCOUNT_V3_IX];
    args.serialize(&mut data)?;
    Ok(data)
}

#[event_cpi]
#[derive(Accounts)]
pub struct CreateFusdMetadata<'info> {
    /// MUST equal `config.gov_authority`. Doubles as the metadata account's rent payer (Metaplex's
    /// `payer` slot, signer + writable) and as the metadata UPDATE authority, so the display
    /// metadata stays fixable by governance (never by the program's mint-authority PDA, which
    /// could not sign an update from off-chain).
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Box<Account<'info, ProtocolConfig>>,

    /// The fUSD mint the metadata describes. Read-only in the CPI (only the metadata PDA is created).
    #[account(seeds = [FUSD_MINT_SEED], bump)]
    pub fusd_mint: Box<Account<'info, Mint>>,

    /// CHECK: the fUSD mint-authority PDA; here it signs the Metaplex CPI (the mint authority must
    /// sign metadata creation), never a token instruction.
    #[account(seeds = [MINT_AUTHORITY_SEED], bump)]
    pub mint_authority: UncheckedAccount<'info>,

    /// CHECK: the Metaplex metadata PDA for the fUSD mint, created BY the CPI (so it cannot be a
    /// typed account here). Pinned in the handler to
    /// `find_program_address([b"metadata", mpl_id, fusd_mint], mpl_id)` — Metaplex re-derives and
    /// enforces the same PDA, but failing at our boundary keeps the error legible and never
    /// forwards a caller-chosen account into an external program.
    #[account(mut)]
    pub metadata: UncheckedAccount<'info>,

    /// CHECK: pinned to the hardcoded Metaplex Token Metadata program id (never caller-supplied —
    /// a spoofed program would receive the mint-authority PDA's signature).
    #[account(address = MPL_TOKEN_METADATA_PROGRAM_ID)]
    pub token_metadata_program: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,

    pub rent: Sysvar<'info, Rent>,
}

/// Create the fUSD mint's Metaplex metadata account with the supplied `name`/`symbol`/`uri`.
/// One-time in effect (Metaplex rejects an already-initialized metadata PDA); the metadata stays
/// mutable by the gov update authority for later fixes.
pub fn handler(
    ctx: Context<CreateFusdMetadata>,
    name: String,
    symbol: String,
    uri: String,
) -> Result<()> {
    require_keys_eq!(
        ctx.accounts.authority.key(),
        ctx.accounts.config.gov_authority,
        FusdError::Unauthorized
    );

    // Metaplex's own length caps, enforced up front (see the MAX_METADATA_* doc).
    require!(name.len() <= MAX_METADATA_NAME_LEN, FusdError::ParamOutOfBounds);
    require!(symbol.len() <= MAX_METADATA_SYMBOL_LEN, FusdError::ParamOutOfBounds);
    require!(uri.len() <= MAX_METADATA_URI_LEN, FusdError::ParamOutOfBounds);

    // Pin the (unchecked, CPI-created) metadata account to the canonical Metaplex metadata PDA
    // for the fUSD mint.
    let fusd_mint_key = ctx.accounts.fusd_mint.key();
    let (expected_metadata, _) = Pubkey::find_program_address(
        &[b"metadata", MPL_TOKEN_METADATA_PROGRAM_ID.as_ref(), fusd_mint_key.as_ref()],
        &MPL_TOKEN_METADATA_PROGRAM_ID,
    );
    require_keys_eq!(
        ctx.accounts.metadata.key(),
        expected_metadata,
        FusdError::InvalidMetadataAccount
    );

    // CreateMetadataAccountV3 account list (order + flags verified against the Metaplex source's
    // shank annotations): metadata (w), mint (r), mint_authority (s), payer (s+w),
    // update_authority (optional_signer — a signer is only REQUIRED for verified creators, which
    // we never set, but ours signs anyway: it is the gov authority, already a Signer above),
    // system_program (r), rent (r — the documented optional trailing slot).
    let ix = Instruction {
        program_id: MPL_TOKEN_METADATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(ctx.accounts.metadata.key(), false),
            AccountMeta::new_readonly(fusd_mint_key, false),
            AccountMeta::new_readonly(ctx.accounts.mint_authority.key(), true),
            AccountMeta::new(ctx.accounts.authority.key(), true),
            AccountMeta::new_readonly(ctx.accounts.authority.key(), true),
            AccountMeta::new_readonly(ctx.accounts.system_program.key(), false),
            AccountMeta::new_readonly(ctx.accounts.rent.key(), false),
        ],
        data: instruction_data(&name, &symbol, &uri)?,
    };
    invoke_signed(
        &ix,
        &[
            ctx.accounts.metadata.to_account_info(),
            ctx.accounts.fusd_mint.to_account_info(),
            ctx.accounts.mint_authority.to_account_info(),
            // `authority` covers BOTH the payer and update_authority metas (same pubkey).
            ctx.accounts.authority.to_account_info(),
            ctx.accounts.system_program.to_account_info(),
            ctx.accounts.rent.to_account_info(),
            ctx.accounts.token_metadata_program.to_account_info(),
        ],
        &[&[MINT_AUTHORITY_SEED, &[ctx.bumps.mint_authority]]],
    )?;

    emit_cpi!(crate::events::FusdMetadataCreated {
        metadata: expected_metadata,
        update_authority: ctx.accounts.authority.key(),
        name,
        symbol,
        uri,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden-vector pin of the hand-rolled CreateMetadataAccountV3 wire format. The expectation
    /// is built BY HAND (never through the same serializer), so a refactor of the local structs,
    /// field order, or discriminator cannot silently drift the bytes the Metaplex program parses.
    #[test]
    fn create_metadata_v3_data_golden_vector() {
        let name = "Fusion Dollar";
        let symbol = "FUSD";
        let uri = "https://example.com/fusd.json";
        let data = instruction_data(name, symbol, uri).unwrap();

        // u8 discriminator (33) ++ borsh(CreateMetadataAccountArgsV3):
        //   DataV2 { name, symbol, uri (each: u32 LE length prefix ++ bytes),
        //            seller_fee_basis_points: 0u16 LE, creators/collection/uses: None (0x00 each) }
        //   ++ is_mutable: true (0x01) ++ collection_details: None (0x00)
        let mut expected: Vec<u8> = vec![33];
        expected.extend_from_slice(&(name.len() as u32).to_le_bytes());
        expected.extend_from_slice(name.as_bytes());
        expected.extend_from_slice(&(symbol.len() as u32).to_le_bytes());
        expected.extend_from_slice(symbol.as_bytes());
        expected.extend_from_slice(&(uri.len() as u32).to_le_bytes());
        expected.extend_from_slice(uri.as_bytes());
        expected.extend_from_slice(&0u16.to_le_bytes()); // seller_fee_basis_points
        expected.push(0); // creators: None
        expected.push(0); // collection: None
        expected.push(0); // uses: None
        expected.push(1); // is_mutable: true
        expected.push(0); // collection_details: None
        assert_eq!(data, expected);
    }

    /// The args round-trip through borsh: deserializing everything past the u8 discriminator
    /// reproduces the inputs, so the encode path and the declared struct shapes can't diverge.
    #[test]
    fn create_metadata_v3_args_round_trip() {
        let data = instruction_data("Fusion Dollar", "FUSD", "https://example.com/fusd.json")
            .unwrap();
        assert_eq!(data[0], CREATE_METADATA_ACCOUNT_V3_IX);
        let decoded = CreateMetadataAccountArgsV3::try_from_slice(&data[1..]).unwrap();
        assert_eq!(decoded.data.name, "Fusion Dollar");
        assert_eq!(decoded.data.symbol, "FUSD");
        assert_eq!(decoded.data.uri, "https://example.com/fusd.json");
        assert_eq!(decoded.data.seller_fee_basis_points, 0);
        assert!(decoded.data.creators.is_none());
        assert!(decoded.data.collection.is_none());
        assert!(decoded.data.uses.is_none());
        assert!(decoded.is_mutable);
        assert!(decoded.collection_details.is_none());
    }
}
