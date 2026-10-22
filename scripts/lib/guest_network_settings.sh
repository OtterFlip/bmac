# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

# shellcheck shell=bash

# Print the guest-network settings file for the mox host whose cluster.conf
# and moxN.conf are currently loaded (load_proxmox_config --host moxN). The
# host's guest_network.sh adds its own NIC and bridge names. Host setup and
# sync_cluster_network.sh both use this, so they install identical settings.
guest_network_settings() {
  cat <<EOF
BMAC_HOST=${MOX_HOSTNAME}
BMAC_MOX_INDEX=${MOX_INDEX}
BMAC_PRIVATE_BRIDGE=${PROXMOX_PRIVATE_BRIDGE}
BMAC_PRIVATE_IP=${PROXMOX_SECONDARY_IP%/*}
BMAC_PUBLIC_IP=${PROXMOX_IP}
BMAC_PUBLIC_GATEWAY=${PROXMOX_GATEWAY}
BMAC_GUEST_GATEWAY_IP=${GUEST_GATEWAY_IP}
BMAC_GUEST_GATEWAY_MAC=${GUEST_GATEWAY_MAC,,}
BMAC_SLOT_FIRST=${VM_SLOT_START}
BMAC_SLOT_LAST=${VM_SLOT_END}
BMAC_MOX_FIRST=${MOX_IP_START}
BMAC_MAX_MOX_HOSTS=${MAX_MOX_HOSTS}
BMAC_TRANSFER_LIMIT_GIB=${PROXMOX_HOST_TRANSFER_MONTHLY_LIMIT_GIB:-}
BMAC_TRANSFER_RESET_DAY=${PROXMOX_HOST_TRANSFER_RESET_DAY_EFFECTIVE}
EOF
}
