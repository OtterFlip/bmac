#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Connect the cluster to its Proxmox Backup Server: add the PVE storage
# bmac-pbs from the PROXMOX_PBS_* settings, with client-side encryption and
# the administrator's master public key, and save a copy of the cluster's
# encryption key for offline storage. See docs/MAIN_DESIGN.md, "Backup and
# restore (PBS)", and docs/PBS_MANUAL_SETUP.md for the server side.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../.." && pwd -P)"
# shellcheck source=../../lib/ui_protocol.sh
source "${REPO_ROOT}/scripts/lib/ui_protocol.sh"
# shellcheck source=../../lib/config.sh
source "${REPO_ROOT}/scripts/lib/config.sh"
bmac_ui_bootstrap "$@"

STORAGE_ID=bmac-pbs
REMOTE_PBS_API=/usr/local/lib/app-ha-proxmox/lib/pbs_api.py
REMOTE_KEY="/etc/pve/priv/storage/${STORAGE_ID}.enc"
REMOTE_MASTER="/etc/pve/priv/storage/${STORAGE_ID}.master.pem"

MASTER_PUBKEY=""
KEY_FILE=""
EXPORT_DIR=""
PAPER_KEY=""
CHECK_ONLY=false
ASSUME_YES=false
COORDINATOR=""
RUN_DIR=""
# new: add the storage; existing: it is already set up, only export the key.
MODE=new
KEY_SOURCE=""
KEY_FINGERPRINT=""

log() {
  printf '\n==> %s\n' "$*"
  bmac_ui_step "$*"
}

info() {
  printf '    %s\n' "$*"
}

warn() {
  printf 'WARNING: %s\n' "$*" >&2
  bmac_ui_warning "$*"
}

die() {
  printf '\nERROR: %s\n' "$*" >&2
  bmac_ui_error failed "$*"
  exit 1
}

usage() {
  cat <<'EOF'
Usage: scripts/user_callable/backups/setup_pbs.sh [--master-pubkey PATH]
         [--key-file PATH] [--export-dir DIR] [--paper-key | --no-paper-key]
         [--check] [--yes] [--json]

Connect this cluster to its Proxmox Backup Server. Adds the PVE storage
bmac-pbs (PROXMOX_PBS_* in cluster.conf, PROXMOX_PBS_TOKEN in secrets.env)
with client-side encryption and your master public key, then saves a copy of
the cluster's encryption key on this workstation for you to store offline.

Before changing anything it checks, from a cluster host, that the PBS
certificate matches PROXMOX_PBS_FINGERPRINT, that the token holds
Datastore.Backup and Datastore.Audit on the namespace (and warns about any
privilege that could delete backups), and whether the namespace already
holds backups. A namespace with backups needs their existing key: a new key
could not read them.

  --master-pubkey PATH  The master public key (PEM). Default
                        pbs-master-public.pem in the configuration directory;
                        asked for when that file does not exist.
  --key-file PATH       Use this existing encryption key (the cluster's saved
                        .enc file, or one recovered with the master key)
                        instead of generating one.
  --export-dir DIR      Where to save the key copy (asked for when omitted).
  --paper-key, --no-paper-key
                        Also save a printable copy of the key (asked for when
                        omitted).
  --check               Run the checks and change nothing.
  --yes                 Add the storage without asking.

When bmac-pbs already exists with these settings, only the key copy is made.
EOF
}

parse_args() {
  while (($#)); do
    case "$1" in
      --check) CHECK_ONLY=true ;;
      --yes) ASSUME_YES=true ;;
      --paper-key) PAPER_KEY=true ;;
      --no-paper-key) PAPER_KEY=false ;;
      --master-pubkey | --key-file | --export-dir)
        (($# >= 2)) || die "$1 needs a value"
        case "$1" in
          --master-pubkey) MASTER_PUBKEY="$2" ;;
          --key-file) KEY_FILE="$2" ;;
          --export-dir) EXPORT_DIR="$2" ;;
        esac
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
}

on_coordinator() {
  mox_ssh "$COORDINATOR" "$@"
}

json_field() {
  python3 -c '
import json, sys
value = json.load(open(sys.argv[1]))
for part in sys.argv[2].split("."):
    value = value.get(part) if isinstance(value, dict) else None
print("" if value is None else value)' "$1" "$2"
}

