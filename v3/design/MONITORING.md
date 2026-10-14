<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v3 monitoring, alerting, and logs

How BMAC watches the cluster and its appliances, decides what needs
attention, notifies people, and keeps logs readable after a host dies.

## Goals

- The Overview's "Needs attention" panel and every page's status come from a
  live health model, not from someone pressing Refresh.
- Problems that need a person become alerts, with notification, a history,
  and acknowledgment.
- Someone hears about it even when the whole cluster is down.
- Logs and job history from any host stay readable on the other hosts after
  that host dies.

## Collectors

Each host's `bmacd` runs collectors for **its own host only**, using local
commands and files, never SSH. They replace the logic of v2's diagnostics
(`show_cluster_health.sh`, `show_proxmox_host_state.sh`, `show_prod_vm_state.sh`,
`show_qdevice_state.sh`, `show_cluster_state.sh`, the `list_*.sh` scripts, and
`host_storage.py`). They are native Rust from the start (`WORKFLOW_MIGRATION.md`).

| Collector | Source | Interval |
| --- | --- | --- |
| Quorum, membership, Corosync links | `pvecm status`, `corosync-cfgtool -n` | 15 s |
| CPU, memory, load, uptime | `/proc` | 15 s |
| HA resources and state | `ha-manager status` | 15 s |
| VRRP egress owner | keepalived state, `ip` | 15 s |
| Ingress | HAProxy generation status, local backend checks | 30 s |
| Guest agent of production VMs on this host | QEMU guest agent ping | 60 s |
| Replication jobs | `pvesr status` | 60 s |
| Pools, vdevs, errors, capacity, scrub, resilver, removal | `zpool status -j`, `zfs list -Hp` | 60 s |
| Failed and expected systemd units | `systemctl` | 60 s |
| QDevice votes | `pvecm status` (with quorum) | 15 s |
| QDevice address and qnetd reachability | `tailscale status --json`, a TCP connection to port 5403 | 60 s |
| Tailscale status | `tailscale status --json` | 60 s |
| LUKS mappings against crypttab and pool members | The pools collector's layout (`lsblk`, `/etc/crypttab`) | 60 s |
| Registry agreement with live state (duty host) | Read-only reconciliation, as v2's `reconcile --live` | 5 min |
| Deferred cleanup backlog | Registry | 5 min |
| Clock sync | `chronyc tracking` | 5 min |
| BMAC pmxcfs use | pmxcfs sizes | 5 min |
| SMART health of every disk | `smartctl -j` | 1 h |
| Certificate expiry (host HTTPS, Proxmox node) | Files | 24 h |
| Backup status per appliance | The appliance's check-ins (`BACKUP.md`, "Check-ins") | Each check-in (60 s) |
| Backup transfer rate per appliance and VM | `bmac-source` byte counts | 10 s during a transfer |
| Log copy lag per peer | Log store | 1 min |

Each collector writes its newest observation, with the time it was made, to
the host's SQLite. Selected numbers (CPU, memory, pool use, replication lag,
backup age, backup transfer rate, log lag) also go into a metric history at one-minute resolution
for 7 days, then hourly for 90 days. This feeds the small charts in the
Dashboard and the pool-full projections.

Every host receives every peer's observation changes over the live
host-to-host streams (`ARCHITECTURE.md`, "Cluster-wide view"), so any host can
show and push cluster-wide status to the browsers connected to it. A host
whose stream has dropped is shown as unreachable, with the age of its last
observation.

## Conditions

Every collector turns its observations into **conditions**. A condition has:

- an ID (for example `zfs.pool.capacity`);
- the resource it is about (for example `mox2/rpool`);
- a severity: `info`, `warning`, or `critical`;
- a plain-language message;
- the time it started;
- suggested next steps, each naming a workflow with prefilled values, as
  v2's "Suggested next steps" do.

The "Needs attention" panel lists current warning and critical conditions.
Info conditions show only on their resource's page. Conditions that depend on
several hosts are evaluated by the cluster duty host (`ARCHITECTURE.md`):
HA agreement, backup age per VM and appliance, and minimum healthy
appliances. "Quorum lost" is the exception: a partition without quorum has
no duty host, so every host raises it for itself and notifies directly, as
"Without quorum" below describes.

## Alerts

