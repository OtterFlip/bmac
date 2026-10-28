#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Create a new production VM from a restore point on PBS: add_prod_vm.sh with
# the OS install replaced by a restore. The VM gets a registry slot (the
# restore point's own prodN when it is free), the restore runs stopped with
# the slot's identity, the root disk is patched offline (hostname, network,
# root password, administrator keys, and, in a different slot, a new
# machine-id and SSH host keys), and add_prod_vm.sh's own steps verify the
# guest and add networking, replication, HA, ingress, and monitoring. A rerun
# with the same purpose resumes, as add_prod_vm.sh does.
#
# With --instant (Instant Restore to New Permanent VM) the VM runs within
# about a minute instead of after the whole restore: it boots, patched, from a
# qcow2 overlay over its restore point mapped read-only from PBS, while
# pbs-restore writes the restore point into its zvol. instant_switch.py then
# moves the running disk onto the zvol, and only after that do replication,
# HA, and the hook come, as for any recovery. Any rerun resumes an instant
# restore under way, with or without --instant.

# This script sets many globals that the add_prod_vm.sh functions it sources
# read.
# shellcheck disable=SC2034

set -Eeuo pipefail
set +x
umask 077

RECOVERY_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
RECOVERY_REPO_ROOT="$(cd -- "${RECOVERY_DIR}/../../.." && pwd -P)"
# shellcheck source=../guests/prod/add_prod_vm.sh
PRODUCTION_VM_SOURCE_ONLY=1 source "${RECOVERY_REPO_ROOT}/scripts/user_callable/guests/prod/add_prod_vm.sh"
# shellcheck source=../../lib/backups.sh
source "${RECOVERY_REPO_ROOT}/scripts/lib/backups.sh"

PATCH_HELPER="${RECOVERY_REPO_ROOT}/scripts/utilities/patch_staging_clone.sh"
GUEST_TREE_HELPER="${RECOVERY_REPO_ROOT}/scripts/utilities/patch_staging_guest_tree.py"
POLL_SECONDS="${FULL_RECOVERY_POLL_SECONDS:-5}"
SNAPSHOT=""
REQUESTED_CORES=""
REQUESTED_MEMORY_GIB=""
REMOTE_RUN_DIR=""
NEW_IDENTITY=false
INSTANT=false
# The instant restore's overlay, and whether scsi0 is still that overlay.
INSTANT_OVERLAY=""
INSTANT_PENDING=false
INSTANT_ROUTED=false
COPY_DELAY_SECONDS=0
# What the restore point holds, from its notes and its backed-up VM config.
BACKUP_NAME=""
BACKUP_VMID=""
BACKUP_PURPOSE=""
BACKUP_FQDN=""
BACKUP_CORES=""
BACKUP_MEMORY_MB=""
BACKUP_DISK_BYTES=""
BACKUP_TIME=""

# A byte count as GiB or TiB, for messages.
size_text() {
  python3 -c '
import sys
value = int(sys.argv[1])
print(f"{value / 2**40:.2f} TiB" if value >= 2**40 else f"{value / 2**30:.1f} GiB")' "$1"
}

usage() {
  cat <<'EOF'
Usage: scripts/user_callable/backups/full_recovery.sh [--snapshot vm/VMID/TIME]
         [--cores N] [--memory-gib N] [--instant] [--copy-delay-seconds N] [--json]

Create a new production VM from one of BMAC's restore points on PBS, for
example on a rebuilt cluster. It is add_prod_vm.sh with the OS install
replaced by a restore:

  - The VM gets the restore point's own slot (prodN, and so its address)
    and VMID when they are free, otherwise the lowest free slot and a new
    VMID. It asks for the purpose, the FQDN (default: the restore point's),
    aliases, placement, vCPUs and RAM (default: the restore point's), disk
    allocation, and replication interval. The root disk keeps the restore
    point's size.
  - The restore runs with the VM stopped. The root disk is patched offline:
    hostname, network, the root password from secrets.env, and root's SSH
    keys (ADMIN_1/ADMIN_2_PUBLIC_SSH_KEY). In a different slot than the
    restore point's, where the original VM may still exist, it also gets a
    new machine-id and SSH host keys.
  - The guest is then verified, and gets its final networking, replication,
    HA, Cloudflare Tunnel routes, and monitoring, as add_prod_vm.sh does.

With --instant, the VM does not wait for the restore: it boots from the
restore point mapped read-only from PBS, behind a local overlay patched with
the same identity, and serves its FQDNs while its disk is copied in the
background. Reads of data the copy has not reached yet are slower. Once the
copy is done the VM's disk is switched to its own zvol while it runs, and it
gets replication and HA.

Rerun with the same purpose to resume after a failure.

  --snapshot vm/VMID/TIME   The restore point (chosen from a list when omitted).
  --cores N, --memory-gib N Default: the restore point's values.
  --instant                 Instant Restore to New Permanent VM.
  --copy-delay-seconds N    Developer option, with --instant: keep the VM
                            running from PBS for N seconds (at most 3600)
                            before its data is copied, to try it there.
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --snapshot | --cores | --memory-gib | --copy-delay-seconds)
        (($# >= 2)) || die "$1 needs a value"
        case "$1" in
          --snapshot) SNAPSHOT="$2" ;;
          --cores) REQUESTED_CORES="$2" ;;
          --memory-gib) REQUESTED_MEMORY_GIB="$2" ;;
          --copy-delay-seconds) COPY_DELAY_SECONDS="$2" ;;
        esac
        shift 2
        ;;
      --instant)
        INSTANT=true
        shift
        ;;
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
  [[ -z "$SNAPSHOT" || "$SNAPSHOT" =~ ^vm/[1-9][0-9]{0,8}/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] ||
    die "--snapshot must look like vm/100/2026-10-27T14:50:01Z"
  [[ -z "$REQUESTED_CORES" || "$REQUESTED_CORES" =~ ^[1-9][0-9]{0,3}$ ]] ||
    die "--cores must be a positive whole number"
  [[ -z "$REQUESTED_MEMORY_GIB" || "$REQUESTED_MEMORY_GIB" =~ ^[1-9][0-9]{0,5}$ ]] ||
    die "--memory-gib must be a positive whole number"
  [[ "$COPY_DELAY_SECONDS" =~ ^[0-9]{1,4}$ ]] && ((COPY_DELAY_SECONDS <= 3600)) ||
    die "--copy-delay-seconds must be 0 through 3600"
  [[ "$COPY_DELAY_SECONDS" == 0 || "$INSTANT" == true ]] || die "--copy-delay-seconds is for --instant"
}

# add_prod_vm.sh's cleanup (which releases the orchestration lease) runs
# after the staged patch files are removed. Without set +e, restoring the
# exit status for it would end this trap first.
eval "add_prod_vm_cleanup() $(declare -f cleanup | tail -n +2)"
cleanup() {
  local code=$?
  set +e
  remove_remote_patch_bundle
  (exit "$code")
  add_prod_vm_cleanup
}

