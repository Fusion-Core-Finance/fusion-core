#!/usr/bin/env bash
# Isolated Kani audit runner + merge gate for `fusd-math` (pre-audit formal-verification tier).
#
# WHAT IT DOES
#   - Enumerates every `#[kani::proof]` harness in crates/fusd-math/src/kani_proofs.rs and the
#     proof-strength tag that MUST sit on the line directly above each `#[kani::proof]`, as
#     `// strength: <TAG> — <justification>` (TAG in INDUCTIVE|STRONG|WEAK|UNIT_TEST|VACUOUS; the
#     rubric + the per-harness rationale live in crates/fusd-math/PROOF_STRENGTH.md). The
#     source markers are the single source of truth the runner and gate read.
#   - Counts each harness's `kani::cover!` calls in the source (lexically, comment lines skipped — so
#     covers must live INLINE in the harness body, never in a shared helper) and enforces that Kani
#     reported EVERY one satisfied ("N of N"): an unsatisfied cover is a vacuous branch Kani still
#     calls VERIFICATION SUCCESSFUL. A harness with zero covers must carry an explicit
#     `// covers: none — <reason>` marker on the line directly above its `// strength:` line.
#   - Runs each harness ONE AT A TIME, killing cbmc before/after so an orphaned solver can't poison
#     the next measurement (a real failure mode), with a per-harness timeout.
#   - Regenerates the tracked artifact crates/fusd-math/kani_audit.tsv
#     (columns: proof  cbmc_s  wall_s  status  covers  strength), stamped with a `# inputs_sha256=…`
#     header line binding it to the exact fusd-math sources, crate manifest, unwind bound, and locked
#     dep versions it was proven from (kani/cbmc versions ride along as provenance, not digest inputs).
#   - Acts as a MERGE GATE: exits non-zero if ANY harness is not PASS, ANY harness TIMED OUT, ANY
#     harness lacks a valid `// strength:` tag, ANY harness is tagged VACUOUS (a proof that may
#     prove nothing must be fixed or removed, never merged green), ANY harness's recorded covers
#     differ from the source-expected count, or the artifact's inputs digest no longer matches the tree.
#
# USAGE
#   scripts/kani-audit.sh                # run all harnesses, regenerate the TSV, then gate
#   BUDGET_S=1800 scripts/kani-audit.sh  # raise the per-harness timeout (default 900s)
#   scripts/kani-audit.sh --gate         # FAST gate only (no Kani run; needs cargo + sha256sum/shasum,
#                                        # NOT kani): re-check the source tags, recompute the proof-inputs
#                                        # digest against the TSV header, and assert the committed TSV
#                                        # shows PASS + the expected covers for every current harness
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

# Portable sha256: GNU coreutils `sha256sum` (Linux) or `shasum -a 256` (macOS, ships with perl).
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}

# --- proof-inputs digest -----------------------------------------------------------------------------
# One sha256 over everything that gives the PASS rows their meaning: every fusd-math source file
# (per-file hashes with paths, so renames/moves count), the crate manifest, the
# [[workspace.metadata.kani.proof]] unwind bound, and the exact locked dep versions of the kani lib
# build. The full run stamps it on the TSV; --gate recomputes it, so ANY drift in the proof inputs
# fails closed until a full run regenerates the artifact. Toolchain (kani/cbmc) versions are
# deliberately NOT digest inputs: the per-PR gate runs on CI runners that never install kani, and the
# weekly kani.yml full run owns toolchain-drift coverage (versions are header provenance instead).
compute_digest() {
  local deps
  # Locked dep versions of the kani lib build (normal deps only — dev-deps never enter the proof).
  # `sed 's/ (.*//'` strips cargo tree's machine-specific absolute-path suffix on path deps —
  # required, or local and CI digests diverge.
  deps="$(cargo tree -p "$PKG" -e normal --prefix none --locked 2>/dev/null | sed 's/ (.*//' | LC_ALL=C sort -u)"
  if [ -z "$deps" ]; then
    echo "GATE FAIL: cargo tree -p $PKG --locked failed — Cargo.lock stale?" >&2
    return 1
  fi
  local -a src_files
  mapfile -t src_files < <(find crates/fusd-math/src -name '*.rs' -type f | LC_ALL=C sort)
  {
    sha256 "${src_files[@]}" crates/fusd-math/Cargo.toml
    # The unwind bound, extracted alone so unrelated workspace-manifest churn (dep bumps) doesn't
    # force a re-prove. The window runs to the next '[' section line (or EOF); deleting the block
    # yields an empty extraction, which changes the digest — fail closed.
    sed -n '/^\[\[workspace\.metadata\.kani\.proof\]\]/,/^\[/p' Cargo.toml
    printf '%s\n' "$deps"
  } | sha256 | awk '{print $1}'
}

