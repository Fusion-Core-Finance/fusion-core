#!/usr/bin/env bash
# Surfpool mainnet-fork BOT-SMOKE — a LIVE smoke of the permissionless keeper bots' own
# scan -> gate -> submit path (keepers/liquidator.ts, keepers/redeemer.ts).
#
# The bots' liquidate/redeem ACCOUNT shapes are already fork-proven by tests/surfpool-lifecycle.ts;
# what this adds is proof that the BOT PROCESSES themselves detect the right targets, pass their
# off-chain gates, and submit. Flow (one self-contained job — the sandbox SIGKILLs split fork jobs):
#   boot fork -> SETUP_ONLY lifecycle (leaves B underwater + fresh price, C in the lowest bucket)
#   -> run the liquidator one tick (asserts it liquidates B)
#   -> run the redeemer one tick with redeemAmountFusd>0 (asserts it redeems the lowest bucket).
# The bots run setInterval forever (immediate first tick), so a `timeout` after that tick captures
# one scan; success is the bot's own on-chain-confirmed log line.
#
# Prereqs (same as run-lifecycle.sh): surfpool, anchor 0.32.1 (target/idl/fusd_core.json built),
# `npm i @solana/spl-token`, Node >= 18, a default keypair at ~/.config/solana/id.json. Network-
# dependent (forks a mainnet RPC) -> NOT a per-commit gate; run before a release.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$ROOT"
PROGRAM_ID="$(grep -oE 'declare_id!\("[^"]+"\)' programs/fusd-core/src/lib.rs | sed -E 's/.*"([^"]+)".*/\1/')"
WSOL="So11111111111111111111111111111111111111112"
export ANCHOR_PROVIDER_URL="http://127.0.0.1:8899"
export ANCHOR_WALLET="${ANCHOR_WALLET:-$HOME/.config/solana/id.json}"
# Match the DAEMON's command line ("surfpool start …"), not the bare word "surfpool" — the latter
# also matches this script's own path (tests/surfpool/run-bot-smoke.sh), so it would SIGKILL itself.
trap 'pkill -9 -f "surfpool start" 2>/dev/null || true' EXIT

# Kill ANY prior surfpool and WAIT for port 8899 to free — an orphan holding it makes a fresh boot
# silently fall back to STALE state (the lesson from the lifecycle harness).
pkill -9 -f "surfpool start" 2>/dev/null || true
for _ in $(seq 1 20); do ss -ltn 2>/dev/null | grep -q ":8899" || break; sleep 1; done
if ss -ltn 2>/dev/null | grep -q ":8899"; then echo "!! port 8899 still busy after kill — aborting" >&2; exit 1; fi

echo ">> booting surfpool (mainnet fork) — deploying $PROGRAM_ID"
surfpool start --network mainnet --no-tui -y > /tmp/surfpool-botsmoke.log 2>&1 &

health() { curl -s http://127.0.0.1:8899 -X POST -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' 2>/dev/null | grep -q result; }
prog_ready() { curl -s http://127.0.0.1:8899 -X POST -H 'content-type: application/json' \
  -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"getAccountInfo\",\"params\":[\"$PROGRAM_ID\",{\"encoding\":\"base64\"}]}" \
  2>/dev/null | grep -q '"executable":true'; }

ok=0; for _ in $(seq 1 60); do health && { ok=1; break; }; sleep 2; done
[ "$ok" = 1 ] || { echo "!! RPC never came up"; tail -20 /tmp/surfpool-botsmoke.log; exit 1; }
echo ">> RPC healthy"
ok=0; for _ in $(seq 1 60); do prog_ready && { ok=1; break; }; sleep 2; done
[ "$ok" = 1 ] || { echo "!! program never deployed"; tail -20 /tmp/surfpool-botsmoke.log; exit 1; }
echo ">> fUSD deployed"

echo ">> setup: SETUP_ONLY lifecycle — leaving the fork bot-actionable"
SETUP_ONLY=1 npx ts-node tests/surfpool-lifecycle.ts || { echo "!! setup failed"; exit 1; }

LIQ_LOG=/tmp/botsmoke-liquidator.log
echo ">> liquidator: one tick (timeout 40s, default WSOL config)"
timeout 40 npx ts-node keepers/liquidator.ts > "$LIQ_LOG" 2>&1
if grep -q "liquidated" "$LIQ_LOG"; then
  echo ">> PASS — liquidator detected + liquidated the underwater position"
else
  echo "!! FAIL — liquidator did not liquidate. log:"; cat "$LIQ_LOG"; exit 1
fi

REDEEM_CFG=/tmp/botsmoke-redeemer.json
printf '{"scanIntervalSecs":30,"markets":[{"collateralMint":"%s","redeemAmountFusd":5}]}\n' "$WSOL" > "$REDEEM_CFG"
RED_LOG=/tmp/botsmoke-redeemer.log
echo ">> redeemer: one tick (timeout 40s, redeemAmountFusd=5)"
timeout 40 npx ts-node keepers/redeemer.ts "$REDEEM_CFG" > "$RED_LOG" 2>&1
if grep -q "redeemed" "$RED_LOG"; then
  echo ">> PASS — redeemer redeemed against the lowest rate bucket"
else
  echo "!! FAIL — redeemer did not redeem. log:"; cat "$RED_LOG"; exit 1
fi

echo ">> BOT-SMOKE PASSED — liquidator + redeemer both acted on a live mainnet fork."
