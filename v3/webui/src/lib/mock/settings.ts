// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Sample settings for the mock backend: the keys bmacd describes, v2's
// example cluster.conf, facts for mox1 to mox3, and each host's published
// certificate and SSH host key fingerprints.

import type {
  CertificateView, HostFactsRecord, HostIdentity, HostSlot, SettingKey, SettingsView } from "@bmac/ui";

type KeySpec = [name: string, kind: string, required?: boolean, choices?: string[]];

/** The cluster's address plan, which bmacd fixes once the cluster settings exist. */
const LOCKED = new Set([
  "PRIVATE_SUBNET_CIDR", "MAX_MOX_HOSTS", "MOX_IP_START", "MOX_IP_END", "MOX_REPLICATION_IP_START", "MOX_REPLICATION_IP_END",
  "HAPROXY_IP_START", "HAPROXY_IP_END", "PRODUCTION_IP_START", "PRODUCTION_IP_END", "STAGING_IP_START", "STAGING_IP_END",
]);

const key = ([name, kind, required = false, choices = []]: KeySpec): SettingKey => ({ name, kind, choices, required, locked: LOCKED.has(name) });

export const CLUSTER_KEYS: SettingKey[] = (
  [
    ["PROXMOX_CLUSTER_NAME", "text"],
    ["PROXMOX_QDEVICE_HOST", "text"],
    ["PROXMOX_INTERNAL_DOMAIN", "internal_domain", true],
    ["MAX_MOX_HOSTS", "host_count", true],
    ["PRIVATE_SUBNET_CIDR", "private_subnet", true],
    ["PROXMOX_PRIVATE_BRIDGE", "interface"],
    ["PROXMOX_MIGRATION_NETWORK", "network", true],
    ["DATACENTER_PRIVATE_VLAN_GATEWAY", "private_address", true],
    ["GUEST_EGRESS_VIP", "private_interface", true],
    ["MOX_IP_START", "private_address", true],
    ["MOX_IP_END", "private_address", true],
    ["MOX_REPLICATION_IP_START", "private_address", true],
    ["MOX_REPLICATION_IP_END", "private_address", true],
    ["HAPROXY_IP_START", "private_address", true],
    ["HAPROXY_IP_END", "private_address", true],
    ["PRODUCTION_IP_START", "private_address", true],
    ["PRODUCTION_IP_END", "private_address", true],
    ["STAGING_IP_START", "private_address", true],
    ["STAGING_IP_END", "private_address", true],
    ["PROD_VM_CORES", "positive_integer"],
    ["PROD_VM_MEMORY_GIB", "positive_integer"],
    ["PROD_VM_DISK_GB", "positive_integer"],
    ["STAGING_VM_CORES", "positive_integer"],
    ["STAGING_VM_MEMORY_GIB", "positive_integer"],
    ["VM_BLOCK_SIZES", "block_sizes"],
    ["PROXMOX_ISO_FILE_URL", "iso_url"],
    ["PROXMOX_ISO_FILE_SHA256", "sha256"],
    ["PROD_GUEST_OS_INSTALL_MODE", "choice", false, ["ubuntu-autoinstall", "manual"]],
    ["PROXMOX_TIMEZONE", "text"],
    ["TAILSCALE_TAG", "text"],
    ["ADMIN_1_PUBLIC_SSH_KEY", "ssh_public_key"],
    ["ADMIN_2_PUBLIC_SSH_KEY", "ssh_public_key"],
  ] as KeySpec[]
).map(key);

export const HOST_KEYS: SettingKey[] = (
  [
    ["IDRAC_IP", "ipv4"],
    ["PROXMOX_IP", "ipv4", true],
    ["PROXMOX_GATEWAY", "ipv4", true],
    ["PROXMOX_PREFIX", "prefix_length", true],
    ["PROXMOX_PUBLIC_MAC", "mac", true],
    ["PROXMOX_SECONDARY_MAC", "mac_or_auto", true],
    ["TAILSCALE_HOSTNAME", "text"],
    ["MAX_PROD_VM_COUNT_ON_THIS_HOST", "positive_integer", true],
    ["MAX_STAGING_VM_COUNT_ON_THIS_HOST", "positive_integer", true],
    ["NVME_MIRROR_0_SERIAL_1", "text"],
    ["NVME_MIRROR_0_SERIAL_2", "text"],
    ["NVME_MIRROR_0_CAPACITY_BYTES_1", "positive_integer"],
    ["NVME_MIRROR_0_CAPACITY_BYTES_2", "positive_integer"],
  ] as KeySpec[]
).map(key);

