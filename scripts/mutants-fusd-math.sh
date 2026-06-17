#!/usr/bin/env bash
# Reproducible cargo-mutants run over the fusd-math crate.
#
# DETERMINISM — the two-stage method (the sharp edge: cargo-mutants runs each mutant's suite EXACTLY ONCE,
# and the B8 proptests use an unpinned, system-entropy RNG seed, so a single run can spuriously report a
# probabilistically-caught mutant as "survived"):
#
#   Stage 1 — broad sweep (this script, default). Runs at the crate's TUNED in-code case counts
#     (`with_cases(..)`: 10k where determinism matters, 400 on the expensive full-U256 reactor_pool block
#     the authors deliberately bounded). A global `PROPTEST_CASES` override is NOT used: it would force the
#     bounded block 50x higher (≈92s/mutant, a ~5h run) while LOWERING the 10k blocks — strictly worse on
#     both axes. At 10k cases a behaviour-changing mutant is already near-certain to die in one run.
#   Stage 2 — survivor confirmation. EVERY survivor from stage 1 is re-run deterministically at a high case
#     count to rule out a flaky false gap, e.g.:
#         PROPTEST_CASES=50000 cargo mutants --config crates/fusd-math/mutants.toml \
#             -f crates/fusd-math/src/<file>.rs --in-diff /dev/null   # or target the one mutant
#     A survivor that dies under stage 2 was flaky (drop it); one that persists is a genuine survivor to
#     triage (real gap → add a test; equivalent → `// mutants: skip` + justification).
#
# PROPTEST_CASES override (stage 2): set it in the environment and this script passes it through to the
# test subprocess (verified to override the in-code `with_cases`). Unset by default (stage 1).
set -euo pipefail

cd "$(dirname "$0")/.."

# Honour a caller-provided PROPTEST_CASES (stage-2 confirmation); otherwise leave the in-code values.
if [[ -n "${PROPTEST_CASES:-}" ]]; then
  export PROPTEST_CASES
fi

exec cargo mutants \
  --config crates/fusd-math/mutants.toml \
  --jobs "${MUTANTS_JOBS:-6}" \
  "$@"
