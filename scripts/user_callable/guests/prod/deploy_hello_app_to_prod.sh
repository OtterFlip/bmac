#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Deploy BMAC's example app to one registry-backed production VM: a small
# todo list (hello_todo/, Rust) that serves HTTPS itself with the domain's
# Cloudflare Origin certificate and keeps its data in a SQLite file. It is
# built here into one static binary and installed as hello-todo.service, so
# the VM needs no web server or database service. Its JSON API lives under
# /api/v1/, the path BMAC's Cloudflare setup never challenges.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/../../../.." && pwd -P)"
# shellcheck source=../../../lib/ui_protocol.sh
source "${REPO_ROOT}/scripts/lib/ui_protocol.sh"
bmac_ui_bootstrap "$@"
CONFIG_LIB="${REPO_ROOT}/scripts/lib/config.sh"
APP_DIR="${SCRIPT_DIR}/hello_todo"
BUILD_TARGET=x86_64-unknown-linux-musl
REMOTE_INSTALL_ROOT="/usr/local/lib/app-ha-proxmox"
REMOTE_REGISTRY="${REMOTE_INSTALL_ROOT}/lib/cluster_registry.py"

RUN_DIR=""
COORDINATOR=""
RESOURCE_NAME=""
RESOURCE_JSON=""
PRIMARY_DOMAIN=""
RESOURCE_STATE=""
OWNER_NODE=""
REMOTE_STAGE_DIR=""
CERT_PATH=""
KEY_PATH=""
APP_BINARY=""
MAX_STAGE_SUFFIX=0

declare -a ALIAS_DOMAINS=()
declare -a PLACEMENT_NODES=()
declare -a HOSTNAMES=()
declare -a CERT_EXTRA_NAMES=()

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
  printf 'ERROR: %s\n' "$*" >&2
  bmac_ui_error failed "$*"
  exit 1
}

prompt_yes() {
  local prompt="$1" answer
  if bmac_ui_is_json; then
    bmac_ui_ask "$1"
    return
  fi
  read -r -p "$prompt [y/N] " answer
  [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]]
}

prompt_path() {
  local variable="$1" prompt="$2" value
  if bmac_ui_is_json; then
    bmac_ui_input value --id "${variable,,}" --type file --label "$prompt" --required
    printf -v "$variable" '%s' "$value"
    return 0
  fi
  while :; do
    read -r -e -p "$prompt: " value
    [[ -n "$value" ]] || {
      warn "A path is required."
      continue
    }
    printf -v "$variable" '%s' "$value"
    return 0
  done
}

cleanup() {
  local rc=$?
  set +e
  if [[ -n "$REMOTE_STAGE_DIR" && -n "$RESOURCE_NAME" ]]; then
    ssh -o BatchMode=yes "$RESOURCE_NAME" \
      "rm -rf -- '$REMOTE_STAGE_DIR'" </dev/null >/dev/null 2>&1 || true
  fi
  [[ -z "$RUN_DIR" ]] || rm -rf -- "$RUN_DIR"
  return "$rc"
}
trap cleanup EXIT

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "Required command is not installed: $1"
}

require_regular_file() {
  local path="$1" label="$2"
  [[ -f "$path" && ! -L "$path" ]] ||
    die "$label must be a regular, non-symlink file: $path"
}

validate_positive_integer() {
  local label="$1" value="$2"
  [[ "$value" =~ ^[1-9][0-9]*$ ]] ||
    die "$label must be a positive integer; got: $value"
}

registry_cmd() {
  mox_ssh "$COORDINATOR" "$REMOTE_REGISTRY" \
    --state-dir "$CLUSTER_STATE_DIR" "$@" </dev/null
}

