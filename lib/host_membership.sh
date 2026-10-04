#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Workstation helpers shared by hosts/remove_host_from_cluster.sh,
# hosts/purge_host_from_cluster.sh, qdevice/add_qdevice.sh, and
# diagnostics/show_qdevice_state.sh: logging and prompts, command execution on
# cluster members through one coordinator, QDevice parity, the cluster
# control-plane lock that host setup also takes, Proxmox node deletion, and
# host-slot bookkeeping. Source after lib/config.sh, load_proxmox_config, and
# lib/cluster_control.sh. Callers set HM_COORDINATOR to an online member that
# survives the change.

HM_REMOTE_ROOT="/usr/local/lib/app-ha-proxmox"
HM_REMOTE_REGISTRY="${HM_REMOTE_ROOT}/lib/cluster_registry.py"
# shellcheck disable=SC2034 # Used by the scripts that source this library.
HM_REMOTE_HAPROXY_SYNC="${HM_REMOTE_ROOT}/lib/sync_haproxy_routes.sh"

HM_COORDINATOR=""
HM_QDEVICE_IPV4=""
HM_LOCK_FILE_FD=""
HM_LOCK_TOKEN=""
HM_LOCK_PID=""
HM_LOCK_READ_FD=""
HM_LOCK_WRITE_FD=""

log() {
  printf '\n==> %s\n' "$*"
}

info() {
  printf '    %s\n' "$*"
}

warn() {
  printf 'WARNING: %s\n' "$*" >&2
}

die() {
  printf '\nERROR: %s\n' "$*" >&2
  exit 1
}

prompt_with_default() {
  local destination="$1" prompt="$2" default="$3" entered
  IFS= read -r -p "${prompt} [${default}]: " entered ||
    die "Input ended before a value was entered"
  printf -v "$destination" '%s' "${entered:-$default}"
}

hm_prompt_yes() {
  local answer
  IFS= read -r -p "$1 [y/N] " answer || return 1
  [[ "${answer,,}" == y || "${answer,,}" == yes ]]
}

# Require the operator to type one exact phrase.
hm_confirm_phrase() {
  local message="$1" phrase="$2" entered
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

hm_status_value() {
  awk -v key="$2" '$0 ~ "^" key ":" { print $NF; exit }' <<<"$1"
}

hm_status_is_quorate() {
  grep -Eq '^Quorate:[[:space:]]+Yes[[:space:]]*$' <<<"$1"
}

hm_has_qdevice() {
  grep -Eq '^Flags:.*(^|[[:space:]])Qdevice([[:space:]]|$)' <<<"$1"
}

# The same exact vote checks host setup applies (see setup_proxmox_host.sh).
hm_qdevice_status_is_healthy() {
  local status=$1 node_count=$2 qdevice_voters
  [[ "$(hm_status_value "$status" 'Expected votes')" == "$((node_count + 1))" ]] ||
    return 1
  [[ "$(hm_status_value "$status" 'Total votes')" == "$((node_count + 1))" ]] ||
    return 1
  hm_status_is_quorate "$status" || return 1
  hm_has_qdevice "$status" || return 1
  qdevice_voters="$(
    awk '
      $1 ~ /^0x[0-9a-fA-F]+$/ && $1 != "0x00000000" &&
      $2 == 1 && $3 ~ /^A,V,/ { count++ }
      END { print count + 0 }
    ' <<<"$status"
  )"
  [[ "$qdevice_voters" == "$node_count" ]] || return 1
  awk '
    $1 == "0x00000000" && $2 == 1 && $3 == "Qdevice" { found++ }
    END { exit !(found == 1) }
  ' <<<"$status"
}

hm_qdevice_absent_status_is_healthy() {
  local status=$1 node_count=$2
  [[ "$(hm_status_value "$status" 'Expected votes')" == "$node_count" &&
    "$(hm_status_value "$status" 'Total votes')" == "$node_count" ]] ||
    return 1
  hm_status_is_quorate "$status" || return 1
  ! hm_has_qdevice "$status"
}

