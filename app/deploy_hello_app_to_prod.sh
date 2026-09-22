#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Deploy a minimal static NGINX HTTPS application to one registry-backed
# production VM. The production VM's registered primary/alias domains and all
# possible default stageNprodN.<primary-domain> names are installed as exact
# NGINX server names so staging clones inherit a usable configuration.

set -Eeuo pipefail
set +x
umask 077

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
REPO_ROOT="$(cd -- "${SCRIPT_DIR}/.." && pwd -P)"
CONFIG_LIB="${REPO_ROOT}/lib/config.sh"
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
MAX_STAGE_SUFFIX=0

declare -a ALIAS_DOMAINS=()
declare -a PLACEMENT_NODES=()
declare -a SERVER_NAMES=()
declare -a CERT_EXTRA_NAMES=()

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
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

prompt_yes() {
  local prompt="$1" answer
  read -r -p "$prompt [y/N] " answer
  [[ "$answer" =~ ^[Yy]([Ee][Ss])?$ ]]
}

prompt_path() {
  local variable="$1" prompt="$2" value
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
  # shellcheck source=../lib/config.sh
  source "$CONFIG_LIB"
  load_proxmox_config --no-secrets >/dev/null ||
    die "Could not load env/cluster.conf"

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

  while :; do
    read -r -p "Production VM to deploy to (for example prod1): " RESOURCE_NAME
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
  local missing_raw missing_tool
  local -a missing=()
  local -a required_tools=(
    bash apt-get systemctl install cp rm ln mktemp grep cat curl find sed sleep journalctl tail
  )

  log "Verify required CLI tools on $RESOURCE_NAME"
  cat <<EOF2

This deployment uses standard Ubuntu utilities plus curl on the target guest.
NGINX itself does not need to be preinstalled; the deployment will install the
Ubuntu nginx package automatically if the nginx command is absent.

Please ensure these target-side commands are available before proceeding:

    curl

The script will verify all commands it depends on now.
EOF2

  prompt_yes "Have you ensured the required target-side CLI tools are available on $RESOURCE_NAME?" || {
    printf '\nInstall curl if needed, for example:\n\n'
    printf "    ssh %s 'apt-get update && apt-get install -y curl'\n\n" "$RESOURCE_NAME"
    die "Target prerequisites were not confirmed"
  }

  missing_raw="$(
    ssh -o BatchMode=yes "$RESOURCE_NAME" bash -s -- "${required_tools[@]}" <<'REMOTE'
set -Eeuo pipefail
shift 0
for command_name in "$@"; do
  command -v "$command_name" >/dev/null 2>&1 || printf '%s\n' "$command_name"
done
REMOTE
  )" || die "Could not verify target-side CLI prerequisites on $RESOURCE_NAME"

  if [[ -n "$missing_raw" ]]; then
    while IFS= read -r missing_tool; do
      [[ -n "$missing_tool" ]] && missing+=("$missing_tool")
    done <<<"$missing_raw"

    printf '\nMissing target-side commands:\n' >&2
    printf '    %s\n' "${missing[@]}" >&2
    if printf '%s\n' "${missing[@]}" | grep -Fxq curl; then
      printf '\nInstall curl, then rerun this script:\n\n' >&2
      printf "    ssh %s 'apt-get update && apt-get install -y curl'\n\n" "$RESOURCE_NAME" >&2
    fi
    die "Required target-side CLI tools are missing"
  fi

  info "All required target-side CLI tools are available."
}

