#!/usr/bin/env bash
# Surfpool mainnet-fork SOLVENCY + PEG lifecycle runner.
#
# Boots surfpool forking Solana mainnet, deploys fusd_core into the fork, then drives the full
# lifecycle in tests/surfpool-lifecycle.ts against REAL forked Orca/Pyth/Switchboard accounts:
#   bootstrap → borrow (A/B/C) → Reactor Pool provide → price-drop liquidation (RP-offset waterfall)
#   → redemption (lowest rate bucket) → refresh_market (interest → buffer), asserting the global
#   supply identity (circulating == agg_recorded_debt − unminted_interest + bad_debt) after every step.
#
# This is the level-2 complement to tests/surfpool/run.sh (which proves the CLMM parser on the real
# Orca pool). Network-dependent (forks a mainnet RPC) → NOT part of the per-commit CI gate; run it
# before a release or wire it as a scheduled job.
#
# Prereqs: surfpool, anchor 0.32.1 (target/idl/fusd_core.json built), `npm i @solana/spl-token`,
# Node >= 18, a default keypair at ~/.config/solana/id.json (surfpool airdrops it on the fork).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
PROGRAM_ID="$(grep -oE 'declare_id!\("[^"]+"\)' programs/fusd-core/src/lib.rs | sed -E 's/.*"([^"]+)".*/\1/')"
# Match the DAEMON's command line ("surfpool start …"), not the bare word "surfpool" — the latter
# also matches this script's own path (tests/surfpool/run-lifecycle.sh), so it would SIGKILL itself.
trap 'pkill -9 -f "surfpool start" 2>/dev/null || true' EXIT

# Kill ANY prior surfpool and WAIT for port 8899 to actually free. An orphan holding the port makes a
# fresh boot silently fall back to STALE state — guard against it explicitly.
pkill -9 -f "surfpool start" 2>/dev/null || true
for _ in $(seq 1 20); do ss -ltn 2>/dev/null | grep -q ":8899" || break; sleep 1; done
if ss -ltn 2>/dev/null | grep -q ":8899"; then echo "!! port 8899 still busy after kill — aborting" >&2; exit 1; fi

echo ">> booting surfpool (mainnet fork) — deploying $PROGRAM_ID"
surfpool start --network mainnet --no-tui -y > /tmp/surfpool-lifecycle.log 2>&1 &

health() { curl -s http://127.0.0.1:8899 -X POST -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' 2>/dev/null | grep -q result; }
prog_ready() { curl -s http://127.0.0.1:8899 -X POST -H 'content-type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getAccountInfo\",\"params\":[\"$PROGRAM_ID\",{\"encoding\":\"base64\"}]}" \
  2>/dev/null | grep -q '"executable":true'; }

ok=0; for _ in $(seq 1 60); do health && { ok=1; break; }; sleep 2; done
[ "$ok" = 1 ] || { echo "!! RPC never came up"; tail -20 /tmp/surfpool-lifecycle.log; exit 1; }
echo ">> RPC healthy"
ok=0; for _ in $(seq 1 60); do prog_ready && { ok=1; break; }; sleep 2; done
[ "$ok" = 1 ] || { echo "!! program never deployed"; tail -20 /tmp/surfpool-lifecycle.log; exit 1; }
echo ">> fUSD deployed"

echo ">> running lifecycle harness"
npx ts-node tests/surfpool-lifecycle.ts
echo ">> done. (surfpool log: /tmp/surfpool-lifecycle.log)"
