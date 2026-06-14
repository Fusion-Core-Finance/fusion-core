#!/usr/bin/env bash
# Release gate: the program must contain NO function whose stack frame exceeds the 4 KB
# SBF limit.
#
# `anchor build` only *warns* on these ("Error: Function ... Stack offset of N exceeded max
# offset of 4096 ...") and still emits a `.so` — which then corrupts memory at runtime
# ("Access violation"), exactly how the unboxed `Liquidate` accounts (5504-byte
# `try_accounts` frame) broke every liquidate call. Worse, the margin is
# build-environment-dependent: a frame 56 bytes over passed on one toolchain build and
# aborted on another. This script turns that silent warning into a hard failure.
#
# Usage (run both configurations before any deploy / in CI):
#   ./scripts/check-stack-offsets.sh                          # production build
#   ./scripts/check-stack-offsets.sh -- --features dev-oracle # integration-test build
set -euo pipefail

cd "$(dirname "$0")/.."

log="$(mktemp)"
trap 'rm -f "$log"' EXIT

echo "Building fusd-core (anchor build $*)..."
anchor build "$@" 2>&1 | tee "$log"

if grep -q "Stack offset of" "$log"; then
  echo "" >&2
  echo "FAIL: a function exceeds the 4 KB SBF stack frame (see 'Stack offset of' above)." >&2
  echo "Box large Account payloads in the offending instruction's Accounts struct" >&2
  echo "(see init_market / init_reactor_pool / liquidate for the pattern)." >&2
  exit 1
fi

echo "OK: no stack-frame overflows."
