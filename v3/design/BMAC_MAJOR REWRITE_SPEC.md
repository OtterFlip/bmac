<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
Proposal for implementation. Confirm target PVE/OpenZFS versions and live repository behavior before enabling destructive operations.
-->

# BMAC Re-architecture and Offsite Backup: Integrated Design and Requirements

**Integrated engineering design and requirements specification**

- **Version:** 4.0, native Rust coordinator workflows and test-contract migration, 2026-10-10
- **Status:** Implementation specification, subject to explicitly defined validation gates; not a claim of implemented features
- **Supersedes:** `BMAC_FINAL_ARCHITECTURE_AND_BACKUP_SPEC.md` (v3), `BMAC_CLUSTER_WEB_AND_BACKUP_ARCHITECTURE_V2.md`, and `BMAC_OFFSITE_BACKUP_DESIGN_AND_REQUIREMENTS.md` for new implementation
- **Audience:** BMAC implementers, coding agents, operators, and reviewers
- **Primary design objective:** Move all running BMAC operations and persistent operational state off the administrator workstation and onto the Proxmox cluster, while retaining a small desktop ISO factory and adding independently trusted Linux backup appliances.

> **Normative precedence.** Everything in Part A is authoritative for platform architecture. Part B integrates the detailed ZFS backup requirements from the earlier design. If an older backup subsection mentions the former Tauri Dashboard or a standalone coordinator, read that term as the **cluster-hosted Web Dashboard** or **per-host coordinator** as defined here. The old desktop-operator model and the production human-callable Bash/Python workflow engine are retired. All installed BMAC operational workflows execute as native Rust coordinator functionality. Legacy scripts and tests are authoritative *reference material during migration*, not permanent production runtime dependencies. No requirement in Part B can override the security, quorum, or execution rules of Part A. Version-sensitive assumptions require tests before being relied upon.

---

# PART A - BMAC PLATFORM RE-ARCHITECTURE

## A0. Executive decisions and hard boundaries

1. **Desktop:** The existing Tauri/Rust desktop application becomes **BMAC Host Deploy**. Its only operational features are creating temporary customized Proxmox host installation ISOs and temporary customized Ubuntu Server backup appliance installation ISOs. It does not manage live guests, storage, backups, cluster members, staging, alerts, or production workflows after installation. It MAY assist with offline ISO-building prerequisites and display installation instructions and artifacts only.
2. **Per-host service:** Every Proxmox host runs a systemd-managed, general-purpose **`bmacd`** Rust daemon. This is the authoritative native Rust execution and orchestration implementation for cluster operations, scheduled backups, cluster synchronization, administration jobs, status and notifications.
3. **Web Dashboard:** Every Proxmox host serves the same cluster management user experience over HTTPS. Any appropriately authorized administrator can use any reachable host. No user action or scheduled operation requires a logged-in desktop or browser after admission.
4. **Shared truth:** There is one **logical** cluster state and one mutation/leader authority at a time. Per-host daemon instances do not imply separate independent BMAC databases. Shared durable decisions are quorum-protected and reconciled against live Proxmox/ZFS state.
5. **Native Rust operational engine:** **Port the entire live BMAC Bash/Python operational workflow engine into idiomatic Rust modules inside `bmacd`.** Do not ship the original human-callable operational scripts as the implementation of cluster operations, and do not merely rehost them under Rust subprocess supervision. Preserve behavior, safety, command ordering, error handling and external integration contracts proven by the existing scripts and **more than 500 existing unit tests (maintainer-reported; count and freeze actual baseline at migration start)**. Use the tests as a migration specification; create Rust tests with equivalent or stronger assertions. The old `bmac-ui v1` NDJSON protocol remains valuable as reference fixtures but is not required as a live script protocol.
6. **UI technology:** Preferred **Rust + Tokio + Axum + Askama + HTMX + Tailwind CSS**, server-rendered with polling. No new public general-purpose JSON administration API is required; internal authenticated HTTP routes are unavoidable and explicitly specified. Keep the current BMAC Dashboard layout, colors, hierarchy, activity presentation, controls, and workflows as close as practical; visual parity is a release gate.
7. **Browser authentication:** Default to an **opaque, revocable server-side session** with a Secure, HttpOnly, SameSite cookie and CSRF protection. Do not put credentials or JWTs in localStorage/sessionStorage. JWT bearer tokens kept only in browser memory MAY be offered later for an explicitly designed API client, but are **not** the recommended auth model for server-rendered HTMX. Session stores and permissions are server authoritative.
8. **HTTPS identity:** The **BMAC Host Deploy** desktop app generates a **unique host-specific self-signed HTTPS certificate** and private key as part of creating each customized Proxmox installer ISO, embeds them for secure installation, and prominently displays/exports the **SHA-256 certificate fingerprint** to the operator through an independent channel. Every Proxmox host serves the Web Dashboard over HTTPS with that certificate. Browser warnings for self-signed certificates are expected; the operator **MUST compare the presented certificate fingerprint to the previously recorded fingerprint before accepting an exception**. An operator-supplied trusted certificate is optional, not the default. Keep browser HTTPS certificates and cluster machine-authentication credentials separate. Web listener defaults to management/Tailscale access; no public WAN exposure by default.
9. **Backup appliance:** Independent Linux host with LUKS2 (manual local unlock), ZFS zvols, `bmac-backupd` receiver, appliance-owned retention/scrubs, distinct identities. Installed by either a custom unattended-capable Ubuntu Server ISO or a standalone `.deb` package.
10. **Appliance onboarding:** The custom appliance ISO contains one-use/short-lived enrollment material and bootstrap configuration to join the intended Tailscale network and securely register itself to its intended cluster. It **never** contains a durable appliance admin secret or LUKS passphrase. Cluster policy must independently validate enrollment.
11. **Independent appliance trust:** Compromising *all* Proxmox hosts and their backup-sender identities must not grant appliance shell access or the ability to erase/rewrite protected PIT history. The preferred appliance ingest is a restricted, mutually authenticated TLS receiver over TCP, including on Tailscale. No cluster-to-appliance root SSH credential exists.
12. **Notifications:** `bmacd` sends optional Postmark email alerts according to configurable event and recipient policies. Provision the Postmark server API token as an optional `cluster.conf` input, distribute only to protected server-side secret storage, and deduplicate at the leader. The system functions fully without Postmark.
13. **External command boundary:** Rust owns decisions, state machines, input validation, policies, reconciliation and sequencing. External platform executables, daemons and APIs (for example Proxmox, ZFS, networking, and package tools) remain external dependencies as appropriate; call them through typed, tested Rust adapters with strictly constructed arguments, typed results and explicit failure handling. Rust's compiler does not type-check external program behavior; the inherited tests and end-to-end verification cover that boundary.
14. **ISO lifecycle:** Every generated customized ISO is sensitive and **single-purpose / use-and-destroy**. It must be deleted securely to the extent practical after its intended use; embedded ephemeral enrollment credentials must also be revoked/expire, since file deletion alone does not revoke them.
15. **No new downtime regression:** All existing HA, cluster administration, staging clone, disk growth, replication, QDevice, failover and diagnostic workflows remain semantically equivalent and are subjected to regression testing on live hardware.

## A1. Terms and security domains

**Host:** One Proxmox cluster node. **Cluster coordinator:** One elected `bmacd` instance authorized to make shared decisions. **Host worker:** A `bmacd` instance running eligible tasks on its own host. **Web node:** Any host serving BMAC HTTPS pages. **Web session:** An authenticated human administrator browser session, **not** a service identity. **PIT:** A backed-up point-in-time VM recovery set including disk states and configuration. **Backup appliance:** Separate Linux/ZFS machine not participating in Proxmox Corosync or HA. **Cluster writer:** Restricted appliance-facing service identity. **Appliance admin:** Independent human or local-console identity with destructive retention authority. **Installer:** Temporary workstation application and generated ISO, never a permanent source of cluster truth. **Source of truth:** A combination of authoritative BMAC configuration, verified live Proxmox/ZFS state, and authoritative appliance PIT publication records; not an in-browser cache.

Mandatory isolation: desktop installer credentials; browser-admin credentials; per-host machine credentials; Proxmox cluster credentials; backup-writer credentials; independent appliance-admin credentials; peer-export identities; Postmark token; and LUKS passphrases MUST NOT be interchangeable.

## A2. Component architecture

```text
Admin workstation                            Proxmox cluster (each host)
+---------------------------+               +----------------------------------+
| BMAC Host Deploy (Tauri)  |               | bmacd (Rust/systemd)             |
| 1. build Proxmox ISO      | -- ISO -->    |  - Axum HTTPS Web Dashboard     |
| 2. build Ubuntu backup ISO|               |  - Auth/RBAC/CSRF                 |
+---------------------------+               |  - Native Rust operation engine  |
                                            |  - Local worker and job logs      |
Admin browser (any machine)  -- HTTPS -->   |  - Quorum-aware coordinator      |
                                            |  - Backup scheduling/notifications|
                                            +------------+---------------------+
                                                         |
                                    +--------------------+--------------------+
                                    | quorum-replicated small desired state  |
                                    | guarded leader/operation admissions    |
                                    +-----------------------------------------+
                                                         |
                       signed/restricted mTLS data transfer and limited metadata queries
                                                         |
                            +----------------------------+-----------------+
                            | Independent backup appliance               |
                            | Ubuntu + manual LUKS unlock + OpenZFS      |
                            | bmac-backupd receiver / limited exports    |
                            | independent retention/scrub/admin service  |
                            +---------------------------------------------+
```

Every host's web UI reads the same authoritative desired configuration and cluster job index. Live status may be slightly stale and **must** show observation time/source. Long-running jobs execute on the appropriate host and are visible to all administrators; web processes only observe and issue authorized job requests. Disconnecting the browser never kills admitted jobs.

### A2.1 Trust boundary table

| Actor / context | Permitted | Forbidden |
|---|---|---|
| Desktop installer | Prepare and inspect ISOs; safely erase artifacts | Manage running cluster or hold appliance-retention credentials |
| Browser user | Authorized UI requests by RBAC | Shell execution, direct ZFS operations, unmediated host commands |
| Web node/follower | Read shared state, display status, proxy/forward admitted commands to leader, run local worker | Independently commit conflicting cluster mutation |
| Elected coordinator | Admission, scheduling, orchestration, notifying, safe per-host dispatch | Ignore quorum loss, bypass live state checks, appliance-admin deletes |
| Host worker | Execute allowlisted Rust workflow with explicit typed input under proper authority | Arbitrary web-specified argv/path, unsupervised root shell |
| Cluster backup writer | Add authorized PITs, resume receive, read its own checkpoint/health | Delete/change retention/history; read historical guest data; login on appliance |
| Appliance retention service | Local pruning, holds, scrubs, branch lifetime according to configured policy | Accept destructive commands from cluster writer |
| Appliance administrator | Separate management and sensitive retention approvals | Reuse routine cluster writer credentials |

## A3. Deployment and packaging

### A3.1 Packages and binaries

- `bmac-host-deploy` (existing Tauri 2 desktop app, repurposed): Linux desktop package; contains only the two ISO-building workflows and their UI. Old management pages and live operational commands are **removed** after parity transition, not left hidden.
- `bmacd` (Rust service package for Proxmox hosts): daemon, web assets/templates, **native Rust workflow library and external-tool adapters**, registration, scheduler, notifications, local helpers, service definition, log rotation, and upgrade/uninstall workflows. Installed on **every** Proxmox host. Do **not** package a second, operational Bash/Python script engine for managed cluster workflows.
- `bmac-backupd` (Ubuntu appliance `.deb`): receiver, narrowly privileged storage helper, separate local retention/scrub processes, health endpoints, manifest journal, CLI, systemd units, secure enrollment and upgrade tooling.
- `bmac-cli` (optional thin Rust subcommand/client): diagnostic, read-only and explicitly authorized administrative interface into the **same typed Rust operation registry**, never a second implementation or a bypass around quorum, permissions or locks. A production human-callable legacy-script engine is not required.
- Shared versioned protocols for `bmacd` job input/events, host RPC, appliance backup transport, and durable manifests.

Build and sign release artifacts, embed/verify exact compatible hashes and versions in created ISOs, and refuse unverified bootstrap downloads. Do not `curl | bash` unpinned scripts. Support rolling host upgrades with backward-compatible read-only operation and explicit mutation compatibility gates. A host running an incompatible version must report that fact, not silently process new state.

### A3.2 First host bootstrap / chicken-and-egg

The **first** Proxmox host cannot contact a preexisting BMAC cluster leader. Its generated ISO must include enough pinned installer configuration to initialize a new BMAC cluster identity and `bmacd` trust anchor, plus the unique HTTPS serving certificate/key generated by BMAC Host Deploy, then permit a one-time bootstrap administrator enrollment. Its HTTPS certificate SHA-256 fingerprint must be displayed by Host Deploy at ISO creation time and saved/printed independently by the operator; do not derive trust from the remote page itself. Subsequent hosts present cryptographically scoped one-time join identity and register with that established cluster. Cloning an old ISO must not permit arbitrary repeat host joins. The bootstrap UI is reachable over the configured management interface after operator TLS trust establishment. A cluster initialized without quorum must not claim HA-safe mutating workflow readiness until Proxmox membership/quorum conditions are met.

### A3.3 Proxmox custom ISO workflow

BMAC Host Deploy retains the **currently working** Proxmox unattended installer customization and helper `.deb` integration. Adapt its post-install script to install/configure `bmacd`, register or initialize the cluster according to the host's signed role, set its management network/Tailscale policy, and configure HTTPS service and systemd startup. **For each target host**, Host Deploy generates a unique, strong-key, self-signed X.509 leaf certificate with the exact planned DNS names and/or IP SANs; installs its private key with root-only permissions in the resulting host (never in `pmxcfs`); embeds it into the temporary, sensitive ISO; and displays SHA-256 DER-certificate fingerprint with the corresponding host identity and instructions to verify it at the browser certificate-warning screen. Where an ISO is duplicated accidentally, enrollment safeguards must reject reusing an already provisioned host identity. Never reuse one host's certificate/private key for other hosts. Preserve the host's existing disk/RAID/LUKS provisioning behavior. The ISO must expose a preflight summary of exact target machine, NICs, disks, destructive actions, cluster identity, bootstrap credentials, and package hashes. On initial host setup failure, record a local installer log without leaking secrets.

### A3.4 Ubuntu backup appliance ISO workflow

Produce a customized Ubuntu Server amd64 ISO using Ubuntu's supported **autoinstall/cloud-init** facility and first-boot systemd provisioning. Build from a pinned verified official source ISO. Requirements:

1. Collect appliance identifier, expected MAC/physical host identity as available, disk identities (by-id / serial with byte capacity), intended LUKS device/pool layout, preferred redundancy topology, hostname/network, management Tailscale tag, cluster identity and pinned CA, transport endpoints, quotas/default policy, receiver package version, and an **expiring single-use appliance enrollment token**.
2. Provide mandatory destructive disk selection review; the installer MUST reject unexpected/missing disks, size/serial mismatch and accidental OS-device targets. Custom ISO settings must be consistent with the target hardware or installation stops safely.
3. Install Ubuntu Server, cryptsetup/LUKS, OpenZFS, Tailscale, `bmac-backupd`, hardening/firewall and systemd units. Set up LUKS during installation with interactive operator-entered secret via local console; ISO contents and logs do not contain it.
4. Join the intended Tailscale network with narrow tags/ACLs using a one-use/short-lived auth key or other supported approved bootstrap. Store no reusable Tailscale admin API credential on the ISO. Join success alone is **not** cluster trust.
5. Present enrollment proof to the pinned cluster over a protected outbound connection. Require an authorized coordinator to validate one-use token, appliance identity, certificate public key and intended storage binding; operator approval can be required. Issue independent backup-writer/receiver identity scopes. Until accepted, no production data can be written.
6. Register appliance characteristics and declared defaults without granting it authority over cluster execution or giving the cluster any appliance shell credential.
7. If pool is locked after reboot, expose only limited health/locked state. The appliance requires a **local or out-of-band console** for manual LUKS unlock; unattended unlock is explicitly not required.
8. On reinstallation, treat the machine as potentially new identity; prevent silent takeover of an old backup UUID or pool GUID without a signed operator-supervised replacement/recovery procedure.
9. Support cancellation, diagnostics and clean retry of incomplete one-use enrollment without reusing an already consumed identity.

**ISO disposal:** mark each generated image as `UNUSED`, `IN_USE`, `CONSUMED`, `REVOKED` or `DESTROYED`. The installer UI prompts the operator to delete all customized copies following installation, checks its own working files/caches, and revokes unused bootstrap grants. It cannot guarantee deletion of external copied USB/DVD images; document this limitation. Expiry/one-time use is the primary control.

### A3.5 Appliance `.deb` installation workflow

Install `bmac-backupd` on a normal operator-installed supported Ubuntu Server machine. Guided local root-admin `bmac-backupd init` performs storage/LUKS/ZFS provisioning only with explicit destructive confirmation, Tailscale setup, keypair creation and one-time enrollment identical to the ISO path. Support package upgrades without losing pools, identities, retention settings or data; support clean uninstall without deleting backups by default. The same remote Dashboard lists/manages both installation paths; it cannot impersonate the appliance administrator.

## A4. Coordinator architecture: leader, followers, workers and shared state

### A4.1 Exactly one admissible writer, many hosts