/** v2's example `cluster.conf`, required keys and a few others. */
export const DEMO_CLUSTER_VALUES: Record<string, string> = {
  PROXMOX_CLUSTER_NAME: "MyAppCloud",
  PROXMOX_QDEVICE_HOST: "qdevice",
  PROXMOX_INTERNAL_DOMAIN: "pve.internal",
  MAX_MOX_HOSTS: "10",
  PRIVATE_SUBNET_CIDR: "10.213.0.0/24",
  PROXMOX_MIGRATION_NETWORK: "10.213.0.240/28",
  DATACENTER_PRIVATE_VLAN_GATEWAY: "10.213.0.1",
  GUEST_EGRESS_VIP: "10.213.0.10/24",
  MOX_IP_START: "10.213.0.11",
  MOX_IP_END: "10.213.0.20",
  MOX_REPLICATION_IP_START: "10.213.0.241",
  MOX_REPLICATION_IP_END: "10.213.0.250",
  HAPROXY_IP_START: "10.213.0.21",
  HAPROXY_IP_END: "10.213.0.30",
  PRODUCTION_IP_START: "10.213.0.31",
  PRODUCTION_IP_END: "10.213.0.50",
  STAGING_IP_START: "10.213.0.51",
  STAGING_IP_END: "10.213.0.200",
  PROD_VM_CORES: "64",
  PROD_GUEST_OS_INSTALL_MODE: "ubuntu-autoinstall",
};

const SSH_KEYS = [
  "AAAAC3NzaC1lZDI1NTE5AAAAIBhE1+iRU5AP8b2hmfLfAHJQ/BVRAR0n4ddx0DFCnkg4",
  "AAAAC3NzaC1lZDI1NTE5AAAAIK9mZ2hvc3QtbW94Mi1ob3N0LWtleS1mb3ItZGVtbw01",
  "AAAAC3NzaC1lZDI1NTE5AAAAIK9mZ2hvc3QtbW94My1ob3N0LWtleS1mb3ItZGVtbw02",
];

export function fingerprint(seed: number): string {
  return Array.from({ length: 32 }, (_, i) => ((seed * 37 + i * 11) % 256).toString(16).padStart(2, "0").toUpperCase()).join(":");
}

export function demoSettings(now = Date.now()): SettingsView {
  const hosts: HostFactsRecord[] = [1, 2, 3].map((n) => ({
    host: `mox${n}` as HostSlot,
    revision: n === 2 ? 3 : 1,
    values: {
      PROXMOX_IP: `203.0.113.1${n}`,
      PROXMOX_GATEWAY: "203.0.113.1",
      PROXMOX_PREFIX: "24",
      PROXMOX_PUBLIC_MAC: `aa:bb:cc:dd:ee:0${n}`,
      PROXMOX_SECONDARY_MAC: "auto",
      MAX_PROD_VM_COUNT_ON_THIS_HOST: "2",
      MAX_STAGING_VM_COUNT_ON_THIS_HOST: "4",
      NVME_MIRROR_0_SERIAL_1: `S6MOX${n}A0041`,
      NVME_MIRROR_0_SERIAL_2: `S6MOX${n}A0042`,
    },
    updated_by: "root@pam",
    updated_at_ms: now - n * 86_400_000,
  }));
  const identities: HostIdentity[] = [1, 2, 3].map((n) => ({
    host: `mox${n}` as HostSlot,
    https_fingerprint: fingerprint(n),
    ssh_host_key: `ssh-ed25519 ${SSH_KEYS[n - 1]}`,
    ssh_host_key_fingerprint: n === 1 ? "SHA256:6pyryPWb/Q2XEYAJxJkDp9mclGwSmIvrNn6QEmRg9gY" : `SHA256:demoMox${n}HostKeyFingerprintForTheMockOnly${n}`,
    updated_at_ms: now - n * 60_000,
  }));
  return {
    cluster: { revision: 4, values: { ...DEMO_CLUSTER_VALUES }, updated_by: "root@pam", updated_at_ms: now - 2 * 86_400_000 },
    hosts,
    cluster_keys: CLUSTER_KEYS,
    host_keys: HOST_KEYS,
    identities,
    can_edit: true,
  };
}

const DAY_MS = 86_400_000;

/** mox1's certificate as the Deployer made it, matching its published fingerprint. */
export function demoCertificate(now = Date.now()): CertificateView {
  return {
    host: "mox1",
    current: {
      fingerprint: fingerprint(1),
      names: ["mox1", "mox1.lab.internal", "mox1.example.ts.net", "10.213.0.11"],
      not_before_ms: now - 120 * DAY_MS,
      not_after_ms: now + (3650 - 120) * DAY_MS,
    },
    pending: null,
    can_rotate: true,
  };
}
