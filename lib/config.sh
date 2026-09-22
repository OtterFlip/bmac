#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Strict, non-evaluating loader for the Proxmox orchestration configuration.
#
# This file is both a sourceable Bash library and a small validation command.
# It deliberately does not enable shell options in a caller that sources it.

# mapfile -d, which this library's callers rely on, arrived in Bash 4.4. macOS
# ships /bin/bash 3.2; a newer bash must come first on PATH there.
if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
  printf 'ERROR: lib/config.sh requires Bash 4.4 or newer (found %s).\n' \
    "${BASH_VERSION:-unknown}" >&2
  # shellcheck disable=SC2317 # exit is the executable-script fallback.
  return 2 2>/dev/null || exit 2
fi

PROXMOX_LIB_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
PROXMOX_DEPLOY_DIR="$(cd -- "${PROXMOX_LIB_DIR}/.." && pwd -P)"
# This tree is the git root. Older checkouts nested it under deploy/proxmox/.
PROXMOX_REPO_ROOT="$PROXMOX_DEPLOY_DIR"

# Tests may point the parser at synthetic files. Production callers always use
# env/ next to this library and cannot redirect the loader through an inherited
# value.
if [[ "${APP_HA_CONFIG_TEST_MODE:-0}" == 1 ]]; then
  PROXMOX_ENV_DIR="${APP_HA_ENV_DIR:?APP_HA_ENV_DIR is required in test mode}"
else
  PROXMOX_ENV_DIR="${PROXMOX_DEPLOY_DIR}/env"
fi

PROXMOX_CLUSTER_CONFIG="${PROXMOX_ENV_DIR}/cluster.conf"
PROXMOX_SECRETS_CONFIG="${PROXMOX_ENV_DIR}/secrets.env"

declare -Ag _PROXMOX_CONFIG_SEEN=()
declare -Ag _PROXMOX_CONFIG_ORIGIN=()
declare -Ag _PROXMOX_CONFIG_SCOPE=()
declare -Ag _PROXMOX_CONFIG_SOURCE_FILE=()
declare -Ag _PROXMOX_CONFIG_DERIVED=()
declare -ag _PROXMOX_CONFIG_PUBLIC_KEYS=()
declare -ag _PROXMOX_CONFIG_SECRET_KEYS=()
declare -Ag _PROXMOX_CONFIG_ALLOWED_SCOPE=()
declare -ag _PROXMOX_CONFIG_CLUSTER_KEYS=(
  PROXMOX_CLUSTER_NAME PROXMOX_QDEVICE_HOST
  PROXMOX_INTERNAL_DOMAIN
  MAX_MOX_HOSTS PROXMOX_MAX_HOSTS
  PRIVATE_SUBNET_CIDR PROXMOX_PRIVATE_SUBNET
  PROXMOX_PRIVATE_BRIDGE PROXMOX_MIGRATION_NETWORK
  DATACENTER_PRIVATE_VLAN_GATEWAY GUEST_EGRESS_VIP
  MOX_IP_START MOX_IP_END
  MOX_REPLICATION_IP_START MOX_REPLICATION_IP_END
  HAPROXY_IP_START HAPROXY_IP_END
  PROXMOX_DNS_SERVER PROXMOX_ADMIN_EMAIL
  PROXMOX_TIMEZONE PROXMOX_COUNTRY PROXMOX_KEYBOARD
  PROXMOX_ISO_FILE_FULL_PATH PROXMOX_ISO_FILE_SHA256
  PROD_GUEST_OS_ISO_URL PROD_GUEST_OS_ISO_SHA256
  PROD_GUEST_OS_INSTALL_MODE
  ADMIN_1_PUBLIC_SSH_KEY ADMIN_2_PUBLIC_SSH_KEY
  TAILSCALE_TAG PRODUCTION_VM_TAG STAGING_VM_TAG
  EVICTABLE_VM_TAG PRODUCTION_TAG STAGING_TAG EVICTABLE_TAG
  PURPOSE_TAG_PREFIX GUEST_ROLE_HOOK_PATH CLUSTER_STATE_DIR
  PRODUCTION_IP_START PRODUCTION_IP_END
  PROD_IP_RANGE_START PROD_IP_RANGE_END
  STAGING_IP_START STAGING_IP_END
  STAGING_IP_RANGE_START STAGING_IP_RANGE_END
  HAPROXY_LXC_ROOTFS_GB HAPROXY_LXC_MEMORY_MB
  HAPROXY_LXC_CORES HAPROXY_LXC_STORAGE HAPROXY_LXC_TEMPLATE
  PROD_VM_CORES PROD_VM_MEMORY_GIB PROD_VM_DISK_GB
  PROD_VM_STORAGE PROD_VM_BRIDGE PROD_VM_CPU_TYPE
  PROD_VM_REPLICATION_INTERVAL
  STAGING_VM_CORES STAGING_VM_MEMORY_GIB STAGING_VM_DISK_GB
  STAGING_VM_STORAGE STAGING_VM_BRIDGE
  VM_STORAGE_ID ZFS_STORAGE_ID ISO_STORAGE_ID
  SNIPPETS_STORAGE_ID DNS_SEARCH_DOMAIN GUEST_DNS_SERVERS
  MOX_SSH_USER MOX_SSH_CONNECT_TIMEOUT GUEST_SSH_USER
  PROXMOX_SSH_KNOWN_HOSTS_FILE
)
declare -ag _PROXMOX_CONFIG_MOX_KEYS=(
  IDRAC_IP IDRAC_ASSET_ID
  PROXMOX_IP PROXMOX_GATEWAY PROXMOX_NETMASK PROXMOX_PREFIX
  PROXMOX_PUBLIC_MAC PROXMOX_SECONDARY_MAC
  PROXMOX_SECONDARY_IP TAILSCALE_HOSTNAME
  HAPROXY_LXC_VMID HAPROXY_LXC_HOSTNAME HAPROXY_LXC_IP
  HAPROXY_LXC_GATEWAY VRRP_PRIORITY
  MAX_PROD_VM_COUNT_ON_THIS_HOST MAX_STAGING_VM_COUNT_ON_THIS_HOST
)
declare -ag _PROXMOX_CONFIG_ALLOWED_SECRET_KEYS=(
  IDRAC_USER IDRAC_PASSWORD PROXMOX_ROOT_PASSWORD
  PROD_GUEST_VM_ROOT_PASSWORD STAGING_GUEST_VM_ROOT_PASSWORD
  PROXMOX_LUKS_PASSWORD PROXMOX_QDEVICE_ROOT_PASSWORD
)

_config_initialize_allowlist() {
  local key pair member suffix
  for key in "${_PROXMOX_CONFIG_CLUSTER_KEYS[@]}"; do
    _PROXMOX_CONFIG_ALLOWED_SCOPE["$key"]=cluster
  done
  for key in "${_PROXMOX_CONFIG_MOX_KEYS[@]}"; do
    _PROXMOX_CONFIG_ALLOWED_SCOPE["$key"]=mox
  done
  for pair in 1 2 3 4 5; do
    for member in 1 2; do
      for suffix in SERIAL CAPACITY_BYTES; do
        key="NVME_MIRROR_${pair}_${suffix}_${member}"
        _PROXMOX_CONFIG_MOX_KEYS+=("$key")
        _PROXMOX_CONFIG_ALLOWED_SCOPE["$key"]=mox
      done
    done
  done
  for key in "${_PROXMOX_CONFIG_ALLOWED_SECRET_KEYS[@]}"; do
    _PROXMOX_CONFIG_ALLOWED_SCOPE["$key"]=secrets
  done
}