# BMAC's restore points this cluster's key can decrypt, newest first:
# snapshot, VMID, notes name, purpose, FQDN, and label, separated by \x1f.
list_recovery_points() {
  local key
  key="$(pvesh_get "/storage/${BACKUP_STORAGE_ID}" | python3 -c '
import json, sys
print(json.load(sys.stdin).get("encryption-key") or "")')" || die "Could not read storage ${BACKUP_STORAGE_ID}"
  pvesh_get "/nodes/${COORDINATOR}/storage/${BACKUP_STORAGE_ID}/content" | python3 -c '
import calendar, json, sys, time
prefix, key = sys.argv[1] + ":backup/", sys.argv[2].lower()
rows = []
for row in json.load(sys.stdin):
    first = str(row.get("notes") or "").splitlines()[0] if row.get("notes") else ""
    if not first.startswith("bmac ") or not str(row.get("volid", "")).startswith(prefix):
        continue
    if str(row.get("encrypted") or "").lower() != key:
        continue
    head, _, comment = first.partition(" | ")
    words = head.split()
    fields = dict(word.split("=", 1) for word in words[2:] if "=" in word)
    snapshot = row["volid"][len(prefix):]
    taken = calendar.timegm(time.strptime(snapshot.rsplit("/", 1)[-1], "%Y-%m-%dT%H:%M:%SZ"))
    label = " · ".join(part for part in (
        time.strftime("%Y-%m-%d %H:%M %Z", time.localtime(taken)), words[1] if len(words) > 1 else "?",
        fields.get("fqdn", ""), comment.strip()[:48]) if part)
    rows.append((snapshot, str(row.get("vmid")), words[1] if len(words) > 1 else "",
                 fields.get("slug", ""), fields.get("fqdn", ""), label))
for row in sorted(rows, key=lambda r: r[0].rsplit("/", 1)[-1], reverse=True):
    print("\x1f".join(row))' "$BACKUP_STORAGE_ID" "$key"
}