A condition becomes an **alert** when it has been at warning or critical for
its hold time (default: immediately for critical, 5 minutes for warning).
Alerts are stored in pmxcfs (`/etc/pve/priv/bmac/alerts/`), so every host
shows the same alerts and acknowledgments.

```text
OPEN -> ACKNOWLEDGED -> RESOLVED
  \______________________^
```

- An alert opens once per condition and resource. If the condition stays, it
  doesn't open again. If it clears and returns within 10 minutes, the same
  alert reopens instead of a new one.
- Acknowledging an alert records who did it, stops repeat notifications, and
  leaves it visible until resolved.
- Resolving happens automatically when the condition clears. A recovery
  notification is sent if the alert was notified.
- **Silences** suppress notifications for chosen conditions, resources, or
  hosts until a set time, for maintenance. The alerts still appear in the
  Dashboard.
- History is bounded (the newest 2,000 resolved alerts).

**Without quorum**, pmxcfs is read-only and alerts can't be written there.
Each host then keeps its alerts locally and notifies directly. Duplicates
from several hosts are possible during a partition; that is better than
silence. The local alerts are merged into pmxcfs when quorum returns.

## Notifications

Notifications go through a channel interface. The first channel is Postmark.

- The cluster's Postmark server token lives in
  `/etc/pve/priv/bmac/secrets/postmark.token` (`SECURITY.md`). Without one, no
  email is attempted and everything else works.
- The **sender address** is a cluster setting. Postmark accepts it only if
  it is a confirmed Sender Signature or on a verified domain of that
  Postmark account, so the test-send button is the way to prove it. Email
  is not attempted until a sender is set.
- **Routing rules** map a condition category and severity to recipients, with
  a throttle (default: at most one notification per alert per hour while it
  stays open and unacknowledged), and recovery messages on or off. The
  defaults email warnings and criticals to the default recipients and don't
  email routine successes.
- An optional daily digest lists open alerts and the backup state.
- **Outbox.** Each notification is a record in pmxcfs with an idempotency key
  made from the alert ID, its state, and its generation. The duty host sends
  it, records Postmark's message ID or error, and retries with exponential
  backoff (capped at 1 hour). A change of duty host may resend a message whose
  delivery wasn't recorded, so exactly-once delivery is not promised.
- Messages carry the cluster, host, resource, time (UTC and the cluster's
  zone), what is wrong, and the suggested next step with a link to the
  Dashboard. They never contain secrets or guest data. They are rendered by
  Postmark templates (see "Email templates").
- Settings, then Notifications has a test-send button. Failures to send show
  in the Dashboard. A notification transport failing for more than an hour is
  itself a Dashboard condition; it doesn't try to email about email.

**Sending.** Each notification is one `POST
https://api.postmarkapp.com/email/withTemplate` per recipient (Postmark
allows at most 50 recipients per message, and one message each keeps a bad
address from affecting the others). The request carries the
`X-Postmark-Server-Token` header, `From`, `To`, a `TemplateAlias`, a
`TemplateModel`, message stream `outbound`, a `Tag` naming the condition
category, and `Metadata` holding the alert ID. Open and link tracking are
off. TLS certificates are fully verified, and each request times out after
15 seconds.

