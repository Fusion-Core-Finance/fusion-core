# Security Policy

Fusion is a trustless, permissionless protocol issuing the Fusion Dollar (FUSD), a Solana-native
overcollateralized CDP stablecoin. Its safety story rests on code, not policy — but responsible
disclosure of vulnerabilities still matters, especially while the protocol is pre-deployment and
hardening toward audit. (The program and crates keep their `fusd` names — code identity is
unchanged by branding.)

## Reporting a vulnerability

**Please report privately — do not open a public issue for a security bug.**

- Preferred: open a private advisory via GitHub →
  [**Report a vulnerability**](https://github.com/Fusion-Core-Finance/fusion-core/security/advisories/new).
- The same contact is published on-chain in the deployed program's `security.txt`
  (Neodyme format), so it can be found directly from the program account.

Please include: the affected component (program/instruction or `crates/fusd-math`/`fusd-oracle`),
a description and impact, and a proof-of-concept or failing test if you have one (the in-process
`litesvm` harness under `integration-tests/` is the fastest way to demonstrate a finding).

We aim to acknowledge a report within a few days. Coordinated disclosure: please give us a
reasonable window to ship and verify a fix before any public write-up.

## Scope

In scope:

- The on-chain program `fusd-core` (`programs/fusd-core`) — all instructions, account validation,
  and state transitions.
- The shared logic crates `crates/fusd-math` (fixed-point, interest, P/S Reactor Pool,
  redistribution, rate-bucket) and `crates/fusd-oracle` (Pyth/Switchboard/DEX-TWAP aggregation).

Out of scope (for now): the off-chain `keepers/`, `sdk/`, and `tests/` tooling; third-party
dependencies (report those upstream); and anything requiring a trusted/compromised governance or
upgrade authority key (those trust assumptions are documented in `docs/fusion-docs.md` §7–§8).

## Status & bounty

The protocol is **pre-deployment** and pre-audit (`docs/fusion-docs.md` §11). There is no formal bug
bounty yet; a program will be established before a guarded mainnet launch. Until then, disclosures
are handled on a best-effort, good-faith basis, and meaningful reports will be credited (with your
permission).

## Verifying what is deployed

Once deployed, the on-chain program is built reproducibly (`scripts/verifiable-build.sh` via
`solana-verify`). Confirm an on-chain program matches this source by comparing hashes:

```
solana-verify get-executable-hash target/deploy/fusd_core.so
solana-verify get-program-hash <PROGRAM_ID>
```

The FUSD mint's authorities (`freeze_authority = None`, mint authority = a program PDA) and the
program's upgrade-authority posture are verifiable on-chain; see `docs/fusion-docs.md` §8.
