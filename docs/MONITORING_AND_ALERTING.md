<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v2 Monitoring and Alerting

Status: implemented in v2 (see [Implementation notes](#implementation-notes)
for where the implementation differs from this design). The operator-facing
guide is the README's "Monitoring and Alerts" section; `docs/MAIN_DESIGN.md`
("Monitoring and alerting") records the built behavior.

### Implementation notes

The implementation follows this design except for these deliberate
differences:

- The QDevice's monitoring node is always named `qdevice` (its check is
  `<cluster> bmaccheck qdevice`), whatever `PROXMOX_QDEVICE_HOST` is, so the
  check survives a change of the SSH alias.
- Monitoring keys (`MONITORING_ENABLED`, `MONITOR_*`, `HEALTHCHECKS_*`) are
  excluded from `CONFIG_EFFECTIVE_SHA256`, so changing them neither
  invalidates host-setup checkpoints nor shows as configuration drift.
- `update_cluster_runtime.sh`'s host commit step installs only the agent
  program (as a runtime file) and try-restarts `app-ha-monitor.service` where
  its unit exists; the unit and config are written by the monitoring phase
  (`scripts/lib/monitor_node_install.sh`), which has its own per-phase
  rollback. The unit text comes from `app_ha_monitor.py print-unit`.
- Membership workflows delete only the removed node's check
  (`monitoring_sync --delete-node`). `update_cluster_runtime.sh` lists every
  obsolete check and deletes them only after a yes/no prompt, or with
  `--delete-obsolete-checks`.
- `add_proxmox_host.sh`, `add_prod_vm.sh`, and `update_cluster_runtime.sh`
  stop before changing anything when monitoring is enabled and the QDevice is
  unreachable; removal workflows only warn, so they can still recover from
  failures.
- `send_test_alert.sh` is a `mutating` workflow (it sends email); it is listed
  with the diagnostics. `show_monitoring_state.sh --reset-smart-baseline` is
  terminal-only, since the dashboard tile is read-only.
- The guest unit's `ExecCondition` uses `$$(hostname)`, because systemd would
  otherwise treat `$` as a variable reference.
- `reset-smart-baseline` writes a request file under
  `/var/lib/app-ha-monitor/requests/`; the running agent applies it on its next
  cycle and reruns its slow checks, so the agent and the command never race on
  the state file. Ping sequences are serialized across processes with a lock
  file, so `test-pulse` never interleaves with the service's pings.
- The agent's `AGENT_VERSION` is stamped by `dev/set_version.py` from
  `dev/VERSION`.
- The example app's `/api/v1/healthz` reads the todos table (a count) rather
  than `SELECT 1`, so a missing table also fails it.
- `wait-ping` identifies checks by name (never by UUID on a command line) and
  passes when every named check has left `new` and received a ping since the
  install started.
- `manage_qdevice_probes.sh` adds `--yes` and `--save-if-probe-fails` for
  non-interactive use, and `manage_prod_vm_monitoring.sh` adds `--yes`.

This document is the source material for implementing monitoring and alerting
in the **v2** codebase (everything in the repository outside `v3/`). It records
what was decided, why, the exact behavior each component must have, the files
to add or change, the tests to write, and how to roll the result out to an
already-deployed cluster without rebuilding it.

## Contents