choose_snapshot() {
  local -a points=() options=()
  local row snapshot vmid name purpose fqdn label found=false
  mapfile -t points < <(list_recovery_points)
  ((${#points[@]})) || die "There are no BMAC restore points this cluster's key can restore"
  if [[ -z "$SNAPSHOT" ]]; then
    for row in "${points[@]}"; do
      IFS=$'\x1f' read -r snapshot vmid name purpose fqdn label <<<"$row"
      options+=("$snapshot" "$label")
    done
    if bmac_ui_is_json; then
      bmac_ui_choose SNAPSHOT "Restore point to recover" "${options[0]}" "${options[@]}"
    else
      printf '\nRestore points, newest first:\n'
      for row in "${points[@]}"; do
        IFS=$'\x1f' read -r snapshot vmid name purpose fqdn label <<<"$row"
        printf '  %s  %s\n' "$snapshot" "$label"
      done
      read -r -p "Restore point [${options[0]}]: " SNAPSHOT
      SNAPSHOT="${SNAPSHOT:-${options[0]}}"
    fi
  fi
  for row in "${points[@]}"; do
    IFS=$'\x1f' read -r snapshot vmid name purpose fqdn label <<<"$row"
    if [[ "$snapshot" == "$SNAPSHOT" ]]; then
      found=true
      BACKUP_VMID="$vmid"
      BACKUP_NAME="$name"
      BACKUP_PURPOSE="$purpose"
      BACKUP_FQDN="$fqdn"
      BACKUP_TIME="${label%% · *}"
    fi
  done
  [[ "$found" == true ]] ||
    die "$SNAPSHOT is not a BMAC restore point this cluster's key can restore"
  [[ "$BACKUP_NAME" =~ ^prod[1-9][0-9]*$ ]] ||
    die "$SNAPSHOT's notes do not name its production VM"
}

# The VM config inside the restore point: one EFI disk, one scsi0 root disk,
# one NIC, as add_prod_vm.sh creates them.
read_backup_config() {
  local config fields
  config="$(pvesh_get "/nodes/${COORDINATOR}/vzdump/extractconfig" --volume "${BACKUP_STORAGE_ID}:backup/${SNAPSHOT}")" ||
    die "Could not read the VM configuration stored in $SNAPSHOT"
  fields="$(python3 - "$config" "${RUN_DIR}/backup-config.json" <<'PY'
import json
import re
import sys

text = json.loads(sys.argv[1])
config = {}
for line in str(text).splitlines():
    if line.startswith("["):
        break
    if ": " in line and not line.startswith("#"):
        key, value = line.split(": ", 1)
        config[key] = value
disks = sorted(key for key in config if re.fullmatch(r"(?:scsi|sata|virtio|ide|efidisk|tpmstate|unused)[0-9]+", key)
               and "media=cdrom" not in config[key])
if disks != ["efidisk0", "scsi0"]:
    raise SystemExit(f"the restore point's VM has disks {disks}, not one EFI disk and one scsi0 root disk")
if sorted(key for key in config if re.fullmatch(r"net[0-9]+", key)) != ["net0"]:
    raise SystemExit("the restore point's VM does not have exactly one NIC")
match = re.search(r"(?:^|,)size=([0-9]+)([KMGT])(?:,|$)", config["scsi0"])
if not match:
    raise SystemExit("the restore point's root disk has no size")
size = int(match.group(1)) * 1024 ** "KMGT".index(match.group(2)) * 1024
memory = config.get("memory", "").removeprefix("current=").split(",", 1)[0]
cores = config.get("cores", "1")
if not memory.isdigit() or not cores.isdigit():
    raise SystemExit("the restore point's vCPUs or RAM are unreadable")
with open(sys.argv[2], "w", encoding="utf-8") as stream:
    json.dump(config, stream)
print(cores, memory, size)
PY
  )" || die "$SNAPSHOT cannot be recovered as a BMAC production VM"
  read -r BACKUP_CORES BACKUP_MEMORY_MB BACKUP_DISK_BYTES <<<"$fields"
  ((BACKUP_DISK_BYTES % (1024 * 1024) == 0)) ||
    die "The restore point's root disk is not a whole number of MiB"
}

registry_has_name() {
  python3 -c '
import json, sys
sys.exit(0 if any(row.get("name") == sys.argv[2] for row in json.load(open(sys.argv[1]))) else 1)' \
    "${RUN_DIR}/resources.json" "$1"
}

# domain_holder DOMAIN: prints the registered VM that already routes DOMAIN,
# as its primary domain, an alias, or one of its staging FQDNs
# (stageN<name>.<primary>), and fails when no VM does. The registry and the
# tunnel would refuse it later, after the form.
domain_holder() {
  python3 -c '
import json, re, sys
domain = sys.argv[2]
for row in json.load(open(sys.argv[1])):
    domains = row.get("domains") or {}
    primary, name = domains.get("primary") or "", row.get("name") or ""
    if domain == primary or domain in (domains.get("aliases") or []) or (
            primary and re.fullmatch(r"stage[0-9]+" + re.escape(name) + r"\." + re.escape(primary), domain)):
        print(name)
        sys.exit(0)
sys.exit(1)' "${RUN_DIR}/resources.json" "$1"
}

# domain_conflicts: one "FIELD<US>MESSAGE" line for the (normalized) primary
# domain and for each alias that a registered VM already routes.
domain_conflicts() {
  local domain holder
  if holder="$(domain_holder "$PRIMARY_DOMAIN")"; then
    printf 'primary_domain\x1f%s already routes to %s; enter another FQDN\n' "$PRIMARY_DOMAIN" "$holder"
  fi
  for domain in "${ALIAS_DOMAINS[@]}"; do
    if holder="$(domain_holder "$domain")"; then
      printf 'aliases\x1fAlias %s already routes to %s\n' "$domain" "$holder"
    fi
  done
}

require_free_domains() {
  local conflict
  conflict="$(domain_conflicts | head -n 1)"
  [[ -z "$conflict" ]] || die "${conflict#*$'\x1f'}"
}

# A VMID for the new VM: the restore point's own when no live or registered VM
# uses it (its backup group then continues), otherwise the next one that no
# VM and no other backup group uses.
choose_vmid() {
  write_json "${RUN_DIR}/all-live-vms.json" "$(pvesh_get /cluster/resources --type vm)"
  write_json "${RUN_DIR}/backups.json" "$(pvesh_get "/nodes/${COORDINATOR}/storage/${BACKUP_STORAGE_ID}/content")"
  local nextid
  nextid="$(pvesh_get /cluster/nextid)" || die "Could not query the next free Proxmox VMID"
  VMID="$(python3 - "$nextid" "${RUN_DIR}/all-live-vms.json" "${RUN_DIR}/resources.json" \
    "${RUN_DIR}/backups.json" "$BACKUP_VMID" <<'PY'
import json
import sys

value = int(json.loads(sys.argv[1]))
live = json.load(open(sys.argv[2]))
registered = json.load(open(sys.argv[3]))
backups = json.load(open(sys.argv[4]))
original = int(sys.argv[5])
in_use = {int(row["vmid"]) for row in [*live, *registered] if str(row.get("vmid", "")).isdigit()}
if original not in in_use:
    print(original)
    raise SystemExit(0)
taken = in_use | {int(row["vmid"]) for row in backups if str(row.get("vmid", "")).isdigit()}
while value in taken:
    value += 1
print(value)
PY
  )" || die "Could not choose a VMID"
}

collect_recovery_request() {
  local placement_csv="" aliases_raw="" allocation_choice="sparse" message node default_memory_gib
  local holder="" domain_help=""
  local -a node_options=()
  default_memory_gib=$(((BACKUP_MEMORY_MB + 1023) / 1024))
  VM_CORES="${REQUESTED_CORES:-$BACKUP_CORES}"
  VM_MEMORY_GIB="${REQUESTED_MEMORY_GIB:-$default_memory_gib}"
  PRIMARY_DOMAIN="$BACKUP_FQDN"
  # The restore point's FQDN is only a useful default while no VM holds it,
  # as when its VM is gone.
  if [[ -n "$BACKUP_FQDN" ]] && holder="$(domain_holder "$BACKUP_FQDN")"; then
    PRIMARY_DOMAIN=""
    domain_help="The restore point's FQDN, $BACKUP_FQDN, routes to $holder; this VM needs another one."
  fi
  REPLICATION_MINUTES="$DEFAULT_REPLICATION_MINUTES"
  INITIAL_NODE="${ONLINE_NODES[0]}"
  if bmac_ui_is_json; then
    for node in "${ONLINE_NODES[@]}"; do
      node_options+=(--option "$node" "$node")
    done
    bmac_ui_group_begin recovery "Recovered production VM ($PURPOSE_SLUG)" \
      "The root disk keeps the restore point's size ($(size_text "$BACKUP_DISK_BYTES")); extend it later with extend_prod_vm_disk.sh."
    bmac_ui_group_add placement_csv --id placement --type multiselect \
      --label "Placement nodes (minimum two)" --required --min-selected 2 \
      --default "$(IFS=,; printf '%s' "${ONLINE_NODES[*]}")" "${node_options[@]}"
    bmac_ui_group_add INITIAL_NODE --id initial_node --type select \
      --label "Node to restore on" --required --default "${ONLINE_NODES[0]}" "${node_options[@]}"
    bmac_ui_group_add PRIMARY_DOMAIN --id primary_domain --label "Primary production FQDN" \
      --default "$PRIMARY_DOMAIN" --placeholder example.com --required ${domain_help:+--help "$domain_help"}
    bmac_ui_group_add aliases_raw --id aliases --label "Alias domains (comma-separated, optional)" \
      --placeholder "www.example.com"
    bmac_ui_group_add VM_CORES --id cores --type integer --label "vCPUs" \
      --default "$VM_CORES" --min 1 --required
    bmac_ui_group_add VM_MEMORY_GIB --id memory_gib --type integer --label "RAM" --suffix GiB \
      --default "$VM_MEMORY_GIB" --min 1 --required
    bmac_ui_group_add allocation_choice --id allocation --type select --label "Disk allocation" \
      --default sparse --required --option sparse "Sparse (recommended)" \
      --option full "Full (refreservation)"
    bmac_ui_group_add REPLICATION_MINUTES --id replication_minutes --type integer \
      --label "Replication interval" --suffix minutes --default "$DEFAULT_REPLICATION_MINUTES" --min 1 --required
    while true; do
      bmac_ui_group_request
      if ! message="$(validate_placement_csv "$placement_csv" 2>&1 >/dev/null)"; then
        bmac_ui_field_error placement "${message:-Invalid placement selection}"
      elif [[ ",$placement_csv," != *",$INITIAL_NODE,"* ]]; then
        bmac_ui_field_error initial_node "The node to restore on must be one of the placement nodes"
      fi
      # shellcheck disable=SC2030,SC2031 # A dry run in a subshell; applied below.
      if ! message="$( (
        die() { printf '%s\n' "$*" >&2; exit 1; }
        PRIMARY_DOMAIN="$(normalize_domain "$PRIMARY_DOMAIN" 2>&1)" || die "$PRIMARY_DOMAIN"
        split_and_normalize_aliases "$aliases_raw"
        domain_conflicts >&2
      ) 2>&1 >/dev/null)"; then
        if [[ "$message" == *alias* || "$message" == *Alias* ]]; then
          bmac_ui_field_error aliases "$(tail -n 1 <<<"$message")"
        else
          bmac_ui_field_error primary_domain "$(tail -n 1 <<<"$message")"
        fi
      else
        local conflict
        while IFS= read -r conflict; do
          [[ "$conflict" == *$'\x1f'* ]] &&
            bmac_ui_field_error "${conflict%%$'\x1f'*}" "${conflict#*$'\x1f'}"
        done <<<"$message"
      fi
      local field name
      for field in VM_CORES:cores VM_MEMORY_GIB:memory_gib REPLICATION_MINUTES:replication_minutes; do
        name="${field%%:*}"
        [[ "${!name}" =~ ^[1-9][0-9]*$ ]] || bmac_ui_field_error "${field#*:}" "Enter a positive whole number"
      done
      bmac_ui_group_check && break
    done
  else
    printf '\nOnline nodes: %s\n' "${ONLINE_NODES[*]}"
    prompt_with_default placement_csv "Placement nodes (comma-separated, minimum two)" \
      "$(IFS=,; printf '%s' "${ONLINE_NODES[*]}")"
    prompt_with_default INITIAL_NODE "Node to restore on" "${placement_csv%%,*}"
    [[ -z "$domain_help" ]] || info "$domain_help"
    # shellcheck disable=SC2031 # The form's dry run is a subshell; this path never runs it.
    prompt_with_default PRIMARY_DOMAIN "Primary production FQDN" "$PRIMARY_DOMAIN"
    read -r -p "Alias domains (comma-separated, optional): " aliases_raw
    prompt_with_default VM_CORES "vCPUs" "$VM_CORES"
    prompt_with_default VM_MEMORY_GIB "RAM in GiB" "$VM_MEMORY_GIB"
    prompt_with_default allocation_choice "Disk allocation (sparse or full)" sparse
    prompt_with_default REPLICATION_MINUTES "Replication interval in minutes" "$DEFAULT_REPLICATION_MINUTES"
  fi
  placement_csv="$(validate_placement_csv "$placement_csv")" || die "Invalid placement selection"
  IFS=',' read -r -a PLACEMENT_NODES <<<"$placement_csv"
  [[ ",$placement_csv," == *",$INITIAL_NODE,"* ]] || die "The node to restore on must be a placement node"
  # shellcheck disable=SC2031
  PRIMARY_DOMAIN="$(normalize_domain "$PRIMARY_DOMAIN")" || die "Invalid primary domain"
  split_and_normalize_aliases "$aliases_raw"
  require_free_domains
  STAGING_DNS_BASE="$(normalize_domain "staging.${PRIMARY_DOMAIN}")" || die "Invalid staging DNS base"
  [[ "$VM_CORES" =~ ^[1-9][0-9]{0,3}$ && "$VM_MEMORY_GIB" =~ ^[1-9][0-9]{0,5}$ &&
    "$REPLICATION_MINUTES" =~ ^[1-9][0-9]*$ ]] || die "vCPUs, RAM, and the replication interval must be positive whole numbers"
  VM_MEMORY_MB=$((VM_MEMORY_GIB * 1024))
  VM_DISK_GIB=$((BACKUP_DISK_BYTES / 1024 / 1024 / 1024))
  case "$allocation_choice" in
    full) DISK_ALLOCATION=reserved ;;
    sparse) DISK_ALLOCATION=sparse ;;
    *) die "Disk allocation must be sparse or full" ;;
  esac
}