# True when two PEM files hold the same public key: the base64 bodies between
# their BEGIN and END lines decode to the same bytes.
same_public_key() {
  python3 - "$1" "$2" <<'PY'
import base64
import re
import sys

def der(path):
    text = open(path, encoding="ascii", errors="replace").read()
    match = re.search(r"-----BEGIN PUBLIC KEY-----(.*?)-----END PUBLIC KEY-----", text, re.S)
    if not match:
        raise SystemExit(1)
    return base64.b64decode("".join(match.group(1).split()), validate=True)

sys.exit(0 if der(sys.argv[1]) == der(sys.argv[2]) else 1)
PY
}

# Use --master-pubkey, else pbs-master-public.pem in the configuration
# directory, else ask for the file.
choose_master_pubkey() {
  local default="${PROXMOX_CONFIG_DIR}/pbs-master-public.pem"
  [[ -z "$MASTER_PUBKEY" ]] || return 0
  if [[ -e "$default" ]]; then
    MASTER_PUBKEY="$default"
    return 0
  fi
  printf '\n    No master public key was given, and %s does not exist.\n' "$default"
  if bmac_ui_is_json; then
    bmac_ui_input MASTER_PUBKEY --id master_pubkey --type file --required \
      --label "Master public key (PEM)" \
      --help "The public half of your master key pair, such as master-public.pem. Never the private key."
  elif [[ "$ASSUME_YES" == true ]]; then
    die "Give the master public key with --master-pubkey, or save it as ${default}"
  else
    read -r -p "Path of the master public key (PEM): " MASTER_PUBKEY
  fi
  [[ -n "$MASTER_PUBKEY" ]] || die "A master public key is required"
}

# Size, line count, and first line of a public key file, for error messages.
describe_public_file() {
  python3 - "$1" <<'PY'
import sys

data = open(sys.argv[1], "rb").read()
lines = data.split(b"\n")
first = lines[0][:40].decode("ascii", "replace")
print(f"{len(data)} bytes, {len(lines) - 1} lines, starting {first!r}")
PY
}

# The master key must be an RSA public key; refuse anything holding a private
# key, which must never reach a cluster.
check_master_pubkey() {
  local path="$1" text
  [[ -f "$path" && ! -L "$path" ]] || die "Master public key not found: $path"
  if grep -q 'PRIVATE KEY' "$path"; then
    die "$path contains a PRIVATE key. Only the master PUBLIC key may go to a cluster; keep the private key offline."
  fi
  grep -q -- '-----BEGIN PUBLIC KEY-----' "$path" ||
    die "$path is not a PEM public key (expected -----BEGIN PUBLIC KEY-----)"
  require_command openssl || die "openssl is required on this workstation to check the master public key"
  text="$(openssl pkey -pubin -in "$path" -noout -text 2>/dev/null)" ||
    die "$path is not a readable public key"
  [[ "$text" == *Modulus:* ]] || die "$path is not an RSA public key"
  MASTER_FINGERPRINT="$(openssl pkey -pubin -in "$path" -outform DER 2>/dev/null | sha256sum | awk '{print $1}')"
}

# An existing key must be a PBS key file without a passphrase: PVE cannot
# use a passphrase-protected key for unattended backups.
check_key_file() {
  local path="$1"
  [[ -f "$path" && ! -L "$path" ]] || die "Encryption key file not found: $path"
  python3 - "$path" <<'PY' || die "$path is not a usable PBS encryption key"
import json
import sys

try:
    key = json.load(open(sys.argv[1], encoding="utf-8"))
except (OSError, ValueError) as error:
    raise SystemExit(f"not JSON: {error}")
if not isinstance(key, dict) or not key.get("data"):
    raise SystemExit("not a PBS key file (no data field)")
if key.get("kdf") not in (None, "none", "None"):
    raise SystemExit("the key is passphrase-protected; remove the passphrase with "
                     "'proxmox-backup-client key change-passphrase FILE --kdf none' first")
PY
}

