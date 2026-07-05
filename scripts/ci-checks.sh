#!/usr/bin/env bash
# Aggregate release gate: the single command CI runs — and that a dev runs locally before
# a deploy / PR — so the individual gate scripts can never be silently skipped. Each sub-gate is its
# own script (run them individually while iterating); this just chains them in the correct order and
# fails on the first one that fails.
#
#   scripts/ci-checks.sh            # run every gate
#   FAST=1 scripts/ci-checks.sh     # skip the slow re-builds in the stack gate's dev-oracle pass
#
# Order matters twice over: the integration tests need the `dev-oracle` .so, so that build runs
# FIRST — and the isolation gates run LAST because they rebuild + verify the PRODUCTION artifact,
# guaranteeing target/deploy/ never ends this script holding a dev-oracle .so a deploy could ship.
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

step "6/7  SBF stack-frame gate (production + dev-oracle configurations)"
scripts/check-stack-offsets.sh
scripts/check-stack-offsets.sh -- --features dev-oracle

# The isolation gates run LAST on purpose: check-no-dev-oracle.sh rebuilds the PRODUCTION .so/IDL
# and then scans them, so ci-checks always leaves a verified production artifact in target/ — a
# deploy straight after this script ships the real program, never the dev-oracle .so a feature
# build above left behind. Keep any step that runs `anchor build -- --features …` ABOVE this one.
step "7/7  isolation gates (production build must not expose dev_set_price or the cvlr/certora deps)"
scripts/check-no-dev-oracle.sh
scripts/check-no-certora.sh

echo
echo "================================================================"
echo "ALL RELEASE GATES PASSED."
echo "================================================================"