# Ask whether the workstation can reach the QDevice, then prove it and resolve
# the QDevice's literal Tailscale IPv4 the same way host setup does.
hm_verify_qdevice_access() {
  log "QDevice access"
  info "Membership changes reconcile the external QDevice vote. This script"
  info "connects to it as: ssh root@${PROXMOX_QDEVICE_HOST}"
  hm_prompt_yes "Can you run 'ssh ${PROXMOX_QDEVICE_HOST}' from this workstation without a password prompt?" ||
    die "Set up workstation SSH access to ${PROXMOX_QDEVICE_HOST} (see qdevice/QDEVICE_MANUAL_SETUP.md) and rerun"
  ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
    -o ConnectTimeout=10 "root@${PROXMOX_QDEVICE_HOST}" true </dev/null ||
    die "Non-interactive 'ssh root@${PROXMOX_QDEVICE_HOST}' failed"
  local resolved remote_ip
  resolved="$(
    tailscale status --json | jq -er --arg host "$PROXMOX_QDEVICE_HOST" '
      [
        .Peer | to_entries[] | .value |
        select(
          .HostName == $host or
          ((.DNSName // "") | rtrimstr(".") |
            (. == $host or startswith($host + ".")))
        ) |
        .TailscaleIPs[]? |
        select(type == "string" and test("^100\\.[0-9]+\\.[0-9]+\\.[0-9]+$"))
      ] | unique |
      if length == 1 then .[0]
      else error("QDevice hostname did not resolve to exactly one Tailscale IPv4")
      end
    '
  )" || die "Could not resolve $PROXMOX_QDEVICE_HOST to one stable Tailscale IPv4"
  [[ "$resolved" =~ ^100\.([0-9]{1,3}\.){2}[0-9]{1,3}$ ]] ||
    die "Resolved QDevice address is not a Tailscale IPv4: $resolved"
  remote_ip="$(
    ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
      -o ConnectTimeout=10 "root@${PROXMOX_QDEVICE_HOST}" \
      "tailscale ip -4" </dev/null |
      awk '/^100\./ { print; exit }'
  )" || die "Could not verify the Tailscale IPv4 on $PROXMOX_QDEVICE_HOST"
  [[ "$remote_ip" == "$resolved" ]] ||
    die "MagicDNS resolved $PROXMOX_QDEVICE_HOST to $resolved, but that host reports $remote_ip"
  HM_QDEVICE_IPV4="$resolved"
  info "Verified ssh root@${PROXMOX_QDEVICE_HOST} (Tailscale $HM_QDEVICE_IPV4)"
}

hm_prepare_qdevice() {
  info "Verifying the corosync-qnetd service on ${PROXMOX_QDEVICE_HOST}"
  # shellcheck disable=SC2029 # HM_QDEVICE_IPV4 is a validated literal IPv4.
  ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
    -o ConnectTimeout=10 "root@${PROXMOX_QDEVICE_HOST}" \
    "flock -w 1800 /run/lock/app-ha-qdevice-provision.lock bash -s -- '$HM_QDEVICE_IPV4'" <<'REMOTE'
set -Eeuo pipefail
expected_tailscale_ip="$1"
actual_tailscale_ip="$(tailscale ip -4 | awk '/^100\./ { print; exit }')"
[[ "$actual_tailscale_ip" == "$expected_tailscale_ip" ]]
if ! dpkg-query -W -f='${Status}\n' corosync-qnetd 2>/dev/null |
     grep -Fxq 'install ok installed'; then
  apt-get update
  DEBIAN_FRONTEND=noninteractive apt-get install -y corosync-qnetd
fi
systemctl enable --now corosync-qnetd
for _ in $(seq 1 10); do
  systemctl is-active --quiet corosync-qnetd &&
    ss -lnt | awk '$4 ~ /:5403$/ { found=1 } END { exit !found }' &&
    exit 0
  sleep 1
done
printf 'corosync-qnetd did not become active and listen on TCP 5403\n' >&2
exit 1
REMOTE
}

