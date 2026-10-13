<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v3 architecture

This is the entry point for the BMAC v3 design. It records the decisions,
the reasons for them, and how the parts fit together. Subsystem detail lives
in the companion documents:

| Document | Covers |
| --- | --- |
| [`DEPLOYMENT.md`](DEPLOYMENT.md) | BMAC Deployer, hardware discovery, booting the target, both ISO types, first-host bootstrap, join bundles, host setup, upgrades |
| [`SECURITY.md`](SECURITY.md) | Threat model, login and sessions, HTTPS certificates, process separation, secrets |
| [`STORAGE.md`](STORAGE.md) | Pool layouts with any vdev type and disk type, LUKS, disk workflows |
| [`BACKUP.md`](BACKUP.md) | Backup appliance, pull-based backups, retention, restore, disaster recovery |
| [`MONITORING.md`](MONITORING.md) | Health collectors, alerts, notifications, external watchdog, logs |
| [`WORKFLOW_MIGRATION.md`](WORKFLOW_MIGRATION.md) | Running v2 scripts under `bmacd`, porting them to Rust, testing |

[`BMAC_MAJOR REWRITE_SPEC.md`](BMAC_MAJOR%20REWRITE_SPEC.md) is the earlier,
longer specification. It is reference material: where it disagrees with these
documents, these documents win. Its backup detail (Part B) was carried into
`BACKUP.md` and adapted to the pull model.

The v2 code in the repository root (`scripts/`, `dashboard/`, `docs/`) stays
unchanged and is the behavioral reference for v3. All v3 work lives under
`v3/`. v3 does not need to be compatible with v2 installations.

## Goals

1. Move every running operation and all persistent operational state off the
   administrator's workstation and onto the Proxmox cluster.
2. Keep every v2 workflow and its safety behavior: host setup and removal,
   production VMs with HA, staging linked clones, placement and owner changes,
   disk growth, storage workflows, QDevice, ingress, deferred cleanup.
3. Add offsite backup and restore, monitoring, and alerting.
4. Keep the v2 Dashboard's look, layout, and interaction model, served as a
   web application from every host.
5. Support any pool layout and disk type, with LUKS wherever it is enabled.

## Non-goals

These v2 boundaries remain (see `docs/MAIN_DESIGN.md`, "Deliberate boundaries"):
no Cloudflare automation, no automatic rebalancing, no generic application
deployment, no Ceph or shared storage, one active owner per production VM, at
most 32 hosts (v2 allowed ten; v3 also lets the private subnet be any
network from /16 to /24 instead of only a /24). v3 adds no public WAN listener for BMAC itself.

## Components

```text
Admin workstation                      Proxmox host (every host, mox1..mox32)
+-------------------------+            +-------------------------------------------+
| BMAC Deployer (Tauri)   |  ISO --->  | bmacd  (one program, one package)         |
|  - discovery ISO        |            |  HTTPS front end :8443 (unprivileged):    |
|  - Proxmox host ISO     |  TLS cert  |    React web app, /api/v1 JSON, SSE       |
|  - Backup appliance ISO |  check     |        | Unix socket, typed requests      |
|  - iDRAC media + power  |            |  engine (root): jobs, workflows,          |
|  - handoff link         |  SSH, own  |    scheduler, collectors, alerts,         |
|  - workstation SSH      |  key  ---> |    notifications, backup source,          |
+-------------------------+            |                                           |
Browser (any machine) -- HTTPS ----->  |    log store, live peer streams           |
                                       | bmac-hook   (QEMU lifecycle hook, no      |
                                       |              dependency on bmacd)         |
                                       +-------------------+-----------------------+
                                                           |
                               pmxcfs /etc/pve/priv/bmac   |  shared state, lock
                                                           |
     appliance checks in each minute, pulls snapshots     |  (cluster has no
                                                           v   credential to it)
                                       +-------------------------------------------+
                                       | Backup appliance (Ubuntu Server)          |
                                       |   bmac-backupd: puller, retention, scrubs,|
                                       |   own HTTPS UI, heartbeat watchdog        |
                                       |   LUKS (manual unlock) + ZFS              |
                                       +-------------------------------------------+
```

