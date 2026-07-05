# fUSD keeper suite — server deployment

Systemd packaging for the 24/7 keeper set against mainnet. What runs:

| Unit | What it does | Sends txs? | Burn |
|---|---|---|---|
| `fusd-oracle-crank` | Switchboard update + `sample_twap` + `update_price` + `refresh_market`, cadences derived on-chain | yes | ~0.035 SOL/day (base + priority) |
| `fusd-liquidator` | scans positions, liquidates under `debt_spot` MCR | on liquidation | ~0 baseline |
| `fusd-monitor` | read-only dashboard/metrics/healthz on `127.0.0.1:8787` | never | 0 |
| `fusd-alert-webhook.timer` | every minute: monitor liveness + critical alerts → Discord webhook | — | 0 |

Deliberately **not** installed: `redeemer.ts` (an operator-gated peg-defense tool — a no-op
without below-peg-sourced fUSD) and `keeper.ts` (superseded by oracle-crank; do not co-run).

## Install

```bash
# 1. Node >= 20 (nvm is fine — the units start via a login shell) + the repo
git clone https://github.com/Fusion-Core-Finance/fusion-core && cd fusion-core
yarn install   # no anchor build needed: keepers fall back to the committed sdk/src/idl

# 2. Keeper wallet — fresh, NEVER the gov/deployer key. Fund ~1 SOL (≈1 month of cranking).
sudo solana-keygen new -o /etc/fusd/keeper.json --no-bip39-passphrase
# fund it, then: sudo chown <runuser> /etc/fusd/keeper.json && sudo chmod 600 /etc/fusd/keeper.json

# 3. Units + env
sudo keepers/deploy/install.sh            # templates units to this checkout + $SUDO_USER
sudoedit /etc/fusd/keeper.env             # RPC url, wallet path, webhook url
```

## Start order — this matters

> **Do not start the crank on an empty market.** On-chain `tcr_breach` has no dust floor: the
> market's standing interest dust with ZERO collateral becomes permissionlessly, **irreversibly**
> shutdown-eligible the moment the price goes fresh. Deposit first — deposits work under a stale
> price, and any nonzero collateral defeats the check.

```bash
sudo systemctl enable --now fusd-monitor fusd-alert-webhook.timer   # watchers first
# >>> deposit collateral into the market now (any wallet; web app Mint window with mint amount 0,
# >>> or scripts) and confirm total_collateral > 0 on the dashboard <<<
sudo systemctl enable --now fusd-oracle-crank
# wait for feeds to go fresh — mints unfreeze after ~3 TWAP samples (~5-6 min)
sudo systemctl enable --now fusd-liquidator
```

## Verify

```bash
systemctl status fusd-oracle-crank fusd-liquidator fusd-monitor
journalctl -fu fusd-oracle-crank        # expect ✓ sb / sample_twap / update_price / refresh_market
curl -s 127.0.0.1:8787/healthz          # "ok" (503 = monitor blind — the webhook will page)
curl -s 127.0.0.1:8787/metrics.json | jq '.alerts'
ssh -L 8787:127.0.0.1:8787 <server>     # dashboard at http://localhost:8787 from your machine
```

Expected steady state: `mint_frozen=false`, price age < 250 slots, SB age < 300s, no critical
alerts. The webhook posts on any critical and whenever the monitor itself is unreachable/blind.

## Ops notes

- **The one hard rule:** the crank must not be down > ~1h while the market holds a fresh price —
  `SHUTDOWN_ORACLE_STALENESS_SLOTS` (~1h) makes shutdown permissionless and irreversible past
  that. `Restart=always` + `StartLimitIntervalSec=0` + the webhook liveness page exist for this.
- **Pyth core migration (~2026-07-31):** the on-chain dual-accept alt receiver is pre-seeded;
  after the cutover, confirm the sponsored push feed account (`7UVi…`, shard 0) still updates —
  the crank pins it at startup and will fail loudly if it vanishes.
- **Updating:** `git pull && yarn install`, then `sudo systemctl restart fusd-oracle-crank
  fusd-liquidator fusd-monitor`. Unit file changes need `sudo keepers/deploy/install.sh` again.
- Wallet top-ups: the crank logs every send; ~1 SOL lasts roughly a month at default cadences.
  The `refresh_market` keeper reward (once set by gov) partially self-funds it in fUSD.