A send succeeded only when the HTTP status is 2xx, the body's `ErrorCode`
is 0, and it has a `MessageID`. Otherwise, using Postmark's
[API error codes](https://postmarkapp.com/developer/api/overview):

| Result | Handling |
| --- | --- |
| Timeout, connection error, HTTP 429 or 5xx | Retry with backoff, as above |
| `ErrorCode` 10 (bad token) | Stop sending; Dashboard condition until the token is fixed |
| `ErrorCode` 400 or 401 (sender not a confirmed Sender Signature) | Kept in the outbox and retried hourly, or at once when the notification settings are saved; condition "Postmark refused the sender address" (critical). Sandbox servers refuse it too |
| `ErrorCode` 1101 (template not found) | Kept in the outbox, retried after the templates are created (see "Email templates"); condition "Notification templates missing" |
| `ErrorCode` 406 (inactive recipient) | No retry for that recipient; condition naming the address, which an operator reactivates in Postmark |
| Any other 4xx or `ErrorCode` | No retry; the error is recorded on the outbox record and shown |

### Email templates

Every email BMAC sends is a Postmark template, referenced by alias. The
server token that sends email can also manage templates through Postmark's
[Templates API](https://postmarkapp.com/developer/api/templates-api), so
templates are kept in Git and synchronized with that token, never edited
by hand in Postmark's web UI. Templates belong to one Postmark server (a
grouping of settings in a Postmark account, not a machine), the one the
token belongs to.

- **Definitions in Git.** `v3/notify/templates/` holds one directory per
  template (`subject.txt`, `body.txt`, `body.html`, in Postmark's Mustachio
  syntax) and a `manifest.json` listing each template's alias, name, type
  (`Standard` or `Layout`), layout, model fields, a test model, its state
  (`draft` or `released`), and, when released, a hash of its files.
- **Model fields** are snake_case and typed in the manifest. Every template
  gets the shared fields from its layout: `product_name` (`BMAC`),
  `cluster_name`, `dashboard_url`, and `sent_at`. Dashboard links in email
  point to a page, never carry a login token or any other credential, and
  ask for a login as usual.
- **One layout** (`bmac-layout-v1`) carries the shared header and footer.
  Postmark requires its content placeholder, `{{{ @content }}}`, exactly
  once in each of its bodies, and no subject. A template's type can't
  change after creation, and a layout can't be deleted while templates use
  it, so the layout is created first and retired last.
- **Aliases are versioned and prefixed:** `bmac-alert-opened-v1`,
  `bmac-alert-resolved-v1`, `bmac-digest-v1`, `bmac-test-v1`, and the
  appliance's `bmac-watchdog-down-v1`, `bmac-watchdog-up-v1`,
  `bmac-url-down-v1`, and `bmac-url-up-v1`. **A
  released template never changes,** because Postmark applies an edit to
  every later send at once, including from clusters still on the previous
  release. Any change after release creates the next version (`-v2`), and
  that release's code uses the new alias. A new layout version means new
  versions of every template that uses it.
- **Old versions are retired deliberately.** A Postmark server holds at
  most 100 templates (`ErrorCode` 1105 beyond that), so versions that no
  supported release refers to are removed with `retire`. The tool reports
  the count and refuses a sync that would exceed the limit.
- **Only BMAC's own templates are touched.** Anything without the `bmac-`
  prefix, or not in the manifest, is listed but never changed or deleted.
  Nothing is deleted because it is missing from Git.
- **Developer tool** `v3/dev/postmark_templates.py`, reading
  `POSTMARK_SERVER_TOKEN` from the environment (never from a file in the
  repository, never from its arguments, never printed):
  - Every command first calls `GET /server` and shows the server's name
    and delivery type (`Live` or `Sandbox`). That response includes the
    server's API tokens, so it is never printed or logged whole. Changes
    require the expected server name, from `--server <name>` or
    `POSTMARK_SERVER_NAME`, to match, so a sync can't land on the wrong
    server by accident. A `Sandbox` server is suggested for development.
  - The server token is enough for everything here: template CRUD,
    validation, `GET /server`, and sending all take
    `X-Postmark-Server-Token`. Postmark's account token, which controls
    every server in the account, is needed only for account-wide calls
    (copying templates between servers, managing servers, sender
    signatures, and domains). BMAC doesn't use those, so neither the tool
    nor `bmacd` asks for an account token.
  - `list`: every template on the server (paging through `GET /templates`),
    marked as BMAC's or foreign, with the count against the limit.
  - `diff`: the manifest compared with the server, field by field.
  - `validate`: each template checked with `POST /templates/validate`
    and its test model; it fails unless `AllContentIsValid` is true, and
    unless the response's `SuggestedTemplateModel` uses exactly the
    manifest's fields (Postmark checks syntax, not that the model is
    complete).
  - `sync`: validates, then creates the layout and missing templates,
    updates only `draft` ones with their complete definition, and stops if
    a `released` template on the server differs from Git (drift is
    reported, never overwritten). It shows its plan and changes nothing
    without `--apply`. Unexpected responses are errors, never a reason to
    create another template.
  - `retire <alias>`: deletes one old version on request, after checking
    that no supported release refers to it.
  - Postmark's `/templates/push` (copying between servers) needs an
    account token and is not used.