# The fingerprint of a local key file, computed by the host's
# proxmox-backup-client from a 0600 copy that is shredded right after.
remote_key_fingerprint() {
  # shellcheck disable=SC2016 # Evaluated on the host.
  on_coordinator bash -c '
set -Eeuo pipefail
copy="$(mktemp /run/bmac-pbs-key.XXXXXX)"
trap "shred -u \"\$copy\" 2>/dev/null || rm -f \"\$copy\"" EXIT
cat >"$copy"
proxmox-backup-client key show "$copy" --output-format json
' <"$1" >"${RUN_DIR}/key-show.json" ||
    die "The host could not read the encryption key $1"
  json_field "${RUN_DIR}/key-show.json" fingerprint
}

# Compare an existing bmac-pbs storage with cluster.conf and look for other
# PBS storages on the same namespace.
check_existing_storage() {
  on_coordinator pvesh get /storage --output-format json </dev/null >"${RUN_DIR}/storages.json" ||
    die "Could not list the cluster's storages through $COORDINATOR"
  local verdict
  verdict="$(python3 - "${RUN_DIR}/storages.json" "$STORAGE_ID" "$PROXMOX_PBS_SERVER_HOST" \
    "$PROXMOX_PBS_SERVER_PORT" "$PROXMOX_PBS_DATASTORE" "$PROXMOX_PBS_NAMESPACE" \
    "$PROXMOX_PBS_USERNAME" "$PROXMOX_PBS_FINGERPRINT" <<'PY'
import json
import sys

rows = json.load(open(sys.argv[1]))
storage_id, server, port, datastore, namespace, username, fingerprint = sys.argv[2:]
wanted = {"server": server, "port": port, "datastore": datastore, "namespace": namespace,
          "username": username, "fingerprint": fingerprint.lower()}
for row in rows:
    if row.get("type") != "pbs":
        continue
    actual = {"server": row.get("server", ""), "port": str(row.get("port") or 8007),
              "datastore": row.get("datastore", ""), "namespace": row.get("namespace", ""),
              "username": row.get("username", ""), "fingerprint": str(row.get("fingerprint", "")).lower()}
    if row.get("storage") == storage_id:
        differences = [f"{k} is {actual[k]!r}, cluster.conf says {wanted[k]!r}"
                       for k in wanted if actual[k] != wanted[k]]
        if differences:
            print("differs\t" + "; ".join(differences))
        else:
            print("same\t")
    elif (actual["server"], actual["datastore"], actual["namespace"]) == (server, datastore, namespace):
        print(f"other\t{row.get('storage')}")
PY
  )" || die "Could not read the cluster's storage list"
  local kind detail
  while IFS=$'\t' read -r kind detail; do
    case "$kind" in
      same)
        MODE=existing
        on_coordinator test -f "$REMOTE_KEY" </dev/null ||
          die "Storage ${STORAGE_ID} already exists without an encryption key. Remove it in the PVE GUI (Datacenter > Storage) and run Setup PBS again."
        ;;
      differs)
        die "Storage ${STORAGE_ID} already exists with different settings: ${detail}. Correct cluster.conf, or remove the storage in the PVE GUI (Datacenter > Storage) after checking no backup job uses it."
        ;;
      other)
        warn "PVE storage '${detail}' also writes to namespace ${PROXMOX_PBS_NAMESPACE} on this server. Backups of one VM through both storages would share a backup group with two different keys; remove '${detail}' (Datacenter > Storage) once nothing uses it."
        ;;
    esac
  done <<<"$verdict"
}

# Ask PBS, from the coordinator, about the token, the policy, and the
# namespace's snapshots. Leaves the report in RUN_DIR/inspect.json.
inspect_pbs() {
  local status=0
  on_coordinator test -x "$REMOTE_PBS_API" </dev/null ||
    die "${COORDINATOR} lacks ${REMOTE_PBS_API}; run update_cluster_runtime.sh first"
  printf '%s\n' "$PROXMOX_PBS_TOKEN" |
    on_coordinator python3 -I "$REMOTE_PBS_API" inspect \
      --server "$PROXMOX_PBS_SERVER_HOST" --port "$PROXMOX_PBS_SERVER_PORT" \
      --datastore "$PROXMOX_PBS_DATASTORE" --namespace "$PROXMOX_PBS_NAMESPACE" \
      --username "$PROXMOX_PBS_USERNAME" --fingerprint "$PROXMOX_PBS_FINGERPRINT" \
      --token-stdin >"${RUN_DIR}/inspect.json" 2>"${RUN_DIR}/inspect.err" || status=$?
  case "$status" in
    0) ;;
    3) die "The PBS certificate does not match PROXMOX_PBS_FINGERPRINT, so the token was not sent. $(cat "${RUN_DIR}/inspect.err") Check the fingerprint on the PBS server itself (proxmox-backup-manager cert info) and update cluster.conf." ;;
    4) die "${COORDINATOR} cannot reach PBS at ${PROXMOX_PBS_SERVER_HOST}:${PROXMOX_PBS_SERVER_PORT}: $(cat "${RUN_DIR}/inspect.err") Check the PBS firewall admits this cluster's hosts." ;;
    5) die "PBS rejected the token: $(cat "${RUN_DIR}/inspect.err") Check PROXMOX_PBS_USERNAME and PROXMOX_PBS_TOKEN." ;;
    *) die "Could not inspect PBS from ${COORDINATOR}: $(cat "${RUN_DIR}/inspect.err")" ;;
  esac
}

