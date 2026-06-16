#!/usr/bin/env bash
# Surfpool mainnet-fork oracle test runner.
#
# Boots surfpool forking Solana mainnet, deploys fusd_core into the fork, and runs the
# #[ignore]'d `surfpool_oracle` test — which drives `sample_twap` against the REAL, live
# Orca SOL/USDC whirlpool (forked account), proving our hand-rolled `clmm.rs` byte offsets
# match production reality. (The litesvm suite already covers the aggregation/mode logic
# hermetically, incl. the self-signed-quote `mode == Ok` path; the Switchboard gateway
# real-quote leg is the documented JS extension in this dir's README.)
#
# Network-dependent (forks from a mainnet RPC) → NOT part of the per-commit CI gate.
#
# Prereqs: surfpool, anchor 0.32.1, solana CLI, a funded-on-fork default keypair
# (~/.config/solana/id.json — surfpool airdrops it).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
LOG="$(mktemp)"
SURF_PID=""
cleanup() { [ -n "$SURF_PID" ] && kill "$SURF_PID" 2>/dev/null || true; pkill -f "surfpool start" 2>/dev/null || true; }
trap cleanup EXIT

# 1. Build the production program. The committed declare_id (FuSiont…) already matches
#    target/deploy/fusd_core-keypair.json, so NO `anchor keys sync` is needed — Anchor's
#    entrypoint enforces program_id == declare_id and they are already aligned. We assert
#    that below and bail (rather than silently rotating the id) if they ever drift.
echo ">> anchor build (production)"
anchor build >/dev/null

PROGRAM_ID="$(solana-keygen pubkey target/deploy/fusd_core-keypair.json)"
DECLARED="$(grep -oE 'declare_id!\("[^"]+"\)' programs/fusd-core/src/lib.rs | sed -E 's/.*"([^"]+)".*/\1/')"
if [ "$PROGRAM_ID" != "$DECLARED" ]; then
  echo "!! deploy keypair ($PROGRAM_ID) != declare_id ($DECLARED)." >&2
  echo "!! Align them before deploying (e.g. 'anchor keys sync', then rebuild)." >&2
  exit 1
fi

# 2. Boot surfpool forking mainnet; -y auto-deploys via the runbook (instant_surfnet
#    deployment writes program data directly — no chunked-tx retries on the fast simnet).
echo ">> starting surfpool (mainnet fork) — deploying $PROGRAM_ID"
surfpool start --network mainnet --no-tui -y >"$LOG" 2>&1 &
SURF_PID=$!

# 3. Wait for RPC health, then for the program to be executable.
for _ in $(seq 1 30); do
  curl -s http://127.0.0.1:8899 -X POST -H 'content-type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' 2>/dev/null | grep -q result && break
  sleep 1
done
for _ in $(seq 1 30); do
  curl -s http://127.0.0.1:8899 -X POST -H 'content-type: application/json' \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getAccountInfo\",\"params\":[\"$PROGRAM_ID\",{\"encoding\":\"base64\"}]}" \
    2>/dev/null | grep -q '"executable":true' && { echo ">> program deployed"; break; }
  sleep 1
done

# 4. Run the ignored test against the fork.
echo ">> running surfpool_oracle"
SURFPOOL_RPC=http://127.0.0.1:8899 \
  cargo test -p fusd-integration-tests --test surfpool_oracle -- --ignored --nocapture

echo ">> done. (surfpool log: $LOG)"
