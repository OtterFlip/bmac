// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

// Sample status snapshots: three hosts, two production VMs (one with a
// failing replication job), a staging VM, a guest the registry doesn't
// know, and a pool getting full. Tests change copies of these.

import type {
  ClusterSnapshot,
  ClusterSummary,
  Condition,
  HostObservation,
  HostsSnapshot,
  Pool,
  QdeviceSummary,
  StorageObservation,
  StorageSnapshot,
  Vdev,
  VmsSnapshot,
} from "@bmac/ui";

const GiB = 2 ** 30;
const TiB = 2 ** 40;

export interface DemoStatus {
  cluster: ClusterSnapshot;
  hosts: HostsSnapshot;
  vms: VmsSnapshot;
  storage: StorageSnapshot;
}

function hostObservation(at_ms: number, cpu: number, memoryUsed: number): HostObservation {
  return {
    at_ms,
    bmac_version: "3.0.0",
    uptime_seconds: 86_400 * 12 + 3_600 * 5,
    cores: 16,
    cpu_fraction: cpu,
    load1: cpu * 16,
    memory_used: memoryUsed,
    memory_total: 64 * GiB,
    root_used: 40 * GiB,
    root_total: 900 * GiB,
  };
}

function pool(name: string, size: number, allocated: number): Pool {
  return {
    name,
    health: "ONLINE",
    size,
    allocated,
    free: size - allocated,
    fragmentation_percent: 9,
    free_fraction: (size - allocated) / size,
    scan: "scrub repaired 0B in 00:12:31 with 0 errors on Sun Oct  4 00:36:32 2026",
    remove: null,
    errors: "No known data errors",
  };
}

function mirror(host: string, size: number, allocated: number): Vdev {
  return {
    pool: "rpool",
    name: "mirror-0",
    type: "mirror",
    state: "ONLINE",
    status: "online",
    encryption: "luks",
    holds_esp: true,
    size,
    allocated,
    free: size - allocated,
    members: ["a", "b"].map((x, i) => ({
      path: `/dev/mapper/luks-rpool-${i}`,
      state: "ONLINE",
      disk: `/dev/nvme${i}n1`,
      serial: `S6${host.toUpperCase()}${x.toUpperCase()}0042`,
      model: "Samsung SSD 990 PRO 2TB",
      size: 2 * TiB,
      luks: true,
      mapper: `luks-rpool-${i}`,
      missing: false,
    })),
  };
}

function storageObservation(host: string, at_ms: number, allocated: number, freeDisk: boolean): StorageObservation {
  const size = 1.8 * TiB;
  return {
    at_ms,
    pools: [pool("rpool", size, allocated)],
    storages: [
      { storage: "local", type: "dir", active: true, enabled: true, total: 900 * GiB, used: 40 * GiB, avail: 860 * GiB, content: "iso,vztmpl,backup" },
      { storage: "local-zfs", type: "zfspool", active: true, enabled: true, total: size, used: allocated, avail: size - allocated, content: "images,rootdir" },
    ],
    vdevs: [mirror(host, size, allocated)],
    disks: freeDisk
      ? [
          {
            disk: "/dev/sda",
            serial: "WD-WX12A3456789",
            model: "WDC WD40EFRX",
            size: 4 * TiB,
            tran: "sata",
            contents: "blank",
            status: "available",
            label: "Available",
            detail: "Blank; it can be added to a pool.",
            in_use_reasons: [],
            removable: true,
          },
        ]
      : [],
    findings: [],
    luks_mismatches: [],
    pit_snapshots: [],
    errors: [],
  };
}

