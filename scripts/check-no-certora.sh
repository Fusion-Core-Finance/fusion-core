#!/usr/bin/env bash
# Release gate: the Certora verification dependency (`cvlr`) and any `#[cfg(feature = "certora")]`
# spec code must NEVER reach the deployed program. Mirrors `check-no-dev-oracle.sh`.
#
# The PRIMARY, load-bearing guarantee is STRUCTURAL, exactly like dev-oracle: the `certora` feature +
# its optional `cvlr` dep are enabled ONLY by the Certora cloud build (certora/README.md), never by
# `default`, so a default `anchor build` / `cargo build-sbf` cannot pull them. This script ENFORCES
# that structure two ways:
#   1. `cargo tree -e normal` for fusd-core contains no `cvlr` crate (the real proof — catches an
#      accidental non-optional dep or a `certora` feature wired into `default`).
#   2. a best-effort scan of the compiled `.so` for a `cvlr`/`certora` string (a heuristic backstop;
#      lto='fat' can inline+strip a tiny helper's symbol, so step 1 + the workspace structure are the
#      actual guarantee, not this scan).
#
# Run before any deploy / IDL publish (wired into scripts/verifiable-build.sh and the certora.yml lane).
set -euo pipefail
cd "$(dirname "$0")/.."

# Engine-robust detector: a real `cargo tree` line is e.g. "│   ├── cvlr v0.5.0". Strip every tree
# glyph (any non-alnum/_/- char) to spaces, then match `cvlr` as an EXACT whitespace-delimited field.
# Using awk (not a `grep -E` anchored alternation) avoids the ugrep-vs-GNU ERE divergence that silently
# broke an earlier version of this gate (it passed OK on a ugrep box while cvlr was present).
detect_cvlr() {
  sed 's/[^[:alnum:]_-]/ /g' | awk '{ for (i = 1; i <= NF; i++) if ($i == "cvlr") { found = 1 } } END { exit !found }'
}

# SELF-TEST: prove the detector itself works on this machine's tools BEFORE trusting its verdict, so an
# engine/coreutils swap can never silently re-break the gate (the failure mode that motivated this).
if ! printf '%s\n' '|   +-- cvlr v0.5.0' | detect_cvlr; then
  echo "FAIL(self-test): the cvlr detector does not match a known-cvlr fixture line — gate is broken on this machine's tools." >&2
  exit 1
fi
if printf '%s\n' '|   +-- solana-cvlrx v1.0' | detect_cvlr; then
  echo "FAIL(self-test): the cvlr detector false-matches a lookalike crate ('solana-cvlrx') — too loose." >&2
  exit 1
fi

fail=0

# 1. The program's normal (non-dev, non-build) dependency tree must not contain `cvlr`.
echo "Checking 'cargo tree -e normal' for fusd-core has no cvlr crate..."
if cargo tree -p fusd-core -e normal 2>/dev/null | detect_cvlr; then
  echo "FAIL: 'cvlr' is a NORMAL dependency of fusd-core — the Certora dep is not verification-only." >&2
  cargo tree -p fusd-core -e normal 2>/dev/null | sed 's/[^[:alnum:]_-]/ /g' | grep -w cvlr >&2 || true
  fail=1
fi

# 2. Best-effort backstop: the compiled production program should carry no cvlr/certora string. (A
#    fixed-string scan — this simple alternation is engine-safe; only step 1's tree parse needed awk.)
so="target/deploy/fusd_core.so"
if [ ! -f "$so" ] || [ "${REBUILD:-0}" = "1" ]; then
  echo "Building fusd-core (production, default features) for the .so symbol scan..."
  anchor build
fi
if [ -f "$so" ]; then
  if strings "$so" | grep -qiF -e cvlr -e certora; then
    echo "FAIL: '$so' contains a cvlr/certora string — the certora feature likely leaked into the build." >&2
    fail=1
  fi
else
  echo "WARN: '$so' not found and no build performed; skipping the .so scan (run with REBUILD=1)." >&2
fi

if [ "$fail" -ne 0 ]; then
  echo "Certora isolation BROKEN. The 'cvlr' dep / 'certora' feature must stay verification-only (certora/README.md)." >&2
  exit 1
fi

echo "OK: production build is structurally free of cvlr/certora (dependency tree clean; .so scan clean)."
