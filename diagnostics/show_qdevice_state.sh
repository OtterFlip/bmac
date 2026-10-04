#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Read-only report on the cluster's external QDevice: whether the cluster
# needs one, whether one is registered, whether it is alive and voting on
# every member, and what to do when it is missing or has failed.

set -u
set -o pipefail
umask 077

if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
  printf 'ERROR: show_qdevice_state.sh requires Bash 4.4 or newer (found %s).\n' \
    "${BASH_VERSION:-unknown}" >&2
  exit 2
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
CONTROL_LIB="${REPO_ROOT}/lib/cluster_control.sh"
MEMBERSHIP_LIB="${REPO_ROOT}/lib/host_membership.sh"

for library in "$CONFIG_LIB" "$CONTROL_LIB" "$MEMBERSHIP_LIB"; do
  [[ -f "$library" && ! -L "$library" ]] || {
    printf 'ERROR: required library is unavailable: %s\n' "$library" >&2
    exit 2
  }
done
# shellcheck source=../lib/config.sh
source "$CONFIG_LIB"
# shellcheck source=../lib/cluster_control.sh
source "$CONTROL_LIB"
# shellcheck source=../lib/host_membership.sh
source "$MEMBERSHIP_LIB"

MEMBER_COUNT=0
REGISTERED=0
REGISTERED_ADDRESS=""
MEMBERS_HEALTHY=0
QDEVICE_REACHABLE=0
QDEVICE_TAILSCALE_IP=""
QNETD_ACTIVE=0
QNETD_SERVES_CLUSTER=0
declare -a MEMBERS=()
declare -a OFFLINE_MEMBERS=()
declare -a PROBLEMS=()

usage() {
  cat <<'EOF'
Usage: diagnostics/show_qdevice_state.sh

Report whether the cluster needs an external QDevice (it does when it has an
even number of members), whether one is registered in corosync.conf, and
whether every member sees it alive and voting. When it is functional, print
its details from the cluster and from the QDevice host
(PROXMOX_QDEVICE_HOST, reached as root over SSH). When it is missing or has
failed, explain what to do, including replacing it with
qdevice/purge_qdevice.sh and qdevice/add_qdevice.sh.

Nothing is changed. Commands create normal SSH and command-access log entries.

Exit status: 0 when the QDevice layout is correct, 1 when it needs attention,
and 2 for usage or configuration errors.
EOF
}

hr() {
  printf '%*s\n' 96 '' | tr ' ' '='
}

section() {
  printf '\n'
  hr
  printf '%s\n' "$1"
  printf 'Why: %s\n' "$2"
  hr
}

indent() {
  sed 's/^/    /'
}

qdevice_ssh() {
  ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
    -o ConnectTimeout=10 "root@${PROXMOX_QDEVICE_HOST}" "$@" </dev/null
}