_config_initialize_allowlist

config_die() {
  printf 'ERROR: %s\n' "$*" >&2
  return 1
}

config_info() {
  printf '%s\n' "$*"
}

require_command() {
  local command_name
  for command_name in "$@"; do
    command -v "$command_name" >/dev/null 2>&1 ||
      config_die "Required command is unavailable: ${command_name}" || return
  done
}

require_commands() {
  require_command "$@"
}

require_var() {
  local name="${1:?variable name is required}"
  local expected_file="" expected_scope=""
  [[ "$name" =~ ^[A-Z][A-Z0-9_]*$ ]] ||
    config_die "Unsafe variable name passed to require_var: ${name}" || return
  [[ -n "${!name:-}" ]] ||
    config_die "Required variable ${name} is unset or empty" || return
  if [[ "${CONFIG_LOADED:-0}" == 1 &&
    -n "${_PROXMOX_CONFIG_ALLOWED_SCOPE[$name]-}" &&
    -z "${_PROXMOX_CONFIG_DERIVED[$name]+x}" ]]; then
    expected_scope="${_PROXMOX_CONFIG_ALLOWED_SCOPE[$name]}"
    case "$expected_scope" in
      cluster) expected_file="$PROXMOX_CLUSTER_CONFIG" ;;
      mox) expected_file="${CONFIG_HOST_FILE:-}" ;;
      secrets) expected_file="$PROXMOX_SECRETS_CONFIG" ;;
    esac
    [[ -n "$expected_file" &&
      "${_PROXMOX_CONFIG_SCOPE[$name]-}" == "$expected_scope" &&
      "${_PROXMOX_CONFIG_SOURCE_FILE[$name]-}" == "$expected_file" ]] ||
      config_die "Required variable ${name} does not originate in its configured ${expected_scope} file" ||
      return
  fi
}

require_vars() {
  local name
  for name in "$@"; do
    require_var "$name" || return
  done
}

require_file() {
  [[ -f "$1" ]] || config_die "Required file is missing: $1"
}

require_root() {
  ((EUID == 0)) || config_die "This operation must run as root"
}

prompt_yes() {
  local prompt="${1:?prompt is required}" answer
  IFS= read -r -p "${prompt} [y/N] " answer
  [[ "${answer,,}" == y || "${answer,,}" == yes ]]
}

require_yes() {
  prompt_yes "$1" || config_die "Prerequisite was not confirmed."
}

confirm_exact() {
  local prompt="${1:?prompt is required}" _legacy_phrase="${2:-}" entered
  printf '%s\nType GO to continue.\n> ' "$prompt"
  IFS= read -r entered
  [[ "$entered" == GO ]] ||
    config_die "Confirmation did not match GO; no action was taken."
}

prompt_required() {
  local destination="${1:?destination variable is required}"
  local prompt="${2:?prompt is required}"
  local value
  [[ "$destination" =~ ^[A-Z][A-Z0-9_]*$ ]] ||
    config_die "Unsafe prompt destination: ${destination}" || return
  IFS= read -r -p "${prompt}: " value
  [[ -n "$value" ]] || config_die "${destination} may not be empty" || return
  printf -v "$destination" '%s' "$value"
  export "${destination?}"
}

prompt_secret() {
  local destination="${1:?destination variable is required}"
  local prompt="${2:?prompt is required}"
  local value
  [[ "$destination" =~ ^[A-Z][A-Z0-9_]*$ ]] ||
    config_die "Unsafe secret destination: ${destination}" || return
  unset "$destination" 2>/dev/null ||
    config_die "Secret destination ${destination} is readonly and cannot be reset" || return
  IFS= read -r -s -p "${prompt}: " value
  printf '\n'
  [[ -n "$value" ]] || config_die "${destination} may not be empty" || return
  printf -v "$destination" '%s' "$value"
  export -n "${destination?}"
  unset value
}

prompt_tailscale_auth_key() {
  local value
  unset TAILSCALE_AUTH_KEY 2>/dev/null ||
    config_die "TAILSCALE_AUTH_KEY is readonly and cannot be reset" || return
  IFS= read -r -s -p "Fresh one-time Tailscale auth key: " value
  printf '\n'
  [[ "$value" == tskey-auth-* ]] || {
    unset value
    config_die "The supplied value does not look like a Tailscale auth key"
    return
  }
  TAILSCALE_AUTH_KEY="$value"
  export -n TAILSCALE_AUTH_KEY
  unset value
}

