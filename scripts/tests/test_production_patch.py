# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for the production mode of the offline guest patchers
(scripts/utilities/patch_staging_guest_tree.py and patch_staging_clone.sh),
which Full Restore to New VM uses to give a restored VM its slot's identity."""

from __future__ import annotations

import json
import os
from pathlib import Path
import stat
import subprocess
import sys
import tempfile
import unittest


UTILITIES = Path(__file__).resolve().parents[1] / "utilities"
TREE_HELPER = UTILITIES / "patch_staging_guest_tree.py"
CLONE_HELPER = UTILITIES / "patch_staging_clone.sh"
ADMIN_KEYS = "ssh-ed25519 AAAAone admin1\nssh-ed25519 AAAAtwo admin2\n"


def guest_tree(root: Path) -> None:
    for relative in ("etc/netplan", "etc/cloud/cloud.cfg.d", "etc/ssh", "etc/app-ha-monitor",
                     "etc/systemd/system/multi-user.target.wants", "var/lib/cloud/instance",
                     "var/lib/dbus", "var/log", "usr/local/lib", "root/.ssh"):
        (root / relative).mkdir(parents=True, exist_ok=True)
    (root / "etc/hostname").write_text("prod1\n")
    (root / "etc/hosts").write_text("127.0.0.1 localhost\n127.0.1.1 prod1\n")
    (root / "etc/shadow").write_text("root:$6$old$hash:20000:0:99999:7:::\ndaemon:*:20000:0:99999:7:::\n")
    (root / "etc/shadow").chmod(0o640)
    (root / "etc/netplan/00-installer-config.yaml").write_text("network: {}\n")
    (root / "etc/machine-id").write_text("original-machine-id\n")
    (root / "var/lib/dbus/machine-id").write_text("original-machine-id\n")
    (root / "etc/ssh/ssh_host_ed25519_key").write_text("original host key\n")
    (root / "etc/app-ha-monitor/config.json").write_text('{"ping_url": "https://hc-ping.com/original"}\n')
    (root / "var/lib/cloud/instance/state").write_text("done\n")
    (root / "root/.ssh/authorized_keys").write_text("ssh-ed25519 AAAAold former-admin\n")


