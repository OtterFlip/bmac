#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for scripts/user_callable/guests/prod/reconcile_prod_vm_owner.sh."""

from __future__ import annotations

import json
from pathlib import Path
import shlex
import subprocess
import tempfile
import unittest

SCRIPT = (
    Path(__file__).resolve().parent.parent
    / "user_callable" / "guests" / "prod" / "reconcile_prod_vm_owner.sh"
)


def production(name: str, vmid: int, owner: str, targets: list[str], state: str = "active") -> dict:
    return {
        "kind": "production", "name": name, "index": int(name[4:]), "vmid": vmid,
        "state": state, "owner_node": owner, "placement": ["mox1", "mox2"],
        "proxmox": {"replication_targets": targets},
    }


class ReconcileProdOwnerTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def plan(self, resources: list, vms: list, jobs: list) -> list[list[str]]:
        paths = []
        for name, value in (("resources", resources), ("vms", vms), ("jobs", jobs)):
            path = self.root / f"{name}.json"
            path.write_text(json.dumps(value), encoding="utf-8")
            paths.append(shlex.quote(str(path)))
        completed = subprocess.run(
            ["bash", "-c", f"RECONCILE_PROD_OWNER_SOURCE_ONLY=1 source {shlex.quote(str(SCRIPT))}\n"
             f"plan_rows {' '.join(paths)}"],
            capture_output=True, text=True, check=True,
        )
        return [line.split("\t") for line in completed.stdout.splitlines()]

    def test_failover_is_recorded_and_everything_else_is_explained(self) -> None:
        rows = self.plan(
            [
                production("prod1", 100, "mox1", ["mox2"]),
                production("prod2", 101, "mox1", ["mox2"]),
                production("prod3", 102, "mox1", ["mox2"]),
                production("prod4", 103, "mox1", ["mox2"]),
                production("prod5", 104, "mox1", ["mox2"], state="provisioning"),
                {"kind": "staging", "name": "stage1prod1", "vmid": 200, "state": "active"},
            ],
            [
                {"vmid": 100, "type": "qemu", "status": "running", "node": "mox2", "name": "prod1"},
                {"vmid": 101, "type": "qemu", "status": "running", "node": "mox1", "name": "prod2"},
                {"vmid": 102, "type": "qemu", "status": "stopped", "node": "mox1", "name": "prod3"},
                {"vmid": 103, "type": "qemu", "status": "running", "node": "mox2", "name": "prod4"},
            ],
            [
                {"guest": 100, "target": "mox1"},
                {"guest": 101, "target": "mox2"},
                {"guest": 103, "target": "mox2"},
            ],
        )
        self.assertEqual(
            rows,
            [
                ["prod1", "mox1", "mox2", "mox2", "mox1", "record"],
                ["prod2", "mox1", "mox1", "mox2", "mox2", "ok"],
                ["prod3", "mox1", "-", "mox2", "-", "skip: not running"],
                ["prod4", "mox1", "mox2", "mox2", "mox2",
                 "skip: replication jobs do not fit placement; run change_prod_vm_placement.sh"],
            ],
        )

    def test_shell_syntax_and_safety(self) -> None:
        subprocess.run(["bash", "-n", str(SCRIPT)], check=True)
        text = SCRIPT.read_text(encoding="utf-8")
        self.assertIn("record-owner", text)
        for forbidden in ("ha-manager", "qm ", "relocate"):
            self.assertNotIn(forbidden, text)


if __name__ == "__main__":
    unittest.main()
