#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Strict, non-evaluating loader for the Proxmox orchestration configuration.
#
# This file is both a sourceable Bash library and a small validation command.
# It deliberately does not enable shell options in a caller that sources it.

# mapfile -d, which this library's callers rely on, arrived in Bash 4.4.
if ((BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && BASH_VERSINFO[1] < 4))); then
  printf 'ERROR: scripts/lib/config.sh requires Bash 4.4 or newer (found %s).\n' \
    "${BASH_VERSION:-unknown}" >&2
  # shellcheck disable=SC2317 # exit is the executable-script fallback.
  return 2 2>/dev/null || exit 2
fi

# This file is scripts/lib/config.sh under the runtime root, which bmacd
# runs every script from (/usr/lib/bmac/legacy on a host).
PROXMOX_LIB_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
PROXMOX_REPO_ROOT="$(cd -- "${PROXMOX_LIB_DIR}/../.." && pwd -P)"
# The prompt helpers below ask through the Web Dashboard in JSON mode.
if ! declare -F bmac_ui_is_json >/dev/null; then
  # shellcheck source=ui_protocol.sh
  source "${PROXMOX_LIB_DIR}/ui_protocol.sh"
fi

# bmacd renders cluster.conf and the moxN.conf files from the cluster
# settings in pmxcfs into /run/bmac/config/ before each script runs, and
# /etc/bmac/ssh_config, which names every other host. Tests may point both
# at synthetic files; production callers cannot redirect them through an
# inherited value.
if [[ "${BMAC_CONFIG_TEST_MODE:-0}" == 1 ]]; then
  PROXMOX_CONFIG_DIR="${BMAC_CONFIG_DIR:?BMAC_CONFIG_DIR is required in test mode}"
  BMAC_SSH_CONFIG="${BMAC_SSH_CONFIG:-${PROXMOX_CONFIG_DIR}/ssh_config}"
else
  PROXMOX_CONFIG_DIR=/run/bmac/config
  BMAC_SSH_CONFIG=/etc/bmac/ssh_config
fi
# Relative paths in the configuration are anchored at the runtime root.
PROXMOX_USER_ROOT="$PROXMOX_REPO_ROOT"

PROXMOX_CLUSTER_CONFIG="${PROXMOX_CONFIG_DIR}/cluster.conf"

declare -Ag _PROXMOX_CONFIG_SEEN=()
declare -Ag _PROXMOX_CONFIG_ORIGIN=()
declare -Ag _PROXMOX_CONFIG_SCOPE=()
declare -Ag _PROXMOX_CONFIG_SOURCE_FILE=()
declare -Ag _PROXMOX_CONFIG_DERIVED=()
declare -ag _PROXMOX_CONFIG_PUBLIC_KEYS=()
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
  PROXMOX_ISO_FILE_URL PROXMOX_ISO_FILE_SHA256
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
  PROD_VM_BRIDGE PROD_VM_CPU_TYPE
  PROD_VM_REPLICATION_INTERVAL
  STAGING_VM_CORES STAGING_VM_MEMORY_GIB STAGING_VM_DISK_GB
  STAGING_VM_BRIDGE
  ISO_STORAGE_ID
  SNIPPETS_STORAGE_ID DNS_SEARCH_DOMAIN GUEST_DNS_SERVERS
  MOX_SSH_CONNECT_TIMEOUT
)
declare -ag _PROXMOX_CONFIG_MOX_KEYS=(
  IDRAC_IP IDRAC_ASSET_ID
  PROXMOX_IP PROXMOX_GATEWAY PROXMOX_PREFIX
  PROXMOX_PUBLIC_MAC PROXMOX_SECONDARY_MAC
  PROXMOX_SECONDARY_IP PROXMOX_SECONDARY_GATEWAY PROXMOX_SECONDARY_GATEWAY_ROUTE
  TAILSCALE_HOSTNAME
  HAPROXY_LXC_VMID HAPROXY_LXC_HOSTNAME HAPROXY_LXC_IP
  HAPROXY_LXC_GATEWAY VRRP_PRIORITY
  MAX_PROD_VM_COUNT_ON_THIS_HOST MAX_STAGING_VM_COUNT_ON_THIS_HOST
)

