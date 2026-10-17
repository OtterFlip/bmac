<!--
Copyright (c) 2026 BEENTHERE VENTURES, INC.
SPDX-License-Identifier: GPL-3.0-only
-->

# BMAC v3 implementation status

Where the implementation of [`IMPLEMENTATION_PLAN.md`](IMPLEMENTATION_PLAN.md)
stands, and where to pick it up again. Written on 2026-10-16, at commit
`b005f20`, after M10 was finished.

## Summary

Milestones M0 to M10 are built and proven in the lab, with the exceptions
listed below. M11 (native ports) and M12 (release readiness) haven't been
started. The largest gap is **M6.10, the backup appliance ISO**, which was
never built: the Deployer's Backup appliances page is a placeholder, and
the lab's appliances were never installed from an ISO. They were Ubuntu
24.04 cloud-image VMs made by `lab/lab create`, into which the scenarios
installed the `bmac-backupd` package over SSH and then ran `create-pool`,
`init`, and `enroll` (`lab/scenarios/m9_backup.py`), which is the "Installing
from a package" path in `DEPLOYMENT.md`.

The gaps were found by checking every sub-milestone's **Builds** text, and
the design documents it implements, against the code. They fall into three
kinds: items the plan deferred or marked owed and nothing picked up again,
features the design describes that were never built, and lab checks that
were never written.

## Lab state

Three labs, each with the stages `m7-first-host`, `m7-cluster`, and
`m8-storage` and the snapshots `blank` and `installed`, all built with the
upgrade-aware installers: `bmacnvme` (`lab/lab.toml`), `bmacsata`
(`lab/lab-sata.toml`), and `bmacscsi` (`lab/lab-scsi.toml`). The last full
runs passed on all three: `m10_update` (SATA 74/74, SCSI 71/71, NVMe
72/72), and before it the M9 scenarios (`m9_spike`, `m9_backup`,
`m9_restore`, `m9_accept`). Every item of `BACKUP.md`'s "Lab proofs" is
proven.

## Gaps, by importance

### Needed for real use

- **M6.10, appliance ISO.** No Ubuntu Server autoinstall remaster, no
  Deployer flow, no appliance handoff; `deployer/src/app/App.tsx` renders
  `PlaceholderPage` for the appliances page. Everything else the ISO would
  use exists: the enrollment bundle (Dashboard, Backups, Add appliance), the
  `bmac-backupd` package and its `create-pool`, `init`, `admin-password`,
  and `enroll` commands, the `tag:bmac-backup-appliance` check in
  `crates/bmac-deployer/src/tailnet.rs`, the handoff and shredding used for
  hosts, and the remaster method proven for the discovery ISO.
- **Appliance quotas (M9.1, M9.3).** Enrollment always records
  `quota_bytes: None` (`crates/bmac-backupd/src/enroll.rs`) and no quota is
  set on a cluster's namespace. Check-ins carry the appliance's capacity,
  but `bmacd` doesn't store it (`crates/bmacd/src/backup/source.rs`), so
  the "appliance pool or quota filling" alert (80% and 90%) can't be raised.
  `BACKUP.md` relies on quotas to stop a cluster filling an appliance.
- **LUKS header download (M7.3, phase 8).** Headers are kept under
  `/var/lib/bmac/luks-headers/` and appliances pull them, but the Dashboard
  download that `DEPLOYMENT.md` promises has no API call or page. Without
  an appliance, the host holds the only copy.
- **Alert conditions (M5.2).** Still owed, as the plan's M5.2 notes say:
  the thresholds as cluster settings (`conditions::Thresholds` defaults are
  used), a spare in use, a job failed or interrupted, login failures over a
  threshold, and an enrollment token reused. Also missing: replication lag
  over three intervals (`MONITORING.md`); only `replication.failing` exists.
- **Metric history in the Dashboard (M5.1).** `crates/bmacd/src/metrics.rs`
  keeps the history and the pool-full projection uses it, but there is no
  API to read it and no charts. Backup transfer rates are kept per transfer
  under `backup/transfers/`, not in the metric history, and there is no
  10-second transfer-rate collector.