- **BMAC Deployer.** A copy of the v2 Dashboard desktop app, renamed and cut
  down to provisioning: a hardware-discovery boot ISO, the Proxmox host
  installer ISO, and the backup appliance installer ISO, with the steps for
  booting them through the iDRAC's Virtual Console, another remote console,
  or USB; the operator boots them. When a new host joins the
  tailnet, it checks the host's certificate, sets up the workstation's SSH
  access to it as v2 does, and hands the operator a link to that host's Web
  Dashboard, where host setup continues. It also manages the workstation's
  SSH entries for hosts and for jump SSH to guests, as v2 does, using
  read-only `bmac` commands over SSH. It never calls `bmacd`'s API. It
  keeps Tauri and React and
  shares its component library with the Web Dashboard. It holds no cluster
  state between runs; for every host after the first, and for every appliance,
  it imports a join bundle issued by the running cluster.
- **`bmacd`** (package `bmacd`, on every host). One program that is both the
  coordinator and the web server. It serves the Web Dashboard (a React app
  embedded in the binary), the JSON API under `/api/v1/`, and Server-Sent
  Events for live updates, all over HTTPS on port 8443. It runs as three
  processes started from the same binary:
  - the **engine**, as root: jobs, workflows, the scheduler, health
    collectors, alert evaluation, notifications, PIT capture, the host-local
    log store, and live streams to and from peers;
  - **`bmac-source`**, the backup source endpoint on port 8445, as the user
    `bmac-source` with only ZFS `send` and `hold` delegated on guest datasets
    (`BACKUP.md`, `SECURITY.md`);
  - the **HTTPS front end**, as the unprivileged user `bmac-web` in a
    sandboxed systemd unit that starts and stops with the engine. It
    terminates TLS, serves the static web
    app, checks login tokens, and relays API calls and event streams to the
    engine over a Unix socket. It cannot run commands or read `/etc/pve/priv`.
    This is the same split Proxmox uses between `pveproxy` and `pvedaemon`; it
    means a bug in HTTP handling does not hand out root.
- **`bmac-hook`.** The QEMU lifecycle hook (v2's
  `app-ha-guest-role-hook.sh`), installed on every host. Proxmox runs it on
  every guest start and stop, so it must keep working when `bmacd` is down.
  It reads pmxcfs directly.
- **`bmac`** CLI on every host: read-only diagnostics (including
  `bmac guests --json` and `bmac guest-host-key`, which the Deployer runs
  over SSH and which work with `bmacd` stopped), and job submission
  through `bmacd` with the same checks as the web UI. It is not a second
  implementation. Commands that go through `bmacd` (`bmac jobs`,
  `bmac cancel`, `bmac run`) log in on every run with the Proxmox password
  and second factor (TOTP or a recovery key; a WebAuthn-only user is sent
  to the Dashboard), keep the token only in memory, and log out when they
  end, including on Ctrl-C, which leaves a started job running. `bmac run`
  follows the job and answers its requests at the terminal.
  `bmac run --terminal` runs a script operation's script in the terminal
  for debugging, outside the job system: it is for administrators, needs
  `bmac` and `bmacd` at the same version and no unfinished jobs, and asks
  for the operation's ID to be typed first.
- **`bmac-backupd`** on each backup appliance: checks in with the cluster
  every minute and pulls the backups it is offered, owns retention and scrubs, serves its own HTTPS admin UI, and watches the
  cluster's heartbeat.
- **QDevice.** Unchanged from v2, except that hosts, not the workstation,
  reach it as root over Tailscale, with one cluster key kept in
  `/etc/pve/priv/bmac/` that the operator authorizes on it once.

## Decisions