- **Tests** never contact Postmark. A released template's files still
  match their recorded hash, so editing one instead of making a new version
  fails. The Rust model type for each template has exactly the manifest's
  fields. In a release build, every alias the code uses is `released` in
  the manifest. Values are inserted only with escaped `{{ }}`; the only raw
  `{{{ }}}` allowed is the layout's `@content` placeholder. Test models
  contain no secret-shaped values.
- **At run time,** `bmacd` embeds the manifest and never creates or edits
  templates while sending. Settings, then Notifications, checks that every
  alias this version needs exists on the cluster's Postmark server with
  matching content. Missing ones can be created from the embedded copies
  with one click (create only, layout first); a template whose content
  differs is reported and left alone. If a send fails with `ErrorCode` 1101
  (template not found), `bmacd` sends nothing untemplated: the message stays
  in the outbox, the Dashboard raises "Notification templates missing", and
  the message is retried once the templates are created.

## The appliance as an external watchdog

A cluster that is completely down can't send anything. Each backup appliance
therefore watches the cluster:

- It reads the cluster's heartbeat (the `/v1/heartbeat` endpoint of
  `bmac-source` on each host, `BACKUP.md`) every minute.
- If no host has answered for 5 minutes (configurable), the appliance sends
  an alert through its own notification settings. The appliance has its own
  Postmark token, configured on the appliance, separate from the cluster's.
  Its messages use the same template rules ("Email templates"), with their
  own aliases (`bmac-watchdog-down-v1`, `bmac-watchdog-up-v1`) in the same
  manifest.
- It reports which hosts stopped answering and when, and sends a recovery
  notification when they return.
- Optionally, it checks configured public URLs of the production
  applications from outside, every minute, and alerts when one has given
  no good answer (a 2xx or 3xx status within 15 seconds) for the same
  number of minutes, with `bmac-url-down-v1` and `bmac-url-up-v1`. This
  is the only BMAC check made from outside the cluster.

The cluster watches each appliance in return, from its check-ins: late,
delinquent, `LOCKED`, pool health, capacity, scrub errors, and backup age are
cluster conditions.

## Default conditions

| Condition | Default severity |
| --- | --- |
| Quorum lost | critical |
| Host unreachable | critical |
| HA resource in error, or production VM not running | critical |
| Production failover started / finished / failed | warning / info / critical |
| Production guest agent not answering | warning |
| Ingress generation not applied on a host, or backend down | critical |
| VRRP egress owner missing or duplicated | critical |
| Replication job failing, or lag over 3× its interval | warning (critical over 1 hour) |
| Pool degraded / faulted | critical |
| Pool free under 20% / 10% (calculated as in `STORAGE.md`) | warning / critical |
| Pool projected full within 14 days | warning |
| Disk SMART failing, or growing reallocated/pending sectors | critical / warning |
| Scrub found errors | critical |
| Boot redundancy reduced (spare in use for a boot member) | warning |
| LUKS mapping or crypttab mismatch | critical |
| QDevice needed but missing, or votes inconsistent | critical |
| Corosync link down | warning |
| Failed BMAC or Proxmox unit | warning |
| Clock unsynchronized | warning |
| Registry disagrees with live state | warning |
| Deferred cleanup stuck over 24 hours | warning |
| BMAC pmxcfs use over 8 MiB / 14 MiB | warning / critical |
| HTTPS certificate expires within 60 / 14 days | warning / critical |
| Hosts run different BMAC versions (outside an update) | warning |
| Job failed or interrupted | warning |
| PIT capture failed | critical |
| VM's newest published PIT older than its target (per appliance) | warning |
| Fewer healthy appliances than the configured minimum for a VM | critical |
| Appliance not checking in for 3 minutes (LATE) / locked | warning (critical after 24 hours) / warning |
| Appliance delinquent (no check-in, or nothing published, for 72 hours) | critical |
| Backup skipped: no appliance available for a VM's scheduled PIT | critical |
| An available appliance didn't claim, or failed, an offered PIT | warning |
| Appliance pool or namespace quota over 80% / 90% | warning / critical |
| Restore test failed | critical |
| Log copy from a peer more than 5 minutes behind | warning |
| Log storage forced to evict audit events | warning |
| Notification transport failing over 1 hour | warning (Dashboard only) |
| Login failures above threshold, or an enrollment token reused | warning |