# Find a recovery to resume, or reserve a new registry slot for one.
reserve_or_resume_recovery() {
  CURRENT_PHASE="collecting the recovery request"
  local purpose existing allocation_result
  if bmac_ui_is_json; then
    bmac_ui_input purpose --id purpose --label "Purpose slug of the recovered VM" \
      --help "The restore point's purpose when no registered VM uses it. A purpose that a recovery left unfinished resumes it." \
      --default "${BACKUP_PURPOSE:-production}" --required --pattern '^[a-z][a-z0-9]*(-[a-z0-9]+)*$'
  else
    prompt_with_default purpose "Purpose slug of the recovered VM" "${BACKUP_PURPOSE:-production}"
  fi
  [[ "$purpose" =~ ^[a-z][a-z0-9]*(-[a-z0-9]+)*$ && ${#purpose} -le 63 ]] ||
    die "Purpose must be a lowercase slug of at most 63 characters"
  existing="$(find_existing_purpose "$purpose")" || die "Could not search the registry by purpose"
  if [[ -n "$existing" ]]; then
    python3 -c '
import json, sys
sys.exit(0 if json.loads(sys.argv[1]).get("allocation_id") == "recover-production-" + sys.argv[2] else 1)' \
      "$existing" "$purpose" ||
      die "Purpose $purpose belongs to production VM $(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["name"])' "$existing"); choose another purpose"
    RESOURCE_JSON="$existing"
    parse_resource
    info "Resuming the recovery of $RESOURCE_NAME (VMID $VMID, $PROD_PRIVATE_IP, state $RESOURCE_STATE)"
    NEW_IDENTITY=false
    [[ "$RESOURCE_NAME" == "$BACKUP_NAME" ]] || NEW_IDENTITY=true
    return 0
  fi
  PURPOSE_SLUG="$purpose"
  collect_recovery_request
  validate_prod_host_capacity
  choose_vmid

  local -a allocate_args=(
    allocate-prod --vmid "$VMID" --purpose "$PURPOSE_SLUG" --primary-domain "$PRIMARY_DOMAIN"
    --staging-dns-base "$STAGING_DNS_BASE" --placement "$(IFS=,; printf '%s' "${PLACEMENT_NODES[*]}")"
    --initial-node "$INITIAL_NODE" --allocation-id "recover-production-${PURPOSE_SLUG}"
    --cores "$VM_CORES" --memory-mb "$VM_MEMORY_MB" --disk-gib "$VM_DISK_GIB"
    --disk-allocation "$DISK_ALLOCATION" --replication-interval "$REPLICATION_MINUTES"
  )
  registry_has_name "$BACKUP_NAME" || allocate_args+=(--index "${BACKUP_NAME#prod}")
  local alias
  for alias in "${ALIAS_DOMAINS[@]}"; do
    allocate_args+=(--alias "$alias")
  done
  CURRENT_PHASE="atomically reserving registry identity"
  allocation_result="$(registry_cmd "${allocate_args[@]}")" || die "Atomic production allocation failed"
  NEW_RESOURCE=true
  RESOURCE_JSON="$(python3 -c 'import json,sys; print(json.dumps(json.loads(sys.argv[1])["resource"], sort_keys=True))' "$allocation_result")" ||
    die "Registry returned an invalid allocation"
  parse_resource
  if ((BACKUP_DISK_BYTES > VM_DISK_GIB * 1024 * 1024 * 1024)); then
    registry_update --disk-bytes "$BACKUP_DISK_BYTES"
    refresh_resource
  fi
  NEW_IDENTITY=false
  [[ "$RESOURCE_NAME" == "$BACKUP_NAME" ]] || NEW_IDENTITY=true
  info "Recovering $BACKUP_NAME's restore point as $RESOURCE_NAME (VMID $VMID, $PROD_PRIVATE_IP)"
  [[ "$NEW_IDENTITY" == false ]] ||
    info "$BACKUP_NAME's slot is in use, so the VM gets a new machine-id and SSH host keys"
}

confirm_recovery() {
  local answer kind="Recover" message
  [[ "$INSTANT" == false ]] || kind="Instantly restore"
  log "Plan"
  info "Restore $SNAPSHOT (${BACKUP_TIME}, $BACKUP_NAME) as $RESOURCE_NAME, VMID $VMID, on $INITIAL_NODE"
  [[ "$INSTANT" == false ]] ||
    info "Instant: it boots from the restore point mapped from PBS; its disk is copied while it runs, then switched to its own zvol"
  info "Identity: ${PROD_PRIVATE_IP}, MAC $VM_MAC, ${PRIMARY_DOMAIN}${ALIAS_DOMAINS[*]:+ and ${ALIAS_DOMAINS[*]}}"
  info "Compute: ${VM_CORES} vCPUs, ${VM_MEMORY_GIB} GiB RAM, $(size_text "$(registered_disk_bytes)") root disk ($DISK_ALLOCATION)"
  info "Placement: ${PLACEMENT_NODES[*]}; replication every $REPLICATION_MINUTES minutes"
  bmac_ui_plan_begin "$kind $RESOURCE_NAME from $BACKUP_NAME's restore point"
  bmac_ui_plan_item create "$RESOURCE_NAME" "VMID $VMID restored from $SNAPSHOT on $INITIAL_NODE, ${VM_CORES} vCPUs, ${VM_MEMORY_GIB} GiB RAM"
  [[ "$INSTANT" == false ]] ||
    bmac_ui_plan_item create "instant boot" "Runs from $SNAPSHOT mapped read-only from PBS (slower reads of data not yet copied) until its disk is copied and switched to $PROD_VM_STORAGE while it runs"
  bmac_ui_plan_item update "guest identity" "Hostname $RESOURCE_NAME, ${PROD_PRIVATE_IP}, root password and SSH keys from this cluster's configuration$([[ "$NEW_IDENTITY" == true ]] && printf ', new machine-id and SSH host keys' || true)"
  bmac_ui_plan_item create "HA and replication" "Placement ${PLACEMENT_NODES[*]}"
  bmac_ui_plan_item create "tunnel FQDNs" "${PRIMARY_DOMAIN}${ALIAS_DOMAINS[*]:+, ${ALIAS_DOMAINS[*]}} and its staging slots"
  bmac_ui_plan_end
  message="A new production VM is created from ${BACKUP_TIME}'s restore point of $BACKUP_NAME."
  [[ "$INSTANT" == false ]] ||
    message+=" It serves its FQDNs within minutes; replication and HA follow once its disk is copied."
  if bmac_ui_is_json; then
    bmac_ui_confirm --id recover --title "$kind $RESOURCE_NAME?" --message "$message" \
      --confirm-label "$kind $RESOURCE_NAME" || die "Cancelled; the registry slot stays reserved, rerun with purpose $PURPOSE_SLUG to continue"
  else
    read -r -p "$kind $RESOURCE_NAME? [y/N] " answer
    [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]] ||
      die "Cancelled; the registry slot stays reserved, rerun with purpose $PURPOSE_SLUG to continue"
  fi
}

# Before a restore into the reserved VMID: an earlier attempt that was never
# patched holds nothing of value. Its VM goes with every volume of its VMID
# (an instant restore's zvol and overlay are not all in its config), and so
# does an instant restore's mapping.
discard_partial_restore() {
  local node="$INITIAL_NODE"
  if locate_vm; then
    [[ "$VM_LIVE_STATUS" == stopped ]] || die "VM $VMID is $VM_LIVE_STATUS; an unpatched recovery must stay stopped"
    node="$OWNER_NODE"
    log "Removing VM $VMID, left by an earlier attempt that was never patched"
    node_exec "$node" qm destroy "$VMID" --destroy-unreferenced-disks 1 >/dev/null ||
      die "Could not remove VM $VMID of the earlier attempt"
  else
    [[ "$RESOURCE_STATE" == reserved || "$RESOURCE_STATE" == provisioning ]] ||
      die "Registered $RESOURCE_STATE VM $VMID is missing; refusing to restore it again"
  fi
  if node_exec "$node" test -x "$BACKUP_LINK_HELPER"; then
    { node_exec "$node" "$BACKUP_LINK_HELPER" unmap "$VMID" >/dev/null &&
      node_exec "$node" "$BACKUP_LINK_HELPER" forget "$VMID" >/dev/null; } ||
      die "Could not remove the PBS mapping of VM $VMID on $node"
  fi
}

# Restore the VM stopped, with the registry's identity, into the reserved
# VMID; a rerun before the patch restores again in place of the earlier
# attempt.
restore_vm() {
  CURRENT_PHASE="restoring $SNAPSHOT"
  renew_orchestration_lease
  local upid uuid config
  discard_partial_restore
  log "Restoring $SNAPSHOT as $RESOURCE_NAME on $INITIAL_NODE"
  upid="$(mox_ssh "$INITIAL_NODE" pvesh create "/nodes/${INITIAL_NODE}/qemu" --vmid "$VMID" \
    --archive "${BACKUP_STORAGE_ID}:backup/${SNAPSHOT}" --storage "$PROD_VM_STORAGE" --force 0 \
    --name "$RESOURCE_NAME" --cores "$VM_CORES" --memory "$VM_MEMORY_MB" --cpu "$PROD_VM_CPU_TYPE" \
    --net0 "virtio=${VM_MAC},bridge=${PROD_VM_BRIDGE},firewall=1" --tags "$VM_TAGS" --onboot 0 \
    --description "Production guest (${PURPOSE_SLUG})" \
    --output-format json </dev/null 2>"${RUN_DIR}/restore.err" | grep -o 'UPID:[^"[:space:]]*' | head -n 1)" || true
  [[ -n "$upid" ]] || die "PVE did not start the restore: $(tr '\n' ' ' <"${RUN_DIR}/restore.err")"
  pve_task_follow "$INITIAL_NODE" "$upid" "$RUN_DIR" "$POLL_SECONDS" ||
    die "The restore failed: ${PVE_TASK_EXITSTATUS:-unknown status}"
  # The cluster's resource list shows the restored VM's name a moment later.
  wait_for_created_vm_identity >/dev/null || die "The restored VM $VMID is missing"
  # The backed-up hook would refuse every start before HA manages the VM;
  # apply_final_network_policy attaches it last, as for a new VM.
  config="$(node_exec "$OWNER_NODE" qm config "$VMID")" || die "Could not read the restored configuration"
  if grep -q '^hookscript: ' <<<"$config"; then
    node_exec "$OWNER_NODE" qm set "$VMID" --delete hookscript >/dev/null || die "Could not detach the restored hook"
  fi
  if [[ "$NEW_IDENTITY" == true ]]; then
    uuid="$(python3 -c 'import uuid; print(uuid.uuid4())')"
    node_exec "$OWNER_NODE" qm set "$VMID" --smbios1 "uuid=${uuid}" >/dev/null || die "Could not give the VM a new SMBIOS UUID"
  fi
  verify_vm_config
  apply_disk_allocation
  ensure_registry_provisioning
  refresh_resource
}

upload_to_owner() {
  local source="$1" destination="$2" mode="$3" stage coordinator_destination
  stage="${REMOTE_RUN_DIR}/coordinator-$(basename -- "$destination")"
  _mox_ssh_options "$COORDINATOR"
  coordinator_destination="$(_mox_ssh_destination "$COORDINATOR")"
  scp "${MOX_SSH_OPTIONS[@]}" -- "$source" "${MOX_SSH_USER:-root}@${coordinator_destination}:${stage}"
  node_exec "$COORDINATOR" chmod "$mode" "$stage"
  if [[ "$OWNER_NODE" == "$COORDINATOR" ]]; then
    node_exec "$COORDINATOR" mv -- "$stage" "$destination"
  else
    node_exec "$COORDINATOR" scp -o BatchMode=yes -o ClearAllForwardings=yes \
      -o StrictHostKeyChecking=yes -o CheckHostIP=no -o "HostKeyAlias=${OWNER_NODE}" \
      -o "UserKnownHostsFile=/etc/pve/nodes/${OWNER_NODE}/ssh_known_hosts" -o GlobalKnownHostsFile=none \
      -p -- "$stage" "root@${OWNER_NODE}.${PROXMOX_INTERNAL_DOMAIN}:${destination}"
    node_exec "$COORDINATOR" rm -f -- "$stage"
  fi
  node_exec "$OWNER_NODE" chmod "$mode" "$destination"
}

remove_remote_patch_bundle() {
  [[ -n "$REMOTE_RUN_DIR" ]] || return 0
  [[ -z "$COORDINATOR" ]] || node_exec "$COORDINATOR" rm -rf -- "$REMOTE_RUN_DIR" >/dev/null 2>&1 || true
  [[ -z "$OWNER_NODE" || "$OWNER_NODE" == "$COORDINATOR" ]] ||
    node_exec "$OWNER_NODE" rm -rf -- "$REMOTE_RUN_DIR" >/dev/null 2>&1 || true
  REMOTE_RUN_DIR=""
}

# Give the restored root disk this slot's identity while the VM is stopped:
# its zvol, or with OVERLAY_PATH an instant restore's overlay.
patch_offline() {
  local overlay_path="${1:-}"
  CURRENT_PHASE="patching the restored root disk offline"
  log "Patching $RESOURCE_NAME's identity into the restored disk"
  locate_vm >/dev/null || die "The restored VM disappeared"
  [[ "$VM_LIVE_STATUS" == stopped ]] || die "The restored VM must be stopped while it is patched"
  REMOTE_RUN_DIR="/run/app-ha-recovery-${VMID}-$$"
  node_exec "$COORDINATOR" install -d -m 0700 "$REMOTE_RUN_DIR"
  [[ "$OWNER_NODE" == "$COORDINATOR" ]] || node_exec "$OWNER_NODE" install -d -m 0700 "$REMOTE_RUN_DIR"
  local hash_file="${RUN_DIR}/root-password.hash" keys_file="${RUN_DIR}/authorized_keys"
  printf '%s\n' "$ROOT_PASSWORD_HASH" >"$hash_file"
  printf '%s\n' "$ADMIN_1_PUBLIC_SSH_KEY" "$ADMIN_2_PUBLIC_SSH_KEY" | sed '/^$/d' >"$keys_file"
  chmod 0600 "$hash_file" "$keys_file"
  upload_to_owner "$PATCH_HELPER" "${REMOTE_RUN_DIR}/patch_staging_clone.sh" 0700
  upload_to_owner "$GUEST_TREE_HELPER" "${REMOTE_RUN_DIR}/patch_staging_guest_tree.py" 0700
  upload_to_owner "$hash_file" "${REMOTE_RUN_DIR}/root-password.hash" 0600
  upload_to_owner "$keys_file" "${REMOTE_RUN_DIR}/authorized_keys" 0600
  rm -f -- "$hash_file" "$keys_file"

  local path="" dataset="" volmode source
  local -a disk=(--qcow2 "$overlay_path" --volume-id "$INSTANT_OVERLAY")
  if [[ -z "$overlay_path" ]]; then
    path="$(node_exec "$OWNER_NODE" pvesm path "$ROOT_VOLUME")" || die "Could not resolve $ROOT_VOLUME"
    [[ "$path" =~ ^/dev/zvol/[A-Za-z0-9._/+:-]+$ ]] || die "Unexpected path for $ROOT_VOLUME: $path"
    dataset="${path#/dev/zvol/}"
    read -r volmode source < <(node_exec "$OWNER_NODE" zfs get -H -o value,source volmode "$dataset") ||
      die "Could not read the volmode of $dataset"
    # The patch needs the guest's partitions visible on the host.
    node_exec "$OWNER_NODE" bash -c 'zfs set volmode=full "$1" && udevadm settle' bash "$dataset" ||
      die "Could not expose the restored disk's partitions"
    disk=(--device "$path" --volume-id "$ROOT_VOLUME")
  fi
  local -a args=(
    "${REMOTE_RUN_DIR}/patch_staging_clone.sh" --mode production
    "${disk[@]}" --name "$RESOURCE_NAME" --owner-vmid "$VMID"
    --address "${PROD_PRIVATE_IP}/24" --gateway "$GUEST_GATEWAY" --mac "$VM_MAC"
    --dns "$(IFS=,; printf '%s' "${DNS_SERVERS[*]}")"
    --root-hash-file "${REMOTE_RUN_DIR}/root-password.hash"
    --authorized-keys-file "${REMOTE_RUN_DIR}/authorized_keys"
  )
  [[ "$NEW_IDENTITY" == false ]] || args+=(--new-identity)
  local status=0
  node_exec "$OWNER_NODE" bash "${args[@]}" || status=$?
  if [[ -z "$dataset" ]]; then
    :
  elif [[ "$source" == local ]]; then
    node_exec "$OWNER_NODE" zfs set "volmode=${volmode}" "$dataset" || warn "Could not set $dataset back to volmode=$volmode"
  else
    node_exec "$OWNER_NODE" zfs inherit volmode "$dataset" || warn "Could not return $dataset to its inherited volmode"
  fi
  remove_remote_patch_bundle
  ((status == 0)) || die "The offline patch failed; the VM was not started"
  info "Patched: hostname, network, root password, and SSH keys$([[ "$NEW_IDENTITY" == true ]] && printf ', new machine-id and SSH host keys' || true)"
}

# add_prod_vm.sh's guest verification, without its install-only checks
# (cloud-init's first run, startup.sh): the restored guest keeps its own.
verify_restored_guest() {
  CURRENT_PHASE="verifying the recovered guest"
  log "Starting and verifying $RESOURCE_NAME"
  locate_vm >/dev/null || die "The recovered VM disappeared"
  [[ "$VM_LIVE_STATUS" == running ]] || node_exec "$OWNER_NODE" qm start "$VMID"
  wait_for_qga || die "QEMU guest agent did not become ready within ${QGA_TIMEOUT_SECONDS}s"
  locate_vm >/dev/null

  local shadow_json expected
  expected="$(printf '%s' "$ROOT_PASSWORD_HASH" | sha256sum | awk '{print $1}')"
  shadow_json="$(guest_exec_json /usr/bin/python3 -c 'import hashlib
rows = [line.split(":", 2)[1] for line in open("/etc/shadow", encoding="utf-8") if line.startswith("root:")]
if len(rows) != 1:
    raise SystemExit("invalid root shadow rows")
print(hashlib.sha256(rows[0].encode("utf-8")).hexdigest())')" || die "QGA could not attest the root shadow entry"
  python3 - "$shadow_json" "$expected" <<'PY' || die "The recovered root password hash failed QGA attestation"
import json
import sys
value = json.loads(sys.argv[1])
if value.get("exited") != 1 or value.get("exitcode") != 0 or value.get("out-data", "").strip() != sys.argv[2]:
    raise SystemExit(1)
PY

  local host_key="" deadline=$((SECONDS + QGA_TIMEOUT_SECONDS)) hostname_json key_json
  while ((SECONDS < deadline)); do
    hostname_json="$(guest_exec_json /bin/hostname -s 2>/dev/null)" || { sleep 5; continue; }
    key_json="$(guest_exec_json /bin/cat /etc/ssh/ssh_host_ed25519_key.pub 2>/dev/null)" || { sleep 5; continue; }
    host_key="$(python3 - "$hostname_json" "$key_json" "$RESOURCE_NAME" 2>/dev/null <<'PY'
import json
import re
import sys
name = json.loads(sys.argv[1])
key = json.loads(sys.argv[2])
if name.get("exitcode") != 0 or name.get("out-data", "").strip() != sys.argv[3]:
    raise SystemExit(1)
text = key.get("out-data", "").strip()
if key.get("exitcode") != 0 or not re.fullmatch(r"ssh-ed25519 [A-Za-z0-9+/=]+(?: [^\r\n]+)?", text):
    raise SystemExit(1)
print(text)
PY
    )" || host_key=""
    [[ -z "$host_key" ]] || break
    sleep 5
  done
  [[ -n "$host_key" ]] || die "QGA did not show hostname $RESOURCE_NAME and an SSH host key in time"

  local known_hosts="${RUN_DIR}/guest-known-hosts" base="${PROXMOX_SSH_KNOWN_HOSTS_FILE:-${HOME}/.ssh/known_hosts}"
  if [[ -f "$base" && ! -L "$base" ]]; then install -m 0600 "$base" "$known_hosts"; else : >"$known_hosts"; fi
  printf '%s %s\n%s %s\n' "$PROD_PRIVATE_IP" "$host_key" "$RESOURCE_NAME" "$host_key" >>"$known_hosts"
  local original="${PROXMOX_SSH_KNOWN_HOSTS_FILE:-}"
  PROXMOX_SSH_KNOWN_HOSTS_FILE="$known_hosts"
  export PROXMOX_SSH_KNOWN_HOSTS_FILE
  deadline=$((SECONDS + QGA_TIMEOUT_SECONDS))
  until ssh_via_mox "$COORDINATOR" "root@${PROD_PRIVATE_IP}" true </dev/null >/dev/null 2>&1; do
    ((SECONDS < deadline)) || die "Key-only root SSH did not become ready through $COORDINATOR"
    sleep 10
  done
  ssh_via_mox "$COORDINATOR" "root@${PROD_PRIVATE_IP}" bash -s -- "$RESOURCE_NAME" "$PROD_PRIVATE_IP" \
    "$GUEST_GATEWAY" "$(IFS=,; printf '%s' "${DNS_SERVERS[*]}")" <<'GUEST' || die "The recovered guest failed verification"
set -Eeuo pipefail
[[ "$(id -u)" == 0 && "$(hostname -s)" == "$1" ]]
systemctl is-active --quiet qemu-guest-agent.service
systemctl is-active --quiet ssh.service
ip -4 -o address show dev lan0 | awk -v address="$2/24" '$4 == address { found=1 } END { exit !found }'
ip -4 route show default | awk -v gateway="$3" '$3 == gateway { found=1 } END { exit !found }'
dns_status="$(resolvectl dns lan0)"
IFS=',' read -r -a expected_dns <<<"$4"
for server in "${expected_dns[@]}"; do grep -Fq -- "$server" <<<"$dns_status"; done
sshd_policy="$(sshd -T)"
grep -Eq '^permitrootlogin (without-password|prohibit-password)$' <<<"$sshd_policy"
grep -Fxq 'passwordauthentication no' <<<"$sshd_policy"
grep -Fxq 'kbdinteractiveauthentication no' <<<"$sshd_policy"
GUEST
  if [[ -n "$original" ]]; then
    PROXMOX_SSH_KNOWN_HOSTS_FILE="$original"
    export PROXMOX_SSH_KNOWN_HOSTS_FILE
  else
    unset PROXMOX_SSH_KNOWN_HOSTS_FILE
  fi
  set_install_phase guest-verified
  info "Verified: root password, hostname $RESOURCE_NAME, ${PROD_PRIVATE_IP} on lan0, key-only root SSH"
}