| # | Decision | Reason |
| --- | --- | --- |
| D1 | The desktop app does only provisioning and workstation work: hardware discovery, building ISOs, booting them, handing off to the Web Dashboard, and managing the workstation's SSH access to hosts and (by jump SSH) guests, and deploying the hello app over that access. It never calls `bmacd`'s API; host setup is a `bmacd` job followed in the Dashboard | Nothing operational depends on a workstation being online; one place for setup, its resumption, and troubleshooting; the Deployer holds no login token; the Dashboard can't change the workstation, so workstation settings stay with the workstation app |
| D2 | Hybrid workflow engine: adapted v2 scripts run under `bmacd` at first; each workflow switches to native Rust when its port passes | The platform can be built and used while the port proceeds; proven logic keeps running meanwhile |
| D3 | Web UI is a React app served by `bmacd`, talking to a JSON API over HTTPS and receiving live updates over Server-Sent Events; it shares its component library with the Deployer | Reuses the proven v2 Dashboard UI and its `Backend` interface; one design system for both apps; pushes changes instead of polling. The server still owns all state |
| D4 | pmxcfs is the only shared state and the only coordination mechanism; no separate leader election | Corosync already provides consensus; pmxcfs turns read-only without quorum, which fences stale writers. v2's lock and lease records already work |
| D5 | Detailed job events, logs, and metrics live in host-local SQLite; peers copy each other's logs over live streams, with catch-up after gaps | Keeps bulk data out of pmxcfs; logs of a dead host stay readable |
| D6 | Login uses Proxmox users and two-factor through the local Proxmox API; BMAC roles come from Proxmox groups | No separate user database; same login on every host; Proxmox already has TOTP and WebAuthn |
| D7 | Each host serves a unique self-signed certificate generated by the Deployer; the operator verifies its SHA-256 fingerprint | Defeats man-in-the-middle at first contact without a public CA |
| D8 | Host-to-host TLS reuses the Proxmox cluster CA and node certificates | Root on any node already controls the cluster; a second PKI adds work without adding a boundary |
| D9 | Cluster-wide secrets live in `/etc/pve/priv/bmac/secrets` | Proxmox keeps its own cluster secrets in `/etc/pve/priv` |
| D10 | Backup appliances pull from the cluster; the cluster holds no credential to any appliance | A compromised cluster cannot write to, delete from, or log in to an appliance |
| D11 | Appliance administration happens only on the appliance's own HTTPS UI | Appliance-admin credentials are never typed into a page the cluster serves |
| D12 | Failover never depends on `bmacd` | `bmac-hook`, HAProxy, keepalived, and nftables keep working with `bmacd` stopped |
| D13 | Notifications are optional, Postmark first, behind a channel interface | No delivery dependency; other channels can be added |
| D14 | Any vdev type (except dRAID, deferred) and any disk type, with LUKS where enabled | See `STORAGE.md` |
| D15 | The full backup design ships in v3.0 | See `BACKUP.md` |
| D16 | The browser holds its login token only in memory: no cookies, no browser storage. Every page load starts with a login | Stolen browser cookies or storage can never give access to the cluster |
| D17 | Hardware discovery always boots the target into a temporary discovery OS. The operator boots every ISO (iDRAC Virtual Console, another remote console, or USB) from the Deployer's steps; the Deployer doesn't drive the iDRAC, because Redfish virtual media needs the iDRAC to reach the workstation, which a provider's VPN doesn't allow (`DEPLOYMENT.md`, "Booting the target") | iDRAC inventory is unreliable (R640 disks often missing when powered off); the operator picks vdev members from what the machine itself reports |
| D18 | The block size of each VM disk is chosen when the VM is created, with a default suggested from the pools of its placement hosts | raidz wastes space on small blocks, mirrors don't, and workloads differ (`STORAGE.md`) |

## Trust boundaries

| Boundary | Rule |
| --- | --- |
| Browser to `bmacd` front end | HTTPS with a fingerprint-verified certificate; bearer token held only in the page's memory; role check on every request |
| Front end to engine | Unix socket restricted by peer credentials; only typed requests from a fixed set; the engine checks the token and role again |
| Host to host | One trust domain. Root on any Proxmox node can already read `/etc/pve/priv` and use cluster SSH. BMAC does not pretend otherwise |
| Cluster to appliance | The appliance connects to the cluster: it checks in every minute, reporting its status, and pulls the PITs the cluster's schedule offers it. The cluster never connects to an appliance, except a restore job holding an appliance-issued grant |
| Appliance administrator | Separate credentials, used only on the appliance's UI or console. Required for retention changes, branch retirement, and restore grants |
| Deployer | Runs on a workstation, produces sensitive single-use ISOs, holds no lasting credentials. Toward hosts it only checks HTTPS certificates (a TLS handshake, no request) and uses SSH with the operator's own key, to verify access and run read-only `bmac` commands; it never logs in to `bmacd` |

`SECURITY.md` has the detail.

## Where state lives