_config_trim() {
  local value="$1"
  value="${value#"${value%%[!$' \t']*}"}"
  value="${value%"${value##*[!$' \t']}"}"
  printf '%s' "$value"
}

_config_key_allowed() {
  local scope="$1" key="$2"
  [[ "${_PROXMOX_CONFIG_ALLOWED_SCOPE[$key]-}" == "$scope" ]]
}

_config_check_path_components() {
  local path="$1" cursor="/" component
  local relative="${path#/}"
  IFS=/ read -r -a _config_components <<<"$relative"
  for component in "${_config_components[@]}"; do
    [[ -n "$component" ]] || continue
    if [[ "$cursor" == / ]]; then
      cursor="/${component}"
    else
      cursor="${cursor}/${component}"
    fi
    [[ ! -L "$cursor" ]] ||
      config_die "Configuration path must not contain symlinks: ${cursor}" || return
  done
}

# GNU stat takes -c FORMAT and BSD/macOS stat takes -f FORMAT. Probe the tool,
# not the OS: a Mac with Homebrew coreutils first on PATH runs GNU stat.
if stat -c '%a' / >/dev/null 2>&1; then
  _PROXMOX_CONFIG_STAT_FLAVOR=gnu
else
  _PROXMOX_CONFIG_STAT_FLAVOR=bsd
fi

# Print FILE's permission bits the way GNU "stat -c %a" does (600, 4600, 7).
# The raw output is validated in full before any slicing or arithmetic, so a
# surprising stat can never be read as some mode.
_config_stat_mode() {
  local file="$1" raw
  if [[ "$_PROXMOX_CONFIG_STAT_FLAVOR" == gnu ]]; then
    raw="$(stat -c '%a' -- "$file")" || return 1
    [[ "$raw" =~ ^[0-7]{1,4}$ ]] || return 1
  else
    # %p is the whole st_mode in octal (100600). The file-type bits sit above
    # the low four digits, so those four are always present and exact. %Lp
    # would drop setuid/setgid/sticky, and %Mp%Lp loses digit positions.
    raw="$(stat -f '%p' -- "$file")" || return 1
    [[ "$raw" =~ ^[0-7]{5,7}$ ]] || return 1
    raw="${raw: -4}"
  fi
  printf '%o\n' "$((8#$raw))"
}

_config_stat_uid() {
  local file="$1" raw
  if [[ "$_PROXMOX_CONFIG_STAT_FLAVOR" == gnu ]]; then
    raw="$(stat -c '%u' -- "$file")" || return 1
  else
    raw="$(stat -f '%u' -- "$file")" || return 1
  fi
  [[ "$raw" =~ ^[0-9]+$ ]] || return 1
  printf '%s\n' "$raw"
}

_config_allowed_owner() {
  local owner_uid="$1"
  [[ "$owner_uid" == 0 || "$owner_uid" == "$EUID" ||
    ( -n "${SUDO_UID:-}" && "$owner_uid" == "$SUDO_UID" ) ]]
}

_config_validate_file_security() {
  local file="$1" secret="$2" mode owner_uid
  [[ "$file" == /* ]] || config_die "Configuration path must be absolute: ${file}" || return
  _config_check_path_components "$file" || return
  [[ -f "$file" && ! -L "$file" ]] ||
    config_die "Configuration must be a regular, non-symlink file: ${file}" || return
  mode="$(_config_stat_mode "$file")" ||
    config_die "Cannot read mode for ${file}" || return
  owner_uid="$(_config_stat_uid "$file")" ||
    config_die "Cannot read owner for ${file}" || return
  _config_allowed_owner "$owner_uid" ||
    config_die "Configuration owner is neither root nor the invoking user: ${file}" || return

  if [[ "$secret" == 1 ]]; then
    [[ "$mode" == 600 ]] ||
      config_die "Secret configuration must have mode 0600 (found ${mode}): ${file}" || return
  else
    # Git only stores the executable bit. A umask of 002 (Ubuntu user-private
    # groups) therefore checks out tracked cluster.conf / moxN.conf as 664, and
    # that mode cannot be committed. These files are non-secret, so reject only
    # world-writable checkouts. secrets.env remains exactly 0600.
    (( (8#$mode & 002) == 0 )) ||
      config_die "Configuration must not be world writable (mode ${mode}): ${file}" || return
  fi
}

_config_parse_value() {
  local raw value first last
  raw="$(_config_trim "$1")"
  [[ -n "$raw" ]] || {
    printf ''
    return
  }
  first="${raw:0:1}"
  last="${raw: -1}"
  if [[ "$first" == "'" || "$first" == '"' ]]; then
    [[ "$last" == "$first" && ${#raw} -ge 2 ]] ||
      config_die "Unterminated quoted configuration value" || return
    value="${raw:1:${#raw}-2}"
  else
    value="$raw"
  fi
  [[ "$value" != *$'\r'* && "$value" != *$'\n'* ]] ||
    config_die "Configuration values may not contain line breaks" || return
  printf '%s' "$value"
}

_config_parse_file() {
  local file="$1" scope="$2" secret="$3"
  local line trimmed key raw value line_number=0
  _config_validate_file_security "$file" "$secret" || return

  while IFS= read -r line || [[ -n "$line" ]]; do
    ((line_number += 1))
    [[ "$line" != *$'\r' ]] ||
      config_die "${file}:${line_number}: CRLF input is not accepted" || return
    trimmed="$(_config_trim "$line")"
    [[ -z "$trimmed" || "${trimmed:0:1}" == "#" ]] && continue
    [[ "$trimmed" =~ ^([A-Z][A-Z0-9_]*)[[:space:]]*=(.*)$ ]] ||
      config_die "${file}:${line_number}: expected a literal KEY=VALUE assignment" || return
    key="${BASH_REMATCH[1]}"
    raw="${BASH_REMATCH[2]}"
    _config_key_allowed "$scope" "$key" ||
      config_die "${file}:${line_number}: unknown or misplaced key ${key}" || return
    [[ -z "${_PROXMOX_CONFIG_SEEN[$key]+x}" ]] ||
      config_die "${file}:${line_number}: duplicate key ${key} (first seen in ${_PROXMOX_CONFIG_ORIGIN[$key]})" || return
    value="$(_config_parse_value "$raw")" || return

    printf -v "$key" '%s' "$value" ||
      config_die "${file}:${line_number}: cannot assign configuration key ${key}" || return
    if [[ "$secret" == 1 ]]; then
      export -n "${key?}"
      _PROXMOX_CONFIG_SECRET_KEYS+=("$key")
    else
      export "${key?}"
      _PROXMOX_CONFIG_PUBLIC_KEYS+=("$key")
    fi
    _PROXMOX_CONFIG_SEEN["$key"]=1
    _PROXMOX_CONFIG_ORIGIN["$key"]="${file}:${line_number}"
    _PROXMOX_CONFIG_SCOPE["$key"]="$scope"
    _PROXMOX_CONFIG_SOURCE_FILE["$key"]="$file"
  done <"$file"
}

_config_alias() {
  local canonical="$1" alternate="$2"
  if [[ -n "${_PROXMOX_CONFIG_SEEN[$canonical]+x}" &&
    -n "${_PROXMOX_CONFIG_SEEN[$alternate]+x}" ]]; then
    config_die "Use ${canonical}, not both ${canonical} and legacy alias ${alternate}" || return
  fi
  if [[ -z "${_PROXMOX_CONFIG_SEEN[$canonical]+x}" &&
    -n "${_PROXMOX_CONFIG_SEEN[$alternate]+x}" ]]; then
    printf -v "$canonical" '%s' "${!alternate}"
    export "${canonical?}"
    _PROXMOX_CONFIG_PUBLIC_KEYS+=("$canonical")
    _PROXMOX_CONFIG_ORIGIN["$canonical"]="${_PROXMOX_CONFIG_ORIGIN[$alternate]} (legacy alias ${alternate})"
    _PROXMOX_CONFIG_SCOPE["$canonical"]="${_PROXMOX_CONFIG_SCOPE[$alternate]}"
    _PROXMOX_CONFIG_SOURCE_FILE["$canonical"]="${_PROXMOX_CONFIG_SOURCE_FILE[$alternate]}"
  fi
}

_config_require_from_file() {
  local scope="${1:?configuration scope is required}"
  local file="${2:?configuration source file is required}"
  local key
  shift 2
  for key in "$@"; do
    require_var "$key" || return
    [[ "${_PROXMOX_CONFIG_SCOPE[$key]-}" == "$scope" &&
      "${_PROXMOX_CONFIG_SOURCE_FILE[$key]-}" == "$file" ]] ||
      config_die "Required variable ${key} must originate in ${file}" || return
  done
}

_config_ipv4_octet() {
  local value="${1%/*}" prefix="$2" octet
  if [[ "$value" =~ ^[0-9]+$ ]]; then
    octet="$value"
  elif [[ "$value" =~ ^([0-9]+\.[0-9]+\.[0-9]+)\.([0-9]+)$ &&
    "${BASH_REMATCH[1]}" == "$prefix" ]]; then
    octet="${BASH_REMATCH[2]}"
  else
    return 1
  fi
  [[ "$octet" =~ ^[0-9]+$ ]] && ((10#$octet >= 0 && 10#$octet <= 255)) || return 1
  printf '%d' "$((10#$octet))"
}

_config_validate_ipv4() {
  local value="${1%/*}" IFS=. part count=0
  read -r -a _config_ip_parts <<<"$value"
  ((${#_config_ip_parts[@]} == 4)) || return 1
  for part in "${_config_ip_parts[@]}"; do
    [[ "$part" =~ ^[0-9]+$ ]] || return 1
    ((10#$part >= 0 && 10#$part <= 255)) || return 1
    ((count += 1))
  done
  ((count == 4))
}

_config_validate_positive_integer() {
  [[ "$2" =~ ^[1-9][0-9]*$ ]] ||
    config_die "$1 must be a positive integer"
}

_config_validate_interface() {
  local name="$1" value="${!1}"
  [[ "$value" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]{0,14}$ ]] ||
    config_die "${name} must be a safe Linux interface name (1-15 characters)"
}

_config_validate_sha256() {
  local name="$1"
  [[ "${!name}" =~ ^[[:xdigit:]]{64}$ ]] ||
    config_die "${name} must contain exactly 64 hexadecimal characters"
}

_config_validate_https_iso_url() {
  local name="$1" value="${!1}"
  python3 - "$value" <<'PY' ||
import sys
from urllib.parse import urlsplit

value = sys.argv[1]
if any(ord(character) < 0x20 or character.isspace() for character in value):
    raise SystemExit(1)
parsed = urlsplit(value)
if (
    parsed.scheme != "https"
    or not parsed.hostname
    or parsed.username is not None
    or parsed.password is not None
    or parsed.fragment
    or not parsed.path.lower().endswith(".iso")
):
    raise SystemExit(1)
PY
    config_die "${name} must be a public HTTPS URL ending in .iso"
}

_config_validate_public_key() {
  local name="$1" value="${!1}"
  [[ "$value" =~ ^(ssh-(ed25519|rsa)|ecdsa-sha2-|sk-ssh-|sk-ecdsa-) ]] ||
    config_die "${name} must contain an OpenSSH public key"
}

mox_index() {
  local host="${1:?mox host is required}"
  [[ "$host" =~ ^mox([1-9]|10)$ ]] ||
    config_die "Host must be named mox1 through mox10: ${host}" || return
  printf '%d\n' "${host#mox}"
}

_config_validate_selected_host() {
  local host="$1" index="$2" prefix="$3"
  local mox_start replication_start migration_prefix haproxy_start
  mox_start="$(_config_ipv4_octet "$MOX_IP_START" "$prefix")" || return
  replication_start="$(_config_ipv4_octet "$MOX_REPLICATION_IP_START" "$prefix")" || return
  migration_prefix="${PROXMOX_MIGRATION_NETWORK#*/}"
  haproxy_start="$(_config_ipv4_octet "$HAPROXY_IP_START" "$prefix")" || return
  local expected_mox_ip="${prefix}.$((mox_start + index - 1))/24"
  local expected_replication_ip="${prefix}.$((replication_start + index - 1))/${migration_prefix}"
  local expected_fqdn="${host}.${PROXMOX_INTERNAL_DOMAIN}"
  local expected_haproxy_ip="${prefix}.$((haproxy_start + index - 1))/24"
  local expected_haproxy_vmid="$((9110 + index))"
  local expected_vrrp_priority="$((200 - index))"
  local name

  ((index <= MAX_MOX_HOSTS)) ||
    config_die "${host} exceeds configured MAX_MOX_HOSTS=${MAX_MOX_HOSTS}" || return

  [[ -z "${TAILSCALE_HOSTNAME+x}" || "$TAILSCALE_HOSTNAME" == "$host" ]] ||
    config_die "TAILSCALE_HOSTNAME must equal selected host ${host}" || return
  [[ -z "${PROXMOX_SECONDARY_IP+x}" || "$PROXMOX_SECONDARY_IP" == "$expected_mox_ip" ]] ||
    config_die "PROXMOX_SECONDARY_IP for ${host} must be ${expected_mox_ip}" || return
  [[ -z "${MOX_REPLICATION_IP+x}" || "$MOX_REPLICATION_IP" == "$expected_replication_ip" ]] ||
    config_die "MOX_REPLICATION_IP for ${host} must be ${expected_replication_ip}" || return
  [[ -z "${HAPROXY_LXC_HOSTNAME+x}" || "$HAPROXY_LXC_HOSTNAME" == "haproxy${index}" ]] ||
    config_die "HAPROXY_LXC_HOSTNAME for ${host} must be haproxy${index}" || return
  [[ -z "${HAPROXY_LXC_IP+x}" || "$HAPROXY_LXC_IP" == "$expected_haproxy_ip" ]] ||
    config_die "HAPROXY_LXC_IP for ${host} must be ${expected_haproxy_ip}" || return
  [[ -z "${HAPROXY_LXC_GATEWAY+x}" || "$HAPROXY_LXC_GATEWAY" == "${expected_mox_ip%/*}" ]] ||
    config_die "HAPROXY_LXC_GATEWAY for ${host} must be ${expected_mox_ip%/*}" || return
  [[ -z "${HAPROXY_LXC_VMID+x}" || "$HAPROXY_LXC_VMID" == "$expected_haproxy_vmid" ]] ||
    config_die "HAPROXY_LXC_VMID for ${host} must be ${expected_haproxy_vmid}" || return
  [[ -z "${VRRP_PRIORITY+x}" || "$VRRP_PRIORITY" == "$expected_vrrp_priority" ]] ||
    config_die "VRRP_PRIORITY for ${host} must be ${expected_vrrp_priority}" || return

  MOX_INDEX="$index"
  MOX_HOSTNAME="$host"
  TAILSCALE_HOSTNAME="$host"
  PROXMOX_FQDN="$expected_fqdn"
  PROXMOX_SECONDARY_IP="$expected_mox_ip"
  MOX_REPLICATION_IP="$expected_replication_ip"
  HAPROXY_LXC_HOSTNAME="haproxy${index}"
  HAPROXY_LXC_IP="$expected_haproxy_ip"
  HAPROXY_LXC_GATEWAY="${expected_mox_ip%/*}"
  HAPROXY_LXC_VMID="$expected_haproxy_vmid"
  VRRP_PRIORITY="$expected_vrrp_priority"
  export MOX_INDEX MOX_HOSTNAME TAILSCALE_HOSTNAME PROXMOX_FQDN
  export PROXMOX_SECONDARY_IP MOX_REPLICATION_IP
  export HAPROXY_LXC_HOSTNAME HAPROXY_LXC_IP HAPROXY_LXC_GATEWAY
  export HAPROXY_LXC_VMID VRRP_PRIORITY
  _PROXMOX_CONFIG_PUBLIC_KEYS+=(
    MOX_INDEX MOX_HOSTNAME TAILSCALE_HOSTNAME PROXMOX_FQDN
    PROXMOX_SECONDARY_IP MOX_REPLICATION_IP
    HAPROXY_LXC_HOSTNAME HAPROXY_LXC_IP HAPROXY_LXC_GATEWAY
    HAPROXY_LXC_VMID VRRP_PRIORITY
  )
  for name in MOX_INDEX MOX_HOSTNAME TAILSCALE_HOSTNAME PROXMOX_FQDN \
    PROXMOX_SECONDARY_IP MOX_REPLICATION_IP \
    HAPROXY_LXC_HOSTNAME HAPROXY_LXC_IP HAPROXY_LXC_GATEWAY \
    HAPROXY_LXC_VMID VRRP_PRIORITY; do
    _PROXMOX_CONFIG_DERIVED["$name"]=1
  done
}

_config_validate_mirror_serials() {
  local source_file="${1:?mox configuration file is required}"
  local pair a_name b_name capacity_a_name capacity_b_name
  local prior_missing=0 serial capacity
  local -A serials=()
  for pair in 1 2 3 4 5; do
    a_name="NVME_MIRROR_${pair}_SERIAL_1"
    b_name="NVME_MIRROR_${pair}_SERIAL_2"
    capacity_a_name="NVME_MIRROR_${pair}_CAPACITY_BYTES_1"
    capacity_b_name="NVME_MIRROR_${pair}_CAPACITY_BYTES_2"
    if [[ -n "${!a_name:-}" || -n "${!b_name:-}" ]]; then
      [[ -n "${!a_name:-}" && -n "${!b_name:-}" ]] ||
        config_die "NVMe mirror ${pair} must define both ${a_name} and ${b_name}" || return
      _config_require_from_file mox "$source_file" "$a_name" "$b_name" || return
      ((prior_missing == 0)) ||
        config_die "NVMe mirror pairs must be contiguous; mirror ${pair} follows an omitted pair" || return
      [[ "${!a_name}" != "${!b_name}" ]] ||
        config_die "NVMe mirror ${pair} contains the same serial twice" || return
      for serial in "${!a_name}" "${!b_name}"; do
        [[ -z "${serials[$serial]+x}" ]] ||
          config_die "NVMe serial is reused across mirror pairs: ${serial}" || return
        serials["$serial"]=1
      done
      if [[ -n "${!capacity_a_name:-}" || -n "${!capacity_b_name:-}" ]]; then
        [[ -n "${!capacity_a_name:-}" && -n "${!capacity_b_name:-}" ]] ||
          config_die "NVMe mirror ${pair} must define both ${capacity_a_name} and ${capacity_b_name}" || return
        _config_require_from_file mox "$source_file" \
          "$capacity_a_name" "$capacity_b_name" || return
        for capacity in "${!capacity_a_name}" "${!capacity_b_name}"; do
          [[ "$capacity" =~ ^[1-9][0-9]*$ ]] ||
            config_die "NVMe mirror ${pair} capacities must be positive byte counts" || return
        done
      fi
    else
      ((pair == 1)) &&
        config_die "NVMe mirror 1 is mandatory" && return
      [[ -z "${!capacity_a_name:-}${!capacity_b_name:-}" ]] ||
        config_die "NVMe mirror ${pair} capacities were provided without serials" || return
      prior_missing=1
    fi
  done
}

_config_validate_cluster() {
  local subnet_ip prefix datacenter_gateway vip_octet
  local mox_start mox_end replication_start replication_end
  local haproxy_start haproxy_end
  local prod_start prod_end staging_start staging_end
  _config_alias MAX_MOX_HOSTS PROXMOX_MAX_HOSTS || return
  _config_alias PRIVATE_SUBNET_CIDR PROXMOX_PRIVATE_SUBNET || return
  _config_alias PRODUCTION_IP_START PROD_IP_RANGE_START || return
  _config_alias PRODUCTION_IP_END PROD_IP_RANGE_END || return
  _config_alias STAGING_IP_START STAGING_IP_RANGE_START || return
  _config_alias STAGING_IP_END STAGING_IP_RANGE_END || return
  _config_alias PRODUCTION_VM_TAG PRODUCTION_TAG || return
  _config_alias STAGING_VM_TAG STAGING_TAG || return
  _config_alias EVICTABLE_VM_TAG EVICTABLE_TAG || return

  _config_require_from_file cluster "$PROXMOX_CLUSTER_CONFIG" \
    PROXMOX_INTERNAL_DOMAIN MAX_MOX_HOSTS PRIVATE_SUBNET_CIDR \
    PROXMOX_MIGRATION_NETWORK \
    DATACENTER_PRIVATE_VLAN_GATEWAY GUEST_EGRESS_VIP \
    MOX_IP_START MOX_IP_END \
    MOX_REPLICATION_IP_START MOX_REPLICATION_IP_END \
    HAPROXY_IP_START HAPROXY_IP_END \
    PRODUCTION_IP_START PRODUCTION_IP_END STAGING_IP_START STAGING_IP_END || return
  [[ "$PROXMOX_INTERNAL_DOMAIN" =~ ^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?([.][a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?)*[.]internal$ ]] ||
    config_die "PROXMOX_INTERNAL_DOMAIN must be a lowercase private domain ending in .internal" || return
  [[ "$MAX_MOX_HOSTS" =~ ^([1-9]|10)$ ]] ||
    config_die "MAX_MOX_HOSTS must be between 1 and 10" || return
  [[ "$PRIVATE_SUBNET_CIDR" =~ ^([0-9]+\.[0-9]+\.[0-9]+)\.0/24$ ]] ||
    config_die "PRIVATE_SUBNET_CIDR must be an IPv4 /24 ending in .0" || return
  prefix="${BASH_REMATCH[1]}"
  subnet_ip="${PRIVATE_SUBNET_CIDR%/*}"
  _config_validate_ipv4 "$subnet_ip" ||
    config_die "PRIVATE_SUBNET_CIDR is not valid IPv4" || return
  datacenter_gateway="$(_config_ipv4_octet "$DATACENTER_PRIVATE_VLAN_GATEWAY" "$prefix")" ||
    config_die "DATACENTER_PRIVATE_VLAN_GATEWAY must belong to ${prefix}.0/24" || return
  vip_octet="$(_config_ipv4_octet "$GUEST_EGRESS_VIP" "$prefix")" || return
  mox_start="$(_config_ipv4_octet "$MOX_IP_START" "$prefix")" ||
    config_die "MOX_IP_START must belong to ${prefix}.0/24" || return
  mox_end="$(_config_ipv4_octet "$MOX_IP_END" "$prefix")" ||
    config_die "MOX_IP_END must belong to ${prefix}.0/24" || return
  replication_start="$(_config_ipv4_octet "$MOX_REPLICATION_IP_START" "$prefix")" ||
    config_die "MOX_REPLICATION_IP_START must belong to ${prefix}.0/24" || return
  replication_end="$(_config_ipv4_octet "$MOX_REPLICATION_IP_END" "$prefix")" ||
    config_die "MOX_REPLICATION_IP_END must belong to ${prefix}.0/24" || return
  haproxy_start="$(_config_ipv4_octet "$HAPROXY_IP_START" "$prefix")" ||
    config_die "HAPROXY_IP_START must belong to ${prefix}.0/24" || return
  haproxy_end="$(_config_ipv4_octet "$HAPROXY_IP_END" "$prefix")" ||
    config_die "HAPROXY_IP_END must belong to ${prefix}.0/24" || return
  prod_start="$(_config_ipv4_octet "$PRODUCTION_IP_START" "$prefix")" ||
    config_die "PRODUCTION_IP_START must belong to ${prefix}.0/24" || return
  prod_end="$(_config_ipv4_octet "$PRODUCTION_IP_END" "$prefix")" ||
    config_die "PRODUCTION_IP_END must belong to ${prefix}.0/24" || return
  staging_start="$(_config_ipv4_octet "$STAGING_IP_START" "$prefix")" ||
    config_die "STAGING_IP_START must belong to ${prefix}.0/24" || return
  staging_end="$(_config_ipv4_octet "$STAGING_IP_END" "$prefix")" ||
    config_die "STAGING_IP_END must belong to ${prefix}.0/24" || return
  ((datacenter_gateway >= 1 && datacenter_gateway <= 254)) ||
    config_die "DATACENTER_PRIVATE_VLAN_GATEWAY must be a usable host address" || return
  ((vip_octet >= 1 && vip_octet <= 254)) ||
    config_die "GUEST_EGRESS_VIP must be a usable host address" || return
  ((mox_start >= 1 && mox_start <= mox_end && mox_end <= 254)) ||
    config_die "MOX_IP_START through MOX_IP_END must be a usable ascending range" || return
  ((replication_start >= 1 && replication_start <= replication_end && replication_end <= 254)) ||
    config_die "MOX_REPLICATION_IP_START through MOX_REPLICATION_IP_END must be a usable ascending range" || return
  ((haproxy_start >= 1 && haproxy_start <= haproxy_end && haproxy_end <= 254)) ||
    config_die "HAPROXY_IP_START through HAPROXY_IP_END must be a usable ascending range" || return
  ((prod_start >= 1 && prod_start <= prod_end && prod_end <= 254)) ||
    config_die "PRODUCTION_IP_START through PRODUCTION_IP_END must be a usable ascending range" || return
  ((staging_start >= 1 && staging_start <= staging_end && staging_end <= 254)) ||
    config_die "STAGING_IP_START through STAGING_IP_END must be a usable ascending range" || return
  ((mox_end - mox_start + 1 >= MAX_MOX_HOSTS)) ||
    config_die "The mox range must contain at least MAX_MOX_HOSTS addresses" || return
  ((replication_end - replication_start + 1 >= MAX_MOX_HOSTS)) ||
    config_die "The mox replication range must contain at least MAX_MOX_HOSTS addresses" || return
  ((haproxy_end - haproxy_start + 1 >= MAX_MOX_HOSTS)) ||
    config_die "The HAProxy range must contain at least MAX_MOX_HOSTS addresses" || return

  local -a allocation_octets=(
    "$datacenter_gateway" "$vip_octet"
    "$mox_start" "$mox_end" "$replication_start" "$replication_end"
    "$haproxy_start" "$haproxy_end"
    "$prod_start" "$prod_end" "$staging_start" "$staging_end"
  )
  python3 - "${allocation_octets[@]}" <<'PY' ||
import sys

gateway, vip, ms, me, rs, re, hs, he, ps, pe, ss, se = map(int, sys.argv[1:])
ranges = {
    "mox": set(range(ms, me + 1)),
    "mox replication": set(range(rs, re + 1)),
    "HAProxy": set(range(hs, he + 1)),
    "production": set(range(ps, pe + 1)),
    "staging": set(range(ss, se + 1)),
}
for address_name, address in (("data-center gateway", gateway), ("guest-egress VIP", vip)):
    for range_name, values in ranges.items():
        if address in values:
            raise SystemExit(f"{address_name} overlaps the {range_name} range")
names = list(ranges)
for index, name in enumerate(names):
    for other in names[index + 1:]:
        if ranges[name] & ranges[other]:
            raise SystemExit(f"{name} and {other} ranges overlap")
if gateway == vip:
    raise SystemExit("data-center gateway and guest-egress VIP overlap")
PY
    config_die "Private VLAN address roles must not overlap" || return

  python3 - "$PROXMOX_MIGRATION_NETWORK" \
    "$MOX_REPLICATION_IP_START" "$MOX_REPLICATION_IP_END" \
    "$MOX_IP_START" "$MOX_IP_END" "$GUEST_EGRESS_VIP" <<'PY' ||
import ipaddress
import sys

network = ipaddress.ip_network(sys.argv[1], strict=True)
start, end = (ipaddress.ip_address(value) for value in sys.argv[2:4])
normal_start, normal_end = (ipaddress.ip_address(value) for value in sys.argv[4:6])
vip = ipaddress.ip_interface(sys.argv[6]).ip
if start not in network or end not in network:
    raise SystemExit("replication address range is outside PROXMOX_MIGRATION_NETWORK")
if any(
    address in network
    for address in (normal_start, normal_end, vip)
):
    raise SystemExit("migration network also matches a normal host or floating VIP address")
PY
    config_die "Dedicated migration/replication network is invalid" || return

  PRIVATE_SUBNET_PREFIX="$prefix"
  MOX_IP_START_OCTET="$mox_start"
  HAPROXY_IP_START_OCTET="$haproxy_start"
  export PRIVATE_SUBNET_PREFIX MOX_IP_START_OCTET HAPROXY_IP_START_OCTET
  _PROXMOX_CONFIG_PUBLIC_KEYS+=(
    PRIVATE_SUBNET_PREFIX MOX_IP_START_OCTET HAPROXY_IP_START_OCTET
  )
  _PROXMOX_CONFIG_DERIVED["PRIVATE_SUBNET_PREFIX"]=1
  _PROXMOX_CONFIG_DERIVED["MOX_IP_START_OCTET"]=1
  _PROXMOX_CONFIG_DERIVED["HAPROXY_IP_START_OCTET"]=1

  local name
  for name in PROXMOX_ISO_FILE_SHA256 PROD_GUEST_OS_ISO_SHA256; do
    [[ -z "${!name+x}" ]] || _config_validate_sha256 "$name" || return
  done
  [[ -z "${PROD_GUEST_OS_ISO_URL+x}" ]] ||
    _config_validate_https_iso_url PROD_GUEST_OS_ISO_URL || return
  if [[ -n "${PROD_GUEST_OS_INSTALL_MODE+x}" ]]; then
    case "$PROD_GUEST_OS_INSTALL_MODE" in
      ubuntu-autoinstall | manual) ;;
      *)
        config_die \
          "PROD_GUEST_OS_INSTALL_MODE must be ubuntu-autoinstall or manual" ||
          return
        ;;
    esac
  fi
  for name in ADMIN_1_PUBLIC_SSH_KEY ADMIN_2_PUBLIC_SSH_KEY; do
    [[ -z "${!name+x}" ]] || _config_validate_public_key "$name" || return
  done
  for name in HAPROXY_LXC_ROOTFS_GB HAPROXY_LXC_MEMORY_MB HAPROXY_LXC_CORES \
    PROD_VM_CORES PROD_VM_MEMORY_GIB PROD_VM_DISK_GB \
    STAGING_VM_CORES STAGING_VM_MEMORY_GIB STAGING_VM_DISK_GB \
    MOX_SSH_CONNECT_TIMEOUT; do
    [[ -z "${!name+x}" ]] || _config_validate_positive_integer "$name" "${!name}" || return
  done
  for name in PROXMOX_PRIVATE_BRIDGE PROD_VM_BRIDGE STAGING_VM_BRIDGE; do
    [[ -z "${!name+x}" ]] || _config_validate_interface "$name" || return
  done
}

_config_validate_mox() {
  local host="$1" index prefix="$PRIVATE_SUBNET_PREFIX"
  local source_file="${PROXMOX_ENV_DIR}/${host}.conf"
  index="$(mox_index "$host")" || return
  _config_require_from_file mox "$source_file" \
    PROXMOX_IP PROXMOX_GATEWAY PROXMOX_PREFIX \
    PROXMOX_PUBLIC_MAC PROXMOX_SECONDARY_MAC \
    MAX_PROD_VM_COUNT_ON_THIS_HOST MAX_STAGING_VM_COUNT_ON_THIS_HOST || return
  local name
  for name in \
    MAX_PROD_VM_COUNT_ON_THIS_HOST MAX_STAGING_VM_COUNT_ON_THIS_HOST; do
    [[ "${!name}" =~ ^[1-9][0-9]*$ ]] ||
      config_die "${name} must be a positive integer" || return
  done
  if [[ -n "${IDRAC_IP:-}" ]]; then
    _config_require_from_file mox "$source_file" IDRAC_IP || return
    _config_validate_ipv4 "$IDRAC_IP" ||
      config_die "IDRAC_IP is not valid IPv4" || return
  fi
  _config_validate_ipv4 "$PROXMOX_IP" || config_die "PROXMOX_IP is not valid IPv4" || return
  _config_validate_ipv4 "$PROXMOX_GATEWAY" || config_die "PROXMOX_GATEWAY is not valid IPv4" || return
  [[ "$PROXMOX_PREFIX" =~ ^([1-9]|[12][0-9]|3[0-2])$ ]] ||
    config_die "PROXMOX_PREFIX must be between 1 and 32" || return
  [[ "${PROXMOX_PUBLIC_MAC^^}" =~ ^([0-9A-F]{2}:){5}[0-9A-F]{2}$ ]] ||
    config_die "PROXMOX_PUBLIC_MAC is not a MAC address" || return
  [[ "$PROXMOX_SECONDARY_MAC" == auto ||
    "${PROXMOX_SECONDARY_MAC^^}" =~ ^([0-9A-F]{2}:){5}[0-9A-F]{2}$ ]] ||
    config_die "PROXMOX_SECONDARY_MAC must be a MAC address or auto" || return
  [[ "$PROXMOX_SECONDARY_MAC" == auto ||
    "${PROXMOX_PUBLIC_MAC^^}" != "${PROXMOX_SECONDARY_MAC^^}" ]] ||
    config_die "Public and private NIC MAC addresses must differ" || return
  _config_validate_mirror_serials "$source_file" || return
  _config_validate_selected_host "$host" "$index" "$prefix"
}

_config_compute_hash() {
  local key
  local -a keys=()
  local -A secret_keys=()
  local canonical
  require_command sha256sum sort || return
  for key in "${_PROXMOX_CONFIG_SECRET_KEYS[@]}"; do
    secret_keys["$key"]=1
  done
  for key in "${_PROXMOX_CONFIG_PUBLIC_KEYS[@]}"; do
    [[ -z "${secret_keys[$key]+x}" ]] ||
      config_die "Internal error: secret key ${key} entered the public hash set" || return
    keys+=("$key")
  done
  mapfile -t keys < <(printf '%s\n' "${keys[@]}" | LC_ALL=C sort -u)
  canonical="$(
    for key in "${keys[@]}"; do
      printf '%s=%q\n' "$key" "${!key-}"
    done
  )"
  CONFIG_EFFECTIVE_SHA256="$(
    printf '%s\n' "$canonical" | sha256sum | awk '{print $1}'
  )"
  export CONFIG_EFFECTIVE_SHA256
  unset canonical
}

