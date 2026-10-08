// Result shapes of the fast read-only diagnostics (lib/quick_state.py). The
// dashboard renders these; it never computes cluster state itself.

export interface HostRow {
  name: string;
  nodeid: number | null;
  ip: string | null;
  online: boolean;
  uptime_seconds: number | null;
  cpu_fraction: number | null;
  cores: number | null;
  memory_used: number | null;
  memory_total: number | null;
  disk_used: number | null;
  disk_total: number | null;
  slot_state: string | null;
  is_control: boolean;
  is_probe: boolean;
  ssh_from_here: boolean | null;
}

export interface HostsState {
  collected_at: number;
  probe: string | null;
  cluster: {
    name: string | null;
    quorate: boolean | null;
    expected_votes: number | null;
    total_votes: number | null;
    quorum: number | null;
    host_count: number;
    online_count: number;
    control_node: string | null;
  };
  qdevice: {
    configured_host: string | null;
    registered: boolean | null;
    address: string | null;
    needed: boolean | null;
    voting: boolean | null;
  };
  hosts: HostRow[];
  problems: string[];
}

interface GuestCommon {
  name: string;
  vmid: number | null;
  registry_state: string | null;
  live_status: string;
  node: string | null;
  ip: string | null;
  placement: string[];
  routes: number;
  routes_enabled: boolean;
  domain: string | null;
  cores: number | null;
  memory_mb: number | null;
  disk_gib: number | null;
  /** Exact current root disk size; differs from disk_gib after an online growth. */
  disk_bytes: number | null;
  uptime_seconds: number | null;
}

export interface ProductionRow extends GuestCommon {
  purpose: string | null;
  aliases: string[];
  owner_node: string | null;
  ha: { configured: boolean; requested_state: string | null; state: string | null; node: string | null };
  replication: {
    jobs: number;
    failing: number;
    targets: string[];
    oldest_last_sync: number | null;
    errors: string[];
  };
}

export interface StagingRow extends GuestCommon {
  source: string | null;
  url: string | null;
}

export interface GuestsState {
  collected_at: number;
  probe: string | null;
  production: ProductionRow[];
  staging: StagingRow[];
  unregistered: { vmid: number; name: string | null; node: string | null; status: string | null }[];
  problems: string[];
}

export interface ReplicationJob {
  id: string;
  guest: number | null;
  guest_name: string | null;
  source: string | null;
  target: string | null;
  schedule: string | null;
  last_sync: number | null;
  last_try: number | null;
  next_sync: number | null;
  duration: number | null;
  fail_count: number;
  error: string | null;
  disabled: boolean;
}

export interface ReplicationState {
  collected_at: number;
  probe: string | null;
  jobs: ReplicationJob[];
  problems: string[];
}

export interface PoolRow {
  name: string;
  health: string | null;
  size: number;
  alloc: number;
  free: number;
  frag: number | null;
  free_fraction: number | null;
}

export interface StorageRow {
  storage: string;
  type: string | null;
  active: boolean;
  enabled: boolean;
  total: number | null;
  used: number | null;
  avail: number | null;
  content: string | null;
}

export interface StorageState {
  collected_at: number;
  probe: string | null;
  hosts: { node: string; online: boolean; pools: PoolRow[]; storages: StorageRow[] }[];
  problems: string[];
}

export interface StateSources {
  list_hosts: HostsState;
  list_guests: GuestsState;
  list_replication: ReplicationState;
  list_storage: StorageState;
}

export type SourceId = keyof StateSources;
export const SOURCE_IDS: SourceId[] = ["list_hosts", "list_guests", "list_replication", "list_storage"];
