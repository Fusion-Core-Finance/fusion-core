#!/usr/bin/env bash
# Aggregate release gate: the single command CI runs — and that a dev runs locally before
# a deploy / PR — so the individual gate scripts can never be silently skipped. Each sub-gate is its
# own script (run them individually while iterating); this just chains them in the correct order and
# fails on the first one that fails.
#
#   scripts/ci-checks.sh            # run every gate
#   FAST=1 scripts/ci-checks.sh     # skip the slow re-builds in the stack gate's dev-oracle pass
#
# Order matters: the integration tests need the `dev-oracle` .so, while the production gates rebuild
# WITHOUT it — so the dev-oracle build + integration tests run FIRST, then the production gates last.
# NOTE: this does NOT run the full multi-hour Kani solver pass (that is `scripts/kani-audit.sh`, wired
# as a separate manual/scheduled CI job); here we run only its fast tag+artifact `--gate`.
set -euo pipefail
cd "$(dirname "$0")/.."

step() { echo; echo "===== $* ====="; }

step "1/7  Pure-crate host tests (fusd-math + fusd-oracle + fusd-core unit tests)"
# fusd-core's host-side unit tests carry the layout/discipline pins (the MarketParam borsh-tag
# pin, cdp/governance boundary algebra, Borsh SPACE pins) — they must run in CI, not just the
# litesvm lane.
cargo test -p fusd-math -p fusd-oracle -p fusd-core

step "2/7  Clippy lint gate (fusd-oracle, both feature configs; warnings are errors)"
cargo clippy -p fusd-oracle --all-targets -- -D warnings
cargo clippy -p fusd-oracle --all-targets --features pod -- -D warnings

step "3/7  Build the dev-oracle .so (needed by the litesvm integration tests)"
anchor build -- --features dev-oracle

step "4/7  In-process integration tests (litesvm)"
cargo test -p fusd-integration-tests

step "5/7  Kani strength + artifact gate (fast; no solver run)"
scripts/kani-audit.sh --gate

step "6/7  dev_set_price isolation gate (production build must not expose it)"
scripts/check-no-dev-oracle.sh

step "7/7  SBF stack-frame gate (production + dev-oracle configurations)"
scripts/check-stack-offsets.sh
scripts/check-stack-offsets.sh -- --features dev-oracle

echo
echo "================================================================"
echo "ALL RELEASE GATES PASSED."
echo "================================================================"