# --- parse (harness, strength-tag, expected-covers, cover-exempt) rows from the source ---------------
# The `// strength: TAG` line must be the line immediately above `#[kani::proof]`; a harness with no
# such tag is emitted with tag MISSING (a hard gate failure). Emits 4 tab-separated fields per
# harness: name, tag, expected cover count (the harness's `kani::cover!` calls, counted from source —
# derived, never a maintained table), and cover-exempt (1 iff a `// covers: none — <reason>` marker
# sits directly above the strength line). The comment-skip rule is LOAD-BEARING: prose mentions of
# kani::cover! (and commented-out covers) must not count, so covers must live inline on code lines.
parse_tags() {
  awk '
    /^[[:space:]]*\/\/[[:space:]]*covers:[[:space:]]*none/ { cn_pend=1; next }
    /^[[:space:]]*\/\/[[:space:]]*strength:/ {
      t=$0; sub(/.*strength:[[:space:]]*/, "", t); sub(/[[:space:]].*/, "", t); tag=t; have=1; next
    }
    /^[[:space:]]*\/\// { next }
    /#\[kani::proof\]/ { pend=1; next }
    pend==1 && $1=="fn" {
      if (cur != "") printf "%s\t%s\t%d\t%d\n", cur, curtag, nc, cexempt;
      cur=$2; sub(/\(.*/, "", cur);
      curtag=(have ? tag : "MISSING");
      nc=0; cexempt=cn_pend;
      pend=0; have=0; tag=""; cn_pend=0; next
    }
    { nc += gsub(/kani::cover!/, "&") }
    END { if (cur != "") printf "%s\t%s\t%d\t%d\n", cur, curtag, nc, cexempt }
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
  IFS=$'\t' read -r name tag exp cexempt <<< "$row"
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
  # Every harness must either contain >=1 kani::cover! or carry the explicit exemption marker.
  if [ "$exp" -eq 0 ] && [ "$cexempt" -ne 1 ]; then
    echo "GATE FAIL: harness '$name' has no kani::cover! and no '// covers: none — <reason>' exemption marker above its strength tag." >&2
    fail=1
  fi
  if [ "$exp" -gt 0 ] && [ "$cexempt" -eq 1 ]; then
    echo "GATE FAIL: harness '$name' carries '// covers: none' but has $exp kani::cover! call(s) — remove the stale marker." >&2
    fail=1
  fi
done

# --- --gate mode: verify the committed artifact covers every current harness with PASS, then stop ---
if [ "$GATE_ONLY" -eq 1 ]; then
  if [ ! -f "$ARTIFACT" ]; then
    echo "GATE FAIL: $ARTIFACT is missing; run scripts/kani-audit.sh (no --gate) to generate it." >&2
    exit 1
  fi
  # M-02: the artifact must have been generated from EXACTLY this tree's proof inputs.
  rec="$(sed -n '1s/^# inputs_sha256=\([0-9a-f]\{64\}\).*/\1/p' "$ARTIFACT")"
  if [ -z "$rec" ]; then
    echo "GATE FAIL: $ARTIFACT has no inputs_sha256 header — regenerate with a full scripts/kani-audit.sh run." >&2
    fail=1
  else
    cur="$(compute_digest)" || exit 1
    if [ "$cur" != "$rec" ]; then
      echo "GATE FAIL: proof-input digest mismatch (recorded ${rec:0:12}…, current ${cur:0:12}…) — fusd-math sources, bnum version, or the unwind bound changed since the last full run; re-run scripts/kani-audit.sh and commit the regenerated TSV." >&2
      fail=1
    fi
  fi
  for row in "${ROWS[@]}"; do
    IFS=$'\t' read -r name tag exp cexempt <<< "$row"
    status=$(awk -F'\t' -v n="$name" '$1==n{print $4}' "$ARTIFACT")
    if [ -z "$status" ]; then
      echo "GATE FAIL: harness '$name' is not in $ARTIFACT — re-run scripts/kani-audit.sh." >&2
      fail=1
    elif [ "$status" != "PASS" ]; then
      echo "GATE FAIL: harness '$name' last recorded status is '$status' (expected PASS)." >&2
      fail=1
    fi
    # M-03: the recorded covers must be exactly the "N of N" the source demands — one exact-string
    # compare catches an unsatisfied (vacuous) cover, source drift, AND malformed/missing output.
    # Skipped when the header is missing entirely (a pre-digest artifact is already rejected above
    # with the one actionable fix: regenerate) or the row is absent (already reported).
    if [ -n "$rec" ] && [ -n "$status" ]; then
      covers=$(awk -F'\t' -v n="$name" '$1==n{print $5}' "$ARTIFACT")
      if [ "$exp" -eq 0 ]; then want="no-cover"; else want="$exp of $exp cover properties satisfied"; fi
      if [ "$covers" != "$want" ]; then
        echo "GATE FAIL: harness '$name' covers: artifact records '$covers', source expects '$want' — unsatisfied (vacuous) cover or stale artifact; re-run scripts/kani-audit.sh." >&2
        fail=1
      fi
    fi
  done
  [ "$fail" -eq 0 ] && echo "GATE OK: ${#ROWS[@]} harnesses tagged, PASS, covers verified in $ARTIFACT (inputs digest ${rec:0:12}… matches)."
  exit "$fail"
fi

# --- full run: each harness in strict isolation -----------------------------------------------------
# Provenance + the pre-run digest: the header attests the proof inputs as they were when proving
# STARTED; the mid-run drift guard below rejects the artifact if they changed under the solver.
kani_ver="$(cargo kani --version 2>/dev/null | awk '{print $2}')"; kani_ver="${kani_ver:-unknown}"
cbmc_bin="$(command -v cbmc || true)"
[ -n "$cbmc_bin" ] || cbmc_bin="$HOME/.kani/kani-${kani_ver}/bin/cbmc" # kani's bundled cbmc
cbmc_ver="$("$cbmc_bin" --version 2>/dev/null | awk '{print $1}')"; cbmc_ver="${cbmc_ver:-unknown}"
digest="$(compute_digest)" || exit 1

echo "Running ${#ROWS[@]} Kani harnesses one at a time (budget ${BUDGET_S}s each)..."
tmp="$(mktemp)"
printf "# inputs_sha256=%s\tkani=%s\tcbmc=%s\tgenerated=%s\n" \
  "$digest" "$kani_ver" "$cbmc_ver" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$tmp"
printf "proof\tcbmc_s\twall_s\tstatus\tcovers\tstrength\n" >> "$tmp"
n=0
for row in "${ROWS[@]}"; do
  n=$((n + 1)); IFS=$'\t' read -r name tag exp cexempt <<< "$row"
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
  # M-03: a PASS whose covers aren't exactly the source-expected "N of N" hides a vacuous branch
  # (kani exits 0 with unsatisfied covers) — demote it so the run gate and any later --gate reject it.
  # The raw kani phrase still lands in column 5 unchanged (schema stable).
  if [ "$exp" -eq 0 ]; then want="no-cover"; else want="$exp of $exp cover properties satisfied"; fi
  if [ "$status" = "PASS" ] && [ "$covers" != "$want" ]; then
    status="COVER_FAIL"
    echo "COVER FAIL: harness '$name' — expected '$want', kani reported '$covers'." >&2
  fi
  printf "%s\t%s\t%s\t%s\t%s\t%s\n" "$name" "$cbmc" "$wall" "$status" "$covers" "$tag" >> "$tmp"
  printf "[%s] (%d/%d) %-46s -> %s (cbmc=%ss wall=%ss) [%s]\n" \
    "$(date +%H:%M:%S)" "$n" "${#ROWS[@]}" "$name" "$status" "$cbmc" "$wall" "$tag"
  pkill -9 cbmc 2>/dev/null || true
  rm -f "$log"
done

mv "$tmp" "$ARTIFACT"
# The artifact must correspond to ONE source state: reject it if the proof inputs drifted mid-run
# (a real failure mode for a multi-hour solver pass; the check itself is cheap).
if [ "$(compute_digest)" != "$digest" ]; then
  echo "GATE FAIL: proof inputs changed while the audit was running — the artifact does not correspond to a single source state; re-run." >&2
  fail=1
fi
echo "========================================================================"
column -t -s$'\t' "$ARTIFACT"
echo "========================================================================"

# --- run gate: every harness must be PASS (no TIMEOUT / FAIL / COVER_FAIL) --------------------------
while IFS=$'\t' read -r proof _cbmc _wall status _covers _strength; do
  case "$proof" in proof|"#"*) continue;; esac
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
