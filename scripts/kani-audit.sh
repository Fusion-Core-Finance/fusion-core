#!/usr/bin/env bash
# Isolated Kani audit runner + merge gate for `fusd-math` (pre-audit formal-verification tier).
#
# WHAT IT DOES
#   - Enumerates every `#[kani::proof]` harness in crates/fusd-math/src/kani_proofs.rs and the
#     proof-strength tag that MUST sit on the line directly above each `#[kani::proof]`, as
#     `// strength: <TAG> — <justification>` (TAG in INDUCTIVE|STRONG|WEAK|UNIT_TEST|VACUOUS; the
#     rubric + the per-harness rationale live in crates/fusd-math/PROOF_STRENGTH.md). The
#     source markers are the single source of truth the runner and gate read.
#   - Runs each harness ONE AT A TIME, killing cbmc before/after so an orphaned solver can't poison
#     the next measurement (a real failure mode), with a per-harness timeout.
#   - Regenerates the tracked artifact crates/fusd-math/kani_audit.tsv
#     (columns: proof  cbmc_s  wall_s  status  covers  strength).
#   - Acts as a MERGE GATE: exits non-zero if ANY harness is not PASS, ANY harness TIMED OUT, ANY
#     harness lacks a valid `// strength:` tag, or ANY harness is tagged VACUOUS (a proof that may
#     prove nothing must be fixed or removed, never merged green).
#
# USAGE
#   scripts/kani-audit.sh                # run all harnesses, regenerate the TSV, then gate
#   BUDGET_S=1800 scripts/kani-audit.sh  # raise the per-harness timeout (default 900s)
#   scripts/kani-audit.sh --gate         # FAST gate only (no Kani run): re-check the source tags and
#                                        # assert the committed TSV shows PASS for every current harness
#
# Vendored from scripts/isolated_full_audit.sh + scripts/audit-proof-strength.md.
set -uo pipefail
cd "$(dirname "$0")/.."

PKG="fusd-math"
SRC="crates/fusd-math/src/kani_proofs.rs"
ARTIFACT="crates/fusd-math/kani_audit.tsv"
BUDGET_S="${BUDGET_S:-900}"
VALID_TAGS="INDUCTIVE STRONG WEAK UNIT_TEST VACUOUS"
GATE_ONLY=0
[ "${1:-}" = "--gate" ] && GATE_ONLY=1

# Portable per-harness timeout: GNU `timeout` (Linux) or `gtimeout` (macOS + coreutils). If neither is
# installed, run without a budget — a hung harness then won't be auto-killed (install coreutils for
# `gtimeout` to restore enforcement on macOS). Expanded as an array so the empty case is a clean no-op.
TIMEOUT_BIN="$(command -v timeout || command -v gtimeout || true)"
if [ -n "$TIMEOUT_BIN" ]; then
  TIMEOUT_CMD=("$TIMEOUT_BIN" --kill-after=30 "$BUDGET_S")
else
  TIMEOUT_CMD=()
  echo "WARN: no 'timeout'/'gtimeout' on PATH — running each harness without a per-harness budget." >&2
fi

# --- parse (harness, strength-tag) pairs from the source (the single source of truth) ---------------
# The `// strength: TAG` line must be the line immediately above `#[kani::proof]`; a harness with no
# such tag is emitted with tag MISSING (a hard gate failure).
parse_tags() {
  awk '
    /^[[:space:]]*\/\/[[:space:]]*strength:/ {
      t=$0; sub(/.*strength:[[:space:]]*/, "", t); sub(/[[:space:]].*/, "", t); tag=t; have=1; next
    }
    /#\[kani::proof\]/ { pend=1; next }
    pend==1 && $1=="fn" {
      name=$2; sub(/\(.*/, "", name);
      printf "%s\t%s\n", name, (have ? tag : "MISSING");
      pend=0; have=0; tag=""; next
    }
  ' "$SRC"
}

valid_tag() { case " $VALID_TAGS " in *" $1 "*) return 0;; *) return 1;; esac; }

mapfile -t ROWS < <(parse_tags)
if [ "${#ROWS[@]}" -eq 0 ]; then
  echo "FAIL: no #[kani::proof] harnesses found in $SRC" >&2
  exit 1
fi

fail=0

