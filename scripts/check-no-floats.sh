#!/usr/bin/env bash
# Release gate: NO f32/f64 in the production SBF path (programs/fusd-core, crates/fusd-math,
# crates/fusd-oracle). Floating point is non-deterministic across targets and truncates silently on
# an `as u64` cast — the exact silent-monetary-error class fusion-core structurally avoids by routing
# every money op through integer WAD/RAY fixed-point + bnum::U256. (Foil: Djed-Ergo's
# `IMPLEMENTOR_FEE_PERCENT: f64 = 0.0025` applied via `as u64`, ageusd-headless/src/parameters.rs:24.)
#
# Floats are allowed ONLY inside #[cfg(test)] code — e.g. the f64 REFERENCE oracle a proptest compares
# the integer sqrt-price decode against (crates/fusd-math/src/oracle_scale.rs). Heuristic: test modules
# live at end-of-file, so strip from the first `#[cfg(test)]` to EOF before scanning. A production float
# placed BEFORE any test module fails the gate.
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
while IFS= read -r -d '' f; do
  hits="$(sed '/#\[cfg(test)\]/,$d' "$f" | grep -nE '\bf32\b|\bf64\b' || true)"
  if [ -n "$hits" ]; then
    echo "FAIL: float type/cast in the production SBF path:" >&2
    printf '%s\n' "$hits" | sed "s|^|  $f:|" >&2
    fail=1
  fi
done < <(find programs/fusd-core/src crates/fusd-math/src crates/fusd-oracle/src crates/fusion-stake-math/src crates/fusion-stake-view/src -name '*.rs' -print0)

if [ "$fail" -ne 0 ]; then
  echo "No floats in the SBF money path — use integer WAD/RAY fixed-point + bnum::U256." >&2
  exit 1
fi
echo "OK: production SBF path (fusd-core / fusd-math / fusd-oracle / fusion-stake-*) is float-free."
