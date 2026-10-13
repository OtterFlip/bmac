#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Guest access through the QEMU guest agent. Scripts never log in to a guest
# over SSH: every command a guest runs goes through `qm guest exec` on the
# cluster host that runs the guest. Source after scripts/lib/config.sh.

# guest_agent_run [--stdin] NODE VMID TIMEOUT ARGV...: run ARGV in guest VMID
# on NODE and wait up to TIMEOUT seconds. With --stdin, this function's stdin
# (at most 1 MiB) becomes the command's stdin; otherwise the command gets an
# empty one. Prints the command's stdout and stderr and returns its exit
# status, 124 when it did not finish in time or its output was truncated, and
# 125 when the agent could not run it.
guest_agent_run() {
  local pass_stdin=0 node vmid timeout result
  if [[ "${1:-}" == --stdin ]]; then
    pass_stdin=1
    shift
  fi
  node="${1:-}" vmid="${2:-}" timeout="${3:-}"
  shift 3 || return 2
  [[ "$node" =~ ^mox([1-9]|[12][0-9]|3[0-2])$ && "$vmid" =~ ^[1-9][0-9]*$ &&
    "$timeout" =~ ^[1-9][0-9]*$ ]] && (($# > 0)) || return 2
  if ((pass_stdin)); then
    result="$(mox_ssh "$node" qm guest exec "$vmid" --timeout "$timeout" \
      --pass-stdin 1 -- "$@")" || return 125
  else
    result="$(mox_ssh "$node" qm guest exec "$vmid" --timeout "$timeout" \
      -- "$@" </dev/null)" || return 125
  fi
  python3 -c '
import json
import sys

try:
    value = json.loads(sys.argv[1])
except ValueError:
    sys.exit(125)
if not isinstance(value, dict):
    sys.exit(125)
sys.stdout.write(value.get("out-data", ""))
sys.stderr.write(value.get("err-data", ""))
if value.get("exited") != 1 or value.get("out-truncated") or value.get("err-truncated"):
    sys.exit(124)
code = value.get("exitcode")
sys.exit(code if isinstance(code, int) and 0 <= code <= 255 else 125)
' "$result"
}

# guest_agent_ping NODE VMID: succeed when the guest agent answers.
guest_agent_ping() {
  local node="$1" vmid="$2"
  [[ "$vmid" =~ ^[1-9][0-9]*$ ]] || return 2
  mox_ssh "$node" qm guest cmd "$vmid" ping </dev/null >/dev/null 2>&1
}
