# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

from pathlib import Path
import subprocess
import unittest


SCRIPT = Path(__file__).with_name("destroy_prod_vm.sh")


class DestroyProductionVmTest(unittest.TestCase):
    def test_shell_syntax(self) -> None:
        completed = subprocess.run(
            ["bash", "-n", str(SCRIPT)],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)

    def test_help_describes_destructive_scope_and_retained_cache(self) -> None:
        completed = subprocess.run(
            [str(SCRIPT), "--help"],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("Permanently removes", completed.stdout)
        self.assertIn("source ISO cache is deliberately retained", completed.stdout)

    def test_ordering_and_fail_closed_guards_are_present(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        required = (
            "destroy staging dependents first",
            "every production placement node must be online",
            "VM tags differ from exact production identity",
            "rule_resources != {sid}",
            "DESTROY ${RESOURCE_NAME}",
            "orchestration-acquire",
            "--routes-disabled",
            "--lock-timeout 240",
            "Waiting up to 240 seconds for any current HAProxy route sync",
            "ha-manager set",
            "ha-manager remove",
            "ha-manager rules remove",
            "pvesr delete",
            "qm destroy",
            "--destroy-unreferenced-disks 0",
            "Stopping the non-HA production VM",
            "VM configuration changed before destruction",
            "--clear-owner-node",
            "--clear-volume",
            'registry_cmd release "$RESOURCE_NAME"',
        )
        for value in required:
            self.assertIn(value, text)

        self.assertLess(
            text.index("--routes-disabled"),
            text.index('ha-manager set "vm:${VMID}" --state stopped'),
        )
        self.assertLess(
            text.index('ha-manager remove "vm:${VMID}"'),
            text.index('qm destroy "$VMID"'),
        )
        self.assertLess(
            text.index('pvesr delete "$job"'),
            text.index('qm destroy "$VMID"'),
        )
        self.assertLess(
            text.index('qm destroy "$VMID"'),
            text.index('registry_cmd release "$RESOURCE_NAME"'),
        )

    def test_shared_source_cache_is_never_removed(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        self.assertIn(
            'CACHE_ISO="/var/lib/app-ha-proxmox/iso-cache/',
            text,
        )
        self.assertNotIn(
            'rm -f -- "/var/lib/app-ha-proxmox/iso-cache',
            text,
        )


if __name__ == "__main__":
    unittest.main()