check_existing_nginx_conflicts() {
  local output

  log "Check for conflicting enabled NGINX hostnames"

  # A fresh guest may not have NGINX yet. In that case there cannot be an
  # enabled NGINX server-name conflict, and the deployment installs NGINX.
  if ! ssh -o BatchMode=yes "$RESOURCE_NAME" 'command -v nginx >/dev/null 2>&1' </dev/null; then
    info "NGINX is not installed yet; no existing NGINX hostname conflicts can exist."
    return 0
  fi

  output="$(
    ssh -o BatchMode=yes "$RESOURCE_NAME" bash -s -- "${SERVER_NAMES[@]}" <<'REMOTE'
set -Eeuo pipefail

managed=/etc/nginx/sites-enabled/hello-app
found=false

for root in /etc/nginx/sites-enabled /etc/nginx/conf.d; do
  [[ -d "$root" ]] || continue
  while IFS= read -r -d '' path; do
    [[ "$path" == "$managed" ]] && continue
    [[ -f "$path" ]] || continue
    for hostname in "$@"; do
      if grep -Fq -- "$hostname" "$path"; then
        printf '%s: %s\n' "$path" "$hostname"
        found=true
      fi
    done
  done < <(find -L "$root" -maxdepth 1 -type f -print0 2>/dev/null)
done

[[ "$found" == false ]]
REMOTE
  )" && {
    info "No conflicting enabled NGINX configuration references the managed hostnames."
    return 0
  }

  if [[ -n "$output" ]]; then
    printf '\nPotential NGINX hostname conflicts were found on %s:\n' "$RESOURCE_NAME" >&2
    printf '    %s\n' "$output" >&2
    printf '\nThe deployment will not disable unrelated NGINX sites automatically.\n' >&2
    die "Resolve or disable the conflicting NGINX configuration, then rerun the script"
  fi

  die "Could not inspect existing NGINX configuration on $RESOURCE_NAME"
}

show_rerun_behavior() {
  cat <<'EOF2'

This deployment script is safe to rerun after a failed attempt. It does not
resume from an internal checkpoint; rerunning starts the workflow again,
rediscovers the production resource, revalidates the certificate/key, and
reapplies the same managed files. A failed remote deployment restores the
previous NGINX files before exiting. Temporary staging files are also removed.
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
  local node limit max=0

  log "Determine all possible default staging hostnames"
  for node in "${PLACEMENT_NODES[@]}"; do
    limit="$(
      bash -c '
set -Eeuo pipefail
source "$1"
load_proxmox_config --host "$2" --no-secrets >/dev/null
printf "%s\n" "$MAX_STAGING_VM_COUNT_ON_THIS_HOST"
' bash "$CONFIG_LIB" "$node"
    )" || die "Could not load MAX_STAGING_VM_COUNT_ON_THIS_HOST for $node"

    validate_positive_integer "MAX_STAGING_VM_COUNT_ON_THIS_HOST for $node" "$limit"
    info "$node staging limit: $limit"
    ((limit > max)) && max="$limit"
  done

  ((max > 0)) || die "Could not determine a staging limit"
  MAX_STAGE_SUFFIX="$max"
  info "NGINX will cover stage suffixes 1-$MAX_STAGE_SUFFIX."
  info "The maximum across placement hosts is used so an HA owner move cannot make the inherited config too narrow."
}

build_server_names() {
  local alias i name
  declare -A seen=()

  SERVER_NAMES=()
  for name in "$PRIMARY_DOMAIN" "${ALIAS_DOMAINS[@]}"; do
    [[ -n "$name" ]] || continue
    if [[ -z "${seen[$name]:-}" ]]; then
      SERVER_NAMES+=("$name")
      seen["$name"]=1
    fi
  done

  for ((i = 1; i <= MAX_STAGE_SUFFIX; i++)); do
    name="stage${i}${RESOURCE_NAME}.${PRIMARY_DOMAIN}"
    if [[ -z "${seen[$name]:-}" ]]; then
      SERVER_NAMES+=("$name")
      seen["$name"]=1
    fi
  done

  printf '\nNGINX will accept these exact hostnames:\n'
  printf '    %s\n' "${SERVER_NAMES[@]}"
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
     ECC or RSA is acceptable; NGINX supports either.
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
certificates are intended for the Cloudflare-to-origin TLS connection; direct
browser access to the origin may not trust them.
EOF2
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

  for name in "${SERVER_NAMES[@]}"; do
    openssl x509 -in "$CERT_PATH" -noout -checkhost "$name" >/dev/null 2>&1 ||
      die "The certificate does not cover NGINX hostname: $name"
  done

  # No "--" here: BSD/macOS chmod stops option parsing at the mode, so it reads
  # a following "--" as a file name. KEY_PATH is an absolute resolved path.
  chmod 0600 "$KEY_PATH" || die "Could not set the private-key backup to mode 0600"

  info "Certificate/key pair matches and covers every configured NGINX hostname."
  info "Local certificate backup retained at: $CERT_PATH"
  info "Local private-key backup retained at: $KEY_PATH"
}

