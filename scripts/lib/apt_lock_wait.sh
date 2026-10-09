#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Remote-side helpers that wait for the apt and dpkg locks instead of failing
# when automatic package updates (apt-daily, unattended-upgrades) hold them.
# Workstation scripts send these functions to the host that runs apt-get with
# APT_LOCK_WAIT_FUNCTIONS, ahead of the payload that calls apt_wait_for_locks.

# Print the PID holding any apt or dpkg lock, or nothing when all are free.
# apt and dpkg take POSIX record locks, so /proc/locks identifies the holder
# without depending on fuser or lslocks being installed. It names each locked
# file MAJOR:MINOR:INODE with the device numbers in hex; matching the device
# too keeps a container's apt lock, also listed on a Proxmox host, from
# matching a host lock file with the same inode number.
apt_lock_holder() {
  local lock dev inode key pid
  for lock in /var/lib/dpkg/lock-frontend /var/lib/dpkg/lock \
    /var/lib/apt/lists/lock /var/cache/apt/archives/lock; do
    [[ -e "$lock" ]] || continue
    read -r dev inode < <(stat -c '%d %i' "$lock")
    printf -v key '%02x:%02x:%s' \
      "$(( ((dev >> 8) & 0xfff) | ((dev >> 32) & ~0xfff) ))" \
      "$(( (dev & 0xff) | ((dev >> 12) & ~0xff) ))" "$inode"
    pid="$(awk -v key="$key" '$2 != "->" && $6 == key { print $5; exit }' /proc/locks)"
    if [[ -n "$pid" ]]; then
      printf '%s\n' "$pid"
      return 0
    fi
  done
}

# Wait up to APT_LOCK_WAIT_SECONDS (default 600) for every apt and dpkg lock to
# be free. A progress line is printed every poll so that, after Ctrl+C closes
# the SSH session, the next write ends the remote payload.
apt_wait_for_locks() {
  local limit="${APT_LOCK_WAIT_SECONDS:-600}" poll=10 waited=0 holder command
  holder="$(apt_lock_holder)"
  [[ -n "$holder" ]] || return 0
  command="$(ps -o args= -p "$holder" 2>/dev/null || true)"
  echo
  echo "Another package-management process on $(hostname) holds the apt/dpkg lock:"
  echo "  PID $holder: ${command:-unknown command}"
  echo "This is usually automatic package updates, which finish within a few minutes."
  echo "Waiting up to $((limit / 60)) minutes for the lock to be released."
  echo "To stop waiting instead, press Ctrl+C and run this script again later."
  echo "It is safe to repeat."
  while holder="$(apt_lock_holder)"; [[ -n "$holder" ]]; do
    if ((waited >= limit)); then
      echo "ERROR: The apt/dpkg lock is still held by PID $holder after ${waited}s." >&2
      echo "Let that process finish, then run this script again." >&2
      return 1
    fi
    sleep "$poll"
    waited=$((waited + poll))
    echo "  Still waiting for the apt/dpkg lock (${waited}s of ${limit}s)..."
  done
  echo "The apt/dpkg lock was released after ${waited}s. Continuing."
  echo
}

# shellcheck disable=SC2034 # Used by the scripts that source this library.
APT_LOCK_WAIT_FUNCTIONS="$(declare -f apt_lock_holder apt_wait_for_locks)"