load_config() {
  require_regular_file "$CONFIG_LIB" "Configuration library"
  # shellcheck source=../../../lib/config.sh
  source "$CONFIG_LIB"
  load_proxmox_config --no-secrets >/dev/null ||
    die "Could not load config/cluster.conf"
  # shellcheck source=../../../lib/monitoring.sh
  source "${REPO_ROOT}/scripts/lib/monitoring.sh"

  [[ -n "${CLUSTER_STATE_DIR:-}" ]] || die "CLUSTER_STATE_DIR is not configured"
  [[ -n "${MAX_MOX_HOSTS:-}" ]] || die "MAX_MOX_HOSTS is not configured"
}

select_production_resource() {
  local resources_file="$RUN_DIR/production-resources.json"
  registry_cmd list --kind production >"$resources_file" ||
    die "Could not list production resources from the cluster registry"

  log "Registered production VMs"
  python3 - "$resources_file" <<'PY'
import json
import sys

rows = json.load(open(sys.argv[1], encoding="utf-8"))
if not rows:
    print("    (none)")
    raise SystemExit(0)

for row in sorted(rows, key=lambda r: r["name"]):
    domain = row["domains"]["primary"]
    owner = row.get("owner_node") or "-"
    state = row.get("state") or "-"
    print(f"    {row['name']:<10} {domain:<40} state={state:<10} owner={owner}")
PY

  local -a options=()
  if bmac_ui_is_json; then
    mapfile -t options < <(python3 - "$resources_file" <<'PY'
import json
import sys

for row in sorted(json.load(open(sys.argv[1], encoding="utf-8")), key=lambda r: r["name"]):
    print(row["name"])
    print(f"{row['name']} · {row['domains']['primary']} · {row.get('state') or '-'} · owner {row.get('owner_node') or '-'}")
PY
    )
    ((${#options[@]} > 0)) || die "No production VMs are registered"
  fi
  while :; do
    if bmac_ui_is_json; then
      bmac_ui_choose RESOURCE_NAME "Production VM to deploy to" "" "${options[@]}"
    else
      read -r -p "Production VM to deploy to (for example prod1): " RESOURCE_NAME
    fi
    [[ "$RESOURCE_NAME" =~ ^prod[1-9][0-9]*$ ]] || {
      warn "Enter a production resource name such as prod1."
      continue
    }

    if registry_cmd get "$RESOURCE_NAME" >"$RUN_DIR/resource.json" 2>/dev/null; then
      break
    fi
    warn "No registered production resource named $RESOURCE_NAME was found."
  done

  RESOURCE_JSON="$RUN_DIR/resource.json"

  mapfile -t fields < <(
    python3 - "$RESOURCE_JSON" <<'PY'
import json
import sys

row = json.load(open(sys.argv[1], encoding="utf-8"))
print(row.get("kind", ""))
print(row.get("name", ""))
print(row.get("state", ""))
print(row["domains"]["primary"])
print(",".join(row["domains"].get("aliases", [])))
print(",".join(row.get("placement", [])))
print(row.get("owner_node") or "")
PY
  )

  ((${#fields[@]} == 7)) || die "Unexpected registry response for $RESOURCE_NAME"
  [[ "${fields[0]}" == "production" ]] || die "$RESOURCE_NAME is not a production resource"
  [[ "${fields[1]}" == "$RESOURCE_NAME" ]] || die "Registry resource-name mismatch"

  RESOURCE_STATE="${fields[2]}"
  PRIMARY_DOMAIN="${fields[3]}"
  OWNER_NODE="${fields[6]}"

  IFS=',' read -r -a ALIAS_DOMAINS <<<"${fields[4]}"
  [[ -n "${fields[4]}" ]] || ALIAS_DOMAINS=()
  IFS=',' read -r -a PLACEMENT_NODES <<<"${fields[5]}"

  [[ "$RESOURCE_STATE" == "active" ]] ||
    die "$RESOURCE_NAME is in registry state '$RESOURCE_STATE'; expected active"
  [[ -n "$PRIMARY_DOMAIN" ]] || die "$RESOURCE_NAME has no registered primary domain"
  ((${#PLACEMENT_NODES[@]} >= 2)) ||
    die "$RESOURCE_NAME has fewer than two registered placement nodes"

  info "Selected: $RESOURCE_NAME"
  info "Primary FQDN: $PRIMARY_DOMAIN"
  info "Aliases: ${ALIAS_DOMAINS[*]:-none}"
  info "Current owner: ${OWNER_NODE:-unknown}"
  info "Placement: ${PLACEMENT_NODES[*]}"
}

preflight_target_tools() {
  local missing
  log "Verify the commands the install uses on $RESOURCE_NAME"
  missing="$(
    ssh -o BatchMode=yes "$RESOURCE_NAME" bash -s -- \
      systemctl install mv cp mktemp curl openssl ss journalctl <<'REMOTE'
set -Eeuo pipefail
for command_name in "$@"; do
  command -v "$command_name" >/dev/null 2>&1 || printf '%s ' "$command_name"
done
REMOTE
  )" || die "Could not check the commands available on $RESOURCE_NAME"
  [[ -z "$missing" ]] ||
    die "$RESOURCE_NAME lacks: ${missing% } (install them with apt, then rerun)"
  info "Every command the install uses is available."
}

show_rerun_behavior() {
  cat <<'EOF2'

This deployment script is safe to rerun, including to upgrade the app. Each
run rebuilds the app, revalidates the certificate/key, and reinstalls the
same files; the todo data is kept. A failed remote deployment restores the
previous app files and service before exiting, and temporary files are
removed.
EOF2
}

confirm_guest_ssh() {
  log "Verify direct workstation SSH to the production guest"
  printf '\n'
  printf 'Before proceeding, open another terminal if desired and verify that this works:\n\n'
  printf '    ssh %s\n\n' "$RESOURCE_NAME"
  prompt_yes "Can you successfully ssh to $RESOURCE_NAME using exactly 'ssh $RESOURCE_NAME'?" ||
    die "Guest SSH was not confirmed"

  info "Testing non-interactive SSH now..."
  ssh -o BatchMode=yes -o ConnectTimeout=8 "$RESOURCE_NAME" true </dev/null ||
    die "The script could not connect with 'ssh $RESOURCE_NAME' in BatchMode"
}

compute_stage_limit() {
  log "Determine all possible default staging hostnames"
  # Staging numbers are bounded by the production VM's staging slots, so the
  # names stay valid wherever its staging clones run.
  validate_positive_integer "MAX_STAGING_VMS_PER_PROD" "${MAX_STAGING_VMS_PER_PROD:-}"
  MAX_STAGE_SUFFIX="$MAX_STAGING_VMS_PER_PROD"
  info "Staging clones can answer as stage1-stage$MAX_STAGE_SUFFIX (MAX_STAGING_VMS_PER_PROD)."
}

build_hostnames() {
  local i name
  declare -A seen=()

  HOSTNAMES=()
  for name in "$PRIMARY_DOMAIN" "${ALIAS_DOMAINS[@]}"; do
    [[ -n "$name" ]] || continue
    if [[ -z "${seen[$name]:-}" ]]; then
      HOSTNAMES+=("$name")
      seen["$name"]=1
    fi
  done

  for ((i = 1; i <= MAX_STAGE_SUFFIX; i++)); do
    name="stage${i}${RESOURCE_NAME}.${PRIMARY_DOMAIN}"
    if [[ -z "${seen[$name]:-}" ]]; then
      HOSTNAMES+=("$name")
      seen["$name"]=1
    fi
  done

  printf '\nThe certificate must cover these hostnames (staging clones inherit the app):\n'
  printf '    %s\n' "${HOSTNAMES[@]}"
}

name_is_covered_by_primary_wildcard() {
  local name="$1"
  [[ "$name" =~ ^[^.]+[.]${PRIMARY_DOMAIN//./[.]}$ ]]
}

show_cloudflare_instructions() {
  local alias
  CERT_EXTRA_NAMES=()
  for alias in "${ALIAS_DOMAINS[@]}"; do
    [[ -n "$alias" ]] || continue
    if [[ "$alias" != "$PRIMARY_DOMAIN" ]] && ! name_is_covered_by_primary_wildcard "$alias"; then
      CERT_EXTRA_NAMES+=("$alias")
    fi
  done

  log "Create a Cloudflare Origin CA certificate"
  cat <<EOF2

In the Cloudflare dashboard for the zone containing $PRIMARY_DOMAIN:

  1. Open SSL/TLS -> Origin Server.
  2. Under Origin Certificates, choose Create Certificate.
  3. Choose "Generate private key and CSR with Cloudflare".
     ECC or RSA is acceptable; the app supports either.
  4. Make sure the certificate hostnames include:

         $PRIMARY_DOMAIN
         *.$PRIMARY_DOMAIN
EOF2

  if ((${#CERT_EXTRA_NAMES[@]})); then
    printf '\n     These registered aliases are NOT covered by *.%s and must also be added:\n\n' \
      "$PRIMARY_DOMAIN"
    printf '         %s\n' "${CERT_EXTRA_NAMES[@]}"
  fi

  cat <<'EOF2'

  5. Choose the certificate validity period you want and create the certificate.
  6. Choose PEM for the key/certificate format.
  7. Save BOTH values on this workstation as permanent backup files:
       - the Origin Certificate PEM
       - the Private Key PEM
     Protect the private-key backup with mode 0600.

Do not discard the private key after this deployment. Cloudflare Origin CA
certificates are intended for the Cloudflare-to-origin TLS connection (here,
from the cluster's tunnel replicas); direct browser access to the VM would not
trust them.
EOF2
  if bmac_ui_is_json; then
    local names="$PRIMARY_DOMAIN and *.$PRIMARY_DOMAIN"
    ((${#CERT_EXTRA_NAMES[@]} == 0)) || names+=", plus ${CERT_EXTRA_NAMES[*]}"
    bmac_ui_manual_action --id origin_certificate --title "Create a Cloudflare Origin CA certificate" \
      --instruction "In the Cloudflare dashboard for the zone containing $PRIMARY_DOMAIN, open SSL/TLS -> Origin Server and choose Create Certificate under Origin Certificates." \
      --instruction "Choose \"Generate private key and CSR with Cloudflare\" (ECC or RSA). The certificate hostnames must include $names." \
      --instruction "Choose the validity period, create the certificate, and choose PEM format." \
      --instruction "Save BOTH the Origin Certificate PEM and the Private Key PEM on this workstation as permanent backup files; protect the private key with mode 0600." \
      --ack-label "Both files are saved"
  fi
}

# GNU "realpath -e" has no BSD/macOS equivalent flag: there the option is an
# error, which the callers swallowed, so every path looked missing and the
# prompt looped forever. Test existence first, then resolve portably.
resolve_existing_path() {
  [[ -e "$1" ]] || return 1
  realpath -- "$1"
}

collect_and_validate_certificate() {
  local cert_pub_hash key_pub_hash name

  while :; do
    prompt_path CERT_PATH "Path to the saved Cloudflare Origin Certificate PEM"
    CERT_PATH="$(resolve_existing_path "$CERT_PATH" 2>/dev/null || true)"
    [[ -n "$CERT_PATH" ]] || {
      warn "Certificate path does not exist."
      continue
    }
    if [[ ! -f "$CERT_PATH" || -L "$CERT_PATH" ]]; then
      warn "Certificate must be a regular, non-symlink file."
      continue
    fi
    break
  done

  while :; do
    prompt_path KEY_PATH "Path to the saved Cloudflare Origin private-key PEM"
    KEY_PATH="$(resolve_existing_path "$KEY_PATH" 2>/dev/null || true)"
    [[ -n "$KEY_PATH" ]] || {
      warn "Private-key path does not exist."
      continue
    }
    if [[ ! -f "$KEY_PATH" || -L "$KEY_PATH" ]]; then
      warn "Private key must be a regular, non-symlink file."
      continue
    fi
    break
  done

  openssl x509 -in "$CERT_PATH" -noout >/dev/null 2>&1 ||
    die "Certificate file is not a readable PEM X.509 certificate"
  openssl x509 -in "$CERT_PATH" -checkend 0 -noout >/dev/null 2>&1 ||
    die "Certificate is already expired or not currently valid"
  openssl pkey -in "$KEY_PATH" -passin pass: -noout </dev/null >/dev/null 2>&1 ||
    die "Private key is not a readable, unencrypted PEM private key"

  cert_pub_hash="$(
    openssl x509 -in "$CERT_PATH" -pubkey -noout |
      openssl pkey -pubin -outform DER 2>/dev/null |
      sha256sum | awk '{print $1}'
  )"
  key_pub_hash="$(
    openssl pkey -in "$KEY_PATH" -passin pass: -pubout -outform DER 2>/dev/null |
      sha256sum | awk '{print $1}'
  )"
  [[ -n "$cert_pub_hash" && "$cert_pub_hash" == "$key_pub_hash" ]] ||
    die "The certificate and private key do not match"

  for name in "${HOSTNAMES[@]}"; do
    openssl x509 -in "$CERT_PATH" -noout -checkhost "$name" >/dev/null 2>&1 ||
      die "The certificate does not cover hostname: $name"
  done

  # No "--" here: BSD/macOS chmod stops option parsing at the mode, so it reads
  # a following "--" as a file name. KEY_PATH is an absolute resolved path.
  chmod 0600 "$KEY_PATH" || die "Could not set the private-key backup to mode 0600"

  info "Certificate/key pair matches and covers every hostname."
  info "Local certificate backup retained at: $CERT_PATH"
  info "Local private-key backup retained at: $KEY_PATH"
}

# One static binary, built here, so the VM needs nothing but the binary.
build_app() {
  local target_dir="${PROXMOX_ARTIFACTS_DIR}/hello-todo-build"
  log "Build the todo app ($BUILD_TARGET)"
  [[ "$(uname -s)" == Linux ]] ||
    die "The todo app is built for the VM on this workstation, which must run Linux"
  require_regular_file "$APP_DIR/Cargo.toml" "Todo app manifest"
  require_regular_file "$APP_DIR/Cargo.lock" "Todo app lockfile"
  command -v cargo >/dev/null 2>&1 ||
    die "cargo is not installed; install Rust from https://rustup.rs and rerun"
  if command -v rustup >/dev/null 2>&1 &&
    ! rustup target list --installed 2>/dev/null | grep -Fxq "$BUILD_TARGET"; then
    die "The Rust $BUILD_TARGET target is missing; install it with: rustup target add $BUILD_TARGET"
  fi
  [[ -n "${CC_x86_64_unknown_linux_musl:-}" ]] || command -v musl-gcc >/dev/null 2>&1 ||
    die "musl-gcc is missing (it compiles the app's bundled SQLite and TLS code); install it with: sudo apt install musl-tools"
  install -d -m 0700 "$target_dir"
  CARGO_TARGET_DIR="$target_dir" cargo build --release --locked \
    --manifest-path "$APP_DIR/Cargo.toml" --target "$BUILD_TARGET" ||
    die "The todo app did not build"
  APP_BINARY="$target_dir/$BUILD_TARGET/release/hello-todo"
  require_regular_file "$APP_BINARY" "Built todo app binary"
  info "Built $APP_BINARY ($(wc -c <"$APP_BINARY" | tr -d ' ') bytes)"
}

# Port 443 must be free for the app, or already the app's own.
check_port_443() {
  local holder
  log "Check that port 443 on $RESOURCE_NAME is free for the app"
  holder="$(
    ssh -o BatchMode=yes "$RESOURCE_NAME" bash -s <<'REMOTE'
set -Eeuo pipefail
ss -Hltnp 'sport = :443' 2>/dev/null | sed -n 's/.*users:(("\([^"]*\)".*/\1/p' | sort -u | tr '\n' ' '
REMOTE
  )" || die "Could not inspect listening ports on $RESOURCE_NAME"
  holder="${holder% }"
  if [[ -n "$holder" && "$holder" != hello-todo ]]; then
    printf '\n%s already listens on port 443 on %s.\n' "$holder" "$RESOURCE_NAME" >&2
    printf 'If it is NGINX from the earlier example app, remove it first:\n\n' >&2
    printf "    ssh %s 'systemctl disable --now nginx'\n\n" "$RESOURCE_NAME" >&2
    die "Port 443 on $RESOURCE_NAME is in use by $holder"
  fi
  info "Port 443 is ${holder:+used by the app already}${holder:-free}."
}

write_remote_installer() {
  cat >"$RUN_DIR/remote-install.sh" <<'EOF2'
#!/usr/bin/env bash
set -Eeuo pipefail
set +x
umask 077

stage_dir="${1:?remote staging directory is required}"
primary_domain="${2:?primary domain is required}"

binary_dst=/usr/local/bin/hello-todo
unit_dst=/etc/systemd/system/hello-todo.service
cert_dst=/etc/hello-todo/cert.pem
key_dst=/etc/hello-todo/key.pem

backup_dir="$(mktemp -d /run/hello-todo-backup.XXXXXX)"
committed=false
backups_ready=false

backup_path() {
  local path="$1" name="$2"
  if [[ -e "$path" ]]; then
    cp -a "$path" "$backup_dir/$name"
    printf '1' >"$backup_dir/$name.present"
  else
    printf '0' >"$backup_dir/$name.present"
  fi
}

restore_path() {
  local path="$1" name="$2"
  rm -f -- "$path"
  if [[ "$(cat "$backup_dir/$name.present")" == 1 ]]; then
    cp -a "$backup_dir/$name" "$path"
  fi
}

rollback() {
  set +e
  [[ "$backups_ready" == true ]] || return 0
  systemctl stop hello-todo.service >/dev/null 2>&1
  restore_path "$binary_dst" binary
  restore_path "$unit_dst" unit
  restore_path "$cert_dst" cert
  restore_path "$key_dst" key
  systemctl daemon-reload
  if [[ "$(cat "$backup_dir/unit.present")" == 1 ]]; then
    systemctl restart hello-todo.service
  else
    systemctl disable hello-todo.service >/dev/null 2>&1
  fi
}

cleanup_remote() {
  local rc=$?
  set +e
  if [[ "$committed" != true && $rc -ne 0 ]]; then
    printf 'Deployment failed; restoring the previous app files.\n' >&2
    printf 'Recent app log:\n' >&2
    journalctl -u hello-todo.service --since '-5 minutes' --no-pager 2>/dev/null | tail -n 30 >&2
    rollback
  fi
  rm -rf -- "$backup_dir" "$stage_dir"
  exit "$rc"
}
trap cleanup_remote EXIT

for file in hello-todo hello-todo.service cert.pem key.pem; do
  [[ -f "$stage_dir/$file" && ! -L "$stage_dir/$file" ]] || {
    printf 'Missing staged file: %s\n' "$file" >&2
    exit 1
  }
done

backup_path "$binary_dst" binary
backup_path "$unit_dst" unit
backup_path "$cert_dst" cert
backup_path "$key_dst" key
backups_ready=true

install -d -o root -g root -m 0700 /etc/hello-todo
install -o root -g root -m 0755 "$stage_dir/hello-todo" "${binary_dst}.new"
mv -f "${binary_dst}.new" "$binary_dst"
install -o root -g root -m 0644 "$stage_dir/hello-todo.service" "$unit_dst"
install -o root -g root -m 0600 "$stage_dir/cert.pem" "$cert_dst"
install -o root -g root -m 0600 "$stage_dir/key.pem" "$key_dst"

systemctl daemon-reload
systemctl enable hello-todo.service >/dev/null
systemctl restart hello-todo.service

# Wait for the new process to answer over HTTPS with the installed certificate.
expected="$(openssl x509 -in "$cert_dst" -noout -fingerprint -sha256)"
body="$backup_dir/info.json"
for ((attempt = 1; attempt <= 50; attempt++)); do
  if systemctl is-active --quiet hello-todo.service &&
    status="$(curl --noproxy '*' -sS -k -o "$body" -w '%{http_code}' \
      --resolve "${primary_domain}:443:127.0.0.1" "https://${primary_domain}/api/v1/info" 2>/dev/null)" &&
    [[ "$status" == 200 ]] && grep -Fq '"max_todos"' "$body"; then
    served="$(
      openssl s_client -connect 127.0.0.1:443 -servername "$primary_domain" </dev/null 2>/dev/null |
        openssl x509 -noout -fingerprint -sha256
    )"
    [[ "$served" == "$expected" ]] || {
      printf 'The app serves a different certificate than the one installed.\n' >&2
      exit 1
    }
    committed=true
    printf 'The todo app answers on https://%s/ with the installed certificate.\n' "$primary_domain"
    exit 0
  fi
  sleep 0.2
done
printf 'The todo app did not answer on port 443 within 10 seconds.\n' >&2
exit 1
EOF2
  chmod 0700 "$RUN_DIR/remote-install.sh"
}

deploy_to_guest() {
  log "Deploy the todo app to $RESOURCE_NAME"

  REMOTE_STAGE_DIR="$(
    ssh -o BatchMode=yes "$RESOURCE_NAME" \
      'umask 077; mktemp -d /tmp/hello-todo-deploy.XXXXXX' </dev/null
  )" || die "Could not create a temporary deployment directory on $RESOURCE_NAME"
  [[ "$REMOTE_STAGE_DIR" =~ ^/tmp/hello-todo-deploy[.][A-Za-z0-9]+$ ]] ||
    die "Unexpected remote staging path: $REMOTE_STAGE_DIR"

  scp -q "$APP_BINARY" "$RESOURCE_NAME:$REMOTE_STAGE_DIR/hello-todo" ||
    die "Could not copy the app to $RESOURCE_NAME"
  scp -q "$APP_DIR/hello-todo.service" "$RESOURCE_NAME:$REMOTE_STAGE_DIR/hello-todo.service" ||
    die "Could not copy the service unit to $RESOURCE_NAME"
  scp -q "$CERT_PATH" "$RESOURCE_NAME:$REMOTE_STAGE_DIR/cert.pem" ||
    die "Could not copy the origin certificate to $RESOURCE_NAME"
  scp -q "$KEY_PATH" "$RESOURCE_NAME:$REMOTE_STAGE_DIR/key.pem" ||
    die "Could not copy the origin private key to $RESOURCE_NAME"
  scp -q "$RUN_DIR/remote-install.sh" "$RESOURCE_NAME:$REMOTE_STAGE_DIR/remote-install.sh" ||
    die "Could not copy the remote install helper to $RESOURCE_NAME"

  ssh -o BatchMode=yes "$RESOURCE_NAME" \
    "bash '$REMOTE_STAGE_DIR/remote-install.sh' '$REMOTE_STAGE_DIR' '$PRIMARY_DOMAIN'" </dev/null ||
    die "Remote deployment failed"

  REMOTE_STAGE_DIR=""
}

# The VM's monitor agent watches the app's service and its origin
# certificate; it re-reads guest.json every cycle. Existing entries are kept.
add_app_to_monitoring() {
  monitoring_enabled || return 0
  log "Add the app to $RESOURCE_NAME's monitored services"
  # shellcheck disable=SC2016 # Evaluated by python3 on the VM.
  if ssh -o BatchMode=yes "$RESOURCE_NAME" python3 - <<'REMOTE'; then
import json
import os
from pathlib import Path
import tempfile

path = Path("/etc/app-ha-monitor/guest.json")
try:
    settings = json.loads(path.read_text(encoding="utf-8"))
except FileNotFoundError:
    settings = {"schema_version": 1, "services": [], "cert_paths": []}
settings.setdefault("services", [])
settings.setdefault("cert_paths", [])
changed = False
for key, value in (("services", "hello-todo.service"), ("cert_paths", "/etc/hello-todo/cert.pem")):
    if value not in settings[key]:
        settings[key].append(value)
        changed = True
if changed:
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(dir=path.parent, prefix=".guest.json.")
    with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
        json.dump(settings, handle, indent=2, sort_keys=True)
        handle.write("\n")
    os.chmod(temporary, 0o600)
    os.replace(temporary, path)
print("added" if changed else "already watched")
REMOTE
    info "The monitor watches hello-todo.service and /etc/hello-todo/cert.pem."
  else
    warn "Could not update /etc/app-ha-monitor/guest.json; add the app with the Production page's Monitored services tile"
  fi
}

print_completion() {
  log "Todo app deployment complete"
  info "Production VM: $RESOURCE_NAME"
  info "URL: https://$PRIMARY_DOMAIN/"
  info "API: https://$PRIMARY_DOMAIN/api/v1/todos"
  info "Service: hello-todo.service (enabled; starts at boot)"
  info "Program: /usr/local/bin/hello-todo"
  info "Database: /var/lib/hello-todo/todos.sqlite3"
  info "Origin certificate and key: /etc/hello-todo/ (root only)"
  info "Staging clones inherit the app and a copy of its data; they answer on stage1${RESOURCE_NAME}.${PRIMARY_DOMAIN} through stage${MAX_STAGE_SUFFIX}${RESOURCE_NAME}.${PRIMARY_DOMAIN}."
  info "Workstation certificate/key backups remain at the paths you supplied."
  cat <<EOF2

Try the API (the list holds at most 100 todos):

    curl https://$PRIMARY_DOMAIN/api/v1/todos
    curl -X POST -H 'Content-Type: application/json' -d '{"title":"Hello"}' https://$PRIMARY_DOMAIN/api/v1/todos
EOF2
  bmac_ui_result resource "$RESOURCE_NAME" url "https://$PRIMARY_DOMAIN/" \
    api_url "https://$PRIMARY_DOMAIN/api/v1/todos"
  bmac_ui_next_step "Open https://$PRIMARY_DOMAIN/ to use the todo list."
  if monitoring_enabled; then
    printf '\nThe app answers https://%s/api/v1/healthz for monitoring. To have the QDevice\n' "$PRIMARY_DOMAIN"
    printf 'probe it, use the QDevice page'"'"'s Health-check probes tile, or run:\n\n'
    printf '    scripts/user_callable/qdevice/manage_qdevice_probes.sh --vm %s --enable --path /api/v1/healthz --status 200\n' \
      "$RESOURCE_NAME"
    bmac_ui_next_step "Probe the app's health endpoint from the QDevice." \
      --command "scripts/user_callable/qdevice/manage_qdevice_probes.sh --vm $RESOURCE_NAME --enable --path /api/v1/healthz --status 200" \
      --workflow manage_qdevice_probes --arg "resource=$RESOURCE_NAME"
  fi
}

main() {
  require_command bash
  require_command python3
  require_command ssh
  require_command scp
  require_command openssl
  require_command sha256sum
  require_command realpath
  require_command grep

  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/deploy-hello-todo.XXXXXX")"

  load_config
  COORDINATOR="$(first_reachable_mox)" || die "No reachable mox host was found"
  info "Registry coordinator: $COORDINATOR"

  select_production_resource
  build_app
  confirm_guest_ssh
  preflight_target_tools
  check_port_443
  show_rerun_behavior
  compute_stage_limit
  build_hostnames
  show_cloudflare_instructions
  collect_and_validate_certificate
  write_remote_installer

  printf '\nThis will install the todo app as hello-todo.service on %s.\n' "$RESOURCE_NAME"
  prompt_yes "Proceed with deployment?" || die "Deployment cancelled"

  deploy_to_guest
  add_app_to_monitoring
  print_completion
}

main "$@"
