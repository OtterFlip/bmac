#!/usr/bin/env bash
# Mock workflow that asks for a secret and then (wrongly) prints it, to prove
# the dashboard redacts it from everything it records or shows.
set -euo pipefail
source "$(cd -- "$(dirname -- "$0")/.." && pwd -P)/scripts/lib/ui_protocol.sh"
bmac_ui_bootstrap "$@"

bmac_ui_input key --id key --label "Auth key" --type password --required
echo "the key is ${key}"
echo "ERROR: rejected ${key}" >&2
exit 1
