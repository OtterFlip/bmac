<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v3 implementation plan

How v3 gets built: the milestones, what each work package delivers, which v2
code and tests it draws on, and how we know it is done. The design itself is
in the other documents in this directory; this plan doesn't restate it, and
where they disagree, the design documents win and this plan is corrected.

## How to read this plan

Each work package has the same parts:

- **Builds:** what exists when it is finished.
- **From v2:** the files to read first, and the lessons to carry over. Line
  numbers are as of v2.0.0 and are a starting point, not a contract.
- **Tests:** what proves it, at which level (Rust unit, Vitest, adapted v2
  Python tests, lab scenario, hardware).
- **Done when:** the check that closes the package.

## Ground rules

1. Everything is under `v3/`. Nothing outside it changes. v2 code is copied
   into `v3/` when reused, never moved or edited in place.
2. Every package lands with its tests. `v3/dev/run_tests.py` runs the
   adapted Python tests, `cargo test --workspace` the Rust tests, and
   `pnpm -r test` the frontend tests. All three pass before a package is
   called done. The v2 test rules apply unchanged: per-test temporary
   directories, no `setUpClass`/`setUpModule`, no fixed paths, fakes fed by
   pipes read their stdin, races are fixed rather than serialized.
3. Native code never builds shell command lines (`WORKFLOW_MIGRATION.md`,
   "Typed adapters"). Script operations keep v2's argv-only invocation.
4. A zero exit status is never proof. After every change, check the state
   where v2 does, or where correctness needs it.
5. Fail closed on ambiguity, as v2 does throughout: an unexpected count, an
   unrecognized identity, or a failed query is never treated as "nothing
   there".
6. No secrets in pmxcfs, logs, job events, or process arguments. Tests feed
   known secrets through every workflow and look for them everywhere
   (`SECURITY.md`, "Secrets").
7. Each milestone ends with a lab scenario that exercises it end to end.

## Milestones at a glance

| # | Milestone | Depends on | Outcome |
| --- | --- | --- | --- |
| M0 | Groundwork | — | Workspace, copied code, test runners, lab skeleton, v2 screenshots |
| M1 | Core libraries | M0 | Types, process runner, pmxcfs access, registry reader, protocols, first adapters, storage model |
| M2 | `bmacd` platform | M1 | Engine and HTTPS front end, login, jobs, events, host-to-host streams, CLI |
| M3 | Web Dashboard | M2 | The v2 Dashboard as a web app, logged in, live |
| M4 | Script operations | M2 | v2 workflows running under `bmacd` on hosts |
| M5 | Monitoring and alerting | M2 | Collectors, conditions, alerts, Postmark, log store and copying |
| M6 | Deployer | M1 (M6.10 also M9.3) | Discovery, ISOs, boot steps, handoff, workstation SSH |
| M7 | Host bootstrap and setup | M2, M4, M6.1–M6.9, M8.1, M8.2 | First boot, setup mode, joins, the host setup job |
| M8 | Storage | M1, M2 | Layout model in use, LUKS, native storage workflows, block-size storages |
| M9 | Backups | M2, M5, M8 | Cluster source, appliance, retention, restore |
| M10 | Packaging, upgrades, releases | M2 | Signed packages, cluster update job |
| M11 | Native ports | M4 | Every script operation replaced by Rust, through the port gate |
| M12 | Release readiness | all | Hardware runs, documentation, v3.0 |

M3, M4, M5, M6, and M8 can proceed in parallel once their dependencies are
in. The first **walking skeleton** is the end of M7's first pass: a lab
cluster installed from Deployer ISOs, set up through the Dashboard, running
v2's workflows as script operations.

```text
M0 -> M1 -+-> M2 -+-> M3
          |       +-> M4 -------------------+-> M11
          |       +-> M5 ------------+      |
          |       +-> M10            |      |
          |       +-> M8.1, M8.2 ----|------+-> M7 (walking skeleton)
          +-> M6.1–M6.9 -------------|------+
          +-> M8a (model) -> M8 -----+-> M9 -> M6.10 -> M12
```

M8a is M1.8, the storage model; M8's write adapters and journal (M8.1,
M8.2) need M2 as well. M6.10, the appliance ISO, waits for M9.3's
appliance daemon. M12 follows everything.

## Decisions made while planning

Reading v2 closely turned up points where the design documents assumed
something v2 doesn't do, or where v2 behavior has to move. All of these are
agreed and applied to the design documents:

1. **`deploy_hello_app_to_prod` moves to the Deployer.** It runs `ssh prodN`
   from the workstation, uploads files, and takes a pasted Cloudflare
   Origin CA certificate and key (`deploy_hello_app_to_prod.sh`). It changes
   nothing in Proxmox. Like jump SSH, it is a workstation operation, so it
   becomes a Deployer feature that uses the workstation's jump SSH entry,
   rather than a script operation on a host.
2. **Guest access from hosts goes through the guest agent.** v2 reaches into
   guests over the workstation's jump SSH in three places: `fstrim` before
   decommissioning (`decommission_disks.sh` 249–283), the in-guest
   `growpart`/`resize2fs` after growing a disk (`extend_prod_vm_disk.sh`),
   and the final SSH login check after creating a VM (`add_prod_vm.sh`
   2307–2484). On a host, the root key isn't authorized in guests, and it
   shouldn't be. These become `qm guest exec` calls through the QEMU guest
   agent, which v2 already relies on for host key attestation. The SSH
   login check moves to the Deployer's jump SSH setup, which proves it.
3. **Two leader roles stay distinct.** v2 has a **control node**
   (`cluster-control.json`, the host new members join through, guarded by a
   workstation lock plus a remote lease, `host_membership.sh` 226–282) and a
   **coordinator** (the lowest-numbered online quorate host, which runs
   ingress sync, `sync_haproxy_routes.sh` 213, 1152). v3's "cluster duty
   host" is the coordinator. The control node's job, serializing
   membership changes, becomes a `cluster-membership` resource lock in
   v3's job admission, taken by joins, removals, and QDevice changes, so the
   workstation lock is no longer needed. Joins go through the host named in
   the bundle, which must hold that lock.
4. **v2 sends no alert email.** The v2 README says failure and recovery
   emails are sent, but no v2 code sends any; `MAIN_DESIGN.md` ("Deliberate
   boundaries") lists monitoring and alerting as out of scope. v3's alerting
   is all new work, with no v2 templates to carry over.
5. **Disk identity in v2 is serial only.** WWN is collected but never used
   for selection (`rpool_mirror.sh` 158–171). v3 uses WWN when present,
   otherwise serial (`STORAGE.md`). Existing v2 behavior, re-resolving the
   identity and requiring exactly one match before every destructive step,
   is kept for both.
6. **The LUKS passphrase during setup.** v2 sends it from `secrets.env` over
   SSH stdin into a root-only file (`add_proxmox_host.sh` 2222–2247); after
   setup, adding disks prompts at the console. v3 uses the console for both
   (`bmac luks-enter`), as agreed. The temporary file's handling (root-only,
   not a symlink, removed and verified gone) is v2's.
