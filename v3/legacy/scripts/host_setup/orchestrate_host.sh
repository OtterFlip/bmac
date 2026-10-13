#!/usr/bin/env bash

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# The host setup job's routing and orchestration phase on this host: v2's
# add_proxmox_host.sh configure_guest_egress, install_shared_orchestration_tools,
# create_haproxy_lxc, and sync_haproxy_routes, which ran here over SSH, run by
# bmacd's setup job instead (IMPLEMENTATION_PLAN.md, M7.3). The runtime, the
# hook, and the timers come from the bmacd package, so only their setup is
# left. bmacd installs keepalived, jq, and curl first, renders the settings
# into /run/bmac/config, and names this host in BMAC_HOST. Every step can be
# rerun.

set -Eeuo pipefail
set +x
umask 022

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
LIB_DIR="${SCRIPT_DIR}/../lib"
RUNTIME_DIR="${SCRIPT_DIR}/../host_runtime"
for library in "$LIB_DIR/config.sh" "$LIB_DIR/apt_lock_wait.sh"; do
  [[ -f "$library" && ! -L "$library" ]] || {
    printf 'ERROR: Missing library: %s\n' "$library" >&2
    exit 1
  }
done
# shellcheck source=../lib/config.sh
source "$LIB_DIR/config.sh"
# shellcheck source=../lib/apt_lock_wait.sh
source "$LIB_DIR/apt_lock_wait.sh"

HOOK_NAME=bmac-guest-role-hook.sh
SYNC="${RUNTIME_DIR}/sync_haproxy_routes.sh"
REGISTRY="${RUNTIME_DIR}/cluster_registry.py"

log() {
  printf '\n==> %s\n' "$*"
}

public_interface() {
  ip -4 route get 1.1.1.1 | awk '{for (i=1;i<=NF;i++) if ($i=="dev") {print $(i+1); exit}}'
}