| Data | Location | Notes |
| --- | --- | --- |
| Cluster settings (v2 `cluster.conf` content) and per-host facts (v2 `moxN.conf` content) | `/etc/pve/priv/bmac/config/` (`cluster.json`, `hosts/moxN.json`) | Edited in the Dashboard with revision checks; checked with every rule of v2's `config.sh`, and more strictly where it is loose (full dotted addresses, no empty values, no secret material). A cluster change that would invalidate a host's facts is refused |
| Registry: resources, orchestration leases, deferred cleanup, history, ingress | `/etc/pve/priv/bmac/registry/` | v2 `app-ha` schema, same rules (1 MiB per file, atomic replace, no secrets) |
| Job records: summary, state, target host, lease, pending prompt | `/etc/pve/priv/bmac/jobs/` | Bounded: active jobs plus the newest 500 finished |
| Schedules | `/etc/pve/priv/bmac/schedules/` | One record per schedule, revision-checked |
| Backup policies and schedules, appliance registrations and states, PIT catalog, claims | `/etc/pve/priv/bmac/backup/` | PIT catalog bounded by the delinquency period and local retention; check-in times are kept in memory and written only on a state change |
| Alert state, notification outbox, silences | `/etc/pve/priv/bmac/alerts/` | Bounded |
| Host HTTPS certificates (public parts) | `/etc/pve/priv/bmac/hosts/<host>/https.pem` | Lets any verified host show the other fingerprints |
| Cluster-wide secrets | `/etc/pve/priv/bmac/secrets/` | Postmark token, guest root password hashes |
| Host HTTPS private key | `/etc/bmac/tls/` on that host | Root only; never in pmxcfs |
| Job events, logs, audit, metric history, copies of peers' logs | `/var/lib/bmac/bmac.db` (SQLite, WAL) plus compressed segments | Never in pmxcfs; size-bounded |
| Backups, manifests, retention state | Each appliance's ZFS pool | Appliance is authoritative |

BMAC's total pmxcfs use has a hard budget of 16 MiB, with a warning at 8 MiB.
Every writer checks the budget, and monitoring reports it. No logs, SQLite
files, or command output are ever written to `/etc/pve`.

## Coordination

v2's registry lock is kept: an empty directory created atomically under
`/etc/pve/priv/lock/`, with owner diagnostics in a sibling file and the same
stale-lock rules. Every shared mutation takes this lock for a few seconds,
checks record revisions, writes, and releases it. Without quorum, pmxcfs is
read-only, so no host can admit a mutation; this is the fencing.

**Jobs.** Any host can admit a job:

1. `bmacd` takes the registry lock, checks quorum, the operation's role,
   per-resource conflicts (v2's resource and orchestration records), and the
   idempotency key, then writes a job record naming the target host. The
   web app generates the idempotency key when it opens a launch dialog, so a
   repeated POST returns the existing job.
2. The target host's `bmacd` claims the job with a lease (nonce, holder,
   expiry), as v2's orchestration leases do, and renews it while running.
3. Long operations never hold the lock; they hold their lease and resource
   records. Joins, host removals, and QDevice changes also take the
   cluster-wide `cluster-membership` resource, so membership changes happen
   one at a time. This replaces v2's control node and its workstation lock;
   a join goes through the host named in its bundle, which takes the
   resource for it.
4. If a lease expires because its host died, the job becomes `INTERRUPTED`.
   Nothing re-runs a destructive job automatically on another host. Each
   workflow declares whether it can resume, and an operator resumes it, or a
   reconciliation step marks it failed after checking live state.

Job records live in `/etc/pve/priv/bmac/jobs/<id>.json`, written under their
own lock (`bmac-jobs-*` in `/etc/pve/priv/lock/`), separate from the
registry's. A lease lasts 60 seconds and the holder renews it every 15,
reporting its phase, whether a soft cancel may interrupt it, and any pending
request; the renewal returns any cancel order. Every host sweeps the records
every 10 seconds. A running job whose lease expired becomes `INTERRUPTED`, and
an admitted job its target hasn't claimed within 5 minutes fails, since it
never ran. An interrupted job keeps its resource locks until it is resumed or
marked failed. The holder may still record how the job ended after its lease
expired, because that is the truth about what happened. An admitted job is
cancelled at once; a running one gets a cancel order that its host carries
out. The records kept are every unfinished job and the newest 500 finished
ones. The target host comes from the operation's rule: `any` and
`placement`, the admitting host; `duty`, the duty host; `host`, the named host; `any_except`, any
online host but the named one; `owner`, the host whose node directory holds
the guest's config; `standby`, the lowest online placement host that isn't
the owner. A named host that is offline refuses the launch.

