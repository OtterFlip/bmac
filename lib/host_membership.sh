#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Workstation helpers shared by hosts/remove_proxmox_host.sh,
# qdevice/add_qdevice.sh, qdevice/remove_qdevice.sh, and the diagnostics:
# logging and prompts, command execution on cluster members through one
# coordinator, the cluster control-plane lock that host setup also takes,
# Proxmox node deletion, and host-slot bookkeeping. It connects lib/qdevice.sh, which holds the QDevice
# logic, to the cluster through the coordinator. Source after lib/config.sh,
# load_proxmox_config, and lib/cluster_control.sh. Callers set HM_COORDINATOR
# to an online member that survives the change.

# shellcheck source=./ui_protocol.sh
declare -F bmac_ui_is_json >/dev/null ||
  source "$(dirname -- "${BASH_SOURCE[0]}")/ui_protocol.sh"

# shellcheck source=qdevice.sh
source "$(dirname -- "${BASH_SOURCE[0]}")/qdevice.sh"

HM_REMOTE_ROOT="/usr/local/lib/app-ha-proxmox"
HM_REMOTE_REGISTRY="${HM_REMOTE_ROOT}/lib/cluster_registry.py"
# shellcheck disable=SC2034 # Used by the scripts that source this library.
HM_REMOTE_HAPROXY_SYNC="${HM_REMOTE_ROOT}/lib/sync_haproxy_routes.sh"

HM_COORDINATOR=""
HM_LOCK_FILE_FD=""
HM_LOCK_TOKEN=""
HM_LOCK_PID=""
HM_LOCK_READ_FD=""
HM_LOCK_WRITE_FD=""

log() {
  printf '\n==> %s\n' "$*"
  bmac_ui_step "$*"
}

info() {
  printf '    %s\n' "$*"
}

warn() {
  printf 'WARNING: %s\n' "$*" >&2
}

die() {
  printf '\nERROR: %s\n' "$*" >&2
  bmac_ui_error failed "$*"
  exit 1
}

prompt_with_default() {
  local destination="$1" prompt="$2" default="$3" entered
  if bmac_ui_is_json; then
    bmac_ui_text entered "$prompt" "$default"
  else
    IFS= read -r -p "${prompt} [${default}]: " entered ||
      die "Input ended before a value was entered"
  fi
  printf -v "$destination" '%s' "${entered:-$default}"
}

hm_prompt_yes() {
  local answer
  if bmac_ui_is_json; then
    bmac_ui_ask "$1"
    return
  fi
  IFS= read -r -p "$1 [y/N] " answer || return 1
  [[ "${answer,,}" == y || "${answer,,}" == yes ]]
}

# Require the operator to type one exact phrase.
hm_confirm_phrase() {
  local message="$1" phrase="$2" entered
  if bmac_ui_is_json; then
    local severity=destructive
    [[ "$phrase" == GO ]] && severity=warning
    bmac_ui_confirm --id confirm_phrase --title "Confirm this change" \
      --message "$message" --severity "$severity" --text "$phrase" &&
      return 0
    die "The change was not confirmed; no change was made"
  fi
  printf '\n%s\nType exactly: %s\n> ' "$message" "$phrase"
  IFS= read -r entered || entered=""
  [[ "$entered" == "$phrase" ]] ||
    die "Confirmation did not match; no change was made"
}

