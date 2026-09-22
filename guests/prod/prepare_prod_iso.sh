#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Cache one hash-addressed Ubuntu source ISO and build a per-VM autoinstall ISO
# locally on a Proxmox node. The source cache is intentionally retained.

set -Eeuo pipefail
set +x
umask 077

INSTALL_ROOT="/usr/local/lib/app-ha-proxmox"
CACHE_ROOT="/var/lib/app-ha-proxmox/iso-cache"
ISO_BUILDER="${INSTALL_ROOT}/guests/prod/build_ubuntu_autoinstall.py"

if [[ "${APP_HA_PREPARE_ISO_TEST_MODE:-0}" == 1 ]]; then
  CACHE_ROOT="${APP_HA_PREPARE_ISO_CACHE_ROOT:?test cache root is required}"
  ISO_BUILDER="${APP_HA_PREPARE_ISO_BUILDER:?test ISO builder is required}"
fi

die() {
  printf 'ERROR: %s\n' "$*" >&2
  exit 1
}

if [[ "${APP_HA_PREPARE_ISO_TEST_MODE:-0}" != 1 ]]; then
  ((EUID == 0)) || die "production ISO preparation must run as root"
fi

usage() {
  cat <<'EOF'
Usage: prepare_prod_iso.sh --source-url HTTPS_URL --expected-sha256 SHA256 \
  --install-mode ubuntu-autoinstall|manual --request FILE --output-iso FILE

Downloads and verifies a hash-addressed source ISO when it is not already
cached, verifies cached content before every use, and builds one per-VM
autoinstall ISO. The source ISO and adjacent .sha256 file are retained.
EOF
}

SOURCE_URL=""
EXPECTED_SHA256=""
INSTALL_MODE=""
REQUEST_PATH=""
OUTPUT_ISO=""