Each `bmacd` starts a read-only web/status plane and a local job worker. A quorum-backed coordinator election chooses **one** leader for shared mutations and scheduled jobs. Implement leader election and fencing using tested Proxmox cluster lock/lease primitives (e.g. `pmxcfs` lock protocol), with monotonically increasing **leadership epoch** and an explicit write-admission check for every mutating transition. Locks held for minutes are **not** assumed safe for hours-long operations. Never rely on only a per-host `flock`, hostname ordering, process PID, wall-clock lease or blindly reading cached quorum.

- A leader reserves a short cluster-wide state transaction, checks quorum, validates expected revision and writes a durable job admission record; then dispatches to a named host worker. A target worker checks authority, job ID, epoch, deadline, action allowlist and current Proxmox/ZFS state before each dangerous phase.
- Every action uses globally unique job ID and idempotency key. Repeated browser POSTs or coordinator restarts return/reconcile the original job rather than starting a second conflicting action.
- A worker can continue a safe already-admitted read/transfer as long as current authority and lineage semantics allow. New destructive phases require renewed authorization. On leader loss, workers fail closed or reach an explicit safe checkpoint.
- If a host with an active job fails, cluster job index reports `INTERRUPTED / RECONCILING`, not fabricated success. Replacement leader probes actual ZFS/Proxmox/appliance state before retrying.
- Existing Proxmox HA controls remain authoritative for production VM ownership and fencing. BMAC never runs two VM owners because a BMAC leadership state was stale.

### A4.2 Configuration, registry, database and persistence model

**Safety boundary:** Proxmox `pmxcfs` is a quorum-replicated *configuration* filesystem (the documented database capacity is **128 MiB**), not a general log store or a concurrent SQLite database directory. **No BMAC runtime logs, external-tool output, SQLite database files, journal segments, high-frequency telemetry, or unbounded audit stream may be stored in `/etc/pve`.** Reserve it for compact, versioned, high-value cluster state. Independently test the Proxmox supported path, file-size limit, quorum/locking behavior and available headroom before enabling writes. Fail closed on insufficient space and alert well before nearing the limit.

| Data category | Durable location | Authority and replication |
|---|---|---|
| Desired cluster configuration, BMAC registry, VM/host identities, placement, backup policies, appliance enrollment, short leader and admission metadata | Small, versioned records in a supported quorum-protected BMAC namespace after validated Proxmox integration | Single serialized cluster mutation authority; every host sees same revision |
| Full job output, typed Rust workflow progress, captured external-tool diagnostics, BMAC audit events, detailed job history | **Host-local SQLite/SQLx and bounded local append-only log segments** outside `pmxcfs` | Origin host writes first; **every other coordinator receives replicated copies** under A4.5 |
| Bounded shared job summary (job ID, owner, status, last update, log origin, latest known copy coverage, revisions) | Small cluster config/job index | Authoritative index only; never full log contents |
| Appliance snapshots, backup transfer commit journal, restore manifests and protected PIT histories | Backup appliance's independent ZFS-backed state | Appliance authoritative for successful receive/retention |
| Service credentials, Postmark API token, HTTPS private keys, machine credentials | Protected host-local secret store, with independent appliance secrets on appliance | Versioned references in shared config; distribute through mutually authenticated privileged workflow; never copy secret bytes through log replication |
| Live observed status | Reconciled from Proxmox, ZFS, systemd and appliance APIs | Observed facts supersede stale cached UI display |

**Database decision:** Use **standard SQLite + SQLx** for local structured persistence. Enable SQLx static checked queries (`query!` / `query_as!`) and embedded schema migrations; CI verifies the prepared/offline SQL metadata against the checked-in schema. Prefer this well-understood combination to Turso unless a separate proof shows practical advantage without weakening Rust/SQL compile-time checks, migration support, integrity or compatibility. SQLite databases stay on ordinary node-local disks, never on `pmxcfs` or a network filesystem. Enable WAL where safe; define busy-timeout, integrity checking, backup/restore procedures, schema migration and crash-replay behavior.

For the registry, evaluate a typed relational view with explicit Rust data models, constraints and migrations, **but do not assume independent per-host SQLite databases become consistent by themselves**. Initially keep critical cluster truth in the quorum-serialized registry and materialize a validated **local SQLite read model/cache** for convenient typed queries. A subsequent registry SQL migration is allowed only if it preserves a provably single writer, epoch fencing, atomic versioned commits, quorum handling and deterministic recovery; it is a separate reviewed milestone. No ad-hoc rsync, SQLite WAL sharing in `/etc/pve`, or unsound last-writer-wins replication.

Secrets are redacted **before** local log persistence or replication. The remaining space in `pmxcfs` is not BMAC's quota; impose separate conservative BMAC metadata budgets, size/health checks and write throttles. The current Proxmox capacity limit is not a promise that BMAC can consume that space.

The implementation MUST **prove** safe, quorum-serialized modification on the supported Proxmox version before enabling multi-admin writes. If existing registry/`pmxcfs` cannot meet this, use a separately specified consensus-backed metadata store rather than pretending per-node SQLite files are shared truth.

### A4.3 Locks, preconditions and conflicts

Keep the established BMAC admission semantics for VM creation/removal, staging snapshots/clones, disk growth, placement changes, host joining/removal, failover-sensitive routing updates and backup operations. At minimum use per-resource exclusive write intent plus a scoped global control-plane lock for operations affecting cluster membership. Admission requires state revision and real owner; handle stale forms with an explicit 'state changed, review again' response. Read-only status updates never need write locks. Jobs record cancellation boundaries, dependencies, human approval and rollback/compensation steps.

### A4.4 Failure behavior

- **No quorum:** read-only status with prominent stale/live distinctions; refuse new mutating jobs, backup PIT creation and unsafe scheduled changes. Distinguish existing in-flight safe phases with clear rules.
- **Leader unreachable but quorum remains:** elect a new fenced leader and reconcile admitted tasks. Web on surviving node continues serving.
- **One web host down:** administrator visits another host; shared inventory and job index remain present; surviving coordinators serve previously replicated logs from the failed host, showing the latest confirmed replication cursor and any missing tail; host failure must not make all earlier host-specific diagnostics disappear.
- **Split-brain / stale epoch:** any worker finding mismatched epoch/quorum must refuse further mutation. Integration tests deliberately partition hosts and verify no duplicate job execution.
- **Backup target offline:** continues to be configured; unaffected healthy destinations keep backing up, and checkpoint grace policy applies.
- **Workstation offline:** no impact on admitted workflow or scheduled operations.

### A4.5 Required all-peer log replication and loss-of-host diagnostics

**Goal:** Every healthy `bmacd` coordinator independently stores (a) its own origin logs and (b) a replicated copy of every other coordinator's eligible logs. Administrators browsing **any** surviving node must be able to review the same operational history, including history originating on a now-dead host. **Nothing in this log pipeline is written to `pmxcfs`**. Replicated logs are diagnostic data, **never** an authoritative substitute for quorum, the BMAC registry, fencing, or appliance PIT commit records.

#### A4.5.1 Event model and local durability

- Each worker/coordinator records structured Rust workflow events, bounded/redacted external-tool stdout/stderr chunks, run IDs, actor and authorization audit events, local failure diagnostics, and relevant backup/HA transitions **locally first**. Give every event a stable `(cluster_id, origin_host_uuid, origin_stream_id, sequence)` key, plus `event_id`, `job_id` if any, host-local timestamp and UTC timestamp, severity, category, payload/content-type and schema version. Sequence numbers are monotonic within an origin stream; reboot/new host incarnation changes stream ID but **does not** reset immutable host identity.
- Write small indexed records to host-local SQLite via SQLx, and store large raw text/output in **bounded append-only compressed segments** referenced by SQLite metadata if appropriate. Single large progress messages are chunked with size limits; outputs are never loaded entirely into RAM. Use SQLite WAL on ordinary local disk and guarantee crash-recovery of acknowledged local events.
- A job's log origin is its actual execution host, not whichever host the browser happened to access. Coordinator-wide events record leader/epoch but preserve the origin host. Deduplicate normal retries by immutable event key, and preserve the original timestamp and identity.
- Logs must be **redacted before durable storage** for password/passphrase, Postmark token, SSH private keys, session cookies, Tailscale enrollment keys, appliance restore credentials and authorization headers. Define redaction tests for Rust adapters and captured raw external-tool output. Replication may not elevate a remote node's permissions to access host secrets.
- Capture BMAC workflow logs and daemon audit events as required. **Also require a bounded diagnostic collector for key host failures**, including warning/error-level events (subject to explicit privacy and filtering rules) from Proxmox HA services (`pve-ha-crm` / `pve-ha-lrm`), Corosync, replication (`pvesr`), kernel hardware/storage errors, ZFS health, and `bmacd` itself, plus periodic compact host health/heartbeat samples. Make additional journal sources and verbosity opt-in and separately budgeted; **do not ingest all OS journal or VM guest logs unboundedly**. Snapshot relevant diagnostics proactively on host health degradation, so pre-failure evidence can replicate. Never promise recovery of bytes never persisted/replicated before a sudden power loss.

#### A4.5.2 Authenticated transport and replication protocol

- **Every coordinator replicates directly to all other active cluster coordinators** over a dedicated host-to-host authenticated **mTLS** protocol, reusing the validated cluster machine trust domain but not browser user cookies or appliance writer credentials. Tailscale/private network is a transport option, not an authorization mechanism. Peers must verify cluster ID, host UUID, certificate identity and membership before accepting data.
- Origin writes one canonical local event stream. For each peer it maintains a durable acknowledgement cursor and retry queue. Peers acknowledge a batch **only after durable local persistence**, not merely network delivery. Replicate batches continuously or with short bounded delay (target: **5 seconds during healthy connectivity**); compress batches and impose concurrency/bandwidth caps. Retries are idempotent. Reconnect performs range-based anti-entropy using source stream ID and sequence and checks event hashes. Gaps trigger backfill and Dashboard warnings.
- Copies retain the immutable original origin identity; a replicated event must **not be re-originated** as a new event or echoed in a replication loop. If the origin disappears, surviving peers can supply existing copies to a new/rebuilt host using explicitly marked **peer-assisted historical backfill**, authorized by membership and retention. Reconciled history records its actual origin and the copy sources.
- A joining host receives bounded eligible retained history from existing members without blocking cluster admission. An intentionally retired host's logs remain on surviving peers through normal retention, independent of host removal from the live registry. Host ID/stream ID reuse must not let a replacement forge the retired host's log history.
- This is **asynchronous diagnostic replication**. It must not block Proxmox HA or create a hard dependency for snapshot/backup correctness. When no peer accepts logs, record locally, surface `LOG_REPLICATION_DEGRADED`, preserve retry backlog within configured storage ceilings, and state that the latest unreplicated tail may be lost if the origin host dies. Critical mutation admissions must still be durably represented in quorum-protected **compact metadata**, independent of their expanded logs. The system must not claim RPO=0 for logs.
- Log replication must have independent queueing, CPU, disk-I/O and network limits and priority lower than HA/Corosync and production replication. Throttle automatically during congestion and ongoing bulk ZFS sends.

#### A4.5.3 Retention, capacity and integrity

- Each host keeps a **bounded local log budget covering both own events and all peer copies**, with configurable age-based and byte-based retention, minimum protected recent diagnostic window, critical-free-space threshold and per-origin fairness. Suggested initial operator-configurable defaults: **30 days or 8 GiB per host, whichever limit is reached first**, with capacity-aware reduction on small nodes; measure rather than assume these defaults suit all deployments.
- Cleanup is performed by the local log-retention service on each host, not by deleting Proxmox cluster config. Set explicit overflow/eviction priority: transient verbose output first, ordinary job details second, audited security/error events last, subject to a bounded hard ceiling. Audit eviction is visible and alertable. No promise of perpetual history; Dashboard shows per-host retention horizon and coverage gaps.
- Never allow exhausted log partitions/databases to fill the root filesystem, block SSH/Proxmox HA, or exhaust the ZFS pool required by production guests. Support quotas or dedicated filesystem where practical, proactive alerts and emergency bounded purge of eligible log segments.
- Use checksums/hashes for replicated chunks and verify per-stream integrity upon replay. The scheme establishes corruption detection and origin identity among trusted peers, **not immutable evidence against an attacker with root on all hosts**. Distinguish diagnostics from tamper-proof external audit archives.

#### A4.5.4 Dashboard experience, observability and failure cases

- Every Web Dashboard's Activity/Logs views query its **local replicated log index first**, showing all hosts by origin, job, severity, event time, retention and replication status, with paginated search and cursor-based live polling. No dependency on the origin being online to open replicated history. If a history range is absent locally but another healthy peer has it, allow authenticated fetch/backfill, then display it. Never silently fabricate missing events.
- Show per-peer **last durable acknowledged sequence, last sync time, lag (events/bytes/time), gaps, byte usage, remote availability, current retention horizon** and origin-dead status. Surface failure notifications through Dashboard and configurable Postmark policies, but do not generate unbounded recursive alerts from the logging system itself.
- Expose host-loss diagnostics: latest recorded heartbeat/status, last emitted error/warning, job execution phases, last durable replicated event and any missing terminal job result. Correlate with compact shared job index and later live reconciliation. Show `INCOMPLETE / UNKNOWN` rather than treating a missing final log line as job failure or success.
- Rejoining or newly installed coordinators repair missing eligible history from peers with verified identities, respecting disk budget. Logs remain viewable after graceful removal/purge of a node until retention naturally expires.

#### A4.5.5 Required acceptance/fault tests

1. Two hosts emit concurrent jobs; all healthy hosts have equivalent sorted event sets after convergence and no duplicates/re-originating loops.
2. Power off an origin after acknowledged peer replication. Surviving host can view its pre-failure activity and logs without contacting the dead machine.
3. Power off origin before peer acknowledgement. The UI clearly marks the unreplicated tail as potentially missing; no invented outcome.
4. Partition peer links and continue safe local logging; reconnect and repair all retained ranges with hash/sequence verification and idempotent batch replay.
5. Install a new host and verify bounded historical backfill; remove a host and verify surviving peers preserve its logs until configured expiry.
6. Generate high-volume stderr, >max-line records, burst external-command outputs and 10-host activity. Validate per-host hard budget, backpressure, throttling and prevention of root/pool exhaustion; Proxmox quorum and HA remain healthy.
7. Compromise one peer's machine credentials: reject cross-host impersonation, wrong cluster, invalid records, over-quota ingest and secret-bearing inputs; no arbitrary commands.
8. Shut down the Dashboard/browser and restart `bmacd`; durable job progress and replicated logs remain accessible after restart and via another web node.
9. Verify **zero** BMAC log bytes, SQLite/WAL files, detailed output or audit streams reside in `/etc/pve` or increase `pmxcfs` occupancy beyond compact configured metadata.

## A5. Web Dashboard technology and visual compatibility

### A5.1 Why a webapp can avoid a public API, but not HTTP routes

Axum serves entire HTML documents and Askama-rendered HTML fragments; HTMX sends GET/POST requests to these private server routes. For example:

```text
GET  /                        dashboard HTML
GET  /backups                 backups HTML
GET  /jobs/{job_id}           workflow page
GET  /jobs/{job_id}/status     progress HTML fragment, polling
GET  /jobs/{job_id}/log       sanitized incremental log fragment
POST /jobs                    authorized and CSRF-protected job request
POST /jobs/{job_id}/input     reply to an existing workflow prompt
POST /jobs/{job_id}/cancel    explicit allowed cancellation
```

These **are** HTTP handlers and form-processing endpoints. They are *not* an unauthenticated automation API and do not require adopting a public stable JSON API. Internal server-to-server RPC between host daemons is a separately versioned authenticated machine protocol, not the browser interface. Avoid SSE/WebSocket as baseline; if polling is unable to satisfy measured use cases, add a narrowly justified optional streaming mode later.

### A5.2 Polling and job progress

- Poll visible cluster summaries at adaptive intervals (proposed default 5 seconds; background tabs slower). Poll an **active** workflow at 1-2 seconds, or use conditional/long polling as measured. Stop or back off for completed, hidden or disconnected views. Use ETags/revision counters to avoid full rerenders.
- Implement **typed Rust job-progress events** (phase, progress, prompt, warning, diagnostic, result, failure, completion) and persist/replicate them to **every** peer under A4.5. Use the prior legacy-script NDJSON transcripts as migration test fixtures where helpful; production progress must not depend on a legacy subprocess emitting NDJSON. HTMX retrieves server-rendered fragments based on cursors/sequence numbers from the local origin-or-replica index. If an administrator opens a job in a new browser, it replays sanitized persisted progress and then continues polling from the cursor.
- Forms and batched inputs are defined by typed Rust operation schemas and rendered from durable prompt events. The workflow can await a response while the browser disconnects; show pending prompt to another properly authorized administrator, with takeover/lock rules and identity recorded.
- Escape all external-tool output and operator-supplied text as untrusted text; detailed logs support copy and incremental loading. Never interpret external-tool output as HTML or shell instructions. Secrets are redacted at emission and before persistence.
- Browser polls should not SSH to nodes or invoke ZFS commands. All status is server-side and permission checked.

### A5.3 Preserve existing Dashboard appearance

