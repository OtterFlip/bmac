#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The registry's instant_restore marker: a production VM under Instant
Restore to Existing VM, whose phases only go forward, and which is no source
for prod-linked staging VMs until it is cleared."""

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


class InstantRestoreRegistryTest(unittest.TestCase):
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

    def run_registry(self, *arguments: str, expected: int = 0):
        completed = subprocess.run([str(REGISTRY), "--state-dir", str(self.state), *arguments],
                                   text=True, capture_output=True, check=False)
        self.assertEqual(completed.returncode, expected,
                         msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}")
        return (json.loads(completed.stdout) if completed.stdout.strip() else None), completed

    def start(self, expected: int = 0):
        return self.run_registry("update", "prod1", "--instant-restore-storage", "bmac-pbs",
                                 "--instant-restore-snapshot", SNAPSHOT, expected=expected)

    def phase(self, phase: str, expected: int = 0):
        return self.run_registry("update", "prod1", "--instant-restore-phase", phase, expected=expected)

    def staging(self, *extra: str, expected: int = 0):
        return self.run_registry("allocate-staging", "--source", "prod1", "--vmid", "901", "--placement", "mox2",
                                 "--placement-limit", "5", *extra, expected=expected)

    def test_phases_only_go_forward_and_freed_cannot_be_cleared(self) -> None:
        record, _ = self.start()
        self.assertEqual(record["instant_restore"], {"storage": "bmac-pbs", "snapshot": SNAPSHOT, "phase": "planned"})
        # Repeating the start, as a resumed run does, changes nothing.
        again, _ = self.start()
        self.assertEqual(again["revision"], record["revision"])
        _, completed = self.run_registry("update", "prod1", "--instant-restore-snapshot",
                                         "vm/100/2026-10-26T10:00:00Z", expected=1)
        self.assertIn("another snapshot", completed.stderr)
        self.phase("freed")
        _, completed = self.phase("planned", expected=1)
        self.assertIn("cannot go back from freed to planned", completed.stderr)
        _, completed = self.run_registry("update", "prod1", "--clear-instant-restore", expected=1)
        self.assertIn("freed the VM's zvol; finish it", completed.stderr)
        self.phase("switched")
        cleared, _ = self.run_registry("update", "prod1", "--clear-instant-restore")
        self.assertNotIn("instant_restore", cleared)

    def test_only_the_vms_own_restore_points(self) -> None:
        _, completed = self.run_registry("update", "prod1", "--instant-restore-storage", "bmac-pbs",
                                         "--instant-restore-snapshot", "vm/101/2026-10-27T16:04:23Z", expected=1)
        self.assertIn("one of the VM's own", completed.stderr)
        _, completed = self.run_registry("update", "prod1", "--instant-restore-snapshot", SNAPSHOT, expected=1)
        self.assertIn("needs --instant-restore-storage", completed.stderr)

    def test_no_prod_linked_staging_vm_during_an_instant_restore(self) -> None:
        self.start()
        _, completed = self.staging(expected=1)
        self.assertIn("prod1 is under an instant restore", completed.stderr)
        # A backup-linked one depends on PBS only.
        self.staging("--backup-storage", "bmac-pbs", "--backup-snapshot", SNAPSHOT)
        self.phase("switched")
        self.run_registry("update", "prod1", "--clear-instant-restore")
        self.staging("--vmid", "902", "--allocation-id", "prod-linked")

    def test_none_starts_while_a_prod_linked_staging_vm_exists(self) -> None:
        self.staging()
        _, completed = self.start(expected=1)
        self.assertIn("while prod-linked staging VMs exist", completed.stderr)


if __name__ == "__main__":
    unittest.main()
