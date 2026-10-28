#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The registry's backup-linked staging VMs: a staging record whose disk is a
qcow2 overlay over one of its source's restore points on PBS."""

from __future__ import annotations

import json
from pathlib import Path
import subprocess
import tempfile
import unittest


# The registry rejects paths with symlinked components (macOS /var).
tempfile.tempdir = str(Path(tempfile.gettempdir()).resolve())

REGISTRY = Path(__file__).resolve().parent.parent / "host_runtime" / "cluster_registry.py"
SNAPSHOT = "vm/100/2026-10-27T16:04:23Z"
OVERLAY = "bmac-overlay:901/vm-901-disk-1.qcow2"


class BackupLinkedStagingRegistryTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.state = Path(self.temporary.name) / "registry"
        self.run_registry(
            "init", "--network", "10.213.0.0/24", "--guest-gateway", "10.213.0.10",
            "--mox-ip-start", "10.213.0.11", "--mox-ip-end", "10.213.0.20",
            "--vm-slot-start", "10.213.0.110", "--max-production", "10",
            "--max-staging-per-production", "9",
        )
        self.run_registry(
            "allocate-prod", "--vmid", "100", "--purpose", "myapp",
            "--primary-domain", "example.com", "--placement", "mox1,mox2", "--initial-node", "mox1",
        )
        self.run_registry("update", "prod1", "--owner-node", "mox1")

    def run_registry(self, *arguments: str, expected: int = 0, stdin: str | None = None):
        completed = subprocess.run(
            [str(REGISTRY), "--state-dir", str(self.state), *arguments],
            text=True, capture_output=True, check=False, input=stdin,
        )
        self.assertEqual(
            completed.returncode, expected,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        parsed = json.loads(completed.stdout) if completed.stdout.strip() else None
        return parsed, completed

    def allocate(self, *extra: str, expected: int = 0):
        return self.run_registry(
            "allocate-staging", "--source", "prod1", "--vmid", "901", "--placement", "mox2",
            "--placement-limit", "5", *extra, expected=expected,
        )

    def test_a_backup_linked_slot_records_its_restore_point(self) -> None:
        link = ("--backup-storage", "bmac-pbs", "--backup-snapshot", SNAPSHOT, "--allocation-id", "link-1")
        result, _ = self.allocate(*link)
        resource = result["resource"]
        self.assertEqual((resource["name"], resource["ip"]), ("stage1prod1", "10.213.0.111"))
        self.assertEqual(resource["backup"], {"storage": "bmac-pbs", "snapshot": SNAPSHOT})
        self.assertIsNone(resource["proxmox"]["snapshot"])
        # The same request again is the same allocation.
        again, _ = self.allocate(*link)
        self.assertFalse(again["created"])
        self.assertEqual(again["resource"]["name"], "stage1prod1")

    def test_prod_linked_records_are_unchanged(self) -> None:
        result, _ = self.allocate()
        self.assertNotIn("backup", result["resource"])

    def test_the_backup_link_is_validated(self) -> None:
        _, completed = self.allocate("--backup-snapshot", SNAPSHOT, expected=1)
        self.assertIn("--backup-snapshot and --backup-storage go together", completed.stderr)
        _, completed = self.allocate("--backup-storage", "bmac-pbs", "--backup-snapshot", "vm/100/latest",
                                     expected=1)
        self.assertIn("backup snapshot must look like vm/VMID/", completed.stderr)
        _, completed = self.allocate("--backup-storage", "bmac:pbs", "--backup-snapshot", SNAPSHOT, expected=1)
        self.assertIn("backup storage must be a PVE storage ID", completed.stderr)

    def test_its_volume_must_be_its_own_overlay(self) -> None:
        self.allocate("--backup-storage", "bmac-pbs", "--backup-snapshot", SNAPSHOT)
        updated, _ = self.run_registry("update", "stage1prod1", "--state", "cloning", "--owner-node", "mox2",
                                       "--volume-id", OVERLAY)
        self.assertEqual(updated["proxmox"]["volume_id"], OVERLAY)
        for volume in ("local-zfs:vm-901-disk-0", "bmac-overlay:902/vm-902-disk-1.qcow2"):
            with self.subTest(volume=volume):
                _, completed = self.run_registry("update", "stage1prod1", "--volume-id", volume, expected=1)
                self.assertIn("must be its own qcow2 overlay", completed.stderr)

    def test_cleanup_releases_after_the_vm_and_overlay_are_destroyed(self) -> None:
        allocated, _ = self.allocate("--backup-storage", "bmac-pbs", "--backup-snapshot", SNAPSHOT)
        resource_id = allocated["resource"]["id"]
        for state in ("cloning", "patching", "stopped", "active", "stopping"):
            self.run_registry("update", "stage1prod1", "--state", state,
                              *(("--owner-node", "mox2", "--volume-id", OVERLAY) if state == "cloning" else ()))
        queued = []
        for action, target in (("destroy-vm", "vm:901"), ("destroy-overlay", OVERLAY)):
            row, _ = self.run_registry("defer-cleanup", "--resource", "stage1prod1", "--node", "mox2",
                                       "--action", action, "--target", target, "--reason", "test")
            queued.append(row["cleanup"]["id"])
        self.run_registry("update", "stage1prod1", "--state", "cleanup_pending")
        _, completed = self.run_registry("finalize-staging-cleanup", "stage1prod1", "--resource-id", resource_id,
                                         expected=1)
        self.assertIn("deferred cleanup remains pending", completed.stderr)
        self.run_registry("reconcile", "--observed", "-", "--apply",
                          stdin=json.dumps({"schema_version": 1, "cleanup_completed": queued}))
        released, _ = self.run_registry("finalize-staging-cleanup", "stage1prod1", "--resource-id", resource_id)
        self.assertTrue(released["released"])

    def test_a_zvol_destruction_does_not_release_an_overlay(self) -> None:
        _, completed = self.run_registry("defer-cleanup", "--resource", "prod1", "--node", "mox2",
                                         "--action", "destroy-overlay", "--target", "local-zfs:vm-901-disk-0",
                                         "--reason", "test", expected=1)
        self.assertIn("destroy-overlay cleanup target must be STORAGE:VMID/vm-VMID-disk-N.qcow2", completed.stderr)

    def test_production_records_cannot_be_backup_linked(self) -> None:
        record_path = self.state / "resources" / "prod1.json"
        record = json.loads(record_path.read_text())
        record["backup"] = {"storage": "bmac-pbs", "snapshot": SNAPSHOT}
        record_path.write_text(json.dumps(record))
        _, completed = self.run_registry("list", expected=1)
        self.assertIn("only staging resources can be backup-linked", completed.stderr)


if __name__ == "__main__":
    unittest.main()