configure_guest_egress() {
  log "Configuring the quorum-aware floating guest egress gateway"
  local host_id="$MOX_HOSTNAME" bridge="$PROXMOX_PRIVATE_BRIDGE"
  local private_ip="${PROXMOX_SECONDARY_IP%/*}" vip_cidr="$GUEST_EGRESS_VIP"
  local priority="$VRRP_PRIORITY" public_ip="$PROXMOX_IP"
  local production_start="$PRODUCTION_IP_START" staging_end="$STAGING_IP_END"
  local index candidate peer_lines="" peer_set="" peer_count=0 public_if

  ((MAX_MOX_HOSTS >= 2)) || {
    printf 'Guest egress needs MAX_MOX_HOSTS of at least 2 for a VRRP peer.\n' >&2
    exit 1
  }
  [[ "$priority" =~ ^[1-9][0-9]*$ && "$priority" -le 255 ]]
  for ((index = 1; index <= MAX_MOX_HOSTS; index += 1)); do
    candidate="$(mox_private_ip "mox${index}")"
    [[ "$candidate" == "$private_ip" ]] && continue
    peer_lines+="        ${candidate}"$'\n'
    peer_set+="${candidate}, "
    peer_count=$((peer_count + 1))
  done
  ((peer_count == MAX_MOX_HOSTS - 1))
  peer_set="${peer_set%, }"

  public_if="$(public_interface)"
  [[ -n "$public_if" && "$public_if" != "$bridge" ]]
  ip -4 -o address show dev "$public_if" |
    awk -v ip="$public_ip" '$4 ~ ("^" ip "/") { found=1 } END { exit !found }'

  cat >/etc/sysctl.d/91-bmac-guest-egress.conf <<'EOF'
net.ipv4.ip_forward=1
EOF
  sysctl --system >/dev/null

  install -d -m 0755 /etc/nftables.d
  cat >/etc/nftables.d/bmac-guest-egress.nft <<EOF
table ip bmac_guest_egress {
  chain vrrp_input {
    type filter hook input priority -190; policy accept;
    iifname "$bridge" ip protocol 112 ip saddr { $peer_set } ip daddr $private_ip counter accept
    iifname "$bridge" ip protocol 112 counter drop
  }

  chain vrrp_output {
    type filter hook output priority -190; policy accept;
    oifname "$bridge" ip protocol 112 ip saddr $private_ip ip daddr { $peer_set } counter accept
    oifname "$bridge" ip protocol 112 counter drop
  }

  chain forward {
    type filter hook forward priority -190; policy accept;
    iifname != "$bridge" ip saddr $production_start-$staging_end counter drop
    iifname "$bridge" oifname "$public_if" ip saddr $production_start-$staging_end counter accept
    iifname "$public_if" oifname "$bridge" ip daddr $production_start-$staging_end ct state established,related counter accept
  }

  chain postrouting {
    type nat hook postrouting priority srcnat; policy accept;
    iifname "$bridge" oifname "$public_if" ip saddr $production_start-$staging_end masquerade
  }
}
EOF

  # Proxmox's host firewall must allow VRRP too: an nft accept verdict isn't
  # final if a later Proxmox chain rejects it. Proxmox's interface validator
  # refuses hyphens, so the interface binding stays in the nft chains above.
  local rules_json position managed_rule_count
  rules_json="$(pvesh get "/nodes/$host_id/firewall/rules" --output-format json)"
  while IFS= read -r position; do
    pvesh delete "/nodes/$host_id/firewall/rules/$position" </dev/null
  done < <(
    jq -r '
      [.[] | select(
        ((.comment // "") | startswith("bmac managed VRRP peer ")) or
        (.comment // "") == "bmac managed guest egress forward"
      )] |
      sort_by(.pos) | reverse | .[].pos
    ' <<<"$rules_json"
  )
  for ((index = 1; index <= MAX_MOX_HOSTS; index += 1)); do
    candidate="$(mox_private_ip "mox${index}")"
    [[ "$candidate" == "$private_ip" ]] && continue
    pvesh create "/nodes/$host_id/firewall/rules" \
      --pos 0 --type in --action ACCEPT --enable 1 \
      --proto 112 --source "$candidate" --dest "$private_ip" \
      --comment "bmac managed VRRP peer $candidate inbound"
    pvesh create "/nodes/$host_id/firewall/rules" \
      --pos 0 --type out --action ACCEPT --enable 1 \
      --proto 112 --source "$private_ip" --dest "$candidate" \
      --comment "bmac managed VRRP peer $candidate outbound"
  done
  pvesh create "/nodes/$host_id/firewall/rules" \
    --pos 0 --type forward --action ACCEPT --enable 1 \
    --source "$production_start-$staging_end" \
    --comment "bmac managed guest egress forward"
  rules_json="$(pvesh get "/nodes/$host_id/firewall/rules" --output-format json)"
  managed_rule_count="$(
    jq -r '
      [.[] | select(
        ((.comment // "") | startswith("bmac managed VRRP peer ")) and
        .action == "ACCEPT" and (.enable | tostring) == "1" and
        (.proto | tostring) == "112"
      )] | length
    ' <<<"$rules_json"
  )"
  ((managed_rule_count == peer_count * 2))
  jq -e --arg source "$production_start-$staging_end" '
    any(.[];
      (.comment // "") == "bmac managed guest egress forward" and
      .type == "forward" and .action == "ACCEPT" and
      (.enable | tostring) == "1" and
      (.iface // "") == "" and .source == $source
    )
  ' <<<"$rules_json" >/dev/null

  cat >/usr/local/sbin/bmac-load-guest-egress <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
nft list table ip bmac_guest_egress >/dev/null 2>&1 &&
  nft delete table ip bmac_guest_egress
nft -f /etc/nftables.d/bmac-guest-egress.nft
EOF
  chmod 0755 /usr/local/sbin/bmac-load-guest-egress

  cat >/etc/systemd/system/bmac-guest-egress.service <<'EOF'
[Unit]
Description=BMAC production and staging guest egress NAT
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
ExecStart=/usr/local/sbin/bmac-load-guest-egress
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF

  cat >/usr/local/sbin/bmac-check-egress-router <<EOF
#!/usr/bin/env bash
set -Eeuo pipefail
[[ "\$(sysctl -n net.ipv4.ip_forward)" == 1 ]]
ip -br link show dev "$bridge" |
  awk '\$2 == "UP" { found=1 } END { exit !found }'
ip -br link show dev "$public_if" |
  awk '\$2 == "UP" { found=1 } END { exit !found }'
ip -4 route get 1.1.1.1 |
  awk -v dev="$public_if" '{ for (i=1; i<=NF; i++) if (\$i == "dev" && \$(i+1) == dev) found=1 } END { exit !found }'
ip -4 -o address show dev "$public_if" |
  awk -v ip="$public_ip" '\$4 ~ ("^" ip "/") { found=1 } END { exit !found }'
nft list table ip bmac_guest_egress >/dev/null
nft -nn list chain ip bmac_guest_egress vrrp_input |
  grep -Fq 'ip protocol 112'
nft -nn list chain ip bmac_guest_egress vrrp_output |
  grep -Fq 'ip protocol 112'
nft list chain ip bmac_guest_egress forward |
  grep -Fq 'ct state established,related'
nft list chain ip bmac_guest_egress postrouting |
  grep -Fq 'masquerade'
if nft -nn list ruleset |
  awk '/hook forward/ && /policy drop/ { found=1 } END { exit !found }'; then
  exit 1
fi
if command -v iptables >/dev/null 2>&1 &&
  iptables -w -S FORWARD |
    awk '\$1 == "-P" && \$2 == "FORWARD" && \$3 == "DROP" { found=1 } END { exit !found }'; then
  exit 1
fi
pvecm status 2>/dev/null |
  awk '\$1 == "Quorate:" && \$2 == "Yes" { found=1 } END { exit !found }'
curl -4 --interface "$public_ip" --connect-timeout 3 --max-time 4 \
  --fail --silent --show-error https://1.1.1.1/cdn-cgi/trace >/dev/null
EOF
  chmod 0755 /usr/local/sbin/bmac-check-egress-router

  cat >/usr/local/sbin/bmac-egress-vrrp-notify <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
state="${1:?VRRP state is required}"
logger -t bmac-egress "guest egress gateway entered ${state}"
EOF
  chmod 0755 /usr/local/sbin/bmac-egress-vrrp-notify

  install -d -m 0755 /etc/keepalived
  cat >/etc/keepalived/keepalived.conf <<EOF
global_defs {
    router_id $host_id
    enable_script_security
    script_user root
    vrrp_check_unicast_src
}

vrrp_script bmac_egress_ready {
    script "/usr/local/sbin/bmac-check-egress-router"
    interval 15
    timeout 6
    fall 3
    rise 2
}

vrrp_instance BMAC_GUEST_EGRESS {
    state BACKUP
    interface $bridge
    virtual_router_id 10
    priority $priority
    advert_int 1
    nopreempt

    unicast_src_ip $private_ip
    unicast_peer {
$peer_lines    }

    virtual_ipaddress {
        $vip_cidr dev $bridge
    }

    track_interface {
        $public_if
    }

    track_script {
        bmac_egress_ready
    }

    garp_master_delay 1
    garp_master_repeat 5
    garp_master_refresh 60
    garp_master_refresh_repeat 2

    notify_master "/usr/local/sbin/bmac-egress-vrrp-notify MASTER"
    notify_backup "/usr/local/sbin/bmac-egress-vrrp-notify BACKUP"
    notify_fault "/usr/local/sbin/bmac-egress-vrrp-notify FAULT"
}
EOF

  install -d -m 0755 /etc/systemd/system/keepalived.service.d
  cat >/etc/systemd/system/keepalived.service.d/bmac.conf <<'EOF'
[Unit]
After=bmac-guest-egress.service pve-cluster.service
Requires=bmac-guest-egress.service
EOF

  systemctl daemon-reload
  systemctl enable bmac-guest-egress.service
  systemctl restart bmac-guest-egress.service
  keepalived --config-test --use-file=/etc/keepalived/keepalived.conf
  systemctl enable keepalived.service
  systemctl restart keepalived.service

  systemctl is-active bmac-guest-egress.service
  systemctl is-active keepalived.service
  printf 'Floating gateway %s is managed by keepalived; this host is priority %s.\n' \
    "${vip_cidr%/*}" "$priority"
}

configure_runtime() {
  log "Enabling the guest-role hook, the cleanup and route-sync timers, and the registry"
  local content
  content="$(
    pvesh get /storage/local --output-format json |
      jq -er '.content | select(type == "string" and length > 0)'
  )"
  case ",${content}," in
    *,snippets,*) ;;
    *) pvesm set local --content "${content},snippets" ;;
  esac
  test -x "/var/lib/vz/snippets/$HOOK_NAME"
  bash -n "/var/lib/vz/snippets/$HOOK_NAME"
  pvesm path "local:snippets/$HOOK_NAME" >/dev/null
  install -d -o root -g root -m 0700 /var/lib/bmac/iso-cache
  systemctl enable --now bmac-deferred-cleanup.timer
  systemctl enable bmac-haproxy-route-sync.timer

  local replication_minutes="${PROD_VM_REPLICATION_INTERVAL:-*/15}"
  replication_minutes="${replication_minutes##*/}"
  [[ "$replication_minutes" =~ ^[1-9][0-9]*$ ]]
  if [[ ! -s "${CLUSTER_STATE_DIR}/policy.json" ]]; then
    "$REGISTRY" --state-dir "$CLUSTER_STATE_DIR" init \
      --network "$PRIVATE_SUBNET_CIDR" \
      --guest-gateway "$GUEST_EGRESS_VIP" \
      --mox-ip-start "$MOX_IP_START" \
      --mox-ip-end "$MOX_IP_END" \
      --haproxy-ip-start "$HAPROXY_IP_START" \
      --haproxy-ip-end "$HAPROXY_IP_END" \
      --production-ip-start "$PRODUCTION_IP_START" \
      --production-ip-end "$PRODUCTION_IP_END" \
      --staging-ip-start "$STAGING_IP_START" \
      --staging-ip-end "$STAGING_IP_END" \
      --max-hosts "$MAX_MOX_HOSTS" \
      --production-tag "$PRODUCTION_VM_TAG" \
      --staging-tag "$STAGING_VM_TAG" \
      --evictable-tag "$EVICTABLE_VM_TAG" \
      --prod-cores "${PROD_VM_CORES:-4}" \
      --prod-memory-mb "$((${PROD_VM_MEMORY_GIB:-8} * 1024))" \
      --prod-disk-gib "${PROD_VM_DISK_GB:-64}" \
      --prod-disk-allocation sparse \
      --staging-cores "${STAGING_VM_CORES:-2}" \
      --staging-memory-mb "$((${STAGING_VM_MEMORY_GIB:-4} * 1024))" \
      --staging-disk-gib "${STAGING_VM_DISK_GB:-${PROD_VM_DISK_GB:-64}}" \
      --replication-interval "$replication_minutes" >/dev/null
    printf 'Initialized the registry in %s.\n' "$CLUSTER_STATE_DIR"
  fi
  "$REGISTRY" --state-dir "$CLUSTER_STATE_DIR" host-sync --live >/dev/null
  if [[ "$("$REGISTRY" --state-dir "$CLUSTER_STATE_DIR" control-get)" == null ]]; then
    "$REGISTRY" --state-dir "$CLUSTER_STATE_DIR" \
      control-set "$MOX_HOSTNAME" --expected-node none >/dev/null
  fi
}

create_haproxy_lxc() {
  local vmid="$HAPROXY_LXC_VMID" name="$HAPROXY_LXC_HOSTNAME"
  local ip_cidr="$HAPROXY_LXC_IP" gateway="$HAPROXY_LXC_GATEWAY"
  local bridge="$PROXMOX_PRIVATE_BRIDGE" storage="${HAPROXY_LXC_STORAGE:-local-zfs}"
  local rootfs_gb="${HAPROXY_LXC_ROOTFS_GB:-8}" memory_mb="${HAPROXY_LXC_MEMORY_MB:-1024}"
  local cores="${HAPROXY_LXC_CORES:-1}" created_lxc=0 template config net0 expected
  log "Creating the HAProxy LXC $vmid ($name) on $MOX_HOSTNAME"
  [[ "$(dpkg --print-architecture)" == amd64 && "$(uname -m)" == x86_64 ]]
  systemctl stop bmac-haproxy-route-sync.timer

  if ! pct status "$vmid" >/dev/null 2>&1; then
    created_lxc=1
    template="${HAPROXY_LXC_TEMPLATE:-}"
    if [[ -z "$template" ]]; then
      pveam update
      template="$(
        pveam available --section system |
          awk '$2 ~ /^debian-13-standard_.*_amd64[.]tar[.]zst$/ { print $2 }' |
          sort -V | tail -n1
      )"
    fi
    [[ "$template" =~ ^debian-13-standard_.*_amd64[.]tar[.]zst$ ]] || {
      echo "No Debian 13 AMD64 standard LXC template is available" >&2
      exit 1
    }
    [[ -f "/var/lib/vz/template/cache/$template" ]] || pveam download local "$template"
    pct create "$vmid" "local:vztmpl/$template" \
      --arch amd64 --tags bmac-fixed-haproxy \
      --hostname "$name" --unprivileged 1 --cores "$cores" --memory "$memory_mb" --swap 0 \
      --rootfs "${storage}:${rootfs_gb}" --onboot 1 --startup order=1 \
      --net0 "name=eth0,bridge=${bridge},ip=${ip_cidr},gw=${gateway},type=veth,firewall=1"
  fi
  pct set "$vmid" --onboot 1 --startup order=1
  config="$(pct config "$vmid")"
  grep -Fxq "arch: amd64" <<<"$config"
  grep -Fxq "hostname: $name" <<<"$config"
  net0="$(awk -F ': ' '$1 == "net0" { print $2; count++ } END { if (count != 1) exit 1 }' <<<"$config")"
  for expected in "bridge=$bridge" "gw=$gateway" "ip=$ip_cidr"; do
    case ",${net0}," in
      *",${expected},"*) ;;
      *)
        printf 'HAProxy LXC %s net0 is missing %s: %s\n' "$vmid" "$expected" "$net0" >&2
        exit 1
        ;;
    esac
  done
  pct start "$vmid" 2>/dev/null || true
  local attempt
  for attempt in $(seq 1 30); do
    pct exec "$vmid" -- true 2>/dev/null && break
    ((attempt < 30)) || {
      printf 'HAProxy LXC %s did not start.\n' "$vmid" >&2
      exit 1
    }
    sleep 2
  done

  # Give the private-only LXC outbound access before apt runs; the lasting
  # ingress and NAT table replaces this below.
  local lxc_ip="${ip_cidr%/*}" public_if
  public_if="$(public_interface)"
  [[ -n "$public_if" ]]
  sysctl -w net.ipv4.ip_forward=1 >/dev/null
  nft list table inet bmac_haproxy_bootstrap >/dev/null 2>&1 &&
    nft delete table inet bmac_haproxy_bootstrap
  nft add table inet bmac_haproxy_bootstrap
  nft 'add chain inet bmac_haproxy_bootstrap forward { type filter hook forward priority -151; policy accept; }'
  nft 'add chain inet bmac_haproxy_bootstrap postrouting { type nat hook postrouting priority srcnat; policy accept; }'
  nft add rule inet bmac_haproxy_bootstrap forward ip saddr "$lxc_ip" accept
  nft add rule inet bmac_haproxy_bootstrap forward ct state established,related accept
  nft add rule inet bmac_haproxy_bootstrap postrouting ip saddr "$lxc_ip" oifname "$public_if" masquerade

  pct exec "$vmid" -- bash -s -- "$created_lxc" < <(
    declare -f apt_lock_holder apt_wait_for_locks
    cat <<'LXC'
set -Eeuo pipefail
created_lxc="$1"
export LANG=C.UTF-8
export LC_ALL=C.UTF-8
cat >/etc/default/locale <<'EOF'
LANG=C.UTF-8
LC_ALL=C.UTF-8
EOF
apt_wait_for_locks
apt-get -o DPkg::Lock::Timeout=60 update
DEBIAN_FRONTEND=noninteractive apt-get -o DPkg::Lock::Timeout=60 install -y diffutils haproxy rsyslog
if [[ "$created_lxc" == 1 || ! -s /etc/haproxy/haproxy.cfg ]]; then
cat >/etc/haproxy/haproxy.cfg <<EOF
# Temporary reject-only bootstrap. sync_haproxy_routes.sh replaces this with
# the registry's route generation.
global
    log /dev/log local0
    log /dev/log local1 notice
    user haproxy
    group haproxy
    daemon

defaults
    log global
    timeout connect 5s
    timeout client  60s
    timeout server  60s

frontend public_http
    bind :80
    mode http
    option httplog
    default_backend no_configured_http_route

frontend public_tls
    bind :443
    mode tcp
    option tcplog
    default_backend no_configured_tls_route

backend no_configured_http_route
    mode http
    http-request deny deny_status 404

backend no_configured_tls_route
    mode tcp
    server no_route 127.0.0.1:1 disabled
EOF
fi
haproxy -c -f /etc/haproxy/haproxy.cfg
systemctl enable haproxy rsyslog
systemctl start rsyslog
systemctl reload-or-restart haproxy
LXC
  )
  pct exec "$vmid" -- ip -4 route show default |
    awk -v gateway="$gateway" '$1 == "default" && $3 == gateway { found=1 } END { exit !found }'
}