Every threshold is a cluster setting, and every condition has an on/off
switch in the alert settings: a condition that is off is still shown on its
resource's page, but opens no alert and sends no email.

## Logs

### What is stored

Each host stores in its SQLite (`/var/lib/bmac/bmac.db`), with large output
in compressed segment files next to it:

- job events (`ARCHITECTURE.md`), including captured command output, in
  chunks of at most 64 KiB, never loaded whole into memory;
- audit events (`SECURITY.md`);
- `bmacd`'s own log;
- a bounded **journal collector**: warning-and-above entries from
  `pve-ha-crm`, `pve-ha-lrm`, `corosync`, `pve-cluster`, `pvescheduler`,
  `zfs-zed`, kernel storage and hardware errors, keepalived, and BMAC's own
  units, rate-limited per unit. More units or lower levels are opt-in. Guest
  logs are never collected.

When a host's health turns critical, its collectors store a snapshot of the
relevant recent journal entries at once, so the evidence is copied to peers
before the host goes down.

Every record has a key `(origin host, stream, sequence)`. `stream` changes
when `bmacd` starts a new incarnation; the sequence increases within a
stream. Secrets are redacted before anything is stored (`SECURITY.md`).
Each record also carries a SHA-256 over the previous record's hash and its
own key, time, kind, job, and body, so a copy can be checked against its
stream. A job's events are stored before they are published, and the
engine's job view pages through them: at most 2,000 events or about 1 MiB
per page. When a job holding more than 64 KiB of events finishes, its events
move unchanged into deflate frames of at most 256 KiB each, appended to
`segments/<job>.seg` next to the database. Each frame is synced before its
rows point at it, so a crash leaves at worst unreferenced bytes at the end of
the file. A reader decompresses one frame at a time. A stdout line longer
than 9 MiB stops the script.

### Copying logs between hosts

Copying has two parts, both over host-to-host TLS:

- **Live.** Each host's live stream to every peer (`ARCHITECTURE.md`,
  "Cluster-wide view") carries the host's own new records as they are
  written, so copies normally lag by about a second. The receiver checks
  each record against its stream's hash chain and stores it. After a
  reconnect it asks for the stream from the last record it stored, so
  nothing needs acknowledging; the sender finishes that stream, then sends
  each of its later incarnations from the start.
- **Catch-up.** Every minute, and whenever a peer connects, each host asks
  every peer what it holds, for **every origin it knows**, not just that
  peer, and fetches what it is missing:

  ```text
  GET /v1/holdings
  GET /v1/records?origin=<host>&stream=<id>&after=<sequence>&limit=<n>
  ```

  So if host A held copies of dead host C's records, a rebuilt host B gets
  them from A.

Records are inserted idempotently by key and never rewritten, so retries and
overlaps are harmless. Each record carries the hash of the one before it, so
a copy is checked against its stream even when its predecessor hasn't
arrived yet, and a gap is filled from any peer that has it.

Copying runs at low priority with a bandwidth cap, and backs off during heavy
ZFS sends or replication. It never blocks jobs, HA, or backups. If no peer is
reachable, records stay local and the newest unreplicated records may be
lost if the host dies; the Dashboard shows this.

### Retention

Each host keeps its own records and its copies of everyone else's within one
budget: 30 days or 8 GiB, whichever comes first, reduced automatically on
small root filesystems so free space never drops below 15%. Eviction order:

1. captured command output of successful jobs;
2. other job events;
3. the journal collector's records;
4. audit events and failure records, last.

An evicted record keeps its key, time, and hash, so peers never send it
again; records deleted for age are remembered per stream the same way.
Evicting audit events raises a condition. A removed host's records stay on
the others until they age out.

### In the Dashboard

- **Activity** lists jobs from all hosts: operation, user, host, phase,
  state, and times. Opening a job replays its events from the local store,
  then follows new ones over the event stream. A job whose host died shows
  its last copied event and
  "outcome unknown", never a guessed result.
- **Logs** searches all stored records by host, job, severity, unit, and
  time, with paging, and exports a search as a file.
- **Settings, then Log replication** shows, per origin and per peer: the last
  sequence copied, lag in records and time, gaps, bytes stored, and the oldest
  record kept.