# Add or remove the coordinator's root key on the QDevice. pvecm qdevice setup
# copies its certificates over that SSH trust.
hm_qdevice_setup_key() {
  local mode="$1" key key_b64
  key="$(hm_exec "$HM_COORDINATOR" cat /root/.ssh/id_rsa.pub)" ||
    die "Could not read $HM_COORDINATOR's QDevice setup SSH key"
  [[ "$key" == ssh-rsa\ * ]] ||
    die "$HM_COORDINATOR's QDevice setup SSH key is not an RSA public key"
  key_b64="$(printf '%s' "$key" | base64 -w0)"
  # shellcheck disable=SC2029 # key_b64 is restricted to base64 output.
  ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
    -o ConnectTimeout=10 "root@${PROXMOX_QDEVICE_HOST}" \
    "KEY_B64='$key_b64' MODE='$mode' bash -s" <<'REMOTE'
set -Eeuo pipefail
key="$(printf '%s' "$KEY_B64" | base64 -d)"
install -d -m 0700 /root/.ssh
touch /root/.ssh/authorized_keys
chmod 0600 /root/.ssh/authorized_keys
if [[ "$MODE" == add ]]; then
  grep -Fxq "$key" /root/.ssh/authorized_keys ||
    printf '%s\n' "$key" >>/root/.ssh/authorized_keys
else
  work="$(mktemp /root/.ssh/authorized_keys.app-ha.XXXXXX)"
  awk -v key="$key" '$0 != key { print }' /root/.ssh/authorized_keys >"$work"
  install -o root -g root -m 0600 "$work" /root/.ssh/authorized_keys
  rm -f "$work"
fi
REMOTE
}

# Require every member to report the expected QDevice layout for NODE_COUNT.
hm_assert_qdevice_layout() {
  local node_count="$1" node state status states members=0
  states="$(hm_member_states)" || die "Could not list cluster members"
  while read -r node state; do
    [[ -n "$node" ]] || continue
    members=$((members + 1))
    [[ "$state" == online ]] || die "$node is not online"
    status="$(hm_exec "$node" pvecm status)" ||
      die "Could not read cluster status from $node"
    if ((node_count % 2 == 0)); then
      hm_qdevice_status_is_healthy "$status" "$node_count" ||
        die "$node does not report the QDevice alive and voting with expected/total votes $((node_count + 1))"
    else
      hm_qdevice_absent_status_is_healthy "$status" "$node_count" ||
        die "$node does not report $node_count votes with the QDevice absent"
    fi
  done <<<"$states"
  ((members == node_count)) ||
    die "Expected $node_count cluster members, found $members"
}

# Print the first reason the members do not show the QDevice layout for
# NODE_COUNT, or nothing when they all do. Returns 1 when there is a reason.
hm_qdevice_layout_problem() {
  local node_count="$1" node state status states members=0
  states="$(hm_member_states)" || {
    printf 'could not list cluster members\n'
    return 1
  }
  while read -r node state; do
    [[ -n "$node" ]] || continue
    members=$((members + 1))
    if [[ "$state" != online ]]; then
      printf '%s is not online\n' "$node"
      return 1
    fi
    if ! status="$(hm_exec "$node" pvecm status)"; then
      printf 'could not read cluster status from %s\n' "$node"
      return 1
    fi
    if ((node_count % 2 == 0)); then
      if ! hm_qdevice_status_is_healthy "$status" "$node_count"; then
        printf '%s does not report the QDevice alive and voting with expected/total votes %s\n' \
          "$node" "$((node_count + 1))"
        return 1
      fi
    elif ! hm_qdevice_absent_status_is_healthy "$status" "$node_count"; then
      printf '%s does not report %s votes with the QDevice absent\n' "$node" "$node_count"
      return 1
    fi
  done <<<"$states"
  if ((members != node_count)); then
    printf 'expected %s cluster members, found %s\n' "$node_count" "$members"
    return 1
  fi
}

# Replace the managed QDevice host-key block in a known_hosts file with one
# trusted line. Run on a cluster member as: bash -c "$HM_QDEVICE_PIN_SCRIPT"
# bash TRUST_B64 KNOWN_HOSTS. The same block is written by host setup.
HM_QDEVICE_PIN_SCRIPT="$(
  cat <<'REMOTE'