class ProductionTreePatchTest(unittest.TestCase):
    def patch(self, *extra: str, expected: int = 0) -> tuple[Path, subprocess.CompletedProcess[str]]:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        base = Path(temporary.name)
        root = base / "root"
        guest_tree(root)
        hash_file = base / "root.hash"
        hash_file.write_text("$6$cluster$newhash\n")
        keys = base / "keys"
        keys.write_text(ADMIN_KEYS)
        completed = subprocess.run(
            [sys.executable, str(TREE_HELPER), "--root", str(root), "--name", "prod3",
             "--address", "10.213.0.130/24", "--gateway", "10.213.0.10", "--mac", "02:AB:00:00:00:03",
             "--dns", "1.1.1.1,1.0.0.1", "--root-hash-file", str(hash_file), *extra],
            capture_output=True, text=True, check=False)
        self.assertEqual(completed.returncode, expected, completed.stderr)
        return root, completed

    def test_same_slot_keeps_the_machine_identity(self) -> None:
        root, _ = self.patch("--mode", "production", "--authorized-keys-file", self.keys_path())
        self.assertEqual((root / "etc/hostname").read_text(), "prod3\n")
        self.assertIn("127.0.1.1 prod3", (root / "etc/hosts").read_text())
        self.assertFalse((root / "etc/netplan/00-installer-config.yaml").exists())
        netplan = json.loads((root / "etc/netplan/50-app-ha-production.yaml").read_text())
        private = netplan["network"]["ethernets"]["private"]
        self.assertEqual((private["match"]["macaddress"], private["set-name"], private["addresses"]),
                         ("02:ab:00:00:00:03", "lan0", ["10.213.0.130/24"]))
        self.assertIn("root:$6$cluster$newhash:", (root / "etc/shadow").read_text())
        keys = root / "root/.ssh/authorized_keys"
        self.assertEqual(keys.read_text(), ADMIN_KEYS)
        self.assertEqual(stat.S_IMODE(keys.stat().st_mode), 0o600)
        self.assertEqual(stat.S_IMODE((root / "root/.ssh").stat().st_mode), 0o700)
        # The machine, its host keys, cloud-init, and monitoring stay as backed up.
        self.assertEqual((root / "etc/machine-id").read_text(), "original-machine-id\n")
        self.assertTrue((root / "etc/ssh/ssh_host_ed25519_key").exists())
        self.assertTrue((root / "var/lib/cloud/instance/state").exists())
        self.assertFalse((root / "etc/cloud/cloud-init.disabled").exists())
        self.assertTrue((root / "etc/app-ha-monitor/config.json").exists())
        self.assertFalse((root / "etc/systemd/system/app-ha-ssh-host-keys.service").exists())

    def test_another_slot_gets_a_new_machine_identity(self) -> None:
        root, _ = self.patch("--mode", "production", "--authorized-keys-file", self.keys_path(),
                             "--new-identity")
        self.assertEqual((root / "etc/machine-id").read_text(), "")
        self.assertFalse((root / "var/lib/dbus/machine-id").exists())
        self.assertFalse((root / "etc/ssh/ssh_host_ed25519_key").exists())
        unit = (root / "etc/systemd/system/app-ha-ssh-host-keys.service").read_text()
        self.assertIn("for this production guest", unit)
        # It must not report to the original VM's Healthchecks check.
        self.assertFalse((root / "etc/app-ha-monitor/config.json").exists())
        self.assertTrue((root / "var/lib/cloud/instance/state").exists())

    def test_modes_take_only_their_own_options(self) -> None:
        _, completed = self.patch("--mode", "production", expected=2)
        self.assertIn("production mode needs --authorized-keys-file", completed.stderr)
        _, completed = self.patch("--authorized-keys-file", self.keys_path(), expected=2)
        self.assertIn("are for production mode", completed.stderr)

    def test_administrator_keys_must_be_public_keys(self) -> None:
        bad = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: [p.unlink() for p in bad.iterdir()] and bad.rmdir())
        (bad / "keys").write_text("-----BEGIN OPENSSH PRIVATE KEY-----\n")
        _, completed = self.patch("--mode", "production", "--authorized-keys-file", str(bad / "keys"),
                                  expected=1)
        self.assertIn("OpenSSH public keys", completed.stderr)

    def keys_path(self) -> str:
        directory = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: [p.unlink() for p in directory.iterdir()] and directory.rmdir())
        (directory / "keys").write_text(ADMIN_KEYS)
        return str(directory / "keys")


