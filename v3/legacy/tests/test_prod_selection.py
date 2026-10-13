# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""prod_select_production uses the operation's --resource without asking."""

from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path

PROD_HA = Path(__file__).resolve().parent.parent / "scripts" / "lib" / "prod_ha.sh"


def production(name: str, index: int) -> dict:
    return {
        "name": name,
        "index": index,
        "kind": "production",
        "state": "active",
        "vmid": 1000 + index,
        "revision": 3,
        "owner_node": "mox1",
        "placement": ["mox1", "mox2"],
        "proxmox": {
            "volume_id": f"bmac-zfs-64k:vm-{1000 + index}-disk-0",
            "ha_nodes": ["mox1", "mox2"],
            "replication_targets": ["mox2"],
        },
        "spec": {"memory_mb": 4096, "storage": "bmac-zfs-64k"},
    }


HARNESS = """
set -euo pipefail
source "$1"
die() { printf 'DIE: %s\\n' "$*"; exit 3; }
prompt_with_default() { echo PROMPTED; exit 9; }
bmac_ui_choose() { echo PROMPTED; exit 9; }
RUN_DIR="$2"
SELECTED_RESOURCE="$3"
prod_select_production "Production VM to relocate"
printf 'RESOURCE=%s VMID=%s STORAGE=%s\\n' "$RESOURCE_NAME" "$VMID" "$VM_STORAGE"
"""


class ProdSelectionTest(unittest.TestCase):
    def select(self, resource: str) -> subprocess.CompletedProcess[str]:
        with tempfile.TemporaryDirectory() as run_dir:
            rows = [production("prod1", 1), production("prod2", 2)]
            Path(run_dir, "resources.json").write_text(json.dumps(rows), encoding="utf-8")
            return subprocess.run(
                ["bash", "-c", HARNESS, "bash", str(PROD_HA), run_dir, resource],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                stdin=subprocess.DEVNULL,
                check=False,
            )

    def test_given_resource_is_used_without_asking(self) -> None:
        completed = self.select("prod2")
        self.assertEqual(completed.returncode, 0, completed.stdout + completed.stderr)
        self.assertEqual(completed.stdout, "RESOURCE=prod2 VMID=1002 STORAGE=bmac-zfs-64k\n")

    def test_unknown_resource_is_refused(self) -> None:
        completed = self.select("prod7")
        self.assertEqual(completed.returncode, 3, completed.stdout + completed.stderr)
        self.assertNotIn("PROMPTED", completed.stdout)
        self.assertIn("DIE: Invalid production VM selection", completed.stdout)
        self.assertIn("prod7 is not one registered production resource", completed.stderr)

    def test_root_volume_off_the_recorded_storage_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as run_dir:
            row = production("prod1", 1)
            row["proxmox"]["volume_id"] = "local-zfs:vm-1001-disk-0"
            Path(run_dir, "resources.json").write_text(json.dumps([row]), encoding="utf-8")
            completed = subprocess.run(
                ["bash", "-c", HARNESS, "bash", str(PROD_HA), run_dir, "prod1"],
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                stdin=subprocess.DEVNULL,
                check=False,
            )
        self.assertEqual(completed.returncode, 3, completed.stdout + completed.stderr)
        self.assertIn("no registered root volume on its storage bmac-zfs-64k", completed.stderr)

    def test_without_resource_it_asks(self) -> None:
        completed = self.select("")
        self.assertEqual(completed.returncode, 9, completed.stdout + completed.stderr)
        self.assertEqual(completed.stdout, "PROMPTED\n")


if __name__ == "__main__":
    unittest.main()