set -Eeuo pipefail
trust="$(printf '%s' "$1" | base64 -d)"
known_hosts="$2"
begin='# BEGIN app-ha managed qdevice host key'
end='# END app-ha managed qdevice host key'
[[ "$trust" =~ ^[A-Za-z0-9._-]+,[0-9.]+\ ssh-ed25519\ [A-Za-z0-9+/]+={0,2}$ ]]
install -d -m 0700 "$(dirname -- "$known_hosts")"
touch "$known_hosts"
[[ -f "$known_hosts" && ! -L "$known_hosts" ]]
chmod 0600 "$known_hosts"
begin_count="$(grep -Fxc "$begin" "$known_hosts" || true)"
end_count="$(grep -Fxc "$end" "$known_hosts" || true)"
if ! { [[ "$begin_count" == 0 && "$end_count" == 0 ]] ||
  [[ "$begin_count" == 1 && "$end_count" == 1 ]]; }; then
  printf 'Managed QDevice host-key markers are unbalanced or duplicated in %s\n' \
    "$known_hosts" >&2
  exit 1
fi
work="$(mktemp "${known_hosts}.app-ha.XXXXXX")"
awk -v begin="$begin" -v end="$end" '
  $0 == begin { managed=1; next }
  $0 == end { managed=0; next }
  !managed { print }
' "$known_hosts" >"$work"
printf '%s\n%s\n%s\n' "$begin" "$trust" "$end" >>"$work"
chmod 0600 "$work"
mv -f -- "$work" "$known_hosts"
REMOTE
)"

# Pin the QDevice's ED25519 host key, read over the operator-verified
# workstation SSH connection, in every member's root known_hosts. pvecm
# qdevice setup connects to the QDevice by its Tailscale IPv4, and a
# replacement QDevice has a new host key.
hm_pin_qdevice_host_key() {
  local host_key key_type key_data trust trust_b64 node state states
  [[ "$HM_QDEVICE_IPV4" =~ ^100\. ]] ||
    die "A verified Tailscale IPv4 is required to pin the QDevice host key"
  host_key="$(
    ssh -o BatchMode=yes -o StrictHostKeyChecking=yes -o CheckHostIP=yes \
      -o ConnectTimeout=10 "root@${PROXMOX_QDEVICE_HOST}" \
      cat /etc/ssh/ssh_host_ed25519_key.pub </dev/null
  )" || die "Could not read the ED25519 host key of ${PROXMOX_QDEVICE_HOST}"
  read -r key_type key_data _ <<<"$host_key"
  [[ "$key_type" == ssh-ed25519 && "$key_data" =~ ^[A-Za-z0-9+/]+={0,2}$ ]] ||
    die "${PROXMOX_QDEVICE_HOST} returned an invalid ED25519 host key"
  info "QDevice host key: $(printf '%s %s\n' "$key_type" "$key_data" | ssh-keygen -lf -)"
  trust="${PROXMOX_QDEVICE_HOST},${HM_QDEVICE_IPV4} ${key_type} ${key_data}"
  trust_b64="$(printf '%s' "$trust" | base64 | tr -d '\n')"
  states="$(hm_member_states)" || die "Could not list cluster members"
  while read -r node state; do
    [[ -n "$node" ]] || continue
    [[ "$state" == online ]] || die "$node is not online"
    hm_exec "$node" bash -c "$HM_QDEVICE_PIN_SCRIPT" bash "$trust_b64" \
      /root/.ssh/known_hosts ||
      die "Could not pin the QDevice host key on $node"
  done <<<"$states"
  info "Pinned the QDevice host key on every member"
}