The present Tauri Dashboard's React/TypeScript/Tailwind/shadcn look and feel is the **visual reference**, not a UI spec to disregard. Before porting, capture screenshots/golden samples, navigation structure, component styles, density, typography, forms, progress panels, toasts, icons, confirmation dialogs and color contrast. Implement reusable Askama components, Tailwind tokens and minimal progressive enhancement to match them. A server-rendered rewrite may not reuse JSX source, but must reuse the UX behavior and visual tokens. Color is never the sole status indicator. Desktop installer keeps the same brand system with only two setup workflows.

**Design alternative permitted only by documented benchmark:** retain the existing React UI served as static assets by Rust with polling fetch requests, if exact UI preservation and migration risk demonstrably outweigh SSR benefits. This requires internal authenticated JSON endpoints and should be treated as an explicit architecture change, not silently implemented. Preferred/committed baseline remains Axum + Askama + HTMX.

### A5.4 Primary UI navigation and workflows

Navigation retains *all currently shipped* cluster management functions: overview, hosts and quorum, placement, production VMs, staging clones, storage, disk growth, ingress/routing, QDevice, diagnostics, operations/activity/history, settings, backup appliances, backup policies, PIT timeline/branches, restore wizard, scrubs and notifications. Only ISO building is absent and is offered in the separate Installer desktop app. All buttons map to the central allowlisted job registry; no arbitrary SSH/shell command field. Strong confirmation/review for destructive operations; read-only views clearly show quorate or stale state.

## A6. Authentication, authorization and transport

### A6.1 Browser HTTPS

- Every host serves HTTPS from its management/Tailscale address, not public `0.0.0.0` by default; allow custom bind/TLS setup only after explicit security configuration. Restrict inbound with host firewall and Tailscale ACLs; network reachability is **not** authentication.
- **Default: unique self-signed certificate per host, produced by BMAC Host Deploy at Proxmox ISO-build time**, with DNS/IP SANs matching the expected browser address(es), a private key embedded into the sensitive one-time ISO, and its **SHA-256 fingerprint printed, displayed and optionally saved by the desktop app**. On the deployed host, the private key is root-readable only and never in `pmxcfs`, a log, or another node's general browser secret store.
- When visiting `https://<host-management-address>:<configured-port>`, a standard browser can warn that the self-signed certificate is not rooted in a trusted public CA. The operator may proceed **only after comparing the certificate's full SHA-256 fingerprint to the separately retained Host Deploy output**. Provide exact browser-independent instructions and optional `openssl x509 -in host.crt -noout -fingerprint -sha256` check against a trusted copy. A self-signed certificate plus user confirmation encrypts the session, but if the fingerprint is not verified, a man-in-the-middle risk remains. Do not treat a click-through alone as secure verification.
- Support optional operator-supplied CA/publicly trusted certificates later, plus explicit renewal/reissue/replacement procedures. On rotation, show a new fingerprint through authenticated, preferably out-of-band confirmation **before** cutover. Report expiration in the Dashboard and Postmark alerts. Keep machine-to-machine mTLS credentials in a **distinct** issuer/trust domain from the browser's self-signed leaf certificates. An ISO containing a browser TLS private key is sensitive even if short lived; ISO disposal and one-use enrollment are mandatory.
- Session authentication: named admin users; password hashed with modern password KDF (Argon2id where appropriate) and optional enforced MFA (TOTP or WebAuthn in follow-on phase), login throttling, account lockout policy and audited access. Support at least administrator, operator and viewer roles; future custom scopes can be additive. First user via one-time locally attested bootstrap enrollment.
- Browser receives only a high-entropy opaque session cookie (`Secure; HttpOnly; SameSite=Strict; __Host- prefix where host-policy supports it; Path=/`), not a JWT exposed to scripts. Session data/privileges live on the server, expire both idle and absolutely, support per-user revocation and all-session revocation. If per-host sessions cannot be shared safely, logging into each host independently is acceptable; operational state remains common. Avoid `localStorage` and `sessionStorage` for tokens.
- Every state-changing POST requires origin check, CSRF token, permission evaluation, resource revision/idempotency key, and typed input validation. Do not permit browser-generated arbitrary command lines, dataset names, host paths or command flags.
- HTTP security: HSTS only after CA trust and domain correctness, CSP, X-Content-Type-Options, no password/token logs, bound request body sizes, headers/cache controls. Sensitive pages use `Cache-Control: no-store`.

**Why not default to a JavaScript-memory JWT?** A server-rendered HTML/HTMX application naturally benefits from revocable HttpOnly cookie sessions. Keeping a bearer JWT solely in JavaScript RAM would force explicit client code to attach it to all HTMX/fetch requests, complicate full navigation and still expose it to XSS. A JWT may be used entirely server-side or later for explicitly scoped integration clients, but it is not a requirement for secure browser sessions.

### A6.2 Host-to-host internal machine RPC

Separate short-lived mutually authenticated machine credentials from browser credentials. Bind identities to cluster ID and physical host ID; allow only permitted worker operations, status and coordinator admission messages. Validate epoch, job ID, typed Rust operation identifier and version, input schema, rate limits and resource scope. Protect from SSRF and shell/path injection. Do not derive cluster membership trust solely from a Tailscale IP or hostname. Maintain detailed audit logs of who initiated each job, on which web node, selected coordinator, execution host, and outcome.

### A6.3 Appliance backup transport

Routine cluster-to-appliance path is mutually authenticated TLS 1.3 carrying framed ZFS stream bytes plus a restricted metadata/control protocol. Prefer direct TCP when WireGuard UDP pps limits harm throughput; Tailscale network is supported for control/discovery and optionally data. Service-side writer authorization is **append-only to assigned namespaces**; receiver validation must ensure no `-F`/`-R` history pruning, cross-namespace writes, retention edits, arbitrary admin RPC or SSH access. Appliance-side privileged storage helper is fixed-operation and carefully constrained; local retention authority is separate. Use independent credentials for approved restore/export and peer repair. All normal backup credentials remain unusable for appliance OS login.

## A7. Replace the tested Bash/Python workflow engine with native Rust

### A7.1 Authoritative goal and scope

The implementation **MUST translate the current BMAC operational scripts and supporting Python helpers into equivalent idiomatic Rust functions and modules within `bmacd`**. This is a required part of this re-architecture, not an optional later refactor. Rust owns all BMAC domain state, orchestration decisions, preconditions, planning, error handling, persistence, scheduling, cancellation and external-command sequencing. The existing tested scripts cease to be production execution paths when their Rust equivalents graduate. No general human-callable Bash/Python BMAC operations engine is retained. Installer/autoinstall helpers, OS-provided shell utilities and external platform executables can still exist where technically necessary; they are not a parallel BMAC operational implementation.

**Non-negotiable principle:** the 500+ existing unit tests and their scripts are an unusually valuable behavioral and integration-contract specification. Their exact baseline must be inventoried at a pinned source commit before porting. Rust's strict type system and ownership rules can catch internal defects but **cannot prove the correctness of commands, API shapes, Proxmox timing, side effects or external textual outputs**. The migration must preserve those observable contracts or intentionally revise them with evidence.

### A7.2 Workflow migration inventory and traceability

Produce checked-in machine-readable inventories for all operator workflows and every supporting helper, including prerequisites and ISO-only functionality. Each inventory entry MUST record:

- Legacy file path(s), functions, entrypoints and workflow ID, plus the final Rust module/function/operation ID and owning component (`bmacd`, `bmac-backupd`, or Host Deploy).
- Input fields, defaults, constraints, user-prompt groups, secret handling, environment variables, config keys, and dependence on current working directory or path resolution.
- Preconditions, lock and quorum assumptions, Proxmox owner/placement constraints, destructive operations, idempotency strategy, rollback/cleanup and expected intermediate states.
- Every external program/API invoked, relevant flags, stdin schema, exit status meanings, stdout/stderr formats, configuration files and side effects; distinguish locally invoked programs from remote operations.
- Current test IDs/files, mocked command fixtures, false-positive assumptions, known bugs or intentional behavior changes; Rust replacement test IDs and parity results.
- Migration status: `INVENTORIED`, `CONTRACT_CAPTURED`, `PORT_IN_PROGRESS`, `UNIT_PARITY`, `INTEGRATION_PARITY`, `HARDWARE_VALIDATED`, `RUST_ACTIVE`, `LEGACY_RETIRED`.

The inventory is the source for progress and release gates. A workflow with unmapped existing tests, undocumented external interactions, or unresolved parity deviations MUST NOT be marked migrated.

### A7.3 External-interface contract catalog (critical)

The existing scripts frequently call tools/services maintained outside BMAC. Their unit-test mocks often encode exact argument lists, output shapes, failure responses, ordering constraints, and required retries. **Treat the scripts together with their mocks, fixtures, assertions and captured traces as the primary evidence for the external-component contracts.** Build a versioned contract catalog of all observed integrations, including (as applicable after inspection) `qm`, `pvesh`, `pvesr`, `ha-manager`, `pvecm`, `pct`, `zfs`, `zpool`, `zdb`, `systemctl`, `ssh`, `ip`, `lsblk`, `blkid`, `cryptsetup`, QDevice/Corosync tools, HAProxy, networking and the Proxmox unattended installer helper. This list is illustrative, NOT a claim that the repository currently uses every item.

For each operation, the catalog MUST specify:

| Contract dimension | Required evidence and Rust representation |
|---|---|
| Command or protocol | Executable/API name, exact supported version(s), subcommand, argument order/flags, expected environment and privilege context |
| Inputs | Typed Rust request models, allowable values, units, paths, safe escaping, stdin content if any, and preconditions |
| Success result | Exit code/status, parsed JSON or text format, expected fields, optional fields, encoding, units and side effects |
| Failure result | Nonzero exit codes, timeouts, signals, stderr warnings, empty/malformed output, partial side effects and retryability |
| State transition | Exact operations already applied before/after each external call, snapshot/VM/network consequences and rollback conditions |
| Version behavior | Differences across target Proxmox/OpenZFS/Ubuntu releases, fallback behavior and a capability detection mechanism |
| Tests | Legacy test/fixture reference, Rust mock/contract test, negative cases and real integration test identifier |

Never assume a successful exit means the intended system state exists; follow with state verification where the scripts already do so or where correctness requires it. Do not treat a warning on stderr as necessarily fatal if a tool documents a successful exit. Treat SSH disconnects and transport errors differently from remote command errors. Preserve semantics for `grep` statuses, ZFS snapshot/replication naming and GUIDs, `pvesr`/HA transitions, and Proxmox CLI exit behaviors where present in the tested baseline. Any reliance on undocumented output MUST be characterized and version-gated; prefer machine-readable modes or structured Proxmox APIs where a proved equivalent is available.

### A7.4 Typed Rust integration layers

Use small, testable Rust service layers rather than mechanically translating shell lines:

```text
Web form / scheduler / operator CLI
  -> RBAC + quorum + fencing + admission + durable job
  -> typed operation (Rust async orchestration)
  -> domain interfaces (Proxmox, ZFS, networking, SSH, disk/partition, appliance)
  -> controlled external process / supported HTTP API / filesystem or system call
  -> typed outcome + validated observed state + durable structured progress
```

Implement a narrow `CommandRunner` abstraction with a real executor and deterministic fake/test executor. All external invocations MUST use literal executable identities and separated argument vectors (`std::process::Command` / `tokio::process::Command`) rather than shell interpolation. Define explicit stdout/stderr byte capture limits, streaming for long operations, timeouts, cancellation, process-group handling, expected exit code sets, least-required execution privileges, environment allowlists, working directory and redaction. Avoid elevated generic command execution; an operation may only reach its declared commands/endpoints and validated resource namespace.

Create domain-specific Rust adapters with typed request/response structs and errors (for example `ProxmoxGuestOps`, `ReplicationOps`, `ZfsOps`, `HostNetworkOps`, `ApplianceTransferOps`), using traits or equivalent substitution seams for tests. Parse output once in adapters. No scattered regex/substring parsing duplicated across workflows. Prefer official machine-readable responses where proved compatible; record the reasoning when a direct CLI path is retained. Use typed identifiers and units for VMID, disk index, bytes/GiB, pool/dataset, snapshot GUID/name, host UUID and job ID. Validate before side effects and after external results.

A successful Rust compilation is not completion: verify external-interface contracts against the actual installed tool versions and live BMAC cluster behavior. Do not rewrite proven ZFS, HA or ingress algorithms just to make the code look more Rust-like; first reproduce their safety semantics exactly, then make separate reviewed improvements with their own tests.

### A7.5 Preserve the full 500+ test corpus as a migration contract

At migration start, pin a Git commit and run the existing test suite in a reproducible environment. Record exact case names, count, passing/failing/skipped status, fixture paths, external-command stubs and integration dependencies. The maintainer reports **more than 500 unit tests**; determine and record the true current number, rather than silently rounding it or claiming it has been verified in this document.

The required migration procedure is:

1. **Catalog every legacy test case** and classify it as pure decision logic, parsing/serialization, external integration contract, order-of-operations/side effects, failure/cleanup, security, UI protocol, or environment/integration test. A test may cover more than one category.
2. **Extract reusable golden fixtures and traces:** tool argv, stdin, stdout, stderr, exit status, expected subsequent calls, filesystem effects, HTTP requests/responses, retries and cancellation checkpoints. Capture both typical successes and negative/boundary outcomes; normalize only nondeterministic fields such as timestamps, random IDs, hostnames and ephemeral paths using documented rules.
3. **Map each legacy test to one or more Rust tests** with equal or stronger assertions. A shared parametrized Rust test is acceptable if the mapping still enumerates every old case and proves every important input, failure and side effect. Do not use test-count alone as a quality metric.
4. **Use differential tests where safe:** run the original script and new Rust code in isolated mocked/dry-run environments, compare planned external interactions, resulting normalized state and public events. **Never dual-run mutating implementations against live production** to establish parity.
5. **Retain legacy tests as an executable reference in CI** for the migration period and archive their complete source/fixtures even after production stops shipping legacy scripts. The final Rust suite must run without a Bash/Python operational workflow runtime.
6. **Handle divergences explicitly:** classify any mismatch as a migration bug, legacy bug, intentional supported behavioral improvement, or environment/version difference. A change requires an issue/decision record, regression test and review; silently changing expected results or deleting tests is prohibited.
7. **Gate every workflow promotion** on 100% mapped legacy test cases, relevant Rust unit tests passing, external command contract tests passing, mocked end-to-end parity, destructive failure scenarios, and a documented hardware-level test where applicable.
8. **Gate full retirement** on a repo-wide zero-unmapped-tests report; no previously passing high-value test may disappear merely because its original language or UI protocol is no longer present. Tests of obsolete *transport-only* details may be superseded only by named equivalent/higher-level Rust/event/HTML tests.

CI MUST publish: legacy baseline count, mapped/ported/equivalent/superseded counts, unmapped IDs, fixture coverage, Rust pass/fail results, test contract differences, supported external tool versions and hardware verification status. CI checks must fail on unmapped/accidentally removed cases. Human reviewer signs off intentional behavior changes.

### A7.6 Durable workflow and interactive input semantics

Define a versioned Rust `Operation` registry containing ID, request/result schema, RBAC role, resource locks, retry semantics, cancellation safety, severity/destructive confirmation, leader/worker placement and progress-event schema. Rust emits typed events (`started`, `phase`, `progress`, `input_requested`, `warning`, `detail`, `result`, `failed`, `completed`) directly to the persisted job journal; the old `bmac-ui v1` NDJSON transcripts are *test fixtures*, not a production subprocess protocol.

A job can pause awaiting a batch of form inputs. Pending prompts and authorizations are durable and visible to other authorized browsers. Capture which administrator answered, compare state revisions, and ensure repeated HTTP form submissions cannot repeat a side effect. Persist each externally visible irreversible phase with idempotency/observed-state reconciliation to handle coordinator crash or leader changes. For external long-running operations, enforce correct child process cancellation/cleanup and defend against killed coordinators leaving unknown remote state.

### A7.7 Deployment strategy: safe workflow-by-workflow native replacement

- During development, the original script implementation is the pinned test/reference baseline. A temporary compatibility runner MAY exist solely for nonproduction migration/test comparison or carefully controlled rollback; it is **not** the final production architecture or user-facing API.
- Port lower-risk diagnostic/read-only workflows and generic adapters first, then backups/replication primitives, staging/placement/host operations and destructive operations in an explicit dependency order based on the actual inventory. Important existing behaviors (including staging clones, HA failover, ingress continuity, QDevice changes and disk growth) require corresponding real-cluster regression tests before cutover.
- Do not activate a partially converted workflow in production unless its complete end-to-end operation is implemented in Rust with verified adapter behavior and a recovery/rollback plan. A feature flag can route **whole workflows** to a tested legacy implementation only during a short, explicitly labeled compatibility window; do not interleave Rust and script subphases without transaction/recovery analysis.
- Preserve the previous released desktop/script system for operator recovery until the new server/web version has passed parity gates; after cutover, ship only the ISO-building desktop app plus Rust coordinator and appliance services.
- No hidden second Bash implementation, unbounded arbitrary-command endpoint, or independent CLI safety bypass is permissible in the final deliverable. External utilities run through the typed Rust adapters, which are themselves covered by regression tests.

### A7.8 Workflow rewrite exit criteria

