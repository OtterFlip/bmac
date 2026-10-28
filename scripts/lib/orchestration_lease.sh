#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Taking over a production VM's orchestration lease that a failed run left
# behind. A run releases its lease when it exits, but one that crashed, or
# whose terminal was closed, keeps it until it expires hours or a day later.
# Callers define warn, info, and prompt_yes, and source ui_protocol.sh.

# lease_owner [LABEL]: this run's lease owner, [LABEL@]HOST-PID, from the
# workstation's short host name and this process's PID, so a later run on
# the same workstation can tell whether the lease's run is still going.
# shellcheck disable=SC2120 # Callers may pass a label.
lease_owner() {
  local host
  host="$(hostname -s 2>/dev/null || hostname)"
  host="$(LC_ALL=C tr -c 'A-Za-z0-9_.\n-' '-' <<<"$host")"
  printf '%s%s-%s\n' "${1:+$1@}" "${host:-workstation}" "$$"
}

# lease_holder_from_error FILE: "OWNER EXPIRES" from the registry's refusal
# in FILE, or nothing when another error refused the lease.
lease_holder_from_error() {
  sed -n 's/.*orchestration lease is held by \([^ ]*\) until \([^ ]*\)$/\1 \2/p' "$1" | tail -n 1
}

# offer_lease_takeover NAME OWNER EXPIRES: ask to take over NAME's lease.
# A yes is enough when the lease's run was on this workstation and its
# process is gone. Otherwise NAME must be typed, since two runs at once could
# damage the VM.
offer_lease_takeover() {
  local name="$1" owner="$2" expires="$3" pid host here verdict severity=warning phrase="" answer
  pid="${owner##*-}"
  host="${owner%-*}"
  host="${host#*@}"
  here="$(lease_owner)"
  here="${here%-*}"
  warn "The orchestration lease of $name is held by $owner until $expires"
  if [[ "$host" == "$here" && "$pid" =~ ^[0-9]+$ ]] && ! kill -0 "$pid" 2>/dev/null; then
    verdict="It belongs to a run on this workstation (process $pid) that is no longer running, so a failed run left it behind."
  else
    verdict="This workstation cannot tell whether that run ($owner) is still going. Take the lease over only if no other run for $name is going on any workstation: two at once could damage the VM."
    severity=destructive
    phrase="$name"
  fi
  info "$verdict"
  if bmac_ui_is_json; then
    bmac_ui_confirm --id lease-takeover --title "Take over the lease of $name?" \
      --message "$verdict" --severity "$severity" ${phrase:+--text "$phrase"} \
      --detail "Held by $owner until $expires" --confirm-label "Take over the lease"
    return
  fi
  if [[ -n "$phrase" ]]; then
    IFS= read -r -p "Type $phrase to take over the lease: " answer
    [[ "$answer" == "$phrase" ]]
  else
    prompt_yes "Take over the lease?"
  fi
}