# Stop when the token lacks a required privilege; warn about extra ones.
check_privileges() {
  local line kind text
  local -a dangerous=() other=()
  while IFS=$'\t' read -r kind text; do
    case "$kind" in
      missing) die "The token lacks ${text}. Grant DatastoreBackup and DatastoreAudit on /datastore/${PROXMOX_PBS_DATASTORE}/${PROXMOX_PBS_NAMESPACE} to both ${PROXMOX_PBS_USERNAME} and its user (docs/PBS_MANUAL_SETUP.md, section 6)." ;;
      dangerous) dangerous+=("$text") ;;
      other) other+=("$text") ;;
    esac
  done < <(python3 -c '
import json, sys
privileges = json.load(open(sys.argv[1]))["privileges"]
if privileges["missing"]:
    print("missing\t" + " and ".join(privileges["missing"]) + " on " + privileges["path"])
for row in privileges["excess"]:
    print(("dangerous" if row["dangerous"] else "other") + "\t" + row["privilege"] + " on " + row["path"])' \
    "${RUN_DIR}/inspect.json")
  info "Token privileges: Datastore.Backup and Datastore.Audit on /datastore/${PROXMOX_PBS_DATASTORE}/${PROXMOX_PBS_NAMESPACE}"
  for line in "${other[@]}"; do
    warn "The token also holds ${line}; it needs only DatastoreBackup and DatastoreAudit on its namespace."
  done
  for line in "${dangerous[@]}"; do
    warn "The token holds ${line}, which lets anyone with this cluster's token delete its backups. Remove it on the PBS server."
  done
  DANGEROUS_PRIVILEGES=("${dangerous[@]}")
}

describe_pbs() {
  python3 - "${RUN_DIR}/inspect.json" <<'PY'
import json
import sys

report = json.load(open(sys.argv[1]))
print(f"    PBS {report.get('version') or '(version unknown)'} at {report['server']}, "
      f"datastore {report['datastore']}, namespace {report['namespace']}")
prune = report["prune"]
if not prune["readable"]:
    print("    Retention: not readable with this token")
elif not prune["jobs"]:
    print("    Retention: NO prune job for this namespace; PBS keeps every backup until one is added")
for job in prune["jobs"]:
    keeps = ", ".join(f"{key} {value}" for key, value in sorted(job.items()) if key.startswith("keep-"))
    print(f"    Retention ({job['id']}, runs {job.get('schedule', '?')}): {keeps or 'keep everything'}")
snapshots = report["snapshots"]
if not snapshots["readable"]:
    print(f"    Existing backups: could not list ({snapshots.get('error')})")
elif snapshots["count"]:
    print(f"    Existing backups: {snapshots['count']} snapshots in {len(snapshots['groups'])} groups")
    for fingerprint in snapshots["key_fingerprints"]:
        print(f"      encrypted with key {fingerprint}")
    if snapshots["unencrypted"]:
        print(f"      {snapshots['unencrypted']} unencrypted")
else:
    print("    Existing backups: none")
PY
}