write_nginx_assets() {
  local names_file="$RUN_DIR/server_names.txt"
  local site_file="$RUN_DIR/hello-app.nginx"
  local index_file="$RUN_DIR/index.html"
  local remote_helper="$RUN_DIR/remote-install.sh"
  local name

  : >"$names_file"
  for name in "${SERVER_NAMES[@]}"; do
    printf '        %s\n' "$name" >>"$names_file"
  done

  cat >"$site_file" <<EOF2
# Managed by app/deploy_hello_app_to_prod.sh for $RESOURCE_NAME.
# Production aliases and every default staging name are intentionally listed
# explicitly so staging clones inherit a ready-to-use NGINX configuration.

server {
    listen 80;
    listen [::]:80;

    server_name
$(cat "$names_file")
        ;

    location = /healthz {
        access_log off;
        default_type text/plain;
        return 200 "ok\\n";
    }

    location / {
        return 301 https://\$host\$request_uri;
    }
}

server {
    listen 443 ssl;
    listen [::]:443 ssl;

    server_name
$(cat "$names_file")
        ;

    ssl_certificate     /etc/nginx/ssl/cloudflare-origin.pem;
    ssl_certificate_key /etc/nginx/ssl/cloudflare-origin.key;
    ssl_protocols TLSv1.2 TLSv1.3;

    root /var/www/hello-app;
    index index.html;

    location = /healthz {
        access_log off;
        default_type text/plain;
        return 200 "ok\\n";
    }

    location / {
        try_files \$uri \$uri/ =404;
    }
}
EOF2

  cat >"$index_file" <<EOF2
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Hello</title>
</head>
<body>
  <h1>Hello, world!</h1>
  <p>Deployed to $RESOURCE_NAME.</p>
</body>
</html>
EOF2

  cat >"$remote_helper" <<'EOF2'
#!/usr/bin/env bash
set -Eeuo pipefail
set +x
umask 077

stage_dir="${1:?remote staging directory is required}"
primary_domain="${2:?primary domain is required}"

cert_dst=/etc/nginx/ssl/cloudflare-origin.pem
key_dst=/etc/nginx/ssl/cloudflare-origin.key
site_dst=/etc/nginx/sites-available/hello-app
site_link=/etc/nginx/sites-enabled/hello-app
default_link=/etc/nginx/sites-enabled/default
index_dst=/var/www/hello-app/index.html

backup_dir="$(mktemp -d /run/hello-app-backup.XXXXXX)"
committed=false
backups_ready=false

backup_path() {
  local path="$1" name="$2"
  if [[ -e "$path" || -L "$path" ]]; then
    cp -a --no-dereference "$path" "$backup_dir/$name"
    printf '1' >"$backup_dir/$name.present"
  else
    printf '0' >"$backup_dir/$name.present"
  fi
}

restore_path() {
  local path="$1" name="$2"
  rm -f -- "$path"
  if [[ "$(cat "$backup_dir/$name.present")" == 1 ]]; then
    cp -a --no-dereference "$backup_dir/$name" "$path"
  fi
}

rollback() {
  set +e
  [[ "$backups_ready" == true ]] || return 0
  restore_path "$cert_dst" cert
  restore_path "$key_dst" key
  restore_path "$site_dst" site
  restore_path "$site_link" site-link
  restore_path "$default_link" default-link
  restore_path "$index_dst" index
  if command -v nginx >/dev/null 2>&1 && nginx -t >/dev/null 2>&1; then
    systemctl is-active --quiet nginx && systemctl reload nginx >/dev/null 2>&1 || true
  fi
}

cleanup_remote() {
  local rc=$?
  set +e
  if [[ "$committed" != true && $rc -ne 0 ]]; then
    printf 'Deployment failed; restoring the previous NGINX files.\n' >&2
    rollback
  fi
  rm -rf -- "$backup_dir" "$stage_dir"
  exit "$rc"
}
trap cleanup_remote EXIT

for file in cloudflare-origin.pem cloudflare-origin.key hello-app.nginx index.html; do
  [[ -f "$stage_dir/$file" && ! -L "$stage_dir/$file" ]] || {
    printf 'Missing staged file: %s\n' "$file" >&2
    exit 1
  }
done

if ! command -v nginx >/dev/null 2>&1; then
  apt-get update
  DEBIAN_FRONTEND=noninteractive apt-get install -y nginx
fi

install -d -m 0755 /etc/nginx/ssl /etc/nginx/sites-available /etc/nginx/sites-enabled /var/www/hello-app

backup_path "$cert_dst" cert
backup_path "$key_dst" key
backup_path "$site_dst" site
backup_path "$site_link" site-link
backup_path "$default_link" default-link
backup_path "$index_dst" index
backups_ready=true

install -o root -g root -m 0644 "$stage_dir/cloudflare-origin.pem" "$cert_dst"
install -o root -g root -m 0600 "$stage_dir/cloudflare-origin.key" "$key_dst"
install -o root -g root -m 0644 "$stage_dir/hello-app.nginx" "$site_dst"
install -o root -g root -m 0644 "$stage_dir/index.html" "$index_dst"
ln -sfn ../sites-available/hello-app "$site_link"
rm -f -- "$default_link"

nginx -t
systemctl enable nginx >/dev/null
if systemctl is-active --quiet nginx; then
  systemctl reload nginx
else
  systemctl start nginx
fi
systemctl is-active --quiet nginx

command -v curl >/dev/null 2>&1 || {
  printf 'curl disappeared after prerequisite validation; refusing to skip HTTP checks.\n' >&2
  exit 1
}

# A graceful NGINX reload is asynchronous. systemctl can report the reload
# complete while old workers are still accepting requests for a short time.
# Poll the actual virtual host instead of treating the first request after a
# reload as authoritative.
wait_for_http_validation() {
  local label="$1" expected_status="$2" body_file="$3" body_mode="$4" expected_body="$5"
  shift 5
  local attempt status="" curl_rc=0
  local max_attempts=50
  local sleep_seconds=0.1

  for ((attempt = 1; attempt <= max_attempts; attempt++)); do
    : >"$body_file"
    curl_rc=0
    status="$(
      curl --noproxy '*' -sS -o "$body_file" -w '%{http_code}' "$@"
    )" || curl_rc=$?

    if ((curl_rc == 0)) && [[ "$status" == "$expected_status" ]]; then
      case "$body_mode" in
        exact)
          grep -Fxq -- "$expected_body" "$body_file" && return 0
          ;;
        contains)
          grep -Fq -- "$expected_body" "$body_file" && return 0
          ;;
        none)
          return 0
          ;;
        *)
          printf 'Internal error: unknown validation body mode: %s\n' "$body_mode" >&2
          return 1
          ;;
      esac
    fi

    ((attempt < max_attempts)) && sleep "$sleep_seconds"
  done

  if ((curl_rc != 0)); then
    printf '%s failed after %d attempts: final curl exit status was %d.\n' \
      "$label" "$max_attempts" "$curl_rc" >&2
  else
    printf '%s failed after %d attempts: final HTTP status was %s; expected %s.\n' \
      "$label" "$max_attempts" "${status:-unknown}" "$expected_status" >&2
  fi

  printf '%s final response body follows:\n' "$label" >&2
  sed -n '1,40p' "$body_file" >&2 || true
  printf '\nActive NGINX configuration references for %s:\n' "$primary_domain" >&2
  nginx -T 2>&1 | grep -n -F -C 3 -- "$primary_domain" >&2 || true
  printf '\nRecent NGINX service log:\n' >&2
  journalctl -u nginx --since '-2 minutes' --no-pager 2>/dev/null | tail -n 40 >&2 || true
  return 1
}

