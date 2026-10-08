#!/usr/bin/env bash
# Mock workflow that runs until interrupted, with a phase that must not be
# interrupted, and cleans up on SIGINT.
set -euo pipefail
source "$(cd -- "$(dirname -- "$0")/.." && pwd -P)/lib/ui_protocol.sh"
bmac_ui_bootstrap "$@"

cleanup() {
  echo "cleaning up after interrupt"
  exit 130
}
trap cleanup INT

bmac_ui_phase finalize "Finalizing receive" running --no-cancel
sleep "${MOCK_NO_CANCEL_SECONDS:-0.5}"
bmac_ui_phase finalize "Finalizing receive" complete
bmac_ui_step "Waiting"
for ((i = 1; i <= 600; i += 1)); do
  bmac_ui_progress "tick ${i}" "$i" 600
  sleep 0.1
done