# The qm create options an instant restore's VM takes from the restore
# point's config, as a restore would keep them, NUL-separated. Its EFI disk
# is new, on production storage; the restore point's EFI variables are
# written into it next. A new identity gets a new SMBIOS UUID, as in
# restore_vm.
instant_create_args() {
  python3 - "${RUN_DIR}/backup-config.json" "$PROD_VM_STORAGE" "$NEW_IDENTITY" <<'PY'
import json
import sys
import uuid

config = json.load(open(sys.argv[1], encoding="utf-8"))
args = []
for key in ("machine", "bios", "sockets", "balloon", "ostype", "scsihw", "agent", "vga", "tablet", "numa", "smbios1"):
    if key in config:
        args += [f"--{key}", config[key]]
if sys.argv[3] == "true":
    args = [value for index, value in enumerate(args) if "--smbios1" not in args[max(index - 1, 0):index + 1]]
    args += ["--smbios1", f"uuid={uuid.uuid4()}"]
options = [part for part in config["efidisk0"].split(",")[1:] if not part.startswith("size=")]
args += ["--efidisk0", ",".join([f"{sys.argv[2]}:1", *options])]
for value in args:
    sys.stdout.buffer.write(value.encode() + b"\0")
PY
}

# Instant restore, before the first boot: the VM with its restored EFI
# variables, its empty zvol (the registry's root volume), and as scsi0 an
# overlay over the mapped restore point with this slot's identity patched in.
instant_prepare_vm() {
  CURRENT_PHASE="preparing the instant restore"
  renew_orchestration_lease
  { node_exec "$INITIAL_NODE" test -x "$BACKUP_LINK_HELPER" &&
    node_exec "$INITIAL_NODE" test -x "$INSTANT_SWITCH_HELPER"; } ||
    die "$INITIAL_NODE lacks the instant-restore helpers; run update_cluster_runtime.sh first"
  discard_partial_restore
  ensure_overlay_storage
  local -a create_args=()
  mapfile -d '' -t create_args < <(instant_create_args)
  ((${#create_args[@]} >= 2)) || die "Could not derive the VM's settings from $SNAPSHOT"
  CURRENT_PHASE="creating the instant-restore VM"
  log "Creating $RESOURCE_NAME (VMID $VMID) on $INITIAL_NODE for an instant restore of $SNAPSHOT"
  node_exec "$INITIAL_NODE" qm create "$VMID" --name "$RESOURCE_NAME" \
    --description "Production guest (${PURPOSE_SLUG})" --cores "$VM_CORES" --memory "$VM_MEMORY_MB" \
    --cpu "$PROD_VM_CPU_TYPE" --net0 "virtio=${VM_MAC},bridge=${PROD_VM_BRIDGE},firewall=1" \
    --tags "$VM_TAGS" --onboot 0 "${create_args[@]}" >/dev/null ||
    die "Could not create VM $VMID"
  # The cluster's resource list shows a new VM's name a moment later.
  wait_for_created_vm_identity >/dev/null || die "The new VM $VMID is missing"

  CURRENT_PHASE="mapping $SNAPSHOT and restoring its EFI variables"
  log "Mapping $SNAPSHOT read-only from PBS on $OWNER_NODE"
  node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" map "$VMID" "$SNAPSHOT" --storage "$BACKUP_STORAGE_ID" >/dev/null ||
    die "Could not map $SNAPSHOT from PBS"
  local efi_volume efi_path overlay_path
  efi_volume="$(node_exec "$OWNER_NODE" qm config "$VMID" | awk -F': ' '$1 == "efidisk0" {print $2}' | cut -d, -f1)"
  efi_path="$(node_exec "$OWNER_NODE" pvesm path "$efi_volume")" || die "Could not resolve the EFI disk $efi_volume"
  node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" restore "$VMID" drive-efidisk0.img.fidx "$efi_path" ||
    die "Could not restore the EFI variables of $SNAPSHOT"

  CURRENT_PHASE="creating the root zvol and patching the overlay"
  ROOT_VOLUME="${PROD_VM_STORAGE}:vm-${VMID}-disk-1"
  [[ -z "$REGISTERED_ROOT_VOLUME" || "$REGISTERED_ROOT_VOLUME" == "$ROOT_VOLUME" ]] ||
    die "The registry names $REGISTERED_ROOT_VOLUME as the root disk, not $ROOT_VOLUME"
  node_exec "$OWNER_NODE" pvesm alloc "$PROD_VM_STORAGE" "$VMID" "vm-${VMID}-disk-1" \
    "$((BACKUP_DISK_BYTES / 1024))" >/dev/null || die "Could not create $ROOT_VOLUME"
  apply_disk_allocation
  ensure_registry_provisioning
  refresh_resource

  overlay_path="$(node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" create-overlay "$VMID" "$INSTANT_OVERLAY")" ||
    die "Could not create the overlay $INSTANT_OVERLAY"
  patch_offline "$overlay_path"
  node_exec "$OWNER_NODE" qm set "$VMID" --scsi0 "${INSTANT_OVERLAY},discard=on,iothread=1,replicate=1,ssd=1" \
    --boot order=scsi0 >/dev/null || die "Could not attach the overlay"
  INSTANT_PENDING=true
  ROOT_OVERLAY_VOLUME="$INSTANT_OVERLAY"
  verify_vm_config
}

# On a resumed recovery: whether scsi0 is still an instant restore's overlay.
instant_detect() {
  local scsi0
  scsi0="$(node_exec "$OWNER_NODE" qm config "$VMID" | awk -F': ' '$1 == "scsi0" {print $2}' | cut -d, -f1)"
  [[ "$scsi0" == "$INSTANT_OVERLAY" ]] || return 0
  INSTANT=true
  INSTANT_PENDING=true
  ROOT_OVERLAY_VOLUME="$INSTANT_OVERLAY"
  info "Resuming the instant restore: $RESOURCE_NAME still runs on its overlay"
}

# Until the switch the VM reads its restore point through the mapping. After
# a reboot of its host or a mapping process that died, the restore point is
# mapped again (backup_link.sh only ever maps the one the overlay was made
# over), and the VM, whose reads failed, is stopped to boot again.
instant_ensure_mapping() {
  local state
  state="$(node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" status "$VMID")" ||
    die "Could not read the PBS mapping of $RESOURCE_NAME"
  [[ "$state" != alive ]] || return 0
  warn "The PBS mapping of $RESOURCE_NAME is $state; mapping its restore point again"
  locate_vm >/dev/null || die "VM $VMID disappeared"
  if [[ "$VM_LIVE_STATUS" != stopped ]]; then
    node_exec "$OWNER_NODE" qm stop "$VMID" --timeout 60 >/dev/null || die "Could not stop $RESOURCE_NAME"
  fi
  node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" map "$VMID" "$SNAPSHOT" --storage "$BACKUP_STORAGE_ID" >/dev/null ||
    die "Could not map $SNAPSHOT again; it must be the restore point the recovery started from"
}

instant_switch_state() {
  node_exec "$OWNER_NODE" python3 "$INSTANT_SWITCH_HELPER" state "$VMID" "$INSTANT_OVERLAY" "$ROOT_VOLUME"
}

# With the guest verified and serving: copy the restore point's in-use data
# into the zvol, switch the running disk onto it, then drop the overlay.
instant_copy_and_switch() {
  CURRENT_PHASE="copying $SNAPSHOT into ${RESOURCE_NAME}'s own disk"
  renew_orchestration_lease
  instant_ensure_mapping
  locate_vm >/dev/null || die "VM $VMID disappeared"
  if [[ "$VM_LIVE_STATUS" != running ]]; then
    node_exec "$OWNER_NODE" qm start "$VMID" || die "Could not start $RESOURCE_NAME"
    wait_for_qga || die "QEMU guest agent did not become ready within ${QGA_TIMEOUT_SECONDS}s"
  fi
  if [[ "$INSTANT_ROUTED" == false ]]; then
    info "Routing ${PRIMARY_DOMAIN}${ALIAS_DOMAINS[*]:+ and ${ALIAS_DOMAINS[*]}} to $RESOURCE_NAME while its disk is copied"
    mox_ssh "$COORDINATOR" "$REMOTE_TUNNEL_SYNC" --all </dev/null ||
      warn "Could not update the Cloudflare Tunnel config yet; it is updated again after the switch"
    INSTANT_ROUTED=true
  fi
  local state path started
  state="$(instant_switch_state)" || die "Could not read the disk state of $RESOURCE_NAME"
  if [[ "$state" == backup ]]; then
    path="$(node_exec "$OWNER_NODE" pvesm path "$ROOT_VOLUME")" || die "Could not resolve $ROOT_VOLUME"
    [[ "$path" =~ ^/dev/zvol/[A-Za-z0-9._/+:-]+$ ]] || die "Unexpected path for $ROOT_VOLUME: $path"
    if ((COPY_DELAY_SECONDS > 0)); then
      instant_copy_delay "$COPY_DELAY_SECONDS" "$RESOURCE_NAME"
      [[ "$(node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" status "$VMID")" == alive ]] ||
        die "The PBS mapping of $RESOURCE_NAME died during the wait; rerun with purpose $PURPOSE_SLUG to map it again"
    fi
    renew_orchestration_lease
    log "Copying the in-use data of $SNAPSHOT into $ROOT_VOLUME while $RESOURCE_NAME runs"
    # --skip-zero leaves zero chunks unwritten, so the zvol must read as
    # zeros first; it is not in use until the switch.
    # -f is needed once the zvol holds a partition table; its warning is noise.
    node_exec "$OWNER_NODE" blkdiscard -f "$path" 2>"${RUN_DIR}/blkdiscard.err" ||
      die "Could not free $ROOT_VOLUME before the copy: $(tr '\n' ' ' <"${RUN_DIR}/blkdiscard.err")"
    started=$SECONDS
    node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" restore "$VMID" drive-scsi0.img.fidx "$path" --skip-zero ||
      die "The copy of $SNAPSHOT failed; $RESOURCE_NAME keeps running on its overlay. Rerun with purpose $PURPOSE_SLUG to copy again"
    info "Copied in $((SECONDS - started))s"
    renew_orchestration_lease
  fi
  log "Switching ${RESOURCE_NAME}'s running disk to $ROOT_VOLUME"
  node_exec "$OWNER_NODE" python3 "$INSTANT_SWITCH_HELPER" switch "$VMID" "$INSTANT_OVERLAY" "$ROOT_VOLUME" >/dev/null ||
    die "The switch did not complete; $RESOURCE_NAME keeps running. Rerun with purpose $PURPOSE_SLUG to finish it"
  INSTANT_PENDING=false
  ROOT_OVERLAY_VOLUME=""
  instant_remove_overlay
  info "$RESOURCE_NAME now runs on $ROOT_VOLUME alone"
}

# Once scsi0 is the zvol: the mapping, its record, and the overlay go. Also
# run on resume, after a switch whose cleanup was interrupted.
instant_remove_overlay() {
  local path
  node_exec "$OWNER_NODE" test -x "$BACKUP_LINK_HELPER" || return 0
  { node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" unmap "$VMID" >/dev/null &&
    node_exec "$OWNER_NODE" "$BACKUP_LINK_HELPER" forget "$VMID" >/dev/null; } ||
    die "Could not remove the PBS mapping of $RESOURCE_NAME"
  path="$(node_exec "$OWNER_NODE" pvesm path "$INSTANT_OVERLAY" 2>/dev/null)" || return 0
  if node_exec "$OWNER_NODE" test -e "$path"; then
    node_exec "$OWNER_NODE" pvesm free "$INSTANT_OVERLAY" >/dev/null || die "Could not remove the overlay $INSTANT_OVERLAY"
  fi
}

main() {
  parse_args "$@"
  require_command_local awk
  require_command_local openssl
  require_openssl_sha512_crypt
  require_command_local sha256sum
  load_and_validate_config
  preflight_workstation
  install_traps
  select_coordinator_and_nodes
  backup_storage_check "$COORDINATOR" || die "PBS backups are not available"

  choose_snapshot
  read_backup_config
  reserve_or_resume_recovery
  confirm_recovery

  acquire_orchestration_lease
  preflight_placement
  INSTANT_OVERLAY="${OVERLAY_STORAGE_ID}:${VMID}/vm-${VMID}-disk-1.qcow2"
  if [[ "$INSTALL_PHASE" == unstarted || "$INSTALL_PHASE" == unknown ]]; then
    if [[ "$INSTANT" == true ]]; then
      instant_prepare_vm
    else
      restore_vm
      patch_offline
    fi
    set_install_phase installed-confirmed
  else
    locate_vm >/dev/null || die "VM $VMID of the recovery being resumed is missing"
    instant_detect
    verify_vm_config
  fi
  if [[ "$INSTALL_PHASE" == installed-confirmed ]]; then
    [[ "$INSTANT_PENDING" == false ]] || instant_ensure_mapping
    verify_restored_guest
  fi
  if [[ "$INSTANT_PENDING" == true ]]; then
    instant_copy_and_switch
    verify_vm_config
  elif [[ "$INSTALL_PHASE" == guest-verified ]]; then
    instant_remove_overlay
  fi
  [[ "$INSTALL_PHASE" != guest-verified ]] || apply_final_network_policy
  ensure_replication_jobs
  ensure_ha_resource
  register_ingress_and_finalize

  log "$RESOURCE_NAME is recovered from ${BACKUP_TIME}'s restore point of $BACKUP_NAME"
  info "VM $VMID on $OWNER_NODE: ${PROD_PRIVATE_IP}, ${PRIMARY_DOMAIN}, ${VM_CORES} vCPUs, ${VM_MEMORY_GIB} GiB RAM"
  info "HA placement ${PLACEMENT_NODES[*]}; replication every $REPLICATION_MINUTES minutes"
  info "It has no backup schedule yet"
  info "If ${PRIMARY_DOMAIN}'s Cloudflare DNS does not point at this cluster's tunnel yet, add its records (README.md, \"Instructions for Cloudflare Setup\")"
  # As add_prod_vm.sh does: offer 'ssh prodN' from this workstation, pinned to
  # the QGA-attested host key. A recovery into another slot, or on a rebuilt
  # cluster, has host keys that an existing alias does not know.
  setup_workstation_jump_ssh
  monitoring_membership_sync "$COORDINATOR"
  bmac_ui_result vm "$RESOURCE_NAME" vmid:int "$VMID" snapshot "$SNAPSHOT" source "$BACKUP_NAME" \
    new_identity:bool "$NEW_IDENTITY" instant:bool "$INSTANT" fqdn "$PRIMARY_DOMAIN" node "$OWNER_NODE"
  bmac_ui_next_step "Schedule backups for $RESOURCE_NAME." --workflow manage_backup_schedule --arg "resource=$RESOURCE_NAME"
}

if [[ "${FULL_RECOVERY_SOURCE_ONLY:-0}" != 1 ]]; then
  bmac_ui_bootstrap "$@"
  main "$@"
fi
