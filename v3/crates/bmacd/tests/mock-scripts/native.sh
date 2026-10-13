#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Stands in for `bmacd native OPERATION --json ARGS`: reports how it was
# called, then speaks the protocol like a script does.
set -euo pipefail
if [ -z "${MOCK_NATIVE_CALL:-}" ]; then
  [ "${1:-}" = native ] && [ "${3:-}" = --json ] || exit 9
  export MOCK_NATIVE_CALL="operation=$2 args=${*:4}"
  set -- "${@:3}"
fi
source "$(cd -- "$(dirname -- "$0")/.." && pwd -P)/scripts/lib/ui_protocol.sh"
bmac_ui_bootstrap "$@"

echo "${MOCK_NATIVE_CALL} work=${BMAC_WORK_DIR}"
