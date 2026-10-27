#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Back a production VM up to PBS now, with the same settings as its backup
# job. The backup runs as a PVE task on the host that runs the VM, so it
# continues if this workstation disconnects; the script follows its log.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
# shellcheck source=../../lib/ui_protocol.sh
source "${REPO_ROOT}/scripts/lib/ui_protocol.sh"
# shellcheck source=../../lib/config.sh
source "${REPO_ROOT}/scripts/lib/config.sh"
# shellcheck source=../../lib/backups.sh
source "${REPO_ROOT}/scripts/lib/backups.sh"
bmac_ui_bootstrap "$@"

VM=""
COMMENT=""
COMMENT_GIVEN=false
ASSUME_YES=false
COORDINATOR=""
RUN_DIR=""
POLL_SECONDS="${BACKUP_NOW_POLL_SECONDS:-5}"

log() {
  printf '\n==> %s\n' "$*"
  bmac_ui_step "$*"
}

info() {
  printf '    %s\n' "$*"
}

die() {
  printf '\nERROR: %s\n' "$*" >&2
  bmac_ui_error failed "$*"
  exit 1
}

usage() {
  cat <<'EOF'
Usage: scripts/user_callable/backups/backup_now.sh [prodN] [--vm prodN]
         [--comment TEXT] [--yes] [--json]

Back a production VM up to the cluster's PBS storage (bmac-pbs) now, in
snapshot mode with the guest agent's filesystem freeze, fleecing on local
storage, and BMAC's notes, the same as its scheduled backups. The VM keeps
running. The backup is a PVE task on the VM's host, so it finishes even if
this workstation disconnects; the script shows its progress.

A manual backup is one more restore point and counts toward the PBS prune
job's keep-last.

  --comment TEXT  A note shown with the restore point (asked for when
                  omitted): one line, at most 200 characters, without { } \ |.
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --yes) ASSUME_YES=true ;;
      --vm | --comment)
        (($# >= 2)) || die "$1 needs a value"
        case "$1" in
          --vm) VM="$2" ;;
          --comment) COMMENT="$2"; COMMENT_GIVEN=true ;;
        esac
        shift
        ;;
      -h | --help)
        usage
        exit 0
        ;;
      prod[1-9]*) VM="$1" ;;
      *)
        usage >&2
        exit 2
        ;;
    esac
    shift
  done
}

choose_vm() {
  local -a rows=() options=()
  local row name purpose fqdn
  mapfile -t rows < <(backup_production_names "$COORDINATOR")
  ((${#rows[@]})) || die "No production VMs are registered"
  for row in "${rows[@]}"; do
    IFS=$'\x1f' read -r name purpose fqdn <<<"$row"
    options+=("$name" "${name}${purpose:+ · $purpose}${fqdn:+ · $fqdn}")
  done
  if bmac_ui_is_json; then
    bmac_ui_choose VM "Production VM" "${options[0]}" "${options[@]}"
  elif [[ "$ASSUME_YES" == true ]]; then
    die "Name the production VM (for example prod1)"
  else
    printf '    Production VMs: %s\n' "$(printf '%s\n' "${rows[@]}" | cut -d $'\x1f' -f1 | tr '\n' ' ')"
    read -r -p "Production VM: " VM
  fi
}

# The host and run state of VMID, tab-separated.
locate_vm() {
  mox_ssh "$COORDINATOR" pvesh get /cluster/resources --type vm --output-format json </dev/null |
    python3 -c '
import json, sys
for row in json.load(sys.stdin):
    if str(row.get("vmid")) == sys.argv[1]:
        print("\t".join([str(row.get("node") or ""), str(row.get("status") or "")]))
        break' "$1"
}

main() {
  parse_args "$@"
  load_proxmox_config --no-secrets >/dev/null || die "cluster configuration is invalid"
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/backup-now.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  trap 'rm -rf -- "$RUN_DIR"' EXIT
  COORDINATOR="$(first_reachable_mox)" || die "No reachable Proxmox host was found"
  backup_storage_check "$COORDINATOR" || die "PBS backups are not available"

  [[ -n "$VM" ]] || choose_vm
  backup_production_record "$COORDINATOR" "$VM" "${RUN_DIR}/record.json" || die "Cannot back up $VM"
  local vmid purpose fqdn node state
  vmid="$(backup_record_field "${RUN_DIR}/record.json" vmid)"
  purpose="$(backup_record_field "${RUN_DIR}/record.json" purpose.slug)"
  fqdn="$(backup_record_field "${RUN_DIR}/record.json" domains.primary)"
  IFS=$'\t' read -r node state < <(locate_vm "$vmid") || true
  [[ -n "${node:-}" ]] || die "VMID $vmid ($VM) is not on any cluster host"
  [[ "$node" =~ ^mox([1-9]|10)$ ]] || die "$VM is on an unexpected node: $node"

  log "Back up $VM now"
  info "$VM (VMID $vmid) is ${state:-in an unknown state} on $node; the backup runs there"
  if [[ "$COMMENT_GIVEN" == false && "$ASSUME_YES" != true ]]; then
    if bmac_ui_is_json; then
      bmac_ui_input COMMENT --id comment --label "Comment (optional)" \
        --placeholder "For example: before upgrading the database" \
        --help "Shown with this restore point. One line, at most 200 characters, without { } \\ or |."
      until backup_valid_comment "$COMMENT"; do
        bmac_ui_reask COMMENT "One line of at most 200 characters, without { } \\ or |."
      done
    else
      read -r -p "Comment for this backup (optional): " COMMENT
    fi
  fi
  backup_valid_comment "$COMMENT" ||
    die "The comment must be one line of at most 200 characters, without { } \\ or |"
  [[ -z "$COMMENT" ]] || info "Comment: $COMMENT"
  if [[ "$ASSUME_YES" != true ]]; then
    if bmac_ui_is_json; then
      bmac_ui_confirm --id backup --title "Back up $VM now?" \
        --message "The VM keeps running. The new restore point counts toward the PBS prune job's keep-last." \
        --confirm-label "Back up now" || die "Cancelled; nothing was changed"
    else
      local answer
      read -r -p "Back up $VM now? [y/N] " answer
      [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]] || die "Cancelled; nothing was changed"
    fi
  fi

  local started upid
  started="$(date +%s)"
  upid="$(backup_start_task "$node" "$vmid" "$VM" "$purpose" "$fqdn" "$RUN_DIR" "$COMMENT")" ||
    die "PVE did not start the backup: $(tr '\n' ' ' <"${RUN_DIR}/start.err")"
  info "Task $upid"

  log "Backing up $VM"
  pve_task_follow "$node" "$upid" "$RUN_DIR" "$POLL_SECONDS" ||
    die "The backup of $VM failed: ${PVE_TASK_EXITSTATUS:-unknown status}"

  local volid
  volid="$(backup_newest_volid "$COORDINATOR" "$vmid" "$started")" || volid=""
  log "Backup of $VM finished"
  info "Restore point: ${volid:-(not listed yet)}"
  bmac_ui_result vm "$VM" vmid:int "$vmid" node "$node" task "$upid" volid "$volid"
}

if [[ "${BACKUP_NOW_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