configure_ingress() {
  log "Sending public HTTP and HTTPS to the HAProxy LXC"
  local public_ip="$PROXMOX_IP" lxc_ip="${HAPROXY_LXC_IP%/*}" vmid="$HAPROXY_LXC_VMID" public_if
  public_if="$(public_interface)"
  [[ -n "$public_if" ]]
  cat >/etc/sysctl.d/90-bmac-haproxy-router.conf <<'EOF'
net.ipv4.ip_forward=1
EOF
  sysctl --system >/dev/null
  install -d -m 0755 /etc/nftables.d
  cat >/etc/nftables.d/bmac-haproxy-ingress.nft <<EOF
table inet bmac_haproxy_ingress {
  chain prerouting {
    type nat hook prerouting priority dstnat; policy accept;
    ip daddr $public_ip tcp dport { 80, 443 } dnat ip to $lxc_ip
  }
  chain forward {
    type filter hook forward priority -150; policy accept;
    ct state established,related accept
    iifname "$public_if" ip daddr $lxc_ip tcp dport { 80, 443 } accept
    ip saddr $lxc_ip accept
  }
  chain postrouting {
    type nat hook postrouting priority srcnat; policy accept;
    ip saddr $lxc_ip oifname "$public_if" masquerade
  }
}
EOF
  cat >/usr/local/sbin/bmac-load-haproxy-ingress <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
nft list table inet bmac_haproxy_ingress >/dev/null 2>&1 &&
  nft delete table inet bmac_haproxy_ingress
nft -f /etc/nftables.d/bmac-haproxy-ingress.nft
EOF
  chmod 0755 /usr/local/sbin/bmac-load-haproxy-ingress
  cat >/etc/systemd/system/bmac-haproxy-ingress.service <<EOF
[Unit]
Description=BMAC public ingress DNAT to this host's HAProxy LXC
After=network-online.target pve-container@${vmid}.service bmac-haproxy-route-sync.service
Wants=network-online.target bmac-haproxy-route-sync.service
[Service]
Type=oneshot
ExecStartPre=${SYNC} --assert-local-current
ExecStart=/usr/local/sbin/bmac-load-haproxy-ingress
RemainAfterExit=yes
[Install]
WantedBy=multi-user.target
EOF
  systemctl daemon-reload
  systemctl enable bmac-haproxy-ingress.service
  "$SYNC" --local-reconcile --lock-timeout 240
  systemctl restart bmac-haproxy-ingress.service

  nft list table inet bmac_haproxy_bootstrap >/dev/null 2>&1 &&
    nft delete table inet bmac_haproxy_bootstrap
  systemctl is-active bmac-haproxy-ingress.service
}

sync_routes() {
  log "Synchronizing the registry's routes across the online HAProxy LXCs"
  "$SYNC" --lock-timeout 240
  systemctl start bmac-haproxy-route-sync.timer
}

main() {
  (($# == 0)) || {
    printf 'Usage: orchestrate_host.sh\n' >&2
    exit 2
  }
  local host
  host="$(this_mox)"
  load_proxmox_config --host "$host"
  pvecm status 2>/dev/null | awk '$1 == "Quorate:" && $2 == "Yes" { found=1 } END { exit !found }' || {
    printf 'The cluster is not quorate.\n' >&2
    exit 1
  }
  configure_guest_egress
  configure_runtime
  create_haproxy_lxc
  configure_ingress
  sync_routes
  printf '\nRouting and orchestration are set up on %s.\n' "$host"
}

main "$@"
