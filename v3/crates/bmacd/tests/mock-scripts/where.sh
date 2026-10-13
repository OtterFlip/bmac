#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Mock workflow that reports the host and work directory it was given and
# leaves a file in the work directory.
set -euo pipefail
source "$(cd -- "$(dirname -- "$0")/.." && pwd -P)/scripts/lib/ui_protocol.sh"
bmac_ui_bootstrap "$@"

echo "host=${BMAC_HOST} work=${BMAC_WORK_DIR} mode=$(stat -c %a "$BMAC_WORK_DIR")"
echo artifact >"${BMAC_WORK_DIR}/artifact"
