#!/bin/bash
# hl-visor watchdog — restarts hl-visor if it crashes (e.g. after a hardfork upgrade)
#
# Hyperliquid pushes hardfork upgrades to testnet without warning. When this happens,
# hl-node crashes with an assertion like:
#   "Hardfork { version: 1240 } != Hardfork { version: 1239 }"
#
# hl-visor is supposed to auto-update, but a hard crash requires a manual restart with
# the new binary. This watchdog automates that recovery.
#
# Install as a cron job (checks every 5 minutes):
#   crontab -e
#   */5 * * * * /path/to/hl-visor-watchdog.sh >> ~/hl-visor-watchdog.log 2>&1
#
# Configuration: set these to match your setup
VISOR_BIN="${HL_VISOR_BIN:-$HOME/hl-visor}"
VISOR_LOG="${HL_VISOR_LOG:-$HOME/hl-visor.log}"
CHAIN="${HL_CHAIN:-Testnet}"

# Binary download URL (Testnet or Mainnet)
if [ "$CHAIN" = "Mainnet" ]; then
    VISOR_URL="https://binaries.hyperliquid.xyz/Mainnet/hl-visor"
else
    VISOR_URL="https://binaries.hyperliquid-testnet.xyz/Testnet/hl-visor"
fi

set -euo pipefail

# If hl-visor is already running, nothing to do
if pgrep -f "hl-visor run-non-validator" > /dev/null 2>&1; then
    exit 0
fi

echo "$(date -u '+%Y-%m-%dT%H:%M:%SZ') hl-visor not running, restarting..."

# Check if the last crash was a hardfork mismatch — if so, download fresh binary
if grep -q "Hardfork.*assertion.*failed" "$VISOR_LOG" 2>/dev/null; then
    echo "$(date -u '+%Y-%m-%dT%H:%M:%SZ') Detected hardfork crash, downloading latest binary..."
    curl -sL "$VISOR_URL" > "${VISOR_BIN}.new"
    mv "${VISOR_BIN}.new" "$VISOR_BIN"
    chmod a+x "$VISOR_BIN"
    echo "$(date -u '+%Y-%m-%dT%H:%M:%SZ') Binary updated."
fi

nohup "$VISOR_BIN" run-non-validator --serve-evm-rpc --disable-output-file-buffering \
    >> "$VISOR_LOG" 2>&1 &

echo "$(date -u '+%Y-%m-%dT%H:%M:%SZ') Restarted hl-visor with PID $!"
