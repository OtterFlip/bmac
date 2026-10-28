#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Instant Restore to New Permanent VM: full_recovery.sh --instant. The VM
# boots from its restore point mapped read-only from PBS within minutes, its
# disk is copied while it runs and then switched to its own zvol, and only
# then does it get replication and HA. See full_recovery.sh --help.

set -Eeuo pipefail

# shellcheck source=full_recovery.sh
FULL_RECOVERY_SOURCE_ONLY=1 source "$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)/full_recovery.sh"

eval "full_recovery_parse_args() $(declare -f parse_args | tail -n +2)"
# full_recovery.sh's options, with --instant implied.
parse_args() {
  local -a forwarded=(--instant)
  while (($#)); do
    case "$1" in
      --snapshot | --cores | --memory-gib)
        (($# >= 2)) || die "$1 needs a value"
        forwarded+=("$1" "$2")
        shift 2
        ;;
      *)
        forwarded+=("$1")
        shift
        ;;
    esac
  done
  full_recovery_parse_args "${forwarded[@]}"
}

bmac_ui_bootstrap "$@"
main "$@"
