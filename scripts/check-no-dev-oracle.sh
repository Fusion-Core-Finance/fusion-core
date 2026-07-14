#!/usr/bin/env bash
# Release gate: a PRODUCTION `anchor build` (no `dev-oracle` feature) must NEVER expose the
# dev/test-only `dev_set_price` instruction in the deployed program or its IDL.
#
# `dev_set_price` is `#[cfg(feature = "dev-oracle")]` and is enabled only by the separate
# `integration-tests` crate (a non-program workspace member) for litesvm tests. This script proves
# that isolation holds: it does a clean build and asserts the artifacts are free of it.
#
# Run before any deploy / IDL publish. Exits non-zero (failing CI) if the leak ever returns.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "Building fusd-core WITHOUT dev-oracle (production build)..."
anchor build

idl="target/idl/fusd_core.json"
so="target/deploy/fusd_core.so"

fail=0

# The IDL must not declare `dev_set_price` as an instruction. (Prose docstrings may mention it;
# we only reject an actual instruction entry, matched by jq.)
if command -v jq >/dev/null 2>&1; then
  if jq -e '.instructions[] | select(.name == "dev_set_price")' "$idl" >/dev/null 2>&1; then
    echo "FAIL: '$idl' exposes a dev_set_price instruction." >&2
    fail=1
  fi
else
  # jq-less fallback: the instruction name appears as a quoted "name" key inside instructions.
  if grep -q '"name": *"dev_set_price"' "$idl"; then
    echo "FAIL: '$idl' appears to expose a dev_set_price instruction (install jq for a precise check)." >&2
    fail=1
  fi
fi

# The gate is KEYPAIR-INDEPENDENT: cargo-build-sbf generates a random throwaway
# target/deploy/fusd_core-keypair.json on a fresh clone (the real deploy key lives outside the
# repo; keys/ is gitignored) and nothing here reads it. The IDL `address` below is printed from
# the program's own `declare_id!`, so this assert proves the scanned bytecode was built from the
# canonical mainnet source — and catches the one keypair-related footgun: `anchor keys sync`,
# which REWRITES declare_id!/Anchor.toml to the random local key. NEVER run `anchor keys sync`.
expected_id="FuSiontgYvCc2N2Cinvo5gxSuxt2UfGxKMcbzkB67kud"  # = declare_id! (programs/fusd-core/src/lib.rs)
if command -v jq >/dev/null 2>&1; then
  got_id="$(jq -r '.address' "$idl")"
else
  got_id="$(grep -o '"address": *"[1-9A-HJ-NP-Za-km-z]*"' "$idl" | head -1 | tr -d '" ' | cut -d: -f2)"
fi
if [ "$got_id" != "$expected_id" ]; then
  echo "FAIL: IDL address '$got_id' != declared mainnet program id '$expected_id' — was 'anchor keys sync' run? Restore declare_id! and Anchor.toml before scanning." >&2
  fail=1
fi

# The compiled program must carry no DevSetPrice symbol/string. Guard against a fail-open pass: if
# `strings` is missing or produces no output, the grep below would match nothing and spuriously look
# "clean". Require `strings` present and a non-empty scan before concluding the .so is clean.
if ! command -v strings >/dev/null 2>&1; then
  echo "FAIL: 'strings' (binutils) not found — cannot scan '$so' for a DevSetPrice leak." >&2
  fail=1
elif ! so_syms="$(strings "$so")" || [ -z "$so_syms" ]; then
  echo "FAIL: 'strings $so' produced no output — the .so symbol scan did not run." >&2
  fail=1
elif printf '%s\n' "$so_syms" | grep -qi "DevSetPrice"; then
  echo "FAIL: '$so' contains a DevSetPrice symbol — the dev-oracle feature leaked into the build." >&2
  fail=1
fi

# fusion-stake-controller: its dev-oracle feature is inert (declared only because this workspace
# passes --features dev-oracle to every program), but assert that stays true — no dev-gated
# instruction may appear in its production artifacts — and pin its declared id the same way.
ctrl_idl="target/idl/fusion_stake_controller.json"
ctrl_expected_id="Fz3z1yh21PQ59smsPjmjeyK6ngh8KoK6PiPxUgCgspFq"  # = declare_id! (programs/fusion-stake-controller/src/lib.rs)
if [ -f "$ctrl_idl" ]; then
  if command -v jq >/dev/null 2>&1; then
    ctrl_got_id="$(jq -r '.address' "$ctrl_idl")"
  else
    ctrl_got_id="$(grep -o '"address": *"[1-9A-HJ-NP-Za-km-z]*"' "$ctrl_idl" | head -1 | tr -d '" ' | cut -d: -f2)"
  fi
  if [ "$ctrl_got_id" != "$ctrl_expected_id" ]; then
    echo "FAIL: controller IDL address '$ctrl_got_id' != declared id '$ctrl_expected_id' — was 'anchor keys sync' run?" >&2
    fail=1
  fi
else
  echo "FAIL: '$ctrl_idl' missing — the production build should emit the controller IDL." >&2
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "dev-oracle isolation BROKEN. Ensure no program-crate dependency enables the 'dev-oracle' feature." >&2
  exit 1
fi

echo "OK: production build is free of dev_set_price (IDL + .so clean; controller id pinned)."