# Print the QDevice address registered in corosync.conf, or nothing.
hm_registered_qdevice_address() {
  # shellcheck disable=SC2016 # awk program.
  hm_exec "$HM_COORDINATOR" awk '
    /^[[:space:]]*device[[:space:]]*[{]/ { device=1 }
    device && /^[[:space:]]*host:/ { print $2; exit }
  ' /etc/pve/corosync.conf
}

hm_add_qdevice() {
  local node_count="$1"
  [[ "$HM_QDEVICE_IPV4" =~ ^100\. ]] ||
    die "A verified Tailscale IPv4 is required for QDevice setup"
  log "Adding the external QDevice vote for the ${node_count}-node cluster"
  info "This needs the Tailscale ACL that lets tag:proxmox-host reach tag:proxmox-qdevice on tcp:22."
  hm_prepare_qdevice
  hm_pin_qdevice_host_key
  hm_exec "$HM_COORDINATOR" bash -c \
    'test -f /root/.ssh/id_rsa.pub || ssh-keygen -q -t rsa -b 4096 -N "" -f /root/.ssh/id_rsa' ||
    die "Could not prepare $HM_COORDINATOR's QDevice setup SSH key"
  hm_qdevice_setup_key add
  _mox_ssh_options "$HM_COORDINATOR" || die "Could not prepare SSH to $HM_COORDINATOR"
  local destination
  destination="$(_mox_ssh_destination "$HM_COORDINATOR")" ||
    die "Could not resolve $HM_COORDINATOR"
  # shellcheck disable=SC2029 # HM_QDEVICE_IPV4 is a validated literal IPv4.
  ssh -tt "${MOX_SSH_OPTIONS[@]}" "${MOX_SSH_USER:-root}@${destination}" \
    "pvecm qdevice setup '${HM_QDEVICE_IPV4}' --force" ||
    die "pvecm qdevice setup failed on $HM_COORDINATOR"
  hm_qdevice_setup_key remove ||
    warn "Could not remove $HM_COORDINATOR's setup key from ${PROXMOX_QDEVICE_HOST}; remove it manually"
  hm_assert_qdevice_layout "$node_count"
  info "Every member reports the QDevice alive and voting"
}

# Remove the QDevice vote. Proxmox stops the qdevice service over SSH on every
# configured node, so an offline node can make the command report failure
# after it already removed the device from corosync.conf; the configuration,
# not the exit status, decides.
hm_remove_qdevice() {
  log "Removing the external QDevice vote before the membership change"
  hm_exec "$HM_COORDINATOR" pvecm qdevice remove ||
    warn "pvecm qdevice remove reported an error; checking corosync.conf"
  hm_qdevice_configured &&
    die "corosync.conf still configures the QDevice; inspect 'pvecm status' on $HM_COORDINATOR and remove it with 'pvecm qdevice remove' before rerunning"
  info "The QDevice is no longer configured"
}

hm_qdevice_configured() {
  hm_exec "$HM_COORDINATOR" grep -Eq '^[[:space:]]*device[[:space:]]*[{]' \
    /etc/pve/corosync.conf
}

# Bring the QDevice to the layout required for NODE_COUNT online members.
hm_reconcile_qdevice() {
  local node_count="$1" status
  status="$(hm_cluster_status)" || die "Could not read cluster status"
  if ((node_count % 2 == 1)); then
    if hm_qdevice_configured; then
      hm_remove_qdevice
    fi
    hm_assert_qdevice_layout "$node_count"
    info "The ${node_count}-node cluster has an odd vote count; the QDevice is correctly absent"
    return 0
  fi
  if hm_has_qdevice "$status" && hm_qdevice_configured; then
    hm_assert_qdevice_layout "$node_count"
    info "The QDevice is configured, alive, and voting"
    return 0
  fi
  hm_add_qdevice "$node_count"
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
hm_delete_cluster_node() {
  local node="$1" deadline listed
  log "Deleting $node from the Proxmox cluster"
  hm_exec "$HM_COORDINATOR" pvecm delnode "$node" ||
    die "pvecm delnode $node failed on $HM_COORDINATOR"
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
    prompt_with_default answer "New cluster control node" "$default"
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
  printf '    hosts/setup_proxmox_host.sh recommends the lowest free slot.\n'
  printf '  - In Cloudflare Load Balancing, remove the %s pool and monitor from every\n' "$node"
  printf '    load balancer if you have not already done so.\n'
}
