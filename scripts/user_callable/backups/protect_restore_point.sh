#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Protect a BMAC restore point on PBS, so the PBS prune job never removes it,
# or clear that protection again. The cluster's token may change protection
# only on its own backups, and can never delete them.

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

SNAPSHOT=""
WANT=""
ASSUME_YES=false
COORDINATOR=""
RUN_DIR=""

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
Usage: scripts/user_callable/backups/protect_restore_point.sh
         [--snapshot vm/VMID/TIME] [--protect | --unprotect] [--yes] [--json]

Protect a BMAC restore point so the PBS server's prune job keeps it however
old it gets, for example before a risky change, or clear the protection so
the normal retention policy applies to it again. Protected restore points use
PBS space until their protection is cleared.

  --snapshot vm/VMID/TIME  The restore point, such as
                           vm/100/2026-10-27T14:50:01Z (chosen from a list
                           when omitted).
  --protect, --unprotect   What to do (default: the opposite of now).
  --yes                    Apply without asking.
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --yes) ASSUME_YES=true ;;
      --protect) WANT=1 ;;
      --unprotect) WANT=0 ;;
      --snapshot)
        (($# >= 2)) || die "--snapshot needs a value"
        SNAPSHOT="$2"
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
    shift
  done
  [[ -z "$SNAPSHOT" || "$SNAPSHOT" =~ ^vm/[1-9][0-9]{0,8}/[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$ ]] ||
    die "--snapshot must look like vm/100/2026-10-27T14:50:01Z"
}

# BMAC's restore points, newest first, as tab-separated snapshot, protected
# (0 or 1), VM name, and time in this workstation's time zone.
list_points() {
  mox_ssh "$COORDINATOR" pvesh get "/nodes/${COORDINATOR}/storage/${BACKUP_STORAGE_ID}/content" \
    --output-format json </dev/null | python3 -c '
import json, sys, time
prefix = sys.argv[1] + ":backup/"
rows = [r for r in json.load(sys.stdin)
        if str(r.get("volid", "")).startswith(prefix) and str(r.get("notes") or "").startswith("bmac ")]
for row in sorted(rows, key=lambda r: r.get("ctime") or 0, reverse=True):
    first, _, comment = str(row["notes"]).splitlines()[0].partition(" | ")
    words = first.split()
    comment = comment.strip()
    name = words[1] if len(words) > 1 else "?"
    if comment:
        name += " · " + (comment if len(comment) <= 48 else comment[:47] + "…")
    print("\t".join([row["volid"][len(prefix):], "1" if row.get("protected") else "0", name,
                     time.strftime("%Y-%m-%d %H:%M %Z", time.localtime(int(row.get("ctime") or 0)))]))' \
    "$BACKUP_STORAGE_ID"
}

main() {
  parse_args "$@"
  load_proxmox_config --no-secrets >/dev/null || die "cluster configuration is invalid"
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/protect-restore-point.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  trap 'rm -rf -- "$RUN_DIR"' EXIT
  COORDINATOR="$(first_reachable_mox)" || die "No reachable Proxmox host was found"
  backup_storage_check "$COORDINATOR" || die "PBS backups are not available"

  local -a points=()
  local row snapshot protected name="" when=""
  mapfile -t points < <(list_points)
  ((${#points[@]})) || die "There are no BMAC restore points"
  if [[ -z "$SNAPSHOT" ]]; then
    local -a options=()
    for row in "${points[@]}"; do
      IFS=$'\t' read -r snapshot protected name when <<<"$row"
      options+=("$snapshot" "${when} · ${name}$([[ "$protected" == 1 ]] && printf ' · protected' || true)")
    done
    if bmac_ui_is_json; then
      bmac_ui_choose SNAPSHOT "Restore point" "${options[0]}" "${options[@]}"
    elif [[ "$ASSUME_YES" == true ]]; then
      die "Name the restore point with --snapshot"
    else
      printf '    %s\n' "${points[@]}"
      read -r -p "Restore point (vm/VMID/TIME): " SNAPSHOT
    fi
  fi

  local found="" current=""
  for row in "${points[@]}"; do
    IFS=$'\t' read -r snapshot protected name when <<<"$row"
    if [[ "$snapshot" == "$SNAPSHOT" ]]; then
      found=1
      current="$protected"
      break
    fi
  done
  [[ -n "$found" ]] || die "$SNAPSHOT is not a BMAC restore point on ${BACKUP_STORAGE_ID}"
  [[ -n "$WANT" ]] || WANT=$((1 - current))
  local volid="${BACKUP_STORAGE_ID}:backup/${SNAPSHOT}" action
  [[ "$WANT" == 1 ]] && action="Protect" || action="Clear the protection of"

  log "Restore point ${SNAPSHOT} (${name}, ${when})"
  info "Now $([[ "$current" == 1 ]] && printf protected || printf 'not protected')"
  if [[ "$WANT" == "$current" ]]; then
    info "Nothing to change"
    bmac_ui_result snapshot "$SNAPSHOT" protected:bool "$([[ "$WANT" == 1 ]] && printf true || printf false)" changed:bool false
    return 0
  fi
  local message="PBS's prune job will keep it however old it gets, until its protection is cleared."
  [[ "$WANT" == 1 ]] || message="The PBS prune job may then remove it at its next run, if the retention policy no longer keeps it."
  if [[ "$ASSUME_YES" != true ]]; then
    if bmac_ui_is_json; then
      bmac_ui_confirm --id apply --title "${action} ${name}'s restore point from ${when}?" \
        --message "$message" --confirm-label "$([[ "$WANT" == 1 ]] && printf Protect || printf 'Clear protection')" ||
        die "Cancelled; nothing was changed"
    else
      local answer
      printf '    %s\n' "$message"
      read -r -p "${action} it? [y/N] " answer
      [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]] || die "Cancelled; nothing was changed"
    fi
  fi

  mox_ssh "$COORDINATOR" pvesh set "/nodes/${COORDINATOR}/storage/${BACKUP_STORAGE_ID}/content/${volid}" \
    --protected "$WANT" </dev/null >/dev/null || die "PBS did not accept the change"
  local now=""
  for row in $(list_points | cut -f1,2 | tr '\t' '='); do
    [[ "${row%%=*}" != "$SNAPSHOT" ]] || now="${row#*=}"
  done
  [[ "$now" == "$WANT" ]] || die "The restore point's protection did not change"
  info "$([[ "$WANT" == 1 ]] && printf Protected || printf 'Protection cleared')"
  bmac_ui_result snapshot "$SNAPSHOT" protected:bool "$([[ "$WANT" == 1 ]] && printf true || printf false)" changed:bool true
}

if [[ "${PROTECT_RESTORE_POINT_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
