// Result shapes of the fast read-only diagnostics (scripts/utilities/quick_state.py). The
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
  /** Public-interface transfer this month, read over SSH from this workstation; null if not read. */
  transfer: HostTransfer | null;
}

/** scripts/host_runtime/transfer_counter.py status: inbound + outbound bytes since the last reset. */
export interface HostTransfer {
  total_bytes: number;
  /** PROXMOX_HOST_TRANSFER_MONTHLY_LIMIT_GIB, or null when the host has no limit. */
  limit_gib: number | null;
  used_percent: number | null;
  period_start: string | null;
  next_reset: string | null;
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

export interface StagingSnapshot {
  name: string;
  /** Epoch seconds from the UTC stamp in the snapshot name; null if absent. */
  created_at: number | null;
  verified: boolean;
  /** Other staging VMs cloned from this snapshot; it is deleted with the last one. */
  shared_with: string[];
}

/** A backup-linked staging VM's restore point on PBS. */
export interface StagingRestorePoint {
  snapshot: string;
  storage: string | null;
  /** Epoch seconds from the restore point's name; null if unreadable. */
  created_at: number | null;
}

export interface StagingRow extends GuestCommon {
  source: string | null;
  url: string | null;
  snapshot: StagingSnapshot | null;
  /** prod: a linked clone of a production snapshot; backup: an overlay over a restore point. */
  link?: "prod" | "backup";
  restore_point?: StagingRestorePoint | null;
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

export interface VdevMember {
  path: string;
  state: string | null;
  disk: string | null;
  serial: string | null;
  model: string | null;
  size: number | null;
  luks: boolean;
  mapper: string | null;
  /** The member no longer resolves to an installed disk. */
  missing: boolean;
}

export type VdevStatus = "online" | "degraded" | "faulted" | "evacuating" | "resilvering";

export interface VdevRow {
  pool: string;
  name: string;
  type: string;
  state: string | null;
  status: VdevStatus;
  encryption: "luks" | "none" | "mixed" | "unknown";
  holds_esp: boolean;
  size: number | null;
  allocated: number | null;
  free: number | null;
  members: VdevMember[];
}

export type DiskStatus =
  | "available"
  | "awaiting_finalization"
  | "evacuating"
  | "pending_replacement"
  | "pending_addition"
  | "in_use"
  | "no_serial";

export interface DiskRow {
  disk: string;
  serial: string | null;
  model: string | null;
  size: number | null;
  tran: string | null;
  contents: "blank" | "partitions or signatures" | "mounted";
  status: DiskStatus;
  label: string;
  detail: string;
  in_use_reasons: string[];
  /** Nothing holds the disk; it can be pulled physically. */
  removable: boolean;
}

export interface DiskHost {
  node: string;
  online: boolean;
  readable: boolean;
  error: string | null;
  pools: { name: string; state: string | null; health: string | null; scan: string | null; remove: string | null; errors: string | null }[];
  vdevs: VdevRow[];
  /** Physical disks that are not in any pool. */
  disks: DiskRow[];
  removals: { vdev: string; status: string; serials: string[]; requested_at: number | null }[];
}

export interface DisksState {
  collected_at: number;
  hosts: DiskHost[];
  problems: string[];
}

/** scripts/user_callable/diagnostics/check_qdevice_access.sh: needs no cluster member. */
export interface QDeviceAccessState {
  configured_host?: string;
  accessible: boolean;
  problem?: string;
}

/** scripts/user_callable/backups/list_backups.sh (quick_state.py render backups). */
export interface BackupStorage {
  configured: boolean;
  id: string;
  /** Null until the storage exists. */
  active: boolean | null;
  server: string | null;
  port: number | null;
  datastore: string | null;
  namespace: string | null;
  username: string | null;
  key_fingerprint: string | null;
  master_pubkey: boolean;
}

export interface BackupRetention {
  /** False when the cluster's token cannot read the PBS prune jobs; nothing is guessed. */
  readable: boolean;
  jobs: { id: string; schedule: string | null; keep: Record<string, number> }[];
  error: string | null;
}

export interface BackupJob {
  id: string;
  enabled: boolean;
  schedule: string;
  /** 1, 2, 3, 4, 6, 8, 12 or 24; null for a schedule BMAC did not write. */
  every_hours: number | null;
  next_run: number | null;
  bwlimit: number | null;
}

export interface BackupVm {
  name: string;
  vmid: number | null;
  purpose: string | null;
  fqdn: string | null;
  state: string | null;
  job: BackupJob | null;
  latest_backup: number | null;
  restore_points: number;
}

/** How a backup was made, read once from its vzdump task log and cached on the workstation. */
export interface BackupMetrics {
  /** fast-incremental: only blocks changed since the previous backup were read. */
  mode: "fast-incremental" | "full-read";
  detail: string;
  dirty_bytes: number | null;
  /** What vzdump read and processed ("transferred" in its log), not what it uploaded. */
  read_bytes: number | null;
  zero_bytes: number | null;
  reused_bytes: number | null;
  reused_percent: number | null;
  /** Data PBS did not have yet: the image size less what was reused (approximate). */
  new_bytes: number | null;
  transfer_seconds: number | null;
  duration_seconds: number | null;
  node: string;
  task: string;
}

export interface RestorePoint {
  volid: string;
  vmid: number | null;
  time: number | null;
  /** The prodN named in the snapshot's notes, which outlives a removed VM. */
  name: string | null;
  purpose: string | null;
  fqdn: string | null;
  cluster: string | null;
  notes: string;
  /** The comment given when the backup was made, if any. */
  comment: string | null;
  protected: boolean;
  size: number | null;
  encrypted: string | null;
  /** Encrypted with this cluster's current key, so it can be restored here. */
  key_matches: boolean;
  verified: string | null;
  /** Null when no task log for it was found (pruned from PVE's task history, or its host is offline). */
  metrics: BackupMetrics | null;
}

export interface BackupsState {
  collected_at: number;
  probe: string | null;
  storage: BackupStorage;
  retention: BackupRetention;
  vms: BackupVm[];
  restore_points: RestorePoint[];
  /** Backups in the namespace without BMAC's notes; counted, never listed. */
  unmanaged_backups: number;
  problems: string[];
}

export interface StateSources {
  list_hosts: HostsState;
  list_guests: GuestsState;
  list_replication: ReplicationState;
  list_storage: StorageState;
  list_disks: DisksState;
  check_qdevice_access: QDeviceAccessState;
  list_backups: BackupsState;
}

export type SourceId = keyof StateSources;
export const SOURCE_IDS: SourceId[] = ["list_hosts", "list_guests", "list_replication", "list_storage", "list_disks", "check_qdevice_access", "list_backups"];
/** Read together as soon as the dashboard is ready, so every page has its state. */
export const STARTUP_SOURCE_IDS: SourceId[] = ["list_backups", "list_replication", "list_guests", "list_disks", "list_storage", "list_hosts"];
/** The sources read through a cluster member; check_qdevice_access is not one of them. */
export const CLUSTER_SOURCE_IDS: SourceId[] = SOURCE_IDS.filter((id) => id !== "check_qdevice_access");