A workflow is **Rust active and legacy retired** only when its implementation and external adapters have been reviewed; all mapped tests pass; real tool/API compatibility is verified; job/event/UI parity is demonstrated; faults and side effects are reconciled; and HA, quorum, permission, security and cancellation boundaries remain intact. Final release requires every original production BMAC operational workflow to meet this gate. ISO customization logic may remain in Host Deploy and supported installer bootstrap helpers where necessary; that exception is scoped to provisioning, not live infrastructure orchestration.

## A8. Backup policy, scheduling, recovery and appliance operation

The entire preexisting backup design is carried forward in Part B. The main change is **execution location**: `bmacd` creates snapshots and orchestrates sends on cluster members, and the Web Dashboard provides interaction from any node. Fundamental requirements remain:

- Full initial send of an immutable coordinated VM PIT (including precise VM config) to one received zvol per VM disk per branch; subsequent valid `zfs send -i` incrementals against per-target common GUID checkpoint, **no `-F`** normal receive.
- One source PIT snapshot can be shared by all targets; local source snapshots propagate normally with Proxmox. Retain local PITs by separate short-term policy for fast clone/revert; keep bookmarks for offsite catch-up only after verified compatibility across ownership changes.
- Per-target checkpoints and offline grace states; healthy destinations continue independently. Source bookmark -> alternate peer appliance -> full reseed recovery planner; do not delete target configuration automatically.
- Offsite appliance exclusively controls snapshot pruning and scrubs. LUKS manual unlock is intentional. Branch management and protected ancestor logic survive production rollbacks; separate recovery branches prevent rewriting previous PIT histories.
- ZFS scrubs detect checksum errors across allocated snapshots but **do not** test guest boot/application consistency. Provide isolated test restore and full-cluster-loss VM recreation using backed-up VM configuration, all disks and environment manifest.
- Every appliance transfer/restore/retention event is a durable job with visible state, emitted by coordinator and corroborated by appliance. All backup configuration and alert state must be available to all web nodes.

## A9. Postmark notification engine

### A9.1 Configuration and secret placement

The optional cluster configuration accepts (names illustrative; adapt to the actual `cluster.conf` conventions):

```ini
NOTIFICATIONS_ENABLED=false
POSTMARK_SERVER_TOKEN=           # optional secret input only, never in web HTML/logs
POSTMARK_FROM_EMAIL=
POSTMARK_DEFAULT_TO_EMAILS=
```

`cluster.conf` is a provisioning input. When written to cluster nodes, split the Postmark token into a **root-only protected secret store** on eligible hosts and store a versioned `secret_ref` in quorum-replicated nonsecret configuration. Never return or log the token from the Dashboard, APIs, subprocess stdout or export files. Token is a **Postmark Server API Token**, not a Postmark Account Token. Send over HTTPS to `https://api.postmarkapp.com/email` with `X-Postmark-Server-Token`. Disabled or absent token means no email attempts; all Dashboard alerts still operate.

### A9.2 Event catalog and user-configurable policy

Events include: log peer replication degraded/recovered, diagnostic-log storage low/retention forced eviction; cluster quorum lost/restored; host node unavailable/recovered; Proxmox HA failover initiated/completed/failed; production ingress unhealthy/restored; replication lag; staged VM lifecycle failures; storage capacity critical; ZFS pool degraded/scrub errors; hardware failures; security/enrollment anomalies; backup target offline/locked/recovered; missed PIT / RPO breached; checkpoint grace approaching expiry/expired; transfer fail/resumed/committed; retention failures; branch creation/retirement; scheduled restore test fail; TLS credential expiring; coordinator election/change; job failures and cancellation.

Admin configures per-event-type **enabled, severity, recipients, throttle/dedup window, escalation and recovery messages**, plus optional maintenance silences. Defaults should alert on materially serious failures and avoid noisy routine success. Add Dashboard Notification Settings and Alerts History pages including redacted test-send operation. Notifications are *best effort* and never block HA jobs, Proxmox operations or backup PIT publication. An independent local durable notification outbox plus leader/event idempotency key guarantees controlled retries across leader changes; do not claim exactly-once email delivery across network ambiguity. Dedup by cluster/event/resource/generation/state, log provider message ID/status and retry with exponential backoff and sane caps. If Postmark is unreachable, show failure locally and alert on prolonged notification transport outage through Dashboard.

### A9.3 Postmark API handling

Request JSON contains From, To, Subject, TextBody (optional HTML), MessageStream `outbound`; use TLS certificate verification, response body/error classification, rate limit handling, idempotency in local outbox, plain concise actionable copy with timestamps and target IDs but no secrets, PIT contents or passphrases. Expose sender verification prerequisites and a test mode in setup instructions. Postmark credentials on host are not equivalent to appliance retention credentials, and must never be placed into backup ISO images.

## A10. Operator workflows and web UX

### A10.1 BMAC Host Deploy (only two workflow categories)

- **Build Proxmox host ISO:** inspect source hash, choose existing/new cluster, host settings, show destructive disk and private/management network plan, **generate and embed host-unique self-signed Dashboard HTTPS certificate/key**, display/print SHA-256 fingerprint and identity, embed verified boot scripts/deb, produce ISO, show certificate-verification/install instructions, secure disposal checklist, monitor local *ISO build only*.
- **Build backup appliance Ubuntu Server ISO:** collect appliance settings, Tailnet key/enrollment, LUKS/pool choices, build autoinstall ISO, local verification, single-use status, secure disposal checklist. All routine appliance management **after enrollment** belongs to Web Dashboard.

No former web Dashboard cluster management pages remain in desktop release after migration. `.deb` direct appliance provisioning is a supported package/CLI workflow, not a third desktop management feature.

### A10.2 Web Dashboard

- **Overview:** quorum, coordinator leader/epoch, data freshness, cluster services, protection summary and live alerts.
- **Hosts/HA:** node discovery, placement, HA health, replication, ZFS capacity, disk lifecycle, onboarding/removal, QDevice and TLS/Tailscale status.
- **Production/Staging:** VM creation, maintenance, disk expansion, lightweight staging linked clones, routing and existing workflows.
- **Backups:** appliances, retention policies per VM and destination, backup jobs, PIT history and branch graph, target outage/grace/repair, capacity, scrub/test restore, restore/promotion and complete cluster-loss recovery.
- **Activity:** globally identified jobs, actor/time/phase/progress; current prompt ownership; past logs and outcomes; cancellation and retry; merged host-origin log views served from local peer-replicated log storage, including failed hosts, provenance, lag and retention gaps.
- **Administration:** admins/roles/sessions, self-signed certificate fingerprints/trust and rotation, cluster settings, per-event Postmark notification settings and alert history, audit logs and peer log-replication health/capacity.

All pages must remain useful under polling, and display explanation when another administrator is manipulating the same resource. Use job revision tokens for optimistic conflicts, enforce safety server-side, and expose exact dry-run impact before destructive actions.

## A11. System upgrades, state exports, disaster recovery of BMAC itself

- Cluster package rollouts are staged, version-checked and reversible; coordinator leader avoids mixed-version destructive operations; web nodes with older binaries show compatibility warning. Migrations are idempotent and snapshot config before upgrade.
- Export **BMAC's own cluster control metadata** and nonsecret config in the appliance VM PIT recovery manifests, along with external sealed secret recovery instructions. A full-cluster-loss event must not create a paradox where an existing coordinator is needed to restore the coordinator.
- Provide **standalone restore bootstrap mode** on a fresh Proxmox node or the Installer's limited Proxmox ISO workflow, using a human-approved separate appliance recovery identity. It can read an appliance repository inventory and restore a chosen VM/config without needing old BMAC cluster state. After rebuild, enroll fresh host IDs/CA as required and re-establish HA replication.
- Admin account recovery and lost CA remediation require physically/local-console verified break-glass operation; no remotely reusable universal backdoor, magic recovery password or appliance-admin identity stored on production cluster.
- Promote old data only after verifying single active production, correct network/SSH keys, disk mapping, stale replication disabled and new protection checkpoint established.

## A12. Delivery/migration plan

**Phase 0 - Audit / proof gates:** pin a known-good BMAC commit; inventory every legacy script, helper, all **500+ maintainer-reported unit tests** and external-tool mock; freeze executable baseline traces and fixture corpus; create the test-to-Rust operation mapping and external contract catalog. Capture current Dashboard visual golden screenshots, staging/HA/ingress hardware baseline and Proxmox/OpenZFS versions. Prove pmxcfs/quorum/epoch-fencing and host HTTPS trust assumptions.

**Phase 1 - Typed Rust foundation and read-only workflow migration:** package `bmacd` on every node; implement Rust typed operation registry, structured progress and input schemas, `CommandRunner` and Proxmox/ZFS/network adapters with fixtures extracted from legacy tests, durable jobs, SQLite/SQLx and **all-peer bounded log replication**, shared compact state, leader election/locking and follower dispatch. Port diagnostics and read-only operations first; pass mapped Rust tests, mock parity, and multi-admin/daemon-restart tests. Legacy scripts remain reference-only during development.

**Phase 2 - Rust operational workflow parity and Web Dashboard:** Port every remaining active Bash/Python workflow to Rust domain operations and checked external adapters, in risk-ranked slices; enforce per-workflow legacy-test mapping and live hardware regression before activation. Build Axum/Askama/HTMX/Tailwind web UI with polling, host-specific self-signed HTTPS and fingerprint verification, and authenticated HttpOnly server-side sessions. Preserve current UI appearance and behavior. Maintain previous released desktop/script system solely as recovery fallback until the entire replacement passes cutover gates; then remove it from the shipping app.

**Phase 3 - Host Deploy split and enrollment:** rename desktop app to **BMAC Host Deploy** and limit it to two ISO types; extend Proxmox ISO to install/initialize/join `bmacd` with unique injected host HTTPS cert and displayed fingerprint; add customized Ubuntu backup ISO; package standalone appliance `.deb`; ephemeral Tailscale + appliance enrollment; secure trust setup and ISO disposal workflows.

**Phase 4 - Append-only appliance and core backups:** appliance receiver hardening, LUKS/ZFS setup, full/incremental PITs, manifests, commit ledger, resume, schedules, independent retention and basic Dashboard views. Verify no cluster writer can destroy old PITs.

**Phase 5 - Disaster recovery and resilience:** local clone/rollback, historical full restore and full-cluster loss bootstrap, branch graph, multiple appliances, source bookmark catch-up, peer-assisted repair, grace windows, quotas/scrubs and isolated restore tests.

**Phase 6 - Notifications and operational release:** Postmark rules, event outbox, alert history, capacity thresholds, transport/performance testing (including UDP PPS throttling), upgrade/rollback drills, loss-of-quorum fault injection and all feature release gates.

Phases are delivery checkpoints, not permission to ignore interdependencies. Every partial release must clearly label not-yet-implemented functionality and avoid unqualified 'fully backed up' or 'fully recovered' claims. Finish and validate the **native Rust** server/dashboard migration, with a zero-unmapped-legacy-test report, before removing the old desktop operational engine. Email alerts may be developed in parallel but must not delay safe-core functionality.

## A13. New acceptance criteria (platform)

### Leadership and shared state

- Two browser sessions on **different** Proxmox hosts see the same VM/config revision, target inventory and job identity; job begun by Admin A appears to Admin B at another host while A disconnects, including current progress and pending prompts.
- Concurrent conflicting mutation requests from different hosts lead to **one admitted job** and a conflict message. No dual VM owner, competing staging/backup snapshot deletion, or duplicate resource creation.
- Kill leader daemon; with cluster quorum, a new one is elected, admitted jobs recover/reconcile and web UI stays reachable on surviving node. Partition quorum and verify **no new mutations**.
- Job state survives `bmacd` restart; each coordinator stores its own logs and authenticated copies of every other coordinator's eligible logs, including after an origin host is lost. Missing unreplicated tail is labeled, not silently reconstructed, and appliance PIT remains authoritative. Status GETs cannot accidentally run scripts with side effects.

### Web and security

- HTTPS host-unique self-signed certificate generated by Host Deploy at ISO creation with fingerprint displayed/saved off-host; browser exception accepted only after verifying full SHA-256 fingerprint against independent creation record; no default public listener. Valid admin login uses HttpOnly cookies, CSRF, replay/expiry protection and per-role permissions. No browser-stored Postmark token, appliance root login, BMAC host command credential or LUKS passphrase.
- Existing Dashboard visual snapshots have reviewed parity; every current cluster workflow is implemented natively in Rust, works from a browser without the workstation, and emits durable typed progress displayed by polling rather than SSE/WebSockets.
- Browser injection attempts in external-tool output and Rust operation progress are escaped; out-of-role POST fails, stale revision POST fails, arbitrary shell/dataset flags fail, credential material does not appear in logs.

### Provisioning

- Proxmox ISO installs compatible `bmacd` with a host-specific self-signed TLS certificate and protected key; Host Deploy displays the expected SHA-256 fingerprint; new node can join and be admitted with one-use credentials; an ISO replay after consumption cannot enroll another arbitrary host.
- Ubuntu backup ISO installs/verifies packages, provisions expected encrypted disks, uses local LUKS input, joins Tailscale and registers with cluster only after explicit identity validation. Wrong disk details halt safely. Machine requires local unlock after reboot; no passphrase is embedded.
- Standalone appliance `.deb` init leads to the same secure enrolled outcome as custom ISO. Both appear identically in Web Dashboard after enrollment. ISO temporary files can be deleted and bootstrap grants revoked/expired.

### Job orchestration, backup and alerts

- The maintainer-reported **500+ legacy unit tests** are inventoried against a pinned baseline; **every individual test has a traceable Rust equivalent, stronger replacement, or reviewed supported supersession**. All Rust regression/adapter/integration tests pass. Existing staging linked-clone, HA failover, replication, ingress, disk-growth and QDevice invariants remain intact on hardware.
- Protect multiple appliances, deliberately fail one, independently alert/grace/recover it by source bookmark or healthy peer, and preserve each appliance's retention history.
- Compromise simulation with **all cluster-held credentials** fails to delete, change, release, rewrite or export protected appliance PITs. Sending excessive data hits quotas and admission limits rather than destroying history.
- Postmark sends only configured events, optional token absence is harmless, leader changes do not cause notification floods, and invalid token causes Dashboard-visible failure without blocking normal operations.
- Complete-cluster-loss recovery on new hardware reconstructs VM config and disk recovery set using only appliance data and documented independent admin trust/recovery prerequisites.

## A14. Explicit decisions and verification gates

### Recommended choices treated as defaults (can be changed intentionally, not omitted)

| Topic | Recommended design | Reason |
|---|---|---|
| Primary web UI | Axum + Askama + HTMX + Tailwind; HTML fragment polling | Fits Rust, avoids browser SPA state and SSE, remains approachable to agents |
| Workflow engine | **Native Rust operations in `bmacd` using typed Proxmox/ZFS/etc. adapters; legacy scripts reference/test-only** | Stronger internal type safety and one operational implementation while preserving externally verified behavior |
| Browser authentication | Revocable opaque HttpOnly session cookie | Safer and simpler for server-rendered HTML than JS-memory JWT |
| Cluster-wide state | Existing registry + small quorum-replicated structured configuration | Avoids external DB circular dependency and pmxcfs log abuse |
| SQL implementation | Standard SQLite + SQLx on each host | Compile-time checked queries and embedded migrations; no unproven Turso compatibility |
| Cluster registry | Quorum-protected compact authoritative desired state, with typed local SQLite cache/read model | Centralizes correctness without assuming independent SQLite files have consensus |
| Detailed logs | Host-local SQLite/segment files, asynchronously replicated to **every** other host | Same cross-host activity/diagnostics after node loss, no `pmxcfs` log data |
| Coordinator authority | Quorum, epoch-fenced single leader | Prevents multi-admin / multi-host races |
| Browser HTTPS | Host-specific self-signed TLS certs created during ISO build, operator-verifiable SHA-256 fingerprint | Matches Proxmox-style local trust model; distinct machine mTLS credentials |
| Access | Per-host HTTPS on management/Tailscale | High availability without exposing to public WAN |
| Appliance protocol | Separate mTLS restricted TCP receiver | Transport-independent and blast-radius limited |
| Appliance install | Ubuntu ISO and standalone `.deb` | Both automated and manually installed appliances |
| Backups | OpenZFS send/receive, independent appliance retention | No proprietary delta engine |
| Postmark | Optional cluster-level Server Token + per-event policies | Predictable emails and no dependency on delivery |
| LUKS | Manual local console unlock on backup appliance | Explicit preference for physical key control |

### Gates that coding agents MUST verify, not guess