1. [Instructions for the implementer](#1-instructions-for-the-implementer)
2. [Goals and non-goals](#2-goals-and-non-goals)
3. [The alerting model](#3-the-alerting-model)
4. [Components and where they run](#4-components-and-where-they-run)
5. [The pulse protocol](#5-the-pulse-protocol)
6. [Checks performed by each node role](#6-checks-performed-by-each-node-role)
7. [Debounce, hysteresis, priorities, and reminders](#7-debounce-hysteresis-priorities-and-reminders)
8. [The monitor agent (`app_ha_monitor.py`)](#8-the-monitor-agent-app_ha_monitorpy)
9. [Configuration, secrets, and the registry](#9-configuration-secrets-and-the-registry)
10. [Healthchecks.io integration](#10-healthchecksio-integration)
11. [Deployment and the runtime update](#11-deployment-and-the-runtime-update)
12. [Membership changes](#12-membership-changes)
13. [Workflows, scripts, and the Dashboard](#13-workflows-scripts-and-the-dashboard)
14. [The example app's health endpoint](#14-the-example-apps-health-endpoint)
15. [Things outside BMAC (README material)](#15-things-outside-bmac-readme-material)
16. [Interpreting alerts](#16-interpreting-alerts)
17. [Security](#17-security)
18. [Tests](#18-tests)
19. [Facts to verify before or during implementation](#19-facts-to-verify-before-or-during-implementation)
20. [Rollout to the existing cluster](#20-rollout-to-the-existing-cluster)
21. [Implementation order and acceptance criteria](#21-implementation-order-and-acceptance-criteria)
22. [Decision log](#22-decision-log)

---

## 1. Instructions for the implementer

Read these before writing code.

- **Do not change anything under `v3/`.** It may be read for reference
  (`v3/design/MONITORING.md` describes a different, Postmark-based design that
  this one deliberately replaces for v2).
- **Follow `AGENTS.md`.** Run tests with `dev/run_tests.py`, run the whole
  suite before reporting work as finished, run `bash -n` on every changed shell
  script, keep tests independent (per-test temporary directories, no
  `setUpClass`/`setUpModule`, fake commands fed by a pipe must read stdin).
- **Match existing conventions.** Look at how `egress_health.py`,
  `update_cluster_runtime.sh`, `show_cluster_health.sh`, `scripts/lib/config.sh`,
  `scripts/lib/ui_protocol.sh`, and `dashboard/engine/workflows.json` are written
  and follow the same patterns: `set -Eeuo pipefail`, `set +x`, `umask 077`,
  copyright headers, `--json` UI protocol support, `--dry-run` where a script
  mutates, plan/confirm before changes, typed confirmations for destructive
  actions, next steps on completion.
- **Python on nodes is stdlib-only.** The agent runs on Proxmox hosts (Debian),
  the QDevice (Ubuntu), and production VMs (Ubuntu). Use `urllib.request`,
  `json`, `subprocess`, `ssl`, `socket`; no third-party packages. The QDevice
  and prod1 ran Python 3.14.4 (Ubuntu 26.04.1) and the Proxmox hosts Python
  3.13.5 (Debian) when captured, on a test cluster that had not been
  upgraded. These versions will change with `apt upgrade` and OS releases, so
  never depend on an exact version: target Python 3.11 as the minimum (avoid
  newer language features and standard-library APIs), and have
  `validate-config` and the deploy step fail clearly if a node's `python3` is
  older than 3.11.
- **Reuse, don't duplicate, parsing logic.** `show_cluster_health.sh` already
  parses `corosync-cfgtool -n` link status, `zpool status -x`, rpool free space
  (`RPOOL_MIN_FREE_FRACTION = 0.10`), and `smartctl -H`. Move the shared parsing
  into the agent module (or a small shared module) and make
  `show_cluster_health.sh` use it, so both agree. Keep the existing diagnostic's
  output unchanged except where it reads better.
- **Never put secrets in pmxcfs.** Healthchecks ping URLs and the API key are
  secrets (see [§17](#17-security)).
- **Two config entries already exist.** The operator has already added
  `MONITORING_ENABLED` (with a comment) to `config/cluster.conf` and
  `config/cluster_dot_conf`, and `HEALTHCHECKS_API_KEY` to `config/secrets.env`
  and `config/secrets_dot_env` (value redacted in the example). Do not add them
  again; add the validation and loading for them to `scripts/lib/config.sh`.
  Every other monitoring setting is optional and must **not** be added to
  `cluster_dot_conf` (see [§9](#9-configuration-secrets-and-the-registry)).
- **Collectors check their own node only and never SSH anywhere.** An agent
  never logs in to another node. It may only observe peers through local tools
  (corosync status, qnetd status) and network probes (ICMP over Tailscale,
  HTTPS through Cloudflare).
- **Where this document says "verify",** a fact was not confirmed against a
  live system or current vendor docs. Section [19](#19-facts-to-verify-before-or-during-implementation)
  lists them and how to capture real fixtures. Ask the operator to run the
  capture commands on the live cluster and build test fixtures from the real
  output rather than guessing formats.
- **Update the docs that describe changed behavior:** `README.md` (new
  "Monitoring and Alerts" section, Cloudflare setup, runtime-update section),
  `docs/MAIN_DESIGN.md` (runtime files, registry schema, workflows, security
  invariants, failure cases), `dashboard/README.md` if workflows change, and
  move this file to `docs/MONITORING_AND_ALERTING.md`.

## 2. Goals and non-goals

### Goals

Alert the operator, by email (and optionally push via Pushover), when any of
these happens:

| # | Condition | Detected by |
|---|---|---|
| G1 | A Proxmox host, the QDevice, or a production VM stops working or loses its internet connection | Healthchecks dead-man switch (node goes silent) |
| G2 | Cluster nodes can no longer reach each other over Tailscale | Host agents (corosync link1), QDevice agent (ICMP over Tailscale, qnetd connections) |
| G3 | Proxmox hosts can no longer reach each other over the private VLAN | Host agents (corosync link0) |
| G4 | A Proxmox host loses its public internet connection | G1 (the host goes silent) plus peers reporting its Tailscale link down while its VLAN link is up |
| G5 | Production apps cannot be reached through the Cloudflare Tunnel | QDevice ingress probes; Cloudflare's own tunnel-health email |
| G6 | SMART warnings or failures on any host disk | Host agents (and, separately, Proxmox's smartd email) |
| G7 | ZFS faults or disk failures | Host agents (and, separately, Proxmox's ZED email) |
| G8 | A ZFS pool has less than 10% free space | Host agents |
| G9 | Proxmox's own alert email cannot leave a host | Host agents (stuck Postfix queue) |
| G10 | A monitored service on a production VM stops, its disk fills, or its origin certificate nears expiry | Production VM agents |
| G11 | The cluster loses the QDevice's vote while it needs it, or loses quorum | Host agents |

Additional requirements:

- **Zero recurring cost.** Healthchecks.io's free Hobbyist plan (20 checks),
  email, Cloudflare's free tunnel-health alert, and optionally Pushover
  (one-time per-platform purchase) are the only services used.
- **No inbound public ports anywhere.** Nothing receives webhooks; every
  component only makes outbound requests.
- **One Healthchecks check per monitored node.** Check budget is
  `hosts + 1 (QDevice) + production VMs <= 20`.
- **Deployable to an existing cluster through the runtime update.** No rebuild.
- **Monitoring always follows membership.** Adding or removing a host, the
  QDevice, or a production VM updates every agent and the Healthchecks checks.
- **Staging VMs are never monitored.**

### Non-goals

- No ntfy, Gotify, Postmark, SMTP relay, or self-hosted notification server.
- No metrics storage, dashboards of time series, or Prometheus/Grafana.
- No webhook receivers (Cloudflare and Tailscale webhooks are not used).
- No monitoring of staging VMs.
- No changes to v3.

## 3. The alerting model

Every monitored node runs one agent, `app_ha_monitor.py`, bound to that node's
own Healthchecks.io check. The check carries two kinds of signal:

1. **Liveness (dead-man switch).** The agent sends a plain success ping every
   60 seconds. If the node dies, or loses its route to the internet, the pings
   stop and Healthchecks emails "DOWN" after the period plus grace time. That
   email has no BMAC marker tokens.
2. **Events (pulses).** When the agent finds a problem (about itself or about
   something it can observe, such as a peer's link or a prod app's ingress), it
   sends a `/fail` ping whose body describes the problem, immediately followed
   by a success ping whose body contains the clear token. Healthchecks emails
   "DOWN" (with the event body) and then "UP" (with the clear body). Gmail
   filters forward the DOWN-with-event email to Pushover and delete the
   clearing UP email.

Because every event is immediately cleared, the check is "up" almost all the
time, so the dead-man switch keeps working even while problems are
outstanding. This was verified on Healthchecks.io: a `/fail` followed
immediately by a success produces both emails, and both emails include the ping
body. A second `/fail` while the check is already down sends no email, which is
why every pulse is cleared at once.

Delivery chain:

```
agent ──HTTPS──▶ Healthchecks.io ──email──▶ Workspace Gmail ──filters──▶ inbox / Pushover / deleted
Cloudflare tunnel health ─────────email──▶ Workspace Gmail
Proxmox (smartd, ZED, fencing, replication) ──Postfix direct-to-MX──▶ Workspace Gmail
```

Proxmox's own email is kept as an independent second path. It is not changed by
this work.

## 4. Components and where they run

| Component | Runs on | Installed by | Purpose |
|---|---|---|---|
| `app_ha_monitor.py` (role `host`) | every Proxmox host | runtime update; host setup | links, quorum, disks, pools, cloudflared, Postfix queue, liveness |
| `app_ha_monitor.py` (role `qdevice`) | the QDevice VPS (always, whether or not it is currently voting) | runtime update; QDevice setup | Tailscale ICMP to hosts, qnetd connections, ingress probes, liveness |
| `app_ha_monitor.py` (role `guest`) | every production VM | runtime update; `add_prod_vm.sh` | services, guest disk, certificate expiry, liveness (proves guest egress) |
| `app-ha-monitor.service` | all of the above | same | systemd unit running the agent |
| `scripts/lib/healthchecks.py` | workstation | n/a | Healthchecks Management API client: create/update/delete checks, read status |
| `scripts/lib/monitoring.sh` | workstation | n/a | shared functions: render node configs, sync checks, deploy agents |
| `scripts/user_callable/diagnostics/show_monitoring_state.sh` | workstation | n/a | read-only: every check's status and last ping, every agent's state |
| `scripts/user_callable/guests/prod/manage_prod_vm_monitoring.sh` | workstation | n/a | view/update a prod VM's monitored services and certificate paths (Production page tile) |
| `scripts/user_callable/qdevice/manage_qdevice_probes.sh` | workstation | n/a | view/update the ingress health-check probes the QDevice runs for each prod VM (QDevice page tile) |
| `scripts/user_callable/diagnostics/send_test_alert.sh` | workstation | n/a | make one node send a test pulse, to test the email/filter/Pushover chain |

The QDevice is **required** for monitoring: it is the only node outside the
FiberState network and the only one that can test ingress end to end. When
monitoring is enabled and the QDevice is not reachable over `ssh
$PROXMOX_QDEVICE_HOST`, the runtime update and the membership workflows must
stop with a clear error.

The QDevice runs the agent even when the cluster has an odd number of hosts and
the QDevice is on standby (not running qnetd for the cluster). In that state it
still does ICMP and ingress checks; it skips the qnetd check.

## 5. The pulse protocol

### Marker tokens

Filter terms are single lowercase words without spaces. Gmail search ignores
most punctuation, so tokens must not contain brackets, colons, or spaces.

| Token | Meaning | Appears in |
|---|---|---|
| `bmaccheck` | this email is about a BMAC check | every Healthchecks email about a BMAC check, because it is part of every check's name ([§10](#10-healthchecksio-integration)) and Healthchecks puts the check name in each email's subject and body (verified) |
| `bmacevent` | this ping reports an event | the body of every pulse `/fail` |
| `bmacp1` | high priority event | pulse `/fail` body, with `bmacevent` |
| `bmacp2` | normal priority event | pulse `/fail` body, with `bmacevent` |
| `bmacclear` | this success only clears a pulse | the body of every clearing success ping |

**Hard rule:** no other ping body may contain any token. The agent must
sanitize every message string (problem text, hostnames, unit names, file paths,
command output excerpts) by replacing any case-insensitive occurrence of
`bmacevent`, `bmacclear`, `bmacp1`, `bmacp2` (and the prefix `bmac` followed by
those suffixes) with a neutralized form such as `bmac-event`. Routine heartbeat
bodies never contain tokens. `bmaccheck` needs no sanitizing; it may appear
anywhere.

`bmacp1`/`bmacp2` are for operators who want separate Pushover priorities
([§15.4](#154-gmail-filters)); the basic filter set does not use them.

### Sequences

Event pulse:

```
POST <ping_url>/fail      body: event body (tokens on line 1)
POST <ping_url>           body: clear body ("bmacclear" on line 1)
POST <ping_url>           body: heartbeat body (no tokens)
```

The trailing plain heartbeat makes sure the most recent ping never carries a
token. This matters because Healthchecks' dead-man DOWN email includes the
**last** ping's body (confirmed: after a `/fail`, a clear, and a plain
heartbeat, the dead-man email showed only `plain heartbeat` under "Last Ping
Body", and no earlier bodies). Without the trailing heartbeat, a node that
died right after a pulse would produce a DOWN email containing `bmacclear`.
Gmail filters also use the subject as a second guard (see
[§15.4](#154-gmail-filters)).

Routine heartbeat (every 60 seconds):

```
POST <ping_url>           body: heartbeat body (no tokens)
```

### Body formats

Event body (UTF-8 text, at most 8,000 bytes; Healthchecks stores up to 10 kB):

```
bmacevent bmacp1
BMAC event cluster=MyAppCloud node=mox1 role=host pulse=3f9c2a1b
time=2026-10-23T14:02:11Z agent=<version>

NEW
  [p1] zfs:rpool         pool rpool is DEGRADED (mirror-0: /dev/disk/by-id/ata-XYZ FAULTED)
  [p2] link0:mox2        corosync link0 (private VLAN) to mox2 DOWN for 95s
RESOLVED
  [p2] link1:mox3        corosync link1 (Tailscale) to mox3 is up again (was down 14m)
ONGOING
  [p2] capacity:rpool    rpool 8.7% free (1.2 TiB of 13.9 TiB), below 10%
```

- Line 1 is exactly `bmacevent bmacp1` or `bmacevent bmacp2`. The pulse
  priority is the highest priority among NEW and REMINDER items; a pulse
  containing only RESOLVED items is `bmacp2`.
- Sections appear in the order `NEW`, `REMINDER`, `RESOLVED`, `ONGOING`; empty
  sections are omitted. `ONGOING` lists every other open problem so each email
  is a complete current picture.
- Each item line: priority, problem key, message.
- If the body would exceed the limit, truncate ONGOING first, then RESOLVED,
  and end with `(N more items omitted; see show_monitoring_state.sh)`.

Clear body:

```
bmacclear
BMAC clear cluster=MyAppCloud node=mox1 pulse=3f9c2a1b
```

Heartbeat body:

```
BMAC heartbeat cluster=MyAppCloud node=mox1 role=host
time=2026-10-23T14:03:11Z agent=<version> open_problems=2
```

The heartbeat names open problem counts but never their text, to stay short.
A node coming back after a dead-man alert therefore produces an "UP" email whose
body says it is a heartbeat, which the operator's filters keep.

### Problem keys

Problem keys are stable strings so state can be diffed between cycles:

| Key | Role | Example |
|---|---|---|
| `link0:<peer>` | host | `link0:mox2` |
| `link1:<peer>` | host | `link1:mox3` |
| `corosync-peer:<peer>` | host | peer missing from `corosync-cfgtool -n` |
| `quorum` | host | cluster not quorate |
| `votes` | host | total votes below expected votes |
| `qdevice-vote` | host | QDevice vote expected but absent |
| `cloudflared` | host | this host's tunnel replica not ready |
| `postfix-queue` | host | mail older than the limit waiting in the queue |
| `zfs:<pool>` | host | `zpool status -x` reports a problem |
| `capacity:<pool>` | host | pool free space below threshold |
| `smart:<serial>` | host | SMART health failed or a watched counter rose |
| `nvme-warning:<serial>` | host | NVMe `critical_warning` non-zero |
| `nvme-spare:<serial>` | host | NVMe available spare at or near its threshold |
| `nvme-wear:<serial>` | host | NVMe `percentage_used` at or above the warning level |
| `smart-read:<serial>` | host | `smartctl` failed or timed out on a disk |
| `tailscale:<host>` | qdevice | ICMP over Tailscale to a host fails |
| `qnetd:<host>` | qdevice | host not connected to qnetd while the QDevice is registered |
| `qnetd:service` | qdevice | `corosync-qnetd` is not running, or `corosync-qnetd-tool -l` fails, while the QDevice is registered |
| `ingress:<fqdn>` | qdevice | probe failed |
| `ingress-challenged:<fqdn>` | qdevice | Cloudflare returned a challenge instead of the app |
| `service:<unit>` | guest | monitored unit not active |
| `disk:<mountpoint>` | guest (and qdevice) | filesystem free space below threshold |
| `cert:<path>` | guest | certificate expires soon, has expired, or cannot be read |
| `agent:<check>` | any | a check itself crashed (exception) |

`<serial>` is the drive serial number from `smartctl -j` output, so the key
survives device renumbering and identifies the physical drive to replace.

### Delivery reliability

- Use `urllib.request` with a 10-second timeout. Retry each request up to 3
  times with 2, 5, and 10 second waits.
- Keep a `pending_clear` flag in state. Set it before sending `/fail`. Clear it
  only after the clearing success returns HTTP 200. While it is set, every
  heartbeat is sent as a clear body instead, and then a plain heartbeat
  follows, until a clear succeeds.
- If the `/fail` cannot be delivered after retries, keep the NEW items queued
  and include them in the next pulse attempt (every cycle) until delivered.
  Delivered-or-not is tracked per item, so nothing is announced twice and
  nothing is lost.
- Rate limit: at most one pulse per 60 seconds per node, and at most 12 pulses
  per hour per node. Changes found while rate-limited are merged into the next
  pulse. When the hourly limit is hit, the next pulse includes the line
  `pulse rate limit reached; some changes were merged`.

## 6. Checks performed by each node role

Fast checks run every `MONITOR_FAST_INTERVAL_SECONDS` (default 30). Slow checks
run at agent start and then every `MONITOR_SLOW_INTERVAL_SECONDS` (default
1800). The liveness heartbeat is sent every `MONITOR_ALIVE_PERIOD_SECONDS`
(default 60), independent of the check schedule. Every check has its own
timeout and runs in a way that one hung command cannot stop the heartbeat
(see [§8](#8-the-monitor-agent-app_ha_monitorpy)).

### 6.1 Proxmox host (`role: host`)

| Check | Speed | How | Problem when | Priority |
|---|---|---|---|---|
| Corosync links to each expected peer | fast | `corosync-cfgtool -n` (reuse `parse_links` from `show_cluster_health.sh`) | link0 (VLAN) or link1 (Tailscale) not connected, or an expected peer is not listed | p2 per link; p1 if both links to the same peer are down |
| Quorum | fast | `corosync-quorumtool -s` (it lists members by name; `pvecm status` lists them by IP) | the `Quorate:` line is not `Yes` | p1 |
| Missing votes | fast | same output | `Total votes` is less than `Expected votes`, and the shortfall is not explained by the QDevice vote check below | p2 (the link checks usually explain which member) |
| QDevice vote | fast | same output, "Membership information" (see the three captured shapes in [§19.1](#191-confirmed-shapes)); detail from `corosync-qdevice-tool -s` | only when the cluster has a registered QDevice: the `Qdevice` member line is missing, or its votes column is `0` (shown as `0  0  Qdevice (votes 1)` when qnetd is unreachable), or a host's Qdevice column contains `NV` (not voting) instead of `V`. Include the `State:` line from `corosync-qdevice-tool -s` in the message (`Connected` when healthy, `Connect failed` when qnetd is unreachable). Do **not** use the `Flags:` line: it still says `Quorate Qdevice` while the vote is lost | p1 |
| Own tunnel replica | fast | GET `http://127.0.0.1:20241/ready` (confirmed: HTTP 200 with JSON `{"status":200,"readyConnections":4,"connectorId":"..."}`; connection refused, curl exit 7, while `app-ha-cloudflared.service` is stopped) | connection refused, not HTTP 200, or `readyConnections` is 0. Ignore `connectorId`; it changes on every restart | p2 |
| Postfix queue | fast (cheap) | `postqueue -j` (one JSON object per line; empty output when the queue is empty; shape confirmed in [§19.1](#191-confirmed-shapes)) | any message whose `arrival_time` (Unix seconds) is older than `MONITOR_POSTFIX_QUEUE_MAX_AGE_MINUTES` (default 60); the message gives the count, the oldest age, the `queue_name`, the recipient **domain** only, and the first recipient's `delay_reason` | p2 |
| ZFS health | slow | `zpool status -x` per pool, plus `zpool status -P -p` for detail | anything but "is healthy" | p1 |
| Pool capacity | slow | `zfs list -H -p -o used,available <pool>` for every pool from `zpool list -H -o name` | free fraction below `MONITOR_POOL_MIN_FREE_PERCENT` (10); clears above `MONITOR_POOL_CLEAR_FREE_PERCENT` (12) | p2; p1 below 5% |
| SMART health | slow | list drives with `smartctl --scan` (or `smartctl --scan -j`), then `smartctl -j -i -H -A <device>` for each; `-i` is required because `-H -A` alone does not return `serial_number` or `model_name` (confirmed) | `smart_status.passed` is false | p1 |
| NVMe critical warning | slow | same JSON, `nvme_smart_health_information_log.critical_warning` | non-zero; decode the bits into the message (bit 0 available spare below threshold, bit 1 temperature out of range, bit 2 reliability degraded, bit 3 read-only mode, bit 4 volatile memory backup failed) | p1 |
| NVMe spare capacity | slow | same JSON, `available_spare` and `available_spare_threshold` (both percentages) | `available_spare` is at or below `available_spare_threshold` | p1 |
| NVMe spare capacity early warning | slow | same | `available_spare` is at or below `available_spare_threshold + 10` (but still above the threshold) | p2 |
| NVMe wear | slow | same JSON, `percentage_used` (may exceed 100) | `percentage_used` is at or above `MONITOR_NVME_WEAR_WARN_PERCENT` (default 90); escalates to p1 at 100 or more | p2; p1 at 100+ |
| NVMe media errors | slow | same JSON, `media_errors` | increased since the stored baseline | p2 |
| ATA SMART counters | slow | same JSON, `ata_smart_attributes.table` | raw value increased since the stored baseline for attribute 5 (Reallocated_Sector_Ct), 197 (Current_Pending_Sector), or 198 (Offline_Uncorrectable) | p2 |
| SMART readability | slow | same | `smartctl` exits with an error bit that means it could not read the disk, or times out (30s) | p2 |

Notes:

- Expected peers come from the agent config, rendered from the cluster
  membership (see [§9](#9-configuration-secrets-and-the-registry)), not from
  corosync's own view, so a peer that never joined or vanished is reported.
- The host does not check its own public internet connection directly: if it
  is lost, the host cannot reach Healthchecks and the dead-man switch fires.
  The other hosts report `link1:<host>` down while `link0:<host>` stays up,
  which together identify the cause ([§16](#16-interpreting-alerts)).
- The Proxmox disk checks deliberately duplicate smartd and ZED. They are the
  path that does not depend on the host's Postfix direct delivery.
- SMART counter baselines are stored in state at first read. A counter that
  rises is reported once (NEW), stays ONGOING until the operator resets the
  baseline (`show_monitoring_state.sh --reset-smart-baseline <host>:<disk>`,
  or automatically when the disk serial disappears after replacement).
- The operator's hosts use NVMe drives only (confirmed on mox1: two Intel
  SSDPE2KX020T7/T8 2 TB drives; smartd reports "Monitoring 0 ATA/SATA, 0
  SCSI/SAS and 2 NVMe devices"). The NVMe rows above are therefore the ones
  that matter in practice; the ATA rows exist for other BMAC users and must be
  covered by tests too. These drives do not support NVMe self-tests, so the
  agent never schedules or reads self-test results.
- Identify each disk by the `serial_number` in `smartctl -j -i` output (with
  `model_name` as context in messages), not by device name. Problem keys are
  `smart:<serial>`, `nvme-warning:<serial>`, `nvme-spare:<serial>`,
  `nvme-wear:<serial>`, and `smart-read:<serial>`. `smartctl --scan` lists
  NVMe controllers (`/dev/nvme0 -d nvme`), one per physical drive; still
  deduplicate by serial in case a drive appears twice.
- `smartctl` without a device name exits 1 with an error message (confirmed);
  always pass the device. Use `smartctl`'s exit status bits: bit 1 (device
  open failed) or bit 2 (command failed) means the drive could not be read
  (`smart-read:<serial>`, or `smart-read:<device>` when no serial is known);
  bit 3 (disk failing) agrees with `smart_status.passed` being false.
- `percentage_used` is the drive's own estimate of how much of its rated
  endurance has been consumed. It can pass 100; the drive keeps working, but
  should be replaced on a planned schedule. Include the current value, the
  `data_units_written` converted to TB (1 data unit = 512,000 bytes), and the
  `power_on_hours` in the message so the operator can judge urgency.
- Do not alert on `num_err_log_entries`. Several drive models increment it for
  benign events (for example, unsupported admin commands issued by tools),
  which would cause false alerts.
- The wear and spare checks do not need a baseline. They are threshold checks
  evaluated on every slow run, so a newly added drive that is already worn is
  reported on the agent's first run.

### 6.2 QDevice (`role: qdevice`)

| Check | Speed | How | Problem when | Priority |
|---|---|---|---|---|
| Tailscale reachability of each host | fast | from `tailscale status --json`, take the `Peer` entries whose `Tags` contain `tag:proxmox-host` and whose `HostName` equals the host's `TAILSCALE_HOSTNAME`; use the IPv4 address in `TailscaleIPs`; then `ping -c 1 -W 3 <ip>` (confirmed working with the ICMP rule) | ping fails, or the expected host is not among the peers | p2; p1 if every host fails at once (QDevice-side Tailscale problem or whole-site outage) |
| qnetd connections | fast | `corosync-qnetd-tool -l` (format confirmed; see [§19](#19-facts-to-verify-before-or-during-implementation)) | only when the QDevice is registered for the cluster: under `Cluster "<PROXMOX_CLUSTER_NAME>":`, an expected host's node ID has no `Node ID N:` block, or its `Vote:` line does not end in `(ACK)`; if the tool itself fails or `corosync-qnetd.service` is not active, report `qnetd:service` instead of per-host problems (confirmed shape: with qnetd stopped, `systemctl is-active corosync-qnetd` prints `inactive`, and the tool prints "Can't connect to qnetd socket (is QNetd running?): No such file or directory" and exits 3) | p1 if the QDevice vote is currently needed (even host count), else p2 |
| Ingress probes | fast (every 60s, not 30s) | for each probe in config: HTTPS GET `https://<fqdn><path>`, timeout 10s, no redirects followed, header `User-Agent: bmac-monitor/<version>`, `Cache-Control: no-cache` | status differs from expected, or the optional expected text is not in the body, or connection/TLS error | p1 |
| Challenge detection | with probes | response has header `cf-mitigated: challenge` (from Cloudflare's documentation; no challenge was observed in testing, see [§19.1](#191-confirmed-shapes)) | the probe was challenged, so the result is inconclusive | p2, key `ingress-challenged:<fqdn>` |
| Own disk | slow | `df -P` on real filesystems | below 10% free (clears above 12%) | p2 |

Notes from the captured output:

- The QDevice's `Peer` list also contains people's devices (workstations,
  laptops). Filter by tag and host name; never treat a missing non-host peer
  as a problem.
- Which hosts to probe comes from BMAC's membership (the agent config), never
  from the Tailscale peer list. A machine that was removed from the cluster but
  is still in the tailnet (the capture shows one, mox3, which the operator had
  removed from the cluster but not yet from Tailscale) must be ignored.
- `Online` reflects whether a peer is connected to Tailscale's coordination
  server. Include it, and `LastSeen` when not the zero value, in the message of
  a failed ping as context; it is not a check of its own, because the ICMP
  ping is what shows whether traffic flows.
- `LastSeen` is `0001-01-01T00:00:00Z` while a peer is online; treat that value
  as "not applicable".
- A peer the QDevice has not exchanged traffic with yet has `LastHandshake`
  zero and `InEngine: false`. The first ICMP probe establishes the path; allow
  the first probe after agent start a longer timeout (10s).

The ICMP check relies on this Tailscale policy rule, which the operator has
already added:

```jsonc
// Monitoring: QDevice may ping hosts over Tailscale (no ports, no shell)
{
	"src": ["tag:proxmox-qdevice"],
	"dst": ["tag:proxmox-host"],
	"ip":  ["icmp:*"],
}
```

The README's "Instructions for Tailscale Setup" section must list this rule,
with the comment line, as a required part of the policy, next to the existing
QDevice rules.

### 6.3 Production VM (`role: guest`)

| Check | Speed | How | Problem when | Priority |
|---|---|---|---|---|
| Staging guard | every cycle | `socket.gethostname()` | hostname starts with `stage`: the agent sends nothing and does nothing else | n/a |
| Liveness | 60s | heartbeat | (the heartbeat reaching Healthchecks proves guest egress works) | n/a |
| Monitored services | fast | `systemctl is-active <unit>` for each unit in `/etc/app-ha-monitor/guest.json` `services` | not `active` | p1 |
| Guest filesystems | slow | `df -P -B1 -T`; include only filesystems whose source is a block device (`/dev/...`) and whose type is not one of tmpfs, devtmpfs, efivarfs, overlay, squashfs (on prod1 that leaves `/` (ext4) and `/boot/efi` (vfat); the many `none` tmpfs mounts under `/run/credentials/` are skipped) | below 10% free, clears above 12% | p2; p1 below 5% |
| Certificate expiry | slow | for each path in `cert_paths`: `openssl x509 -enddate -noout -in <path>` | expires within `MONITOR_CERT_WARN_DAYS` (14) | p2; p1 within 3 days or expired; p2 if unreadable |

The staging guard is in the agent, not in staging clone patching: staging VMs
are linked clones of production and inherit the agent and its config, and every
staging hostname starts with `stage`. Also add `ExecCondition=` to the unit
that exits non-zero for such hostnames, so the service does not even start on a
staging clone; the in-agent check covers renames while running.

If the guest's egress breaks, its heartbeats stop and the dead-man switch
fires, which is the intended signal.

## 7. Debounce, hysteresis, priorities, and reminders

### Which checks are debounced

| Check type | Fail after | Clear after |
|---|---|---|
| Fast connectivity checks (corosync links, quorum, qdevice vote, Tailscale ICMP, qnetd, ingress, cloudflared) | `MONITOR_FAST_FAIL_THRESHOLD` consecutive failures (default 3; about 90s at 30s intervals) | the same number of consecutive passes |
| Monitored guest services | 2 consecutive failures (allows a normal restart during a deploy) | 1 pass |
| Postfix queue age | 1 (the age threshold already debounces) | 1 pass |
| Slow checks: SMART, ZFS health, capacity, certificates, filesystems | **1: report on first detection, never wait for later runs** | 1 pass, except capacity hysteresis below |

### Capacity hysteresis

A pool or filesystem becomes a problem when free space falls below 10%. It is
only resolved when free space rises above 12%. Between 10% and 12% the current
state is kept. Escalation to p1 at 5% free is reported as a NEW p1 item.

### Priorities

- **p1** (`bmacp1`): immediate attention: something is down or data is at
  risk. Forwarded to the high-priority Pushover address.
- **p2** (`bmacp2`): needs attention soon; redundancy lost or a warning.
  Forwarded to the normal Pushover address.
- A node going silent (dead-man) has no tokens; the operator's filter treats it
  as high priority.

### Reminders

While a problem stays open, the agent includes it in a `REMINDER` section of a
pulse every `MONITOR_REMINDER_HOURS` (default 6) after its first announcement.
Reminders are combined into one pulse per node per reminder time.

Healthchecks' own hourly/daily reminders apply only while a check is down, so
they cover the dead-man case; enable them in the Healthchecks account settings.

### Startup behavior

- On first start (no state file), announce every open problem found by the
  first full run of all checks as NEW, in a single pulse.
- On a restart with a state file, do not re-announce known problems. Run all
  checks (including slow ones) immediately and diff against state.
- If the state file is unreadable or from an incompatible version, move it
  aside (`state.json.corrupt-<timestamp>`) and behave as a first start.

## 8. The monitor agent (`app_ha_monitor.py`)

### File and invocation

- Source: `scripts/host_runtime/app_ha_monitor.py` (single file, stdlib only,
  executable, copyright header, `--help` works without root and without
  config, because the runtime updater validates staged `.py` files by running
  `--help`).
- Subcommands:
  - `run` – the long-running loop used by systemd.
  - `check --once [--json]` – run every check once and print results; send
    nothing. Used by `show_monitoring_state.sh` and by tests.
  - `status [--json]` – print the state file in a readable form.
  - `test-pulse [--priority p1|p2]` – send one clearly labelled test pulse
    (`NEW [p2] test:alert  test alert requested from the workstation`), then
    clear it. Used by `send_test_alert.sh`.
  - `reset-smart-baseline <serial>`.
  - `validate-config` – load and validate the config, exit non-zero on error.
- Options: `--config PATH` (default `/etc/app-ha-monitor/config.json`),
  `--state-dir PATH` (default `/var/lib/app-ha-monitor`), `--dry-run-send`
  (log what would be sent instead of sending; used in tests).

### Structure

- One scheduler thread drives timing. Heartbeats are sent from their own
  thread so slow checks never delay liveness.
- Each check is a function `(config, state) -> list[Finding]` where a finding
  is `(key, ok: bool, priority, message, detail)`.
- Every external command runs through one helper with an explicit timeout and
  captured output (copy the `run()` helper style used in
  `show_cluster_health.sh`'s embedded Python). A timeout or exception becomes
  an `agent:<check>` finding, not a crash.
- A debounce/hysteresis layer turns raw findings into problem state
  transitions (`opened`, `resolved`, `escalated`).
- A pulse composer builds bodies, applies sanitization and truncation, and
  hands them to the sender.
- The sender implements [§5](#5-the-pulse-protocol) delivery rules.
- State is written atomically (temp file plus `os.replace`, mode 0600) after
  every transition and every send.

### State file (`/var/lib/app-ha-monitor/state.json`)

```json
{
  "schema_version": 1,
  "agent_version": "x.y.z",
  "problems": {
    "link0:mox2": {
      "priority": "p2",
      "message": "...",
      "first_seen": "2026-10-23T14:00:41Z",
      "announced": true,
      "last_reminder": null
    }
  },
  "debounce": {"link0:mox2": {"fails": 3, "passes": 0}},
  "smart_baselines": {"PHLJ9252002S2P0BGN": {"media_errors": 0}, "ATA-SERIAL": {"5": 0, "197": 0, "198": 0}},
  "outbox": [],
  "pending_clear": false,
  "pulse_times": ["2026-10-23T14:02:11Z"],
  "last_heartbeat_ok": "2026-10-23T14:03:11Z"
}
```

### Logging

Log to stdout/stderr (journald). One line per transition and per send (never
log the ping URL or its UUID; log `ping=<redacted>`). Do not log every passing
check.

### systemd unit (`app-ha-monitor.service`)

```ini
[Unit]
Description=BMAC monitor agent
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
ExecCondition=/bin/sh -c 'case "$(hostname)" in stage*) exit 1;; *) exit 0;; esac'
ExecStart=/usr/bin/python3 <install path>/app_ha_monitor.py run
Restart=always
RestartSec=10
User=root
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/app-ha-monitor
StateDirectory=app-ha-monitor
StateDirectoryMode=0700

[Install]
WantedBy=multi-user.target
```

- The `ExecCondition` line is only installed on production VMs.
- The agent runs as root because `smartctl`, `zpool`, corosync tools, and
  reading root-only certificate files require it. Verify that the sandboxing
  does not block `smartctl` device access, `corosync-cfgtool` (it talks to
  corosync over a local IPC socket), `postqueue`, or ICMP `ping`; relax only
  the specific setting that breaks, and test on a live node.
- Install paths:
  - Proxmox hosts: `/usr/local/lib/app-ha-proxmox/lib/app_ha_monitor.py`
    (alongside the existing runtime files, managed by
    `update_cluster_runtime.sh`).
  - QDevice and production VMs: `/usr/local/lib/app-ha-monitor/app_ha_monitor.py`.

### Required packages

- Hosts: `smartmontools`, `zfsutils-linux`, corosync tools, `postfix`
  (all present on Proxmox VE).
- QDevice: `iputils-ping`, `tailscale`, `corosync-qnetd` (only when registered),
  `python3`.
- Production VMs: `python3`, `openssl`.

The deploy step checks for missing tools and installs them with the existing
`apt_lock_wait.sh` pattern, or fails with a clear message.

## 9. Configuration, secrets, and the registry

### Required keys

| Key | File | Status |
|---|---|---|
| `MONITORING_ENABLED` | `cluster.conf` and `cluster_dot_conf` | already added by the operator, with a comment |
| `HEALTHCHECKS_API_KEY` | `secrets.env` and `secrets_dot_env` | already added by the operator |

`scripts/lib/config.sh` must:

- accept `MONITORING_ENABLED` with values `true`/`false`, defaulting to
  `false` when absent (older `cluster.conf` files do not have it);
- when it is `true`, require `HEALTHCHECKS_API_KEY` in `secrets.env` (non-empty
  and not the example's placeholder) and `PROXMOX_QDEVICE_HOST` in
  `cluster.conf`, and fail validation with a message naming the missing key.

### Optional keys (not in `cluster_dot_conf`)

The example `cluster_dot_conf` is already long, so these settings are **not**
added to it or to `cluster.conf`. `scripts/lib/config.sh` must still accept
them as known optional keys in `cluster.conf` (so it does not reject them as
unknown), validate them, and use the defaults below when absent. The README's
"Monitoring and Alerts" section documents them in a table, as options an
operator may add to `cluster.conf`.

| Key | Default | Validation |
|---|---|---|
| `HEALTHCHECKS_API_URL` | `https://healthchecks.io` | https URL, no trailing slash |
| `MONITOR_MAX_CHECKS` | `20` | integer 1-1000 (free plan limit) |
| `MONITOR_ALIVE_PERIOD_SECONDS` | `60` | integer >= 60 |
| `MONITOR_ALIVE_GRACE_SECONDS` | `180` | integer >= 60 |
| `MONITOR_FAST_INTERVAL_SECONDS` | `30` | integer 10-300 |
| `MONITOR_FAST_FAIL_THRESHOLD` | `3` | integer 1-10 |
| `MONITOR_SLOW_INTERVAL_SECONDS` | `1800` | integer 300-86400 |
| `MONITOR_POOL_MIN_FREE_PERCENT` | `10` | integer 1-50 |
| `MONITOR_POOL_CLEAR_FREE_PERCENT` | `12` | integer, greater than the min |
| `MONITOR_REMINDER_HOURS` | `6` | integer 1-168 |
| `MONITOR_POSTFIX_QUEUE_MAX_AGE_MINUTES` | `60` | integer 10-1440 |
| `MONITOR_CERT_WARN_DAYS` | `14` | integer 1-90 |
| `MONITOR_NVME_WEAR_WARN_PERCENT` | `90` | integer 50-100 |
| `MONITOR_PROBE_DEFAULT_PATH` | `/api/v1/healthz` | absolute path starting with `/`; pre-filled in the QDevice probe editor |
| `MONITOR_PROBE_DEFAULT_STATUS` | `200` | integer 100-599; pre-filled in the QDevice probe editor |

Check how `config.sh` distinguishes required from optional cluster keys today
and follow that pattern; if it has no notion of optional keys, add one with
tests.

### Secret key comment

`HEALTHCHECKS_API_KEY` is already in both files (added by the operator). The
example file's comment should say, in BMAC's style:

```
# Healthchecks.io project API key (read-write). Required when
# MONITORING_ENABLED=true in cluster.conf. Used only by the workstation to
# create, update, and delete this cluster's checks and to read their status.
# Nodes never receive it; each node receives only its own check's ping URL.
HEALTHCHECKS_API_KEY=redacted
```

Keep the operator's existing wording if it already says this.

### Registry: production VM monitoring settings

Ingress probe settings for each production VM live in its registry resource
record, because the QDevice config is rendered from the registry and the
settings must survive workstation changes. They contain no secrets. They are
edited only with the QDevice page's probe editor
(`manage_qdevice_probes.sh`, [§13](#13-workflows-scripts-and-the-dashboard)),
usually after an app has been deployed to the VM. `add_prod_vm.sh` does not
ask for them, because a new VM has no app yet; new records have no
`monitoring` field.

Add an optional top-level field `monitoring` to production resource records:

```json
"monitoring": {
  "probe": {
    "enabled": true,
    "path": "/api/v1/healthz",
    "expected_status": 200,
    "expected_text": null,
    "fqdns": ["prod1.example.com"]
  }
}
```

- `fqdns` defaults to the record's primary domain only. Aliases may be added
  explicitly; each FQDN probed counts as one probe, not one check.
- Staging records never have this field (`null`/absent).
- Implementation must update: the "exact top-level fields" list and its
  validation in `cluster_registry.py`; `docs/MAIN_DESIGN.md`'s
  `resources/prodN.json` section; reading of records without the field
  (treat as `{"probe": {"enabled": false}}` for records created before this
  change, so the existing prod1 keeps working until the operator sets it);
  and a new registry command:
  `cluster_registry.py update <name> --expected-revision N --monitoring-json '<json>'`
  with schema validation (path must start with `/`, status 100-599,
  `expected_text` at most 200 characters, fqdns must be this record's
  domains).
- Add `monitoring` to `tunnel-productions`-style read paths only where needed;
  add a `monitoring-probes` registry command that lists every production
  record's probe config for rendering the QDevice config.

### Production VM local settings (`/etc/app-ha-monitor/guest.json`)

Services and certificate paths live **on the VM**, because they describe what
runs inside it, and the operator manages them with the Production page's
Monitored services tile ([§13](#13-workflows-scripts-and-the-dashboard)):

```json
{
  "schema_version": 1,
  "services": ["hello-todo.service"],
  "cert_paths": ["/etc/hello-todo/cert.pem"]
}
```

- Mode 0600, owned by root.
- `deploy_hello_app_to_prod.sh` adds `hello-todo.service` and
  `/etc/hello-todo/cert.pem` to this file (idempotently) after it installs the
  app.
- The agent re-reads this file every cycle (cheap), so changes apply without a
  restart.

### Agent config (`/etc/app-ha-monitor/config.json`)

Rendered on the workstation by `scripts/lib/monitoring.sh` (using a Python
renderer, e.g. `scripts/lib/monitor_config.py`) and installed with mode 0600.
It contains this node's ping URL and is therefore secret.

```json
{
  "schema_version": 1,
  "cluster": "MyAppCloud",
  "node": "mox1",
  "role": "host",
  "ping_url": "https://hc-ping.com/<uuid>",
  "intervals": {"alive": 60, "fast": 30, "slow": 1800, "probe": 60},
  "thresholds": {
    "fast_fail": 3, "service_fail": 2,
    "pool_min_free_percent": 10, "pool_clear_free_percent": 12,
    "postfix_queue_max_age_minutes": 60, "cert_warn_days": 14,
    "nvme_wear_warn_percent": 90, "reminder_hours": 6
  },
  "host": {
    "expected_peers": ["mox2"],
    "qdevice_expected": true
  },
  "qdevice": {
    "hosts": [{"name": "mox1", "tailscale_hostname": "mox1"}],
    "qnetd_expected": true,
    "probes": [
      {"vm": "prod1", "fqdn": "prod1.example.com", "path": "/api/v1/healthz",
       "expected_status": 200, "expected_text": null}
    ]
  },
  "guest": {"settings_file": "/etc/app-ha-monitor/guest.json"}
}
```

Only the section matching `role` is present. `qdevice_expected` and
`qnetd_expected` are true when the cluster has a registered QDevice (even host
count); derive this the same way `show_qdevice_state.sh` and
`scripts/lib/qdevice.sh` do.

## 10. Healthchecks.io integration

### Account setup (operator, once)

1. Create an account and a project at healthchecks.io.
2. In project settings, create a **read-write API key** and put it in
   `secrets.env` as `HEALTHCHECKS_API_KEY`.
3. Keep the project's default email integration (the account email). The
   recipient must be the Workspace address the Gmail filters run on.
4. Optionally add a native Pushover integration instead of email forwarding
   for dead-man alerts (not required; the Gmail route covers everything).
5. In account settings, enable email reminders for down checks.
6. In account settings, turn off the monthly summary report email (or accept
   that it is forwarded to Pushover): it lists every check by name, so it
   contains `bmaccheck` and matches the forwarding filter.

### `scripts/lib/healthchecks.py`

Stdlib Python CLI used by `monitoring.sh`. It reads the API key from an
environment variable or stdin (never argv). Subcommands:

- `sync --desired desired.json --tag bmac-cluster-<name> [--dry-run]`
  Creates or updates each desired check and prints a JSON result mapping node
  name to `{uuid, ping_url, status, last_ping}`. Lists checks carrying the
  cluster tag that are not desired, as `obsolete`, and deletes them only with
  `--delete-obsolete` (the calling workflow decides, after its plan and
  confirmation).
- `status --tag bmac-cluster-<name>` – current status of every cluster check.
- `wait-first-ping --uuid U --timeout 240` – poll until the check's status is
  no longer `new`; exit non-zero on timeout.
- `delete --uuid U`.

Desired check fields:

| Field | Value |
|---|---|
| `name` | `<PROXMOX_CLUSTER_NAME> bmaccheck <node>` (e.g. `MyAppCloud bmaccheck mox1`); spaces are allowed in check names (verified) |
| `slug` | `<cluster-lower>-<node>` |
| `tags` | `bmac bmac-cluster-<cluster-lower> bmac-role-<role>` |
| `desc` | `BMAC <role> <node> in cluster <cluster>. Managed by BMAC; do not rename or edit.` |
| `timeout` | `MONITOR_ALIVE_PERIOD_SECONDS` |
| `grace` | `MONITOR_ALIVE_GRACE_SECONDS` |
| `channels` | `*` (all project integrations) |
| `unique` | `["name"]` (makes creation idempotent) |

Management API facts to rely on (verify against the current docs at
`https://healthchecks.io/docs/api/` before implementing; see
[§19](#19-facts-to-verify-before-or-during-implementation)): the API key goes in
the `X-Api-Key` header; `POST /api/v3/checks/` creates a check and, with
`unique`, returns and updates an existing match; `GET /api/v3/checks/?tag=...`
lists checks (ping URLs are included only for read-write keys);
`DELETE /api/v3/checks/<uuid>` deletes; `timeout` and `grace` are seconds with a
minimum of 60; a check that has never been pinged has status `new` and never
alerts.

Confirmed from a live `GET /api/v3/checks/` (read-write key, HTTP 200; full
field list in [§19.1](#191-confirmed-shapes)):

- `tags` is one space-separated string, not a list; filter by tag with
  `?tag=` and also check the string client-side.
- `channels` is a comma-separated string of integration UUIDs (empty when
  none).
- `timeout` and `grace` are integers in seconds.
- `status` was `down` for the test check; expect `new`, `up`, `grace`,
  `down`, `paused`, and `started`.
- **`slug` does not follow renames**: after the test check was renamed from
  `bmaccheck testcheck` to `MyAppCloud bmaccheck test`, its slug stayed
  `bmaccheck-testcheck`. BMAC must always send `slug` explicitly when it
  creates or updates a check.
- The check's UUID, which is the secret part of its ping URL, also appears in
  `update_url`, `pause_url`, and `resume_url`. (`badge_url` uses a different,
  non-secret ID.)

Why this name: Healthchecks puts the check name in the subject and body of
every email about the check (verified with a test check named `bmac_check
test_check_name`: both the DOWN and the UP email contained the name in the
subject and the body; the token was later changed to `bmaccheck`, without an
underscore, so Gmail search matches it as one plain word). So every BMAC email contains `bmaccheck`, which the Gmail
filters use instead of the sender address, and the cluster name tells
operators of several clusters which one alerted. Because `unique: ["name"]`
matches on the name, an operator who renames a check in the Healthchecks UI
makes BMAC create a new one; the `desc` says not to.

The client must refuse to create checks that would bring the project above
`MONITOR_MAX_CHECKS`, counting all checks in the project (not only this
cluster's), and say which nodes could not be added.

### First-ping verification

A check in `new` state never alerts, so a node that is broken from the start
would be invisible. Every workflow that creates a check, or installs or
restarts an agent, must call `wait-first-ping` (or, for an existing check,
confirm `last_ping` advanced after the restart) and fail clearly if it did not.

## 11. Deployment and the runtime update

### Goal

`scripts/user_callable/hosts/update_cluster_runtime.sh` brings monitoring to
every node of an existing cluster: Proxmox hosts, the QDevice, and every
production VM. No rebuild.

### Changes to `update_cluster_runtime.sh`

1. Add `app_ha_monitor.py` to `RUNTIME_FILES` for hosts. The existing staging,
   hash verification, atomic commit, and rollback then cover it.
2. Install or update the `app-ha-monitor.service` unit on hosts in the commit
   step (the unit file is new on existing hosts; host setup currently creates
   other units, so the updater must create this one when absent, keep the old
   unit for rollback, and `systemctl daemon-reload`). Replace
   `systemctl try-restart app-ha-egress-health.service` with a step that
   restarts both services; enable and start the monitor only when
   `MONITORING_ENABLED=true`, and stop and disable it when false.
3. After the hosts phase succeeds, when `MONITORING_ENABLED=true`:
   1. **Healthchecks sync**: build the desired check list (every host, the
      QDevice, every production VM from the registry), run
      `healthchecks.py sync`, show created/updated/obsolete checks in the plan,
      and delete obsolete ones after confirmation.
   2. **Render configs** for every node.
   3. **Hosts**: install each host's `/etc/app-ha-monitor/config.json`
      (mode 0600) and restart its agent.
   4. **QDevice** over `ssh $PROXMOX_QDEVICE_HOST`: stage, verify hash, install
      the agent, unit, and config atomically with the same keep-old-files
      rollback pattern, then restart.
   5. **Production VMs** over `ssh <name>` (the jump SSH alias created by
      `setup_jump_ssh_access.sh`): same as the QDevice, plus the
      `ExecCondition` staging guard and creation of `guest.json` if absent
      (never overwrite an existing `guest.json`).
   6. **Verify**: for every node, wait for its check's `last_ping` to advance
      past the restart time (or leave `new`); run `app_ha_monitor.py check
      --once --json` on each node and show the results.
4. **Preflight** before any change: when monitoring is enabled, every target
   (each host, the QDevice, each production VM in the registry with state
   active) must be reachable with BatchMode SSH, and the API key must work
   (a `status` call). If anything is unreachable, stop before changing any
   node and say which, matching the updater's existing "every node must be
   online" rule.
5. `--dry-run` shows everything that would change, including Healthchecks
   checks to create, update, or delete, without changing anything.
   When `MONITORING_ENABLED=false` and agents are installed, the update stops
   and disables them on every node and offers (typed confirmation) to delete
   this cluster's checks; if the operator declines, the checks are left and
   will report the nodes as down, which the result explains.
6. **Rollback**: if a later phase fails, nodes already updated in that phase
   are restored to their previous agent, unit, and config (or the agent is
   removed if it was absent). The host phase keeps its existing all-or-nothing
   behavior. Healthchecks checks created in a failed run are left in place
   (harmless, reused next run by `unique`), and the result says so.
7. Update the script header comment, `--help`, the README section "Updating
   BMAC Runtime Code on Existing Hosts", and the "Updating the deployed
   cluster runtime" section of `docs/MAIN_DESIGN.md`.
8. Update `scripts/tests/test_update_cluster_runtime.py`.

### Host setup and QDevice setup

- `add_proxmox_host.sh` must install the agent and unit on a new host as part
  of setup (through the same runtime file set), then run the membership sync
  ([§12](#12-membership-changes)).
- QDevice setup (`add_qdevice.sh`, and the QDevice step inside
  `add_proxmox_host.sh`) must install the QDevice agent when monitoring is
  enabled. `docs/QDEVICE_MANUAL_SETUP.md` should mention that the monitor is
  installed later by BMAC, so no manual step is needed.
- `add_prod_vm.sh` installs the guest agent after the guest is verified and
  SSH access is set up, then runs the membership sync.

## 12. Membership changes

Create one shared entry point, `monitoring_sync` in `scripts/lib/monitoring.sh`,
that does steps 3.1–3.6 of [§11](#11-deployment-and-the-runtime-update) for the
whole cluster. It is idempotent. Call it at the end of:

| Workflow | Monitoring change |
|---|---|
| `add_proxmox_host.sh` | create the new host's check; install its agent; update every host's `expected_peers` and the QDevice's host list; update QDevice expectations (even/odd host count) |
| `remove_proxmox_host.sh` | **delete the removed host's check** (listed in the plan; the existing typed confirmation covers it); update peers and QDevice expectations. For forced removal of a dead host, deleting its check is what stops further dead-man alerts for it |
| `add_qdevice.sh` | set `qdevice_expected`/`qnetd_expected` on every host and the QDevice; reuse the QDevice's existing check and agent (create them only if missing) |
| `remove_qdevice.sh` | clear `qdevice_expected`/`qnetd_expected`. **Keep the QDevice's check and agent:** removing it from its quorum role does not remove it as the cluster's external monitor. Say so in the plan and result |
| `add_prod_vm.sh` | create the VM's check; install the guest agent; write an empty `guest.json`; no services and no probe questions |
| `remove_prod_vm.sh` | **delete the VM's check** (listed in the plan); the registry record, and with it any probe, is removed by the existing workflow, so re-render the QDevice config |
| `manage_prod_vm_monitoring.sh` | none beyond the VM's own `guest.json` |
| `manage_qdevice_probes.sh` | re-render and install the QDevice config |
| `change_prod_vm_owner.sh`, `change_prod_vm_placement.sh` | none (the guest agent is inside the VM and follows it); do not call |

The QDevice's check is deleted only when an operator explicitly decommissions
the QDevice as a monitor, which is a manual step documented in the README
(delete the check in the Healthchecks UI after stopping the agent), or when
`MONITORING_ENABLED` is set to `false` and `update_cluster_runtime.sh` is run
(which offers to delete all of this cluster's checks; see [§11](#11-deployment-and-the-runtime-update)).

`add_prod_vm.sh` asks nothing about monitoring: a newly created VM has no app
deployed yet, so there are no services to monitor and no health endpoint to
probe. It only creates the check and installs the agent with an empty
`guest.json` (so the dead-man switch covers the VM from the start). After an
app is deployed, the operator adds its services with the Production page's
**Monitored services** tile and its health endpoint with the QDevice page's
**Health-check probes** tile. `deploy_hello_app_to_prod.sh` adds the example
app's service and certificate automatically.

If monitoring is enabled but the sync fails, the membership workflow must still
complete its main job, then report the monitoring failure as an error result
with a next step: "Run update_cluster_runtime.sh to finish updating
monitoring." The cluster must never be left half-joined because Healthchecks
was unreachable.

Agents re-read their config every cycle, so a sync only rewrites config files;
it restarts an agent only when its program file changed.

## 13. Workflows, scripts, and the Dashboard

All new user-callable scripts support `--json` through `ui_protocol.sh` and
appear in `dashboard/engine/workflows.json`.

### `manage_prod_vm_monitoring.sh` (new)

Location: `scripts/user_callable/guests/prod/manage_prod_vm_monitoring.sh`.
Workflow id `manage_prod_vm_monitoring`, category `production`, mode
`mutating`, not destructive, param `resource` of type `guest` (production VMs
only). Edits only `/etc/app-ha-monitor/guest.json` on the VM; it never touches
the registry or the QDevice.

Behavior:

1. Read `/etc/app-ha-monitor/guest.json` over `ssh <name>`.
2. Show the current settings: every monitored service with its current
   `systemctl is-active` state, and every certificate path with its expiry
   date.
3. Present one form (a `bmac_ui_group_*` request) to add and remove services
   and certificate paths:
   - services: systemd unit names; validate each matches
     `^[A-Za-z0-9@._:-]+$` and exists on the VM (`systemctl cat <unit>`
     succeeds). Offer the VM's enabled services (`systemctl list-unit-files
     --state=enabled --type=service`) as suggestions;
   - certificate paths: absolute paths; validate each is a readable PEM
     certificate on the VM.
4. Show a plan of changes and confirm.
5. Write `guest.json` atomically on the VM (temp file then `mv`, mode 0600).
6. Run `app_ha_monitor.py check --once --json` on the VM and show the result
   for the edited items.

Non-interactive flags for terminal use: `--show` (read-only),
`--add-service UNIT`, `--remove-service UNIT`, `--add-cert PATH`,
`--remove-cert PATH` (each repeatable).

### `manage_qdevice_probes.sh` (new)

Location: `scripts/user_callable/qdevice/manage_qdevice_probes.sh`. Workflow id
`manage_qdevice_probes`, category `qdevice`, mode `mutating`, not destructive.
Optional param `resource` of type `guest` to open one VM directly.

Behavior:

1. Read every production VM's registry record (through the coordinator, as
   other workflows do) and its `monitoring.probe` settings.
2. Show a table: VM, FQDN(s), probe enabled, path, expected status, expected
   text, and the latest probe result from the QDevice agent
   (`app_ha_monitor.py status --json` over `ssh $PROXMOX_QDEVICE_HOST`).
3. Let the operator pick a VM and edit its probe in one form: enabled; FQDNs
   (chosen from the record's primary and alias domains; default the primary);
   path (pre-filled with `MONITOR_PROBE_DEFAULT_PATH`); expected status
   (pre-filled with `MONITOR_PROBE_DEFAULT_STATUS`); optional expected text.
4. Before saving, run the probe once from the QDevice with the new settings
   (`app_ha_monitor.py probe --fqdn F --path P --status N [--text T] --json`,
   a new agent subcommand that sends nothing to Healthchecks) and show the
   result. If it fails, ask whether to save anyway (an operator may set up the
   probe before the app is deployed).
5. Show a plan and confirm; update the registry with `--expected-revision`;
   run `monitoring_sync` (re-renders and installs the QDevice config).

Non-interactive flags: `--show`, `--vm NAME` with `--enable`, `--disable`,
`--path P`, `--status N`, `--text T`, `--clear-text`, `--fqdn F` (repeatable).

Add `probe` to the agent subcommands in [§8](#8-the-monitor-agent-app_ha_monitorpy).

### Dashboard

- **Production page:** add `manage_prod_vm_monitoring` to the `WorkflowTiles`
  list on `dashboard/src/pages/ProductionPage.tsx` (the "Production
  operations" section) as its own new tile titled **"Monitored services"**
  (summary: view and add/remove the systemd services, and certificate files,
  that a production VM's monitor watches), and a row menu item "Monitored
  services…" that launches it with `{ resource: vm.name }`. The tile opens
  with a guest picker limited to production VMs.
- **QDevice page:** add `manage_qdevice_probes` as a tile on
  `dashboard/src/pages/QDevicePage.tsx`, titled "Health-check probes" (follow
  how that page lays out its existing tiles and update
  `QDevicePage.test.tsx`). Optionally show a small read-only table of probes
  on the page, filled by `manage_qdevice_probes.sh --show --json`.
- Add `show_monitoring_state` and `send_test_alert` to the Diagnostics page
  tiles.
- Add mock fixtures in `dashboard/src/lib/mock/` for the new workflows (the
  browser preview runs entirely on fixtures) and update
  `dashboard/test_workflow_registry.py` and any page tests.
- Choose a `lucide-react` icon already used in the app style (e.g.
  `Activity` or `BellRing`).

### `show_monitoring_state.sh` (new, read-only)

Location: `scripts/user_callable/diagnostics/show_monitoring_state.sh`.

- Lists every node with: check status from Healthchecks (`up`, `down`, `new`,
  `grace`, `paused`), last ping time, and the agent's open problems (from
  `app_ha_monitor.py status --json` over SSH).
- Lists obsolete checks tagged for this cluster and checks over budget.
- Shows the check budget: used / `MONITOR_MAX_CHECKS`.
- Supports `--reset-smart-baseline <host>:<serial>` (the one mutating
  option; confirm first).
- Next steps for every problem found.

### `send_test_alert.sh` (new)

Location: `scripts/user_callable/diagnostics/send_test_alert.sh`. Param: node
(any monitored node), priority (`p1`/`p2`). Runs `app_ha_monitor.py test-pulse`
on that node over SSH. The result tells the operator which emails to expect
and which filters should act on them.

### Changes to existing scripts

| Script | Change |
|---|---|
| `add_prod_vm.sh` | No monitoring questions (no services, no probe). Install the guest agent; write an empty `guest.json`; run `monitoring_sync` (creates the check); wait for the first ping. |
| `remove_prod_vm.sh` | Delete the VM's check as part of the plan; run `monitoring_sync`. |
| `deploy_hello_app_to_prod.sh` | After installing the app, add `hello-todo.service` and `/etc/hello-todo/cert.pem` to `guest.json` if monitoring is enabled. |
| `add_proxmox_host.sh`, `remove_proxmox_host.sh` | Call `monitoring_sync` as in [§12](#12-membership-changes); host removal deletes the host's check. |
| `add_qdevice.sh`, `remove_qdevice.sh` | Call `monitoring_sync`; never delete the QDevice's check or agent. |
| `show_cluster_health.sh` | Use the shared parsing code; optionally add a short "Monitoring" section summarizing check status. |
| `update_cluster_runtime.sh` | As in [§11](#11-deployment-and-the-runtime-update). |

## 14. The example app's health endpoint

Add `GET /api/v1/healthz` to `scripts/user_callable/guests/prod/hello_todo/src/main.rs`:

- Lives under `/api/`, because BMAC's Cloudflare setup skips bot challenges for
  `/api/` paths; a request from a datacenter IP (the QDevice) to a non-API path
  may be challenged.
- Runs a trivial database read (e.g. `SELECT 1` through the existing store) so a
  broken database makes the endpoint fail.
- Returns `200` with `{"status":"ok"}` on success, `503` with
  `{"status":"error"}` on failure (no internal details).
- Sends `Cache-Control: no-store`.
- Add it to the module doc comment's route list and add tests next to the
  existing `info` tests.

Document in the README that users' own apps should expose an equivalent cheap
endpoint under `/api/`, set it with the QDevice page's **Health-check probes**
tile, and add the app's systemd services with the Production page's
**Monitored services** tile.

## 15. Things outside BMAC (README material)

Add a new top-level README section **"Monitoring and Alerts"** (placed after
"Common Tasks" or as a subsection of it; keep the README's existing tone and
second-person style). It must contain everything in this section, written as
step-by-step instructions.

### 15.1 Overview

Explain the three alert sources (Healthchecks via BMAC agents; Cloudflare's
tunnel alert; Proxmox's own email), the pulse model in plain words, the cost
(free; optional Pushover one-time purchase), and the check budget formula.

### 15.2 Healthchecks.io setup

The steps in [§10](#10-healthchecksio-integration), then: set
`MONITORING_ENABLED=true`, run `update_cluster_runtime.sh --dry-run`, then
without `--dry-run`, then `send_test_alert.sh`.

### 15.3 Cloudflare tunnel health alert

In the Cloudflare dashboard, create a Notification of type "Tunnel Health
Alert" for the cluster's tunnel, delivered by email to the alert address, for
the Degraded and Down states. Explain that it reports only the connection
between `cloudflared` and Cloudflare, not whether apps respond, which is why
BMAC's QDevice also probes each app. Add this to the existing "Instructions for
Cloudflare Setup" as well. (The operator has already set this up and confirmed
it works; describe the steps as they appear in the dashboard at implementation
time.)

### 15.4 Gmail filters

Explain these facts first:

- Gmail applies **every** filter that matches a message; there is no order and
  no "stop processing". So the filters below are written to be mutually
  exclusive.
- Filters search the whole message, including the subject and the ping body
  that Healthchecks puts in its email.
- The filters do not use the sender address. (Matching on the sender's email
  address has proven unreliable for the operator; matching on the sender's
  display name works but is fragile.) Instead they match `bmaccheck`, which
  every BMAC check email contains because it is part of every check's name.
- Test each query in Gmail's search box before creating the filter, and confirm
  it finds the test emails you expect and nothing else.
- Before creating forwarding filters, add each Pushover address as a
  forwarding address (Settings → Forwarding and POP/IMAP → Add a forwarding
  address). Gmail sends a confirmation message to it; Pushover turns that into
  a notification on your phone containing the confirmation code.
- In Google Workspace, an admin must allow automatic forwarding for the user
  (describe the exact Admin console location at implementation time).
- Filters only act on mail that arrives after they are created.

**Basic filter set (two filters).** Create them with Settings → Filters and
Blocked Addresses → Create a new filter → type the query in "Has the words":

| # | Purpose | Query (Has the words) | Actions |
|---|---|---|---|
| F1 | Event cleared (noise) | `bmaccheck bmacclear subject:up` | Delete it |
| F2 | Everything else about BMAC checks: node went silent, events, node back | `bmaccheck -bmacclear` | Never send it to Spam; Apply label `BMAC`; Forward to your Pushover address |

Why F1 also requires the subject to contain "up": a dead-man DOWN email must
never be deleted. The agent already ensures the most recent ping before a node
goes silent never contains `bmacclear` ([§5](#5-the-pulse-protocol)), but if
Healthchecks ever includes older ping bodies in a DOWN email, `bmacclear`
could appear in it. Requiring "up" in the subject makes deletion impossible for
DOWN emails. Note the consequence: a DOWN email that contains `bmacclear` would
match neither F1 nor F2 and would stay in the inbox without being forwarded.
That is only a fallback: testing showed that a dead-man DOWN email contains
only the last ping's body, and the agent never leaves a token in the last
ping ([§5](#5-the-pulse-protocol)).

Confirmed Healthchecks email details (used by these filters):

- Subjects are `DOWN | <check name>` and `UP | <check name>`, for example
  `DOWN | MyAppCloud bmaccheck mox1`. Gmail's `subject:up` matches the word
  "UP".
- The first line of the body repeats the check name in quotes and says why:
  "is DOWN (received a failure signal)" for a pulse, "is DOWN (success signal
  did not arrive on time, grace time passed)" for a dead-man alert, and "is
  UP" for both a clear and a recovery.
- The body includes "Last Ping Body" (the body of the last ping only) and a
  "Projects Overview" listing the account's projects and whether their checks
  are up. The overview names projects, not checks, so it does not add
  `bmaccheck` to unrelated emails.

**Optional priority split.** To get two Pushover priorities, replace F2 with:

| # | Purpose | Query | Actions |
|---|---|---|---|
| F2a | High priority: events marked p1, and nodes going silent | `bmaccheck -bmacclear -bmacp2` | as F2, forwarding to the **high-priority** Pushover address |
| F2b | Normal priority: events marked p2 | `bmaccheck -bmacclear bmacp2` | as F2, forwarding to the **normal** Pushover address |

Dead-man DOWN emails and "node is back" UP emails carry no priority token, so
they go to F2a (high priority).

**Other emails that contain `bmaccheck`:** Healthchecks' reminder emails
(while a check is down) list check names, so F2 forwards them; that is useful.
Its monthly summary also lists every check; turn it off in the Healthchecks
account settings, or it is forwarded monthly.

**Optional filters for the other alert sources.** These are not Healthchecks
emails, so they need their own match terms. Use words that appear in their
subjects, and test in the search box first:

| Purpose | Example query | Actions |
|---|---|---|
| Cloudflare tunnel alert | `subject:tunnel` plus a word from the subject of the alert emails you receive | Never send it to Spam; label; forward to Pushover |
| Proxmox SMART email | `subject:("SMART error")` | Never send it to Spam; label; forward to Pushover |
| Other Proxmox email (fencing, replication, ZFS) | words from those subjects, as received | Never send it to Spam; label; forward to Pushover |

Test the whole chain with `send_test_alert.sh` for p1 and p2, and by stopping
`app-ha-monitor.service` on a production VM for about four minutes (dead-man
DOWN, then UP when it is started again).

### 15.5 Pushover

- What it is: a commercial push service; a one-time purchase per platform
  after a 30-day trial; each user can send up to 10,000 messages per month.
- The basic filter set needs one Pushover address: every user has a default
  `@pomail.net` address. For the optional priority split, create two email
  aliases in the Pushover dashboard: one with high priority (and a distinct
  sound), one with normal priority. Pushover provides the receiving address only,
  so mail is forwarded to it by Gmail.
- Optionally, use Pushover's emergency priority on the high alias if it is
  offered for email aliases (verify), which repeats until acknowledged.
- Send one test email from Gmail to each alias before setting up forwarding.

### 15.6 Keeping Proxmox's own emails out of spam (Google Workspace)

Proxmox delivers its own alert email directly to the recipient's mail server
(Postfix with no relay), without SPF or DKIM for your domain. With Google
Workspace recipients:

Proxmox's emails arrive from `root@<host>.pve.internal`. Because that domain
is not public, they can never pass SPF or DKIM; Gmail shows them with an
unverified-sender icon. They are accepted today, but nothing guarantees that.

1. Admin console → Apps → Google Workspace → Gmail → Spam, Phishing and
   Malware → **Email allowlist**: add each Proxmox host's public IPv4. Mail
   from these IPs will not be put in spam, though Gmail may still show a
   warning banner.
2. Optional but recommended: set the Proxmox notification "from" address to an
   address on a domain you control, publish an SPF record for that domain
   listing the hosts' public IPs, and ask the provider to set reverse DNS (PTR)
   records for those IPs.
3. BMAC's host agents report stuck Proxmox email (the `postfix-queue`
   problem), and they report disk problems themselves, so a blocked Proxmox
   email path is noticed and does not hide disk alerts.

### 15.7 Verifying Proxmox's SMART email (optional)

To prove smartd's email path on a host, temporarily change the `DEVICESCAN`
line in `/etc/smartd.conf` to:

```
DEVICESCAN -d removable -n standby -m root -M exec /usr/share/smartmontools/smartd-runner -M test
```

Run `systemctl restart smartmontools`; one test email per monitored disk
should arrive. `journalctl -u smartmontools --since '-2 min'` lists the disks
smartd picked up. Remove ` -M test` and restart again afterwards.

The operator ran this test on mox1 on 2026-10-22: smartd listed both NVMe
drives, sent one test email per drive (subject "SMART error (EmailTest)
detected on host: mox1"), Postfix delivered both to Google (`status=sent`),
and both arrived in the Workspace inbox. Gmail marked them "External" and
showed an unverified-sender icon, because the envelope sender is
`root@mox1.pve.internal`, a non-public domain that cannot pass SPF or DKIM.
Postfix also first tried Google over IPv6, which the hosts do not have
("Network is unreachable"), then succeeded over IPv4. Setting
`postconf -e 'inet_protocols = ipv4'` on each host avoids that retry; it is
optional and not part of this implementation.

## 16. Interpreting alerts

Put this table in the README section too.

| What you receive | Most likely meaning |
|---|---|
| Dead-man DOWN for one host, plus other hosts report `link1:<host>` DOWN but not `link0:<host>` | The host is alive but lost its internet connection (Tailscale and Healthchecks are both unreachable from it). Its guests fall back through another host; ingress continues through other replicas. |
| Dead-man DOWN for one host, plus other hosts report both `link0` and `link1` to it DOWN, and the QDevice reports `tailscale:<host>` | The host is down or completely isolated. Check the iDRAC. HA restarts its prod VMs elsewhere. |
| Every host reports `link0` to every peer DOWN; no dead-man alerts | Private VLAN failure. Corosync keeps running over Tailscale; ingress through other hosts' replicas fails for VMs not on the receiving host; replication fails (Proxmox will also email). |
| Hosts report `link1` DOWN to each other, QDevice reports `tailscale:*`; no dead-man alerts | Tailscale problem. The cluster runs on the VLAN link; the QDevice's vote (if used) and your SSH access are affected. |
| QDevice reports `tailscale:<host>` for every host at once, hosts are fine | Tailscale problem on the QDevice side. |
| Dead-man DOWN for the QDevice; hosts report `qdevice-vote` (if a QDevice is registered) | The QDevice VPS is down. With an even host count, one more host failure would lose quorum. |
| QDevice reports `ingress:<fqdn>` only | That app is not answering through Cloudflare (app down, origin certificate, tunnel route). Check `service:*` from the VM's agent and Cloudflare's tunnel email. |
| QDevice reports `ingress:*` for all apps plus Cloudflare tunnel Down email | No tunnel replica is connected, or Cloudflare is having a problem. |
| Dead-man DOWN for a production VM only | The VM is down, or its egress is broken. |
| `postfix-queue` on a host | Proxmox's own alert emails are not leaving that host; BMAC's own alerts are unaffected. |

## 17. Security

- `HEALTHCHECKS_API_KEY` stays on the workstation in `secrets.env` (mode 0600).
  Never pass it in argv, environment of remote commands, logs, or pmxcfs. Pass
  it to `healthchecks.py` through stdin or the local process environment only.
- Each node receives only its own ping URL, in `/etc/app-ha-monitor/config.json`
  (root, 0600). A compromised node can only affect its own check.
- Ping URLs never appear in logs, history, Dashboard output, or the UI
  protocol stream. Redact them as `https://hc-ping.com/<redacted>`. A check's
  UUID is the secret part of its ping URL, and the API also returns it in
  `uuid`, `update_url`, `pause_url`, and `resume_url`: redact every check UUID
  wherever it appears in anything BMAC prints, and show checks by name only.
- No secrets in the registry: the `monitoring` field holds probe settings only.
- No inbound ports are added anywhere. The only new network permission is the
  Tailscale ICMP rule from the QDevice to hosts.
- The agent runs sandboxed under systemd ([§8](#8-the-monitor-agent-app_ha_monitorpy)).
- Staging clones inherit a production VM's config file, including its ping URL;
  the staging guard prevents use. As optional hardening (not required by this
  design), the staging identity patcher may delete
  `/etc/app-ha-monitor/config.json` from clones.
- Ingress probes send no credentials and accept only the configured responses.
- Pulse bodies contain host names, pool names, disk models and serial-based
  IDs, unit names, and FQDNs. They contain no secrets, IP addresses other than
  Tailscale/private ones when useful for diagnosis, or certificate contents.
- Add the new invariants to `docs/MAIN_DESIGN.md`'s "Security invariants" and
  check `SECURITY_AUDIT_STATUS.md` for items to update.

## 18. Tests

Write tests in `scripts/tests/` following existing patterns
(`test_egress_health.py`, `test_update_cluster_runtime.py`,
`test_show_cluster_health.py`, `ui_test_driver.py`).

### Agent (`test_app_ha_monitor.py`)

- Parsing, from fixtures captured on the live cluster ([§19](#19-facts-to-verify-before-or-during-implementation)):
  `corosync-cfgtool -n` (all links up; link0 down; link1 down; both down;
  peer missing), quorum output (quorate, not quorate, with and without
  QDevice), `corosync-qnetd-tool -l`, `zpool status -x` (healthy, degraded),
  `zfs list` capacity, `smartctl -j` (ATA: passed, failed, attribute 5/197/198
  increase; NVMe: passed, each `critical_warning` bit decoded, spare at
  threshold, spare within 10 points of threshold, `percentage_used` at 89, 90,
  and 100+, `media_errors` increase, the same drive seen as both
  `/dev/nvme0` and `/dev/nvme0n1` deduplicated by serial; read error and
  timeout), `postqueue -j` (empty, old message),
  `tailscale status --json`, cloudflared `/ready`, `df -P`, `openssl x509
  -enddate`.
- Debounce: fast check fails twice then passes (no problem); fails three times
  (opened); passes three times (resolved). Service check opens after 2.
  Slow check opens after 1.
- Hysteresis: 9.9% opens; 11% stays open; 12.1% resolves; 4.9% escalates to p1.
- Pulse composition: token line, priority selection, section order,
  sanitization of tokens inside messages, truncation within 8,000 bytes.
- Delivery: uses a local HTTP test server (bound to 127.0.0.1 on an ephemeral
  port) that records requests. Verify the fail → clear → heartbeat sequence;
  retry behavior; `pending_clear` survives restart; outbox replay after a
  failed `/fail`; rate limits; no ping URL in logs.
- State: atomic writes, corrupt state handling, first-start announcement,
  restart without re-announcement, reminders after the configured hours (use
  an injected clock, never `sleep`).
- Staging guard: hostname `stage1prod1` sends nothing.
- Tailscale JSON: non-host peers ignored; a tagged host that is not in BMAC's
  membership ignored; `Online`/`LastSeen` included in a failed ping's message;
  `LastSeen` zero value.
- qnetd: registered QDevice with all hosts ACKed (the captured output); a
  missing node; `Vote` not ending in `(ACK)`; the cluster name absent.
- Quorum: `Total votes` below `Expected votes`.
- `--help` works without root and without config.

### Workstation

- `healthchecks.py` against a fake API server: create, idempotent update with
  `unique`, list by tag, obsolete detection, delete only with the flag, budget
  refusal, `wait-first-ping` success and timeout, API key never in argv or
  output.
- Config rendering: host peers, QDevice expectations for odd and even host
  counts, probes from registry records with and without `monitoring`, guest
  configs.
- `update_cluster_runtime.sh`: monitoring disabled (no agent started, no API
  calls), enabled happy path, unreachable QDevice or prod VM in preflight (no
  change made), rollback when a guest install fails, dry run makes no changes
  and no mutating API calls.
- Registry: `monitoring` field validation, records without it, update with
  expected revision, `monitoring-probes` listing.
- `manage_prod_vm_monitoring.sh`: show, add/remove service, invalid unit,
  invalid certificate path, JSON-mode forms via `ui_test_driver.py`; it never
  calls the registry.
- `manage_qdevice_probes.sh`: show, enable/disable, path/status/text edits,
  FQDN not belonging to the record rejected, failed pre-save probe with
  save-anyway and cancel, registry revision conflict, JSON-mode forms.
- Check names: `<cluster> bmaccheck <node>`; `remove_qdevice.sh` never deletes
  the QDevice check; host and prod VM removal delete theirs; disabling
  monitoring offers deletion.
- `config.sh`: `MONITORING_ENABLED` absent (defaults false), `true` without
  `HEALTHCHECKS_API_KEY` (fails), each optional key accepted, validated, and
  defaulted when absent; optional keys are not in `cluster_dot_conf`.
- Membership workflows call `monitoring_sync`, and a sync failure does not fail
  the main operation.
- Dashboard: workflow registry test, ProductionPage tile and menu item, mock
  fixtures.
- Rust: `hello_todo` tests for `/api/v1/healthz` (200 and `Cache-Control`
  header; 503 when the store fails if the test harness allows it).

Run `dev/run_tests.py` for the whole suite, `bash -n` on changed shell scripts,
and the dashboard's own test commands (`pnpm test` or as documented in
`dashboard/README.md`).

## 19. Facts to verify before or during implementation

This section has two parts: output shapes the operator has already captured
from the live cluster (use these to build test fixtures), and facts still to
collect. Fixtures are committed to a public repository, so anonymize them as
the excerpts below are anonymized: replace public IPs, Tailscale IPs, serial
numbers, connector IDs, node keys, email addresses, and tailnet names with
placeholders. Make the anonymized values consistent across fixtures (the same
drive has the same placeholder serial in `smartctl` and `zpool status`).

### 19.1 Confirmed shapes

All captured on 2026-10-23 on a test cluster that had not been updated with
`apt upgrade`: Proxmox hosts had `smartctl` 7.5 and Python 3.13.5 on kernel
7.0.2-6-pve; the QDevice and prod1 had Ubuntu 26.04.1 with Python 3.14.4.
Versions, and possibly output details, will change after upgrades. Parsers
must tolerate extra fields and lines, and must not depend on tool versions.

#### Quorum: three hosts, no QDevice

`ssh mox1 'corosync-quorumtool -s'` while the cluster had three hosts. Use
`corosync-quorumtool -s` rather than `pvecm status`: it prints the same
sections but names members by host name and uses decimal node IDs
(`pvecm status` uses IPs and hex IDs such as `0x00000001`).

```
Quorum information
------------------
Date:             Fri Oct 23 09:17:38 2026
Quorum provider:  corosync_votequorum
Nodes:            3
Node ID:          1
Ring ID:          1.3a
Quorate:          Yes

Votequorum information
----------------------
Expected votes:   3
Highest expected: 3
Total votes:      3
Quorum:           2  
Flags:            Quorate 

Membership information
----------------------
    Nodeid      Votes Name
         1          1 mox1 (local)
         2          1 mox2
         3          1 mox3
```

#### Quorum: two hosts and a QDevice, healthy

```
Quorum information
------------------
Date:             Fri Oct 23 09:53:15 2026
Quorum provider:  corosync_votequorum
Nodes:            2
Node ID:          1
Ring ID:          1.3e
Quorate:          Yes

Votequorum information
----------------------
Expected votes:   3
Highest expected: 3
Total votes:      3
Quorum:           2  
Flags:            Quorate Qdevice 

Membership information
----------------------
    Nodeid      Votes    Qdevice Name
         1          1    A,V,NMW mox1 (local)
         2          1    A,V,NMW mox2
         0          1            Qdevice
```

#### Quorum: two hosts and a QDevice, qnetd stopped (vote lost)

Captured 20 seconds after `systemctl stop corosync-qnetd` on the QDevice:

```
Votequorum information
----------------------
Expected votes:   3
Highest expected: 3
Total votes:      2
Quorum:           2  
Flags:            Quorate Qdevice 

Membership information
----------------------
    Nodeid      Votes    Qdevice Name
         1          1   A,NV,NMW mox1 (local)
         2          1   A,NV,NMW mox2
         0          0            Qdevice (votes 1)
```

Parsing notes for all three shapes:

- Key lines have trailing spaces (`Quorum:           2  `, `Flags:            Quorate `).
  Parse with `^\s*([^:]+):\s*(.*?)\s*$`.
- The `Qdevice` column exists only when a QDevice is configured, the Qdevice
  member line has an empty value in that column, and host names carry a
  ` (local)` suffix. Parse the membership table using the header line's column
  positions rather than splitting on whitespace, and test all three fixtures.
- In the Qdevice column, `A` means alive, `V` voting, `NV` not voting, and
  `NMW` "not master wins".
- `Flags:` still reads `Quorate Qdevice` after the vote is lost, so it says
  only that a QDevice is configured.
- After `systemctl start corosync-qnetd`, the healthy shape returned within
  30 seconds.

#### QDevice client status on a host

`ssh mox1 'corosync-qdevice-tool -s'`, healthy:

```
Qdevice information
-------------------
Model:			Net
Node ID:		1
Configuration version:	7
Configured node list:
    0	Node ID = 1
    1	Node ID = 2
Membership node list:	1, 2

Qdevice-net information
----------------------
Cluster name:		MyAppCloud
QNetd host:		100.64.0.2:5403
Algorithm:		Fifty-Fifty split
Tie-breaker:		Node with lowest node ID
State:			Connected
```

With qnetd stopped, the output is identical except `State:			Connect failed`.
Values are separated from keys by tabs.

#### Corosync links

`ssh mox1 'corosync-cfgtool -n'` with three hosts (already parsed by
`parse_links` in `show_cluster_health.sh`):

```
Local node ID 1, transport knet
nodeid: 2 reachable
   LINK: 0 udp (10.213.0.11->10.213.0.12) enabled connected mtu: 1397
   LINK: 1 udp (100.64.0.11->100.64.0.12) enabled connected mtu: 1173

nodeid: 3 reachable
   LINK: 0 udp (10.213.0.11->10.213.0.13) enabled connected mtu: 1397
   LINK: 1 udp (100.64.0.11->100.64.0.13) enabled connected mtu: 1173
```

With two hosts, the output contains only the `nodeid: 2` block. A link that is
down has not been captured; `parse_links` treats a `LINK` line as up only when
it contains `connected` and not `disconnected`, and a missing `nodeid` block
as the peer missing. Keep that logic and its existing tests.

#### qnetd

`ssh qdevice 'corosync-qnetd-tool -l'`, healthy, QDevice registered for the
two-host cluster. Values are separated from keys by tabs:

```
Cluster "MyAppCloud":
    Algorithm:		Fifty-Fifty split (KAP Tie-breaker)
    Tie-breaker:	Node with lowest node ID
    Node ID 1:
        Client address:		::ffff:100.64.0.11:58120
        Configuration version:	7
        Configured node list:	1, 2
        Membership node list:	1, 2
        Vote:			ACK (ACK)
    Node ID 2:
        Client address:		::ffff:100.64.0.12:45528
        Configuration version:	7
        Configured node list:	1, 2
        Membership node list:	1, 2
        Vote:			No change (ACK)
```

`Vote:` shows the last vote and, in parentheses, the current one. With qnetd
stopped: `systemctl is-active corosync-qnetd` prints `inactive`; the tool
prints `corosync-qnetd-tool: Can't connect to qnetd socket (is QNetd running?): No such file or directory`
on stderr and exits 3.

#### cloudflared readiness

`ssh mox1 'curl -s -i http://127.0.0.1:20241/ready'`:

```
HTTP/1.1 200 OK
Content-Length: 88
Content-Type: text/plain; charset=utf-8

{"status":200,"readyConnections":4,"connectorId":"00000000-0000-0000-0000-000000000000"}
```

With `app-ha-cloudflared.service` stopped, the port is closed (curl exit 7,
connection refused). Ten seconds after a restart it was ready again with 4
connections and a **new** `connectorId`.

#### Postfix queue

Empty queue: `postqueue -j` prints nothing. One deferred message (captured by
temporarily setting `defer_transports = smtp`):

```json
{"queue_name": "deferred", "queue_id": "A0B19119BF", "arrival_time": 1792771213, "message_size": 489, "forced_expire": false, "sender": "root@mox1.pve.internal", "recipients": [{"address": "admin@example.com", "delay_reason": "deferred transport"}]}
```

#### ZFS

```
$ zpool list -H -o name
rpool
$ zpool status -x
all pools are healthy
$ zpool status -P -p rpool
  pool: rpool
 state: ONLINE
config:

	NAME                                                                   STATE     READ WRITE CKSUM
	rpool                                                                  ONLINE       0     0     0
	  mirror-0                                                             ONLINE       0     0     0
	    /dev/disk/by-id/nvme-INTEL_SSDPE2KX020T8_SERIALA0000000000-part3  ONLINE       0     0     0
	    /dev/disk/by-id/nvme-INTEL_SSDPE2KX020T7_SERIALB0000000000-part3  ONLINE       0     0     0

errors: No known data errors
$ zfs list -H -p -o used,available rpool
9796521984	1921358725120
```

The by-id names contain the model and serial, which ties ZFS members to
`smartctl` serials for messages ("mirror member SERIALA… is the drive with
SMART warnings"). A degraded pool has not been captured; use
`simulate_disk_failure_and_replacement.sh` on a test cluster, or upstream
examples, for that fixture.

#### SMART, NVMe

`smartctl --scan -j`:

```json
{
  "smartctl": {"argv": ["smartctl", "--scan", "-j"], "exit_status": 0},
  "devices": [
    {"name": "/dev/nvme0", "info_name": "/dev/nvme0", "type": "nvme", "protocol": "NVMe"},
    {"name": "/dev/nvme1", "info_name": "/dev/nvme1", "type": "nvme", "protocol": "NVMe"}
  ]
}
```

`smartctl -j -i /dev/nvme0` identity fields: `model_name` (`INTEL
SSDPE2KX020T8`), `serial_number`, `firmware_version`. `smartctl -j -H -A
/dev/nvme0`, relevant parts (`-H -A` alone has no `model_name` or
`serial_number`, so the agent runs `-j -i -H -A`):

```json
{
  "smartctl": {"exit_status": 0},
  "device": {"name": "/dev/nvme0", "type": "nvme", "protocol": "NVMe"},
  "smart_status": {"passed": true, "nvme": {"value": 0}},
  "nvme_smart_health_information_log": {
    "critical_warning": 0,
    "temperature": 30,
    "available_spare": 99,
    "available_spare_threshold": 10,
    "percentage_used": 3,
    "data_units_written": 413489848,
    "power_on_hours": 24589,
    "unsafe_shutdowns": 26,
    "media_errors": 0,
    "num_err_log_entries": 0
  },
  "spare_available": {"current_percent": 99, "threshold_percent": 10},
  "endurance_used": {"current_percent": 3}
}
```

Read values from `nvme_smart_health_information_log`; the summary objects
duplicate them. `smartctl -j -H -A` with no device prints `"exit_status": 1`
and the message "smartctl requires a device name as the final command-line
argument." No ATA drives exist on these hosts; build ATA fixtures from
smartmontools' documented JSON.

#### Filesystems

`ssh prod1 'df -P -B1 -T'` (columns: Filesystem, Type, 1-blocks, Used,
Available, Capacity, Mounted on), abridged:

```
Filesystem     Type          1-blocks        Used     Available Capacity Mounted on
tmpfs          tmpfs      15096426496     1445888   15094980608       1% /run
/dev/sda2      ext4     1079988703232 12111208448 1012941602816       2% /
efivarfs       efivarfs        262044       62668        194256      25% /sys/firmware/efi/efivars
/dev/sda1      vfat        1124995072     6606848    1118388224       1% /boot/efi
none           tmpfs          1048576        8192       1040384       1% /run/credentials/hello-todo.service
```

The QDevice has a single real filesystem, `/dev/sda` (ext4) on `/`, about 24
GiB, 16% used.

#### Production VM details

- `hostname`: `prod1`; `hello-todo.service`: `active`.
- The example app's Cloudflare Origin certificate:
  `openssl x509 -enddate -noout` prints `notAfter=Sep 16 23:49:00 2041 GMT`
  (a 15-year certificate). Parse the `notAfter=` value with format
  `%b %d %H:%M:%S %Y GMT`, allowing a space-padded day.
- `systemctl list-unit-files --state=enabled --type=service --no-pager` has a
  header line (`UNIT FILE  STATE  PRESET`), then one unit per line, and a
  footer with a count. The Monitored services tile offers these as
  suggestions; skip template units ending in `@.service`.

#### Tailscale peers

`ssh qdevice 'tailscale status --json'`. `Peer` is an object keyed by node
key. One entry, trimmed; this is mox3 shortly after it was removed from the
cluster and shut down, so it is also the fixture for an offline peer:

```json
"nodekey:<redacted>": {
  "HostName": "mox3",
  "DNSName": "mox3.<tailnet>.ts.net.",
  "TailscaleIPs": ["100.64.0.13", "fd7a:115c:a1e0::13"],
  "Tags": ["tag:proxmox-host"],
  "CurAddr": "",
  "Relay": "den",
  "LastSeen": "2026-10-23T15:20:00.1Z",
  "LastHandshake": "0001-01-01T00:00:00Z",
  "Online": false,
  "Active": false,
  "InEngine": false
}
```

Peers that are people's devices have no `Tags` (or other tags) and must be
ignored. `Self` describes the QDevice itself. ICMP over Tailscale from the
QDevice to a host works with the new policy rule (`ping -c 2`: 0% loss, about
14 ms).

#### Healthchecks.io

Tested by the operator with a test check (1-minute period and grace):

- Both the DOWN email (from `/fail`) and the UP email (from the success)
  include the ping body, under "Last Ping Body".
- A `/fail` immediately followed by a success, with no delay, produces both
  emails, every time.
- A second `/fail` while the check is already down sends no email.
- A check name may contain spaces.
- Subjects: `DOWN | <check name>` and `UP | <check name>`.
- First body line: `"<check name>" is DOWN (received a failure signal).`,
  `"<check name>" is DOWN (success signal did not arrive on time, grace time passed).`,
  or `"<check name>" is UP. The downtime lasted 0 seconds.`
- After a `/fail`, a clear, and a plain heartbeat, the dead-man DOWN email
  contained only `plain heartbeat` as "Last Ping Body", with "Last Ping Type:
  Success". No earlier ping bodies appear.
- Every email also shows Project, Period, Total Pings, Last Ping (time and
  source IP), Last Ping Type, the time of the status change, and a "Projects
  Overview" of the account's projects.

#### Healthchecks Management API

`GET https://healthchecks.io/api/v3/checks/` with a read-write key in
`X-Api-Key` returned HTTP 200 and this check object (UUIDs and URLs
replaced):

```json
{"checks": [{"name": "MyAppCloud bmaccheck test", "slug": "bmaccheck-testcheck",
  "tags": "", "desc": "", "grace": 60, "n_pings": 5, "status": "down",
  "started": false, "last_ping": "2026-10-23T16:07:07+00:00", "next_ping": null,
  "manual_resume": false, "methods": "", "subject": "", "subject_fail": "",
  "start_kw": "", "success_kw": "", "failure_kw": "", "filter_subject": false,
  "filter_body": false, "filter_http_body": false, "filter_default_fail": false,
  "badge_url": "https://healthchecks.io/b/2/<badge-id>.svg",
  "uuid": "<uuid>", "ping_url": "https://hc-ping.com/<uuid>",
  "update_url": "https://healthchecks.io/api/v3/checks/<uuid>",
  "pause_url": "https://healthchecks.io/api/v3/checks/<uuid>/pause",
  "resume_url": "https://healthchecks.io/api/v3/checks/<uuid>/resume",
  "channels": "<integration-uuid>", "timeout": 60}]}
```

An earlier attempt printed `null` because the key line was commented out (the
API returned an error object, which `jq '.checks[0]'` turned into `null`).
Always check the HTTP status and print the API's error message.

#### Ingress probe from the QDevice

`curl -s -o /dev/null -D - https://pinbinapp.com<path>` from the QDevice,
with curl's default user agent and with `-A "bmac-monitor/test"`: no request
was challenged, on any path. Headers returned:

| Path | Status | `content-type` | Notes |
|---|---|---|---|
| `/` | `HTTP/2 200` | `text/html; charset=utf-8` | not challenged, even outside `/api/` |
| `/api/v1/info` | `HTTP/2 200` | `application/json` | |
| `/api/v1/healthz` | `HTTP/2 404` | `application/json` | endpoint not implemented yet; the app's own JSON 404 |

Every response had `server: cloudflare` and a `cf-ray` header, and no
`cf-mitigated` header. Keep the challenge detection anyway (other operators'
zones may have stricter bot settings), and keep the recommendation to put
health endpoints under `/api/`.

#### smartd email path

See [§15.7](#157-verifying-proxmoxs-smart-email-optional).

### 19.2 Still to collect

| # | Fact | Command or action |
|---|---|---|
| C3 | Gmail search matches the token | Search Gmail for `bmaccheck`; report whether it finds the four test emails whose check names contain it |
| C13 | Pushover alias priority options; Workspace Admin console locations for automatic forwarding and the email allowlist | Look in the Pushover dashboard and the Admin console |
| C14 | systemd sandbox compatibility | During implementation: run `app_ha_monitor.py check --once` under the unit's settings on a host, the QDevice, and prod1 |

## 20. Rollout to the existing cluster

The operator's cluster at the time of writing: two Proxmox hosts (`mox1`,
`mox2`), the QDevice registered and voting (`ssh qdevice` works), and one
production VM (`ssh prod1` via jump SSH). mox3 was removed from the cluster,
and from Tailscale, to bring the QDevice into its quorum role so its output
could be captured. Already done: the Tailscale ICMP rule is in the policy;
Cloudflare tunnel email alerts are configured; `MONITORING_ENABLED=true` is in
`cluster.conf`; `HEALTHCHECKS_API_KEY` is in `secrets.env`.

The steps below do not depend on the host count. If mox3 is added back before
monitoring is rolled out, expect one more check and agent, and the QDevice's
role changes to standby (odd host count), which the agents handle: hosts stop
expecting the QDevice vote and the QDevice skips its qnetd check. If mox3 is
added back afterwards, `add_proxmox_host.sh` runs `monitoring_sync`, which
does the same.

Steps after the implementation is merged:

1. `update_cluster_runtime.sh --dry-run`, review: 4 checks to create
   (`MyAppCloud bmaccheck mox1`, `… mox2`, `… qdevice`, `… prod1`), agents
   to install on all four nodes. Budget 4/20.
2. `update_cluster_runtime.sh`. Expect all four checks to leave `new` and
   report `up`. prod1's registry record has no `monitoring` field, so no
   ingress probe runs yet.
3. Redeploy the example app to get `/api/v1/healthz`:
   `deploy_hello_app_to_prod.sh` for prod1. This also adds
   `hello-todo.service` and its certificate to prod1's `guest.json`; confirm
   with the Production page's **Monitored services** tile.
4. QDevice page → **Health-check probes** → prod1: enable the probe with path
   `/api/v1/healthz`, status 200. The tool runs the probe once before saving.
5. Turn off the Healthchecks monthly summary report; enable reminders.
6. Create the Gmail filters and the Pushover address(es) ([§15](#15-things-outside-bmac-readme-material)).
7. `send_test_alert.sh` for one host with p1 and the QDevice with p2; confirm
   each DOWN email is forwarded and each UP clear is deleted.
8. Dead-man test: `ssh prod1 systemctl stop app-ha-monitor`, wait about 4
   minutes for the DOWN email (forwarded by F2), then start it again and
   confirm the UP email is kept and forwarded (F2).
9. QDevice-vote test (two-host cluster only, during a quiet period):
   `ssh qdevice systemctl stop corosync-qnetd`, wait about 2 minutes, and
   expect a `qdevice-vote` event from each host and a `qnetd:service` event
   from the QDevice. Start it again and confirm the RESOLVED pulses. The
   cluster stays quorate while both hosts are up.
10. `show_monitoring_state.sh` shows everything up with no open problems.

No host, VM, or cluster rebuild is needed.

## 21. Implementation order and acceptance criteria

Implement in this order; each step ends with the full test suite passing.

1. **Shared parsing and the agent core**: `app_ha_monitor.py` with checks,
   debounce, hysteresis, state, pulse composer, sender, subcommands, and tests.
   Refactor `show_cluster_health.sh` to use the shared parsing.
2. **Config keys and secrets** in `config.sh`, `cluster_dot_conf`,
   `secrets_dot_env`, with tests.
3. **Registry `monitoring` field** and commands, with tests and
   `MAIN_DESIGN.md` updates.
4. **`healthchecks.py`** and **config rendering** (`monitor_config.py`,
   `monitoring.sh`), with tests.
5. **Runtime update** for hosts, QDevice, and prod VMs, with tests.
6. **Membership workflow integration** (`monitoring_sync` calls), with tests.
7. **`manage_prod_vm_monitoring.sh`**, **`show_monitoring_state.sh`**,
   **`send_test_alert.sh`**, with tests.
8. **Dashboard** tiles, menu item, workflows, fixtures, tests.
9. **Example app** `/api/v1/healthz`, deploy-script `guest.json` update, tests.
10. **Documentation**: README "Monitoring and Alerts" section (including the
    Gmail filter, Workspace allowlist, and Pushover guides, and the table of
    optional monitoring keys), the Cloudflare section (tunnel alert), the
    Tailscale section (add the QDevice ICMP rule with its comment to the
    required policy), the runtime-update section, `MAIN_DESIGN.md`; move this
    document to `docs/`.

The work is done when:

- [ ] `dev/run_tests.py` passes for the whole suite; `bash -n` passes for every
      changed shell script; dashboard tests pass.
- [ ] Nothing under `v3/` changed (`git diff --stat -- v3` is empty).
- [ ] With `MONITORING_ENABLED=false` (the default), behavior is unchanged:
      no agents run, no Healthchecks calls are made.
- [ ] With monitoring enabled, the runtime update deploys to every host, the
      QDevice, and every production VM, and every check reports `up`.
- [ ] Each condition G1–G11 produces the expected email in a test on the live
      cluster, or in an automated test where a live test is impractical (SMART
      failure, ZFS fault: use `simulate_disk_failure_and_replacement.sh` where
      appropriate).
- [ ] A pulse produces exactly one DOWN and one UP email; the UP is deleted by
      F1; the DOWN is forwarded by F2.
- [ ] Every check is named `<cluster> bmaccheck <node>`.
- [ ] Removing a host or prod VM deletes its check; removing the QDevice's
      quorum role keeps the QDevice's check and agent.
- [ ] The Production page has its own Monitored services tile; the QDevice page has
      a Health-check probes tile; `add_prod_vm.sh` asks no monitoring questions.
- [ ] Optional monitoring keys work from `cluster.conf` but are not in
      `cluster_dot_conf`.
- [ ] A staging clone of prod1 sends nothing.
- [ ] Adding or removing a production VM creates or deletes its check and
      updates the QDevice's probes; adding or removing a host updates every
      agent's peers.
- [ ] No ping URL or API key appears in any log, history file, UI protocol
      event, or pmxcfs file.

## 22. Decision log

| Decision | Reason |
|---|---|
| Healthchecks.io dead-man switch for liveness | Free (20 checks), external to the cluster, detects nodes that cannot report anything themselves. |
| Events sent through the same check as fail-then-clear pulses | A second `/fail` while down sends no email, and a check left down would hide a later dead-man event. Clearing immediately keeps one check per node and keeps the dead-man switch working. Verified: both emails arrive, both include the body. |
| Email as the only delivery channel; Gmail filters route to Pushover | Email is free and already used by Proxmox and Cloudflare. Filters give priority routing without any server. Pushover is optional. |
| No ntfy, Gotify, or Postmark | Self-hosted push needs a server and, for iOS, an upstream relay; hosted ntfy's free tier has public topics; Postmark's free tier caps at 100 emails/month with no overage, which an incident could exhaust. |
| No webhooks | Receiving them needs an inbound public endpoint, which BMAC never exposes. |
| QDevice monitor always required | Only node outside FiberState; only one that can test ingress through Cloudflare end to end; independent view of Tailscale. |
| ICMP-only Tailscale rule for QDevice → hosts | Works whether or not the QDevice runs qnetd; grants no ports or shell access. |
| `bmaccheck` (no underscore) in every check name, filters without the sender | Matching on the sender address was unreliable for the operator. Healthchecks puts the check name in every email's subject and body, so a word in the name identifies every BMAC email. |
| Cluster name at the start of every check name | Operators with several clusters can tell which one alerted, and names stay unique across clusters in one Healthchecks project. |
| Two-filter basic set; delete only "up" emails with `bmacclear` | Simple to set up; a DOWN email can never be deleted. |
| QDevice keeps its check when its quorum role is removed | It remains the cluster's external monitor. |
| Services and probes edited from their own Dashboard tiles, never asked at VM creation | A new VM has no app deployed yet, so there are no services to monitor or endpoints to probe; both are set after deploying the app. |
| Optional monitoring keys kept out of `cluster_dot_conf` | Keeps the example config approachable; defaults are sensible and documented in the README. |
| Single-word lowercase tokens | Gmail search ignores most punctuation; single words match reliably. |
| Mutually exclusive filters | Gmail applies every matching filter, in no particular order. |
| Trailing plain heartbeat after every clear | The most recent ping must never carry a token, in case Healthchecks includes it in a later dead-man email. |
| Debounce only frequent connectivity checks | Network checks blip; SMART, ZFS, capacity, and certificate problems must be reported on first detection. |
| Capacity hysteresis 10% / 12% | Avoids repeated pulses for a pool sitting near the threshold. |
| NVMe wear and spare-capacity checks | The hosts use NVMe drives only; smartd's defaults report health failures but not gradual wear, so BMAC warns at 90% rated endurance used and as spare capacity approaches the drive's own threshold, giving time to plan replacements. |
| Host disk checks duplicate smartd/ZED | Proxmox's Postfix direct delivery may be blocked or queued; BMAC's path is independent. |
| Probe settings in the registry; services and certificates on the VM | Probes are rendered into the QDevice config from cluster state; services describe the VM's own software and are edited in place. |
| Staging guard by hostname prefix in the agent | Staging clones inherit everything; every staging hostname starts with `stage`; no clone patching needed. |
| Health endpoint at `/api/v1/healthz` | BMAC's Cloudflare rules skip bot challenges for `/api/`, so probes from a datacenter IP are not challenged. |
| Deployment through `update_cluster_runtime.sh` | Brings monitoring to the existing cluster without a rebuild. |
