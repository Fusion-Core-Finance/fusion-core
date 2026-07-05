#!/usr/bin/env bash
# fUSD alert webhook — cron bridge from the monitor (keepers/monitor.ts) to a webhook.
# Polls /healthz (liveness — the oracle-crank-down tripwire: the market permissionlessly shuts
# down if the crank is stale >1h, so an unreachable monitor pages immediately) and /metrics.json
# (critical alerts), POSTing changes as Discord-compatible {"content": "..."} JSON (for Slack,
# change "content" to "text"). Dedup via a state file holding the last posted alert-set hash:
# posts only on change, plus an all-clear when criticals go from nonempty to empty.
#
# Env: WEBHOOK_URL (required), MONITOR_URL (default http://127.0.0.1:8787),
#      STATE_FILE (default /tmp/fusd-alert-state). Needs curl + jq.
#
# Crontab (every minute):
#   * * * * * WEBHOOK_URL=https://discord.com/api/webhooks/... /path/to/fusion-core/keepers/alert-webhook.sh
set -euo pipefail

MONITOR_URL="${MONITOR_URL:-http://127.0.0.1:8787}"
WEBHOOK_URL="${WEBHOOK_URL:?WEBHOOK_URL is required (Discord-compatible webhook)}"
STATE_FILE="${STATE_FILE:-/tmp/fusd-alert-state}"

# -f: an HTTP-rejected post (429 rate-limit, 400 oversize, rotated 404) must FAIL so set -e aborts
# BEFORE the state write and the next run retries — without it the alert is recorded as delivered
# and permanently swallowed. Content truncated to Discord's 2000-char cap (no retry-loop on 400).
post() { curl -sSf -m 10 -H 'content-type: application/json' --data "$(jq -cn --arg m "$1" '{content: $m[:2000]}')" "$WEBHOOK_URL" >/dev/null; }
last="$(cat "$STATE_FILE" 2>/dev/null || true)"

# 1) Liveness: an unreachable monitor means nobody is watching the crank.
if ! curl -sf -m 10 "$MONITOR_URL/healthz" >/dev/null 2>&1; then
  if [ "$last" != "down" ]; then
    post "fUSD monitor UNREACHABLE at $MONITOR_URL — check monitor + oracle-crank (market shuts down if the crank is stale >1h)"
  fi
  echo "down" >"$STATE_FILE"
  exit 0
fi

# 2) Critical alerts. healthz answers 200 before the first poll completes ("starting"), so a
#    failed metrics fetch here just means no snapshot yet — try again next run.
metrics="$(curl -sf -m 10 "$MONITOR_URL/metrics.json")" || exit 0
criticals="$(jq -r '[.alerts[] | select(.severity=="critical") | "[\(.scope)] \(.message)"] | join("\n")' <<<"$metrics")"
hash="$(printf '%s' "$criticals" | sha256sum | cut -d' ' -f1)"
empty_hash="$(printf '' | sha256sum | cut -d' ' -f1)"

if [ -n "$criticals" ] && [ "$hash" != "$last" ]; then
  post "fUSD CRITICAL alerts:
$criticals"
elif [ -z "$criticals" ] && [ -n "$last" ] && [ "$last" != "$empty_hash" ]; then
  post "fUSD all clear — no critical alerts." # nonempty→empty (or monitor recovered)
fi
echo "$hash" >"$STATE_FILE"
