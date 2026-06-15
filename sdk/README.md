# @fusd/sdk

TypeScript client for Fusion (FUSD).

- **PDA derivation** for every account (`deriveConfig`, `deriveMarket`, `derivePosition`,
  `deriveFusdMint`, `deriveMintAuthority`, the reactor/buffer/oracle/bitmap vaults, `deriveAta`, …).
  Seeds mirror `programs/fusd-core/src/constants.rs`.
- **`getProgram(provider)`** — an Anchor `Program` over the bundled production IDL
  (`src/idl/fusd_core.json`). Instruction builders (`program.methods.borrow(...)`) and typed account
  decoders (`program.account.position.fetch(...)`) come from Anchor; it auto-resolves most PDA seeds,
  so callers usually pass only the signer, ATAs, and oracle accounts.
- **Health math** (`positionHealth`, `currentDebt`, `collateralValue`, `maxDebt`, `isHealthy`,
  `collateralRatioBps`, `maxBorrow`) — pure BigInt, ports `cdp.rs`/`accrual.rs`, rounds against the
  protocol. `currentDebt`/`positionHealth` add interest accrued since `last_debt_update` so a UI shows
  the borrower's *live* debt/CR, not the stale recorded value. (Pending tier-2 redistribution is
  omitted — applied lazily on touch, zero in the common case; this is a display estimate.)

Validated: derivers reproduce the on-chain PDA addresses, and the math matches the `cdp.rs` tests.

## Regenerating the bundled IDL

The IDL is a committed copy of the **production** (non-dev) build. After changing the program:

```sh
anchor build            # writes target/idl/fusd_core.json (NO dev_set_price)
yarn --cwd sdk sync-idl # copies it into src/idl/
```