7. **The operator boots every ISO, as in v2.** v2 reads iDRAC inventory
   through Redfish and leaves mounting media and power to the operator
   (`add_proxmox_host.sh` 984–996, 1575–1585). v3 drops the iDRAC
   inventory and keeps the rest: the Deployer gives the steps (for the
   iDRAC's Virtual Console, another remote console, or USB) and waits. An
   earlier version of this plan had the Deployer mount media and drive
   power through Redfish; the lab's R640 showed that the iDRAC can't fetch
   an ISO from a workstation behind the provider's VPN (M6.5), so that was
   removed. v2's typed detach confirmation before shredding the ISO is
   kept.

Two more were settled after the design review:

8. **No CI for v3 yet.** Everything stays in `v3/`, and GitHub reads
   workflows only from the repository root, so v3 has no CI workflow.
   `v3/dev/check` is run by hand and is the gate wherever this plan says a
   check must pass.
9. **Old `rpool` labels are cleared by the host setup job.** The discovery
   OS never writes; it reports the labels and the Deployer warns. The setup
   job clears them on approved non-boot disks after re-identifying each
   disk and asking the operator (M7.3).

---

## M0. Groundwork

### M0.1 Repository skeleton

**Builds:** the layout in `ARCHITECTURE.md` ("Repository layout"): a Cargo
workspace `v3/Cargo.toml` with empty crates, a pnpm workspace
`v3/pnpm-workspace.yaml` for `ui`, `webui`, `applianceui`, `deployer`, the
`v3/legacy/`, `v3/dev/`, `v3/lab/`, and `v3/packaging/` directories, and
license headers on every file.

**From v2:**

- `dev/VERSION` and `dev/set_version.py` (keeps Cargo, `package.json`, and
  `Cargo.lock` versions in sync, 8–38). Copy to `v3/dev/` and extend it to
  every v3 crate and package.
- `dashboard/scripts/check-toolchain.mjs`: the toolchain check (Node, pnpm,
  Rust, WebKit). Copy, and split into a Deployer check and a `bmacd` build
  check.
- `dashboard/.nvmrc`, `dashboard/tsconfig.*.json`, `vite.config.ts`.

**Tests:** `cargo test --workspace` and `pnpm -r test` run empty and pass;
`v3/dev/test_set_version.py` (copied from `dev/test_set_version.py`).

**Done when:** a clean clone builds every crate and package with one
command, and `set_version.py` updates every version.

### M0.2 Test runner

**Builds:** `v3/dev/run_tests.py`, a copy of `dev/run_tests.py`, discovering
tests under `v3/legacy/tests/` and `v3/dev/`, plus a wrapper `v3/dev/check`
that runs the Python suite, `cargo test --workspace`, `cargo clippy`, the
ts-rs staleness check (M1.6), and `pnpm -r test` in one go.

**From v2:** `dev/run_tests.py` (one process per test, one worker per CPU,
stdin from `/dev/null`, `-j`, `-k`, `-f`, `-v`); `AGENTS.md` and
`docs/MAIN_DESIGN.md` "Tests" for the rules. Add a `v3/AGENTS.md` pointing
at the new runner. v3 development is Linux-only (amd64 with KVM), so v2's
Mac test VM (`dev/run-tests-in-vm.sh`, `dev/lima-bmac.yaml`) is not
carried over.

**Done when:** `v3/dev/check` passes on a clean tree.

### M0.3 Baseline screenshots of the v2 Dashboard

**Builds:** screenshots of every v2 Dashboard page, each run state, dialog,
and request kind, captured from the v2 mock preview (`dashboard/src/lib/mock/`, served by
`pnpm web:dev`) by `v3/dev/visual-baseline/capture.mjs`, stored under
`v3/dev/visual-baseline/v2/`. These are the reference for M3's visual
comparison.

**From v2:** `dashboard/src/lib/mock/backend.ts` and `fixtures.ts` (replays
every protocol event); `docs/DASHBOARD_SCREENSHOTS.md` and `media/*.png`.

**Done when:** every page in `dashboard/src/pages/` and every request kind
in `components/workflow/Requests.tsx` has a baseline image.

### M0.4 Lab skeleton

**Builds:** `v3/lab/` scripts that create, snapshot, reset, and destroy the
nested lab (`WORKFLOW_MIGRATION.md`, "The lab") on a Linux machine with
nested virtualization: three Proxmox VMs with virtual NVMe, SATA, and SCSI
disks carrying distinct serials and WWNs, a private bridge standing in for
the 10 Gbps VLAN (with a configurable carrier delay, to reproduce
FiberState's slow link), one appliance VM, one QDevice VM, and serial
consoles driven by scripts. A scenario runner gives pass/fail output.

**From v2:** `docs/MAIN_DESIGN.md` "Names, addresses, VMIDs, MACs" (address
plan), `config/cluster_dot_conf` and `config/mox1_dot_conf` (example
values), `docs/QDEVICE_MANUAL_SETUP.md`.

**Tests:** a smoke scenario: create, boot a plain Proxmox ISO on each VM,
reach each over the management network, reset.

**Done when:** the lab can be rebuilt from nothing by one command, and the
slow-carrier bridge reproduces a link that comes up after a set delay.

---

## M1. Core libraries

### M1.1 `bmac-core`: shared types

**Builds:** typed identifiers and units used everywhere: `HostSlot` (1–10,
`moxN`), `Vmid`, `GuestName` (`prodN`, `stageNprodN`), `PoolName`,
`DatasetName`, `SnapshotGuid`, `Bytes`, `JobId`, `OperationId`,
`DiskIdentity` (WWN or serial), and the job event types (`ARCHITECTURE.md`,
"Jobs and the event model").

**From v2:** the validation regexes scattered through the scripts (for
example `^mox([1-9]|10)$` in `setup_jump_ssh_access.sh` 125,
`^[1-9][0-9]*$` for VMIDs, safe identifiers in `protocol.rs` 15–18), and
`MAIN_DESIGN.md` "Names, addresses, VMIDs, MACs" for the naming rules.

**Tests:** parse and display round trips; every v2 regex's accept and
reject cases.

### M1.2 `bmac-exec`: the process runner

**Builds:** `CommandRunner` as specified in `WORKFLOW_MIGRATION.md`: literal
program path, argv, environment allowlist, working directory, stdin,
timeout, cancellation by process group, captured-output byte limits with
streaming, expected exit codes, and redaction. A real implementation and a
fake that matches expected calls and returns recorded results.

**From v2:**

- `dashboard/engine/src/supervisor.rs`: own process group
  (`process_group(0)`, 215), signalling the whole group (99–105),
  `kill_on_drop(false)` so dropping a handle doesn't kill a child (216),
  SIGTERM then SIGKILL after 10 seconds (37, 726–737), clearing inherited
  `BMAC_UI_*` variables before spawning (208–211).
- `scripts/user_callable/diagnostics/show_cluster_health.sh` 86–99 and
  `quick_state.py` 32–46: every external call has a timeout, and a timeout
  is reported as such (exit 124), not as a hang or as empty output.
- `LC_ALL=C` for every parsed command, as v2's collectors do.
- `scripts/lib/apt_lock_wait.sh`: waiting for the apt lock through
  `/proc/locks`; becomes a runner helper for any apt call.

**Tests:** process-group cancellation kills grandchildren; timeouts; output
limits; redaction of registered secrets; the fake rejects unexpected calls
and reports unused expectations.

### M1.3 `bmac-pmxcfs`: lock and atomic records

**Builds:** the pmxcfs access layer: the registry lock, atomic record writes,
size and budget checks, schema versions, and revision checks.

**From v2** (`scripts/host_runtime/cluster_registry.py`):

- **Lock** (`AllocatorLock`, 839–978): an empty directory created with
  `mkdir` under `/etc/pve/priv/lock/`, named from the first 16 hex digits of
  the SHA-256 of the pmxcfs-relative path. The lock directory must stay
  empty (pmxcfs forbids non-empty lock directories), so owner diagnostics go
  in a sibling file. Default wait 15 seconds. Age alone never breaks a lock;
  an explicit break on pmxcfs sets the directory's times to zero, which
  pmxcfs treats as a cluster-wide unlock request (880–896), never `rmdir`.
  Release re-reads the owner nonce and refuses if ownership changed
  (964–978).
- **Atomic writes** (`atomic_write_json`, 804–836): canonical JSON with
  sorted keys and a trailing newline, size checked against 1 MiB, a sibling
  temporary file opened with `O_NOFOLLOW`, then `rename`. Under pmxcfs, skip
  `fsync` and `chmod`, which pmxcfs doesn't support (`is_pmxcfs_path`,
  729–742). Reject symlinks on every path component (760–767).
- **Secret rejection** (`reject_secret_material`, 303–315): refuse any
  record whose keys or values look like secrets.
- The 16 MiB total budget and 8 MiB warning are new (`ARCHITECTURE.md`,
  "Where state lives").

**Tests:** port the lock and write tests from `scripts/tests/test_shared_libs.py`:
`test_atomic_lock_times_out_and_can_explicitly_reap_stale_lock`,
`test_simulated_pmxcfs_uses_supported_atomic_file_and_lock_operations`,
`test_simulated_pmxcfs_stale_break_uses_safe_unlock_request`. Add budget
enforcement and concurrent-writer tests (many threads contending for the
lock in a temporary directory).

**Done when:** the Rust lock interoperates with v2's Python lock on the same
directory in both directions (a test runs both), because script operations
will use the Python one until the registry is ported.

### M1.4 `bmac-registry`: reading v2 records

**Builds:** typed, validated readers for every v2 registry record, at the
new path `/etc/pve/priv/bmac/registry/`: policy, resources, orchestration
records with leases, deferred cleanup, history, ingress (desired, transition,
generations, node status), host slots, and cluster control.

**From v2** (`cluster_registry.py`): exact key sets per record
(`RESOURCE_KEYS` 204–229, `LEASE_KEYS` 125, `ORCHESTRATION_KEYS`,
`INGRESS_*_KEYS` 46–85, `HOST_SLOT_KEYS` 126–135, `CLUSTER_CONTROL_KEYS`
136–143), state sets and transitions (`RESOURCE_STATES` 145–158,
`STATE_TRANSITIONS` 160–192, `INSTALL_PHASES` 88–105), limits
(`MAX_JSON_BYTES`, `MAX_ROUTES` 4096, `MAX_HISTORY_RECORDS` 256, cleanup
bounds, 39–44), and the orchestration contract rule that a partial contract
is rejected (`validate_orchestration_record`, 647–714). Unknown keys are
errors, as in v2.

**Tests:** generate records with v2's `cluster_registry.py` in a temporary
state directory (every subcommand in `build_parser`, 4734–4768) and read
them with the Rust reader; mutate them to each invalid form and check
rejection. Writing stays with the Python CLI until M11.

### M1.5 `bmac-proto`: protocol types

**Builds:** the types for the JSON API and event stream, the front end to
engine socket, host-to-host streams, the bmac-source and appliance protocol
(`BACKUP.md`, "Protocol"), and the v2 `bmac-ui` protocol used by script
operations.

**From v2:** `dashboard/engine/src/protocol.rs` (copy it; message types
171–331, limits 15–18 and 395–446, password fields always sensitive with
defaults stripped 403–411 and 639–646, response building and redaction
459–517) and `dashboard/protocol/bmac-ui-v2.schema.json`.

**Tests:** copy the protocol tests; check every schema example parses; check
the limits.

### M1.6 TypeScript generation

**Builds:** `ts-rs` derives on every API and event type, generated into
`v3/ui/src/generated/`, and a check in `v3/dev/check` that fails when the
generated files are stale.

**From v2:** `dashboard/src/protocol/types.ts` and `state.ts`, the
hand-written TypeScript equivalents; compare field by field when replacing
them, so nothing the UI uses is lost.

### M1.7 First adapters (read-only)

**Builds:** in `bmac-adapters`, the read-only side of the adapters needed by
collectors and the platform: `ProxmoxCluster` (`pvesh get /nodes`,
`/cluster/status`, `pvecm status`, `corosync-cfgtool -n`,
`corosync-quorumtool`), `ProxmoxGuests` (`/cluster/resources`, `qm config`),
`ProxmoxHa` (`ha-manager status`, `rules config`), `Replication`
(`/nodes/{n}/replication`), `Zpool` (`status -P -p`, `list -v -Hp`), `Zfs`
(`list`/`get -Hp`), `Disks` (`lsblk -J`, `smartctl -H`/`-j`), `Systemd`,
`Tailscale` (`status --json`, `ip -4`).

**From v2:**

- `scripts/utilities/host_storage.py`: the `zpool status` parser, including
  log, cache, spare, and special sections it already recognizes (55,
  116–118), resilver and removal progress, ESP matching, and the attention
  rules (753–808). Port its parsing and its fixtures.
- `scripts/utilities/quick_state.py`: the collection set per page (98–207)
  and the rule that errors are appended and collection continues.
- `show_cluster_health.sh`: the SMART interpretation (PASSED or OK; bit 8
  means failing; exit 127, meaning `smartctl` is missing, is skipped rather
  than raised, 807–828), corosync link parsing (427–527), and QDevice
  health (`qd_status_is_healthy` in `scripts/lib/qdevice.sh` 73–95).

**Tests:** recorded command outputs from v2's test fixtures
(`test_host_storage.py`, `test_show_cluster_health.py`,
`test_quick_state.py`) become adapter fixtures. Add raidz, special, log,
cache, and spare `zpool status` fixtures captured in the lab.

**As built:** `lab/lab scenario m2_cli` builds a raidz2 pool with a dedup,
special, log, cache, and spare vdev on loop devices, healthy and with a
member faulted, and with `BMAC_LAB_CAPTURE_FIXTURES=1` writes its
`zpool status` and `zpool list` outputs to
`crates/bmac-adapters/tests/fixtures/lab/`; both parse as v2 parses them.
They showed that `zpool list -v -H` pads the class headings with spaces,
not tabs, and lists a hot spare in use twice (the parser keeps the second,
`INUSE` row, as v2 does; the sizes match).

### M1.8 `bmac-storage`: layout model and capacity (M8a)

**Builds:** the layout model, validation, and capacity function from
`STORAGE.md`, with no I/O, so the Deployer, `bmacd`, and the appliance share
it from the start.

- Layout types, classes, disk identity, JSON schema version 1.
- Every refusal and warning in `STORAGE.md` ("Checks"), with the wording the
  UI shows, and the per-type restrictions that are shown before a type is
  picked (shared with `v3/README.md`, "Choosing vdev types").
- `vdev_raidz_asize`, `E(B)`, `I(b)`, metadata allowance `m(b)`, mixed-pool
  worst case, layout summary figures, and the suggested block size.
- Partition arithmetic: member end at the smaller disk's whole-GiB size
  minus 1 GiB, replacement size from the survivor's layout end plus the GPT
  tail, installer `hdsize` leaving 4 GiB.

**From v2:**

- `scripts/host_runtime/rpool_mirror.sh` constants and arithmetic (50–55,
  `pair_end_bytes` 350–354, `replacement_bytes` 363–367, capacity match within 1%
  493–494, expected-size check 468–470).
- `add_proxmox_host.sh` 1144–1150 (`hdsize`).
- `extend_prod_vm_disk.sh` 24–25 and 310–355 (10% reserve, reserved and
  sparse growth formulas, 1.5% sparse overhead) and
  `change_prod_vm_placement.sh` 236–239 (placement reserve), which become
  inputs to the capacity function rather than separate arithmetic.

**Tests:** the `STORAGE.md` example table, computed and asserted exactly; the
v2 arithmetic tests (`test_pair_partitions_end_at_the_smaller_disk_in_either_order`,
`test_replacements_only_need_room_for_the_survivors_partitions`, the
`test_extend_prod_vm_disk.py` math tests) ported; property tests that
efficiency never exceeds the ideal ratio and inflation is never below 1.
Lab measurements are added in M8.

---

## M2. `bmacd` platform

### M2.1 Two processes and the engine socket

**Builds:** one `bmacd` binary that starts as the **engine** (root) or the
**front end** (`bmac-web`), two systemd units (`bmacd.service`,
`bmacd-web.service`, bound together), and the Unix socket
`/run/bmac/engine.sock` checked with `SO_PEERCRED`. The front end's unit is
sandboxed (`ProtectSystem=strict`, no capabilities, no access to
`/etc/pve`), gets the HTTPS key through `LoadCredential`, and can only send
the fixed set of typed requests in `bmac-proto`.

**From v2:** nothing directly; v2 runs as the desktop user. The pattern is
Proxmox's own `pveproxy`/`pvedaemon` split.

**Tests:** the socket rejects other UIDs; the front end's unit has no
access to `/etc/pve/priv` or the key file (checked in the lab with
`systemd-analyze security` and by trying); every request type round-trips.

### M2.2 HTTPS listener

**Builds:** the front end's TLS listener on 8443, on the Tailscale and
private addresses only; TLS 1.3 and 1.2 with modern ciphers; the response
headers in `SECURITY.md` ("Requests"); serving the embedded web app.

**From v2:** the public-port guard installed at first boot
(`add_proxmox_host.sh` 1335–1365) drops TCP 22, 3128, and 8006 on the public
address. **v3 adds 8443, 8444, and 8445 to it**, and the lab checks from
outside that none of them answers on the public address.

**Tests:** header assertions; listener addresses; a lab check from the
"internet" side of the lab network.

### M2.3 Login, roles, sessions

**Builds:** `POST /api/v1/login` and logout, passing username, password, and
the second factor through Proxmox's `access/ticket` on `127.0.0.1:8006`
(including its TFA challenge round trip); roles from `bmac-admin`,
`bmac-operator`, `bmac-viewer` and `root@pam`; the opaque 256-bit token;
the session table in SQLite (hash only); idle and absolute expiry; the
five-minute group re-check; "log out everywhere"; rate limiting per address
and per user; audit events.

**From v2:** none (new). WebAuthn through BMAC's origin is a lab question;
until it passes, the login page offers TOTP and recovery keys and says so.

**Tests:** a fake Proxmox ticket endpoint covering success, TOTP challenge,
recovery key, bad password, disabled user, user without a BMAC group;
expiry; revocation closing open event streams; rate limits. Lab: real
Proxmox users with TOTP.

### M2.4 API and event stream

**Builds:** the axum router for `/api/v1/` (`ARCHITECTURE.md`, "API"), body
validation against the Rust types, the Origin check, `Cache-Control:
no-store`, and `GET /api/v1/events` with message IDs, `Last-Event-ID`
replay, the "snapshot too old" signal, 15-second keep-alives, and
`POST /api/v1/events/watch` for job events.

**From v2:** `dashboard/src-tauri/src/lib.rs` (471–498) lists the commands
the v2 UI calls and the three events it listens to (`bmac://run-events`,
`bmac://run-updated`, `bmac://close-requested`, 32–34). Each maps to an API
endpoint or event-stream message, except the workstation-only ones
(repository, file browsing, config relocation, opening external programs,
quit), which have no web equivalent.

**Tests:** replay after reconnect; a slow client doesn't block others
(bounded per-client queues, dropped clients told to resnapshot); every
endpoint rejects a missing or expired token; the Origin check.

### M2.5 Operation registry

**Builds:** the v3 operation registry, generated from Rust declarations
(native) and a `script-operations.json` (script), each entry declaring ID,
title, category, typed parameters, minimum role, resource locks, target host
rule, cancellability per phase, resumability, destructive flag, and kind.
`GET /api/v1/operations` serves it with parameter schemas.

**From v2:** `dashboard/engine/workflows.json` and `registry.rs`: parameter
types (`flag`, `flag_choice`, `choice`, `host`, `production`, `staging`,
`guest`, `integer`, `path`, `hostname`), the `suggest` mechanism, `mode`,
`destructive`, `concurrent`, `supports_dry_run`, and `build_invocation`
(233–302: the frontend never supplies a path or argv, unknown parameters are
rejected, positionals can't start with `-`, `--json` is reserved). v2's
`platforms`/`requires` fields described the workstation; they become the
target host rule. Default `dry_run: true` where v2 has it.

**Tests:** port `dashboard/test_workflow_registry.py` (unique IDs matching
script names, scripts exist, are executable, call `bmac_ui_bootstrap`, every
flag appears in the script's `case`, destructive implies mutating, `bash -n`)
to run against `v3/legacy/` and `script-operations.json`.

### M2.6 Jobs: admission, leases, conflicts, interruption

**Builds:** job records in `/etc/pve/priv/bmac/jobs/`, admission under the
registry lock (quorum, role, resource conflicts, idempotency key), claiming
by the target host with a lease, renewal, `INTERRUPTED` on lease expiry,
operator resume, the reconciliation step that marks an interrupted job
failed after checking live state (`ARCHITECTURE.md`, "Jobs"), cancellation,
and the bounded record set (active plus the newest 500 finished).

**From v2:**

- Orchestration leases (`cluster_registry.py` 2884–3054): 32-hex nonce,
  owner, TTL, renewal only by the same nonce, refusal while another nonce is
  unexpired, the contract fields immutable on resume, no forced takeover.
  Job leases follow the same rules.
- v2's concurrency rule (`supervisor.rs` 186–191): one mutating run at a
  time, except `add_proxmox_host` alongside itself. v3 replaces "one at a
  time" with per-resource locks, and adds `cluster-membership` (decision 3)
  for joins, removals, and QDevice changes.
- `history.rs` 258–271: a run whose supervisor died is marked interrupted
  with "inspect before retrying", never retried automatically. Same in v3.
- Cancellation (`supervisor.rs` 710–749): a soft cancel answers a pending
  request with a cancel, otherwise sends SIGINT to the process group; it is
  refused while `cancel_allowed` is false and no request is pending; a forced
  cancel sends SIGTERM, then SIGKILL after 10 seconds.

**Tests:** concurrent admission from several simulated hosts against one
temporary pmxcfs directory; idempotent repeat POST; lease expiry and
interruption; conflicts; cancellation rules; quorum loss refusing admission.

### M2.7 Script supervisor

**Builds:** the supervisor for script operations, copied from
`dashboard/engine/src/supervisor.rs` and adapted to run on the target host
under the engine, mapping `bmac-ui` events onto v3 job events, storing
pending requests in the job record, and accepting answers relayed from any
host.

**From v2:** keep every behavior the v2 engine tests protect
(`dashboard/engine/tests/supervisor.rs`), including:

- the protocol header must come first, or the run is killed (422–439);
- events are framed with a per-run marker token, so a script printing JSON
  can't forge events (`scripts/lib/ui_protocol.py` 26, 127–144);
- the filters ignore SIGINT, SIGTERM, and SIGHUP so `completed` still
  arrives (`ui_protocol.py` 118–122); script stdin is `/dev/null` and
  answers use `BMAC_UI_INPUT_FD` (`ui_json_run.sh` 95);
- secrets of three characters or more are remembered and scrubbed from every
  later event (`supervisor.rs` 108–128, 667–674);
- a run without `completed` is a failure; `completed: success` with a
  non-zero exit is a failure (603–621);
- a declined confirmation is a cancellation, not a failure; a `--question`
  is not (`ui_protocol.sh` 595–628, `ui_json_run.sh` 117–119);
- new requests are cancelled automatically once a cancel is pending
  (370–406);
- `result` at most 8 MiB; at most 50 `next_step`s; next steps naming unknown
  operations are dropped (524–541);
- output flushed every 80 ms or 256 events; unparsed stdout becomes one
  warning plus `log` lines (347–361, 568–579);
- `TERM=dumb` and symlinked scripts refused (`ui_json_run.sh` 33–36, 78).

**Tests:** copy `dashboard/engine/tests/supervisor.rs` and
`dashboard/mock-scripts/*`; copy `scripts/tests/test_ui_protocol.py`,
`test_ui_json_forms.py`, and `ui_test_driver.py` into `v3/legacy/tests/`.
Add: an answer submitted on host A reaches a script on host B; a second
answer to the same request has no effect.

### M2.8 Event and log store

**Builds:** `/var/lib/bmac/bmac.db` (SQLite, WAL) with job events, output in
chunks of at most 64 KiB plus compressed segments, audit events, `bmacd`'s
own log, and the `(origin, stream, sequence)` keys from `MONITORING.md`.
Retention in M5.

**From v2:** `history.rs` (per-run JSON written atomically, `0700`
directory, 300 runs kept) and the live-output limits (100,000 events in the
live ring, 50,000 returned, 120,000 in the UI console buffer). v3 streams
output instead of capping it in memory, and pages the UI through it.

**Tests:** large outputs never loaded whole; redaction before storage; crash
during write leaves a readable database.

### M2.9 Host-to-host streams

**Builds:** the TLS listener on 8444 using the Proxmox node certificate and
`pve-root-ca.pem`, the peer allowlist from current cluster membership, and
the live stream carrying observation changes, job events, request answers
for jobs running on the peer, and log records (M5). The same listener takes
`POST /v1/join` and `POST /v1/enroll` from clients without a member
certificate (`SECURITY.md`, "Joins and enrollment"): token hash and expiry
checked in pmxcfs, single use, rate-limited, producing only a pending
request; every other request from such a client is refused.

**Tests:** a peer outside the membership is refused; a client without a
member certificate gets only join and enroll, and an expired, reused, or
unknown token is refused; a dropped stream
resumes from the last acknowledged sequence; a three-host in-process test
harness (three engines on loopback with temporary state) used by later
milestones.

### M2.10 Duty host and schedules

**Builds:** the cluster duty host (lowest-numbered online host in the quorate
partition) and the schedule evaluator with idempotency keys from schedule ID
and slot time.

**From v2:** `sync_haproxy_routes.sh` `first_online_node_from_json` (213)
and the coordinator assertion before acting (1152); a non-coordinator that
sees a current generation doesn't act (1251).

**Tests:** the duty host changes when the lowest host goes offline; two
hosts evaluating the same schedule produce one job.

### M2.11 Cluster settings

**Builds:** typed cluster settings and host facts in
`/etc/pve/priv/bmac/config/` covering every key of v2's `cluster.conf` and
`moxN.conf`, edited with revision checks, and the renderer that writes them
into `/run/bmac/config/` in exactly the format v2's `config.sh` parses, for
script operations.

**From v2:** `config/cluster_dot_conf`, `config/mox1_dot_conf`,
`scripts/lib/config.sh` (`load_proxmox_config` and its validation), and
`dashboard/engine/src/config.rs` (revision compare-and-swap, `0600` writes,
secrets never sent to the UI, 8–11).

**Tests:** for a range of settings, render the files and load them with v2's
`config.sh` in a test shell; every variable comes back identical. A setting
v2 validates and rejects is rejected by the typed layer too.

### M2.12 The `bmac` CLI

**Builds:** `bmac status`, `bmac jobs`, `bmac run <operation>` (with
`--terminal` for script operations), `bmac luks-enter`, and the read-only
commands the Deployer uses: `bmac guests --json` and
`bmac guest-host-key <guest> --json`, and `bmac capacity --json` (the
capacity function of M1.8, for script operations, M4.3). The read-only
commands read pmxcfs and
Proxmox directly and work with `bmacd` stopped; the others go through the
engine socket with the same checks as the web UI.

**From v2:** `setup_jump_ssh_access.sh` `guest_rows` (230–260) and
`attest_guest_host_key` (354–382): the guest list joins registry records
with live VM state, and the host key is read with `qm guest exec ... cat
/etc/ssh/ssh_host_ed25519_key.pub` on the node running the guest, checking
`exited == 1`, `exitcode == 0`, and the Ed25519 format.

**Tests:** fixtures from `test_setup_jump_ssh_access.py`; `bmac luks-enter`
writes the root-only file and refuses symlinks.

**As built (lab):** `m7_storage` types the passphrase with `bmac luks-enter`
at the console. `lab/lab scenario m2_cli`, on the three-host cluster, checks
`bmac status`, `bmac guests`, and `bmac capacity` on rpool and on a raidz2
of four, where a filled sparse zvol of 256 MiB at 16 KiB blocks took
294,060,032 bytes against the capacity function's 297,414,284. `bmac
guest-host-key` needs a guest with its agent, so M4's end-to-end run
checks it on the production VM it creates.

### M2.13 Interim lab bootstrap

**Builds:** `lab/lab bootstrap`, a development-only command that makes the
lab's installed hosts a cluster running a development build of `bmacd`
(so M2 and M4 don't wait for M6 and M7). v2's `add_proxmox_host.sh` itself
can't run in the lab: it requires a connected Tailscale client and a
Tailscale auth key per host, and the lab never joins the real tailnet before
M6. So the bootstrap does, from the workstation over SSH, what that script
does that `bmacd` relies on, the way v2 does it:

- host names on the private VLAN, each host's replication address, and
  `pvecm create` and `pvecm add` with the VLAN as link0 and the management
  network (the lab's stand-in for the tailnet) as link1, then the migration
  network in `datacenter.cfg`;
- the registry initialized by v2's own `cluster_registry.py` directly in
  `/etc/pve/priv/bmac/registry`;
- `bmacd` and `bmac` built on the first host (which runs the hosts' Debian)
  with the `lab` feature, which adds the mock operations `lab.interactive`
  and `lab.long`, from crates vendored out of the workstation's cargo cache;
  installed on every host with the units, the legacy
  runtime, the mock scripts, and a self-signed HTTPS certificate;
- v2-style `cluster.conf` and `moxN.conf` for the lab, loaded with the
  lab-only `bmacd import-settings`;
- Proxmox users in the BMAC groups: alice (administrator, with TOTP and
  recovery keys), bob (operator), and vera (viewer).

Every step can be rerun. It is never shipped; v3 has no upgrade path from
v2. The `m2` scenario checks the milestone's done-when below. The web app
arrives in M3, so the scenario stands in for the browser: it calls host C's
HTTPS API the way the browser does, through an SSH tunnel to the host's
management address, with the host's certificate pinned and the session
token held only in memory.

**M2 done when:** three lab hosts (set up by the interim bootstrap in M2.13)
run `bmacd`; a user logs in on each with TOTP; a mock operation started on
host A runs on host B, its live output and a request appear in a browser
connected to host C, and the answer given there completes it.

---

## M3. Web Dashboard

### M3.1 The shared UI package

**Builds:** `v3/ui/`, extracted from copies of `dashboard/src/components/`
(layout, workflow, console, status, ui primitives), `dashboard/src/lib/`
(`format.ts`, `params.ts`, `utils.ts`), `dashboard/src/index.css`, and the
Tailwind theme. The job view (steps, plan, requests, result, next steps,
live output) is one component used by both apps.

**From v2:** `components/workflow/Requests.tsx` (typed-phrase confirmation
stays typed, 215–216; `manual_action` with copyable commands and `run_on`;
request context from the last ~400 console lines, 25–47),
`components/console/Terminal.tsx` (xterm, logs kept outside React state per
`state/logs.ts` 1–4), `WorkflowPanel.tsx`, `LaunchDialog.tsx`,
`FieldControl.tsx`, `RunParts.tsx`, `RunSwitcher.tsx`, `Sidebar.tsx`,
`Page.tsx`, `Toaster.tsx`.

**Tests:** copy the v2 Vitest tests that cover these components.

### M3.2 The web `Backend`

**Builds:** `v3/webui/src/lib/backend.ts` implementing v2's `Backend`
interface (`dashboard/src/lib/api.ts` 22–56) over the JSON API and the event
stream, with the token held in that module only, the stream read with
`fetch`, reconnect with `Last-Event-ID`, and downloads as `Blob`s. The mock
backend (`dashboard/src/lib/mock/`) is kept for development and tests.

**From v2:** `state/store.ts`: the store shape, bootstrapping sources from
the last results (202–219), refreshing all sources after a mutating success
(411–413), parameterless read-only launches skipping the dialog (362–368).
In v3, sources become API snapshots plus pushed changes, so the refresh
interval setting goes away.

**Tests:** store tests with a fake event stream: reconnect and replay,
"snapshot too old", token expiry returning to the login page.

### M3.3 Login and session behavior

**Builds:** the login page (user, password, TOTP or recovery key), the
first-visit fingerprint guidance with browser-specific steps, logout,
expiry handling, the warning before closing a tab while a request is
pending, and in-app routing that never reloads the page.

**Tests:** no cookie or storage API is ever touched (a test wraps
`document.cookie`, `localStorage`, `sessionStorage`, and `indexedDB` and
fails on access); the token never appears in a URL; a reload returns to
login.

### M3.4 Pages

**Builds:** Overview, Hosts, Production, Staging, Storage, QDevice, Backups,
Alerts, Activity, Logs, Diagnostics, Settings, and setup mode. Beyond the v2
pages, Settings also builds:

- **Certificates:** every host's HTTPS fingerprint and SSH host key
  fingerprint from pmxcfs (`SECURITY.md`), and rotation (new key on the
  host, new fingerprint shown over the old certificate, switch on confirm);
- **Join and enrollment bundles:** issue, list as `unused`, `used`,
  `expired`, or `revoked`, and revoke (`DEPLOYMENT.md`, "ISO lifecycle");
- **Log replication** (M5.4);
- **The address plan locked:** `PRIVATE_SUBNET_CIDR` (any network from
  /16 to /24), `MAX_MOX_HOSTS` (1 to 32), and every `*_IP_START`/`*_IP_END`
  range are shown read-only once the cluster settings exist, and bmacd
  refuses a change to them (`bmac_settings::LOCKED_CLUSTER_KEYS`).

Each guest page shows the `ssh_config` block and `known_hosts` line for
workstations without the Deployer (`DEPLOYMENT.md`, "Workstations without
the Deployer").

**From v2:** each v2 page and the sources it used:

| v2 page | v2 sources | v3 data |
| --- | --- | --- |
| `DashboardPage.tsx` (Overview) | `list_hosts`, `list_guests`, `list_replication`, `list_storage` | Cluster snapshot and conditions (M5) |
| `HostsPage.tsx` | `list_hosts` | Hosts snapshot |
| `ProductionPage.tsx` | `list_guests`, `list_replication` | VMs snapshot |
| `StagingPage.tsx` | `list_guests` | VMs snapshot |
| `StoragePage.tsx` | `list_storage`, `list_disks` | Storage snapshot with layouts and capacity (M8) |
| `QDevicePage.tsx` | `list_hosts`, `check_qdevice_access` | QDevice snapshot |
| `DiagnosticsPage.tsx` | `show_*` workflows | Deep diagnostics operations |
| `OperationsPage.tsx` | catalog and history | Activity |
| `ConfigPage.tsx` | workstation config files | Settings, then Cluster settings |
| `SettingsPage.tsx` | preflight, repository | Settings (notifications, certificates and SSH host keys, updates); the preflight and first-host parts move to the Deployer (M6) |

The v2 UI never computes cluster state itself (`protocol/state.ts` 1–2);
v3's pages keep that rule, showing what the server sends.

**Tests:** port v2's page tests: `QDevicePage.test.tsx` and
`SettingsPage.test.tsx` here; `SettingsFirstHost.test.tsx`,
`SettingsHostsDetected.test.tsx`, and `SettingsQDevice.test.tsx` cover the
first-host and detection flows that move to the Deployer, and are ported
in M6. Add tests for every page against the mock backend.

### M3.5 Content Security Policy and safety

**Builds:** the CSP from `SECURITY.md`, verified to work with xterm and the
bundled fonts; a lint rule rejecting `dangerouslySetInnerHTML`; output
rendered as text.

The lint is oxlint's `react/no-danger` (with `no-eval` and friends) in
`dev/check`, plus a search for `innerHTML`-style sinks: `typescript-eslint`
supports TypeScript only below 6.1, and v3 uses TypeScript 7.
`lab/browser/check-webui.mjs` fails on any CSP violation in Chrome while it
logs in and runs `lab.interactive` against a lab host.

### M3.6 Visual comparison

**Builds:** a Playwright run of the web app against the mock backend that
screenshots every page and state from M0.3 and compares them, with a small
tolerance and a reviewed list of intended differences (login page, new
pages).

**M3 done when:** in the lab, every v2 page's equivalent works live against
`bmacd`, and the visual comparison passes.

---

## M4. Script operations

### M4.1 Copy and rename

**Builds:** `v3/legacy/scripts/` and `v3/legacy/tests/` with copies of the
v2 scripts that run as script operations, their libraries
(`scripts/lib/*`), utilities, host runtime, and tests; the `app-ha` names
renamed to `bmac` throughout (paths, units, nftables tables, mapper
prefixes stay as the storage design says), as one mechanical change with
its tests updated in the same change.

Not copied as script operations:

| v2 | Becomes |
| --- | --- |
| `list_*.sh`, `check_qdevice_access.sh` | Collectors (M5) |
| Storage workflows (`add_new_disk_vdev`, `add_replacement_disk`, `decommission_disks`, `inventory_disks`, `simulate_disk_failure_and_replacement`) | Native (M8) |
| `add_proxmox_host.sh` | Split: ISO building in the Deployer (M6), post-install phases in the host setup job (M7) |
| `update_cluster_runtime.sh` | Package installation and the cluster update job (M10) |
| `setup_jump_ssh_access.sh`, `report_stale_jump_ssh.sh` | The Deployer's Workstation SSH (M6.9) |
| `deploy_hello_app_to_prod.sh` | A Deployer feature (decision 1, M6.9) |

### M4.2 Common adaptations

**Builds:** the changes every adapted script needs, listed in
`WORKFLOW_MIGRATION.md` ("Adapting the scripts"), implemented once in the
libraries where possible:

- `config.sh` loads from `/run/bmac/config/`; the config-location pointer,
  macOS handling, Homebrew paths, and Git checks are removed.
- `mox_ssh` and `node_exec` run locally when the target is this host;
  otherwise they use `/etc/bmac/ssh_config`, which maps `moxN` to the private
  address and pins host keys from `/etc/pve/nodes/<moxN>/ssh_known_hosts`
  with `HostKeyAlias` and `CheckHostIP=no`. v2 already uses exactly these
  pins for its nested hops (`config.sh` 1093–1253, `host_membership.sh`
  `hm_exec` 95–122), so this is the same trust model without the
  workstation hop. Remote arguments stay `%q`-quoted.
- Coordinator discovery (`control_find_cluster`, `cluster_control.sh`
  120–153) is replaced by `bmacd` choosing the target host from the
  operation's rule; the script is told which host it is on.
- Registry path `/etc/pve/priv/bmac/registry/`; artifacts under
  `/var/lib/bmac/work/<job>/`; secrets on stdin or a root-only file deleted
  after the run; JSON mode always.
- Guest access through the guest agent (decision 2).

### M4.3 Per-workflow adaptation

| Operation | Target host rule | Script-specific changes |
| --- | --- | --- |
| `prod.add` (`add_prod_vm.sh`) | An online placement host, preferring the initial owner | Takes the chosen storage (block size) and records it; ISO request built locally instead of copied through a hop (1605–1622); root password arrives as a secret field and is hashed on the host with v2's stable salt (642–655); the final SSH login check becomes guest-agent checks (sshd active, attested host key); the workstation jump SSH step (3156+) is replaced by a next step pointing to the Deployer |
| `prod.remove` | The owner | None beyond the common changes; the source ISO cache is still kept, as v2's tests require |
| `prod.change_owner` | The current owner | None beyond common |
| `prod.change_placement` | The owner | Capacity check (236–239) calls the capacity function through `bmac capacity --json` |
| `prod.extend_disk` | The owner | Capacity limit from the capacity function (refused on raidz until it does); in-guest grow through the guest agent |
| `staging.add`, `staging.remove` | The chosen standby host | Clone on the production disk's storage; the workstation alias cleanup in `remove_staging_vm` is dropped (the Deployer shows the entry as gone) |
| `hosts.remove` (`remove_proxmox_host.sh`) | Any remaining host | Takes `cluster-membership`; the control-node handover (`hm_choose_new_control_node`) is replaced by that lock; the call to `report_stale_jump_ssh.sh` is dropped |
| `qdevice.add`, `qdevice.remove` | The duty host | Takes `cluster-membership`; QDevice SSH from the host with the cluster's QDevice access key in `/etc/pve/priv/bmac/`, authorized on the QDevice once by the operator (v2's workstation key has no host-side equivalent, and the duty host changes); `pvecm qdevice setup` still uses the coordinator's RSA key only while it runs. A host removal that leaves an even count asks for the same access. Verified in the lab by `lab/lab scenario qdevice` |
| `diagnostics.cluster_state`, `diagnostics.prod_vm_state`, `diagnostics.host_state`, `diagnostics.qdevice_state`, `diagnostics.cluster_health` | Any host | Kept as deep, on-demand diagnostics until their checks are covered by collectors |
| Host setup phases 4–6 | The new host, and for joins the bundle's host | Split out of `add_proxmox_host.sh` in M7 |

### M4.4 Installed runtime

**Builds:** the `bmacd` package installs the v2 runtime (`cluster_registry.py`,
`haproxy_routes.py`, `sync_haproxy_routes.sh`, `process_deferred_cleanup.sh`,
the lifecycle hook) under `/usr/lib/bmac/legacy/`, the hook into
`local:snippets/`, and the route-sync and deferred-cleanup timers, on every
host by package rather than by pushing files.

**From v2:** `update_cluster_runtime.sh` (7–71, 289–315): the list of files
and targets, hash verification, `bash -n` before installing, and rollback
on failure (`test_cluster_wide_safety_order_and_rollback_are_explicit`).
Package installation gives atomicity per host; the cluster update job (M10)
gives the ordering across hosts.

### M4.5 Tests

**Builds:** every copied test passes under `v3/dev/run_tests.py`, with fakes
changed for host-side execution. Tests of behavior that no longer exists
(workstation config, macOS, `~/.ssh/config`, the workstation lock) are
removed and listed in `v3/legacy/tests/NOT_APPLICABLE.md` with the reason.
Examples: `test_config_location.py`, the `test_workstation_ssh_config_*`
tests, `test_setup_jump_ssh_access.py` (moves to the Deployer's Rust tests),
the alias cleanup test in `test_remove_staging_vm.py`.

**M4 done when:** every script operation runs end to end on the lab cluster
from the Web Dashboard, including a staging VM created, patched, started,
and removed, a production VM created with replication and HA, and a
failover drill showing staging eviction.

**As built (lab, in progress):** `m4_prod` runs prod.add from mox3's
Dashboard on the `m7-cluster` stage. prod1 is installed unattended from
the pinned Ubuntu 26.04.1 ISO on mox1, replicated to mox2 and mox3, under
HA, and `bmac guest-host-key` attests its host key (owed by M2.12). The
three hosts are then saved as the stage `m4-prod`, from which the
production VM diagnostic passes. The run found:
- **Two bugs in `add_prod_vm.sh`, both from v2.** After the installer's
  poweroff, the script decided whether to start the VM from
  `/cluster/resources`, which pvestatd refreshes every 10 seconds. It
  still said running, so the VM was never started and the guest agent
  wait ran out. The script now asks `qm status` on the owner. The guest
  check also used a host variable (`PRIVATE_SUBNET_LENGTH`) inside its
  quoted heredoc, where it is unset; it is now passed as an argument, and
  a test checks that no guest script uses a host variable.
- **A read race in bmac-pmxcfs.** While a job record was being replaced,
  FUSE answered the stat from its cached attributes of the old file, which
  the read then found gone, and the Dashboard got HTTP 500.
  `Pmxcfs::read_json` now retries for up to 3 seconds when only the read
  misses the file.
- **The lab network.** QEMU's multicast sockets handed each VM its own
  frames, so mox1's bridge learned prod1's MAC on its uplink and prod1
  could not be reached from another host. The lab now switches its
  segments itself.
- **Fencing on a live restore.** Stepping a resumed host's clock fenced
  it when it ran an HA service, because watchdog-mux times its clients by
  the wall clock. The lab now stops the host's HA services around the step.
- **A job that never ended.** staging.add's script finished, but its job
  stayed running: the staging VM's QEMU, started through `mox_ssh` on the
  local host, inherited the job's protocol pipe, so bmacd never saw the
  end of its output. `mox_ssh` now closes the protocol descriptors for a
  local command, and bmacd gives a script's output 10 seconds after the
  script exits, then logs a warning and finishes the job without it.
- **HTTP 503 for a removed guest.** Starting an operation on a guest the
  registry no longer has answered "unavailable"; it is now "not found".
- **Eviction is deferred.** prod1's start on a standby stops that host's
  staging VM and marks it `cleanup_pending`; the cleanup worker, every 2
  minutes, destroys it once replication has dropped its snapshot copies.
  prod.change_owner refuses while staging VMs derived from the guest
  remain, so the scenario removes them first. Its refusal named the wrong
  phase; fixed.
- **A replication race in `add_staging_vm.sh`, from v2.** It accepted any
  replication run that started up to 5 seconds before its snapshot, by the
  script host's clock, so a run without the snapshot could pass and the
  GUID check then found it missing on a standby. The bound now comes from
  the owner's clock, and a run counts only once the target has the
  snapshot; otherwise the job is run again.
- **Settling after a restore.** The scripts read node status from
  `/nodes`, which pvestatd fills in after `/cluster/status` already shows
  the node online; the scenario waits for every host's `/nodes` view. A
  job's record can also name a request whose event is still being relayed
  to the Dashboard's host, so the lab waits for the event.

`m4_ops` passes from the `m4-prod` stage (57 checks, 29 minutes):
staging.add on mox2 and mox3, each with its own name, address, hostname,
and host key; the failover drill, where mox1 is powered off, HA restarts
prod1 on mox2, its staging VM is stopped within 15 seconds while mox3's
runs on, and the cleanup worker destroys it; staging.remove;
prod.change_owner back to mox1; prod.change_placement removing mox3 and
adding it back; prod.extend_disk, grown online in the guest; and
prod.remove, after which prod1 is gone from the registry, HA,
replication, and every host's pool. **M4's done-when is met in the lab.**

---

## M5. Monitoring and alerting

### M5.1 Collectors

**Builds:** every collector in `MONITORING.md`'s table, native, each with its
interval, timeout, and a typed observation, and the metric history (one
minute for 7 days, hourly for 90) behind the Dashboard charts and the
pool-full projection.

**From v2** (`show_cluster_health.sh` unless noted), the checks to keep with
their thresholds:

| Check | v2 source | Rule |
| --- | --- | --- |
| Quorum, votes | `pvecm status` (390–391) | `Quorate: yes` |
| Cluster units | `systemctl is-active` (397–404) | `corosync`, `pve-cluster`, and `corosync-qdevice` when registered |
| QDevice parity and votes | 405–406, 534–546; `qdevice.sh` `qd_status_is_healthy` 73–95 | Odd member count: no QDevice; even: alive and voting, expected votes = N + 1 |
| Corosync links | `corosync-cfgtool -n` (427–527) | Every peer listed, every link connected; a peer down from every observer reported once (514–527) |
| QDevice host | 556–583 | Collector (local only): the QDevice's Tailscale address equals the registered one and port 5403 accepts a connection. The checks that need SSH to the QDevice (qnetd active, serving this cluster's name) stay in the on-demand QDevice diagnostic, since collectors never use SSH (`MONITORING.md`) |
| Pool state, members, errors | `zpool status -P -p` (692–743) | Pool `ONLINE`; non-spare members `ONLINE`, spares `AVAIL` or `INUSE`; read, write, and checksum counts zero; no data errors; resilver in progress is a condition; last scan errors |
| Pool free space | 745–790; `host_storage.py` 60, 762–767 | Warning at 80% used, critical under 10% free, using the capacity function |
| SMART | `smartctl -H`, 30 s timeout (807–828) | PASSED or OK; bit 8 failing; missing `smartctl` skipped; timeout is a condition |
| Required units | `show_proxmox_host_state.sh` 175–196 | `pve-cluster`, `corosync`, `pve-ha-lrm`, `pve-ha-crm`, `keepalived`, the BMAC timers; `systemctl --failed` empty |
| Production contract | `show_prod_vm_state.sh` 243–313 | Registry `active` with routes enabled and install phase `network-finalized`; VM running on a placement host; one HA resource, `started`, no group, `max_restart=3`, `max_relocate=1`, failback 0, auto-rebalance 0; one strict equal-priority affinity rule matching placement; replication jobs equal placement minus owner |
| Guests | `show_proxmox_host_state.sh` 294–311 | Active production not running; staging and its production both running on one host |
| Replication | `quick_state.py` | Idle, no `fail_count`, no `error` |
| VIP owner | `add_proxmox_host.sh` final verification 5010–5069 | Exactly one owner among online hosts, stable across polls |
| Registry agreement | `cluster_registry.py reconcile --live` | Read-only reconciliation |

Collectors run commands locally, never over SSH, append errors and carry on
(`quick_state.py` 123–134), and report a timeout as a timeout.

**Tests:** v2's fixtures from `test_show_cluster_health.py`
(`test_failed_drive_names_host_member_and_serial`,
`test_one_down_link_is_named`, `test_down_host_is_reported_once_per_symptom`,
`test_qdevice_failures`, `test_low_rpool_free_space_asks_for_storage`, and
the rest), `test_show_proxmox_host_state.py`, `test_quick_state.py`, run
against the collectors. A test asserts collectors only run read-only
commands, as v2's `test_only_read_only_commands_run` does.

**Notes (built):** the collectors are in `bmacd/src/status/collect.rs` and
`status/health.rs`, scheduled by `status::SCHEDULE`. Choices made while
building them:

- LUKS has no collector of its own. The storage collector derives the
  crypttab and open-mapper mismatches from the layout it already reads.
- Registry agreement (`bmac_registry::reconcile::live`, matching v2's
  `reconcile --observed` exactly in `tests/reconcile_parity.rs`) runs only
  on the duty host, since each run reads every VM's config.
- The read-only test is
  `status::tests::collectors_only_run_read_only_commands`, an allowlist of
  command patterns.
- Metric history (`bmacd/src/metrics.rs`, SQLite migration 3) holds CPU,
  load, memory, pool use, and replication lag. Each hour, the pool-full
  projection fits a least-squares line to a pool's last 7 days.

### M5.2 Conditions and next steps

**Builds:** the condition model and the default conditions in
`MONITORING.md`, each with a message and suggested next steps naming
operations with prefilled values.

**From v2:** the `problems` and `next_steps` that the `list_*` and `show_*`
scripts emit (for example the rpool space problem pointing to adding a vdev,
`show_cluster_health.sh` 745–790, and the QDevice add/remove/repair steps in
`show_qdevice_state.sh` 112–229). Reuse their wording where it fits.

**Notes (built):** `bmacd/src/status/conditions.rs` covers corosync links,
units, Tailscale, the clock, certificate expiry, stuck cleanups, the
QDevice's address and qnetd, mixed BMAC versions, pmxcfs use, the egress VIP
owner, ingress, guest agents, registry agreement, pool members, LUKS, SMART
failure and wear (reallocated sectors growing past the lowest count seen
since bmacd started), and the pool-full projection. The thresholds are
`conditions::Thresholds` defaults for now. Still owed:

- the thresholds as cluster settings;
- boot redundancy, meaning a spare in use (with the storage operations);
- a job failed or interrupted;
- login failures over a threshold, and an enrollment token reused (M6);
- log copy lag (M5.4);
- backups and appliances (M9).

### M5.3 Alerts, silences, notifications

**Builds:** the alert state machine in pmxcfs, reopening within 10 minutes,
acknowledgment, silences, the local fallback without quorum and merge after,
the Postmark channel, routing rules and throttles, the outbox sent by the
duty host with idempotency keys and backoff, the daily digest, and the
test-send button.

Email goes through Postmark templates kept in Git (`MONITORING.md`, "Email
templates"):

- `v3/notify/templates/`: the layout and the first templates
  (`bmac-alert-opened-v1`, `bmac-alert-resolved-v1`, `bmac-digest-v1`,
  `bmac-test-v1`, and the appliance's `bmac-watchdog-down-v1` and
  `bmac-watchdog-up-v1`), each with a test model, in `manifest.json`.
- `v3/dev/postmark_templates.py` with `list`, `diff`, `validate`, `sync`
  (plan by default; `--apply` and a matching server name from `--server`
  or `POSTMARK_SERVER_NAME` to change), and `retire`, using
  `POSTMARK_SERVER_TOKEN` from the environment (no account token needed). Before
  the first sync, run `list` against the real server and record any
  existing templates in the plan's notes, so nothing already in use is
  touched.
- Rust model types per template, one send per recipient with
  `/email/withTemplate`, success judged by status, `ErrorCode`, and
  `MessageID`, the error handling table in `MONITORING.md`, the sender
  address setting, the template check and create-missing action in
  Settings, and `ErrorCode` 1101 (template missing) held in the outbox with
  a "templates missing" condition instead of any untemplated send.
- Short help text beside the Postmark fields in Settings (the only place
  the token is entered), linking to `v3/README.md` ("Email alerts with
  Postmark"), which is kept accurate as this is built.

**From v2:** none; v2 sends no alerts (decision 4).

**Tests:** state machine transitions; the outbox under a duty host change
(no duplicate when delivery was recorded); throttling; a fake Postmark
endpoint (success, 429, 5xx, timeout, and `ErrorCode` 10, 406, 1101, 300,
each handled as the table says); no secret ever appears in a message body
or template model (secret-sweep test); the template tests in
`MONITORING.md`; the developer tool against a fake Postmark API (refuses to
modify foreign or released templates, reports drift, refuses a server name
mismatch, refuses to exceed 100 templates, never prints the `GET /server`
tokens, changes nothing without `--apply`). Lab: `validate` and a `sync`
of drafts against a `Sandbox` Postmark server, then real sends of each
template from a `Live` server to a test inbox.

**Notes (built):**

- The lab's Postmark server is `bmac_lab`, delivery type `Sandbox`. It had
  no templates and no layouts before the first sync, so nothing was in
  use. `lab/lab postmark validate` passed, then `sync --apply` created the
  layout and the six templates as drafts; `diff` shows them all the same.
  A sandbox delivers nothing, so the real-inbox sends wait for a `Live`
  server; until then the lab checks the server's activity instead. One
  test send showed that a sandbox server also refuses a From address that
  isn't a confirmed Sender Signature (`ErrorCode` 400), so the lab needs a
  confirmed sender before any send, and BMAC treats 400 and 401 as
  `notify.sender_refused`, holding the outbox rather than dropping it.
  With the confirmed sender in `POSTMARKAPP_LAB_API_SENDER_SIGNATURE`,
  `lab/lab postmark send bmac-test-v1` was accepted (a MessageID came
  back).
- Crates: `bmac-notify` has the template models (embedded from
  `v3/notify/templates/`, so a build always sends what it checked) and the
  Postmark client (system CA bundle, 15-second timeout). `bmacd/src/notify/`
  has the alert book, the outbox, the stores, and the notifier task (every
  15 seconds).
- Storage: alerts, history (the newest 2,000, messages cut to 300
  characters), silences, settings, and the outbox are records in
  `/etc/pve/priv/bmac/notify/`, and the token is
  `/etc/pve/priv/bmac/secrets/postmark.token` (mode 0600, at most 4 KiB).
  Without quorum, each host keeps its own alerts and outbox in
  `/var/lib/bmac/notify-local`, reading settings and silences from pmxcfs,
  which stays readable; each host merges its own back when quorum
  returns. Settings and silences are only changed with quorum.
- Alerts: a critical condition opens an alert at once and a warning after
  5 minutes. One that returns within 10 minutes reopens with the next
  generation, and so does one that gets worse; the generation is part of
  every idempotency key (`{alert}:opened:{generation}:{recipient}`). For 5
  minutes after bmacd starts, nothing resolves, so a restart doesn't email
  recoveries and reopenings.
- Routing: no routes means warnings and criticals of every category to the
  default recipients, with recoveries. The throttle (60 minutes by default)
  allows one opening message per alert in that time; a worse severity
  bypasses it. There are no reminders while an alert stays open; the daily
  digest lists them. `notify.*` conditions are never emailed, since they
  are about email itself.
- Delivery trouble is reported by the host doing the sending, as
  conditions: `notify.unauthorized` and `notify.sender_refused` (both
  critical), `notify.templates_missing`,
  `notify.inactive_recipient` per address, and `notify.failing` after an
  hour of failures. This was owed by M5.2.
- Roles: everyone reads alerts; operators acknowledge and manage silences;
  admins change the notification settings, store the token, check and
  create templates, and send the test email. Each change is audited; the
  token never is, and no answer ever contains it.

### M5.4 Log store and copying

**Builds:** the journal collector, the critical-health snapshot, live
copying over the host-to-host streams with acknowledgments, the catch-up for
every known origin, the per-stream hash chain, gap filling, low-priority
bandwidth limits, and retention with the eviction order in `MONITORING.md`.
In the Dashboard: the Logs page (search by host, job, severity, unit, and
time, with paging, and export of a search as a file download, `SECURITY.md`)
and Settings, then Log replication (per origin and peer:
last sequence, lag, gaps, bytes, oldest record).

**Tests:** three-host harness: a host dies and its records are readable on
the others; a rebuilt host receives a dead host's records from a third;
gaps are filled; eviction order; audit eviction raises a condition; Logs
search filters and pages over records from several origins.

**Notes (built):**

- Records: every record now stores the hash before it, its body size, its
  job event type, a severity (info, warning, error), and an eviction class,
  all set when it is written (database migration 4). A copy carries the
  hash before it, so a host can check and store a copy without holding its
  predecessor; holes fill in any order. A held predecessor must agree with
  it. Only a copy with no `prev` and no predecessor is refused as a gap.
- Catch-up: `GET /v1/holdings` lists every stream a host holds (sequence
  range, count, times, bytes); `GET /v1/records?origin&stream&after&limit`
  returns up to 500 records, or about 4 MiB. Every minute, and when a link
  connects, `bmacd/src/mesh/catchup.rs` reads each peer's holdings, finds
  the ranges it lacks with a window query, and fetches them, for every
  origin. Fetches share a 2 MiB/s token bucket. Backing off during heavy
  ZFS sends or replication is not built; it waits for M8, which runs them.
  Copies of an origin's streams can arrive in any order, so an origin's
  newest stream is the one with the latest start time, read from its name,
  not the one learned last.
- Retention (`bmacd/src/records/retention.rs`, hourly): records older than
  30 days are deleted a stream prefix at a time. The stream's floor
  remembers the deletion, so peers never send those records back, and
  copies older than the cutoff are not stored. Over the budget (8 GiB of
  body bytes, reduced so the root filesystem keeps 15% free), the oldest
  records are evicted in the class order: output of completed jobs, other
  job events, journal and bmacd log, then audit events and failed or
  interrupted job events. An evicted record keeps its key, time, and hash,
  so it is never fetched again, and the Logs page shows it as evicted.
  Segment frames no row points at any more are removed. Sizes are body
  bytes before compression, the same on every host.
- Journal collector (`bmacd/src/status/journal.rs`, every 30 seconds):
  `journalctl -o json -p warning` after its cursor, keeping the units in
  `MONITORING.md` and BMAC's own, plus kernel messages that match storage
  and hardware words. Each unit is limited to 20 entries a minute, with a
  note of how many were dropped. On restart it starts after the newest
  journal record stored. When a critical condition about this host first
  appears, the last 15 minutes of those units at info level and above are
  stored at once, marked as a snapshot.
- Conditions, from the `logs` observation every host publishes each
  minute: `logs.copy_lag` when an online peer's copy of this host's
  current stream is more than 5 minutes behind (owed by M5.2), and
  `logs.audit_evicted` for 7 days after audit events were evicted.
- Engine and API: `search_logs` (`POST /api/v1/logs/search`) and
  `log_replication` (`GET /api/v1/logs/replication`), for every role.
  Search is newest first, with filters in SQL and a cursor on
  `(time, origin, stream, sequence)`. A text search also reads compacted
  output from its frames, at most 20,000 rows per page; past that the page
  comes back short, with a cursor.
- Dashboard: the Logs page searches, pages, shows a record's body, and
  exports a search as NDJSON (up to 50,000 records), fetched with the
  in-memory token and handed over as a Blob. Settings, then Log
  replication shows storage use against the budget, per-origin holdings
  and gaps, and each peer's copy of each origin: last sequence, lag in
  records and time, gaps, and when it was last read.
- Tests: three-host harness tests in `bmacd/tests/logs.rs` (a dead host's
  records readable on the others; a rebuilt host getting a dead host's
  records and its own old ones from a third; gaps filled; lag reported),
  store tests in `bmacd/src/records/tests.rs` (holes, eviction order and
  audit eviction, aging and floors, search filters and paging across
  origins with compacted output), conditions, API, and webui tests.
- Lab (`lab/scenarios/m5.py`): the journal collector read corosync's
  KNET warnings from a real journal, and a host powered off caught up on
  every origin within a minute of coming back.

**M5 done when:** in the lab, pulling a virtual disk, stopping corosync on a
host, filling a pool, and powering off a host each produce the expected
condition and alert within its interval, an email reaches a test inbox
through Postmark's sandbox, and the recovery message follows.

**Done** (`lab/lab scenario m5`, 35 checks). Each fault opened its alert
and sent its email through the `bmac_lab` sandbox server, and each repair
resolved it and sent the recovery: a SCSI disk hot-unplugged over QMP
(`zfs.pool.health` and `zfs.pool.member`, within 15–55 s), a 4 GiB pool
filled (`zfs.pool.capacity` critical at 3% free, about 60 s), corosync
stopped on mox3 and mox3 powered off (`host.offline`, about 10 s; the
second reopens the same alert). Found on the way: the journal collector
watched `bmac-web.service` instead of `bmacd-web.service`; the lab build
lacked `notify/templates`. A pool under about 4 GiB never reads critical,
because ZFS's minimum 128 MiB of slop counts as free in `zpool list`;
real pools are far larger.

Added after M5, with the backup redesign: every condition can be switched
off in the notification settings (`disabled_conditions`, an exact condition
or a category ending in `.`, as silences match). A switched-off condition
still shows on its resource's page, opens no alert, and an alert open when
it is switched off closes without a recovery email.

---

## M6. BMAC Deployer

### M6.1 Copy and cut

**Builds:** `v3/deployer/`, a copy of `dashboard/` (Tauri, React, Rust
backend), with the engine, workflow catalog, run history, Diagnostics,
Operations, the `list_*` sources, the repository checks, and config editing
removed, and the shared `v3/ui` components used in place of the copied
ones. What stays: the window, theme, preflight checks for workstation
commands, the job-style progress view, and request dialogs (now raised by
the Deployer's own Rust code instead of a script).

**From v2:** `dashboard/src-tauri/` (window, close handling, single
instance), `tauri.conf.json`, the `.deb` build and its desktop entry, and
`dashboard/engine/src/preflight.rs` (command presence and versions), moved
into the Deployer's backend. Removed commands are those
listed in M2.4.

**Tests:** copy the Tauri-side tests that still apply; the Deployer's
frontend tests use a mock of its own small backend.

### M6.2 `bmac-inventory`: hardware facts and link-wait

**Builds:** the `bmac-inventory` crate and its static binary, run on the
discovery ISO. It reports every disk (path, model, serial, WWN, exact byte
size, logical and physical sector size, rotational, transport, existing
partition tables, LUKS headers, and ZFS labels with their pool names), every
physical NIC (name, MAC, driver, speed, carrier after link-wait), CPU,
memory, firmware mode (UEFI or BIOS), and Secure Boot state, as JSON.
Its one writing command, `clear-zfs-labels`, is described in M6.3.

**From v2:**

- `cluster_setup_prereq.sh`: the values it lists (NVMe serials, exact byte
  capacities, physical NIC MACs, link state) and its `/sys` reading that
  tests point at a fixture tree (`CLUSTER_SETUP_PREREQ_SYS`).
- `scripts/utilities/disk_inventory.py`: serial normalization and duplicate
  serial handling (`test_disk_inventory.py`).
- The link-wait rules from `DEPLOYMENT.md`, taken from
  `add_proxmox_host.sh` 3277–3298: `ip link set up` first, carrier polled
  10 times 3 seconds apart, speed read only after carrier, "no carrier" and
  "wrong speed" reported separately.
- iDRAC "Virtual MAC Addresses" (`cluster_setup_prereq.sh` 37–44) were a
  source of confusion; v3 reads MACs from the OS, which is what the
  installer's `ID_NET_NAME_MAC` filter matches.

**Tests:** fixture `/sys` and `/dev` trees: NVMe and SATA, duplicate
serials, a NIC whose carrier appears on the fourth poll (passes), one that
never gets carrier (reported as no carrier, not as wrong speed), and one
with carrier at the wrong speed. Port `test_cluster_setup_prereq.py`.

### M6.3 Discovery ISO

**Builds:** the discovery ISO from Alpine Extended (a pinned release, or
the operator's choice in Settings by URL and SHA-256), with an Alpine
overlay added by the Deployer with `xorriso`. The first build used Ubuntu
Server's live ISO; on the real R640 its 4 GB root, read on demand through
the iDRAC's Virtual Media over the VPN, stalled and then failed with
squashfs read errors, so discovery moved to Alpine, which runs from RAM
(`DEPLOYMENT.md`, "The discovery ISO"). The overlay holds a script that
copies the kernel modules into RAM, installs udev, brings up the public NIC by DHCP or
with the static values the operator gave (trying each physical NIC in turn
with link-wait when the MAC isn't known, as `DEPLOYMENT.md` describes),
records which port it brought up and how (offered later as the host's
public network), joins Tailscale with a one-use
ephemeral key, starts SSH with a pre-generated host key and the Deployer's
key only, and runs `bmac-inventory`. The Deployer finds it on the tailnet
the same way as an installed host (M6.6) and reads the inventory over
pinned SSH. `bmac-inventory` reports old ZFS labels read-only (pool name
and GUID from the label).

**Stale `rpool` labels are cleared here, before installing.** The Proxmox
installer erases only the disks it installs to. A disk that was a raw
(unencrypted) member of an earlier `rpool` keeps its label, so the first
boot finds two pools named `rpool` and stops at the `(initramfs)` prompt
(v2's `print_duplicate_rpool_help`, 1510–1563, describes the failure and
the manual `zpool labelclear` fix). That happens before `bmacd` ever runs,
so the host setup job is too late to fix it. When the inventory shows any
disk carrying an `rpool` label, the Deployer lists each one (model, serial,
WWN, size, the labelled partitions, and the old pool's GUID). It explains
that the installed host may not boot unless they are cleared, and offers to
clear them, with every listed disk selected and each one deselectable.
Clearing needs a destructive confirmation that names the disks by serial.
The Deployer then runs `bmac-inventory clear-zfs-labels` over the pinned
SSH session, with the expected serial, WWN, and pool GUID of each disk. It
re-identifies each disk and refuses on any mismatch, then runs `zpool
labelclear -f` (the remaster adds `zfsutils-linux` if the lab spike shows
the live image lacks it) on each labelled device (whole disk or partition) and only
on those, and re-reads the labels to prove they are gone. The inventory is
then taken again. If the operator declines, the warning stays on the host's
card and in the installation steps, along with v2's initramfs recovery
instructions. Labels with other pool names are shown but not offered for
clearing, since they don't stop the boot (the storage phase refuses
foreign-labelled disks, M7.3).

Apart from that one confirmed action, the discovery OS never writes to a
disk: it enables no swap, mounts no disks, and `bmac-inventory`'s other
commands only read. The other action offered on the live system is
**Power off**.

**Tests:** the remaster step runs in `v3/dev/check` against a small fixture ISO; the lab
boots it in a VM with NICs on the slow-carrier bridge (M0.4).
`clear-zfs-labels` against fixture devices: refuses on a serial, WWN, or GUID
mismatch without touching any device, clears only the labelled ones, and
reports a label that survives. In the lab, a host VM is given an old raw
`rpool` member on a non-boot disk. Installing without clearing reproduces
the initramfs stop, and installing after clearing boots.

### M6.4 Proxmox ISO builder

**Builds:** the host's network chosen from the inventory: the public port
(prefilled with the one discovery brought up), its static address, prefix,
and gateway (prefilled with what discovery used, with a reminder to check a
DHCP lease against the provider's assignment), and the private VLAN port
(prefilled when exactly one other port has a link), which must differ from
the public one, with its address and prefix (prefilled with the slot's
default and the private subnet's prefix, editable within
`MOX_IP_START`–`MOX_IP_END` with that prefix or a shorter one, and unique
among the hosts, which a join bundle lists with their private addresses), an optional
gateway (prefilled with `DATACENTER_PRIVATE_VLAN_GATEWAY`), and a box, off
by default, to route to the internet through that gateway. The per-host ISO: source ISO cache, answer file, first-boot
payload with the host's bootstrap config, the new-cluster seed or join bundle,
generated SSH host key, generated HTTPS key and certificate, and the
one-use Tailscale key.

**From v2** (`add_proxmox_host.sh` unless noted):

- `ensure_source_iso` (485–525): hash-verified download into a partial file
  then rename, under `flock`; `offer_to_delete_source_iso` (526–546).
- `assistant_binary` (1157–1182): `proxmox-auto-install-assistant` pinned
  to the 9.2.5 package, verified by hash.
- `build_iso` (1183–1480):
  - a new SSH host key for every image (1205–1210), because an older image
    may still exist and must not identify this install;
  - the root password hashed with SHA-512 crypt and a salt derived from
    cluster name and host ID (1213–1222), passed to Python on fd 3, never on
    the command line;
  - the answer file: `root-password-hashed`, the admin keys, `reboot-on-error
    = false`, network `from-answer` with `ID_NET_NAME_MAC` filter on the
    public NIC, disk filters by `ID_SERIAL_SHORT` and `ID_SERIAL` for the
    boot disks, `zfs.hdsize`, and `[first-boot]` from the ISO with ordering
    `before-network` (1241–1270);
  - first boot (1273+): install the host key before sshd first starts and
    verify it, the host guard, `authorized_keys`, the Tailscale key in a
    root-only file, and the Tailscale bootstrap with a DNS wait and curl
    retries.
- `reserve_fresh_tailscale_auth_key` (770–802): refuses a key whose digest
  was used before; v3 keeps the used-key digest list in cluster settings and
  carries it in the join bundle; for the first host, the list starts with
  that ISO's own key digest, carried in the new-cluster seed, so the
  Deployer keeps no state between runs (`DEPLOYMENT.md`). Builds with the
  `lab` feature accept a reused key, with a warning in the log, because the
  lab's keys are reusable (`lab/README.md`, "Tailscale for the lab"); unit
  tests cover the refusal. Before a lab VM joins with a reused key, the lab
  removes that VM's old device through the Tailscale API
  (`lab/bmaclab/tailnet.py`), and `lab/lab destroy` removes them all.
- `discover_hardware` (1007–1155): mirror members within 1% of each other,
  at least 8 GiB, and `hdsize` = smaller disk in GiB minus 4, leaving at
  least 4 GiB for the LUKS header added later. v3 takes the same numbers
  from discovery instead of iDRAC inventory or typed capacities.
- Proxmox ISO 9.2-1 (`config/cluster_dot_conf` 154).

v3 additions to the first-boot payload: install the `bmacd` package from
the ISO, write the seed or join bundle to `/var/lib/bmac/bootstrap/`, install the HTTPS key and
certificate, add 8443–8445 to the host guard, and start `bmacd` in setup
mode.

**Tests:** port the `build_iso` tests in `test_add_proxmox_host.py` (answer
file contents, hash and salt, `hdsize` arithmetic, host key handling, no
secret on a command line, digest refusal). New: the HTTPS certificate in
the ISO matches the fingerprint the Deployer shows; the ISO contains no
secret the bootstrap doesn't need (secret sweep of the extracted image).

### M6.5 iDRAC control (removed)

This step built `bmac-redfish` (power, virtual media from a URL the
Deployer served, one-time boot, certificate pinning) with a fake Redfish
server, and a temporary HTTPS ISO server. On the lab's real R640 (iDRAC9
7.00.00.181, reached over the provider's VPN) the read-only probes passed,
but `InsertMedia` never got the ISO: the iDRAC never opened a connection
back to the workstation, even with the workstation's firewall open, and its
`ping` diagnostic couldn't run. The browser's Virtual Media works because
the browser pushes the ISO to the iDRAC. So the operator boots every ISO
(M6.6), the crate and the ISO server were deleted, and the handoff's
fingerprint-pinned TLS check moved into `bmac-deployer` (`tls.rs`). Serving
the ISO from a machine on the iDRAC's own network could bring Redfish
mounting back later; it is not planned.

### M6.6 Booting and waiting for the host

**Builds:** the method choice (iDRAC Virtual Console, other remote
console, USB), the instructions for each, for discovery ISOs as well as
installer ISOs, and the wait for the host on the tailnet. The iDRAC steps
are the Virtual Console's: Connect Virtual Media, choose the ISO under Map
CD/DVD, Map Device, a one-time boot from Virtual CD/DVD/ISO, power on or
cycle, keep the console open (it streams the ISO), and Disconnect Virtual
Media afterwards. The optional iDRAC address only makes the first step a
link.

**From v2:**

- `installation_gate` (1565–1609): the steps per method, the destructive
  warning naming the disks by serial, the MAC selectors shown, the "Install
  complete" acknowledgment; `scrub_installation_media` (1487–1508):
  shred the ISO, answer file, first-boot script, and host key once installed,
  warn that USB copies still hold the host's private key and password hash.
- `tailscale_peer_selection` and `resolve_tailscale_ip` (1651–1703): a peer
  matches only by `HostName` or `DNSName` prefix, with the required tag,
  created after the ISO was built, exactly one, online, with a `100.x`
  address; ambiguous matches stop with instructions, offline and absent
  keep waiting.

**Tests:** port the peer selection tests; method instructions snapshot
tests.

### M6.7 Handoff

**Builds:** the five handoff steps from `DEPLOYMENT.md`: detect on the
tailnet, TLS fingerprint check (mismatch: stop, warn, no link; refused or
timed out: keep waiting), workstation SSH, the Open Dashboard button with
the mandatory browser fingerprint instruction, and cleanup (the ISO is
shredded once the host is on the tailnet, since nothing reads it after
that; `DEPLOYMENT.md`, "ISO lifecycle"). After that the Deployer does nothing more for the host; setup
continues in the Web Dashboard.

**Tests:** a local TLS server presenting the expected certificate, a
different one, and none; the link is never produced after a mismatch, and
the instruction to check in the browser is always present when it is.

### M6.8 Workstation SSH: hosts

**Builds:** the managed `~/.ssh/config` block and `known_hosts` pins for
cluster hosts, set up at handoff with the ISO's host key, or later for any
host with `ssh-keyscan` confirmed against the fingerprints in Dashboard
Settings followed by Proxmox's node keys read over that pinned connection.

**From v2:** `write_workstation_ssh_config` and `configure_workstation_ssh`
(1888–2110), and `scripts/lib/config.sh`'s workstation SSH helpers:

- the BEGIN/END markers and the block prepended, ending with `Host *`
  handling as v2 does;
- `HostKeyAlias` pins, `CheckHostIP=no`, `StrictHostKeyChecking=yes`;
- an exclusive lock, timestamped backups, refusal to follow symlinks, `0600`
  modes;
- proof with `ssh -G` and `ssh <name> true`; both files restored on failure;
- `workstation_ssh_settings_match` (1888–1900): no rewrite when nothing
  changed; `print_manual_workstation_ssh_instructions` as the fallback.

**Tests:** port the `test_workstation_ssh_config_*` tests from
`test_add_proxmox_host.py` and the config library tests, rewritten against
the Rust implementation, with a fake `ssh` that reads stdin.

### M6.9 Workstation SSH: guests, and the hello app

**Builds:** jump SSH entries for production and staging VMs through a host
(`ProxyJump`), using `bmac guests --json` and `bmac guest-host-key` over SSH
to a host; stale-entry reporting; and the hello app deployment (decision 1).

**From v2:** `setup_jump_ssh_access.sh` (guest list 232, attestation 354,
the managed block and alias rules), `report_stale_jump_ssh.sh`, and
`deploy_hello_app_to_prod.sh`, with `test_setup_jump_ssh_access.py` and
`test_report_stale_jump_ssh.py` ported to Rust tests. v2 has no tests for
the hello app; the Deployer's version gets new ones (certificate and key
handling, no secret on a command line, upload verified).

**Notes (built):** the libraries are `workstation_ssh.rs`, `jump_ssh.rs`,
and `hello/`. The tasks are `flows/ssh.rs` (`host_ssh` and `guest_ssh`)
and `flows/hello.rs` (`hello`), started from the Workstation SSH page. That
page also lists the managed entries, read offline from `~/.ssh/config` and
`known_hosts`, and removes one under the setup lock after a backup. The
problems it shows are the ones the files reveal: a missing pin, or a guest
jumping through a host with no entry. Live problems (a changed key, a host
gone from the cluster) come up when a host or guest is set up again. The
hello app takes the certificate and key pasted, with the key's field
sensitive, so it is redacted from the task's record. The tests in
`flows/tests_ssh.rs` drive a fake cluster whose `ssh -G` answers from the
config the task wrote. The lab check against real hosts belongs to the M6
done-when scenario.

### M6.10 Appliance ISO

**Builds:** the backup appliance ISO (Ubuntu Server LTS with
`bmac-backupd`, as `BACKUP.md` "Appliance platform" describes) with its
first boot, OS disk and backup pool chosen from discovery, LUKS2 under the
pool with the passphrase entered at the console, enrollment against the
cluster, and the same handoff and fingerprint rules as hosts. The Tailscale
key field says it must be single-use and tagged
`tag:bmac-backup-appliance`, and the wait for the appliance (M6.6's peer
selection) requires that tag. Its ISO holds the appliance's keys and
enrollment token, so it is shredded like the host's once the appliance is
on the tailnet, and its scratch files whether or not the build succeeds.
Built after M9's appliance daemon exists; the remaster method is the one proven for the
discovery ISO.

**M6 done when:** in the lab, the Deployer boots a VM from the discovery
ISO over a slow-carrier NIC, reads its inventory, clears a disk's old
`rpool` label after the operator confirms, builds the Proxmox ISO, shows the
boot steps while the lab plays the operator (attaching the ISO to the VM),
waits for the host on the tailnet, and hands off to the Dashboard with SSH
set up on the workstation. On the real R640, the operator follows the
iDRAC Virtual Console steps for the discovery ISO once, and the Deployer
reads its inventory.

**Notes (lab):** `lab/scenarios/m6_discovery.py` covers the slow carrier
and the label clearing; `lab/scenarios/m6.py` runs the whole "Install a
Proxmox host" task through `examples/lab_task.rs` on mox3, with the
development package from `dev/build_deb.py`, and puts mox3 back from a
checkpoint. Running it found three things since fixed: the installer takes
a first-boot file of at most 1 MiB, so the packages are a separate file on
the ISO (`DEPLOYMENT.md`, "Installation and first boot"); systemd's udev
spells an NVMe disk's `ID_SERIAL` differently from the discovery OS's, so
boot disks are selected by `ID_SERIAL_SHORT` and `ID_WWN` only, which
allows at most two boot disks; and both the assistant and xorriso (with
`-return_with SORRY 0`) could fail while exiting 0. The R640 step is done:
the operator booted the discovery ISO, built with static public network
settings, through the iDRAC Virtual Console, and the Deployer read its
inventory. A later R640 run with no network settings tried DHCP on every
port for three rounds and got no lease (that data center has no DHCP), so
the boot steps now say to rebuild with static settings when a DHCP-only
ISO never joins the tailnet; discovery by DHCP is covered by the lab VMs.
The same runs showed that a warm reboot into the ISO can hang early in
its boot, so every boot method's steps say to power off fully first.

---

## M7. Host bootstrap and setup

### M7.1 First boot and setup mode

**Builds:** `bmacd`'s setup mode on a host that isn't in a cluster yet:
the normal login (M2.3) as `root@pam`, then only the setup page, reading
the new-cluster seed from `/var/lib/bmac/bootstrap/`, and the start of the
host setup job (`DEPLOYMENT.md`, "Handoff"). There is no separate setup
token; further admins are Proxmox users in the `bmac-admin` group, created
in Proxmox.

**From v2:** `bootstrap_host` (2111–2221): wait for pinned SSH, verify
`boot_id`, check the Tailscale bootstrap completed before retiring the
first-boot payload, and `wait_for_pinned_setup_ssh` (1827–1841) with
`fail_host_key_mismatch` (1823). In v3 the host does these checks on
itself; the Deployer's pinned SSH check happens at handoff.

### M7.2 Joining additional hosts

**Builds:** the join bundle created on an existing host's Dashboard
(`DEPLOYMENT.md`, "Additional hosts": cluster UUID and name, the cluster
settings without secrets, the registry slot reservation, existing hosts'
addresses and HTTPS fingerprints, the Proxmox cluster CA fingerprint, used
Tailscale key digests, the public half of a per-join setup SSH key whose
private half stays in pmxcfs, and a one-use join token stored as hash and
expiry), the bundle list and revocation (M3.4), the new host's
`POST /v1/join` (M2.9), approval on the existing host, the join phase of the
setup job, and removal of the setup key once the host has joined.

**From v2:** `cluster_setup` (3608–3737): the registry host slot reserved
first (`reserve_registry_host_slot`, 3589), all members online before a
membership change (3517–3571), QDevice removed before an even-to-odd change
and added back after (`remove_qdevice_before_membership_change`,
`reconcile_qdevice`, `recover_qdevice_after_failed_join`), `pvecm create`
with `link0` on the private network (priority 100) and `link1` on Tailscale
(priority 10), `pvecm add` checking the cluster certificate fingerprint
obtained over SSH against `openssl s_client`, `/etc/hosts` pins written
before (`configure_cluster_host_resolution`, 3346–3436), and
`reconcile_private_cluster_ssh_trust` (3738–3790) afterwards. Concurrent
setups: the control node must finish its cluster creation first
(`main` 5112–5116); in v3 the `cluster-membership` lock orders them.

**As built:** approval runs `hosts.join` on the approving host
(`native/join.rs`). It drives the new host's own `bmacd native hosts.setup
--driven-by` over SSH and relays it into the job (`DEPLOYMENT.md`, "Host
setup job"). For now the job refuses to start while a QDevice is configured,
rather than removing it and adding it back as v2 does; the operator removes
it first and adds it again afterwards. Joins take turns: two new hosts may
ask at once, but approving one while another's `hosts.join` runs is
refused (the `cluster-membership` lock), and its request waits to be
approved again. A bundle names the members when it was issued, so a host
pins every member in the shared settings once it has joined. bmacd's own
pmxcfs locks are short-lived: a waiter asks pmxcfs to drop an expired one,
as Proxmox's `cfs_lock` does, because a membership change can leave one
behind (its removal refused while quorum was briefly lost). Lab:
`lab/lab scenario m7_cluster`.

**Tests:** port the cluster setup tests in `test_add_proxmox_host.py` and
`test_host_membership.py`; three-host lab join with two joins started at
once.

### M7.3 Host setup phases

**Builds:** the native host setup job, phase by phase, each idempotent and
resumable from live state.

| Phase | v2 source | Notes |
| --- | --- | --- |
| Identity verification | state markers (431–445), `hardware-config-sha256` | Disks re-identified by serial and WWN from the approved layout before any change. Old `rpool` labels were already offered for clearing at discovery (M6.3), since they stop the first boot; a disk in the approved layout that still carries any ZFS label is cleared only after an operator confirmation naming it |
| Boot vdev encryption (optional) | `stage_luks_password_file`, `verify_luks_password_file`, `rebuild_luks_member` (2222–2607), `verify_encrypted_mirror` | `STORAGE.md`'s in-place method, one member at a time: take it offline, LUKS-format its ZFS partition, `zpool replace` with the LUKS device, wait for the resilver, verify; the boot vdev is a single disk or a two-way mirror, and LUKS needs the mirror. The passphrase is typed only at the console, with `bmac luks-enter` (decision 6) |
| Data vdevs and per-block-size storages | `configure_extra_mirror`, `configure_raw_extra_mirror` (2802–3015), `verify_clear_mirrors` | Generalized to every layout type in `STORAGE.md` (M8); `rpool/data-32k` and the others with their Proxmox storages are part of this storage phase |
| Shared unlock | `configure_shared_luks_unlock` (3040–3172) | Shared crypttab form with `decrypt_keyctl`, proved by unpacking the initramfs before reboot |
| Boot drills (optional) | `raw_boot_test`, `encrypted_boot_test`, `final_normal_boot_test` (2303–2383, 2608–2690, 3173–3215), `reboot_and_wait` (2291) | Each boot proved by a `boot_id` change; degraded boots from each member; the job waits through reboots because its record is in the job store and `bmacd` resumes it |
| Private network | `configure_private_network`, `resolve_physical_nic` (3216–3345) | Link-wait rules; the NIC chosen by MAC (the Deployer always records one; automatic detection, requiring exactly one candidate with carrier, remains only for settings that say `auto`). The address is the host's `PROXMOX_SECONDARY_IP` with its prefix, no longer always the slot's `/24`. `PROXMOX_SECONDARY_GATEWAY` is recorded only; when `PROXMOX_SECONDARY_GATEWAY_ROUTE=yes` the phase adds a second default route through it with a higher metric than the public gateway's (the public port stays preferred), and proves it with a route lookup. With `no` or no gateway, no route goes through the VLAN |
| Cluster create or join | M7.2 | Under `cluster-membership` |
| Guest egress and VRRP | `configure_guest_egress` (3902–4170) | Exactly one VIP owner among online hosts, polled 10 times 5 seconds apart |
| Orchestration runtime, hook, HAProxy | `install_shared_orchestration_tools`, `install_guest_role_hook`, `create_haproxy_lxc`, `sync_haproxy_routes` (4171–4912) | Runtime comes from the package (M4.4); routes synced before strict ingress |
| LUKS header backup (when encrypted) | new | Each member's header under `/var/lib/bmac/luks-headers/`, offered as a Dashboard download, and available to enrolled appliances (`DEPLOYMENT.md` phase 8) |
| Final verification | `final_verify` (4913–5092) | Every check kept, plus `bmacd` health and peer streams |
| Setup key retirement | `retire_setup_key_with_admin_identity`, `enable_admin_ssh_for_target` (1795–1822, 1790) | Setup key removed from `authorized_keys`; admin keys remain |

The `bmacd` package the host ISO carries is installed at first boot, before
the network is up, so it `Depends` only on what Proxmox VE ships; the
programs the legacy runtime calls (`packaging/bmacd.depends`, such as
`corosync-qdevice`, `jq`, and `xorriso`) are its `Recommends`. Host setup's
first phase that needs them installs them with apt, and the orchestration
phase enables `snippets` content on the `local` storage, where the
package's postinst put the guest-role hook.

The orchestration phase (`DEPLOYMENT.md` phase 7; v2's step 8) and final
verification start as v2's script bodies, adapted in
`legacy/scripts/host_setup/` and run by the setup job with their output in
it, so the walking skeleton works early; they move to native code in M11.
The private network phase (`DEPLOYMENT.md` phase 4) and cluster creation
(phase 5) were small enough to write natively in M7, on `bmac-inventory`'s
link-wait code (`bmacd`'s `native/network.rs` and `native/cluster.rs`).

**Tests:** port the corresponding tests in `test_add_proxmox_host.py` and
`test_rpool_mirror.py` (including
`test_add_uses_shared_crypttab_form_and_verifies_boot_unlock`). New: the
job resumes correctly after a reboot at every phase boundary (lab).

**M7 done when:** the walking skeleton: in the lab, three hosts go from
blank VMs to a working cluster through the Deployer and Dashboard alone,
with LUKS on one of them, boot drills passing, and `final_verify` green.

**Status:** met on the SATA lab, except the boot drills, which were proved
earlier on the first host (`m7_storage`) and are run again with
`BMAC_LAB_DRILLS=1` in the final validation, with the SCSI and NVMe labs.
`m7_storage` takes mox3 from blank through every setup phase with LUKS;
`m7_cluster` joins mox1 and mox2 through the Dashboard, `final_verify`
green on all three. The owed lab checks are done: M1.7 (`zpool` fixtures
from the lab), M2.12 (`m2_cli`), and M4.5 (`m4_prod`, `m4_ops`).

---

## M8. Storage

M8a (the layout model, checks, and capacity function) is M1.8. M8 puts it
to work.

### M8.1 Write adapters

**Builds:** typed adapters for every storage change: `zpool`
(`create`, `add`, `attach`, `detach`, `replace`, `remove`, `scrub`, `wait`,
`labelclear`, `set`), `zfs` (`create`, `set`, `destroy` limited to BMAC
names), `cryptsetup` (`luksFormat`, `open`, `close`, `luksHeaderBackup`,
`luksDump`, key test), `sgdisk` (copy and randomize, new partitions, wipe),
`proxmox-boot-tool` (`format`, `init`, `refresh`, `status`), `/etc/crypttab`
editing, `update-initramfs`, `unmkinitramfs`, and `udevadm settle`. Each
returns typed results and is replayed from fixtures in tests.

**From v2:** `rpool_mirror.sh` (1606 lines), which is the reference for every
step:

- identity: `validate_serial`, `disk_by_serial` (158–172) require exactly
  one disk per serial; `holds_zfs_label`, `held_pool_devices`,
  `disk_holds_luks` (189–213, 371–380);
- partitions: `command_copy_partitions` (1322–1384) copies the survivor's
  table and randomizes GUIDs, refusing a target that shares the survivor's
  GUIDs (`shares_survivor_guids`, 1229); `wait_for_udev_fstype` (1385)
  waits for udev before using the ESP; sizes from `pair_end_bytes`,
  `layout_end_bytes`, `replacement_bytes` (350–370) and the logical sector
  size;
- ESPs: `command_boot_esp` (1396–1433), `verify_esps_in_sync` (1303);
- LUKS: `obtain_shared_key` and `shared_key_opens_all_members` (588–634),
  which test the passphrase against every online member, three attempts;
  `refuse_existing_luks` (1312) never reuses a foreign LUKS header;
  `command_luks_backup_headers` (741);
- unlock at boot: `crypttab_form`, `crypttab_line`,
  `replace_crypttab_entries`, `rebuild_initramfs`, and
  `verify_initramfs_mappers` (770–850), which unpacks the new initramfs and
  checks `cryptroot/crypttab` and the `decrypt_keyctl` script before any
  `zpool add` or `attach`;
- pool checks: `uniform_ashift`, `same_mirror`, `pool_healthy` (851–872).

**Tests:** port `test_rpool_mirror.py` scenario by scenario into Rust tests
against the adapters' fixture runner; the port gate (`WORKFLOW_MIGRATION.md`)
compares command sequences for the mirror cases.

**As built:** no separate adapter per tool. The write commands run through
the process runner from two places that share one implementation: the
setup phase's `Storage` (`native/storage.rs`: partitioning, LUKS, the
crypttab and initramfs proof, `zpool create`/`add`) and the storage
operations (`native/disks/`), which reuse `Storage` for member
preparation and add `zpool attach`/`replace`/`remove`/`offline`/`online`,
best-effort `labelclear`, and metadata erasure. Every program comes from
`bmac_adapters::programs`. The tests replay the commands against a fake
host (`native/disks/tests.rs`, `storage_tests.rs`) that names disks as
`lsblk -p` does (`/dev/sdX`), because the lab found a mismatch between
the layout's `/dev/sdX` and the inventory's `sdX`. Proxmox doesn't
install `cryptsetup`, so an unencrypted pool never calls it (the lab found
`isLuks` failing with exit 127); the inventory's LUKS facts answer instead.

### M8.2 Host storage journal

**Builds:** `/var/lib/bmac/storage-state.json`, written under `flock` with
`fsync` and rename, recording vdev numbers (never reused), additions,
replacements, removals in progress, trims, and scrubs.

**From v2:** `scripts/utilities/storage_state.py` (`validate`, `mutate`,
`record_trims`, `record_scrub`, `record_removal`, `set_removal`,
`record_replacement`, `record_addition`) and `test_storage_state.py`.

**As built:** `bmac_storage::journal` holds trims, scrubs, removals with
their members' identities, replacements until the old member's LUKS and
crypttab line are retired, additions (numbered, never reused), and a
failure drill under way. Changes take `flock`, then write, `fsync`, and
rename. Its tests port `test_storage_state.py`.

### M8.3 Workflows

**Builds:** the operations in `STORAGE.md` ("Workflows"), each with a
dry-run plan, resumable phases recorded in the journal, and the checks of
M1.8 run again at the start of every phase.

**From v2,** per workflow:

- `storage.add_vdev` from `add_new_disk_vdev.sh` (456 lines) and the
  helpers it calls in `scripts/host_runtime/rpool_mirror.sh`: new disks
  checked blank by `check_new_disks` (443–505), mirror pair end bytes
  (`checked_pair_end_bytes`, 506), LUKS prepared per member, the initramfs
  proof, then `zpool add`, then `record_new_pair` (86). v3 adds every vdev
  type and class, and shows the restrictions in `../README.md` before the
  operator picks (raidz rules out later removal).
- `storage.attach_disk`: new; reuses the same preparation; raidz expansion
  needs the OpenZFS feature check first.
- `storage.replace_disk` from `add_replacement_disk.sh` (488 lines) and
  `command_replace_member` (1450–1571): replace only a missing or faulted
  member; for the boot vdev, copy partitions and set up the ESP first; don't
  wait for the resilver inside the job (it shows as a condition); return a
  spare after replacement. Proxmox runs the ZFS event daemon, which puts a
  hot spare in for a faulted member within a second (seen in the lab by
  `m2_cli`), so the workflow finds the spare already resilvering in and
  replaces the faulted member under it rather than offering the spare.
- `storage.remove_vdev` from `decommission_disks.sh` (611 lines): refuse
  on layouts that don't allow removal; destroy related staging first;
  `fstrim` every production guest on or replicating to the host through the
  guest agent (decision 2), one at a time, with trims under 24 hours old
  reused (249–283); force replication with `pvesr schedule-now` after the
  trims and retry while locked (318); a clean scrub under 24 hours old or a
  new `zpool scrub -w` (350–378); the minimum free space of at least 50 GiB
  (`MIN_FREE_FLOOR_GIB`, 26) checked with the capacity function; then
  `zpool remove`, keeping the record if the result is ambiguous.
- `storage.inventory` from `inventory_disks.sh`, `command_release_disks`,
  `release_blocker`, `disk_is_blank` (1000–1097), and `command_retire_luks`
  (968): finish removals by closing LUKS, updating crypttab and the
  initramfs; release is metadata only; list disks safe to pull; a stale
  DEGRADED after removal clears with a scrub.
- `storage.simulate_failure` from `simulate_disk_failure_and_replacement.sh`.

**Tests:** port `test_disk_workflows.py` (it covers all five v2 disk
workflows, in text and JSON mode), `test_host_storage.py`,
`test_disk_inventory.py`, and `test_rpool_mirror.py`;
add raidz, special, log, cache, and spare cases. Lab: every
workflow on mirror and raidz2 pools, with LUKS, on the VM lab with virtual
NVMe disks.

**As built:** `native/disks/` holds the six operations (`add.rs`,
`replace.rs`, `remove.rs`, `inventory.rs`, `simulate.rs`, and attach in
`add.rs`), each with the lock `storage:{host}`. The form shows the
removal restriction for the pool's current shape before the operator
picks. `attach_disk` widens a mirror, or widens a raidz once
`feature@raidz_expansion` is enabled. `simulate_failure` takes one member
of a healthy redundant vdev offline and closes its LUKS mapping, as if it
had been pulled; it then either erases the disk for Replace a disk or
brings it back online. That is how the lab exercises replacement and
removal: the lab has no disk hotplug, and neither does the FiberState
hardware. The Storage page offers all six as operation tiles.

### M8.4 Block-size storages

**Builds:** the per-block-size datasets and Proxmox storages created during
host setup, the cluster setting for which sizes are offered, the suggestion
logic at VM creation (smallest size with inflation at most 1.05 on every
placement host), and threading the chosen storage through VM creation,
staging clones, extension, and backups.

**From v2:** `local-zfs` is hard-coded in `add_prod_vm.sh`,
`add_staging_vm.sh`, `extend_prod_vm_disk.sh`, and the registry's volume
checks; every occurrence becomes the VM's recorded storage ID (M4.3).

**Tests:** suggestion table tests for mixed placement layouts; a staging
clone lands on its production disk's storage.

**As built:** the cluster setting `VM_BLOCK_SIZES` (a comma-separated list
drawn from 16K, 32K, 64K, and 128K; all four when unset) limits what new
VMs are offered. Setup still creates all four storages. `bmac capacity
--json` reports each block size's inflation, and `bmac block-sizes`
picks the suggestion from the placement hosts' capacities. In
`prod.add`, the root-disk form's first and default option is
"suggested", which resolves once the placement is known. Each choice is
labelled with its inflation per host, and anything above 1.05 is warned
about. `prod.change_placement` warns the same way when an added host
inflates the VM's blocks.

### M8.5 Capacity measurements

**Builds:** a lab run that writes known data to zvols with each block size
on raidz1, raidz2, and raidz3 pools of several widths, and records
`zfs list`, `zpool list`, and the capacity function's predictions.

**Done when:** predictions are within 1% of the measurements (`STORAGE.md`), or the
function is corrected until they are.

**As built:** `lab/lab scenario m8_capacity` uses a scratch pool of sparse
files on mox1 for each layout: raidz1 of 3–6 disks, raidz2 of 4, 6, 8, and
10, and raidz3 of 5, 7, and 9, all ashift 12 with compression off. Each
pool gets a 512 MiB zvol per offered block size, filled with random data,
and its `used` is compared with `bmac capacity --json`. The inflation
matched. The metadata allowance didn't: the planned `2 × 128 / b` made
every 16K prediction 1.05–1.42% high, because ZFS compresses the indirect
blocks. Measured metadata is 0.2–0.8 × `128 / b`, so the function now
charges `m(b) = 128 / b`. All 44 predictions are then 0.02–0.64% above the
measurement and never below it. `bmac-storage`'s `tests/capacity_lab.rs`
holds the function to that against the recorded measurements
(`tests/fixtures/lab/capacity.json`).

**M8 done when:** in the lab, a raidz2 host and a mirror host are set up,
disks are added, replaced, and removed (mirror only) through the Dashboard,
and VMs with 16K and 64K disks are created with the suggested sizes.

**As built (lab):** `lab/lab scenario m8_storage` runs on the SATA lab from
the three-host cluster stage and saves stage `m8-storage` (106 checks).
Everything goes through the Dashboard's API:

- mox2 gets a raidz2 of its four blank disks, and then refuses every vdev
  removal.
- mox1 gets two plain mirrors. mox3 gets an encrypted mirror, unlocked by
  typing `bmac luks-enter` at the serial console.
- The 16K VM goes on mox1 and mox3, and the 64K VM (`bmac-zfs-64k`) on
  mox2 and mox3. Both use the form's suggested size, and each replica keeps
  its block size.
- `storage.simulate_failure`, `storage.replace_disk`, and
  `storage.inventory` run a drill on mox2's raidz2 and on mox3's encrypted
  mirror.
- A mirror is removed from mox1 and from mox3, then retired by
  `storage.inventory`, which reports the disks safe to pull. mox3's crypttab
  and open mappings drop to four.

The removal's 50 GiB floor can't be met with 16 GiB disks, so the scenario
first grows mox1's and mox3's SCSI disks to 96 GiB (QMP `block_resize`,
then a SCSI rescan). SATA disks aren't resized, because libata may drop a
disk whose size changed.

The lab found three bugs, now fixed and covered by tests:

- Disk names: `lsblk -p` gives `/dev/sdX` where the inventory had `sdX`.
- `cryptsetup`: Proxmox doesn't install it, but unencrypted pools called it.
- Mapper names: `lsblk -p` gives a member's mapper as `/dev/mapper/NAME`,
  so a removal recorded mapper names crypttab didn't match. The mappers
  closed, but their crypttab lines stayed, and the initramfs check passed
  without testing anything.

`attach_disk` and the console unlock after a restart
(`BMAC_LAB_DRILLS=1`) aren't in this run, and the NVMe and SCSI labs come
at the end, as planned.

---

## M9. Backups

v2 has no backups. Its staging snapshot handling is the closest code, and
its lessons apply directly.

### M9.0 Lab spike: bookmarks and clone receives

**Builds:** lab scenarios for `BACKUP.md` lab items 2, 3, and 4 before any
code relies on them: bookmarks made on each placement host from its copy of
a PIT snapshot keep the GUID and survive replication runs, an HA move, and a
forced failover; an incremental sent from a bookmark by the new owner
applies on an appliance-like receiver; and an incremental received with
`-o origin=` as a clone, on the receiver (new branches) and on a host
(restore method 2); and a restored volume keeping its PIT snapshot's GUID
through rename, promote, a local send and receive, and replication's first
sync, with the next PIT sent from its bookmark and received as a clone of
the appliance's copy. The results decide whether M9.1 ships bookmarks or the
snapshot fallback, and whether backups after a restore continue
incrementally or start with a full send.

### M9.1 Cluster side: policies, schedule, capture, and snapshot lifetime

**Builds:** per-VM backup policies in pmxcfs (schedule, assigned
appliances, local retention, consistency) with the cluster backup settings
(`BACKUP.md`, "Backup policies"); the capture job (every host evaluates the
schedule and the first admission wins, as `ARCHITECTURE.md` "Schedules
without a leader" says; the VM's owner host runs it), with the "is anyone
there" check against the ready window and "Back up now"; freeze through the
guest agent with a watchdog thaw, `bmac-pit-*` snapshots of every disk of
the VM, replication and GUID checks on every placement host, the PIT
manifest in pmxcfs; offers, claims, and the claim window; bookmarks on
every placement host, then snapshot release; bookmark retention by the
delinquency period; local retention (none by default, newest N, daily,
weekly, with the trade-off explained in the setting); appliance states from
check-ins; and the alerts in `BACKUP.md` ("Alerts"), each behind its on/off
switch.

**From v2:**

- Snapshot names: `cluster_registry.py` 423–445 refuses unsafe names and
  Proxmox's reserved `__replicate_` prefix; v3 does the same and never
  touches replication snapshots.
- GUID agreement: staging snapshot records keep a GUID per node and per
  volume and require them identical (`cluster_registry.py` 1651–1733,
  1859–1864); a PIT is offered only when every placement host has the same
  GUID for every volume.
- Replication: `pvesr schedule-now` retried while the job is locked
  (`decommission_disks.sh` 318) to carry snapshots to replicas before
  checking.
- Deferred cleanup (`process_deferred_cleanup.sh`): snapshots that can't be
  destroyed now (a clone depends on them) are retried later, never forced.

**Tests:** fake `zfs` and `pvesr` fixtures: GUID mismatch refuses the
offer; a staging clone blocks retention with the reason shown; owner moves
between snapshot and offer; no available appliance skips the capture and
alerts, unless local retention claims the slot; the snapshot is kept for a
claimed transfer and released after the claim window otherwise; bookmarks
are made on every placement host before the snapshot is destroyed; a
delinquent appliance's bookmarks are released; LATE after 3 minutes without
a check-in.

### M9.2 `bmac-source`

**Builds:** a third process from the `bmacd` binary, in its own unit
(`bmac-source.service`) as the user `bmac-source` with ZFS `send` and
`hold` delegated on guest datasets (`SECURITY.md`); the 8445 listener with
mutual TLS, the request set in `BACKUP.md` ("Protocol"), check-ins handed to
the local `bmacd`, fingerprint checks against approved appliances, refusal
of anything not `bmac-pit-*` on a registered volume offered to that
appliance, incrementals from a bookmark only along the catalog's lineage, no
`-R` streams, resumable sends, transfer byte counts into the metric history
and a per-transfer log event (bytes, duration, average rate), and the
delegated or root fallback from lab item 5.

### M9.3 Appliance daemon

**Builds:** `bmac-backupd`: unlock at the console, import by pool GUID, the
catalog, check-ins every 60 seconds, claims, pulls with resume tokens,
two-phase publication, new branches received as clones, retention, prunes,
restore grants, trusted peers and repair from them (`BACKUP.md`, "Trusted
peers"), scrubs, the watchdog (`MONITORING.md`) with its optional checks of
public production URLs, its own Postmark settings, adoption after an OS
reinstall, and the appliance UI (`v3/applianceui/`, memory-only token) with
the PIT graph, transfer throughput, and the Trusted peers page (trust with a
typed fingerprint checked against the peer's certificate, untrust, recent
refusals).

Trusted peers, all or nothing: the list in the metadata dataset; the 8445
listener's client certificate check (trusted peer, or cluster host with a
restore grant), refusing everyone else in the handshake; `GET /v1/peer-index`
and `POST /v1/export` of published PITs for a trusted peer; untrust closing
open streams; repair asking only peers that this appliance trusts.
**Tests:** an untrusted appliance is refused before any request; one-way
trust serves one direction only; a fingerprint mismatch saves nothing;
untrust ends a stream in progress; a trusted peer still can't read
unpublished receives or change anything.

Enrollment, end to end (`DEPLOYMENT.md`, "Backup appliance ISO"): the
Dashboard issues an enrollment bundle (M3.4); on first boot, or from
`bmac-backupd init` on a package install (disk selection, LUKS, pool,
Tailscale, certificate), the appliance presents the token and its client
certificate to `POST /v1/enroll` (M2.9); the administrator approves the
pending request in the Dashboard, which records the certificate fingerprint
in pmxcfs for `bmac-source`; revoking removes it, and `bmac-source` refuses
that appliance from the next connection on.

### M9.4 Restore

**Builds:** restore to a new VM, restore into the existing VM (restored
disks swapped in, the current ones set aside with their snapshots and
bookmarks), and in-place rollback where allowed, each volume by the cheapest
method (`BACKUP.md`, "Moving only what's needed") and made as "How a
restored volume is made" says (received under a staging name, renamed into
the storage for its block size, attached to a VM created without disks),
with the method, bytes, and time estimate shown before it starts; backups
that continue from the restored PIT (`BACKUP.md`, "Backups continue from the
restored PIT": the new branch's parent, its base bookmarked on every
placement host, appliances holding granted PITs, and the wizard showing
which appliances continue incrementally); promotion
of a restored VM to production (`BACKUP.md` "Promotion": fence, registry
transaction, identity transfer, replication convergence, HA, new branch;
refused without quorum); manual restore tests; and recovery of BMAC after
losing the whole cluster (`BACKUP.md`). Restores are only started by hand;
there is no restore schedule.

**From v2:** `add_staging_vm.sh`'s clone, identity patch, and mount policy
are the model for restoring as a new VM without colliding with production
(new MAC, hostname, machine ID; network disconnected until the operator
chooses).

### M9.5 Dashboard backup pages

**Builds:** the Backups page: policies per VM with the appliance
assignment, the PIT graph (`BACKUP.md`, "PIT graph"; time top to bottom,
oldest PIT at the top left, branches to the right), appliance states and check-in times, live and
historical transfer rates, local retention space, and the restore wizard.
Ignore BACKUP.md if it says to do the graph with time left-to-right,
since browsers and UIs are more naturally arranged vertically it makes more
sense to arrange the graph timeline vertically from top-to-bottom with branches
extending to the right.


### M9.6 Lab proofs and acceptance

**Builds:** the remaining lab proofs in `BACKUP.md` ("To prove in the
lab"), each with its fallback wired in until it passes, then the acceptance
scenario run continuously for a week.

**M9 done when:** the acceptance scenario passes in the lab, including a
whole-cluster loss recovered from appliance B.

---

## M10. Packaging, upgrades, releases

### M10.1 Packages

**Builds:** signed `.deb` packages: `bmacd` (with the embedded web UI and
the legacy runtime of M4.4), `bmac-backupd`, and the Deployer (Tauri
`.deb`, amd64 Linux only, as `DEPLOYMENT.md` says). A signing key kept offline; release
manifests with hashes, packed with the packages into one release bundle;
reproducible builds checked by building twice and comparing. A development
signing key, trusted only by builds with the `dev-signing` feature (the lab
enables it), signs the bundles used to test upgrades.

**From v2:** the Tauri bundle configuration and `.deb` build in
`dashboard/`, and the release steps in `dev/README.md`.

### M10.2 Cluster update job

**Builds:** the update job (`DEPLOYMENT.md`, "Upgrades"): take a release
bundle (signed manifest plus packages) uploaded through the Dashboard or
fetched from a URL the administrator gives, stage it on the receiving host
and copy it to the others over 8444, verify the manifest signature and
package hashes, check every host's version and the schema versions,
update one host at a time (migrating HA resources away first when the
operator asks), verify `bmacd` health and peer streams after each, and stop
at the first failure with the remaining hosts untouched. Each host keeps
its previous packages cached, and the job offers to roll the upgraded hosts
back to them. Mixed versions are allowed only for the
duration of an update, and a newer schema version refuses older engines.

**From v2:** `update_cluster_runtime.sh`: staged files, hash checks, `bash
-n`, one host at a time, rollback on failure, and the order tests
(`test_update_cluster_runtime.py`).

**M10 done when:** the lab updates a three-host cluster from one build to
the next with a production VM running, and an injected failure on the
second host stops the update with the first host on the new version and the
third untouched, after which rollback returns the first two hosts to the
previous build.

---

## M11. Native ports

Each script operation is replaced by native Rust only after it passes every
step of the port gate in `WORKFLOW_MIGRATION.md` (checklist in
`v3/design/workflows/<id>.md`, Rust tests, comparison with intended
differences recorded, lab, hardware where required, review). The order is
`WORKFLOW_MIGRATION.md`'s "Porting order", with the v2 lessons each port
must keep:

1. **Registry writes** (`cluster_registry.py` → `bmac-registry`): schemas
   and validation, the lock, atomic writes, leases (M2.6), and
   `reconcile --live`. The old path and command-line interface stay
   supported, so the remaining scripts and the hook keep working and v2's
   registry tests run against the Rust binary unchanged.
2. **`prod.change_owner`**: relocate, then wait for the reversed
   replication jobs.
3. **`prod.change_placement`**: registry first when adding a host, HA first
   when removing one; replication jobs created on the source, with a new
   job's baseline at 0.
4. **`staging.remove`**, **`prod.remove`**.
5. **`staging.add`**: snapshot naming, GUID records, the identity patch,
   the mount policy, rollback on failure.
6. **`prod.extend_disk`**: re-sync replication until the new size shows on
   every replica.
7. **`prod.add`**: phases keyed by purpose for resume; never destroy on
   failure; keep the ISO after an ambiguous `qm create`; HA added as
   `ignored`, then the affinity rule, then `started`; `failback 0`,
   `max_restart 3`, `max_relocate 1`; strict node affinity.
8. **Host setup phases 4–6** (M7.3), **`hosts.remove`**,
   **`qdevice.add`**, **`qdevice.remove`**: graceful or forced removal;
   QDevice parity; clearing a stale QDevice.
9. **Ingress sync** (`haproxy_routes.py`, `sync_haproxy_routes.sh`): the
   eight-step generation protocol, the coordinator check, and routes synced
   before strict ingress; **deferred cleanup**
   (`process_deferred_cleanup.sh`): its order and timer.
10. **The lifecycle hook**, last: the pre-start lock and 900-second
    reservation; the stop phases lock-free to avoid deadlock; validate
    everything before stopping anything.

Each port deletes its script operation in the same change, and the
`legacy` tests that covered it move to the Rust test suite or to
`NOT_APPLICABLE.md` with a reason.

**M11 done when:** no script operation remains, or the ones left are listed
with a reason to keep them (v3.0 may ship with some script operations).

---

## M12. Release readiness

### M12.1 Hardware runs

On the real Dell hosts: the full Deployer path with iDRAC, a USB install on
one host, LUKS on every host, boot drills, a raidz2 host, disk replacement
by pulling a drive, failover drills, the backup acceptance scenario with
two appliances, and the update job.

### M12.2 Security review

A review of `SECURITY.md` against the code: process separation, the
front end's sandbox, the token never stored in the browser, Origin and CSP,
secret sweeps of logs, job records, pmxcfs, and ISOs, the Deployer's
certificate check, and the TLS configuration of every listener.

### M12.3 Documentation

`v3/README.md` brought up to date, an operator guide (install, first host,
additional hosts, daily use, disks, backups, restore, recovery, updates),
and the design documents corrected wherever the implementation differs.

**M12 done when:** every item above is done, and v3.0 is tagged.