http_health_body="$backup_dir/http-health.body"
https_health_body="$backup_dir/https-health.body"
https_root_body="$backup_dir/https-root.body"

wait_for_http_validation \
  'HTTP /healthz validation' 200 "$http_health_body" exact 'ok' \
  -H "Host: $primary_domain" \
  http://127.0.0.1/healthz

wait_for_http_validation \
  'HTTPS /healthz validation' 200 "$https_health_body" exact 'ok' \
  -k --resolve "$primary_domain:443:127.0.0.1" \
  "https://$primary_domain/healthz"

wait_for_http_validation \
  'HTTPS hello-page validation' 200 "$https_root_body" contains '<h1>Hello, world!</h1>' \
  -k --resolve "$primary_domain:443:127.0.0.1" \
  "https://$primary_domain/"

committed=true
printf 'NGINX deployment validated and active.\n'
EOF2

  chmod 0700 "$remote_helper"
}

deploy_to_guest() {
  log "Deploy hello application to $RESOURCE_NAME"

  REMOTE_STAGE_DIR="$(
    ssh -o BatchMode=yes "$RESOURCE_NAME" \
      'umask 077; mktemp -d /tmp/hello-app-deploy.XXXXXX' </dev/null
  )" || die "Could not create a temporary deployment directory on $RESOURCE_NAME"
  [[ "$REMOTE_STAGE_DIR" =~ ^/tmp/hello-app-deploy[.][A-Za-z0-9]+$ ]] ||
    die "Unexpected remote staging path: $REMOTE_STAGE_DIR"

  scp -q "$CERT_PATH" \
    "$RESOURCE_NAME:$REMOTE_STAGE_DIR/cloudflare-origin.pem" ||
    die "Could not copy the origin certificate to $RESOURCE_NAME"
  scp -q "$KEY_PATH" \
    "$RESOURCE_NAME:$REMOTE_STAGE_DIR/cloudflare-origin.key" ||
    die "Could not copy the origin private key to $RESOURCE_NAME"
  scp -q "$RUN_DIR/hello-app.nginx" \
    "$RESOURCE_NAME:$REMOTE_STAGE_DIR/hello-app.nginx" ||
    die "Could not copy the NGINX site to $RESOURCE_NAME"
  scp -q "$RUN_DIR/index.html" \
    "$RESOURCE_NAME:$REMOTE_STAGE_DIR/index.html" ||
    die "Could not copy the hello page to $RESOURCE_NAME"
  scp -q "$RUN_DIR/remote-install.sh" \
    "$RESOURCE_NAME:$REMOTE_STAGE_DIR/remote-install.sh" ||
    die "Could not copy the remote install helper to $RESOURCE_NAME"

  ssh -o BatchMode=yes "$RESOURCE_NAME" \
    "bash '$REMOTE_STAGE_DIR/remote-install.sh' '$REMOTE_STAGE_DIR' '$PRIMARY_DOMAIN'" ||
    die "Remote NGINX deployment failed"

  REMOTE_STAGE_DIR=""
}