**Schedules without a leader.** Every `bmacd` evaluates the schedules. A
scheduled job's idempotency key is derived from the schedule ID and the slot
time, so only the first admission succeeds; the others see the existing job.
Each host evaluates every 30 seconds. A schedule recurs at an interval
counted from the Unix epoch, or daily, weekly, or monthly (days 1 to 28) at a
time in the cluster's time zone. A time skipped by a daylight-saving change
runs at the first moment after the gap, and a repeated time runs once. Jobs
run as the user `schedule:<id>`, with the role stored in the schedule, which
is never more than the role of whoever saved it. A slot that can't start (no
quorum, a conflicting job) is tried again until it is 15 minutes late, then
logged as missed; a slot that admission refuses outright (role, parameters)
is not retried. A slot earlier than the schedule's last edit doesn't run.

**Cluster duties.** A few tasks need exactly one performer at a time: sending
queued notifications and evaluating cluster-wide alert conditions. The
performer, the **cluster duty host**, is the lowest-numbered online host in
the quorate partition, as with v2's ingress coordinator; a partition without
quorum has none. Its outputs are idempotent records, so a brief overlap
during a change of performer produces no duplicates. Each duty rereads
membership right before it acts, rather than trusting an earlier answer.

## Jobs and the event model

A job is an instance of an operation from the operation registry. Each
registry entry declares: ID, typed parameters, minimum role, resource locks,
target host rule, whether it can be cancelled in each phase, whether it can
resume, and whether it is a script operation or a native one (see
`WORKFLOW_MIGRATION.md`).

Lock names are hierarchical, with parts separated by `:`: `capacity`,
`cluster-membership`, `production:prod3`, `staging:stage1prod3`,
`host:mox2`, `qdevice`. Two locks conflict when they are equal or one is a
parent of the other, so `production` (taken by a host removal) conflicts
with every `production:prodN`. An entry writes its locks as templates over
its parameters (`production:{resource}`), and a template part marked
optional (`{host?}`) widens the lock to its class when that parameter is
left out. A destructive operation requires an administrator; any other
mutating one requires at least an operator. A read-only one takes no locks.
The browser gets each entry's parameter schema and whether the user may
start it; script paths and command-line flags stay in the engine, which
builds the arguments itself as v2 did: unknown parameters are refused,
positional values can't start with `-`, and `--json` and `--dry-run` are
reserved.

Jobs emit typed events, stored in host-local SQLite and copied to peers:
`started`, `phase`, `plan`, `request` (input form, confirmation, typed-phrase
confirmation, or manual action at a console), `progress`, `output`, `warning`,
`result`, `next_step`, and a terminal `completed`, `failed`, `cancelled`, or
`interrupted`. These match the v2 `bmac-ui` protocol, so script operations map
onto them directly and the v2 Dashboard's run view (steps, plan, requests,
result, next steps, live output) carries over.

A pending request is stored in the job record, so it survives a browser
disconnect and any authorized administrator can answer it from any host. The
answer records who answered. Secret answers go to the job's stdin or memory
and are never stored. Repeating a submitted answer has no effect.

The target host's `bmacd` looks for admitted jobs every 2 seconds, and at
once when one is launched or resumed there. It claims each one and runs a
script operation as `bash SCRIPT --json ARGS` from `/usr/lib/bmac/legacy`, in
its own process group with a fixed environment, under the supervisor copied
from v2. The supervisor turns `bmac-ui` lines into job events, numbering
them from 2 after the runner's `started`. It rewrites the lease record when
the phase or pending request changes, as well as every 15 seconds. It also
rereads the record every 2 seconds for a cancel order placed on another
host. A soft cancel answers a pending request with cancel or sends SIGINT. A
forced cancel sends SIGTERM to the group, then SIGKILL 10 seconds later. An
answer given on another host is checked there, relayed to the host running
the job, and checked again against that host's view of the record before it
reaches the script. A next step naming an operation that isn't in the
registry keeps its text but loses its link.

**Jobs that reboot their own host.** Host setup with boot drills, and
upgrading `bmacd`, restart the host or service that runs them. Such workflows
mark checkpoints; `bmacd` records the checkpoint before the restart and
resumes from it afterwards. When the host has LUKS, the job shows a manual
action ("unlock at the console") until the host comes back.

## Failover independence

These keep working with `bmacd` stopped on every host:
Proxmox HA and replication, `bmac-hook`, HAProxy ingress with its current
generation, keepalived guest egress, nftables guards, and the deferred-cleanup
timer while it remains a script. `bmacd` may observe and manage them; it never
sits in their data path.