# --- source-tag gate (always runs; the only thing --gate needs to recompute) -----------------------
for row in "${ROWS[@]}"; do
  name="${row%%$'\t'*}"; tag="${row#*$'\t'}"
  if [ "$tag" = "MISSING" ]; then
    echo "GATE FAIL: harness '$name' has no '// strength:' tag above its #[kani::proof]." >&2
    fail=1
  elif ! valid_tag "$tag"; then
    echo "GATE FAIL: harness '$name' has invalid strength tag '$tag' (allowed: $VALID_TAGS)." >&2
    fail=1
  elif [ "$tag" = "VACUOUS" ]; then
    echo "GATE FAIL: harness '$name' is tagged VACUOUS — a proof that may prove nothing must not merge green." >&2
    fail=1
  fi
done

# --- --gate mode: verify the committed artifact covers every current harness with PASS, then stop ---
if [ "$GATE_ONLY" -eq 1 ]; then
  if [ ! -f "$ARTIFACT" ]; then
    echo "GATE FAIL: $ARTIFACT is missing; run scripts/kani-audit.sh (no --gate) to generate it." >&2
    exit 1
  fi
  for row in "${ROWS[@]}"; do
    name="${row%%$'\t'*}"
    status=$(awk -F'\t' -v n="$name" '$1==n{print $4}' "$ARTIFACT")
    if [ -z "$status" ]; then
      echo "GATE FAIL: harness '$name' is not in $ARTIFACT — re-run scripts/kani-audit.sh." >&2
      fail=1
    elif [ "$status" != "PASS" ]; then
      echo "GATE FAIL: harness '$name' last recorded status is '$status' (expected PASS)." >&2
      fail=1
    fi
  done
  [ "$fail" -eq 0 ] && echo "GATE OK: ${#ROWS[@]} harnesses tagged and PASS in $ARTIFACT."
  exit "$fail"
fi

# --- full run: each harness in strict isolation -----------------------------------------------------
echo "Running ${#ROWS[@]} Kani harnesses one at a time (budget ${BUDGET_S}s each)..."
tmp="$(mktemp)"
printf "proof\tcbmc_s\twall_s\tstatus\tcovers\tstrength\n" > "$tmp"
n=0
for row in "${ROWS[@]}"; do
  n=$((n + 1)); name="${row%%$'\t'*}"; tag="${row#*$'\t'}"
  pkill -9 cbmc 2>/dev/null || true; sleep 1
  log="$(mktemp)"; start="$(date +%s)"
  if "${TIMEOUT_CMD[@]}" cargo kani -p "$PKG" --harness "$name" --output-format regular > "$log" 2>&1; then
    status="PASS"
  else
    ec=$?
    if [ "$ec" -eq 124 ] || [ "$ec" -eq 137 ]; then status="TIMEOUT"; else status="FAIL($ec)"; fi
  fi
  wall=$(( $(date +%s) - start ))
  cbmc=$(grep -oE 'Verification Time: [0-9.]+s' "$log" | grep -oE '[0-9.]+' | head -1); cbmc="${cbmc:-NA}"
  covers=$(grep -oE '[0-9]+ of [0-9]+ cover properties satisfied' "$log" | head -1); covers="${covers:-no-cover}"
  printf "%s\t%s\t%s\t%s\t%s\t%s\n" "$name" "$cbmc" "$wall" "$status" "$covers" "$tag" >> "$tmp"
  printf "[%s] (%d/%d) %-46s -> %s (cbmc=%ss wall=%ss) [%s]\n" \
    "$(date +%H:%M:%S)" "$n" "${#ROWS[@]}" "$name" "$status" "$cbmc" "$wall" "$tag"
  pkill -9 cbmc 2>/dev/null || true
  rm -f "$log"
done

mv "$tmp" "$ARTIFACT"
echo "========================================================================"
column -t -s$'\t' "$ARTIFACT"
echo "========================================================================"

# --- run gate: every harness must be PASS (no TIMEOUT / FAIL) ---------------------------------------
while IFS=$'\t' read -r proof _cbmc _wall status _covers _strength; do
  [ "$proof" = "proof" ] && continue
  if [ "$status" != "PASS" ]; then
    echo "GATE FAIL: harness '$proof' status '$status' (expected PASS)." >&2
    fail=1
  fi
done < "$ARTIFACT"

if [ "$fail" -eq 0 ]; then
  echo "GATE OK: ${#ROWS[@]} harnesses — all PASS, all tagged, none VACUOUS."
else
  echo "GATE FAIL: see above. (artifact written to $ARTIFACT for inspection)" >&2
fi
exit "$fail"