while (($#)); do
  case "$1" in
    --source-url)
      (($# >= 2)) || die "--source-url requires a value"
      SOURCE_URL="$2"
      shift 2
      ;;
    --expected-sha256)
      (($# >= 2)) || die "--expected-sha256 requires a value"
      EXPECTED_SHA256="${2,,}"
      shift 2
      ;;
    --install-mode)
      (($# >= 2)) || die "--install-mode requires a value"
      INSTALL_MODE="$2"
      shift 2
      ;;
    --request)
      (($# >= 2)) || die "--request requires a value"
      REQUEST_PATH="$2"
      shift 2
      ;;
    --output-iso)
      (($# >= 2)) || die "--output-iso requires a value"
      OUTPUT_ISO="$2"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      die "Unknown argument: $1"
      ;;
  esac
done

[[ "$EXPECTED_SHA256" =~ ^[0-9a-f]{64}$ ]] ||
  die "expected SHA-256 must contain 64 lowercase hexadecimal characters"
case "$INSTALL_MODE" in
  ubuntu-autoinstall | manual) ;;
  *) die "install mode must be ubuntu-autoinstall or manual" ;;
esac
[[ "$REQUEST_PATH" == /* && "$OUTPUT_ISO" == /* ]] ||
  die "request and output paths must be absolute"
[[ -f "$REQUEST_PATH" && ! -L "$REQUEST_PATH" ]] ||
  die "request must be a regular, non-symlink file"
[[ "$OUTPUT_ISO" =~ ^/[A-Za-z0-9._/-]+[.]iso$ ]] ||
  die "output ISO path is unsafe"

python3 - "$SOURCE_URL" <<'PY' ||
import sys
from urllib.parse import urlsplit

value = sys.argv[1]
if any(ord(character) < 0x20 or character.isspace() for character in value):
    raise SystemExit("source URL contains whitespace or control characters")
parsed = urlsplit(value)
if (
    parsed.scheme != "https"
    or not parsed.hostname
    or parsed.username is not None
    or parsed.password is not None
    or parsed.fragment
    or not parsed.path.lower().endswith(".iso")
):
    raise SystemExit("source URL must be a public HTTPS URL ending in .iso")
PY
  die "source URL is invalid"

for command_name in cp curl df findmnt flock python3 sha256sum stat; do
  command -v "$command_name" >/dev/null 2>&1 ||
    die "Required host command is unavailable: $command_name"
done
if [[ "$INSTALL_MODE" == ubuntu-autoinstall ]]; then
  command -v xorriso >/dev/null 2>&1 ||
    die "Required host command is unavailable: xorriso"
  [[ -x "$ISO_BUILDER" && ! -L "$ISO_BUILDER" ]] ||
    die "Installed autoinstall ISO builder is unavailable"
fi

if [[ "${APP_HA_PREPARE_ISO_TEST_MODE:-0}" == 1 ]]; then
  install -d -m 0700 "$CACHE_ROOT"
else
  install -d -o root -g root -m 0700 "$CACHE_ROOT"
fi
[[ -d "$CACHE_ROOT" && ! -L "$CACHE_ROOT" ]] ||
  die "ISO cache root is unavailable or unsafe"

cache_filesystem="$(findmnt -n -o FSTYPE --target "$CACHE_ROOT")"
case "$cache_filesystem" in
  tmpfs | ramfs) die "ISO cache must be disk-backed, not $cache_filesystem" ;;
esac

CACHE_ISO="${CACHE_ROOT}/${EXPECTED_SHA256}.iso"
CACHE_SHA256="${CACHE_ISO}.sha256"
CACHE_LOCK="${CACHE_ROOT}/${EXPECTED_SHA256}.lock"
CACHE_REUSED=false

exec {cache_lock_fd}>"$CACHE_LOCK"
chmod 0600 "$CACHE_LOCK"
flock "$cache_lock_fd"

cache_is_valid=false
if [[ -f "$CACHE_ISO" && ! -L "$CACHE_ISO" ]]; then
  observed="$(sha256sum "$CACHE_ISO" | awk '{print tolower($1)}')"
  if [[ "$observed" == "$EXPECTED_SHA256" ]]; then
    cache_is_valid=true
    CACHE_REUSED=true
  else
    printf 'WARNING: removing corrupt cached source ISO %s\n' "$CACHE_ISO" >&2
    rm -f -- "$CACHE_ISO" "$CACHE_SHA256"
  fi
fi

if [[ "$cache_is_valid" == false ]]; then
  partial="${CACHE_ISO}.partial.$$"
  trap 'rm -f -- "${partial:-}" "${sidecar_partial:-}"' EXIT
  printf 'Downloading production guest source ISO on this Proxmox host:\n  %s\n' \
    "$SOURCE_URL" >&2
  curl \
    --proto '=https' \
    --proto-redir '=https' \
    --fail \
    --location \
    --retry 5 \
    --retry-delay 3 \
    --retry-all-errors \
    --show-error \
    --progress-bar \
    --output "$partial" \
    "$SOURCE_URL"
  [[ -f "$partial" && ! -L "$partial" && -s "$partial" ]] ||
    die "downloaded source ISO is missing or empty"
  observed="$(sha256sum "$partial" | awk '{print tolower($1)}')"
  [[ "$observed" == "$EXPECTED_SHA256" ]] ||
    die "downloaded source ISO SHA-256 mismatch (expected $EXPECTED_SHA256, observed $observed)"
  chmod 0600 "$partial"
  mv -f -- "$partial" "$CACHE_ISO"
fi

# Verify on every use, including a cache hit, before refreshing the sidecar.
observed="$(sha256sum "$CACHE_ISO" | awk '{print tolower($1)}')"
[[ "$observed" == "$EXPECTED_SHA256" ]] ||
  die "cached source ISO failed final SHA-256 verification"
chmod 0600 "$CACHE_ISO"
sidecar_partial="${CACHE_SHA256}.partial.$$"
printf '%s  %s\n' "$EXPECTED_SHA256" "$(basename -- "$CACHE_ISO")" \
  >"$sidecar_partial"
chmod 0600 "$sidecar_partial"
mv -f -- "$sidecar_partial" "$CACHE_SHA256"
trap - EXIT

output_parent="$(dirname -- "$OUTPUT_ISO")"
[[ -d "$output_parent" && ! -L "$output_parent" ]] ||
  die "output ISO directory is unavailable or unsafe"
output_filesystem="$(findmnt -n -o FSTYPE --target "$output_parent")"
case "$output_filesystem" in
  tmpfs | ramfs) die "output ISO storage must be disk-backed, not $output_filesystem" ;;
esac
source_bytes="$(stat -c %s "$CACHE_ISO")"
available_bytes="$(
  df -B1 --output=avail "$output_parent" | awk 'NR == 2 {print $1}'
)"
[[ "$available_bytes" =~ ^[0-9]+$ ]] ||
  die "could not determine output ISO storage capacity"
required_bytes=$((source_bytes + 67108864))
((available_bytes >= required_bytes)) ||
  die "output ISO storage has $available_bytes bytes free; $required_bytes required"

if [[ "$INSTALL_MODE" == ubuntu-autoinstall ]]; then
  "$ISO_BUILDER" \
    --source-iso "$CACHE_ISO" \
    --expected-sha256 "$EXPECTED_SHA256" \
    --output-iso "$OUTPUT_ISO" \
    --request "$REQUEST_PATH" \
    --xorriso "$(command -v xorriso)" >&2
else
  output_partial="${OUTPUT_ISO}.partial.$$"
  trap 'rm -f -- "$output_partial"' EXIT
  cp --reflink=auto --sparse=always -- "$CACHE_ISO" "$output_partial"
  chmod 0600 "$output_partial"
  manual_hash="$(sha256sum "$output_partial" | awk '{print tolower($1)}')"
  [[ "$manual_hash" == "$EXPECTED_SHA256" ]] ||
    die "manual installer copy failed SHA-256 verification"
  mv -f -- "$output_partial" "$OUTPUT_ISO"
  trap - EXIT
fi

[[ -f "$OUTPUT_ISO" && ! -L "$OUTPUT_ISO" && -s "$OUTPUT_ISO" ]] ||
  die "per-VM installer ISO was not created"
chmod 0600 "$OUTPUT_ISO"
custom_sha256="$(sha256sum "$OUTPUT_ISO" | awk '{print tolower($1)}')"
custom_bytes="$(stat -c %s "$OUTPUT_ISO")"

python3 - \
  "$CACHE_ISO" "$CACHE_SHA256" "$CACHE_REUSED" \
  "$OUTPUT_ISO" "$custom_sha256" "$custom_bytes" "$INSTALL_MODE" <<'PY'
import json
import sys

print(json.dumps({
    "cache_iso": sys.argv[1],
    "cache_sha256_file": sys.argv[2],
    "cache_reused": sys.argv[3] == "true",
    "install_mode": sys.argv[7],
    "custom_iso": sys.argv[4],
    "custom_sha256": sys.argv[5],
    "custom_bytes": int(sys.argv[6]),
}, sort_keys=True))
PY
