#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Mock workflow that starts a long-lived process holding its output, as a
# VM's QEMU does when started from the script, and then finishes.
set -euo pipefail
source "$(cd -- "$(dirname -- "$0")/.." && pwd -P)/scripts/lib/ui_protocol.sh"
bmac_ui_bootstrap "$@"

sleep 60 &
echo "started a daemon"
