#!/usr/bin/env bash
# Fetch the Squads V4 program + its ProgramConfig account, the test fixtures for the
# MetaDAO -> Squads -> fUSD localnet PoC. Both are gitignored (fetched, not
# committed); re-run to (re)create them. Optional arg: an RPC URL (defaults to mainnet-beta).
#
#   scripts/fetch-squads.sh [RPC_URL]
set -euo pipefail
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
URL="${1:-https://api.mainnet-beta.solana.com}"
PROGRAM=SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf   # Squads V4 (mainnet)
PROGRAM_CONFIG=BSTq9w3kZwNwpBXJEvTZz2G9ZTNyKBvoSeXMvwb4cNZr  # PDA [b"multisig", b"program_config"]
mkdir -p "$DIR/fixtures"

echo "Dumping Squads V4 program ($PROGRAM) from $URL ..."
solana program dump "$PROGRAM" "$DIR/fixtures/squads_v4.so" --url "$URL"
echo "  wrote fixtures/squads_v4.so ($(wc -c < "$DIR/fixtures/squads_v4.so") bytes)"

echo "Dumping Squads ProgramConfig account ($PROGRAM_CONFIG) ..."
solana account "$PROGRAM_CONFIG" --output json --output-file "$DIR/fixtures/squads_program_config.json" --url "$URL"
echo "  wrote fixtures/squads_program_config.json"