print_completion() {
  log "Hello application deployment complete"
  info "Production VM: $RESOURCE_NAME"
  info "Primary URL: https://$PRIMARY_DOMAIN/"
  info "Health URL: https://$PRIMARY_DOMAIN/healthz"
  info "NGINX site: /etc/nginx/sites-available/hello-app"
  info "Origin certificate: /etc/nginx/ssl/cloudflare-origin.pem"
  info "Origin private key: /etc/nginx/ssl/cloudflare-origin.key"
  info "Staging hostnames configured: stage1${RESOURCE_NAME}.${PRIMARY_DOMAIN} through stage${MAX_STAGE_SUFFIX}${RESOURCE_NAME}.${PRIMARY_DOMAIN}"
  info "Workstation certificate/key backups remain at the paths you supplied."
  printf '\nCloudflare should use Full (strict) origin TLS once this certificate is installed.\n'
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

  RUN_DIR="$(mktemp -d "${TMPDIR:-/tmp}/deploy-hello-app.XXXXXX")"

  load_config
  COORDINATOR="$(first_reachable_mox)" || die "No reachable mox host was found"
  info "Registry coordinator: $COORDINATOR"

  select_production_resource
  confirm_guest_ssh
  preflight_target_tools
  show_rerun_behavior
  compute_stage_limit
  build_server_names
  check_existing_nginx_conflicts
  show_cloudflare_instructions
  collect_and_validate_certificate
  write_nginx_assets

  printf '\nThis will install/reconfigure NGINX on %s and deploy the hello page.\n' "$RESOURCE_NAME"
  prompt_yes "Proceed with deployment?" || die "Deployment cancelled"

  deploy_to_guest
  print_completion
}

main "$@"