# Decide between generating a key and using an existing one.
choose_key() {
  local count readable
  readable="$(json_field "${RUN_DIR}/inspect.json" snapshots.readable)"
  [[ "$readable" == True ]] ||
    die "Could not list the backups in namespace ${PROXMOX_PBS_NAMESPACE}, so it is unknown whether a key already exists; nothing was changed"
  count="$(json_field "${RUN_DIR}/inspect.json" snapshots.count)"
  if ((count > 0)) && [[ -z "$KEY_FILE" ]]; then
    printf '\n    The namespace already holds backups. A new key could not read them, so\n'
    printf '    Setup needs their key: the .enc file saved when this cluster was set up,\n'
    printf '    or one recovered with the master private key (docs/MAIN_DESIGN.md,\n'
    printf '    "Setup PBS and the encryption key").\n'
    if bmac_ui_is_json; then
      bmac_ui_input KEY_FILE --id key_file --type file --required \
        --label "Existing encryption key (.enc)" \
        --help "A PBS key file without a passphrase, which encrypted the backups listed above."
    elif [[ "$ASSUME_YES" == true ]]; then
      die "The namespace holds backups; give their key with --key-file"
    else
      read -r -p "Path of the existing encryption key: " KEY_FILE
    fi
  elif ((count == 0)) && [[ -z "$KEY_FILE" && "$ASSUME_YES" != true ]]; then
    local answer=generate
    if bmac_ui_is_json; then
      bmac_ui_choose answer "Encryption key" generate \
        generate "Generate a new key for this cluster (recommended)" \
        existing "Use an existing key file"
    else
      read -r -p "The namespace is empty. Generate a new encryption key? [Y/n] " answer
      [[ ! "$answer" =~ ^[Nn] ]] && answer=generate || answer=existing
    fi
    if [[ "$answer" == existing ]]; then
      if bmac_ui_is_json; then
        bmac_ui_input KEY_FILE --id key_file --type file --required \
          --label "Existing encryption key (.enc)"
      else
        read -r -p "Path of the existing encryption key: " KEY_FILE
      fi
    fi
  fi

  if [[ -z "$KEY_FILE" ]]; then
    KEY_SOURCE=generate
    info "Encryption key: a new one, generated on ${COORDINATOR}"
    return 0
  fi
  KEY_SOURCE=existing
  check_key_file "$KEY_FILE"
  KEY_FINGERPRINT="$(remote_key_fingerprint "$KEY_FILE")"
  [[ -n "$KEY_FINGERPRINT" ]] || die "Could not read the fingerprint of $KEY_FILE"
  info "Encryption key: $KEY_FILE (fingerprint $KEY_FINGERPRINT)"
  if ((count > 0)); then
    python3 -c '
import json, sys
used = json.load(open(sys.argv[1]))["snapshots"]["key_fingerprints"]
sys.exit(0 if sys.argv[2].lower() in [f.lower() for f in used] else 1)' \
      "${RUN_DIR}/inspect.json" "$KEY_FINGERPRINT" ||
      die "$KEY_FILE (fingerprint $KEY_FINGERPRINT) did not encrypt any backup in namespace ${PROXMOX_PBS_NAMESPACE}; nothing was changed"
    local others
    others="$(python3 -c '
import json, sys
used = json.load(open(sys.argv[1]))["snapshots"]["key_fingerprints"]
print(" ".join(f for f in used if f.lower() != sys.argv[2].lower()))' "${RUN_DIR}/inspect.json" "$KEY_FINGERPRINT")"
    [[ -z "$others" ]] ||
      warn "Some backups in the namespace were encrypted with other keys (${others}); this cluster cannot restore those."
  fi
}

