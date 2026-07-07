#!/usr/bin/env bash
# Install the fUSD keeper suite as systemd units, templated to THIS repo checkout.
#
#   sudo keepers/deploy/install.sh [--user <runuser>]
#
# Installs fusd-oracle-crank / fusd-liquidator / fusd-monitor services + the fusd-alert-webhook
# timer, creates /etc/fusd/keeper.env (from keeper.env.example, if missing) and /var/lib/fusd.
# Deliberately does NOT enable or start anything — the start ORDER matters (deposit collateral
# before the first crank start); follow keepers/deploy/README.md.
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "run with sudo"; exit 1; }

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
RUNUSER="${SUDO_USER:-root}"
if [ "${1:-}" = "--user" ]; then RUNUSER="${2:?--user needs a value}"; fi
id "$RUNUSER" >/dev/null
# The runuser's real home — the units' InaccessiblePaths must mask ITS ~/.config/solana. NOTE: the
# systemd `%h` specifier resolves to /root in a SYSTEM unit regardless of User=, so we template the
# absolute path in instead.
RUNHOME="$(getent passwd "$RUNUSER" | cut -d: -f6)"
[ -n "$RUNHOME" ] || { echo "could not resolve home dir for user $RUNUSER"; exit 1; }

UNITS="fusd-oracle-crank.service fusd-liquidator.service fusd-monitor.service fusd-alert-webhook.service fusd-alert-webhook.timer"
for u in $UNITS; do
  sed -e "s|@REPO@|$REPO|g" -e "s|@RUNUSER@|$RUNUSER|g" -e "s|@HOME@|$RUNHOME|g" "$HERE/$u" > "/etc/systemd/system/$u"
done

mkdir -p /etc/fusd /var/lib/fusd
chown "$RUNUSER" /var/lib/fusd
if [ ! -f /etc/fusd/keeper.env ]; then
  cp "$HERE/keeper.env.example" /etc/fusd/keeper.env
  chmod 600 /etc/fusd/keeper.env
  chown "$RUNUSER" /etc/fusd/keeper.env
  echo ">> created /etc/fusd/keeper.env — EDIT IT (RPC url, wallet path, webhook url)"
fi

systemctl daemon-reload
echo ">> units installed for repo $REPO, user $RUNUSER."
echo ">> next: edit /etc/fusd/keeper.env, create + fund the keeper wallet, then follow"
echo ">>       keepers/deploy/README.md (start order matters — deposit before the first crank)."
