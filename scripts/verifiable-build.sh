#!/usr/bin/env bash
# Deterministic, verifiable build of fusd-core (fusion-docs.md).
# Requires `solana-verify` (https://github.com/otter-sec/solana-verifiable-build) + anchor/cargo (the
# isolation gates below run a production `anchor build`).
# Verifiable/release builds use overflow-checks + lto="fat" + codegen-units=1.
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v solana-verify >/dev/null 2>&1; then
  echo "solana-verify not found. Install: cargo install solana-verify" >&2
  exit 1
fi

# Isolation gates BEFORE the solana-verify build (not after): check-no-dev-oracle.sh does its own
# production `anchor build`, so running it after would overwrite target/deploy/fusd_core.so — the
# deterministic artifact we hash and ship — with a non-reproducible host build. Run here they prove
# the SOURCE carries no dev-oracle / cvlr-certora leak (the load-bearing structural guarantee) and
# fail the build before the expensive Docker build; solana-verify runs LAST so target/deploy holds
# exactly the artifact whose hash we print.
scripts/check-no-dev-oracle.sh
scripts/check-no-certora.sh

# Deterministic Docker build; emits the on-chain-comparable program hash.
solana-verify build --library-name fusd_core

echo
echo "Executable hash (compare against on-chain via 'solana-verify get-program-hash <PROGRAM_ID>'):"
solana-verify get-executable-hash target/deploy/fusd_core.so