confirm_add() {
  local key_line
  if [[ "$KEY_SOURCE" == generate ]]; then
    key_line="Generate a new encryption key (${REMOTE_KEY})"
  else
    key_line="Use the existing key ${KEY_FINGERPRINT}"
  fi
  log "Plan"
  info "Add PVE storage ${STORAGE_ID}: ${PROXMOX_PBS_USERNAME} at ${PROXMOX_PBS_SERVER_HOST}:${PROXMOX_PBS_SERVER_PORT}, datastore ${PROXMOX_PBS_DATASTORE}, namespace ${PROXMOX_PBS_NAMESPACE}"
  info "$key_line"
  info "Master public key: ${MASTER_PUBKEY} (SHA-256 ${MASTER_FINGERPRINT})"
  info "PVE never prunes (prune-backups keep-all=1); the PBS prune job does"
  bmac_ui_plan_begin "Connect the cluster to PBS"
  bmac_ui_plan_item create "$STORAGE_ID" "PVE storage for namespace ${PROXMOX_PBS_NAMESPACE} on ${PROXMOX_PBS_SERVER_HOST}"
  bmac_ui_plan_item create "encryption key" "$key_line"
  bmac_ui_plan_item create "master public key" "${MASTER_PUBKEY} (SHA-256 ${MASTER_FINGERPRINT:0:16}...)"
  bmac_ui_plan_end
  [[ "$ASSUME_YES" != true ]] || return 0
  local -a details=()
  ((${#DANGEROUS_PRIVILEGES[@]} == 0)) ||
    details+=(--detail "The token can delete backups (${DANGEROUS_PRIVILEGES[*]}); fix that on PBS soon.")
  if bmac_ui_is_json; then
    bmac_ui_confirm --id add --title "Add storage ${STORAGE_ID}?" \
      --message "$key_line, and connect namespace ${PROXMOX_PBS_NAMESPACE}." \
      --confirm-label "Add storage" "${details[@]}" || die "Cancelled; nothing was changed"
  else
    local answer
    read -r -p "Add the storage? [y/N] " answer
    [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]] || die "Cancelled; nothing was changed"
  fi
}

# The token, the master public key, and an existing key travel on stdin, one
# line each (the keys base64-encoded). pvesm reads both keys from files, so
# the host writes them to a root-only directory under /run that is shredded
# when pvesm returns. pvesm takes the token as an argument, so root on the
# coordinator can see it in the process list while it runs.
add_storage() {
  log "Adding storage ${STORAGE_ID}"
  # shellcheck disable=SC2016 # Evaluated on the host.
  {
    printf '%s\n' "$PROXMOX_PBS_TOKEN"
    base64 <"$MASTER_PUBKEY" | tr -d '\n'
    printf '\n'
    if [[ "$KEY_SOURCE" == existing ]]; then
      base64 <"$KEY_FILE" | tr -d '\n'
      printf '\n'
    fi
  } |
    on_coordinator bash -c '
set -Eeuo pipefail
dir="$(mktemp -d /run/bmac-pbs-setup.XXXXXX)"
trap "shred -u \"\$dir\"/* 2>/dev/null || true; rm -rf \"\$dir\"" EXIT
IFS= read -r token
IFS= read -r master
key=""
IFS= read -r key || true
printf "%s" "$master" | base64 -d >"$dir/master-public.pem"
encryption=autogen
if [[ -n "$key" ]]; then
  printf "%s" "$key" | base64 -d >"$dir/encryption-key.json"
  encryption="$dir/encryption-key.json"
fi
pvesm add pbs "$1" --server "$2" --port "$3" --datastore "$4" --namespace "$5" \
  --username "$6" --password "$token" --fingerprint "$7" \
  --encryption-key "$encryption" --master-pubkey "$dir/master-public.pem" \
  --content backup --prune-backups keep-all=1
' bash "$STORAGE_ID" "$PROXMOX_PBS_SERVER_HOST" "$PROXMOX_PBS_SERVER_PORT" \
      "$PROXMOX_PBS_DATASTORE" "$PROXMOX_PBS_NAMESPACE" "$PROXMOX_PBS_USERNAME" \
      "$PROXMOX_PBS_FINGERPRINT" ||
    die "pvesm could not add storage ${STORAGE_ID}"
  info "Added"
}

# Check the new storage is active and holds the intended keys. A storage that
# fails is removed again: nothing has been backed up through it yet.
verify_storage() {
  local problem="" active fingerprint
  log "Verifying storage ${STORAGE_ID}"
  active="$(on_coordinator pvesm status --storage "$STORAGE_ID" </dev/null 2>/dev/null |
    awk -v id="$STORAGE_ID" '$1 == id {print $3}')" || true
  [[ "$active" == active ]] || problem="the storage is not active on ${COORDINATOR} (pvesm status says '${active:-nothing}')"
  if [[ -z "$problem" ]]; then
    on_coordinator proxmox-backup-client key show "$REMOTE_KEY" --output-format json </dev/null \
      >"${RUN_DIR}/stored-key.json" 2>/dev/null || problem="the stored key ${REMOTE_KEY} is unreadable"
  fi
  if [[ -z "$problem" ]]; then
    fingerprint="$(json_field "${RUN_DIR}/stored-key.json" fingerprint)"
    [[ "$KEY_SOURCE" != existing || "${fingerprint,,}" == "${KEY_FINGERPRINT,,}" ]] ||
      problem="the stored key has fingerprint ${fingerprint}, not ${KEY_FINGERPRINT}"
  fi
  if [[ -z "$problem" ]]; then
    # PVE may store the PEM with different line breaks or a trailing newline,
    # so compare the keys, not the files.
    if ! on_coordinator cat "$REMOTE_MASTER" </dev/null >"${RUN_DIR}/stored-master.pem" 2>/dev/null; then
      problem="the stored master public key ${REMOTE_MASTER} is unreadable"
    elif ! same_public_key "${RUN_DIR}/stored-master.pem" "$MASTER_PUBKEY"; then
      problem="the stored master public key ${REMOTE_MASTER} is not the key in ${MASTER_PUBKEY} (the stored file: $(describe_public_file "${RUN_DIR}/stored-master.pem"); yours: $(describe_public_file "$MASTER_PUBKEY"))"
    fi
  fi
  if [[ -n "$problem" ]]; then
    on_coordinator pvesm remove "$STORAGE_ID" </dev/null >/dev/null 2>&1 || true
    die "Storage ${STORAGE_ID} was removed again because ${problem}"
  fi
  KEY_FINGERPRINT="$fingerprint"
  info "Active on ${COORDINATOR}; key fingerprint ${KEY_FINGERPRINT}; master public key stored"
}

# Copy the key (and optionally a printable paper key) to the workstation and
# wait until the operator says it is stored offline.
export_key() {
  local fingerprint name target paper=""
  if [[ -z "$KEY_FINGERPRINT" ]]; then
    on_coordinator proxmox-backup-client key show "$REMOTE_KEY" --output-format json </dev/null \
      >"${RUN_DIR}/stored-key.json" || die "Could not read ${REMOTE_KEY} on ${COORDINATOR}"
    KEY_FINGERPRINT="$(json_field "${RUN_DIR}/stored-key.json" fingerprint)"
  fi
  fingerprint="${KEY_FINGERPRINT//:/}"
  name="$(printf '%s-%s' "${PROXMOX_CLUSTER_NAME:-cluster}" "$PROXMOX_PBS_NAMESPACE" |
    tr -c 'A-Za-z0-9._-' '-')-pbs-key-${fingerprint:0:8}.enc"
  log "Saving a copy of the encryption key"
  if [[ -z "$EXPORT_DIR" ]]; then
    if bmac_ui_is_json; then
      bmac_ui_input EXPORT_DIR --id export_dir --type directory --required \
        --label "Save the key copy in" --default "$HOME" \
        --help "The file ${name} is written here, readable only by you. Move it offline afterwards."
    elif [[ "$ASSUME_YES" == true ]]; then
      EXPORT_DIR="$HOME"
    else
      read -r -p "Directory to save ${name} in [$HOME]: " EXPORT_DIR
      EXPORT_DIR="${EXPORT_DIR:-$HOME}"
    fi
  fi
  [[ -d "$EXPORT_DIR" ]] || die "Not a directory: $EXPORT_DIR"
  target="${EXPORT_DIR%/}/${name}"
  [[ ! -e "$target" ]] || die "$target already exists; move it away or choose another directory"
  on_coordinator cat "$REMOTE_KEY" </dev/null >"$target" ||
    { rm -f "$target"; die "Could not copy ${REMOTE_KEY} from ${COORDINATOR}"; }
  chmod 0600 "$target"
  [[ "$(sha256sum <"$target" | awk '{print $1}')" == \
    "$(on_coordinator sha256sum "$REMOTE_KEY" </dev/null | awk '{print $1}')" ]] ||
    { rm -f "$target"; die "The copied key differs from ${REMOTE_KEY}; it was deleted"; }
  info "Saved $target"

  if [[ -z "$PAPER_KEY" ]]; then
    if bmac_ui_is_json; then
      bmac_ui_yes_no "Also save a printable paper key?" false \
        "A text file to print and keep with your offline records." && PAPER_KEY=true || PAPER_KEY=false
    elif [[ "$ASSUME_YES" == true ]]; then
      PAPER_KEY=false
    else
      local answer
      read -r -p "Also save a printable paper key? [y/N] " answer
      [[ "$answer" =~ ^[Yy] ]] && PAPER_KEY=true || PAPER_KEY=false
    fi
  fi
  if [[ "$PAPER_KEY" == true ]]; then
    paper="${target%.enc}.paperkey.txt"
    [[ ! -e "$paper" ]] || die "$paper already exists"
    on_coordinator proxmox-backup-client key paperkey "$REMOTE_KEY" --output-format text </dev/null \
      >"$paper" || { rm -f "$paper"; die "Could not make the paper key"; }
    chmod 0600 "$paper"
    info "Saved $paper"
  fi

  local -a lines=(
    "Store ${target} offline now (for example in a password manager and on removable media). Without it, or the master private key, this cluster's backups cannot be restored."
    "Then delete the workstation copy. The working key stays on the cluster at ${REMOTE_KEY}; never delete that one."
  )
  [[ -z "$paper" ]] || lines+=("Print ${paper}, keep it with your offline records, and delete the file.")
  if bmac_ui_is_json; then
    local -a arguments=(--id key_stored --title "Store the encryption key offline")
    arguments+=(--instruction "${lines[0]}" --copy "$target" --instruction "${lines[1]}")
    [[ -z "$paper" ]] || arguments+=(--instruction "${lines[2]}" --copy "$paper")
    bmac_ui_manual_action "${arguments[@]}" --ack-label "The key is stored offline"
  else
    printf '\n'
    printf '    %s\n' "${lines[@]}"
    if [[ "$ASSUME_YES" != true ]]; then
      local answer=""
      until [[ "$answer" == STORED ]]; do
        read -r -p "Type STORED once the key is stored offline: " answer
      done
    fi
  fi
  EXPORTED_KEY="$target"
  EXPORTED_PAPER="$paper"
}