# Print the verdict for the collected state. Returns 0 when the layout is
# correct and 1 when it needs attention.
qdevice_verdict() {
  local problem inaccessible=0
  if ((REGISTERED)) &&
    { ((QDEVICE_REACHABLE == 0)) ||
      [[ -n "$QDEVICE_TAILSCALE_IP" && "$QDEVICE_TAILSCALE_IP" != "$REGISTERED_ADDRESS" ]]; }; then
    inaccessible=1
  fi
  section "VERDICT" "what the QDevice state means and what to do next."

  if ((MEMBER_COUNT % 2 == 1)); then
    if ((REGISTERED == 0)); then
      printf 'OK: the %s-member cluster has an odd number of votes and correctly has no QDevice.\n' \
        "$MEMBER_COUNT"
      return 0
    fi
    printf 'ATTENTION: a QDevice (%s) is registered, but the %s-member cluster has an odd\n' \
      "${REGISTERED_ADDRESS:-unknown address}" "$MEMBER_COUNT"
    printf 'number of votes and must not use one. hosts/setup_proxmox_host.sh removes it on its\n'
    printf 'next run, or run "pvecm qdevice remove" on a member while every member is online.\n'
    return 1
  fi

  if ((REGISTERED == 0)); then
    printf 'ATTENTION: the %s-member cluster needs a QDevice and has none registered.\n' \
      "$MEMBER_COUNT"
    printf 'With an even number of members and no tie-breaking vote, losing any one member\n'
    printf 'loses quorum. To add one:\n'
    printf '  1. Prepare the machine with qdevice/QDEVICE_MANUAL_SETUP.md.\n'
    printf '  2. Run qdevice/add_qdevice.sh.\n'
    if ((QDEVICE_REACHABLE)); then
      printf '%s is reachable from this workstation and can be the one you add.\n' \
        "$PROXMOX_QDEVICE_HOST"
    fi
    ((${#OFFLINE_MEMBERS[@]} == 0)) ||
      printf 'Offline members (%s) must be online first.\n' "${OFFLINE_MEMBERS[*]}"
    return 1
  fi

  if ((MEMBERS_HEALTHY && ${#OFFLINE_MEMBERS[@]} == 0 && inaccessible == 0)); then
    printf 'OK: the QDevice at %s is alive and voting on all %s members (%s expected votes).\n' \
      "$REGISTERED_ADDRESS" "$MEMBER_COUNT" "$((MEMBER_COUNT + 1))"
    return 0
  fi
  if ((MEMBERS_HEALTHY && ${#OFFLINE_MEMBERS[@]} == 0)); then
    printf 'ATTENTION: the QDevice at %s is alive and voting, but %s is not that machine\n' \
      "$REGISTERED_ADDRESS" "$PROXMOX_QDEVICE_HOST"
    printf '(reachable: %s, Tailscale address: %s). The scripts reach the QDevice through\n' \
      "$([[ "$QDEVICE_REACHABLE" == 1 ]] && printf yes || printf no)" \
      "${QDEVICE_TAILSCALE_IP:-unknown}"
    printf 'PROXMOX_QDEVICE_HOST in env/cluster.conf; correct it or your SSH configuration.\n'
    return 1
  fi

  if ((inaccessible)); then
    printf 'ATTENTION: the cluster needs its QDevice and has one registered at %s, but\n' \
      "$REGISTERED_ADDRESS"
    if ((QDEVICE_REACHABLE == 0)); then
      printf 'it is not accessible: ssh root@%s fails from this workstation.\n' "$PROXMOX_QDEVICE_HOST"
    else
      printf '%s is reachable but is a different machine (Tailscale address %s).\n' \
        "$PROXMOX_QDEVICE_HOST" "$QDEVICE_TAILSCALE_IP"
    fi
    printf '\nIf it is only temporarily unreachable (network or Tailscale), restore access\n'
    printf 'and rerun this script. If it has failed and must be replaced:\n'
    printf '  1. Run qdevice/purge_qdevice.sh. It cannot purge an unreachable machine, but it\n'
    printf '     offers to unregister the QDevice from the cluster and remove every\n'
    printf '     association with it from the Proxmox hosts. You must first remove the old\n'
    printf '     machine from Tailscale so it can never communicate with the cluster again.\n'
    printf '  2. Prepare the new machine with qdevice/QDEVICE_MANUAL_SETUP.md.\n'
    printf '  3. Run qdevice/add_qdevice.sh to add it to the cluster.\n'
  else
    printf 'ATTENTION: the QDevice at %s is registered and %s is reachable, but not\n' \
      "$REGISTERED_ADDRESS" "$PROXMOX_QDEVICE_HOST"
    printf 'every member sees it alive and voting.\n'
  fi
  if ((${#PROBLEMS[@]} > 0)); then
    printf '\nProblems found:\n'
    for problem in "${PROBLEMS[@]}"; do
      printf '  - %s\n' "$problem"
    done
  fi
  if ((inaccessible == 0)); then
    printf '\nRepair the problems above (for example restart corosync-qnetd on the QDevice or\n'
    printf 'corosync-qdevice on a member). If the QDevice cannot be repaired, replace it:\n'
    printf 'qdevice/purge_qdevice.sh, then qdevice/QDEVICE_MANUAL_SETUP.md, then\n'
    printf 'qdevice/add_qdevice.sh.\n'
  fi
  ((${#OFFLINE_MEMBERS[@]} == 0)) ||
    printf '\nEvery member must be online before the QDevice can be removed or added.\n'
  return 1
}

collect_cluster() {
  local status=0 probe
  section "CLUSTER MEMBERSHIP" "the member count decides whether a QDevice is needed."
  control_find_cluster || status=$?
  if ((status != 0)); then
    printf 'ERROR: no reachable cluster member was found over strict SSH.\n' >&2
    exit 1
  fi
  probe="$CONTROL_PROBE_NODE"
  # shellcheck disable=SC2034 # Read by the lib/host_membership.sh helpers.
  HM_COORDINATOR="$probe"
  MEMBERS=("${CONTROL_MEMBER_NODES[@]}")
  MEMBER_COUNT="${#MEMBERS[@]}"
  local node
  for node in "${MEMBERS[@]}"; do
    control_list_contains "$node" "${CONTROL_ONLINE_NODES[@]}" || OFFLINE_MEMBERS+=("$node")
  done
  printf 'Read through:   %s\n' "$probe"
  printf 'Members:        %s (%s)\n' "${MEMBERS[*]}" "$MEMBER_COUNT"
  printf 'Online:         %s\n' "${CONTROL_ONLINE_NODES[*]:-none}"
  printf 'Offline:        %s\n' "${OFFLINE_MEMBERS[*]:-none}"
  if ((MEMBER_COUNT % 2 == 0)); then
    printf 'QDevice needed: yes (even member count; expected votes with QDevice: %s)\n' \
      "$((MEMBER_COUNT + 1))"
  else
    printf 'QDevice needed: no (odd member count)\n'
  fi
  for node in "${OFFLINE_MEMBERS[@]}"; do
    PROBLEMS+=("member $node is offline")
  done

  printf '\n[pvecm status on %s]\n' "$probe"
  mox_ssh "$probe" pvecm status </dev/null 2>&1 | indent

  printf '\n[quorum section of /etc/pve/corosync.conf]\n'
  # shellcheck disable=SC2016 # awk program.
  mox_ssh "$probe" awk '
    /^quorum[[:space:]]*[{]/ { inside=1 }
    inside { print; depth += gsub(/[{]/, "{"); depth -= gsub(/[}]/, "}") }
    inside && depth == 0 { exit }
  ' /etc/pve/corosync.conf </dev/null 2>&1 | indent
  local registered_status=0
  hm_qdevice_configured 2>/dev/null || registered_status=$?
  case "$registered_status" in
    0)
      REGISTERED=1
      REGISTERED_ADDRESS="$(hm_registered_qdevice_address 2>/dev/null)"
      printf '\nRegistered QDevice address: %s\n' "${REGISTERED_ADDRESS:-unknown}"
      ;;
    1)
      printf '\nNo QDevice is registered.\n'
      ;;
    *)
      printf 'ERROR: could not read /etc/pve/corosync.conf on %s.\n' "$probe" >&2
      exit 1
      ;;
  esac
}

collect_members() {
  local node status service healthy=1
  section "MEMBER VIEW OF THE QDEVICE" "every member must see the QDevice vote for quorum to survive a member loss."
  for node in "${CONTROL_ONLINE_NODES[@]}"; do
    printf '\n[%s]\n' "$node"
    status="$(mox_ssh "$node" pvecm status </dev/null 2>/dev/null)" || status=""
    if [[ -z "$status" ]]; then
      printf '    could not read pvecm status\n'
      PROBLEMS+=("could not read pvecm status on $node")
      healthy=0
      continue
    fi
    printf '    Expected votes: %s  Total votes: %s  Quorate: %s\n' \
      "$(hm_status_value "$status" 'Expected votes')" \
      "$(hm_status_value "$status" 'Total votes')" \
      "$(hm_status_value "$status" 'Quorate')"
    service="$(mox_ssh "$node" systemctl is-active corosync-qdevice </dev/null 2>/dev/null)"
    printf '    corosync-qdevice: %s\n' "${service:-unknown}"
    if ((REGISTERED)); then
      mox_ssh "$node" corosync-qdevice-tool -sv </dev/null 2>&1 | indent | indent
      if ((MEMBER_COUNT % 2 == 0)) &&
        ! hm_qdevice_status_is_healthy "$status" "$MEMBER_COUNT"; then
        PROBLEMS+=("$node does not see the QDevice alive and voting with $((MEMBER_COUNT + 1)) expected/total votes")
        healthy=0
      fi
      [[ "$service" == active ]] || {
        PROBLEMS+=("corosync-qdevice is ${service:-unknown} on $node")
        healthy=0
      }
    fi
  done
  MEMBERS_HEALTHY="$healthy"
}

collect_qdevice_host() {
  local report
  section "QDEVICE HOST ${PROXMOX_QDEVICE_HOST}" "the QDevice must be reachable to inspect, repair, or purge it."
  if ! qdevice_ssh true >/dev/null 2>&1; then
    printf 'ssh root@%s: FAILED (BatchMode, strict host key checking)\n' "$PROXMOX_QDEVICE_HOST"
    ((REGISTERED == 0)) || PROBLEMS+=("ssh root@${PROXMOX_QDEVICE_HOST} fails from this workstation")
    return 0
  fi
  QDEVICE_REACHABLE=1
  printf 'ssh root@%s: OK\n' "$PROXMOX_QDEVICE_HOST"
  # shellcheck disable=SC2016 # Evaluated on the QDevice host.
  report="$(qdevice_ssh bash -s 2>&1 <<'REMOTE'
printf 'HOSTNAME %s\n' "$(hostname)"
printf 'OS %s\n' "$(. /etc/os-release 2>/dev/null && printf '%s' "${PRETTY_NAME:-unknown}")"
printf 'UPTIME %s\n' "$(uptime -p 2>/dev/null || uptime)"
printf 'TAILSCALE %s\n' "$(tailscale ip -4 2>/dev/null | head -n 1)"
printf 'QNETD_VERSION %s\n' "$(dpkg-query -W -f='${Version}' corosync-qnetd 2>/dev/null || printf 'not installed')"
printf 'QNETD_ACTIVE %s\n' "$(systemctl is-active corosync-qnetd 2>/dev/null || true)"
printf 'QNETD_ENABLED %s\n' "$(systemctl is-enabled corosync-qnetd 2>/dev/null || true)"
if ss -lnt 2>/dev/null | awk '$4 ~ /:5403$/ { found=1 } END { exit !found }'; then
  printf 'LISTEN yes\n'
else
  printf 'LISTEN no\n'
fi
if command -v corosync-qnetd-tool >/dev/null 2>&1; then
  printf 'QNETD_TOOL_BEGIN\n'
  corosync-qnetd-tool -l -v 2>&1 || true
  printf 'QNETD_TOOL_END\n'
fi
REMOTE
  )" || true
  QDEVICE_TAILSCALE_IP="$(awk '$1 == "TAILSCALE" { print $2 }' <<<"$report")"
  local active listen
  active="$(awk '$1 == "QNETD_ACTIVE" { print $2 }' <<<"$report")"
  listen="$(awk '$1 == "LISTEN" { print $2 }' <<<"$report")"
  [[ "$active" != active ]] || QNETD_ACTIVE=1
  awk '
    $1 == "QNETD_TOOL_BEGIN" || $1 == "QNETD_TOOL_END" { next }
    /^(HOSTNAME|OS|UPTIME|TAILSCALE|QNETD_VERSION|QNETD_ACTIVE|QNETD_ENABLED|LISTEN) / {
      key = $1; $1 = ""; sub(/^ /, "")
      printf "%-16s %s\n", key ":", $0
    }
  ' <<<"$report"
  printf '\n[corosync-qnetd-tool -l -v]\n'
  sed -n '/^QNETD_TOOL_BEGIN$/,/^QNETD_TOOL_END$/p' <<<"$report" |
    sed '1d;$d' | indent
  if grep -Fq "Cluster \"${PROXMOX_CLUSTER_NAME}\":" <<<"$report"; then
    QNETD_SERVES_CLUSTER=1
  fi
  if ((REGISTERED)); then
    if [[ -n "$QDEVICE_TAILSCALE_IP" && "$QDEVICE_TAILSCALE_IP" != "$REGISTERED_ADDRESS" ]]; then
      printf '\n%s has Tailscale address %s; the cluster registered %s.\n' \
        "$PROXMOX_QDEVICE_HOST" "$QDEVICE_TAILSCALE_IP" "$REGISTERED_ADDRESS"
      PROBLEMS+=("${PROXMOX_QDEVICE_HOST} (${QDEVICE_TAILSCALE_IP}) is not the registered QDevice (${REGISTERED_ADDRESS})")
      return 0
    fi
    ((QNETD_ACTIVE)) || PROBLEMS+=("corosync-qnetd is ${active:-unknown} on ${PROXMOX_QDEVICE_HOST}")
    [[ "$listen" == yes ]] || PROBLEMS+=("corosync-qnetd is not listening on TCP 5403")
    ((QNETD_SERVES_CLUSTER)) ||
      PROBLEMS+=("corosync-qnetd does not list cluster ${PROXMOX_CLUSTER_NAME} as connected")
  fi
}

main() {
  while (($#)); do
    case "$1" in
      -h | --help)
        usage
        exit 0
        ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
  done
  command -v python3 >/dev/null 2>&1 || {
    printf 'ERROR: python3 is required on this workstation\n' >&2
    exit 2
  }
  load_proxmox_config --no-secrets >/dev/null || {
    printf 'ERROR: cluster configuration is invalid\n' >&2
    exit 2
  }
  printf 'show_qdevice_state.sh - read-only QDevice report for cluster %s\n' \
    "$PROXMOX_CLUSTER_NAME"
  printf 'Started: %s\n' "$(date +%Y-%m-%dT%H:%M:%S%z)"
  collect_cluster
  collect_members
  collect_qdevice_host
  qdevice_verdict
}

if [[ "${SHOW_QDEVICE_STATE_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
