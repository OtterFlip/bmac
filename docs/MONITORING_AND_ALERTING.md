<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v2 Monitoring and Alerting

Status: design, approved for implementation. Target location in the repository:
`docs/MONITORING_AND_ALERTING.md`.

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
  `json`, `subprocess`, `ssl`, `socket`; no third-party packages.
- **Reuse, don't duplicate, parsing logic.** `show_cluster_health.sh` already
  parses `corosync-cfgtool -n` link status, `zpool status -x`, rpool free space
  (`RPOOL_MIN_FREE_FRACTION = 0.10`), and `smartctl -H`. Move the shared parsing
  into the agent module (or a small shared module) and make
  `show_cluster_health.sh` use it, so both agree. Keep the existing diagnostic's
  output unchanged except where it reads better.
- **Never put secrets in pmxcfs.** Healthchecks ping URLs and the API key are
  secrets (see [§17](#17-security)).
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
| `scripts/user_callable/guests/prod/manage_prod_vm_monitoring.sh` | workstation | n/a | view/update a prod VM's monitored services, certificate paths, and ingress probe |
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

All filter terms are single lowercase alphanumeric words. Gmail search ignores
most punctuation, so tokens must not contain brackets, colons, or spaces.

| Token | Meaning | Appears in |
|---|---|---|
| `bmacevent` | this ping reports an event | the body of every pulse `/fail` |
| `bmacp1` | high priority event | pulse `/fail` body, with `bmacevent` |
| `bmacp2` | normal priority event | pulse `/fail` body, with `bmacevent` |
| `bmacclear` | this success only clears a pulse | the body of every clearing success ping |

**Hard rule:** no other ping body may contain any token. The agent must
sanitize every message string (problem text, hostnames, unit names, file paths,
command output excerpts) by replacing any case-insensitive occurrence of
`bmacevent`, `bmacclear`, `bmacp1`, `bmacp2` (and the prefix `bmac` followed by
those suffixes) with a neutralized form such as `bmac-event`. Routine heartbeat
bodies never contain tokens.

### Sequences

Event pulse:

```
POST <ping_url>/fail      body: event body (tokens on line 1)
POST <ping_url>           body: clear body ("bmacclear" on line 1)
POST <ping_url>           body: heartbeat body (no tokens)
```

The trailing plain heartbeat makes sure the most recent ping never carries a
token. If Healthchecks includes the last ping's body in a later dead-man "DOWN"
email, that email must not contain `bmacclear` (which would make the clear
filter delete it). Gmail filters also use the subject as a second guard (see
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
| `qdevice-vote` | host | QDevice vote expected but absent |
| `cloudflared` | host | this host's tunnel replica not ready |
| `postfix-queue` | host | mail older than the limit waiting in the queue |
| `zfs:<pool>` | host | `zpool status -x` reports a problem |
| `capacity:<pool>` | host | pool free space below threshold |
| `smart:<disk-id>` | host | SMART health failed or a watched counter rose |
| `smart-read:<disk-id>` | host | `smartctl` failed or timed out on a disk |
| `tailscale:<host>` | qdevice | ICMP over Tailscale to a host fails |
| `qnetd:<host>` | qdevice | host not connected to qnetd while the QDevice is registered |
| `ingress:<fqdn>` | qdevice | probe failed |
| `ingress-challenged:<fqdn>` | qdevice | Cloudflare returned a challenge instead of the app |
| `service:<unit>` | guest | monitored unit not active |
| `disk:<mountpoint>` | guest (and qdevice) | filesystem free space below threshold |
| `cert:<path>` | guest | certificate expires soon, has expired, or cannot be read |
| `agent:<check>` | any | a check itself crashed (exception) |

`<disk-id>` is the `/dev/disk/by-id/` name (serial-based) when available, so the
key survives device renumbering.

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
| Quorum | fast | `corosync-quorumtool -s` (or `pvecm status`; verify) | not quorate | p1 |
| QDevice vote | fast | same output | the cluster expects a QDevice (QDevice registered) and its vote is not present | p1 |
| Own tunnel replica | fast | GET `http://127.0.0.1:20241/ready` (cloudflared metrics; verify) | not HTTP 200, or ready connections 0 | p2 |
| Postfix queue | fast (cheap) | `postqueue -j` | any message older than `MONITOR_POSTFIX_QUEUE_MAX_AGE_MINUTES` (default 60); include oldest age, recipient domain, and delay reason | p2 |
| ZFS health | slow | `zpool status -x` per pool, plus `zpool status -P -p` for detail | anything but "is healthy" | p1 |
| Pool capacity | slow | `zfs list -H -p -o used,available <pool>` for every pool from `zpool list -H -o name` | free fraction below `MONITOR_POOL_MIN_FREE_PERCENT` (10); clears above `MONITOR_POOL_CLEAR_FREE_PERCENT` (12) | p2; p1 below 5% |
| SMART health | slow | `smartctl -j -H -A <disk>` for each whole disk (same selection rule as `show_cluster_health.sh`: `lsblk` type disk, excluding zd/rbd/nbd/zram/ram) | `smart_status.passed` is false | p1 |
| SMART counters | slow | same JSON | an increase since the stored baseline in ATA attributes 5 (Reallocated_Sector_Ct), 197 (Current_Pending_Sector), 198 (Offline_Uncorrectable), or NVMe `critical_warning` non-zero, `media_errors` increased | p2 (critical_warning: p1) |
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

### 6.2 QDevice (`role: qdevice`)

| Check | Speed | How | Problem when | Priority |
|---|---|---|---|---|
| Tailscale reachability of each host | fast | resolve each host's Tailscale IPv4 from `tailscale status --json` (match `HostName` or `DNSName` to the host's `TAILSCALE_HOSTNAME`), then `ping -c 1 -W 3 <ip>`; include the peer's `Online` and `LastSeen` from the same JSON in the message | ping fails | p2; p1 if every host fails at once (QDevice-side Tailscale problem or whole-site outage) |
| qnetd connections | fast | `corosync-qnetd-tool -l` (verify format) | only when the QDevice is registered for the cluster: an expected host is not connected | p1 if the QDevice vote is currently needed (even host count), else p2 |
| Ingress probes | fast (every 60s, not 30s) | for each probe in config: HTTPS GET `https://<fqdn><path>`, timeout 10s, no redirects followed, header `User-Agent: bmac-monitor/<version>`, `Cache-Control: no-cache` | status differs from expected, or the optional expected text is not in the body, or connection/TLS error | p1 |
| Challenge detection | with probes | response has header `cf-mitigated: challenge` (verify header name) | the probe was challenged, so the result is inconclusive | p2, key `ingress-challenged:<fqdn>` |
| Own disk | slow | `df -P` on real filesystems | below 10% free (clears above 12%) | p2 |

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

The README's Tailscale section must list this as a required rule.

### 6.3 Production VM (`role: guest`)

| Check | Speed | How | Problem when | Priority |
|---|---|---|---|---|
| Staging guard | every cycle | `socket.gethostname()` | hostname starts with `stage`: the agent sends nothing and does nothing else | n/a |
| Liveness | 60s | heartbeat | (the heartbeat reaching Healthchecks proves guest egress works) | n/a |
| Monitored services | fast | `systemctl is-active <unit>` for each unit in `/etc/app-ha-monitor/guest.json` `services` | not `active` | p1 |
| Guest filesystems | slow | `df -P -B1` on real filesystems (skip tmpfs, devtmpfs, overlay, squashfs, efivarfs) | below 10% free, clears above 12% | p2; p1 below 5% |
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
  - `reset-smart-baseline <disk-id>`.
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
  "smart_baselines": {"ata-XYZ": {"5": 0, "197": 0, "198": 0}},
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

### New keys in `config/cluster.conf` (and `config/cluster_dot_conf`)

`scripts/lib/config.sh` rejects unknown keys, so add each key to
`_PROXMOX_CONFIG_CLUSTER_KEYS` with validation, and document each one in
`cluster_dot_conf` with a comment.

| Key | Default | Validation |
|---|---|---|
| `MONITORING_ENABLED` | `false` | `true`/`false` |
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
| `MONITOR_PROBE_DEFAULT_PATH` | `/api/v1/healthz` | absolute path starting with `/` |
| `MONITOR_PROBE_DEFAULT_STATUS` | `200` | integer 100-599 |

When `MONITORING_ENABLED=true`, `HEALTHCHECKS_API_KEY` must be set in
`secrets.env` and `PROXMOX_QDEVICE_HOST` must be set and reachable.

### New key in `config/secrets.env` (and `config/secrets_dot_env`)

```
# Healthchecks.io project API key (read-write). Used only by the workstation
# to create, update, and delete this cluster's checks and to read their status.
# Nodes never receive it; each node receives only its own check's ping URL.
HEALTHCHECKS_API_KEY=redacted
```

### Registry: production VM monitoring settings

Ingress probe settings for each production VM live in its registry resource
record, because the QDevice config is rendered from the registry and the
settings must survive workstation changes. They contain no secrets.

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
runs inside it, and the operator manages them with the Dashboard tile
([§13](#13-workflows-scripts-and-the-dashboard)):

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
    "reminder_hours": 6
  },
  "host": {
    "expected_peers": ["mox2", "mox3"],
    "qdevice_expected": false
  },
  "qdevice": {
    "hosts": [{"name": "mox1", "tailscale_hostname": "mox1"}],
    "qnetd_expected": false,
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
| `name` | `<PROXMOX_CLUSTER_NAME>/<node>` (e.g. `MyAppCloud/mox1`) |
| `slug` | `<cluster-lower>-<node>` |
| `tags` | `bmac bmac-cluster-<cluster-lower> bmac-role-<role>` |
| `desc` | `BMAC <role> <node> in cluster <cluster>. Managed by BMAC; do not edit.` |
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

| Workflow | Why |
|---|---|
| `add_proxmox_host.sh` | new check; every host's `expected_peers` and the QDevice's host list change; QDevice expectations may change (even/odd count) |
| `remove_proxmox_host.sh` | delete the removed host's check (show it in the plan; the existing `REMOVE moxN` confirmation covers it); update peers |
| `add_qdevice.sh` / `remove_qdevice.sh` | `qdevice_expected` and `qnetd_expected` change on every host and the QDevice |
| `add_prod_vm.sh` | new check, guest agent, QDevice probe |
| `remove_prod_vm.sh` | delete the VM's check and probe (the agent goes with the VM) |
| `manage_prod_vm_monitoring.sh` | probe settings changed |
| `change_prod_vm_owner.sh`, `change_prod_vm_placement.sh` | no monitoring change needed (the guest agent is inside the VM and follows it); do not call |

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
only).

Behavior:

1. Read the VM's registry record (`monitoring.probe`) and its
   `/etc/app-ha-monitor/guest.json` over `ssh <name>`.
2. Show the current settings: monitored services (with each unit's current
   `systemctl is-active` state), certificate paths (with expiry dates), and the
   ingress probe (enabled, FQDNs, path, expected status, expected text).
3. Present one form (a `bmac_ui_group_*` request) to edit:
   - services: list of systemd unit names; validate each exists on the VM
     (`systemctl cat <unit>` succeeds) and matches `^[A-Za-z0-9@._:-]+$`;
   - certificate paths: absolute paths; validate each is a readable PEM
     certificate on the VM;
   - probe enabled; FQDNs (chosen from the record's domains); path; expected
     status; expected text (optional).
4. Show a plan of changes and confirm.
5. Write `guest.json` atomically on the VM (temp file then `mv`, mode 0600).
6. Update the registry `monitoring` field with `--expected-revision`.
7. Run `monitoring_sync` (re-renders the QDevice config).
8. Optionally run one probe immediately from the QDevice and show the result.

Also accept non-interactive flags for terminal use:
`--add-service UNIT`, `--remove-service UNIT`, `--add-cert PATH`,
`--remove-cert PATH`, `--probe-path PATH`, `--probe-status N`,
`--probe-text TEXT`, `--probe-fqdn FQDN` (repeatable), `--probe-enable`,
`--probe-disable`, `--show` (read-only).

### Dashboard

- Add `manage_prod_vm_monitoring` to the `WorkflowTiles` list on
  `dashboard/src/pages/ProductionPage.tsx` (the "Production operations"
  section), titled "Monitoring settings", and a row menu item "Monitoring
  settings…" that launches it with `{ resource: vm.name }`.
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
- Supports `--reset-smart-baseline <host>:<disk-id>` (the one mutating
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
| `add_prod_vm.sh` | Ask (form) whether to enable the ingress probe (default yes), the path (default `MONITOR_PROBE_DEFAULT_PATH`), expected status (default `MONITOR_PROBE_DEFAULT_STATUS`), optional expected text, and an optional list of services. Store probe settings in the registry at creation; install the guest agent; write `guest.json`; run `monitoring_sync`; wait for the first ping. |
| `remove_prod_vm.sh` | Delete the VM's check as part of the plan; run `monitoring_sync`. |
| `deploy_hello_app_to_prod.sh` | After installing the app, add `hello-todo.service` and `/etc/hello-todo/cert.pem` to `guest.json` if monitoring is enabled. |
| `add_proxmox_host.sh`, `remove_proxmox_host.sh`, `add_qdevice.sh`, `remove_qdevice.sh` | Call `monitoring_sync` as in [§12](#12-membership-changes). |
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
endpoint under `/api/` and set it with the Monitoring settings tile.

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
- Filters search the whole message, including the ping body that Healthchecks
  puts in its email.
- Every filter is anchored to the Healthchecks sender address and the email's
  subject, so it cannot touch any other mail. Copy the exact sender address
  from a Healthchecks email you already received; confirm the subject wording
  for "down" and "up" emails the same way (shown below as `DOWN` and `UP`).
- Before creating forwarding filters, add each Pushover address as a
  forwarding address (Settings → Forwarding and POP/IMAP → Add a forwarding
  address). Gmail sends a confirmation message to it; Pushover turns that into
  a notification on your phone containing the confirmation code.
- In Google Workspace, an admin must allow automatic forwarding for the user
  (Admin console → Apps → Google Workspace → Gmail → Routing → "Allow users to
  automatically forward incoming email to another address"; describe the exact
  location at implementation time).
- Filters only act on mail that arrives after they are created.

Create these filters (Settings → Filters and Blocked Addresses → Create a new
filter → type the query in "Has the words"; leave other fields empty):

| # | Purpose | Query (Has the words) | Actions |
|---|---|---|---|
| F1 | A node went silent (dead-man) | `from:(HC_SENDER) subject:(DOWN) -bmacevent` | Never send it to Spam; Apply label `BMAC`; Forward to your **high-priority** Pushover address |
| F2 | High-priority event | `from:(HC_SENDER) subject:(DOWN) bmacevent bmacp1` | Never send it to Spam; Apply label `BMAC`; Forward to your **high-priority** Pushover address |
| F3 | Normal event | `from:(HC_SENDER) subject:(DOWN) bmacevent -bmacp1` | Never send it to Spam; Apply label `BMAC`; Forward to your **normal** Pushover address |
| F4 | Event cleared (noise) | `from:(HC_SENDER) subject:(UP) bmacclear` | Delete it |
| F5 | A node is back after going silent | `from:(HC_SENDER) subject:(UP) -bmacclear` | Never send it to Spam; Apply label `BMAC`; optionally Forward to your **normal** Pushover address |

Why they cannot overlap: DOWN vs UP by subject; F1 vs F2/F3 by `bmacevent`;
F2 vs F3 by `bmacp1`; F4 vs F5 by `bmacclear`. If a dead-man email ever
contains `bmacevent` from an earlier ping, F2 or F3 forwards it instead of F1,
so it is still pushed. Only F4 deletes, and only UP emails.

Optional filters for the other alert sources (anchored to their senders):

| Purpose | Query | Actions |
|---|---|---|
| Cloudflare tunnel alert | `from:(CLOUDFLARE_NOTIFICATION_SENDER) tunnel` | Never send it to Spam; label; forward to high-priority Pushover address |
| Proxmox email | `from:(PROXMOX_FROM_ADDRESS)` | Never send it to Spam; label; forward to normal Pushover address |

Test the whole chain with `send_test_alert.sh` for p1 and p2, and by stopping
`app-ha-monitor.service` on a production VM for about four minutes (dead-man
DOWN, then UP when started again).

### 15.5 Pushover

- What it is: a commercial push service; a one-time purchase per platform
  after a 30-day trial; each user can send up to 10,000 messages per month.
- Create two email aliases in the Pushover dashboard: one with high priority
  (and a distinct sound), one with normal priority. Every user also has a
  default `@pomail.net` address. Pushover provides the receiving address only,
  so mail is forwarded to it by Gmail.
- Optionally, use Pushover's emergency priority on the high alias if it is
  offered for email aliases (verify), which repeats until acknowledged.
- Send one test email from Gmail to each alias before setting up forwarding.

### 15.6 Keeping Proxmox's own emails out of spam (Google Workspace)

Proxmox delivers its own alert email directly to the recipient's mail server
(Postfix with no relay), without SPF or DKIM for your domain. With Google
Workspace recipients:

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
  protocol stream. Redact them as `https://hc-ping.com/<redacted>`.
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
  `zfs list` capacity, `smartctl -j` (passed, failed, counter increase, NVMe
  critical warning, read error), `postqueue -j` (empty, old message),
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
  invalid certificate path, probe edits, JSON-mode forms via `ui_test_driver.py`.
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

These were not verified against a live system or current vendor documentation.
Ask the operator to run the capture commands on the live cluster
(`ssh mox1`, `ssh qdevice`, `ssh prod1`) and save the outputs as test fixtures.
Never commit real ping URLs, IPs that should stay private, or serial numbers
the operator does not want published; anonymize fixtures.

| # | Fact | How to verify / capture |
|---|---|---|
| V1 | Healthchecks Management API details in [§10](#10-healthchecksio-integration) | Read `https://healthchecks.io/docs/api/`; try `GET /api/v3/checks/` with the key |
| V2 | Subject wording of Healthchecks "down" and "up" emails (used by filters) | Look at the emails already received during testing |
| V3 | Whether Healthchecks' dead-man DOWN email includes the last ping's body or recent pings | Send a pulse, wait for the trailing heartbeat, stop pinging, read the DOWN email |
| V4 | Quorum and QDevice vote output | `ssh mox1 'pvecm status; corosync-quorumtool -s'` (the current 3-host cluster has no QDevice; capture again after any QDevice is registered, or use Proxmox docs examples) |
| V5 | Corosync link output | `ssh mox1 'corosync-cfgtool -n'` (already parsed in `show_cluster_health.sh`) |
| V6 | `corosync-qnetd-tool -l` format | only on a registered QDevice; use upstream examples until available |
| V7 | cloudflared readiness endpoint | `ssh mox1 'curl -s -i http://127.0.0.1:20241/ready'` |
| V8 | `postqueue -j` output | `ssh mox1 'postqueue -j | head'` (empty is normal) |
| V9 | `smartctl -j -H -A` JSON per disk type | `ssh mox1 'smartctl --scan; for d in $(smartctl --scan | cut -d" " -f1); do smartctl -j -H -A $d; done'` |
| V10 | `tailscale status --json` fields `HostName`, `DNSName`, `Online`, `LastSeen`, `TailscaleIPs` | `ssh qdevice 'tailscale status --json'` |
| V11 | ICMP over Tailscale from the QDevice works with the new rule | `ssh qdevice 'ping -c 2 <mox1 tailscale ip>'` |
| V12 | Cloudflare challenge response header (`cf-mitigated: challenge`) | Cloudflare docs; or probe a non-API path from the QDevice |
| V13 | systemd sandbox settings do not block the tools | run `check --once` under the unit on each role |
| V14 | Pushover email aliases support priority (and whether emergency priority is available) | Pushover dashboard |
| V15 | Exact Workspace admin locations for automatic forwarding and the email allowlist | Admin console |

## 20. Rollout to the existing cluster

The operator's cluster: three Proxmox hosts (`mox1`–`mox3`), a QDevice VPS on
standby (`ssh qdevice` works; not voting because the host count is odd), one
production VM (`ssh prod1` via jump SSH). The Tailscale ICMP rule is already in
the policy. Cloudflare tunnel email alerts are already configured.

Steps, after the implementation is merged:

1. Healthchecks: create the read-write API key ([§10](#10-healthchecksio-integration));
   add `HEALTHCHECKS_API_KEY` to `secrets.env`.
2. `cluster.conf`: add `MONITORING_ENABLED=true` and any non-default
   thresholds.
3. `update_cluster_runtime.sh --dry-run`, review: 5 checks to create
   (`mox1`, `mox2`, `mox3`, `qdevice`, `prod1`), agents to install on all five
   nodes. Budget 5/20.
4. `update_cluster_runtime.sh`. Expect all five checks to leave `new` and
   report `up`. prod1's registry record has no `monitoring` field yet, so no
   ingress probe runs until step 6.
5. Redeploy the example app to get `/api/v1/healthz`:
   `deploy_hello_app_to_prod.sh` for prod1 (this also adds `hello-todo.service`
   and its certificate to prod1's `guest.json`).
6. Dashboard → Production → **Monitoring settings** for prod1: enable the
   probe with path `/api/v1/healthz`, status 200.
7. Create the Gmail filters and Pushover aliases ([§15](#15-things-outside-bmac-readme-material)).
8. `send_test_alert.sh` for one host with p1 and the QDevice with p2; confirm
   the DOWN emails are forwarded and the UP clears are deleted.
9. Dead-man test: `ssh prod1 systemctl stop app-ha-monitor`, wait about 4
   minutes for the DOWN email, then start it again and confirm the UP email
   (F5) is kept.
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
    Gmail filter, Workspace allowlist, and Pushover guides), Cloudflare and
    Tailscale section updates, runtime-update section, `MAIN_DESIGN.md`,
    move this document to `docs/`.

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
      F4; the DOWN is forwarded by F2 or F3.
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
| Single-word lowercase tokens | Gmail search ignores most punctuation; single words match reliably. |
| Filters anchored to sender and subject | Gmail applies every matching filter; anchoring prevents touching other mail and makes the filter set mutually exclusive. |
| Trailing plain heartbeat after every clear | The most recent ping must never carry a token, in case Healthchecks includes it in a later dead-man email. |
| Debounce only frequent connectivity checks | Network checks blip; SMART, ZFS, capacity, and certificate problems must be reported on first detection. |
| Capacity hysteresis 10% / 12% | Avoids repeated pulses for a pool sitting near the threshold. |
| Host disk checks duplicate smartd/ZED | Proxmox's Postfix direct delivery may be blocked or queued; BMAC's path is independent. |
| Probe settings in the registry; services and certificates on the VM | Probes are rendered into the QDevice config from cluster state; services describe the VM's own software and are edited in place. |
| Staging guard by hostname prefix in the agent | Staging clones inherit everything; every staging hostname starts with `stage`; no clone patching needed. |
| Health endpoint at `/api/v1/healthz` | BMAC's Cloudflare rules skip bot challenges for `/api/`, so probes from a datacenter IP are not challenged. |
| Deployment through `update_cluster_runtime.sh` | Brings monitoring to the existing cluster without a rebuild. |