### Smaller features

- **M6.2, inventory:** no SMART health summary, and no controller or
  "RAID logical disk" fields, both listed in `DEPLOYMENT.md` ("The
  discovery session", step 4).
- **M6.3, discovery:** no "measure again" for a NIC that never got carrier.
- **M6.4, host card:** written to a file; no print action. No offer to
  delete the cached source ISO, as v2's `offer_to_delete_source_iso` did.
- **M7.2, join bundles:** the Dashboard lists outstanding bundles only, not
  each one as `unused`, `used`, `expired`, or `revoked`; revoking drops it.
- **M3.4, guest pages:** no `ssh_config` block and `known_hosts` line for
  workstations without the Deployer.
- **M4.3, staging sanitizer:** how the sanitizer file reaches the host was
  left to M4.3 and never decided (`legacy/script-operations.json`);
  `add_staging_vm.sh` still asks for a path on the host.
- **M10, appliance upgrades:** `DEPLOYMENT.md` says appliances upgrade from
  their own UI or with `apt`; only `apt` exists.
- **M10.2, record schema writes:** the update job raises the cluster's
  schema range in `/etc/pve/priv/bmac/release.json`, and reads refuse newer
  schemas, but nothing holds back writes of a new schema version until
  every host can read it. Harmless until a schema goes past version 1.
- **M8.3, dry runs:** storage workflows show their plan in the
  confirmation, but have no dry-run mode (`supports_dry_run: false`).
- **M5.4, log catch-up:** a fixed 2 MiB/s limit
  (`crates/bmacd/src/mesh/catchup.rs`); the backoff under heavy ZFS sends
  or replication, which waited for M8, was never built.

### Deferred on purpose

- **M2.3, WebAuthn:** refused until proven through BMAC's origin in the
  lab (`SECURITY.md`); TOTP and recovery codes only.
- **M9.1, application-consistent hooks:** `Consistency::Application` is
  reserved; PITs are crash-consistent, or filesystem-consistent through the
  guest agent's freeze.
- **M9.6, the 8-day soak** with default settings: not built as a scenario
  option and not run.
- **M7.2, QDevice during joins:** a join refuses while a QDevice is
  configured, instead of removing and restoring it as v2 did. Add the
  QDevice after the last join.

### Lab checks never written

- **M2.1:** `systemd-analyze security` on the units, and probing the
  front end's forbidden paths (`crates/bmacd/tests/units.rs` checks the
  unit files only).
- **M9.6, `BACKUP.md` security tests:** `m9_accept` covers cluster
  credentials refused by the appliance, an untrusted peer refused, a wrong
  fingerprint, and the secret-leak scan. Not in the lab: a stolen
  certificate sending non-BMAC or unassigned datasets, one-way trust,
  peers unable to modify data (unit-tested in `export_tests.rs`), and
  quotas stopping a fill (needs quotas first).

## Not started

- **M11, native ports:** every script operation replaced by Rust through
  the port gate.
- **M12, release readiness:** hardware runs, the security review, and the
  documentation. The hardware run planned next is two R640s as Proxmox
  hosts (two NVMe disks each, a two-disk boot mirror), a third R640 with
  four NVMe disks as the backup appliance, and a Linode VPS as the QDevice.
  Until M6.10 is built, the appliance is installed by hand: Ubuntu Server
  24.04, the `bmac-backupd` package, then `create-pool` (at the console),
  `init`, `admin-password`, and `enroll` with its VLAN address.

## Where to pick up

1. M6.10, the appliance ISO, with a lab scenario that installs an
   appliance VM from it.
2. Appliance quotas and the capacity alert, and the LUKS header download.
3. The missing alert conditions and replication lag.
4. The hardware run (M12.1), then the remaining smaller features, the
   missing lab checks, M11, and the rest of M12.

Releases are built with `dev/release.py build --key KEY
--check-reproducible` from a clean, committed tree (packaging/README.md,
"Releases"). A cluster refuses a downgrade, so a second release meant for
testing an update needs a higher version in `dev/VERSION`.