_config_initialize_allowlist() {
  local key pair member suffix
  for key in "${_PROXMOX_CONFIG_CLUSTER_KEYS[@]}"; do
    _PROXMOX_CONFIG_ALLOWED_SCOPE["$key"]=cluster
  done
  for key in "${_PROXMOX_CONFIG_MOX_KEYS[@]}"; do
    _PROXMOX_CONFIG_ALLOWED_SCOPE["$key"]=mox
  done
  for pair in 0 1 2 3 4; do
    for member in 1 2; do
      for suffix in SERIAL CAPACITY_BYTES; do
        key="NVME_MIRROR_${pair}_${suffix}_${member}"
        _PROXMOX_CONFIG_MOX_KEYS+=("$key")
        _PROXMOX_CONFIG_ALLOWED_SCOPE["$key"]=mox
      done
    done
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
  if bmac_ui_is_json; then
    bmac_ui_ask "$prompt"
    return
  fi
  IFS= read -r -p "${prompt} [y/N] " answer
  [[ "${answer,,}" == y || "${answer,,}" == yes ]]
}

require_yes() {
  if bmac_ui_is_json; then
    bmac_ui_manual_action --id prerequisite --title "Confirm a prerequisite" \
      --instruction "$1" --ack-label "Yes, this is done"
    return
  fi
  prompt_yes "$1" || config_die "Prerequisite was not confirmed."
}

# confirm_exact PROMPT [LEGACY] [SEVERITY]: in JSON mode SEVERITY styles the
# dashboard request, which requires typing GO (see bmac_ui_confirm_go).
confirm_exact() {
  local prompt="${1:?prompt is required}" _legacy_phrase="${2:-}" entered
  if bmac_ui_is_json; then
    bmac_ui_confirm_go "$prompt" "${3:-}" ||
      config_die "Confirmation did not match GO; no action was taken."
    return
  fi
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
  if bmac_ui_is_json; then
    bmac_ui_input value --id "${destination,,}" --label "$prompt" --required
  else
    IFS= read -r -p "${prompt}: " value
  fi
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
  if bmac_ui_is_json; then
    bmac_ui_input value --id "${destination,,}" --type password \
      --label "$prompt" --required
  else
    IFS= read -r -s -p "${prompt}: " value
    printf '\n'
  fi
  [[ -n "$value" ]] || config_die "${destination} may not be empty" || return
  printf -v "$destination" '%s' "$value"
  export -n "${destination?}"
  unset value
}