export function demoStatus(now = Date.now()): DemoStatus {
  const at = now - 4_000;
  const cluster: ClusterSummary = {
    name: "bmac",
    quorate: true,
    expected_votes: 3,
    total_votes: 3,
    quorum: 2,
    host_count: 3,
    online_count: 3,
    duty_host: "mox1",
  };
  const qdevice: QdeviceSummary = { registered: false, address: null, needed: false, voting: null };
  const replicationFailing: Condition = {
    id: "replication.failing",
    resource: "prod2",
    severity: "warning",
    message: "Replication job 102-1 from mox2 to mox3 is failing: command 'zfs recv' failed: dataset is busy",
    since_ms: now - 25 * 60_000,
    next_steps: [{ text: "Inspect prod2", operation: "diagnostics.prod_vm_state", args: { resource: "prod2" } }],
  };
  const poolFull: Condition = {
    id: "zfs.pool.capacity",
    resource: "mox2/rpool",
    severity: "warning",
    message: "mox2/rpool has 15% free (276 GiB of 1.8 TiB).",
    since_ms: now - 3 * 3_600_000,
    next_steps: [{ text: "Inspect mox2", operation: "diagnostics.host_state", args: { host: "mox2" } }],
  };
  const fresh = { observed_at_ms: at, reachable: true };
  return {
    cluster: { at_ms: now, served_by: "mox1", cluster, qdevice, conditions: [replicationFailing, poolFull] },
    hosts: {
      at_ms: now,
      served_by: "mox1",
      cluster,
      qdevice,
      hosts: [
        { name: "mox1", online: true, address: "10.10.0.1", slot_state: "member", is_duty: true, is_serving: true, freshness: fresh, observation: hostObservation(at, 0.18, 22 * GiB) },
        { name: "mox2", online: true, address: "10.10.0.2", slot_state: "member", is_duty: false, is_serving: false, freshness: fresh, observation: hostObservation(at, 0.41, 38 * GiB) },
        { name: "mox3", online: true, address: "10.10.0.3", slot_state: "member", is_duty: false, is_serving: false, freshness: fresh, observation: hostObservation(at, 0.07, 12 * GiB) },
      ],
      conditions: [],
    },
    vms: {
      at_ms: now,
      served_by: "mox1",
      production: [
        {
          name: "prod1",
          vmid: 101,
          registry_state: "active",
          live_status: "running",
          node: "mox1",
          ip: "10.10.0.101",
          placement: ["mox1", "mox2", "mox3"],
          routes_enabled: true,
          domain: "shop.example.com",
          cores: 4,
          memory_mb: 8192,
          disk_bytes: 64 * GiB,
          uptime_seconds: 86_400 * 3,
          purpose: "shop",
          aliases: ["www.shop.example.com"],
          owner_node: "mox1",
          ha: { configured: true, requested_state: "started", state: "started" },
          replication: { jobs: 2, failing: 0, targets: ["mox2", "mox3"], oldest_last_sync_ms: now - 90_000, errors: [] },
        },
        {
          name: "prod2",
          vmid: 102,
          registry_state: "active",
          live_status: "running",
          node: "mox2",
          ip: "10.10.0.102",
          placement: ["mox2", "mox3"],
          routes_enabled: true,
          domain: "blog.example.com",
          cores: 2,
          memory_mb: 4096,
          disk_bytes: 32 * GiB,
          uptime_seconds: 86_400,
          purpose: "blog",
          aliases: [],
          owner_node: "mox2",
          ha: { configured: true, requested_state: "started", state: "started" },
          replication: { jobs: 1, failing: 1, targets: ["mox3"], oldest_last_sync_ms: now - 26 * 60_000, errors: ["command 'zfs recv' failed: dataset is busy"] },
        },
      ],
      staging: [
        {
          name: "stage1prod1",
          vmid: 201,
          registry_state: "active",
          live_status: "running",
          node: "mox3",
          ip: "10.10.0.201",
          placement: ["mox3"],
          routes_enabled: true,
          domain: null,
          cores: 2,
          memory_mb: 4096,
          disk_bytes: 64 * GiB,
          uptime_seconds: 7_200,
          source: "prod1",
          url: "https://stage1prod1.example.com/",
          snapshot: { name: "bmac-staging-prod1-261011T081500Z", created_at_ms: now - 2 * 3_600_000, verified: true, shared_with: [] },
        },
      ],
      unregistered: [{ vmid: 900, name: "scratch", node: "mox3", status: "stopped" }],
      replication: [
        { id: "101-0", guest: 101, target: "mox2", schedule: "*/1", last_sync_ms: now - 60_000, last_try_ms: now - 60_000, next_sync_ms: now + 1_000, duration_seconds: 2.4, fail_count: 0, error: null, disabled: false, source: "mox1", guest_name: "prod1" },
        { id: "101-1", guest: 101, target: "mox3", schedule: "*/1", last_sync_ms: now - 90_000, last_try_ms: now - 30_000, next_sync_ms: now + 30_000, duration_seconds: 3.1, fail_count: 0, error: null, disabled: false, source: "mox1", guest_name: "prod1" },
        { id: "102-1", guest: 102, target: "mox3", schedule: "*/1", last_sync_ms: now - 26 * 60_000, last_try_ms: now - 50_000, next_sync_ms: now + 10_000, duration_seconds: null, fail_count: 25, error: "command 'zfs recv' failed: dataset is busy", disabled: false, source: "mox2", guest_name: "prod2" },
      ],
      stale_hosts: [],
      conditions: [replicationFailing],
    },
    storage: {
      at_ms: now,
      served_by: "mox1",
      hosts: [
        { host: "mox1", online: true, freshness: fresh, observation: storageObservation("mox1", at, 0.6 * TiB, false) },
        { host: "mox2", online: true, freshness: fresh, observation: storageObservation("mox2", at, 1.53 * TiB, false) },
        { host: "mox3", online: true, freshness: fresh, observation: storageObservation("mox3", at, 0.4 * TiB, true) },
      ],
      conditions: [poolFull],
    },
  };
}
