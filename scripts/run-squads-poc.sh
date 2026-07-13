#!/usr/bin/env bash
# This document describes an optional governance integration explored during development. Fusion
# Core does not depend on MetaDAO, futarchy or Squads. Any compatible signer or signer PDA may
# serve as the GovernanceGate inbound authority.
#
# Run the Squads -> fUSD governance PoC (optional integration; not required by any release gate)
# on a DEDICATED local validator. It demonstrates ONE possible external governance stack — the core
# validates only the signer/PDA at the GovernanceGate.
#
# Isolated from `anchor test` on purpose: the PoC sets the singleton `config.gov_authority` to a
# Squads vault PDA, which would collide with tests/fusd-core.ts in a shared validator. This script
# spins up its own validator preloaded with fusd-core + Squads V4 + the Squads ProgramConfig
# account, then runs ONLY tests/squads-gov-poc.ts.
#
# Requires Node >= 20 (the project's intended runtime) and the Squads fixtures
# (scripts/fetch-squads.sh).
set -euo pipefail
export PATH="$HOME/.nvm/versions/node/v20.20.0/bin:$PATH"
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"; cd "$DIR"

FUSD=HrhK2RRpMj4CaHS43dxCcnmt6xjt6w64cEDX7iAg9CkK
SQUADS=SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf
PCFG=BSTq9w3kZwNwpBXJEvTZz2G9ZTNyKBvoSeXMvwb4cNZr
RPC=http://127.0.0.1:8899
WALLET="${ANCHOR_WALLET:-$HOME/.config/solana/id.json}"

[ -f "$WALLET" ] || solana-keygen new --no-bip39-passphrase -s -o "$WALLET"
[ -f fixtures/squads_v4.so ] && [ -f fixtures/squads_program_config.json ] || scripts/fetch-squads.sh

echo "node: $(node --version) | anchor: $(anchor --version)"
echo "Building fusd-core (production)..."
anchor build >/dev/null

LEDGER="$(mktemp -d)"
echo "Starting validator (ledger $LEDGER)..."
solana-test-validator --reset --quiet --ledger "$LEDGER" \
  --bpf-program "$FUSD" target/deploy/fusd_core.so \
  --bpf-program "$SQUADS" fixtures/squads_v4.so \
  --account "$PCFG" fixtures/squads_program_config.json \
  --rpc-port 8899 &
VPID=$!
cleanup() { kill "$VPID" 2>/dev/null || true; wait "$VPID" 2>/dev/null || true; rm -rf "$LEDGER"; }
trap cleanup EXIT

echo "Waiting for RPC..."
up=0
for _ in $(seq 1 60); do
  if solana -u "$RPC" cluster-version >/dev/null 2>&1; then up=1; break; fi
  sleep 1
done
[ "$up" = 1 ] || { echo "validator did not come up"; tail -50 "$LEDGER"/*.log 2>/dev/null || true; exit 1; }

PUBKEY="$(solana -u "$RPC" address -k "$WALLET")"
solana -u "$RPC" airdrop 1000 "$PUBKEY" >/dev/null 2>&1 || true
echo "wallet $PUBKEY funded; running PoC test..."

ANCHOR_PROVIDER_URL="$RPC" ANCHOR_WALLET="$WALLET" \
  yarn run ts-mocha -p ./tsconfig.json -t 1000000 tests/squads-gov-poc.ts
