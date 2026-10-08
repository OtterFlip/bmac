// Sample cluster state for the in-browser mock backend (development and
// screenshots only; the desktop app always talks to real scripts).

import type { GuestsState, HostsState, ReplicationState, StorageState } from "@/protocol/state";

const now = () => Math.floor(Date.now() / 1000);
const GiB = 1024 ** 3;
const TiB = 1024 ** 4;

export function hostsState(): HostsState {
  const t = now();
  return {
    collected_at: t,
    probe: "mox1",
    cluster: {
      name: "bmac",
      quorate: true,
      expected_votes: 3,
      total_votes: 3,
      quorum: 2,
      host_count: 3,
      online_count: 3,
      control_node: "mox1",
    },
    qdevice: { configured_host: "qdevice", registered: false, address: null, needed: false, voting: null },
    hosts: [
      host("mox1", 1, "10.213.0.11", 0.18, 48, 92, 256, true, true),
      host("mox2", 2, "10.213.0.12", 0.07, 48, 61, 256, false, false),
      host("mox3", 3, "10.213.0.13", 0.31, 64, 141, 384, false, false),
    ],
    problems: [],
  };
}

function host(
  name: string,
  nodeid: number,
  ip: string,
  cpu: number,
  cores: number,
  memUsedGiB: number,
  memTotalGiB: number,
  control: boolean,
  probe: boolean,
) {
  return {
    name,
    nodeid,
    ip,
    online: true,
    uptime_seconds: 86400 * (12 + nodeid * 3) + 3600 * nodeid,
    cpu_fraction: cpu,
    cores,
    memory_used: memUsedGiB * GiB,
    memory_total: memTotalGiB * GiB,
    disk_used: 38 * GiB,
    disk_total: 1.7 * TiB,
    slot_state: "active",
    is_control: control,
    is_probe: probe,
    ssh_from_here: true,
  };
}

export function guestsState(): GuestsState {
  const t = now();
  return {
    collected_at: t,
    probe: "mox1",
    production: [
      {
        name: "prod1",
        vmid: 100,
        registry_state: "active",
        live_status: "running",
        node: "mox1",
        ip: "10.213.0.101",
        placement: ["mox1", "mox2"],
        routes: 2,
        routes_enabled: true,
        domain: "app.example.com",
        cores: 8,
        memory_mb: 32768,
        disk_gib: 400,
        disk_bytes: 400 * 2 ** 30,
        uptime_seconds: 86400 * 9 + 4000,
        purpose: "web",
        aliases: ["www.example.com"],
        owner_node: "mox1",
        ha: { configured: true, requested_state: "started", state: "started", node: "mox1" },
        replication: { jobs: 1, failing: 0, targets: ["mox2"], oldest_last_sync: t - 74, errors: [] },
      },
      {
        name: "prod2",
        vmid: 101,
        registry_state: "active",
        live_status: "running",
        node: "mox3",
        ip: "10.213.0.102",
        placement: ["mox3", "mox1", "mox2"],
        routes: 1,
        routes_enabled: true,
        domain: "api.example.com",
        cores: 16,
        memory_mb: 65536,
        disk_gib: 800,
        disk_bytes: 816 * 2 ** 30,
        uptime_seconds: 86400 * 3 + 500,
        purpose: "api",
        aliases: [],
        owner_node: "mox3",
        ha: { configured: true, requested_state: "started", state: "started", node: "mox3" },
        replication: {
          jobs: 2,
          failing: 1,
          targets: ["mox1", "mox2"],
          oldest_last_sync: t - 2400,
          errors: ["command 'zfs snapshot' failed: dataset is busy"],
        },
      },
    ],
    staging: [
      {
        name: "stage1prod1",
        vmid: 200,
        registry_state: "active",
        live_status: "running",
        node: "mox2",
        ip: "10.213.0.201",
        placement: ["mox2"],
        routes: 1,
        routes_enabled: true,
        domain: "stage1.app.example.com",
        cores: 4,
        memory_mb: 16384,
        disk_gib: 400,
        disk_bytes: 400 * 2 ** 30,
        uptime_seconds: 7200,
        source: "prod1",
        url: "https://stage1.app.example.com",
      },
    ],
    unregistered: [],
    problems: ["prod2 has 1 failing replication job(s)"],
  };
}

export function replicationState(): ReplicationState {
  const t = now();
  return {
    collected_at: t,
    probe: "mox1",
    jobs: [
      job("100-0", 100, "prod1", "mox1", "mox2", t - 74, t + 46, 0, null),
      job("101-0", 101, "prod2", "mox3", "mox1", t - 51, t + 69, 0, null),
      job("101-1", 101, "prod2", "mox3", "mox2", t - 2400, t + 20, 3, "command 'zfs snapshot' failed: dataset is busy"),
    ],
    problems: [
      "replication job 101-1 (prod2 mox3 -> mox2) is failing: command 'zfs snapshot' failed: dataset is busy",
    ],
  };
}

function job(
  id: string,
  guest: number,
  name: string,
  source: string,
  target: string,
  last: number,
  next: number,
  fails: number,
  error: string | null,
) {
  return {
    id,
    guest,
    guest_name: name,
    source,
    target,
    schedule: "*/2",
    last_sync: last,
    last_try: last,
    next_sync: next,
    duration: 3.4,
    fail_count: fails,
    error,
    disabled: false,
  };
}

export function storageState(): StorageState {
  const pool = (size: number, used: number) => ({
    name: "rpool",
    health: "ONLINE",
    size: size * TiB,
    alloc: used * TiB,
    free: (size - used) * TiB,
    frag: 9,
    free_fraction: (size - used) / size,
  });
  const storages = (total: number, used: number) => [
    { storage: "local-zfs", type: "zfspool", active: true, enabled: true, total: total * TiB, used: used * TiB, avail: (total - used) * TiB, content: "images,rootdir" },
    { storage: "local", type: "dir", active: true, enabled: true, total: 1.2 * TiB, used: 0.04 * TiB, avail: 1.16 * TiB, content: "iso,vztmpl,backup" },
  ];
  return {
    collected_at: now(),
    probe: "mox1",
    hosts: [
      { node: "mox1", online: true, pools: [pool(3.5, 1.9)], storages: storages(3.3, 1.9) },
      { node: "mox2", online: true, pools: [pool(3.5, 2.1)], storages: storages(3.3, 2.1) },
      { node: "mox3", online: true, pools: [pool(7.0, 6.4)], storages: storages(6.6, 6.4) },
    ],
    problems: ["mox3: pool rpool has only 9% free"],
  };
}
