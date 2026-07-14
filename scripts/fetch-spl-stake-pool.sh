#!/usr/bin/env bash
# Fetch the deployed upstream SPL Stake Pool program (SPoo1…) into fixtures/ for tests.
#
# The fuSOL stake-pool fork (vendor/spl-stake-pool) changes ONLY `declare_id!`, which is unused
# at runtime (every PDA derives from the runtime program_id argument) — so the mainnet-deployed
# upstream .so loaded AT THE FORK'S PROGRAM ID is behaviorally identical to a from-source fork
# build. Tests use this dump; the from-source build is a deploy-time step (see
# vendor/spl-stake-pool/UPSTREAM.md).
#
# fixtures/ is gitignored — re-run this script on a fresh clone (same pattern as fetch-squads.sh).
#
#   RPC_URL=<mainnet rpc> bash scripts/fetch-spl-stake-pool.sh
set -euo pipefail
cd "$(dirname "$0")/.."

UPSTREAM_ID="SPoo1Ku8WFXoNDMHPsrGSTSG1Y47rzgn41SLUNakuHy"
RPC_URL="${RPC_URL:-https://api.mainnet-beta.solana.com}"
mkdir -p fixtures

echo "Dumping $UPSTREAM_ID -> fixtures/spl_stake_pool.so"
solana program dump "$UPSTREAM_ID" fixtures/spl_stake_pool.so --url "$RPC_URL"
ls -l fixtures/spl_stake_pool.so