## Web Dashboard

The Web Dashboard reproduces the v2 Dashboard's layout and interaction: the
sidebar, page headers with refresh and primary action, cards, the "Needs
attention" panel with suggested next steps, operation tiles, the right-hand
job panel (steps, plan, requests, result), the bottom activity bar ("Under the
hood"), history, and toasts. It starts from a copy of the v2 Dashboard's
React code (`dashboard/src/`), so the look carries over directly rather than
being reproduced.

### Code organization

- `v3/ui/` is a shared React package: the components (from
  `dashboard/src/components/`), the Tailwind theme, icons, formatting helpers,
  and the job view (steps, plan, requests, result, live output). Both apps
  depend on it.
- `v3/webui/` is the Web Dashboard app. Its `Backend` implementation (the
  interface in v2's `dashboard/src/lib/api.ts`) calls the JSON API and
  consumes the event stream. The mock backend stays, for UI development and
  tests without a cluster.
- `v3/deployer/` is the Deployer app. Its `Backend` calls Tauri commands.
- TypeScript types for every API request, response, and event are generated
  from the Rust types (with `ts-rs`) at build time, so the two sides can't
  drift. `v3/dev/check` fails if the generated files are stale.
- The built web app is embedded in the `bmacd` binary, so a host needs no
  separate web assets.
- The web app routes pages in the browser. Moving between pages never
  reloads the document, which matters because a reload ends the login (see
  `SECURITY.md`).

### API

The JSON API lives under `/api/v1/` on the same HTTPS port as the web app.
It is internal to BMAC: versioned, documented, typed, but not promised as a
stable public automation interface. Examples:

```text
POST /api/v1/login                     user, password, second factor -> token
POST /api/v1/logout
GET  /api/v1/cluster                   cluster-wide status snapshot
GET  /api/v1/hosts, /vms, /storage, /backups, /alerts, ...
POST /api/v1/logs/search               stored records from every host, newest first
GET  /api/v1/logs/replication          holdings, peers' copies, retention
GET  /api/v1/operations                operation registry with parameter schemas
GET  /api/v1/settings                  cluster settings, host facts, host keys
PUT  /api/v1/settings/cluster          admins: every value and the revision read
PUT  /api/v1/settings/hosts/{host}     admins: a host's facts, likewise
GET  /api/v1/certificate               this host's certificate and a pending new one
POST /api/v1/certificate/rotate        admins: make a new key and certificate
POST /api/v1/certificate/switch        admins: switch to the pending one, by fingerprint
POST /api/v1/certificate/discard       admins: throw the pending one away
POST /api/v1/jobs                      operation, parameters, idempotency key
GET  /api/v1/jobs                      unfinished and recent jobs, newest first
GET  /api/v1/jobs/{id}                 job record and events from a sequence
POST /api/v1/jobs/{id}/requests/{rid}  answer a pending request
POST /api/v1/jobs/{id}/cancel
POST /api/v1/jobs/{id}/resume          continue an interrupted job
POST /api/v1/jobs/{id}/reconcile       mark an interrupted job failed
GET  /api/v1/events                    Server-Sent Events stream
```

Every request carries `Authorization: Bearer <token>`. Request and response
bodies are validated against the Rust types.

### Live updates

The web app opens one event stream per tab, `GET /api/v1/events`. It reads it
with `fetch` and a streaming body rather than `EventSource`, because
`EventSource` can't send an `Authorization` header and the token must never
go into a URL. The stream carries:

- status changes: new collector observations, condition changes, host up or
  down, quorum;
- job changes: new jobs, state changes, pending requests;
- events of the jobs the tab is watching (the tab says which, with a
  follow-up `POST /api/v1/events/watch`);
- alert changes.

Each message has an ID. After a disconnect the app reconnects with
`Last-Event-ID` and the server replays what was missed, or tells the app to
fetch fresh snapshots if the gap is too old. The server sends a keep-alive
comment every 15 seconds. Snapshots come from the plain `GET` endpoints, and
the stream carries changes after them.

Message IDs are `<engine start>-<sequence>`, so an ID from before the engine
restarted asks for fresh snapshots. The engine keeps the newest 4096
messages (at most 8 MiB) for replay. Publishing never waits for a tab: a tab
more than 1024 messages behind is told to fetch fresh snapshots and carries
on from the present. The first message names the stream, for
`POST /api/v1/events/watch`; the last, when the session ends, is
`session_ended`, and the stream closes.

### Cluster-wide view

Each host's collectors observe only that host. Every `bmacd` keeps a live
host-to-host stream open to every peer over TLS (`SECURITY.md`): it receives
each peer's observation changes and job events as they happen, and holds a
current view of the whole cluster. So the host a browser is connected to can
push changes from any host without asking around, and a job running on host
B appears live in a browser connected to host A. The same streams carry log
copying (`MONITORING.md`). When a peer's stream drops, the host is shown as
unreachable with the age of its last observation.

Each host pulls: it opens `GET /v1/stream` on port 8444 of every other
member, naming the peer's stream and the last sequence it stored. The answer
is newline-delimited JSON: a hello naming the host and its current stream,
the records the asker is missing, read from the store a page at a time, then
new records, job summaries, and observation changes as they happen, with a
keepalive every 15 seconds. A stream silent for 45 seconds is reopened, with
backoff up to 30 seconds while the peer stays away. Received job events and
summaries go to this host's browsers but are never passed on, so nothing
echoes. An answer for a job on another host goes there as `POST /v1/answer`,
and that host checks the role and pending request again before the script
sees it.

### Pages

Overview, Hosts, Production, Staging, Storage, QDevice, Backups, Alerts,
Activity (jobs and logs), Diagnostics, Settings (cluster settings,
notifications, certificates and fingerprints, updates). v2's Config page
becomes Cluster settings, stored in pmxcfs instead of workstation files.

## Upgrades

Releases are signed. `bmacd` pins the release signing key. Updating is a job:
it verifies the packages, then upgrades one host at a time, each host
upgrading itself and resuming after its own restart. Every pmxcfs record
carries a schema version. A host refuses to write a record version newer than
the lowest version every host can read, and the Dashboard shows when hosts
run different versions. `DEPLOYMENT.md` has the detail.

## Repository layout

```text
v3/
  README.md          v3's main readme, including vdev type trade-offs
  design/            these documents
  Cargo.toml         workspace
  crates/
    bmac-core/       shared types: IDs, units, operation registry, events
    bmac-pmxcfs/     pmxcfs access: lock, atomic writes, budgets, records
    bmac-registry/   registry records (v2 schema) and later the native registry
    bmac-storage/    pool layout model and validation (STORAGE.md)
    bmac-exec/       CommandRunner: real and fake process execution
    bmac-adapters/   typed adapters: Proxmox, ZFS, LUKS, network, SMART, ...
    bmac-proto/      API types, host-to-host, engine socket, and appliance protocols
    bmac-inventory/  hardware inventory (discovery OS, hosts, appliances)
    bmacd/           engine, HTTPS front end, and bmac-source, embedding the web app
    bmac-hook/       lifecycle hook (initially packages the v2 script)
    bmac-cli/        the bmac command
    bmac-backupd/    appliance service, embedding its web app
  ui/                shared React components, theme, job view
  webui/             Web Dashboard React app
  applianceui/       appliance React app
  deployer/          Tauri app (copied from dashboard/, cut down)
  legacy/            adapted copies of v2 scripts and their tests
  notify/templates/  Postmark email templates and their manifest (MONITORING.md)
  dev/               v3 test runner and developer tools
  lab/               nested Proxmox test lab
  packaging/         .deb builds, systemd units, signing
```

Nothing outside `v3/` is changed. v2 files are read as reference and copied
into `v3/` where they are reused.

## Testing

- Rust unit tests for every crate; adapters are tested against recorded and
  hand-written command fixtures through the fake `CommandRunner`.
- Adapted v2 Python tests keep running for every script operation still
  active, under `v3/dev/run_tests.py` (a copy of the v2 runner).
- Frontend tests (Vitest) for `v3/ui/`, `v3/webui/`, `v3/applianceui/`, and
  `v3/deployer/`, carried over from the v2 Dashboard's tests.
- A nested lab (`v3/lab/`): three Proxmox VMs with nested virtualization and
  virtual NVMe, SATA, and SCSI disks with serial numbers, one appliance VM,
  and one QDevice VM. It is scripted, can be rebuilt from nothing, and drives
  console LUKS entry over a serial console. Every workflow, failover drill,
  and backup scenario runs there before it runs on hardware.
- Visual comparison of Web Dashboard pages with screenshots of the v2
  Dashboard's mock preview, captured before work starts.