hm_valid_node() {
  [[ "${1:-}" =~ ^mox([1-9]|10)$ ]] && ((${1#mox} <= MAX_MOX_HOSTS))
}

# Execute one argv array on a cluster member. Members other than the
# coordinator are reached through Proxmox root cluster SSH trust.
hm_exec() {
  local node="$1"
  shift
  hm_valid_node "$node" || return 2
  (($# > 0)) || return 2
  local payload="set -Eeuo pipefail"$'\n'"exec" argument quoted
  for argument in "$@"; do
    printf -v quoted '%q' "$argument"
    payload+=" ${quoted}"
  done
  payload+=$'\n'
  if [[ "$node" == "$HM_COORDINATOR" ]]; then
    printf '%s' "$payload" | mox_ssh "$HM_COORDINATOR" bash -s
  else
    printf '%s' "$payload" |
      mox_ssh "$HM_COORDINATOR" ssh \
        -o BatchMode=yes \
        -o ClearAllForwardings=yes \
        -o "ConnectTimeout=${MOX_SSH_CONNECT_TIMEOUT:-8}" \
        -o StrictHostKeyChecking=yes \
        -o CheckHostIP=no \
        -o "HostKeyAlias=${node}" \
        -o "UserKnownHostsFile=/etc/pve/nodes/${node}/ssh_known_hosts" \
        -o GlobalKnownHostsFile=none \
        "root@${node}.${PROXMOX_INTERNAL_DOMAIN}" bash -s
  fi
}

hm_registry() {
  mox_ssh "$HM_COORDINATOR" "$HM_REMOTE_REGISTRY" \
    --state-dir "$CLUSTER_STATE_DIR" "$@" </dev/null
}

hm_pvesh_get() {
  mox_ssh "$HM_COORDINATOR" pvesh get "$@" --output-format json </dev/null
}

hm_json_field() {
  python3 -c '
import json, sys
value = json.loads(sys.argv[1])
for key in sys.argv[2:]:
    value = value[key]
print("" if value is None else value)
' "$@"
}

# The installed registry must support host slots and their release.
hm_require_slot_registry() {
  local help_text
  help_text="$(hm_registry --help)" ||
    die "Could not run the installed cluster registry on $HM_COORDINATOR"
  if ! grep -q -- 'host-references' <<<"$help_text" ||
    ! grep -q -- 'host-release' <<<"$help_text"; then
    die "The installed cluster registry predates host slots; run hosts/update_cluster_runtime.sh first"
  fi
}

# Print "NODE online|offline" for every configured cluster member.
hm_member_states() {
  local nodes_json
  nodes_json="$(hm_pvesh_get /nodes)" || return 1
  python3 - "$nodes_json" "$MAX_MOX_HOSTS" <<'PY'
import json
import re
import sys

rows = json.loads(sys.argv[1])
maximum = int(sys.argv[2])
if not isinstance(rows, list) or not rows:
    raise SystemExit("pvesh /nodes returned no cluster members")
members = []
seen = set()
for row in rows:
    node = row.get("node", row.get("name")) if isinstance(row, dict) else None
    match = re.fullmatch(r"mox([1-9]|10)", str(node))
    if not match or int(match.group(1)) > maximum:
        raise SystemExit(f"cluster contains unexpected node {node!r}")
    if node in seen:
        raise SystemExit(f"cluster returned duplicate node {node}")
    seen.add(node)
    online = (
        str(row.get("status", "")).lower() == "online"
        or row.get("online") in (1, True)
    )
    members.append((int(match.group(1)), node, "online" if online else "offline"))
for _, node, state in sorted(members):
    print(node, state)
PY
}

hm_cluster_status() {
  hm_exec "$HM_COORDINATOR" pvecm status
}

# lib/qdevice.sh reaches the cluster through HM_COORDINATOR.
qd_coordinator() {
  printf '%s\n' "$HM_COORDINATOR"
}

qd_exec() {
  hm_exec "$@"
}

qd_member_states() {
  hm_member_states
}

qd_exec_coordinator_tty() {
  local destination
  _mox_ssh_options "$HM_COORDINATOR" || return 1
  destination="$(_mox_ssh_destination "$HM_COORDINATOR")" || return 1
  # shellcheck disable=SC2029 # The command string is assembled by lib/qdevice.sh.
  ssh -tt "${MOX_SSH_OPTIONS[@]}" "${MOX_SSH_USER:-root}@${destination}" "$1"
}

# Check that this workstation reaches the QDevice as root and resolve its
# Tailscale IPv4 into QD_IPV4. Returns 1, with QD_IPV4 empty, when it does
# not; callers decide whether the change they make needs the QDevice.
hm_verify_qdevice_access() {
  log "QDevice access"
  if qd_probe_access; then
    info "Verified ssh root@${PROXMOX_QDEVICE_HOST} (Tailscale $QD_IPV4)"
    return 0
  fi
  warn "The QDevice is not accessible: ${QD_ACCESS_PROBLEM}"
  return 1
}

# Take the same control-plane lock host setup takes: a workstation flock plus
# a lease on LOCK_HOST held by a coprocess until hm_release_control_plane_lock.
hm_acquire_control_plane_lock() {
  local lock_host="$1" lock_file="${REPO_ROOT}/hosts/artifacts/cluster-control-plane.lock"
  local remote_lock_command status="" destination
  hm_valid_node "$lock_host" || die "Invalid control-plane lock host: $lock_host"
  install -d -m 0700 "${REPO_ROOT}/hosts/artifacts"
  exec {HM_LOCK_FILE_FD}>"$lock_file"
  chmod 0600 "$lock_file"
  info "Waiting for exclusive cluster membership changes..."
  flock -w 1800 "$HM_LOCK_FILE_FD" ||
    die "Timed out waiting 30 minutes for another host installer or membership change"
  HM_LOCK_TOKEN="$(
    printf '%s:%s:%s' "membership-$$" "$(date -u +%Y%m%dT%H%M%SZ)" \
      "$(openssl rand -hex 16)"
  )"
  # shellcheck disable=SC2016 # The lock program expands only on the lock host.
  printf -v remote_lock_command '%q ' bash -c '
    set -Eeuo pipefail
    token="$1"
    host="$2"
    lease=/run/lock/app-ha-cluster-control-plane.lease
    lock=/run/lock/app-ha-cluster-control-plane.lock
    if ! mkdir "$lease" 2>/dev/null; then
      owner="$(cat "$lease/owner" 2>/dev/null || printf unknown)"
      printf "Cluster control-plane lease already exists on %s (owner: %s). Verify that no installer is active; if it is stale, remove %s manually on %s.\n" \
        "$host" "$owner" "$lease" "$host" >&2
      exit 1
    fi
    printf "%s\n" "$token" >"$lease/owner"
    chmod 0600 "$lease/owner"
    exec 9>"$lock"
    flock -w 1800 9 || exit 1
    printf "LOCKED\n"
    release=""
    IFS= read -r release || exit 2
    [[ "$release" == "RELEASE:${token}" ]] || exit 3
    rm -rf -- "$lease"
  ' _ "$HM_LOCK_TOKEN" "$lock_host"
  _mox_ssh_options "$lock_host" || die "Could not prepare SSH to $lock_host"
  destination="$(_mox_ssh_destination "$lock_host")" ||
    die "Could not resolve $lock_host"
  coproc HM_LOCK_PROCESS {
    ssh "${MOX_SSH_OPTIONS[@]}" "${MOX_SSH_USER:-root}@${destination}" \
      "$remote_lock_command"
  }
  HM_LOCK_READ_FD="${HM_LOCK_PROCESS[0]}"
  HM_LOCK_WRITE_FD="${HM_LOCK_PROCESS[1]}"
  HM_LOCK_PID="$HM_LOCK_PROCESS_PID"
  IFS= read -r -u "$HM_LOCK_READ_FD" status ||
    die "Could not acquire the cluster control-plane lock hosted on ${lock_host}"
  [[ "$status" == LOCKED ]] ||
    die "Unexpected response while acquiring the ${lock_host} cluster control-plane lock"
  info "Holding the cluster control-plane lock on $lock_host"
}

hm_release_control_plane_lock() {
  if [[ -n "$HM_LOCK_PID" ]]; then
    if [[ -n "$HM_LOCK_WRITE_FD" ]]; then
      printf 'RELEASE:%s\n' "$HM_LOCK_TOKEN" 1>&"$HM_LOCK_WRITE_FD" 2>/dev/null || true
      exec {HM_LOCK_WRITE_FD}>&-
    fi
    if [[ -n "$HM_LOCK_READ_FD" ]]; then
      exec {HM_LOCK_READ_FD}<&-
    fi
    wait "$HM_LOCK_PID" 2>/dev/null || true
  fi
  HM_LOCK_PID=""
  HM_LOCK_READ_FD=""
  HM_LOCK_WRITE_FD=""
  HM_LOCK_TOKEN=""
  if [[ -n "$HM_LOCK_FILE_FD" ]]; then
    flock -u "$HM_LOCK_FILE_FD" 2>/dev/null || true
    exec {HM_LOCK_FILE_FD}>&-
    HM_LOCK_FILE_FD=""
  fi
}

# Wait until Proxmox reports NODE offline and the workstation cannot reach it.
hm_wait_until_offline() {
  local node="$1" timeout="$2" deadline state
  deadline=$((SECONDS + timeout))
  while ((SECONDS < deadline)); do
    state="$(
      hm_member_states 2>/dev/null | awk -v node="$node" '$1 == node { print $2 }'
    )"
    if [[ "$state" == offline ]] && ! mox_is_reachable "$node"; then
      return 0
    fi
    sleep 10
  done
  return 1
}

# Delete NODE from corosync and pmxcfs. NODE must already be powered off.
# hm_delete_cluster_node NODE [EXPECTED_VOTES]
# With EXPECTED_VOTES, the coordinator lowers expected votes to that value when
# the cluster is not quorate shortly after the delete. The check runs in the
# same remote command so an inquorate survivor recovers well inside the HA
# watchdog window.
hm_delete_cluster_node() {
  local node="$1" fallback_votes="${2:-}" deadline listed
  log "Deleting $node from the Proxmox cluster"
  if [[ -n "$fallback_votes" ]]; then
    [[ "$fallback_votes" =~ ^[1-9][0-9]*$ ]] ||
      die "Invalid fallback expected votes: $fallback_votes"
    # shellcheck disable=SC2016 # The script is evaluated on the coordinator.
    hm_exec "$HM_COORDINATOR" bash -c '
set -Eeuo pipefail
pvecm delnode "$1"
for _ in 1 2 3 4 5 6 7 8 9 10; do
  if pvecm status | grep -Eq "^Quorate:[[:space:]]+Yes[[:space:]]*$"; then
    exit 0
  fi
  sleep 1
done
printf "Cluster is not quorate after deleting %s; setting expected votes to %s\n" "$1" "$2" >&2
pvecm expected "$2"
' bash "$node" "$fallback_votes" ||
      die "pvecm delnode $node failed on $HM_COORDINATOR"
  else
    hm_exec "$HM_COORDINATOR" pvecm delnode "$node" ||
      die "pvecm delnode $node failed on $HM_COORDINATOR"
  fi
  deadline=$((SECONDS + 120))
  while ((SECONDS < deadline)); do
    listed="$(hm_member_states 2>/dev/null | awk -v node="$node" '$1 == node')" ||
      listed="unknown"
    [[ -n "$listed" ]] || break
    sleep 5
  done
  [[ -z "$listed" ]] || die "$node is still listed as a cluster member after pvecm delnode"
  hm_remove_node_directory "$node"
}

hm_remove_node_directory() {
  local node="$1"
  # shellcheck disable=SC2016 # The script is evaluated on the coordinator.
  hm_exec "$HM_COORDINATOR" bash -c '
set -Eeuo pipefail
node="$1"
[[ "$node" =~ ^mox([1-9]|10)$ ]]
directory="/etc/pve/nodes/${node}"
if [[ -e "$directory" ]]; then
  [[ -d "$directory" && ! -L "$directory" ]]
  rm -rf -- "$directory"
fi
[[ ! -e "$directory" ]]
' bash "$node" || die "Could not remove /etc/pve/nodes/${node}"
  info "Removed /etc/pve/nodes/${node}"
}

# Remove NODE's root key from cluster SSH trust and NODE's names and private
# address from the shared known_hosts. KEY is optional; keys whose comment is
# root@NODE are removed either way.
hm_strip_node_ssh_trust() {
  local node="$1" key="${2:-}" private_ip
  private_ip="${PRIVATE_SUBNET_PREFIX}.$((MOX_IP_START_OCTET + ${node#mox} - 1))"
  # shellcheck disable=SC2016 # The script is evaluated on the coordinator.
  hm_exec "$HM_COORDINATOR" python3 -c '
import sys
from pathlib import Path

node, domain, private_ip, key = sys.argv[1:5]
names = {node, f"{node}.{domain}", private_ip}


def rewrite(path, keep):
    if not path.exists():
        return 0
    lines = path.read_text(encoding="utf-8").splitlines()
    kept = [line for line in lines if keep(line)]
    if len(kept) != len(lines):
        with open(path, "w", encoding="utf-8") as stream:
            stream.write("".join(line + "\n" for line in kept))
    return len(lines) - len(kept)


def keep_authorized(line):
    fields = line.split()
    if key and line.strip() == key.strip():
        return False
    return not (len(fields) >= 3 and fields[-1] == f"root@{node}")


def keep_known(line):
    fields = line.split()
    if not fields or line.startswith("#"):
        return True
    hosts = fields[1] if fields[0].startswith("@") and len(fields) > 1 else fields[0]
    return not (set(hosts.split(",")) & names)


removed = rewrite(Path("/etc/pve/priv/authorized_keys"), keep_authorized)
removed += rewrite(Path("/etc/pve/priv/known_hosts"), keep_known)
print(removed)
' "$node" "$PROXMOX_INTERNAL_DOMAIN" "$private_ip" "$key" >/dev/null ||
    die "Could not remove $node from cluster SSH trust"
  info "Removed $node from cluster SSH authorized_keys and known_hosts"
}

# Rename hosts/artifacts/NODE so a future host in the slot starts fresh.
hm_archive_host_artifacts() {
  local node="$1" source destination
  source="${REPO_ROOT}/hosts/artifacts/${node}"
  [[ -e "$source" ]] || return 0
  [[ -d "$source" && ! -L "$source" ]] ||
    die "Refusing to archive unexpected path $source"
  destination="${source}.retired-$(date -u +%Y%m%dT%H%M%SZ)"
  mv -- "$source" "$destination" ||
    die "Could not archive $source"
  info "Archived this workstation's setup artifacts to $destination"
}

# Prompt for the member that replaces a departing control node.
hm_choose_new_control_node() {
  local departing="$1" destination="$2" default answer
  shift 2
  local -a candidates=("$@")
  ((${#candidates[@]} > 0)) || die "No online member can become the control node"
  default="${candidates[0]}"
  printf '\n%s is the cluster control node: new hosts join through it and it hosts\n' "$departing"
  printf 'the cluster control-plane lock. Choose the member that replaces it.\n'
  printf 'Online members that remain: %s\n' "${candidates[*]}"
  while true; do
    if bmac_ui_is_json; then
      local -a options=()
      local candidate
      for candidate in "${candidates[@]}"; do
        options+=("$candidate" "$candidate")
      done
      bmac_ui_choose answer "New cluster control node (replaces $departing)" "$default" "${options[@]}"
    else
      prompt_with_default answer "New cluster control node" "$default"
    fi
    if control_list_contains "$answer" "${candidates[@]}"; then
      printf -v "$destination" '%s' "$answer"
      return 0
    fi
    warn "Choose one of: ${candidates[*]}"
  done
}

hm_print_reinstall_follow_ups() {
  local node="$1"
  printf '\nFollow-up steps for %s:\n' "$node"
  printf '  - Remove the %s device from the Tailscale admin console (Machines) so a\n' "$node"
  printf '    future host in this slot can register the same Tailscale hostname.\n'
  printf '  - On this workstation: ssh-keygen -R %s\n' "$node"
  printf '  - Remove or update env/%s.conf. A future host may reuse the %s slot;\n' "$node" "$node"
  printf '    hosts/add_proxmox_host.sh recommends the lowest free slot.\n'
  printf '  - In Cloudflare Load Balancing, remove the %s pool and monitor from every\n' "$node"
  printf '    load balancer if you have not already done so.\n'
  bmac_ui_next_step "Remove the $node device from the Tailscale admin console (Machines) so a future host in this slot can register the same Tailscale hostname."
  bmac_ui_next_step "On this workstation, forget $node's SSH host key." --command "ssh-keygen -R $node"
  bmac_ui_next_step "Remove or update env/$node.conf. A future host may reuse the $node slot; host setup recommends the lowest free slot."
  bmac_ui_next_step "In Cloudflare Load Balancing, remove the $node pool and monitor from every load balancer if you have not already done so."
}
