# CLMM pool layouts for `sample_twap` (verified 2026-06-04)

Byte-exact layouts for hand-parsing the spot price from Orca Whirlpool and Raydium CLMM
pool accounts inside `fusd-core` (no orca/raydium crate deps). Source-walked AND
empirically verified against live mainnet accounts (decoded SOL/USDC prices from the two
venues agreed within ~0.3%). Feeds the `DexTwap` observation ring (fusion-docs.md).

## Orca Whirlpool

- **Program:** `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc`
- **Account `Whirlpool`:** borsh (`#[account]`) — packed, no padding; `space = 653`.
- **Discriminator** (`sha256("account:Whirlpool")[0..8]`): `[63, 149, 209, 12, 225, 128, 99, 9]` (`3f95d10ce1806309`).

| field | type | offset |
|---|---|---|
| sqrt_price | u128 | 65 |
| token_mint_a | Pubkey | 101 |
| token_mint_b | Pubkey | 181 |

(Full walk: config 8, bump 40, tick_spacing 41, fee_tier_index_seed 43, fee_rate 45,
protocol_fee_rate 47, liquidity 49, sqrt_price 65, tick_current_index 81,
protocol_fee_owed_a/b 85/93, token_mint_a 101, token_vault_a 133, fee_growth_global_a 165,
token_mint_b 181.)

- `sqrt_price` = Q64.64 of `sqrt(token_b per token_a)` in raw units.
- `fee_tier_index_seed` (off 43) is a rename of always-present bytes — leading offsets stable since launch. The v2/adaptive-fee/`TokenBadge` accounts are *different account types* (different discriminators) — rejected automatically.
- Decode proof: `HJPjoWUrhoZzkNfRpHuieeFk9WcZWjwy6PBjZ81ngndJ` → WSOL/USDC, sqrt_price `4857170867873581308` → **69.33 USDC/SOL**.

## Raydium CLMM

- **Program:** `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK`
- **Account `PoolState`:** zero-copy **`#[repr(C, packed)]`** — `packed` suppresses ALL alignment padding, so offsets are a plain running sum (without `packed` the u128s would have forced 16-byte alignment gaps — it does NOT here). Mainnet `space = 1544`, but the tail has grown across versions — **never assert exact length**.
- **Discriminator** (`sha256("account:PoolState")[0..8]`): `[247, 237, 227, 245, 215, 195, 222, 70]` (`f7ede3f5d7c3de46`).

| field | type | offset |
|---|---|---|
| token_mint_0 | Pubkey | 73 |
| token_mint_1 | Pubkey | 105 |
| mint_decimals_0 | u8 | 233 |
| mint_decimals_1 | u8 | 234 |
| sqrt_price_x64 | u128 | 253 |

- `sqrt_price_x64` = Q64.64 of `sqrt(token_1 per token_0)`; Raydium enforces `mint_0 < mint_1`.

## Price formula (both)

`price_raw (quote per base, raw units) = sqrt_price^2 >> 128` — **sqrt_price² is up to
~256 bits: MUST go through U256 (`fusd_math::mul_div` family), never plain u128.**
Decimal adjustment `* 10^(dec_base − dec_quote)` to a human price; fUSD wants the
RAY-scaled price per native collateral unit (same scale as `Market.spot`), with the
inverse taken when the collateral sits on the quote side.

## Mandatory guard set (per parse)

1. `owner == <hardcoded program id>`;
2. first 8 bytes == discriminator;
3. `data.len() >=` minimum needed (Whirlpool ≥ 213; Raydium ≥ 269) — never exact equality (Raydium tail grows);
4. `sqrt_price` within Whirlpool's global bounds (`4295048016 ..= 79226673515401279992447579055`) and non-zero, checked BEFORE squaring;
5. pool mints must equal the expected collateral+quote pair (owner+discriminator alone doesn't bind the pool to the right asset);
6. token ordering decides price vs inverse (Orca a/b; Raydium 0/1 sorted).

Citations: `orca-so/whirlpools` `programs/whirlpool/src/state/whirlpool.rs`;
`raydium-io/raydium-clmm` `programs/amm/src/states/pool.rs`.