class ProductionClonePatchTest(unittest.TestCase):
    """patch_staging_clone.sh in production mode: what it accepts, and that the
    restored disk may belong to its own VM only while that VM is stopped."""

    def run_sourced(self, body: str, *, expected: int = 0, status: str = "stopped") -> subprocess.CompletedProcess[str]:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        base = Path(temporary.name)
        bin_dir = base / "bin"
        bin_dir.mkdir()
        (bin_dir / "pvesh").write_text(
            "#!/usr/bin/env bash\n"
            'case "$2" in\n'
            f"  /cluster/resources) printf '%s\\n' '[{{\"vmid\":105,\"type\":\"qemu\",\"node\":\"mox1\",\"status\":\"{status}\"}}]' ;;\n"
            "  /nodes/mox1/qemu/105/config) printf '%s\\n' '{\"name\":\"prod3\",\"scsi0\":\"local-zfs:vm-105-disk-1,size=1T\"}' ;;\n"
            "  *) exit 9 ;;\n"
            "esac\n")
        (bin_dir / "pvesh").chmod(0o755)
        (base / "hash").write_text("$6$x$y\n")
        (base / "hash").chmod(0o600)
        (base / "keys").write_text(ADMIN_KEYS)
        completed = subprocess.run(
            ["bash", "-c", f'APP_HA_STAGING_PATCH_SOURCE_ONLY=1 source "$1"; shift\n'
                           f'PATH={str(bin_dir)!r}:$PATH\nHASH={str(base / "hash")!r}\nKEYS={str(base / "keys")!r}\n{body}',
             "bash", str(CLONE_HELPER)],
            capture_output=True, text=True, check=False)
        self.assertEqual(completed.returncode, expected, completed.stderr)
        return completed

    def test_the_restored_vm_may_own_its_disk_while_stopped(self) -> None:
        body = "VOLUME_ID=local-zfs:vm-105-disk-1\nOWNER_VMID=105\nassert_not_referenced_by_vm\n"
        self.run_sourced(body)
        completed = self.run_sourced(body, status="running", expected=1)
        self.assertIn("VM 105 still references local-zfs:vm-105-disk-1", completed.stderr)
        # Without production mode's owner, any reference is refused, as for staging.
        completed = self.run_sourced("VOLUME_ID=local-zfs:vm-105-disk-1\nassert_not_referenced_by_vm\n", expected=1)
        self.assertIn("still references", completed.stderr)

    def test_production_mode_arguments(self) -> None:
        common = ('--device /dev/zvol/rpool/data/vm-105-disk-1 --volume-id local-zfs:vm-105-disk-1 '
                  '--address 10.213.0.130/24 --gateway 10.213.0.10 --mac 02:ab:00:00:00:03 '
                  '--dns 1.1.1.1 --root-hash-file "$HASH"')
        def checks(arguments: str, expected: int = 0) -> subprocess.CompletedProcess[str]:
            # validate_request's mode checks, without its root and file-path checks.
            body = (f"parse_args {common} {arguments}\n"
                    + "eval \"$(declare -f validate_request | sed -e 's/((EUID == 0)) ||/true ||/')\"\n"
                    + "validate_request\n")
            return self.run_sourced(body, expected=expected)

        checks('--mode production --name prod3 --owner-vmid 105 --authorized-keys-file "$KEYS" --new-identity')
        self.assertIn("Production name must use the prodN formula",
                      checks('--mode production --name stage1prod3 --owner-vmid 105 --authorized-keys-file "$KEYS"',
                             expected=1).stderr)
        self.assertIn("--owner-vmid must be the VMID that owns the volume",
                      checks('--mode production --name prod3 --owner-vmid 106 --authorized-keys-file "$KEYS"',
                             expected=1).stderr)
        self.assertIn("are for production mode",
                      checks('--name stage1prod3 --owner-vmid 105', expected=1).stderr)


    def test_overlay_mode_arguments(self) -> None:
        """--qcow2 patches a backup-linked staging VM's overlay through qemu-nbd."""
        directory = Path(tempfile.mkdtemp())
        self.addCleanup(lambda: [p.unlink() for p in directory.rglob("*") if p.is_file()])
        overlay = directory / "images" / "901" / "vm-901-disk-1.qcow2"
        overlay.parent.mkdir(parents=True)
        overlay.write_bytes(b"QFI")
        common = ('--address 10.213.0.111/24 --gateway 10.213.0.10 --mac 02:ab:00:00:01:01 '
                  '--dns 1.1.1.1 --root-hash-file "$HASH" --name stage1prod1')

        def checks(arguments: str, expected: int = 0) -> subprocess.CompletedProcess[str]:
            body = (f"parse_args {common} {arguments}\n"
                    + "eval \"$(declare -f validate_request | sed -e 's/((EUID == 0)) ||/true ||/')\"\n"
                    + "validate_request\n")
            return self.run_sourced(body, expected=expected)

        checks(f"--qcow2 {overlay} --volume-id bmac-overlay:901/vm-901-disk-1.qcow2")
        self.assertIn("Volume id must be the overlay's own",
                      checks(f"--qcow2 {overlay} --volume-id bmac-overlay:902/vm-902-disk-1.qcow2", expected=1).stderr)
        self.assertIn("Volume id must be the overlay's own",
                      checks(f"--qcow2 {overlay} --volume-id local-zfs:vm-901-disk-1", expected=1).stderr)
        self.assertIn("Give --device or --qcow2, not both",
                      checks(f"--qcow2 {overlay} --device /dev/zvol/rpool/data/vm-901-disk-1 "
                             "--volume-id bmac-overlay:901/vm-901-disk-1.qcow2", expected=1).stderr)
        self.assertIn("must be an existing overlay file",
                      checks(f"--qcow2 {directory}/missing/vm-901-disk-1.qcow2 "
                             "--volume-id bmac-overlay:901/vm-901-disk-1.qcow2", expected=1).stderr)


if __name__ == "__main__":
    unittest.main()
