#!/bin/sh

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# The discovery OS (DEPLOYMENT.md, "The discovery ISO"): Alpine's diskless
# boot with the Deployer's overlay, started by OpenRC's local service. It
# copies the kernel modules into RAM so nothing reads the boot media again,
# installs its tools, brings up the public NIC (by DHCP, or with the
# operator's static settings), switches device handling to udev (the serials the Proxmox
# installer matches come from udev), starts SSH with the embedded host key
# and the session's client key only, and joins the tailnet with the
# ephemeral key. It never writes to a disk.
#
# POSIX sh: until the tools are installed, busybox ash is all there is.
# Its `local` (which dash has too) is the one extension used.
# shellcheck disable=SC3043

set -u

conf=/etc/bmac-discovery
state=/run/bmac-discovery
ram=/run/bmac-discovery/ram
# shellcheck source=/dev/null
. "$conf/discovery.env"

say() {
  printf 'bmac-discovery: %s\n' "$*"
  for tty in /dev/console /dev/tty1; do
    printf 'bmac-discovery: %s\r\n' "$*" >"$tty" 2>/dev/null || true
  done
}

# The kernel modules and firmware are a squashfs read from the boot media
# on demand. A remote console's virtual media can stall for minutes, which
# turns into read errors, so the image is copied into RAM once and mounted
# from there.
modules_to_ram() {
  local source loop
  loop="$(awk '$2 == "/.modloop" {print $1}' /proc/mounts)"
  [ -n "$loop" ] || return 0
  source="$(cat "/sys/block/${loop#/dev/}/loop/backing_file" 2>/dev/null)"
  [ -n "$source" ] && [ -f "$source" ] || return 0
  say "copying the kernel modules into RAM"
  mkdir -p "$ram"
  mount -t tmpfs -o mode=0700,size=2g bmac-ram "$ram" || return 1
  cp "$source" "$ram/modloop" || return 1
  umount -l /.modloop 2>/dev/null || true
  mount -o loop,ro "$ram/modloop" /.modloop || return 1
}

# Each package on its own, so one a different Alpine release lacks doesn't
# stop the rest.
install_tools() {
  local package missing=""
  for package in $TOOLS; do
    apk add --quiet --no-network "$package" >/dev/null 2>&1 || missing="$missing $package"
  done
  [ -z "$missing" ] || say "not on the boot media:$missing"
  modprobe zfs 2>/dev/null || say "ZFS isn't available; old labels are cleared without it"
  modprobe tun 2>/dev/null || true
}

physical_nics() {
  local path
  for path in /sys/class/net/*; do
    [ -e "$path/device" ] || continue
    basename -- "$path"
  done | sort
}

# The ports to try, in order. Static settings belong to the operator's port
# alone; DHCP tries that port first, then every other one, since the given
# MAC may not be the port the provider's DHCP serves.
candidate_nics() {
  local nic pinned=""
  [ -n "$PUBLIC_MAC" ] || {
    physical_nics
    return
  }
  for nic in $(physical_nics); do
    [ "$(cat "/sys/class/net/$nic/address" 2>/dev/null)" = "$PUBLIC_MAC" ] && pinned="$nic"
  done
  [ -n "$pinned" ] && printf '%s\n' "$pinned"
  [ -n "$ADDRESS" ] && return
  for nic in $(physical_nics); do
    [ "$nic" = "$pinned" ] || printf '%s\n' "$nic"
  done
}

wait_for_carrier() {
  local nic="$1" attempt=1
  ip link set dev "$nic" up 2>/dev/null || return 1
  while [ "$attempt" -le "$CARRIER_ATTEMPTS" ]; do
    [ "$(cat "/sys/class/net/$nic/carrier" 2>/dev/null)" = 1 ] && return 0
    sleep "$CARRIER_INTERVAL"
    attempt=$((attempt + 1))
  done
  return 1
}

gateway_answers() {
  local attempt
  for attempt in 1 2 3 4 5; do
    ping -c 1 -W 2 "$GATEWAY" >/dev/null 2>&1 && return 0
    ip neigh show "$GATEWAY" 2>/dev/null | grep -Eq 'lladdr .* (REACHABLE|STALE|DELAY|PROBE)' && return 0
    sleep 1
  done
  return 1
}

# The operator's DNS servers, when given, over whatever DHCP supplied.
write_dns() {
  [ -n "$DNS" ] || return 0
  for server in $DNS; do printf 'nameserver %s\n' "$server"; done >/etc/resolv.conf
}

# Tailscale's control server must resolve, or joining can't work.
internet_answers() {
  local attempt
  for attempt in 1 2 3; do
    nslookup controlplane.tailscale.com >/dev/null 2>&1 && return 0
    sleep 2
  done
  return 1
}

release_nic() {
  local nic="$1"
  [ -f "/run/udhcpc-$nic.pid" ] && kill "$(cat "/run/udhcpc-$nic.pid")" 2>/dev/null
  rm -f "/run/udhcpc-$nic.pid"
  ip route flush dev "$nic" 2>/dev/null
  ip address flush dev "$nic" 2>/dev/null
}

try_nic() {
  local nic="$1" how
  say "trying $nic"
  if ! wait_for_carrier "$nic"; then
    say "$nic: no carrier after about $((CARRIER_ATTEMPTS * CARRIER_INTERVAL)) seconds"
    return 1
  fi
  release_nic "$nic"
  if [ -n "$ADDRESS" ]; then
    ip address add "$ADDRESS/$PREFIX" dev "$nic" || return 1
    ip route replace default via "$GATEWAY" dev "$nic" || return 1
    how="$ADDRESS/$PREFIX via $GATEWAY"
  else
    # udhcpc stays in the background to renew the lease.
    if ! udhcpc -i "$nic" -n -t 5 -T 3 -A 3 -p "/run/udhcpc-$nic.pid" >/dev/null 2>&1; then
      say "$nic: no DHCP lease"
      release_nic "$nic"
      return 1
    fi
    GATEWAY="$(ip route show default dev "$nic" 2>/dev/null | awk '{print $3; exit}')"
    if [ -z "$GATEWAY" ]; then
      say "$nic: the DHCP lease has no gateway"
      release_nic "$nic"
      return 1
    fi
    how="DHCP $(ip -4 -o address show dev "$nic" | awk '{print $4; exit}') via $GATEWAY"
  fi
  write_dns
  if ! gateway_answers; then
    say "$nic: the gateway $GATEWAY doesn't answer"
  elif ! internet_answers; then
    say "$nic: the gateway answers but controlplane.tailscale.com doesn't resolve"
  else
    PUBLIC_NIC="$nic"
    PUBLIC_HOW="$how"
    return 0
  fi
  release_nic "$nic"
  [ -n "$ADDRESS" ] || GATEWAY=""
  return 1
}

configure_network() {
  local nic round
  PUBLIC_NIC=""
  for nic in $(physical_nics); do
    ip address flush dev "$nic" 2>/dev/null || true
  done
  for round in 1 2 3; do
    for nic in $(candidate_nics); do
      try_nic "$nic" && break
    done
    [ -n "$PUBLIC_NIC" ] && break
    [ "$round" -lt 3 ] || break
    say "no port reached the internet yet (round $round of 3); trying again"
    sleep 10
  done
  [ -n "$PUBLIC_NIC" ] || {
    say "no port reached the internet; check the cable, the DHCP server, or give static settings"
    return 1
  }
  say "public network up on $PUBLIC_NIC ($PUBLIC_HOW)"
  # What the Deployer offers as the host's public network.
  mkdir -p "$state"
  {
    printf 'MAC=%s\n' "$(cat "/sys/class/net/$PUBLIC_NIC/address")"
    printf 'ADDRESS=%s\n' "$(ip -4 -o address show dev "$PUBLIC_NIC" | awk '{print $4; exit}')"
    printf 'GATEWAY=%s\n' "$GATEWAY"
    if [ -n "$ADDRESS" ]; then echo METHOD=static; else echo METHOD=dhcp; fi
  } >"$state/public-network"
  # Every other NIC stays up for the inventory's carrier readings, with no
  # address.
  for nic in $(physical_nics); do
    ip link set dev "$nic" up 2>/dev/null || true
  done
}

# udev from the mirror of the boot media's own release; apk checks it
# against the Alpine signing keys the media carries.
start_udev() {
  local release attempt=1
  release="$(cut -d. -f1,2 /etc/alpine-release)"
  printf 'https://dl-cdn.alpinelinux.org/alpine/v%s/main\n' "$release" >>/etc/apk/repositories
  while [ "$attempt" -le 10 ]; do
    if setup-devd udev >/dev/null 2>&1; then
      udevadm trigger --action=add --type=subsystems >/dev/null 2>&1
      udevadm trigger --action=add --type=devices >/dev/null 2>&1
      udevadm settle --timeout=120 >/dev/null 2>&1
      say "device identities from udev"
      return 0
    fi
    sleep 10
    attempt=$((attempt + 1))
  done
  say "couldn't install udev from the Alpine mirror; disk serials can't be read"
  return 1
}

start_ssh() {
  install -d -m 0700 "$state"
  install -m 0600 "$conf/ssh_host_ed25519_key" "$state/ssh_host_ed25519_key"
  install -m 0644 "$conf/ssh_host_ed25519_key.pub" "$state/ssh_host_ed25519_key.pub"
  install -m 0600 "$conf/authorized_keys" "$state/authorized_keys"
  cat >"$state/sshd_config" <<EOF
HostKey $state/ssh_host_ed25519_key
PermitRootLogin prohibit-password
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
AuthorizedKeysFile $state/authorized_keys
AllowUsers root
Subsystem sftp internal-sftp
EOF
  /usr/sbin/sshd -f "$state/sshd_config" || return 1
  say "SSH host key $(ssh-keygen -lf "$state/ssh_host_ed25519_key.pub" | awk '{print $2}')"
}

join_tailnet() {
  local programs=/usr/local/lib/bmac-discovery attempt=1
  install -d -m 0700 "$programs" "$state/tailscale" /run/tailscale
  tar -xzf "$conf/tailscale.tgz" -C "$programs" --strip-components=1 || return 1
  rm -f "$conf/tailscale.tgz"
  install -m 0600 "$conf/tailscale-auth-key" "$state/tailscale-auth-key"
  rm -f "$conf/tailscale-auth-key"
  (
    while :; do
      "$programs/tailscaled" --state=mem: --socket=/run/tailscale/tailscaled.sock \
        --statedir="$state/tailscale" >>/var/log/bmac-tailscaled.log 2>&1
      sleep 2
    done
  ) &
  while [ "$attempt" -le 30 ]; do
    if "$programs/tailscale" --socket=/run/tailscale/tailscaled.sock up \
      --auth-key="file:$state/tailscale-auth-key" --hostname="$TAILSCALE_NAME" \
      --accept-dns=false --timeout=60s >/dev/null 2>&1 && tailnet_confirmed "$programs"; then
      rm -f "$state/tailscale-auth-key"
      say "joined the tailnet as $TAILSCALE_NAME ($TAILSCALE_IP)"
      return 0
    fi
    sleep 10
    attempt=$((attempt + 1))
  done
  return 1
}

# `tailscale up` returning isn't enough: the daemon must be running with a
# tailnet address, and the control server must list this node as online.
# Sets TAILSCALE_IP.
tailnet_confirmed() {
  local tailscale="$1/tailscale" check=1 status
  while [ "$check" -le 24 ]; do
    status="$("$tailscale" --socket=/run/tailscale/tailscaled.sock status --json --peers=false 2>/dev/null)"
    TAILSCALE_IP="$("$tailscale" --socket=/run/tailscale/tailscaled.sock ip -4 2>/dev/null | head -n 1)"
    if printf '%s' "$status" | grep -q '"BackendState": *"Running"' &&
      printf '%s' "$status" | grep -q '"Online": *true' &&
      [ -n "$TAILSCALE_IP" ]; then
      return 0
    fi
    sleep 5
    check=$((check + 1))
  done
  say "Tailscale accepted the key but the node isn't online on the tailnet yet"
  return 1
}

main() {
  say "starting"
  swapoff -a 2>/dev/null || true
  modules_to_ram || say "couldn't copy the kernel modules into RAM; continuing from the boot media"
  install_tools
  install -m 0755 "$conf/bmac-inventory" /usr/sbin/bmac-inventory
  configure_network || return 1
  start_udev
  start_ssh || {
    say "SSH didn't start"
    return 1
  }
  join_tailnet || {
    say "couldn't join the tailnet; check that the key is unused and the network reaches the internet"
    return 1
  }
  banner "ready; online on the tailnet as $TAILSCALE_NAME ($TAILSCALE_IP)"
}

# The line the operator watches for, set apart from the boot messages.
banner() {
  local rule="================================================================"
  for tty in /dev/console /dev/tty1; do
    printf '\r\n\r\n%s\r\n%s\r\nbmac-discovery: %s\r\n%s\r\n%s\r\n\r\n\r\n' \
      "$rule" "$rule" "$*" "$rule" "$rule" >"$tty" 2>/dev/null || true
  done
  printf 'bmac-discovery: %s\n' "$*"
}

main "$@"