main() {
  parse_args "$@"
  load_proxmox_config --require-secrets >/dev/null || die "cluster configuration is invalid"
  config_require_pbs_token || die "PBS backups are not configured"
  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/setup-pbs.XXXXXX")"
  chmod 0700 "$RUN_DIR"
  trap 'rm -rf -- "$RUN_DIR"' EXIT
  DANGEROUS_PRIVILEGES=()
  EXPORTED_KEY=""
  EXPORTED_PAPER=""

  choose_master_pubkey
  log "Checking the master public key"
  check_master_pubkey "$MASTER_PUBKEY"
  info "${MASTER_PUBKEY}: RSA public key, SHA-256 ${MASTER_FINGERPRINT}"

  COORDINATOR="$(first_reachable_mox)" || die "No reachable Proxmox host was found"
  log "Checking the cluster's storages through ${COORDINATOR}"
  check_existing_storage
  [[ "$MODE" == new ]] || info "Storage ${STORAGE_ID} already exists with these settings"

  log "Checking PBS from ${COORDINATOR}"
  inspect_pbs
  info "Certificate matches PROXMOX_PBS_FINGERPRINT"
  check_privileges
  describe_pbs

  if [[ "$MODE" == existing ]]; then
    if [[ "$CHECK_ONLY" == true ]]; then
      bmac_ui_result storage "$STORAGE_ID" configured:bool true
      return 0
    fi
    export_key
    bmac_ui_result storage "$STORAGE_ID" configured:bool true created:bool false \
      key_fingerprint "$KEY_FINGERPRINT" key_copy "$EXPORTED_KEY" paper_key "$EXPORTED_PAPER"
    return 0
  fi

  if [[ "$CHECK_ONLY" == true ]]; then
    log "Checks passed; nothing was changed (--check)"
    bmac_ui_result storage "$STORAGE_ID" configured:bool false
    return 0
  fi
  choose_key
  confirm_add
  add_storage
  verify_storage
  export_key
  log "PBS is connected"
  info "Next: choose which production VMs to back up, and how often, on the Backups page"
  bmac_ui_result storage "$STORAGE_ID" configured:bool true created:bool true \
    key_source "$KEY_SOURCE" key_fingerprint "$KEY_FINGERPRINT" \
    master_pubkey_sha256 "$MASTER_FINGERPRINT" key_copy "$EXPORTED_KEY" paper_key "$EXPORTED_PAPER"
}

if [[ "${SETUP_PBS_SOURCE_ONLY:-0}" != 1 ]]; then
  main "$@"
fi
