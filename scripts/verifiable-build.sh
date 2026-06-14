#!/usr/bin/env bash
# Deterministic, verifiable build of fusd-core (fusion-docs.md).
# Requires `solana-verify` (https://github.com/otter-sec/solana-verifiable-build).
# Verifiable/release builds use overflow-checks + lto="fat" + codegen-units=1.
set -euo pipefail

cd "$(dirname "$0")/.."

if ! command -v solana-verify >/dev/null 2>&1; then
  echo "solana-verify not found. Install: cargo install solana-verify" >&2
  exit 1
fi

# Deterministic Docker build; emits the on-chain-comparable program hash.
solana-verify build --library-name fusd_core

echo
echo "Executable hash (compare against on-chain via 'solana-verify get-program-hash <PROGRAM_ID>'):"
solana-verify get-executable-hash target/deploy/fusd_core.so