load_proxmox_config() {
  local selected_host="" secrets_mode="optional"
  local cluster_file="$PROXMOX_CLUSTER_CONFIG" host_file=""
  local key xtrace_was_on=0

  while (($#)); do
    case "$1" in
      --host)
        (($# >= 2)) || config_die "--host requires moxN" || return
        selected_host="$2"
        shift 2
        ;;
      --require-secrets)
        secrets_mode="required"
        shift
        ;;
      --no-secrets)
        secrets_mode="disabled"
        shift
        ;;
      *)
        if [[ -z "$selected_host" && "$1" =~ ^mox([1-9]|10)$ ]]; then
          selected_host="$1"
          shift
        else
          config_die "Unknown load_proxmox_config argument: $1" || return
        fi
        ;;
    esac
  done

  local -A keys_to_clear=()
  for key in "${!_PROXMOX_CONFIG_ALLOWED_SCOPE[@]}" \
    "${!_PROXMOX_CONFIG_SEEN[@]}" \
    "${_PROXMOX_CONFIG_PUBLIC_KEYS[@]}" \
    "${_PROXMOX_CONFIG_SECRET_KEYS[@]}" \
    PROXMOX_FQDN \
    CONFIG_EFFECTIVE_SHA256 CONFIG_SELECTED_HOST CONFIG_CLUSTER_FILE \
    CONFIG_HOST_FILE CONFIG_SECRETS_LOADED CONFIG_LOADED; do
    [[ -n "$key" ]] && keys_to_clear["$key"]=1
  done
  for key in "${!keys_to_clear[@]}"; do
    unset "$key" 2>/dev/null ||
      config_die "Configuration variable ${key} is readonly and cannot be safely reset" || return
  done
  _PROXMOX_CONFIG_SEEN=()
  _PROXMOX_CONFIG_ORIGIN=()
  _PROXMOX_CONFIG_SCOPE=()
  _PROXMOX_CONFIG_SOURCE_FILE=()
  _PROXMOX_CONFIG_DERIVED=()
  _PROXMOX_CONFIG_PUBLIC_KEYS=()
  _PROXMOX_CONFIG_SECRET_KEYS=()

  _config_parse_file "$cluster_file" cluster 0 || return
  _config_validate_cluster || return

  if [[ -n "$selected_host" ]]; then
    mox_index "$selected_host" >/dev/null || return
    host_file="${PROXMOX_ENV_DIR}/${selected_host}.conf"
    _config_parse_file "$host_file" mox 0 || return
    _config_validate_mox "$selected_host" || return
  fi

  if [[ "$secrets_mode" != disabled ]]; then
    if [[ -e "$PROXMOX_SECRETS_CONFIG" || -L "$PROXMOX_SECRETS_CONFIG" ]]; then
      [[ "$-" == *x* ]] && {
        xtrace_was_on=1
        set +x
      }
      _config_parse_file "$PROXMOX_SECRETS_CONFIG" secrets 1 || return
      if [[ "${APP_HA_CONFIG_TEST_MODE:-0}" != 1 ]]; then
        require_command git || return
        git -C "$PROXMOX_REPO_ROOT" check-ignore -q -- "$PROXMOX_SECRETS_CONFIG" ||
          config_die "Secret configuration is not ignored by Git: ${PROXMOX_SECRETS_CONFIG}" || return
      fi
      ((xtrace_was_on == 0)) || set -x
    elif [[ "$secrets_mode" == required ]]; then
      config_die "Required secret configuration is missing: ${PROXMOX_SECRETS_CONFIG}" || return
    fi
  fi

  _config_compute_hash || return
  CONFIG_SELECTED_HOST="$selected_host"
  CONFIG_CLUSTER_FILE="$cluster_file"
  CONFIG_HOST_FILE="$host_file"
  CONFIG_SECRETS_LOADED=0
  ((${#_PROXMOX_CONFIG_SECRET_KEYS[@]} == 0)) || CONFIG_SECRETS_LOADED=1
  CONFIG_LOADED=1
  export CONFIG_SELECTED_HOST CONFIG_CLUSTER_FILE CONFIG_HOST_FILE
  export CONFIG_SECRETS_LOADED CONFIG_LOADED
}

load_config() {
  load_proxmox_config "$@"
}

_validate_mox_for_ssh() {
  local host="$1" index
  index="$(mox_index "$host")" || return
  if [[ -n "${MAX_MOX_HOSTS:-}" ]]; then
    ((index <= MAX_MOX_HOSTS)) ||
      config_die "${host} exceeds configured MAX_MOX_HOSTS=${MAX_MOX_HOSTS}" || return
  fi
}

_mox_known_hosts_file() {
  local host="${1:?mox host is required}"
  local path="${PROXMOX_SSH_KNOWN_HOSTS_FILE:-}"
  if [[ -z "$path" ]]; then
    if [[ -f "/etc/pve/nodes/${host}/ssh_known_hosts" &&
          ! -L "/etc/pve/nodes/${host}/ssh_known_hosts" ]]; then
      path="/etc/pve/nodes/${host}/ssh_known_hosts"
    else
      [[ -n "${HOME:-}" ]] ||
        config_die "HOME is required to locate explicit SSH host trust" || return
      path="${HOME}/.ssh/known_hosts"
    fi
  fi
  [[ "$path" == /* && "$path" != *$'\n'* && "$path" != *$'\r'* ]] ||
    config_die "PROXMOX_SSH_KNOWN_HOSTS_FILE must be a safe absolute path" ||
    return
  [[ -f "$path" && ! -L "$path" ]] ||
    config_die "Explicit SSH known-hosts file is unavailable or unsafe: ${path}" ||
    return
  local mode
  mode="$(_config_stat_mode "$path")" ||
    config_die "Cannot inspect SSH known-hosts file: ${path}" || return
  (( (8#$mode & 022) == 0 )) ||
    config_die "SSH known-hosts file must not be group/world writable: ${path}" ||
    return
  printf '%s\n' "$path"
}

_mox_ssh_options() {
  local host="${1:?mox host is required}"
  local known_hosts user="${MOX_SSH_USER:-root}"
  [[ "$user" =~ ^[A-Za-z_][A-Za-z0-9_-]*$ ]] ||
    config_die "MOX_SSH_USER is unsafe: ${user}" || return
  known_hosts="$(_mox_known_hosts_file "$host")" || return
  MOX_SSH_OPTIONS=(
    -o BatchMode=yes
    -o ClearAllForwardings=yes
    -o ConnectTimeout="${MOX_SSH_CONNECT_TIMEOUT:-8}"
    -o ServerAliveInterval=15
    -o ServerAliveCountMax=2
    -o StrictHostKeyChecking=yes
    -o "UserKnownHostsFile=${known_hosts}"
    -o GlobalKnownHostsFile=none
  )
  if [[ "$known_hosts" == "/etc/pve/nodes/${host}/ssh_known_hosts" ]]; then
    MOX_SSH_OPTIONS+=(-o CheckHostIP=no -o "HostKeyAlias=${host}")
  else
    MOX_SSH_OPTIONS+=(-o CheckHostIP=yes)
  fi
}

_mox_ssh_destination() {
  local host="${1:?mox host is required}"
  if [[ -f "/etc/pve/nodes/${host}/ssh_known_hosts" &&
        ! -L "/etc/pve/nodes/${host}/ssh_known_hosts" ]]; then
    require_var PROXMOX_INTERNAL_DOMAIN || return
    printf '%s.%s\n' "$host" "$PROXMOX_INTERNAL_DOMAIN"
  else
    printf '%s\n' "$host"
  fi
}

_quote_remote_argv() {
  (($# > 0)) || config_die "A remote command is required" || return
  REMOTE_SSH_COMMAND=()
  local argument quoted
  for argument in "$@"; do
    printf -v quoted '%q' "$argument"
    REMOTE_SSH_COMMAND+=("$quoted")
  done
}

mox_is_reachable() {
  local host="${1:?mox host is required}" destination
  _validate_mox_for_ssh "$host" || return
  _mox_ssh_options "$host" || return
  destination="$(_mox_ssh_destination "$host")" || return
  ssh "${MOX_SSH_OPTIONS[@]}" "${MOX_SSH_USER:-root}@${destination}" true </dev/null >/dev/null 2>&1
}

reachable_mox_hosts() {
  local -a candidates=("$@")
  local host index
  if ((${#candidates[@]} == 0)); then
    require_var MAX_MOX_HOSTS || return
    for ((index = 1; index <= MAX_MOX_HOSTS; index += 1)); do
      candidates+=("mox${index}")
    done
  fi
  for host in "${candidates[@]}"; do
    _validate_mox_for_ssh "$host" || return
    mox_is_reachable "$host" && printf '%s\n' "$host"
  done
}

first_reachable_mox() {
  local host
  while IFS= read -r host; do
    [[ -n "$host" ]] || continue
    printf '%s\n' "$host"
    return 0
  done < <(reachable_mox_hosts "$@")
  config_die "No reachable mox jump host was found"
}

select_reachable_mox() {
  first_reachable_mox "$@"
}

mox_ssh() {
  local host="${1:?mox host is required}" destination
  shift
  _validate_mox_for_ssh "$host" || return
  _mox_ssh_options "$host" || return
  destination="$(_mox_ssh_destination "$host")" || return
  _quote_remote_argv "$@" || return
  # shellcheck disable=SC2029 # REMOTE_SSH_COMMAND entries are Bash-%q escaped.
  ssh "${MOX_SSH_OPTIONS[@]}" "${MOX_SSH_USER:-root}@${destination}" \
    "${REMOTE_SSH_COMMAND[@]}"
}

ssh_via_mox() {
  local jump_host="${1:?jump host is required}"
  local target="${2:?target is required}"
  shift 2
  _validate_mox_for_ssh "$jump_host" || return
  [[ "$target" =~ ^([A-Za-z_][A-Za-z0-9_-]*@)?[A-Za-z0-9._:-]+$ ]] ||
    config_die "Unsafe SSH target: ${target}" || return
  local jump_destination
  _mox_ssh_options "$jump_host" || return
  jump_destination="$(_mox_ssh_destination "$jump_host")" || return
  [[ "$target" == *@* ]] || target="root@${target}"
  if (($# == 0)); then
    ssh "${MOX_SSH_OPTIONS[@]}" \
      -o "ProxyJump=${MOX_SSH_USER:-root}@${jump_destination}" "$target"
    return
  fi
  _quote_remote_argv "$@" || return
  ssh "${MOX_SSH_OPTIONS[@]}" \
    -o "ProxyJump=${MOX_SSH_USER:-root}@${jump_destination}" "$target" \
    "${REMOTE_SSH_COMMAND[@]}"
}

guest_ssh_via_mox() {
  local target="${1:?guest target is required}"
  shift
  local jump_host
  jump_host="$(first_reachable_mox)" || return
  ssh_via_mox "$jump_host" "$target" "$@"
}

scp_via_mox() {
  local jump_host="${1:?jump host is required}"
  shift
  _validate_mox_for_ssh "$jump_host" || return
  local jump_destination
  _mox_ssh_options "$jump_host" || return
  jump_destination="$(_mox_ssh_destination "$jump_host")" || return
  scp "${MOX_SSH_OPTIONS[@]}" \
    -o "ProxyJump=${MOX_SSH_USER:-root}@${jump_destination}" "$@"
}

config_usage() {
  cat <<'EOF'
Usage:
  config.sh --check [--host moxN] [--require-secrets | --no-secrets]
  source config.sh; load_proxmox_config [--host moxN] [--require-secrets]

Strictly parses env/cluster.conf, an optional selected
moxN.conf, and the Git-ignored secrets.env. It never evaluates configuration
as shell code and never prints secret values.

Options:
  --check             Validate configuration (default action).
  --host moxN         Load and validate one host layer (mox1 through mox10).
  --require-secrets   Fail if secrets.env is absent.
  --no-secrets        Do not open secrets.env.
  -h, --help          Show this help.

Successful checks print only the selected host, derived addressing, whether a
secret layer was loaded, and the non-secret effective-configuration hash.
EOF
}

config_main() {
  local host="" secrets_arg=""
  while (($#)); do
    case "$1" in
      --check)
        shift
        ;;
      --host)
        (($# >= 2)) || config_die "--host requires moxN" || return
        host="$2"
        shift 2
        ;;
      --require-secrets | --no-secrets)
        [[ -z "$secrets_arg" ]] ||
          config_die "Choose only one secret-loading mode" || return
        secrets_arg="$1"
        shift
        ;;
      -h | --help)
        config_usage
        return 0
        ;;
      *)
        config_die "Unknown option: $1" || return
        ;;
    esac
  done

  local -a load_args=()
  [[ -z "$host" ]] || load_args+=(--host "$host")
  [[ -z "$secrets_arg" ]] || load_args+=("$secrets_arg")
  load_proxmox_config "${load_args[@]}" || return
  config_info "Configuration valid."
  config_info "Selected host: ${CONFIG_SELECTED_HOST:-none}"
  if [[ -n "${CONFIG_SELECTED_HOST:-}" ]]; then
    config_info "Derived mox FQDN: ${PROXMOX_FQDN}"
    config_info "Derived mox address: ${PROXMOX_SECONDARY_IP}"
    config_info "Derived migration/replication address: ${MOX_REPLICATION_IP}"
    config_info "Derived HAProxy address/VMID: ${HAPROXY_LXC_IP} / ${HAPROXY_LXC_VMID}"
  fi
  config_info "Secret layer loaded: ${CONFIG_SECRETS_LOADED}"
  config_info "Effective non-secret SHA-256: ${CONFIG_EFFECTIVE_SHA256}"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  set -Eeuo pipefail
  config_main "$@"
fi
