#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# Installs the hello app on a production guest from the files the Deployer
# unpacked into the staging directory (v2's deploy_hello_app_to_prod.sh
# remote helper). A failure restores the previous NGINX files.

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

apt_lock_holder() {
  local lock dev inode key pid
  for lock in /var/lib/dpkg/lock-frontend /var/lib/dpkg/lock \
    /var/lib/apt/lists/lock /var/cache/apt/archives/lock; do
    [[ -e "$lock" ]] || continue
    read -r dev inode < <(stat -c '%d %i' "$lock")
    printf -v key '%02x:%02x:%s' \
      "$(( ((dev >> 8) & 0xfff) | ((dev >> 32) & ~0xfff) ))" \
      "$(( (dev & 0xff) | ((dev >> 12) & ~0xff) ))" "$inode"
    pid="$(awk -v key="$key" '$2 != "->" && $6 == key { print $5; exit }' /proc/locks)"
    if [[ -n "$pid" ]]; then
      printf '%s\n' "$pid"
      return 0
    fi
  done
}

apt_wait_for_locks() {
  local limit=600 poll=10 waited=0 holder
  holder="$(apt_lock_holder)"
  [[ -n "$holder" ]] || return 0
  echo "Another package-management process holds the apt/dpkg lock (PID $holder); waiting up to 10 minutes."
  while holder="$(apt_lock_holder)"; [[ -n "$holder" ]]; do
    if ((waited >= limit)); then
      echo "The apt/dpkg lock is still held by PID $holder after ${waited}s." >&2
      return 1
    fi
    sleep "$poll"
    waited=$((waited + poll))
  done
}

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

command -v curl >/dev/null 2>&1 || {
  printf 'curl is not installed on this guest; install it (apt-get install -y curl) and deploy again.\n' >&2
  exit 1
}

managed=/etc/nginx/sites-enabled/hello-app
conflicts=""
for root in /etc/nginx/sites-enabled /etc/nginx/conf.d; do
  [[ -d "$root" ]] || continue
  while IFS= read -r -d '' path; do
    [[ "$path" == "$managed" || "$path" == "$default_link" ]] && continue
    while IFS= read -r hostname; do
      [[ -n "$hostname" ]] || continue
      if grep -Fq -- "$hostname" "$path"; then
        conflicts+="    $path: $hostname"$'\n'
      fi
    done <"$stage_dir/server-names"
  done < <(find -L "$root" -maxdepth 1 -type f -print0 2>/dev/null)
done
if [[ -n "$conflicts" ]]; then
  printf 'Enabled NGINX configuration already names these hosts:\n%s' "$conflicts" >&2
  printf 'Disable or fix that configuration, then deploy again; unrelated sites are never changed.\n' >&2
  exit 1
fi

if ! command -v nginx >/dev/null 2>&1; then
  apt_wait_for_locks
  apt-get -o DPkg::Lock::Timeout=60 update
  DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=60 install -y nginx
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

# A graceful reload is asynchronous, so poll the virtual host rather than
# trusting the first request.
wait_for_http() {
  local label="$1" body_file="$2" expected="$3"
  shift 3
  local attempt status="" curl_rc=0
  for ((attempt = 1; attempt <= 50; attempt++)); do
    : >"$body_file"
    curl_rc=0
    status="$(curl --noproxy '*' -sS -o "$body_file" -w '%{http_code}' "$@")" || curl_rc=$?
    if ((curl_rc == 0)) && [[ "$status" == 200 ]] && grep -Fq -- "$expected" "$body_file"; then
      return 0
    fi
    sleep 0.1
  done
  printf '%s failed: curl exit %d, HTTP status %s.\n' "$label" "$curl_rc" "${status:-unknown}" >&2
  sed -n '1,40p' "$body_file" >&2 || true
  journalctl -u nginx --since '-2 minutes' --no-pager 2>/dev/null | tail -n 40 >&2 || true
  return 1
}

wait_for_http 'HTTP /healthz' "$backup_dir/http-health" ok \
  -H "Host: $primary_domain" http://127.0.0.1/healthz
wait_for_http 'HTTPS /healthz' "$backup_dir/https-health" ok \
  -k --resolve "$primary_domain:443:127.0.0.1" "https://$primary_domain/healthz"
wait_for_http 'HTTPS hello page' "$backup_dir/https-root" '<h1>Hello, world!</h1>' \
  -k --resolve "$primary_domain:443:127.0.0.1" "https://$primary_domain/"

committed=true
printf 'NGINX deployment validated and active.\n'