1. **Repository greenfield webapp document:** the source is [`docs/WEBAPP_STACK_OPTIONS.md`](https://github.com/OtterFlip/bmac/blob/main/docs/WEBAPP_STACK_OPTIONS.md), checked during this revision. It explicitly favors Rust + Tokio + Axum + Hyper + rustls + SQLx + Askama + HTMX with PostgreSQL/SQLite, server-side state, compile-time queries and polling. The selected BMAC architecture follows it. A material departure requires an explicit architecture decision.
2. **Proxmox quorum-write/lock semantics:** confirm supported `pmxcfs` namespace, size constraints, locking, epoch and transaction durability in the currently deployed PVE version. Verify failover and corruption recovery in a dedicated test cluster before calling this design production safe.
3. **Rust workflow migration:** inventory and pin all 500+ legacy tests, capture external program input/output/exit/side-effect contracts, implement typed Rust adapters and operations, map 100% of test cases with reviewed deviations, and validate on actual Proxmox/ZFS hardware. Do not assume Rust typing covers external command compatibility.
4. **Ubuntu unattended installer:** validate autoinstall seed injection/installation on chosen Ubuntu release, package signing, net bootstrap availability, exact LUKS prompts, offline operation and early reboot. Avoid hardcoded undocumented assumptions about Subiquity.
5. **Appliance append-only enforcement:** privilege separation and streamed receive validation are security-critical and require destructive negative testing. Tailscale ACL alone does not prove isolation.
6. **Multi-admin authentication:** per-host sessions with shared cluster operational state are acceptable by default; verify secure revocation and role propagation. A future federated session layer requires its own security review. Do not silently make tokens available to every host's Javascript.
7. **Current Dashboard fidelity:** screenshot/diff the actual running application; do not accept a cosmetically unrelated SSR rewrite based on general Tailwind defaults.
8. **Custom ISO secrets:** check expiration, single-use and revocation semantics of the actual Tailscale and enrollment facilities being used. If an auth key cannot be constrained adequately, bootstrap by operator scan/manual approval rather than shipping a durable secret on removable media.
9. **Whole-cluster-loss recovery:** test fresh-host recovery when no original `bmacd`, cluster CA, VMID, qdevice, network mapping or control DB survives.

### Decisions for the user before irreversible implementation

**No additional decision is required to author or begin non-destructive prototypes.** Before broad production migration, the operator should approve these preferences (recommended defaults above permit safe engineering work meanwhile):

- **Web stack vs pixel-level React reuse:** **decided baseline**: Rust SSR/HTMX with strict visual parity. Any request to retain React as the primary runtime is a separately reviewed deviation, not a default alternative.
- **Browser login:** **decided**: opaque, revocable HttpOnly server sessions, not browser-managed JWTs. This is not an open user decision.
- **Cluster metadata durability:** **decided safety requirement**: small quorum-backed authoritative configuration and job admissions, with separate local SQLite+SQLx for query models and peer-replicated logs. No logs or SQLite/WAL files in `pmxcfs`; no ad-hoc database file rsync.
- **Appliance bootstrap approval:** recommend automatic registration only after one-time strong cryptographic attestation *and* policy match, optionally require manual one-click confirmation for a completely new appliance.
- **UI exposure:** default Tailscale-only web access, no public HTTPS listener; opt-in public access with stronger operator auth and ingress hardening.

---

# PART B - OFFSITE BACKUP AND DISASTER RECOVERY TECHNICAL REQUIREMENTS

The following numbered sections preserve the substantive, detailed backup implementation contract while relocating all running orchestration and user experience to `bmacd` and the cluster-hosted Web Dashboard. The numeric section IDs are preserved for traceability to v1 design discussions.


## 5. Appliance provisioning and storage

### 5.1 Supported platform

- Supported starting point: contemporary amd64 Debian/Ubuntu Linux with compatible OpenZFS, systemd, and LUKS2/cryptsetup. Declare and test an exact minimum OpenZFS feature set, host OS version, and send/receive feature negotiation; do not assume all peers run identical versions.
- The appliance is not a Proxmox cluster member and participates in neither Corosync nor VM placement.
- Provision one or more physical devices with a chosen redundant ZFS topology (mirror/RAIDZ as appropriate). Vdevs are pool storage components; **zvols** are individual backup disks.
- Support operator-supplied disks and a confirmed destructive provisioning plan; never silently select unused disks.
- Encrypt appliance data-bearing disks with LUKS beneath ZFS. Require interactive passphrase entry at the local/out-of-band boot console after reboot. Explicitly show status `locked/unavailable` until unlocked; do not store the unlock passphrase on the appliance or production cluster for unattended boot.
- Persist backup manifests **on the protected ZFS pool**, not exclusively in the OS boot filesystem or on the workstation. Back up daemon configuration/exportable recovery state to the pool as well.
- Service startup waits for LUKS unlock and pool import. No automatic pool creation/import with alternate credentials if the expected pool is missing.
- Use a regular ZFS filesystem dataset for metadata, manifests, journal, audit events, and temporary transaction state. Use ZFS zvols for VM disk data.
- Require stable appliance UUID and pool GUID identification; never match a device only by mount path or pool name.

### 5.2 Example layout

```text
tank/
  bmac/                                  # filesystem parent
    metadata/                            # filesystem: manifest database and durable events
    clusters/<cluster_uuid>/              # filesystem namespace, administrative records
    disks/<cluster_uuid>/<vm_uuid>/
      <branch_uuid>/<volume_uuid>         # received ZFS zvol
```

Backup manifests refer to branch and volume UUIDs, not just dataset paths. One PIT can span several volume zvols plus configuration. Reserve namespace identifiers; sanitize generated ZFS names and enforce dataset path confinement in the appliance.

Use ZFS snapshots for PIT history, not a separate vdev or full zvol for every PIT. Use additional zvols for **separate branches** where required. Enable appropriate ZFS compression and sparse target allocation; use conservative pool-space reservations/quotas and spare headroom. Do not enable ZFS deduplication by default. Pool headroom is a hard admission check.

### 5.3 Read-only and storage protections

- Receiving zvols MUST never be mounted or attached for guest writes by ordinary appliance services. Recovery testing uses a separate cloned or newly received scratch dataset.
- Restrict and test `readonly=on` behavior; if this conflicts with native receive on a supported OpenZFS version, the receiver helper must enforce equivalent device access controls and prohibit local writers.
- Receiving helper cannot invoke `zfs destroy`, `rollback`, `release`, `promote`, `rename`, pool-level admin, arbitrary `zfs set`, or forced receive on behalf of a cluster writer.
- Do not send `-R` replication packages or use receiver `-F` in append-only data ingest. Even where `receive:append` ZFS delegation exists, Linux delegation requirements and actual kernel/ZFS behavior must be tested; it is an additional guardrail, not the only security boundary.
- Local appliance retention manager should set ZFS holds on protected PITs; it alone may release them as part of an approved retention transaction. Snapshot holds are not root-proof immutability.

## 6. IDs, PIT naming, recovery manifests

### 6.1 Identity scheme

- `cluster_uuid`: stable BMAC cluster identity, survives host replacement and cluster rebuilding when restored from backup metadata.
- `vm_uuid`: stable registered production identity, independent of host and Proxmox VMID.
- `volume_uuid`: stable VM-disk identity, independent of storage path and `scsi0`/`virtio0` numbering.
- `branch_uuid`: immutable lineage identity. One current active production branch per VM.
- `pit_uuid`: globally unique, immutable recovery-set ID. Include UTC capture timestamp as searchable metadata, not as sole identity.
- `target_uuid`: stable appliance-specific destination identity, not its Tailscale hostname/IP.
- `transfer_uuid`: stable idempotency key for each VM PIT / target / volume transfer attempt.

Example snapshot name: `bmac-bkp-20261009T193000Z-4f7a2c` (short random suffix; full UUID stored in manifest). The same PIT snapshot name is applied to every volume in that VM's recovery set. No collisions with staging or Proxmox internal snapshot names.

### 6.2 Required manifest fields

Each PIT manifest MUST include at least:

```json
{
  "schema_version": 1,
  "cluster_uuid": "<uuid>",
  "vm_uuid": "<uuid>",
  "vmid_at_capture": 100,
  "pit_uuid": "<uuid>",
  "branch_uuid": "<uuid>",
  "parent_pit_uuid": "<uuid-or-null>",
  "capture_time_utc": "<RFC3339>",
  "consistency": "crash|filesystem|application",
  "consistency_evidence": {},
  "pve_version": "<version>",
  "zfs_feature_profile": "<name-or-object>",
  "vm_config_raw": "<exact-qm-conf-text>",
  "vm_config_normalized": {},
  "vm_config_sha256": "<hex>",
  "volume_set": [
    {
      "volume_uuid": "<uuid>",
      "bus_slot": "scsi0",
      "source_dataset": "rpool/data/vm-100-disk-0",
      "source_snapshot_name": "<name>",
      "snapshot_guid": "<ZFS-guid>",
      "source_pool_guid": "<ZFS-guid>",
      "volsize_bytes": 0,
      "volblocksize_bytes": 0,
      "required_for_boot": true,
      "receive_complete": true
    }
  ],
  "boot_dependencies": {},
  "bmac_resource_snapshot": {},
  "external_prerequisites": [],
  "manifest_sha256": "<hex-of-canonical-payload>"
}
```

This is a **field inventory / illustrative shape**, not a literal manifest with `0` sizes. Define a versioned JSON Schema and canonical serialization. The manifest hash excludes its own hash field. Capture optional fields for EFI vars, TPM state, cloud-init sources, passthrough devices, network mapping, SSH/bootstrap assumptions, HA placement, disk interface types, machine type, Secure Boot/OVMF, encryption dependencies, and guest agent settings. Snapshot and store necessary boot-side auxiliary volumes as zvols if applicable.

Capture the raw Proxmox `/etc/pve/qemu-server/<VMID>.conf` configuration (colon-separated text, not a JSON file) **and** normalized structured data from Proxmox/qm. Distinguish current settings from pending settings; do not silently restore an unapplied pending change. Configuration and disk inventory must represent the same PIT, under the BMAC configuration mutation lock or with a before/after digest verification.

Persist the associated BMAC application resource, network addressing, MAC, HA placement intent, relevant Proxmox storage IDs, and operator-relevant recovery prerequisites. Back up a separate, versioned **cluster recovery manifest** containing enough registry, resource, networking, HA, proxy/route and storage intent to rebuild BMAC itself. Clearly identify any intentionally excluded secrets and external services. Protect archived configuration as sensitive data.

### 6.3 Two-phase publish

A PIT is not usable until appliance publication is complete:

1. Create an expected-PIT transaction record with the exact disk set and GUIDs.
2. Receive and validate each required disk snapshot and its GUID/volume geometry.
3. Persist the VM configuration and manifest into the appliance metadata dataset.
4. Verify all components, feature compatibility, and cross-disk PIT identity.
5. Atomically mark the PIT **published** using durable metadata (fsync/atomic rename/transaction log), then acknowledge success.
6. The coordinator advances that target's checkpoint **only after** receiving an authenticated published-PIT acknowledgement and independently reconciling with appliance state.

If any step fails, PIT is `incomplete` and cannot be selected for restore or pruned as a successful recovery point. Preserve resumable receives; expose cleanup and retry. On power loss between received disk and manifest publish, the appliance reconciles and either completes valid publication or quarantines the partial state.

## 7. Snapshot consistency and backup capture

### 7.1 Baseline behavior

- ZFS snapshots preserve a consistent, immutable *block-level PIT*. Snapshotting a running guest without coordination provides crash consistency, not universal database/application consistency.
- BMAC supports `crash` (allowed with explicit warning), `filesystem` (guest-agent freeze/thaw), and `application` (optional administrator-provided pre/post quiesce hooks or recognized integration). Default new policy: `filesystem` if guest agent is verified; otherwise require explicit operator acknowledgement of `crash` or refuse when policy requires a higher level.
- The snapshotting service MUST thaw a frozen guest on errors, timeouts, cancellation, crash recovery, or coordinator restart; include a watchdog. Do not leave the guest frozen while transmitting over the WAN.
- Consistency hooks are optional for webapp agnosticism. Application hooks must run only from configured trusted paths, with timeouts, exit-code checks, audit trail, and no arbitrary commands submitted by backup-appliance writers.

### 7.2 Multiple disks

- Enumerate every boot/required/data zvol from the current VM config, not just disk 0. Explicitly decide inclusion of EFI/TPM/state volumes and other writable disks; PIT is invalid if a required disk is missing.
- For multiple zvols in one pool, use a **single** `zfs snapshot volumeA@pit volumeB@pit ...` command (OpenZFS documents atomic multi-snapshot creation). Do not snapshot the whole rpool recursively and inadvertently capture unrelated VMs.
- If volumes span pools, the same atomic ZFS operation may not give the desired group guarantee; either coordinate with guest freeze and record that consistency bound explicitly or reject unsupported layouts by policy. Do not claim atomic multi-pool capture.
- Keep the freeze interval limited to coordination and snapshot creation; copy/receive occurs afterward.
- Block or retry concurrent disk resize, config mutation, VM removal, clone replacement, and migration while creating the coordinated PIT. Verify volume list and config digest after snapshot.
- During Proxmox failover/owner relocation, re-resolve the authoritative owner before every mutating operation. Never snapshot an offline stale replica as though it were the active production disk.

### 7.3 Source-side snapshot lifecycle

- Only create BMAC PIT snapshots on the authoritative VM owner, then use the existing Proxmox HA replication workflow to propagate as required; user has already validated propagation of external snapshots for linked staging clone workflows.
- `source_complete` means every expected source volume has a PIT and config manifest, not that any offsite copy exists.
- Each snapshot may be temporarily held while actively streaming; hold naming includes operation ID. Do not leave orphan holds after crash recovery.
- Existing local recovery snapshots, staging-clone origins, HA replication constraints, in-flight transfers, recovery branching dependencies, and target checkpoints all contribute to source pruning constraints.
- Do not delete an old PIT merely because one target finished. Each target independently owns its checkpoint and source bookmark protections.

## 8. Full initial backup

For each VM/branch/target, the first transfer is a full snapshot of each required zvol and its matching configuration manifest. The destination zvol **must not already exist** for a conventional full receive.

Illustrative data path (not the recommended privileged appliance command API):

```bash
# Production owner (example only):
zfs snapshot rpool/data/vm-100-disk-0@bmac-bkp-001
zfs send rpool/data/vm-100-disk-0@bmac-bkp-001 \
  | ssh backup1 'zfs receive -u -s tank/bmac/disks/example/branch/disk0'
```

Actual implementation MUST use the restricted, authenticated appliance receiver, not unrestricted remote root. `zfs receive` automatically creates the target zvol and the snapshot. Determine whether `-u` has meaning for a volume on the supported versions; it does not replace device-access controls. The receive may be resumable with `-s`.

For a multi-disk VM, capture all disks in one PIT and publish only when **all** have arrived. Record the first completed PIT as this target's last checkpoint. Protect a corresponding source snapshot and/or bookmark before pruning.

Full seed acceptance checks:

- Source volume properties, logical sizes, and snapshot GUIDs match received versions.
- Target has no unexpected writable live state or preexisting lineage collision.
- Manifest and VM config are durably published.
- Transfer exit statuses, receiver outcome, and authenticated appliance publication agree.
- If transfer is interrupted, never mark PIT successful or remove the source checkpoint.

## 9. Incremental backups and independent histories

The next backup snapshots the whole VM PIT and, for each volume, sends a forward incremental from the *target-specific last committed base* to the new PIT:

```bash
zfs send -i \
  rpool/data/vm-100-disk-0@bmac-bkp-001 \
  rpool/data/vm-100-disk-0@bmac-bkp-002
```

Or, when source snapshot `001` was replaced by a bookmark:

```bash
zfs send -i \
  rpool/data/vm-100-disk-0#bmac-bkp-001 \
  rpool/data/vm-100-disk-0@bmac-bkp-002
```

The receiving dataset must be at the proper committed base. In the ordinary append path its latest snapshot must match the send's starting PIT **by GUID**; it cannot have uncommitted writable changes and cannot require a destructive rollback. The receiver creates the endpoint snapshot. Destination PITs older than the common base may differ entirely from the source's PIT inventory. No `-I` is necessary to reproduce intervening snapshots if only the newest endpoint is being backed up.

**Required coordinator algorithm per target:**

1. Query the appliance's own authoritative published head and base GUID for each volume.
2. Discover source availability of the exact base snapshot or bookmark for every volume; compare GUIDs and lineage; don't trust matching names.
3. Choose one consistent *new* VM PIT newer than that target's head on the active branch; if needed create one coordinated snapshot PIT once for multiple destinations.
4. Preflight receive geometry, stream feature compatibility, pool free/reserved space, availability, and transfer concurrency.
5. Send each volume incrementally, supporting resume; transfer metadata and config; publish the aggregate PIT.
6. Confirm publication; atomically advance only this target's checkpoint; emit audit event.
7. Prune only source PITs no longer required for local rollback, staging clone dependencies, inflight backup, any target checkpoint/grace, or other BMAC constraints.

**If a target is several PITs behind:** use `zfs send -i old-base latest-pit` rather than replaying every intervening PIT. Incremental send transfers the net block changes needed for the endpoint and may still be large; `zfs send -nPv` can estimate it. A single latest PIT on each target is a *transport checkpoint*, not a requirement to keep only one historical offsite snapshot.

The implementation MUST avoid `-F` on an independently retained offsite history. If the destination's live head and stream base conflict, stop and reconcile; never fix disagreement by force-rolling it back.

## 10. Snapshot and bookmark policy on the cluster

### 10.1 Separate retention categories

1. **Fast local recovery PITs:** actual snapshots; retained by daily/weekly local policy and any staging or operational dependency.
2. **Destination checkpoints:** snapshot or bookmark for the last published PIT on each configured target. A bookmark is valid for forward send but **does not** preserve data for local clone/rollback.
3. **In-flight PITs and dependencies:** actual snapshots/holds while a stream, resume, staged recovery, or active clone needs them.
4. **HA/internal snapshots:** managed by Proxmox; BMAC does not prune them.

### 10.2 Bookmark correctness under HA failover

**Critical integration requirement:** A source bookmark alone on the current owner is insufficient if the VM can fail over to a replica that does not also have that bookmark. Proxmox's replication of custom snapshots does **not** by itself prove that it replicates bookmarks.

Before deleting a source PIT that is still a target's checkpoint:

- Create corresponding bookmarks from the **actual matching snapshots** on every eligible current HA owner/replica on which later sending might occur; verify bookmark GUID/creation TXG compatibility and audit results.
- Verify all required owner-eligible nodes have the needed bookmark after an owner move and after membership changes.
- If bookmark propagation cannot be safely performed or proven, retain the actual source snapshot on all required cluster replicas instead. This is a mandatory safe fallback, not an optimization failure.
- During addition of a new placement host, reconcile backup snapshots/bookmarks *before* allowing that host to become a source of a subsequent offsite incremental.
- If a host is permanently lost, treat it as unavailable; do not block forever waiting for it. Quorum/fencing rules still apply.

Bookmarks must be created before deleting their corresponding snapshots. Prove real compatibility with the installed Proxmox/OpenZFS versions in an HA move and forced failover test.

### 10.3 Retention mechanics

- Source PITs can be destroyed only after backup publication and dependencies have been reconciled. Source pruning on the owner and HA replicas must be coordinated with BMAC's existing replication/clone behavior.
- Snapshot holds are safeguards, not the sole source of truth; inspect ZFS clone origins, holds, bookmarks and Proxmox replication state live.
- If backup failed, never advance that target's checkpoint or delete the last usable base/bookmark due to that attempted transfer.
- Expiration of a target's checkpoint grace period means local references *may* be released, subject to other dependencies. It does **not** remove the appliance registration or its independently retained PITs.

## 11. Multi-appliance operations and unavailable-target policy

### 11.1 Default policy (user-configurable)

| Setting | Initial default | Meaning |
|---|---:|---|
| Backup schedule | Daily at 03:00 UTC | One VM PIT job per scheduled run; user may choose timezone/frequency |
| Target retries | Exponential, starting at 1 min, capped at 6 h | No tight retry loop |
| Destination-unavailable warning | After 1 missed scheduled PIT | Notify and mark degraded |
| Checkpoint preservation grace | 14 days | Protect repair bases while unavailable |
| After grace | Expire checkpoint protection only | Keep target registration and old offsite data |
| Automatic destination deletion | Never | Only explicit administrative lifecycle action |
| Maximum concurrent streams per appliance | 1 by default | Configurable with headroom |
| New PIT success policy | Per destination | Success at A never implies success at B |
| Overall protection status | Per-destination + aggregate | Warning if below configured minimum viable destinations |

Definitions: a 'missed scheduled PIT' means a scheduled backup due but not published within its configured deadline; 'unavailable' includes transport timeout, locked pool, unavailable service, or an authenticated receiver refusing due to capacity. Diagnose the reason separately.

### 11.2 Target state machine

```text
UNENROLLED -> ENROLLING -> SEEDING -> HEALTHY
                                  |      |
                                  v      v
                         INCOMPLETE   DEGRADED/OFFLINE
                                        |
                               grace checkpoint protected
                                        |
                                        v
                              CHECKPOINT_EXPIRED
                                        |
                            RECONCILING -> REPAIRING
                                        |           |
                                        +-> HEALTHY <-+
                                        +-> RESEED_REQUIRED

Any managed target -> PAUSED (operator; configuration preserved)
Any managed target -> RETIRED (explicit admin; no implicit data erase)
```

`PAUSED` stops new jobs but preserves the checkpoint according to its explicit pause policy. `RETIRED` disables future jobs and removes writer credentials; appliance data remains until its independently authorized retention decision. Loss of a target MUST NOT suspend snapshots or successful backups to the other targets.

### 11.3 Checkpoint ledger

Keep one durable record per `(target_uuid, vm_uuid, branch_uuid, volume_uuid)` with last published PIT GUID, source checkpoint snapshot/bookmark, acknowledgement, resume token if any, health timestamps, grace deadline, and repair status. Maintain a VM-level published PIT pointer only when every required volume and config component is complete. The appliance's own published records remain authoritative on reconciliation.

A checkpoint can only be promoted with a valid appliance-signed (or mutually authenticated and journal-correlated) publication result. An unattested cluster-side cache value is insufficient.

## 12. Peer-to-peer repair and rejoining

When a previously unavailable target returns, BMAC MUST attempt the following in order, with explicit namespace authorization and lineage checks:

1. **Direct source catch-up:** find its last published base GUID on a currently eligible source owner as an actual snapshot or bookmark, then send one incremental to a current descendant PIT.
2. **Peer-assisted catch-up:** query configured healthy peers for the same base GUID (snapshot or bookmark) and a compatible later published descendant PIT; authorize the healthy appliance as an export-only sender and the lagging appliance as the restricted receiving party.
3. **Full reseed:** if neither path exists, seed a **new** destination zvol/lineage from a current full PIT. Never silently erase the target's older snapshots in place to make room.

For peer repair, `zfs send -i peer@base peer@new` (or a valid peer bookmark as source) can work because received snapshots preserve GUID and ancestry. Intermediate PITs need not have been retained. A bookmark alone cannot serve as the **endpoint** of a send; a newer actual snapshot is required. If the original peer PIT was on a diverged branch, do not attempt to cross branches.

Peer-to-peer communication over Tailscale is allowed, but the transfer may alternatively use direct mTLS/TCP. Backup appliance A is **never** granted ability to delete PITs on backup appliance B. The receiving appliance checks source identity, allowed namespace, base GUID, stream feature compatibility, quota, target branch, and publication. If it cannot independently validate ancestry, fail closed and offer a fresh seed.

If peer catch-up requires several VM disks, the chosen peer endpoints MUST form one previously published VM recovery set (same PIT UUID). Do not synthesize a mixed-disk PIT from individually convenient snapshots.

### 12.1 Policy-managed peer repair cursors

A healthy appliance MAY retain low-cost source bookmarks for known lagging peers even after ordinary retention prunes historical snapshots. Peer repair markers are internal metadata, have a bounded age/quota, and are removable under independent appliance policy. The retention planner must first verify there is no remaining active repair or branch dependency.

### 12.2 Cross-peer authorization

- A requesting target cannot instruct its peer to delete, overwrite, or modify PITs.
- Export credentials grant read access only to committed, explicitly authorized snapshots and metadata.
- Every peer transfer is audited by both appliances and linked to a requesting BMAC coordinator operation; optionally require appliance-initiated pull for additional isolation.
- Ensure an offline appliance's stolen identity cannot drain entire backup inventories or alter retention on others.

## 13. Retention policies and pruning

### 13.1 Example policy defaults

| Tier | Local cluster | Offsite appliance |
|---|---|---|
| Daily | Last 7 days | Last 7 days |
| Weekly | Last 4 ISO weeks | Last 4 ISO weeks |
| Monthly | None by default | Last 12 calendar months |
| Most recent published PIT | Always protected if present | Always protected |
| Branch common ancestors | Preserve if dependent | Preserve if dependent |
| Staging origin snapshots | Preserve while clone depends | Not applicable |
| In-flight or resumable transfer | Protect | Protect |
| Destination-grace checkpoints | Snapshot/bookmark protected | Own latest published snapshot protected |

All values are defaults for the **proposed** feature, not assertions about existing BMAC settings. Retention may be disabled per tier. Do not implement 'N snapshots means N complete disk copies'. Snapshot space reclamation is block-reference dependent.

### 13.2 Deterministic bucket selection

- Use UTC timestamps as canonical PIT capture times; weekly means ISO week (Monday 00:00 UTC); monthly means UTC calendar month.
- For a bucket, keep the **latest successfully published PIT within the bucket**. Keep the newest K *eligible buckets* inside the requested retention window. Overlap between tiers creates one retained PIT, not a duplicate.
- A PIT exists separately on each appliance. An uncompleted PIT is never selected as a retention winner or recovery point.
- The retention keep-set is the union of policy bucket winners, latest branch head, pinned ancestors, linked-clone origins, legal/administrative holds, active repair/resume cursors, and internal transaction dependencies.
- Perform a **dry-run prune plan** listing PIT IDs, dependent clones, estimated freed space, protection reason, and surviving restore coverage. Compare actual live ZFS state again under the appliance retention lock immediately before executing.
- Never run user-supplied delete commands from the production cluster. Prune service releases its own holds and deletes eligible snapshots in a transactionally logged operation.
- Deleting intermediate snapshots never requires replaying deltas or regenerating surviving snapshots. Snapshot deletion MAY reclaim little or no storage if blocks remain referenced by other PITs.
- Retention for old **inactive branches** is explicit and cannot silently delete the only recovery path to an old branch.

### 13.3 Policies and control authority

Appliance retention policy is authoritative and stored locally with an audit/version trail. The Web Dashboard can display policy and, with a distinct appliance-administrator session, **request** an authorized policy update. A production writer cannot submit or impersonate such updates. Security hardening SHOULD offer a minimum retention floor and delayed/locally confirmed weakening of retention, including branch retirement.

All destructive operations have dry-run, exact-item lists, typed confirmations for explicit human pruning, and verifiable postconditions. Policy-driven routine expiry does not need daily human approval but runs solely under the local retention identity.

## 14. Local fast recovery and in-place rollback

### 14.1 Preferred: local clone to new VM

When a required PIT snapshot remains on the cluster, create a new writable ZFS clone per disk from **that exact PIT**, preserving original production and newer snapshots:

```bash
zfs clone rpool/data/vm-100-disk-0@bmac-bkp-001 \
  rpool/data/vm-200-disk-0
```

The example only illustrates the primitive. BMAC must allocate a collision-free VMID/volume identity through the registry and Proxmox storage backend, ensure all disks are from one PIT, isolate network identity, and never attach production's original disk to a test VM.

Run a recovery validation VM isolated from public routes and production VLAN identity until promotion. BMAC already uses ZFS linked clones for staging; reuse relevant safety and identity patching components where appropriate. A successful local clone is typically metadata-scale, not a full disk copy, but startup and guest checks still take time.

### 14.2 Optional: destructive in-place rollback

`zfs rollback -r dataset@oldPIT` is permitted only after stopping and fencing the VM, confirming all relevant disks and dependencies, and receiving a separate destructive confirmation. It discards newer local snapshots and writable state, and clone dependencies may block it. **Do not use `-R` automatically to destroy dependent clones.**

For multi-disk VMs, implement an explicit recovery transaction with staged validation or a fallback to clone-and-promote; there is no safe atomic cross-disk rollback simply because individual rollback commands succeeded. Prefer clone-and-promote in normal operations.

After any production state rewind, BMAC MUST trigger the **new backup branch** protocol before scheduling further sends. The previous appliance published head is not simply a writable destination for divergent incremental states.

## 15. Offsite restore and total-cluster recovery

### 15.1 Restore any retained PIT to a new zvol

Any intact published snapshot can be sent as a **full** stream back to a compatible Proxmox host:

```bash
# Illustrative reverse transfer from the authorized appliance export interface
zfs send tank/bmac/.../disk@chosenPIT | \
  ssh recovered-host 'zfs receive -u rpool/data/new-restored-disk'
```

This is a *conceptual* command. Implementation uses appliance-approved **read-only PIT export** and the appropriate authenticated transport. The source snapshot need not be the latest appliance snapshot, and it need not have a local match. The complete data referenced by that historical snapshot is transmitted, not necessarily all logical zero/hole bytes.

Do not overwrite the existing production zvol during import. Reserve new collision-free dataset/VM identities, verify GUID/geometry and manifest, then attach restored disks to an isolated VM.

### 15.2 VM recreation procedure

1. Select source appliance, cluster/VM/branch/PIT, and verify published manifest and all required disks.
2. Preflight available Proxmox cluster or replacement hardware: CPU/architecture, RAM, pool space, feature negotiation, OVMF/TPM/EFI state, bridges/network, attached devices, VLAN, storage IDs, and guest boot prerequisites.
3. Allocate or select an explicitly approved **new** VMID; reserve target dataset names and restore job ID.
4. Restore required zvols one by one into new names; retry/resume as supported. Verify received snapshot GUIDs, sizes, checksums and PIT manifest completeness.
5. Recreate VM from backed-up raw config **by validated field translation**, not an unchecked blind copy into pmxcfs. Use `qm create`/`qm set` or Proxmox API. Rebind disks to valid local storage IDs and bus slots; recreate or attach EFI/TPM devices from backed-up state. Distinguish settings that must remain exact from site-dependent replacements.
6. Restore BMAC resource identity and intended HA/placement metadata only when the operator promotes the VM; until then it is an isolated recovery VM with new MAC/IP or disconnected network.
7. Boot; check guest-agent availability, filesystem recovery, service readiness, and configured application checks. If test restore, never route public production hostname to it.
8. In a promotion transaction, fence/retire former production identity, establish one authoritative owner, update BMAC registry and routes, and reinitialize HA placement/replication using the recovered zvol lineage. Validate replica convergence before claiming full HA protection.
9. Preserve former production zvol and PIT history until separately approved cleanup.

### 15.3 Rebuilding after complete cluster destruction

- Provision replacement Proxmox/BMAC hosts (with LUKS unlock and hardware configuration as needed), rebuild quorum, networking, and service prerequisites.
- Reconnect to an existing appliance with an independent operator identity; use stored *cluster recovery manifest* to reconstruct BMAC resource inventory, VM configuration, placement intent, and related network/proxy settings.
- Retrieve each selected VM PIT and all required volumes, recreate VMs, verify boot in isolation, and then establish new HA replication. Fail explicitly if a required external secret/service/host resource cannot be reconstructed automatically.
- A recovery procedure must work without **any** surviving original Proxmox host, source ZFS bookmark, or original Web Dashboard installation. Appliance and operator possession of recovery metadata is sufficient for ZFS data retrieval.
- Restore of application services still depends on any non-backed-up external systems (e.g., DNS, public TLS, provider VLAN); document those dependencies clearly.

### 15.4 Recovery authority

`backup read` is a distinct permission from `backup append`. The sender credential used by a production node MUST NOT be sufficient to fetch historical VM data. Require appliance-approved export identity and operation-specific authorization. Audit every restoration export.

## 16. Production rollback and branch creation

### 16.1 Correct conceptual model

Suppose the original history is `P1 -> P2 -> P3 -> P4 -> P5`. Production is restored to `P3` and later makes `P6 -> P7`.

```text
Original backup branch:  P1 ---- P2 ---- P3 ---- P4 ---- P5
                                       \
New production branch:                 P6 ---- P7
```

`P3` is the **common ancestor**, not the new branch head. `P6` is the first new branch head, and `P7` becomes the head after its publication. Offsite `P4` and `P5` stay independently recoverable under original branch retention.

### 16.2 Required branch transition

1. Before any production rewind, ensure chosen PIT and needed volume GUIDs are present locally or recoverable offsite; preserve source and offsite old-head metadata.
2. Freeze new backup scheduling for this VM (not for other VMs), fence stale replicas/owners, and create a distinct `branch_uuid` with parent `P3`. Mark previous branch archived/inactive, **not deleted**.
3. Restore/recreate production from the chosen PIT using the safe local clone/promote or full offsite path; do not reuse the old offsite zvol's writable head `P5` for a new forward `P3 -> P6` receive.
4. Appliance creates a **new physical receiving zvol** for each disk in the new branch. Preferred efficient method is a local ZFS clone of original-branch `@P3` if supported and safe; BMAC must prove and test whether an incremental stream can be applied to that clone and what snapshot identity the receive demands on the installed ZFS version.
5. **Mandatory fallback:** if clone-origin incremental reception cannot be proven for the exact version/lineage, initialize the new backup branch with a **full** send of `P6` into a new zvol. This uses more network bandwidth but never jeopardizes old history.
6. Publish `P6` with parent `P3`, label active branch. Subsequent `P6 -> P7` ordinary incrementals go to the new branch.
7. Keep both branches visible, preserve common-ancestor clone origins and any holds, and only retire an old branch by an authorized appliance-side lifecycle workflow with dependency checks.

The implementation MUST NOT call a divergent incremental a successful append because names happen to match. Snapshot GUIDs, ancestry, and receiver head state determine compatibility.

### 16.3 Branch retirement

- Web Dashboard displays active/inactive branch, origin PIT, published range, storage use, dependent children, and retention settings.
- Retirement proposal includes child dependencies, snapshot holds, whether `zfs promote` or explicit rebase is required, and predicted space release.
- Only an appliance admin can approve retirement. Automatic policy may prune ordinary PITs on inactive branches but MUST NOT remove the sole branch root or an origin still needed by a surviving clone.
- Do not destroy a parent snapshot while a dependent branch clone exists; if promotion is needed, run it solely under appliance-local privileged service and verify before cleanup.

## 17. Restoring while Proxmox HA replicas exist

Restoration and promotion are cluster management operations, **not** just ZFS imports. An old replica can contain newer/different state than the PIT selected for recovery.

- Always restore into **new** disk datasets and a new isolated VM initially. This avoids using destructive `-F` against live HA replica zvols.
- Pause or detach affected Proxmox replication jobs / HA resource only through a coordinated BMAC mutation transaction, with the current owner fenced and quorum verified.
- After recovery verification, promote one VM and one branch atomically at the BMAC orchestration level; update disk bindings and placement metadata; retire, quarantine, or explicitly reseed stale replicas.
- Re-create Proxmox replication for the recovered disks, then verify every intended placement member before re-enabling usual HA behavior.
- Do not let failed-over original disks overwrite the recovered branch or vice versa. Guard against a late owner coming back online during promotion.
- If a multi-disk VM is involved, every disk and its metadata MUST be on the same PIT/branch before promotion.
- If the cluster loses quorum, the operation must fail closed until quorum/fencing is restored; do not circumvent Proxmox HA safety to complete a restore.

## 18. Receive security, validation, and admission control

### 18.1 Protocol design

Use a versioned, authenticated control protocol separate from the existing local Web Dashboard NDJSON protocol. The native ZFS stream is a byte stream transported alongside a bounded envelope, not embedded as JSON.

Suggested operations:

| RPC | Production writer | Peer export | Appliance admin |
|---|---|---|---|
| `GetCapabilities` | Yes | Yes | Yes |
| `GetAssignedDestinations` | Own namespace only | No | Yes |
| `GetPublishedHead` | Own namespace only | Authorized | Yes |
| `BeginFullSeed` | Assigned branch, validated | No | Yes |
| `BeginIncremental` | Assigned branch, validated | No | Yes |
| `ResumeTransfer` | Own transfer only | As approved | Yes |
| `GetTransferStatus` | Own transfer only | As approved | Yes |
| `ListPublishedPITs` | Limited metadata | Approved subset | Yes |
| `AuthorizeExport` / `ReadStream` | No | Read-only approved PIT | Yes |
| `CreateBranch` | Request only; policy-authorized | No | Yes |
| `ChangeRetention` | No | No | Yes |
| `DestroyPIT` / `RetireBranch` | No | No | Appliance retention service only |
| `RunScrub` / `SetQuota` | No | No | Yes |
| `ReadHealth` | Redacted health | Limited | Yes |

Administrative API may request that the local retention service perform work but cannot forward arbitrary OS commands. A policy update does not immediately grant the writer the ability to delete existing snapshots.

### 18.2 Stream acceptance invariants

- Valid TLS/SSH identity maps to **one allowlisted namespace** and a bounded set of VM volume/branch IDs.
- Envelope declares unique operation ID, source PIT, target PIT, expected base GUID, endpoint GUID, expected disk geometry, supported stream features, PIT manifest digest, and idempotency key.
- Verify actual stream/header and resulting ZFS snapshot identity against the envelope, not just caller claims.
- Full receive only into an approved not-yet-existing dataset; incremental only into approved receive head and matching base GUID, with no arbitrary `-F`.
- No `-R` replication packages that can delete or mirror the producer's retention topology.
- Protect existing published PITs with holds and access isolation. A malicious sender can create a **new bad recovery point** but cannot rewrite or destroy an older accepted PIT.
- Reject unknown destination names, feature flags, snapshots crossing branches, unexpected volume count, oversized manifest, malformed framing, concurrent conflicting transfers, quotas exceeded, and nonempty uncommitted write heads.
- Apply pool free-space/reservation policy before receiving and monitor continuously; terminate/retain resumable state on resource exhaustion.
- Require explicit stream compatibility tests and a hardened helper running with only the ZFS privileges necessary for receive. `receive:append` if supported is useful defense-in-depth, but actual Linux delegation and privilege constraints MUST be validated.
- Do not allow untrusted ZFS properties in streams to set mountpoints, shares, device exposure, permissions, or arbitrary destinations. Prefer a tightly defined receive feature/property profile; test property handling with OpenZFS versions.

### 18.3 Root/supply chain threat boundary

An attacker controlling production may stop sending, falsify new guest disk data, create spurious PIT requests within quota, attempt denial of service, or expose information the writer normally reads. It MUST NOT be able to delete historical PITs, reduce local appliance retention, change admin credentials, initiate arbitrary exports, execute shell commands, or compromise other tenants. A compromised appliance root or unlocked physical drives are outside sender-only protection; document optional stronger defenses separately.

## 19. Resumable transfer and partial failures

- Enable resumable receive with `zfs receive -s` when negotiated and supported. The receiver stores and reports `receive_resume_token`; the sender resumes with `zfs send -t TOKEN`. Tokens are opaque and validated against authenticated transfer and destination identity.
- Preserve a stable operation ID through retries; idempotent retry after acknowledgement loss must discover the already-published PIT and return success, not attempt duplicate receive.
- Aborting a saved partial receive with `zfs receive -A` is an appliance-local cleanup decision, **not** permitted directly to a cluster writer. Explicitly bound temporary space and retention time.
- Treat sender and receiver exit code, byte count, checksum/stream validation, snapshot GUID comparison, and durable manifest publication as separate conditions. Use `pipefail` for shell transfer pipelines and never infer success from SSH exit alone.
- On backup coordinator crash, restart from durable intent but reconcile target live state and resumable token before repeating any mutation.
- On receiver crash, distinguish locked pool, pool read-only, service offline, partial receive, and committed PIT awaiting acknowledgement.
- On VM owner move mid-send, existing snapshot on the old host may remain streamable if valid and safely accessible, but automatic restart from authoritative new owner is safer. Resume only if base/endpoint GUIDs and stream token remain valid; otherwise retry with a newly coordinated operation and do not delete the old required PIT prematurely.
- Maintain a bounded append-only audit log per appliance and coordinator. Log operation IDs, PIT IDs, GUIDs, version/capability profile, state transitions, but never guest content, tokens, credentials, or unlock passphrases.

## 20. Appliance data integrity, scrubs, and validation

### 20.1 Pool scrubs

- Schedule regular **pool-level** ZFS scrubs (`zpool scrub <pool>`) via `bmac-backupd` or delegated systemd scrub timers. Proposed default: every 30 days with a staggered schedule; allow per-pool configuration.
- Expose last start/completion, running progress, detected checksum errors, corrected vs uncorrectable corruption, affected datasets (when attributable), and pool/vdev health using `zpool status -v` and equivalent machine-readable interfaces.
- A scrub reads allocated data including snapshot-retained blocks and verifies ZFS checksums. It can automatically correct corrupt copies **only when redundant valid data exists**. On unrepairable corruption, mark affected backup recovery points suspect and alert; do not claim integrity.
- Avoid overlapping scrubs and expensive transfers unless allowed by configurable IO/bandwidth policy; a running scrub must not itself make PIT retention unsafe.
- Scrubs cannot detect logical malware or guarantee a restored guest will boot.

### 20.2 Restore verification

- Support manual restore-test workflow from the Web Dashboard as a required initial feature: choose PIT, restore or clone to isolated scratch VM, verify boot/agent/application checks, record immutable result, then clean up only scratch assets.
- Add optional scheduled restore tests once the basic workflow is stable. A test must never write to protected backup zvols or advertise public production endpoints.
- Record last tested PIT, test timestamp, boot and application status, configuration compatibility, and any changes in dependencies.
- Backup publication success, ZFS scrub health, and VM restore test success are **three separate status signals**.

## 22. Monitoring, metrics, and alerts

### 22.1 Per target and per VM

Display at least:

- Configured/paused/retired, reachable/unreachable, storage locked/unlocked, last probe.
- Most recent published PIT, age, source branch, lag in scheduled intervals, latest attempted PIT, last error, consecutive failures.
- Checkpoint status, bookmark availability on eligible HA hosts, preservation grace time remaining, source/peer repair eligibility.
- Last transfer type (full/incremental/peer), bytes transferred, effective throughput, duration, progress, estimated remaining bytes when measurable; never invent exact ETA.
- Total pool size, allocated/free, fragmentation/health, snapshot usage, quota headroom, reservations and warning thresholds.
- Scrub status and checksum errors, last successful test restore, PITs with known problems.
- PIT retention counts by tier, branch ancestry, protected/held origin snapshots, target recovery availability.
- Encryption and transport mode, certificate expiration, protocol version compatibility.

### 22.2 Alert policy

- Emit Web Dashboard alerts and optional outbound email on missed PIT, target unreachable, grace nearing expiry, expired incremental checkpoint, failed/resumable receive, pool below capacity thresholds, scrub errors, backup publishing failure, stale recovery tests, credential expiration, and loss of minimum healthy target redundancy.
- Email transport configuration and secrets are protected. Deduplicate repeated alerts with acknowledgment, escalation timing, and a recovery/resolved notification.
- Sample warnings: below 20% pool free; critical below 10% or projected receive cannot fit. Defaults are configurable and measured against quotas/refreservations, not pool-wide free bytes alone.
- A locked appliance after planned reboot is `LOCKED`, not `REMOVED`. Notify that local unlock is required; respect operator-approved maintenance silences.

## 26. Operations and algorithms (reference pseudocode)

### 26.1 Scheduled VM PIT

```text
with_cluster_vm_lock(vm_uuid):
  assert quorum, authoritative HA owner, stable disk inventory, sufficient source space
  cfg_before = capture_config_and_registry()
  if consistency_requires_quiesce: quiesce_with_watchdog()
  try:
    create_same_named_snapshots_atomically_for_all_required_zvols()
  finally:
    thaw_if_frozen()
  cfg_after = capture_config_and_registry()
  if config_changed_or_missing_required_snapshot: quarantine_PIT_and_fail()
  manifest = build_PIT_manifest(cfg_before, disk_GUIDs, config_digest, branch)
  register_PIT_locally(manifest)
  coordinate_existing_Proxmox_replication_as_required()

for target in independently_configured_targets:
  enqueue_send_or_catchup(target, pit)

for target_success in results:
  authenticate_and_reconcile_published_PIT()
  advance_only_that_target_checkpoint()

reconcile_source_local_retention_and_target_checkpoint_bookmarks()
```

### 26.2 Reconcile a target after outage

```text
authoritative_state = appliance.get_published_heads_and_partial_receives()
if target_head_matches_source_snapshot_or_bookmark_and_forward_descendant_exists:
    incremental_source_to_target()
elif authorized_peer_has_matching_base_GUID_and_published_descendant:
    incremental_peer_to_target()
else:
    propose_new_full_seed_into_new_target_dataset()

only_after_appliance_publish: advance_checkpoint()
retain_registration_regardless_of_grace_expiry()
```

### 26.3 Retention prune on appliance

```text
with_local_appliance_retention_lock():
    live_state = inspect_zfs_snapshots_clones_holds_bookmarks_resume_states()
    published = load_and_validate_manifests()
    keep_set = policy_buckets + branch_roots + active_heads + dependencies + holds
    candidates = published_snapshots - keep_set
    dry_run_space_and_dependencies(candidates)
    revalidate_live_state_before_mutation()
    release_only_retention_service_owned_holds_for_approved_candidates()
    destroy_only_approved_snapshot_GUIDs_with_exact_names()
    commit_audit_records_and_reconcile_catalog()
```

### 26.4 Restore transaction

```text
choose(target_uuid, vm_uuid, branch_uuid, pit_uuid)
verify_published_manifest_and_every_required_disk()
reserve_new_VM_and_disk_names_and_isolated_network()
for disk in manifest: full_send_selected_PIT_into_new_recovery_zvol()
verify_all_restored_GUIDs_and_disk_geometry()
rebuild_VM_config_with_explicit_compatibility_mapping()
start_in_isolated_network(); validate_boot_and_service()
if user_approves_promotion:
  fence_old_owner_and_block_old_replication()
  promote_exact_recovered_branch_and_VM_identity()
  reseed_or_reconcile_HA_replicas()
  publish_routes_and_enable_normal_HA_only_after_checks()
else:
  retain_or_destroy_scratch_VM_per_operator_request()
```

## 27. Detailed failure and recovery behavior

| Condition | Mandatory response |
|---|---|
| One offsite target unreachable | Mark degraded; protect checkpoint through grace; continue other targets |
| Target down beyond grace | May release checkpoint protection; never remove registration automatically |
| Target returns late | Reconcile GUIDs; source bookmark, peer repair, then full seed fallback |
| Sender crashes midstream | Preserve last committed target PIT; resume if token valid |
| Receiver crashes midstream | Reconcile partial receive; never publish incomplete PIT |
| Publication ack lost | Idempotently discover committed PIT and accept retry as success |
| Pool locked after reboot | Report needs local passphrase; no auto-unlock; queue/retry later |
| Pool full | Stop admission; alert; do not destroy protected PITs via production request |
| Scrub detects unrepairable error | Alert, mark affected PIT suspect, preserve alternate target copies |
| One required volume missing | PIT incomplete; no restore/promote; repair/reseed required |
| Snapshot GUID mismatch | Reject incremental; no forced receiver rollback |
| HA owner moves before snapshot | Re-resolve owner; retry capture under lock |
| Owner moves during send | Validate pinned snapshots/token on new source; safely resume/retry |
| Placement host added | Reconcile usable PITs/bookmarks before eligible sender use |
| Old staging clone pins a PIT | Retention skip; expose reason; do not force-delete clone |
| Local rollback to old PIT | Quiesce/fence; create new branch and separate offsite lineage |
| Peer lacks compatible base | Try another peer or full seed; never fabricate lineage |
| Cluster destroyed | Install replacement, recover config/VM/disks from appliance, reestablish HA |
| Appliance OS destroyed but pool survives | Unlock/import pool and reinstall daemon; reconstruct catalog |
| Appliance pool destroyed | Register new destination and start full backup |
| Web Dashboard process exits | Scheduled coordinator and appliance retention/scrubs continue |
| Concurrent owner/disk resize operation | Serialize with BMAC lock, revalidate geometry after |
| Guest quiesce hangs | Watchdog thaw; fail or downgrade only with explicit policy authorization |
| Operator requests rollback with dependent clones | Refuse destructive rollback; offer clone-and-promote |
| Certificate revoked | Deny future connection; preserve all existing PITs |

## 28. Test plan and acceptance criteria

This is a release gate, not a sample checklist. Tests must include Rust unit/adapter and external contract tests, protocol/security tests, and automated integration tests on **actual ZFS + Proxmox**. All maintainer-reported 500+ legacy test cases must be catalogued, mapped and satisfied by Rust or reviewed stronger replacements; baseline legacy fixtures and UI golden tests remain available for differential checking.

### 28.0 Native Rust workflow migration and external-contract gate

- [ ] Freeze an exact Git baseline; enumerate **all existing Bash/Python unit test cases** (maintainer reports over 500), mocks, fixtures and expected side effects; record pass/fail/skipped baseline with artifacts.
- [ ] Every legacy test has a stable ID and a verified mapping to new Rust tests or a documented, reviewed stronger replacement. CI prevents test disappearance or unmapped behavior.
- [ ] Every external-command boundary has versioned request/response/error/side-effect fixtures from the existing script tests, plus rust adapter tests for partial stdout, warnings on stderr, malformed JSON/text, nonzero status, timeout, SIGTERM, disconnection and uncertain remote completion.
- [ ] Rust invokes external commands with strict `Command` argv, allowlisted environment, bounded output, deadlines, cancellation, identity and permission checks. No web-supplied command strings or shell expansion are permitted.
- [ ] Safe mocked differential runs compare original scripts to Rust planned calls, ordering, resulting state, cleanup and errors. Intentional mismatches have a reviewed decision and new regression tests.
- [ ] All production workflows have been completely translated into Rust, including the significant host/HA/QDevice/placement/staging/ingress/ZFS/provisioning interactions found in the verified repository inventory. No human-callable operational Bash/Python engine remains in shipped coordinator.
- [ ] Rust workflow parity has been validated with the real Proxmox/OpenZFS/tools on live non-destructive and controlled destructive test infrastructure, including failure injection and no known HA/ingress regressions.
- [ ] Rust job activity and interactive requests persist across UI disconnect, leader failover and safe daemon restart; the old NDJSON protocol is required only for test/reference compatibility.

### 28.1 ZFS integration

- [ ] Full send one sparse zvol to newly created target; identical selected snapshot GUID and contents.
- [ ] Incremental send from exact snapshot; only endpoint PIT created on target with `-i`, old target PIT retained.
- [ ] Receive incremental from a **source bookmark** after deleting source base snapshot.
- [ ] Prune intermediate target PITs with older/newer PITs intact; verify both survive and are restorable.
- [ ] Confirm target's unrelated older history does not prevent an ordinary valid append.
- [ ] Reject matching names with mismatched GUIDs.
- [ ] Verify multi-zvol atomic snapshot command, PIT capture and manifest consistency.
- [ ] Confirm target cannot accept an incremental if its live head is inconsistent with expected base.
- [ ] Interrupted initial full and incremental transfers resume correctly using tokens.
- [ ] `zfs receive -A` cleanup handled by appliance, not cluster identity.
- [ ] Restore an old non-head PIT into a fresh zvol and boot isolated VM.
- [ ] Test thin/zvol geometry and feature flag compatibility across supported OpenZFS versions.
- [ ] Test clone-origin incremental branch mechanics: if unsupported, verify full-seed fallback preserves old branch.

### 28.2 HA and BMAC integration

- [ ] Existing staging linked clones work unchanged during active backup snapshots.
- [ ] Production snapshot appears on each intended HA replica; ordinary Proxmox replication continues.
- [ ] Backup checkpoint bookmarks verified on all eligible owners; failover still permits next incremental.
- [ ] Change production owner just before and during backup, without stale-owner modifications.
- [ ] Add third/fourth HA placement host and verify bookmark/snapshot availability, no production outage.
- [ ] Change VM disk size after a successful PIT; source/target geometry handled safely.
- [ ] Remove/add a VM disk and test manifest changes and correct full/incremental strategy per disk.
- [ ] Two disks, EFI/TPM where applicable, correctly restored from same PIT.
- [ ] Full cluster deletion recovery with only appliance data and fresh Proxmox hardware.
- [ ] Promotion rejects stale replicas and only enables HA after re-replication.
- [ ] Coordinator restarts/crashes under every phase and resumes idempotently.

### 28.3 Multiple targets and repair

- [ ] A/B/C get differing last PITs; A and C continue while B offline.
- [ ] B rejoins within grace using production base/bookmark and direct incremental.
- [ ] B rejoins after source base snapshot was deleted; source bookmark catch-up succeeds.
- [ ] B rejoins with no source base but healthy peer with common GUID; peer-assisted repair succeeds.
- [ ] Peer only has a bookmark for B base; new snapshot available; repair succeeds or fails safely per compatibility.
- [ ] No source/peer common base: system offers full reseed into **new** zvol and preserves historical backups.
- [ ] Grace expiry does not remove target registration or halt healthy target backups.
- [ ] Retired target loses write authority but does not automatically lose stored data.

### 28.4 Security negative tests

- [ ] Stolen cluster writer credential cannot execute shell or ZFS commands on appliance.
- [ ] Writer cannot invoke `destroy`, `rollback`, `release`, `promote`, `-F`, `-R`, retention change, or unauthorized dataset receive.
- [ ] Forged base GUID, PIT ID, unexpected stream features, unsafe ZFS properties, or malformed metadata rejected.
- [ ] Writer cannot export old recovery PITs using append credentials.
- [ ] Peer cannot destroy/modify backups on any appliance.
- [ ] Writer cannot escape authorized ZFS dataset prefix through naming or traversal.
- [ ] Quotas and stream admission limits prevent unbounded peer/cluster writer exhaustion.
- [ ] LUKS passphrase never appears in logs, Web Dashboard history, CLI args, or email.
- [ ] LUKS-locked reboot remains safe and requires human local/out-of-band action.
- [ ] Appliance admin auth and cluster write auth are cryptographically and operationally separate.
- [ ] Retention weakening / branch destruction requires explicit authorized appliance-admin action and planned hold checks.

### 28.5 Integrity and Web Dashboard

- [ ] Scrub schedules trigger and report progress, health, errors; pool redundancy repair detected when applicable.
- [ ] Failed scrub does not mark VM restore tests healthy; restore tests are separately reported.
- [ ] Web Dashboard shows exact per-target published PIT/RPO, offline grace and available recovery paths.
- [ ] Branch tree correctly displays original and post-rollback histories; active PIT obvious with text/icon, no color-only meaning.
- [ ] Operator can initiate full seed, increment, repair, restore, test restore, branch retire, and policy change through Web Dashboard allowlisted workflows.
- [ ] Browser HTML forms, optional Rust CLI and coordinator scheduled jobs execute the same Rust operations with equivalent safety/semantics; legacy `--json` transcripts are mapped to typed Rust event tests and no production script subprocesses remain.
- [ ] All scheduled protection continues with Web Dashboard closed and administrative workstation offline.
- [ ] Email warning, duplicate suppression, and recovery notifications work.

### 28.6 Release-gating scenarios

Before declaring production-ready, run a continuous live hardware scenario covering: initial full seed to A/B, repeated daily incremental PITs, creation/removal of staging clones, owner move and hard failover, A offline while B continues, A rejoin via source bookmark, A repair from B after grace, source rollback from local PIT causing branching, historical restore from both branches, deletion of old intermediate PITs, LUKS-locked appliance reboot, resumed receive, pool scrub, full VM recreation, and complete cluster loss/rebuild drill.

No **data-loss or retention-safety** bug may be left as an accepted known issue for a production release. A passing transfer alone is not enough to claim backup recovery works.

## 30. OpenZFS / Proxmox behaviors requiring explicit proof

The following must be demonstrated in the supported test matrix **before the corresponding optimization is enabled**. They are not unresolved user requirements; the specification defines safe fallback behavior:

1. **Bookmarks through owner moves:** Proxmox custom-snapshot replication was validated by the project, but bookmark propagation on every possible HA owner requires separate proof. Fallback: keep actual snapshot on every required host.
2. **Clone-backed branch receiving:** verify native incremental receive semantics when target branch begins as a clone of an older backup snapshot. Fallback: full-seed a new branch zvol.
3. **Linux receive least privilege:** test `receive:append`, required `create`/`mount` delegations, helper capabilities and property sanitization. Fallback: dedicated narrow privileged helper with hardcoded operations and no remote shell.
4. **Version-compatible send features:** negotiate OpenZFS pool feature support before using compression, large-block or advanced flags. Fallback: conservative compatible send stream or new full seed.
5. **Guest consistency:** verify `qm`/QGA freeze/thaw on the supported guest and installed Proxmox version; if required and unavailable, reject backup rather than silently calling it application-consistent.
6. **Disk geometry changes:** verify exact volsize and volblocksize behavior for incremental receivers after an online guest disk expansion. Fallback: explicitly orchestrated new full seed where incompatible.
7. **ZFS snapshot/receive holds and readonly:** prove helper cannot bypass appliance retention protections under crafted incoming streams. Fallback: an isolated temporary receiving dataset and publication step that never exposes old PITs to destructive privileges; if impossible for a given version, exclude that platform from supported use.
8. **Proxmox HA / config restoration:** verify `qm create`/`qm set` and correct copying/rebinding of EFI/TPM/config resources on fresh hardware. Fallback: fail clearly with recorded manual recovery prerequisites, never claim complete restoration.

## 32. Operator documentation to ship

- Appliance hardware/OS/ZFS feature compatibility and secure LUKS setup.
- Manual local-console unlock after reboot; diagnostics when locked.
- Storage sizing and redundancy guidance; different local vs offsite RPO and RTO expectations.
- Enrollment and credential rotation, revoked clients, firewall/Tailscale versus direct TCP/mTLS.
- Schedule/retention explanation including calendar buckets, snapshot sharing, bookmarks, and grace protection.
- Live VM consistency contract and how to add optional application pre/post hooks.
- How to add a target, pause/reconnect, repair from a peer, and reseed without erasing old data.
- Local quick clone/rollback versus full offsite restore, including destructive confirmation boundaries.
- Recovery branches, how to read branch history, and safe retirement.
- How to recreate a VM and rebuild an entire cluster with no source surviving.
- Pool scrub interpretation, ZFS checksums, redundancy, and manual restore drills.
- Alert interpretation, RPO status, and logs with sensitive-data redaction.
- Recovery from power loss, interruption, disk corruption, lost keys, failed hardware and untrusted production credentials.

## 33. Authoritative technical references

Reference links from the earlier design are retained for context; verify their current paths/content and the current Proxmox/OpenZFS versions before implementation. These references support *primitives*, not a claim that the proposed BMAC subsystem exists:

- [BMAC repository](https://github.com/OtterFlip/bmac) - current source is ultimately authoritative over stale design descriptions.
- [BMAC Web Dashboard README](https://github.com/OtterFlip/bmac/blob/main/dashboard/README.md) - existing Tauri/React + Rust + allowlisted-script UI contract.
- [BMAC Web Dashboard workflow registry](https://github.com/OtterFlip/bmac/blob/main/dashboard/engine/workflows.json) - workflow integration convention.
- [BMAC MAIN_DESIGN.md](https://github.com/OtterFlip/bmac/blob/main/MAIN_DESIGN.md) - HA ownership, ZFS replication, registry/locking, stage clone and disk conventions.
- [OpenZFS Send and Receive](https://openzfs.github.io/openzfs-docs/Basic%20Concepts/Operations/Send%20and%20Receive.html) - full/incremental streams, bookmarks, resume, safe receive modes.
- [OpenZFS Snapshots, Clones and Bookmarks](https://openzfs.github.io/openzfs-docs/Basic%20Concepts/Datasets/Snapshots%20and%20Clones.html) - snapshot sharing, clones, promotion, bookmarks.
- [OpenZFS Delegated Administration](https://openzfs.github.io/openzfs-docs/Basic%20Concepts/Operations/Delegated%20Administration.html) - receive permissions, Linux delegation constraints.
- [OpenZFS zfs-receive(8)](https://openzfs.github.io/openzfs-docs/man/master/8/zfs-receive.8.html) - receiver lineage and `-F` behavior.
- [OpenZFS zfs-snapshot(8)](https://openzfs.github.io/openzfs-docs/man/v2.2/8/zfs-snapshot.8.html) - atomic creation of multiple specified snapshots.
- [Proxmox qm.conf](https://github.com/proxmox/pve-docs/blob/master/qm.conf.adoc) - text VM config.
- [Proxmox qm CLI](https://github.com/proxmox/pve-docs/blob/master/generated/qm.1-synopsis.adoc) - VM read/create/restore operation options.
- [Proxmox ZFS storage backend](https://github.com/proxmox/pve-docs/blob/master/pve-storage-zfspool.adoc) - Proxmox ZFS guest disk and replication context.

---

**Definition of done:** BMAC can take repeatable VM PITs, store them on independent append-only-from-production Linux backup appliances, independently prune local/offsite history, heal offline targets with bookmarks or peers where possible, recover any retained PIT and its VM configuration after loss of the entire cluster, safely start a new backup lineage after local rollback, prove restored VMs work, and operate all of that from the cluster-hosted Web Dashboard backed by the **native Rust coordinator workflows** without weakening Proxmox HA or the appliance's security boundary.

---

# PART C - REQUIREMENT TRACEABILITY AND NON-NEGOTIABLE INVARIANTS

## C1. Primary architectural invariants

1. **The desktop is an ISO factory only.** It must not be a hidden scheduler or cluster control plane.
2. **Every Proxmox host runs `bmacd`,** serving Web Dashboard pages and local workers, regardless of current VM placement.
3. **Shared durable state is authoritative server-side.** The browser is a rendering/input client; SQLite on each host is a log/cache, not independent competing desired state.
4. **At most one authorized coordinator leader admits conflicting cluster mutations.** Quorum loss fences all new destructive operations.
5. **Live system facts are verified at execution time.** Cached owner, placement, snapshot name, PIT existence and ZFS GUID may not substitute for confirmation.
6. **The final BMAC operational implementation is native Rust in `bmacd`.** The pinned 500+ legacy tests, scripts and external-command mocks are an explicit executable migration contract. Every old test must be mapped to a Rust equivalent or reviewed stronger replacement; the Bash/Python operational engine is not shipped as a parallel implementation.
7. **Browser interaction runs through authenticated HTTPS,** using RBAC, revocable session credentials, CSRF/idempotency checks, fixed allowlisted workflows and encrypted inter-host RPC.
8. **No cluster-held credential can administrate appliance retention** or delete/alter any previously published and protected PIT. Valid appending does not authorize rewriting. Independent appliance-admin approval is mandatory for destructive retention overrides.
9. **Backups continue while browser and workstation are offline.** Target outage does not stop healthy targets; offline target settings persist independent of checkpoint-grace expiry.
10. **Every published VM recovery point is self-describing:** disk zvol PIT GUIDs, config, companion volumes and required recovery metadata. Partial disk receives are not committed VM PITs.
11. **Local and offsite PIT retention are independent.** Local PITs support fast clones/rollback; offsite PITs support long history and full-cluster-loss recovery; bookmarks are **not** themselves restorable.
12. **Rollback branch transitions never erase historical backup branches.** Clone dependencies and ancestors must be reconciled before pruning.
13. **Manual LUKS unlock is intentional.** A locked backup appliance isn't administratively removed or silently recreated.
14. **Optional Postmark notifications are not an availability dependency.** The key stays on protected cluster servers, messages are idempotency-deduplicated best effort, and event preferences are first-class Web Dashboard settings.
15. **All mutable infrastructure operations are audited** with human/machine identity, coordinated job ID, approved plan, host/leader epoch and actual result. Failed partial work is reconciled from live truth rather than cosmetically marked complete.
16. **Complete cluster-loss restore cannot depend on surviving cluster credentials.** Appliance-side independent recovery identity plus replacement host and stored manifests are sufficient.
17. **No unverified customized ISO may be used as a durable credentials store.** Bootstrap grants expire/revoke and physical ISO copies must be destroyed by operator procedure.
18. **Every host stores every other host's eligible logs within explicitly bounded retention.** Diagnostics replicate via authenticated host-to-host connections and are viewable after origin host loss; no runtime logs or local SQLite database are ever stored in `pmxcfs`.

## C2. Compatibility and revision notes

- This integrated document carries forward three intentional changes to the previous backup-only design: (a) all live BMAC work runs on Proxmox-side `bmacd`; (b) Dashboard is a web server on every Proxmox host and no longer Tauri; (c) the desktop app retains only the two ISO-building paths. Legacy instructions to extend the local desktop Dashboard or drive cluster workflows from a workstation are **superseded**.
- **Change from v3:** The production Bash/Python operational engine and NDJSON subprocess dependency are retired in favor of typed Rust workflows in `bmacd`. The legacy scripts, 500+ unit tests and external-command mocks become pinned migration references; the Rust engine emits native durable events rendered as authenticated HTML fragments. The browser never consumes legacy-script NDJSON.
- The Rust rewrite MUST start from a frozen inventory of every test case and existing external-tool stub; `CommandRunner` and typed domain adapters are used to reproduce validated argv/stdout/stderr/exit/side-effect contracts. Existing scripts are allowed as test-only reference implementations during conversion, never as unreviewed permanent production fallbacks.
- `cluster.conf` continues as bootstrap/operator configuration, but durable live cluster settings and secrets use protected server-side mechanisms, not a workstation file required at runtime.
- Browser HTTPS uses **host-unique self-signed leaf certificates created by BMAC Host Deploy when building each Proxmox ISO**, with independent fingerprint verification. The earlier local cluster CA as default has been intentionally superseded.
- For runtime SQL use **SQLite + SQLx** rather than assuming Turso provides SQLx's checked query macros. Cluster truth remains compact and quorum protected; SQLite is local, not distributed by copying files.
- **Every coordinator keeps replicas of every other coordinator's eligible logs.** All logs, audit streams and raw process output stay off `pmxcfs`. Missing last seconds are possible until durable peer acknowledgement and must be visibly marked.
- Web HTML endpoints are still part of a private HTTP interface, even though there is no *public general-purpose management API*. A literal 'no API at all' interpretation is not technically meaningful for a web application.
- The project's greenfield selection document has been directly checked: [`docs/WEBAPP_STACK_OPTIONS.md`](https://github.com/OtterFlip/bmac/blob/main/docs/WEBAPP_STACK_OPTIONS.md), and the chosen Rust/Axum/Askama/HTMX/SQLx stack agrees with it. Other repository paths must still be verified against the deployed commit.

## C2.1 Rust migration evidence and required delivery artifacts

The implementation repository MUST contain a machine-readable `legacy-tests-to-rust` matrix, an external-integration contract catalog, realistic mock fixtures/transcripts, a Rust workflow registry, a reproducible pinned legacy-baseline test harness, the Rust unit/adapter/integration suite, and a migration review report. Provide counts for total legacy cases, mapped cases, justified supersessions, unresolved differences and successful real-cluster test workflows. Preserve the semantics of a previously tested external call even where Rust's compiler cannot enforce its contract; only validated intentional improvements can change that behavior.

Any operation touching real Proxmox HA, ZFS zvols, replication, networking or machine enrollment MUST be executed by an allowlisted typed Rust operation and adapter, subject to the same BMAC cluster authorization and fencing as every other operation. There is no separate script shell or CLI route capable of bypassing the coordinator's safeguards.

## C3. References for required primitives

- [BMAC project](https://github.com/OtterFlip/bmac)
- [BMAC greenfield webapp stack](https://github.com/OtterFlip/bmac/blob/main/docs/WEBAPP_STACK_OPTIONS.md)
- [SQLx SQLite compile-time query validation](https://docs.rs/sqlx/latest/sqlx/macro.query.html)
- [Proxmox `pmxcfs` documented capacity](https://pve.proxmox.com/pve-docs-9-beta/chapter-pmxcfs.html)
- [OpenZFS send/receive](https://openzfs.github.io/openzfs-docs/Basic%20Concepts/Operations/Send%20and%20Receive.html)
- [OpenZFS snapshots, clones and bookmarks](https://openzfs.github.io/openzfs-docs/Basic%20Concepts/Datasets/Snapshots%20and%20Clones.html)
- [Proxmox pmxcfs](https://pve.proxmox.com/pve-docs/chapter-pmxcfs.html)
- [Proxmox cluster lock implementation](https://github.com/proxmox/pve-cluster/blob/master/src/PVE/Cluster.pm)
- [HTMX polling](https://htmx.org/docs/#polling)
- [Postmark Email API](https://postmarkapp.com/developer/api/email-api)

**Definition of done:** On any healthy Proxmox host, an authenticated administrator can open the visually faithful BMAC Web Dashboard over fingerprint-verified self-signed HTTPS, run every formerly desktop-managed workflow and manage complete independent ZFS backup/recovery through secure appliances; **BMAC Host Deploy** builds only temporary customized ISOs and prints corresponding host certificate fingerprints; quorum-safe coordinators independently execute schedules, handle failover and send optional Postmark notifications; **each host keeps bounded authenticated replicas of every other host's logs outside `pmxcfs` and can show the logs of failed hosts**; full cluster loss can be recovered from protected appliance data and independent credentials.