prompt_tailscale_auth_key() {
  local value
  unset TAILSCALE_AUTH_KEY 2>/dev/null ||
    config_die "TAILSCALE_AUTH_KEY is readonly and cannot be reset" || return
  if bmac_ui_is_json; then
    bmac_ui_input value --id tailscale_auth_key --type password \
      --label "Fresh one-time Tailscale auth key" --required \
      --placeholder tskey-auth-... \
      --help "Create a one-time key in the Tailscale admin console. It is passed to the script only."
  else
    IFS= read -r -s -p "Fresh one-time Tailscale auth key: " value
    printf '\n'
  fi
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

# Print FILE's permission bits as "stat -c %a" does (600, 4600, 7). The raw
# output is validated in full before any arithmetic, so a surprising stat can
# never be read as some mode.
_config_stat_mode() {
  local file="$1" raw
  raw="$(stat -c '%a' -- "$file")" || return 1
  [[ "$raw" =~ ^[0-7]{1,4}$ ]] || return 1
  printf '%o\n' "$((8#$raw))"
}

_config_stat_uid() {
  local file="$1" raw
  raw="$(stat -c '%u' -- "$file")" || return 1
  [[ "$raw" =~ ^[0-9]+$ ]] || return 1
  printf '%s\n' "$raw"
}

_config_allowed_owner() {
  local owner_uid="$1"
  [[ "$owner_uid" == 0 || "$owner_uid" == "$EUID" ]]
}

_config_validate_file_security() {
  local file="$1" mode owner_uid
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
  # bmacd writes every file 0600; anything else wasn't written by it.
  [[ "$mode" == 600 ]] ||
    config_die "Configuration must have mode 0600 (found ${mode}): ${file}" || return
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
  local file="$1" scope="$2"
  local line trimmed key raw value line_number=0
  _config_validate_file_security "$file" || return

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
    [[ "$key" != PROXMOX_ISO_FILE_PATH ]] ||
      config_die "${file}:${line_number}: PROXMOX_ISO_FILE_PATH was replaced by PROXMOX_ISO_FILE_URL, the HTTPS download URL of the Proxmox VE installer ISO" || return
    _config_key_allowed "$scope" "$key" ||
      config_die "${file}:${line_number}: unknown or misplaced key ${key}" || return
    [[ -z "${_PROXMOX_CONFIG_SEEN[$key]+x}" ]] ||
      config_die "${file}:${line_number}: duplicate key ${key} (first seen in ${_PROXMOX_CONFIG_ORIGIN[$key]})" || return
    value="$(_config_parse_value "$raw")" || return

    printf -v "$key" '%s' "$value" ||
      config_die "${file}:${line_number}: cannot assign configuration key ${key}" || return
    export "${key?}"
    _PROXMOX_CONFIG_PUBLIC_KEYS+=("$key")
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

# _config_ipv4_int ADDRESS[/PREFIX]: the address as a 32-bit integer.
_config_ipv4_int() {
  _config_validate_ipv4 "$1" || return 1
  printf '%d' "$(((10#${_config_ip_parts[0]} << 24) | (10#${_config_ip_parts[1]} << 16) |
    (10#${_config_ip_parts[2]} << 8) | 10#${_config_ip_parts[3]}))"
}

_config_ipv4_text() {
  printf '%d.%d.%d.%d' "$((($1 >> 24) & 255))" "$((($1 >> 16) & 255))" \
    "$((($1 >> 8) & 255))" "$(($1 & 255))"
}

# _config_private_offset VALUE: VALUE's offset from PRIVATE_SUBNET_CIDR's
# network address. VALUE is an address in that network, or a bare offset.
_config_private_offset() {
  local value="${1%/*}" network length offset
  network="$(_config_ipv4_int "${PRIVATE_SUBNET_CIDR%/*}")" || return 1
  length="${PRIVATE_SUBNET_CIDR#*/}"
  if [[ "$value" =~ ^[0-9]+$ ]]; then
    offset="$((10#$value))"
  else
    offset="$(_config_ipv4_int "$value")" || return 1
    offset="$((offset - network))"
  fi
  ((offset >= 0 && offset < (1 << (32 - length)))) || return 1
  printf '%d' "$offset"
}

# _config_private_address OFFSET: the address OFFSET past the private
# network's address.
_config_private_address() {
  local network
  network="$(_config_ipv4_int "${PRIVATE_SUBNET_CIDR%/*}")" || return 1
  _config_ipv4_text "$((network + $1))"
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
  [[ "$host" =~ ^mox([1-9]|[12][0-9]|3[0-2])$ ]] ||
    config_die "Host must be named mox1 through mox32: ${host}" || return
  printf '%d\n' "${host#mox}"
}

# The host's private VLAN address (PROXMOX_SECONDARY_IP, ADDRESS/PREFIX) must
# be one of the cluster's host addresses with the private subnet's prefix or
# a shorter one, and its optional gateway another usable address on that
# network.
_config_validate_private_interface() {
  local start="$1" end="$2"
  python3 - "$PROXMOX_SECONDARY_IP" "$start" "$end" "${PRIVATE_SUBNET_CIDR#*/}" \
    "${PROXMOX_SECONDARY_GATEWAY-}" <<'PY'
import ipaddress
import re
import sys

value, start, end, length, gateway = sys.argv[1:6]
start, end = ipaddress.IPv4Address(start), ipaddress.IPv4Address(end)
match = re.fullmatch(r"([0-9.]+)/([1-9][0-9]?)", value)
try:
    if not match or not 8 <= int(match.group(2)) <= int(length):
        raise ValueError
    address = ipaddress.IPv4Address(match.group(1))
except ValueError:
    raise SystemExit(f"PROXMOX_SECONDARY_IP must be ADDRESS/PREFIX with a prefix of /{length} or shorter, such as {start}/{length}")
if not start <= address <= end:
    raise SystemExit(f"PROXMOX_SECONDARY_IP must be an address from {start} to {end}")
if gateway:
    network = ipaddress.IPv4Interface(value).network
    try:
        hop = ipaddress.IPv4Address(gateway)
    except ValueError:
        raise SystemExit("PROXMOX_SECONDARY_GATEWAY is not valid IPv4")
    if hop not in network or hop in (address, network.network_address, network.broadcast_address):
        raise SystemExit(f"PROXMOX_SECONDARY_GATEWAY must be another usable address on {value}")
PY
}

_config_validate_selected_host() {
  local host="$1" index="$2" length="${PRIVATE_SUBNET_CIDR#*/}"
  local mox_start mox_end replication_start migration_prefix haproxy_start
  local mox_first mox_last
  mox_start="$(_config_private_offset "$MOX_IP_START")" || return
  mox_end="$(_config_private_offset "$MOX_IP_END")" || return
  replication_start="$(_config_private_offset "$MOX_REPLICATION_IP_START")" || return
  migration_prefix="${PROXMOX_MIGRATION_NETWORK#*/}"
  haproxy_start="$(_config_private_offset "$HAPROXY_IP_START")" || return
  mox_first="$(_config_private_address "$mox_start")" || return
  mox_last="$(_config_private_address "$mox_end")" || return
  local expected_mox_ip expected_replication_ip expected_haproxy_ip
  expected_mox_ip="$(_config_private_address "$((mox_start + index - 1))")/${length}" || return
  expected_replication_ip="$(_config_private_address "$((replication_start + index - 1))")/${migration_prefix}" || return
  local expected_fqdn="${host}.${PROXMOX_INTERNAL_DOMAIN}"
  expected_haproxy_ip="$(_config_private_address "$((haproxy_start + index - 1))")/${length}" || return
  local expected_haproxy_vmid="$((9110 + index))"
  local expected_vrrp_priority="$((200 - index))"
  local name

  ((index <= MAX_MOX_HOSTS)) ||
    config_die "${host} exceeds configured MAX_MOX_HOSTS=${MAX_MOX_HOSTS}" || return

  [[ -z "${TAILSCALE_HOSTNAME+x}" || "$TAILSCALE_HOSTNAME" == "$host" ]] ||
    config_die "TAILSCALE_HOSTNAME must equal selected host ${host}" || return
  if [[ -n "${PROXMOX_SECONDARY_IP+x}" ]]; then
    _config_validate_private_interface "$mox_first" "$mox_last" ||
      config_die "PROXMOX_SECONDARY_IP for ${host} is invalid" || return
    expected_mox_ip="$PROXMOX_SECONDARY_IP"
  elif [[ -n "${PROXMOX_SECONDARY_GATEWAY+x}" ]]; then
    PROXMOX_SECONDARY_IP="$expected_mox_ip"
    _config_validate_private_interface "$mox_first" "$mox_last" ||
      config_die "PROXMOX_SECONDARY_GATEWAY for ${host} is invalid" || return
  fi
  [[ -z "${PROXMOX_SECONDARY_GATEWAY_ROUTE+x}" ||
    "$PROXMOX_SECONDARY_GATEWAY_ROUTE" =~ ^(yes|no)$ ]] ||
    config_die "PROXMOX_SECONDARY_GATEWAY_ROUTE must be yes or no" || return
  [[ "${PROXMOX_SECONDARY_GATEWAY_ROUTE:-no}" == no || -n "${PROXMOX_SECONDARY_GATEWAY+x}" ]] ||
    config_die "PROXMOX_SECONDARY_GATEWAY_ROUTE=yes needs PROXMOX_SECONDARY_GATEWAY" || return
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
  local serial capacity
  local -A serials=()
  for pair in 0 1 2 3 4; do
    a_name="NVME_MIRROR_${pair}_SERIAL_1"
    b_name="NVME_MIRROR_${pair}_SERIAL_2"
    capacity_a_name="NVME_MIRROR_${pair}_CAPACITY_BYTES_1"
    capacity_b_name="NVME_MIRROR_${pair}_CAPACITY_BYTES_2"
    if [[ -n "${!a_name:-}" || -n "${!b_name:-}" ]]; then
      [[ -n "${!a_name:-}" && -n "${!b_name:-}" ]] ||
        config_die "NVMe mirror ${pair} must define both ${a_name} and ${b_name}" || return
      _config_require_from_file mox "$source_file" "$a_name" "$b_name" || return
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
      ((pair == 0)) &&
        config_die "NVMe mirror 0 is mandatory" && return
      # Pairs 1-4 may have gaps: decommissioning a mirror comments out its
      # entries, and pair N keeps its crypt-rpool-mirrorN-* LUKS names.
      [[ -z "${!capacity_a_name:-}${!capacity_b_name:-}" ]] ||
        config_die "NVMe mirror ${pair} capacities were provided without serials" || return
    fi
  done
}

_config_validate_cluster() {
  local subnet_ip length network last datacenter_gateway vip_octet
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
  [[ "$MAX_MOX_HOSTS" =~ ^([1-9]|[12][0-9]|3[0-2])$ ]] ||
    config_die "MAX_MOX_HOSTS must be between 1 and 32" || return
  [[ "$PRIVATE_SUBNET_CIDR" =~ ^([0-9.]+)/(1[6-9]|2[0-4])$ ]] ||
    config_die "PRIVATE_SUBNET_CIDR must be an IPv4 network from /16 to /24" || return
  subnet_ip="${BASH_REMATCH[1]}"
  length="${BASH_REMATCH[2]}"
  network="$(_config_ipv4_int "$subnet_ip")" ||
    config_die "PRIVATE_SUBNET_CIDR is not valid IPv4" || return
  ((network % (1 << (32 - length)) == 0)) ||
    config_die "PRIVATE_SUBNET_CIDR must be a network address with no host bits set" || return
  last="$(((1 << (32 - length)) - 2))"
  datacenter_gateway="$(_config_private_offset "$DATACENTER_PRIVATE_VLAN_GATEWAY")" ||
    config_die "DATACENTER_PRIVATE_VLAN_GATEWAY must belong to ${PRIVATE_SUBNET_CIDR}" || return
  vip_octet="$(_config_private_offset "$GUEST_EGRESS_VIP")" ||
    config_die "GUEST_EGRESS_VIP must belong to ${PRIVATE_SUBNET_CIDR}" || return
  mox_start="$(_config_private_offset "$MOX_IP_START")" ||
    config_die "MOX_IP_START must belong to ${PRIVATE_SUBNET_CIDR}" || return
  mox_end="$(_config_private_offset "$MOX_IP_END")" ||
    config_die "MOX_IP_END must belong to ${PRIVATE_SUBNET_CIDR}" || return
  replication_start="$(_config_private_offset "$MOX_REPLICATION_IP_START")" ||
    config_die "MOX_REPLICATION_IP_START must belong to ${PRIVATE_SUBNET_CIDR}" || return
  replication_end="$(_config_private_offset "$MOX_REPLICATION_IP_END")" ||
    config_die "MOX_REPLICATION_IP_END must belong to ${PRIVATE_SUBNET_CIDR}" || return
  haproxy_start="$(_config_private_offset "$HAPROXY_IP_START")" ||
    config_die "HAPROXY_IP_START must belong to ${PRIVATE_SUBNET_CIDR}" || return
  haproxy_end="$(_config_private_offset "$HAPROXY_IP_END")" ||
    config_die "HAPROXY_IP_END must belong to ${PRIVATE_SUBNET_CIDR}" || return
  prod_start="$(_config_private_offset "$PRODUCTION_IP_START")" ||
    config_die "PRODUCTION_IP_START must belong to ${PRIVATE_SUBNET_CIDR}" || return
  prod_end="$(_config_private_offset "$PRODUCTION_IP_END")" ||
    config_die "PRODUCTION_IP_END must belong to ${PRIVATE_SUBNET_CIDR}" || return
  staging_start="$(_config_private_offset "$STAGING_IP_START")" ||
    config_die "STAGING_IP_START must belong to ${PRIVATE_SUBNET_CIDR}" || return
  staging_end="$(_config_private_offset "$STAGING_IP_END")" ||
    config_die "STAGING_IP_END must belong to ${PRIVATE_SUBNET_CIDR}" || return
  ((datacenter_gateway >= 1 && datacenter_gateway <= last)) ||
    config_die "DATACENTER_PRIVATE_VLAN_GATEWAY must be a usable host address" || return
  ((vip_octet >= 1 && vip_octet <= last)) ||
    config_die "GUEST_EGRESS_VIP must be a usable host address" || return
  ((mox_start >= 1 && mox_start <= mox_end && mox_end <= last)) ||
    config_die "MOX_IP_START through MOX_IP_END must be a usable ascending range" || return
  ((replication_start >= 1 && replication_start <= replication_end && replication_end <= last)) ||
    config_die "MOX_REPLICATION_IP_START through MOX_REPLICATION_IP_END must be a usable ascending range" || return
  ((haproxy_start >= 1 && haproxy_start <= haproxy_end && haproxy_end <= last)) ||
    config_die "HAPROXY_IP_START through HAPROXY_IP_END must be a usable ascending range" || return
  ((prod_start >= 1 && prod_start <= prod_end && prod_end <= last)) ||
    config_die "PRODUCTION_IP_START through PRODUCTION_IP_END must be a usable ascending range" || return
  ((staging_start >= 1 && staging_start <= staging_end && staging_end <= last)) ||
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

  PRIVATE_SUBNET_LENGTH="$length"
  export PRIVATE_SUBNET_LENGTH
  _PROXMOX_CONFIG_PUBLIC_KEYS+=(PRIVATE_SUBNET_LENGTH)
  _PROXMOX_CONFIG_DERIVED["PRIVATE_SUBNET_LENGTH"]=1

  local name
  for name in PROXMOX_ISO_FILE_SHA256 PROD_GUEST_OS_ISO_SHA256; do
    [[ -z "${!name+x}" ]] || _config_validate_sha256 "$name" || return
  done
  for name in PROXMOX_ISO_FILE_URL PROD_GUEST_OS_ISO_URL; do
    [[ -z "${!name+x}" ]] || _config_validate_https_iso_url "$name" || return
  done
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

# mox_private_ip moxN: the host's private VLAN address without its prefix,
# from its moxN.conf, or the cluster plan's default when it has none.
mox_private_ip() {
  local host="${1:?mox host is required}" index file line value=""
  index="$(mox_index "$host")" || return
  file="${PROXMOX_CONFIG_DIR}/${host}.conf"
  if [[ -r "$file" ]]; then
    while IFS= read -r line || [[ -n "$line" ]]; do
      [[ "$line" =~ ^[[:space:]]*PROXMOX_SECONDARY_IP[[:space:]]*=(.*)$ ]] || continue
      value="$(_config_parse_value "${BASH_REMATCH[1]}")" || return
    done <"$file"
  fi
  if [[ -n "$value" ]]; then
    printf '%s\n' "${value%/*}"
  else
    local start
    start="$(_config_private_offset "$MOX_IP_START")" || return
    _config_private_address "$((start + index - 1))" || return
    printf '\n'
  fi
}

# haproxy_lxc_ip moxN: the address and prefix of the host's HAProxy LXC.
haproxy_lxc_ip() {
  local index start
  index="$(mox_index "${1:?mox host is required}")" || return
  start="$(_config_private_offset "$HAPROXY_IP_START")" || return
  _config_private_address "$((start + index - 1))" || return
  printf '/%s\n' "$PRIVATE_SUBNET_LENGTH"
}

_config_validate_mox() {
  local host="$1" index
  local source_file="${PROXMOX_CONFIG_DIR}/${host}.conf"
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
  _config_validate_selected_host "$host" "$index"
}

_config_compute_hash() {
  local key
  local -a keys=()
  local canonical
  require_command sha256sum sort || return
  for key in "${_PROXMOX_CONFIG_PUBLIC_KEYS[@]}"; do
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

# load_proxmox_config [--host moxN]. Secrets are never configuration: they
# arrive as secret fields (prompt_secret).
load_proxmox_config() {
  local selected_host=""
  local cluster_file="$PROXMOX_CLUSTER_CONFIG" host_file=""
  local key

  while (($#)); do
    case "$1" in
      --host)
        (($# >= 2)) || config_die "--host requires moxN" || return
        selected_host="$2"
        shift 2
        ;;
      *)
        if [[ -z "$selected_host" && "$1" =~ ^mox([1-9]|[12][0-9]|3[0-2])$ ]]; then
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
    PROXMOX_FQDN \
    CONFIG_EFFECTIVE_SHA256 CONFIG_SELECTED_HOST CONFIG_CLUSTER_FILE \
    CONFIG_HOST_FILE CONFIG_LOADED; do
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

  _config_parse_file "$cluster_file" cluster || return
  _config_validate_cluster || return

  if [[ -n "$selected_host" ]]; then
    mox_index "$selected_host" >/dev/null || return
    host_file="${PROXMOX_CONFIG_DIR}/${selected_host}.conf"
    _config_parse_file "$host_file" mox || return
    _config_validate_mox "$selected_host" || return
  fi

  _config_compute_hash || return
  CONFIG_SELECTED_HOST="$selected_host"
  CONFIG_CLUSTER_FILE="$cluster_file"
  CONFIG_HOST_FILE="$host_file"
  CONFIG_LOADED=1
  export CONFIG_SELECTED_HOST CONFIG_CLUSTER_FILE CONFIG_HOST_FILE CONFIG_LOADED
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

# Print the host this script runs on. bmacd names it in BMAC_HOST.
this_mox() {
  [[ "${BMAC_HOST:-}" =~ ^mox([1-9]|[12][0-9]|3[0-2])$ ]] ||
    config_die "BMAC_HOST must name the host this runs on (mox1 through mox32)" || return
  printf '%s\n' "$BMAC_HOST"
}

mox_is_local() {
  local here
  here="$(this_mox)" || return 2
  [[ "$1" == "$here" ]]
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
  local host="${1:?mox host is required}" local_status=0
  _validate_mox_for_ssh "$host" || return
  mox_is_local "$host" || local_status=$?
  ((local_status != 2)) || return 2
  ((local_status != 0)) || return 0
  ssh -F "$BMAC_SSH_CONFIG" "$host" true </dev/null >/dev/null 2>&1
}

# mox_ssh HOST ARGV...: run ARGV as root on HOST. On this host ARGV runs
# directly; on another, through the host entry bmacd keeps in
# /etc/bmac/ssh_config (private address, cluster root key, host key pinned
# from pmxcfs), with every argument %q-quoted for the remote shell.
mox_ssh() {
  local host="${1:?mox host is required}" local_status=0
  shift
  _validate_mox_for_ssh "$host" || return
  (($# > 0)) || config_die "A remote command is required" || return
  mox_is_local "$host" || local_status=$?
  ((local_status != 2)) || return 2
  if ((local_status == 0)); then
    # A daemon the command starts, such as a VM's QEMU, would otherwise hold
    # a JSON-mode job's protocol pipes, and the job would never end.
    if [[ "${BMAC_UI_EVENT_FD:-}" =~ ^[0-9]+$ && "${BMAC_UI_INPUT_FD:-}" =~ ^[0-9]+$ ]]; then
      "$@" {BMAC_UI_EVENT_FD}>&- {BMAC_UI_INPUT_FD}>&-
    else
      "$@"
    fi
    return
  fi
  _quote_remote_argv "$@" || return
  # shellcheck disable=SC2029 # REMOTE_SSH_COMMAND entries are Bash-%q escaped.
  ssh -F "$BMAC_SSH_CONFIG" "$host" "${REMOTE_SSH_COMMAND[@]}"
}

# mox_copy SOURCE HOST DESTINATION MODE: copy a local file to HOST through
# mox_ssh. DESTINATION appears only complete and already at MODE.
mox_copy() {
  local source="$1" host="$2" destination="$3" mode="$4"
  [[ -f "$source" && "$destination" =~ ^/[A-Za-z0-9._/-]+$ &&
    "$mode" =~ ^0[0-7]{3}$ ]] ||
    config_die "mox_copy needs a file, a safe absolute destination, and a mode" || return
  # shellcheck disable=SC2016 # Expanded by the shell on HOST.
  mox_ssh "$host" bash -c \
    'set -euo pipefail; umask 077; cat >"$1.partial"; chmod "$2" "$1.partial"; mv -f -- "$1.partial" "$1"' \
    _ "$destination" "$mode" <"$source"
}

config_usage() {
  cat <<'EOF'
Usage:
  config.sh --check [--host moxN]
  source config.sh; load_proxmox_config [--host moxN]

Strictly parses cluster.conf in /run/bmac/config/, which bmacd writes from
the cluster settings before each script runs, and an optional selected
moxN.conf. It never evaluates configuration as shell code. Secrets are never
configuration; scripts ask for them as secret fields.

Options:
  --check             Validate configuration (default action).
  --host moxN         Load and validate one host layer (mox1 through mox32).
  -h, --help          Show this help.

Successful checks print only the selected host, derived addressing, and the
effective-configuration hash.
EOF
}

config_main() {
  local host=""
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
  load_proxmox_config "${load_args[@]}" || return
  config_info "Configuration valid."
  config_info "Selected host: ${CONFIG_SELECTED_HOST:-none}"
  if [[ -n "${CONFIG_SELECTED_HOST:-}" ]]; then
    config_info "Derived mox FQDN: ${PROXMOX_FQDN}"
    config_info "Derived mox address: ${PROXMOX_SECONDARY_IP}"
    config_info "Derived migration/replication address: ${MOX_REPLICATION_IP}"
    config_info "Derived HAProxy address/VMID: ${HAPROXY_LXC_IP} / ${HAPROXY_LXC_VMID}"
  fi
  config_info "Effective configuration SHA-256: ${CONFIG_EFFECTIVE_SHA256}"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  set -Eeuo pipefail
  config_main "$@"
fi
