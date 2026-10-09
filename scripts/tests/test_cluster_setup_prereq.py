#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Fake-sysfs tests for the standalone hosts/cluster_setup_prereq.sh."""

from __future__ import annotations

import os
import pty
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("cluster_setup_prereq.sh")

FAKE_LSBLK = r'''#!/usr/bin/env bash
# lsblk -dno FIELD DEVICE or lsblk -dnbo SIZE DEVICE
field="$2"; device="${3##*/}"
case "$device:$field" in
  nvme0n1:SERIAL) echo "S6BOOTA   " ;;
  nvme0n1:SIZE) echo 2000398934016 ;;
  nvme0n1:MODEL) echo "Dell Ent NVMe v2 AGN RI U.2 1.92TB" ;;
  nvme1n1:SERIAL) echo "S6BOOTB" ;;
  nvme1n1:SIZE) echo 1990000000000 ;;
  nvme1n1:MODEL) echo "Dell Ent NVMe" ;;
  *) exit 32 ;;
esac
'''

FAKE_ETHTOOL = r'''#!/usr/bin/env bash
case "$2" in
  eno1) echo "Permanent address: aa:bb:cc:dd:ee:01" ;;
  eno2) echo "Permanent address: 00:00:00:00:00:00" ;;
  *) exit 1 ;;
esac
'''


class ClusterSetupPrereqTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.sys = self.root / "sys"
        self.bin = self.root / "bin"
        self.bin.mkdir()
        for name, text in (("lsblk", FAKE_LSBLK), ("ethtool", FAKE_ETHTOOL)):
            path = self.bin / name
            path.write_text(text)
            path.chmod(0o755)
        for name in ("nvme0n1", "nvme1n1", "nvme0c0n1", "sda", "loop0"):
            (self.sys / "block" / name).mkdir(parents=True)
        # nvme2n1 has no lsblk answers; sysfs serial and sector count are used.
        fallback = self.sys / "block" / "nvme2n1"
        (fallback / "device").mkdir(parents=True)
        (fallback / "device" / "serial").write_text("S6SYSFS  \n")
        (fallback / "device" / "model").write_text("Samsung SSD 990 PRO 1TB   \n")
        (fallback / "size").write_text("1875385008\n")
        self.nic("eno1", "aa:bb:cc:dd:ee:01", carrier="1")
        self.nic("eno2", "aa:bb:cc:dd:ee:02", carrier="0")
        self.nic("eno3", "aa:bb:cc:dd:ee:03", carrier=None, operstate="down")
        self.nic("lo", "00:00:00:00:00:00", physical=False, type_="772")
        self.nic("vmbr0", "aa:bb:cc:dd:ee:01", physical=False)
        self.nic("wlp2s0", "aa:bb:cc:dd:ee:09", wireless=True)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def nic(self, name, mac, *, physical=True, type_="1", carrier="1",
            operstate="up", wireless=False) -> None:
        path = self.sys / "class" / "net" / name
        path.mkdir(parents=True)
        (path / "address").write_text(mac + "\n")
        (path / "type").write_text(type_ + "\n")
        (path / "operstate").write_text(operstate + "\n")
        if carrier is not None:
            (path / "carrier").write_text(carrier + "\n")
        if physical:
            (path / "device").mkdir()
        if wireless:
            (path / "wireless").mkdir()

    def run_script(self, *args: str, path: str | None = None) -> subprocess.CompletedProcess[str]:
        env = dict(os.environ)
        env["CLUSTER_SETUP_PREREQ_SYS"] = str(self.sys)
        env["PATH"] = path or f"{self.bin}:{os.environ['PATH']}"
        return subprocess.run(
            ["bash", str(SCRIPT), *args], env=env, text=True, input="",
            capture_output=True, check=False, timeout=30,
        )

    def test_lists_nvme_serials_capacities_and_nic_macs(self) -> None:
        completed = self.run_script()
        self.assertEqual(completed.returncode, 0, completed.stderr)
        output = completed.stdout
        drives, nics = output.split("NVMe drives:", 1)[1].split("Network interfaces", 1)
        self.assertRegex(
            drives,
            r"/dev/nvme0n1\s+S6BOOTA\s+2000398934016\s+Dell Ent NVMe v2 AGN RI U\.2 1\.92TB\n",
        )
        self.assertRegex(drives, r"/dev/nvme1n1\s+S6BOOTB\s+1990000000000\s+Dell Ent NVMe\n")
        self.assertRegex(drives, r"/dev/nvme2n1\s+S6SYSFS\s+960197124096\s+Samsung SSD 990 PRO 1TB\n")
        for absent in ("nvme0c0n1", "sda", "loop0"):
            self.assertNotIn(absent, drives)
        self.assertRegex(nics, r"eno1\s+AA:BB:CC:DD:EE:01\s+up\n")
        self.assertRegex(nics, r"eno2\s+AA:BB:CC:DD:EE:02\s+no-cable-or-link\n")
        self.assertRegex(nics, r"eno3\s+AA:BB:CC:DD:EE:03\s+down\n")
        for absent in ("lo ", "vmbr0", "wlp2s0"):
            self.assertNotIn(absent, nics)
        header = next(line for line in output.splitlines() if line.startswith("DEVICE"))
        row = next(line for line in output.splitlines() if "nvme1n1" in line)
        self.assertEqual(header.index("CAPACITY_BYTES"), row.index("1990000000000"))

    def test_explains_purpose_and_idrac_before_listing(self) -> None:
        output = self.run_script().stdout
        intro, listing = output.split("NVMe drives:", 1)
        for text in (
            "Linux Live environment booted on a machine",
            "changes nothing",
            "env/moxN.conf",
            "NVME_MIRROR_<P>_SERIAL_<M> and NVME_MIRROR_<P>_CAPACITY_BYTES_<M>",
            "PROXMOX_PUBLIC_MAC",
            "PROXMOX_SECONDARY_MAC",
            "iDRAC ALTERNATIVE",
            "iDRAC\nadmin web UI",
            "CAPACITY_BYTES values are not needed",
        ):
            self.assertIn(text, intro)
        self.assertIn("S6BOOTA", listing)

    def test_waits_for_enter_on_a_terminal(self) -> None:
        env = dict(os.environ)
        env["CLUSTER_SETUP_PREREQ_SYS"] = str(self.sys)
        env["PATH"] = f"{self.bin}:{os.environ['PATH']}"
        controller, terminal = pty.openpty()
        process = subprocess.Popen(
            ["bash", str(SCRIPT)], env=env, stdin=terminal,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        os.close(terminal)
        try:
            with self.assertRaises(subprocess.TimeoutExpired):
                process.wait(timeout=1)
            os.write(controller, b"\n")
            stdout, stderr = process.communicate(timeout=30)
        finally:
            os.close(controller)
            if process.poll() is None:
                process.kill()
        self.assertEqual(process.returncode, 0, stderr)
        self.assertLess(stdout.index("iDRAC ALTERNATIVE"), stdout.index("NVMe drives:"))
        self.assertIn("S6BOOTA", stdout)

    def test_works_without_ethtool(self) -> None:
        (self.bin / "ethtool").unlink()
        isolated = self.root / "isolated"
        isolated.mkdir()
        for tool in ("bash", "sed", "cat"):
            (isolated / tool).symlink_to(shutil.which(tool))
        (isolated / "lsblk").symlink_to(self.bin / "lsblk")
        completed = self.run_script(path=str(isolated))
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertRegex(completed.stdout, r"eno1\s+AA:BB:CC:DD:EE:01\s+up")

    def test_reports_when_nothing_is_found(self) -> None:
        shutil.rmtree(self.sys)
        (self.sys / "block").mkdir(parents=True)
        (self.sys / "class" / "net").mkdir(parents=True)
        completed = self.run_script()
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout.count("  none found"), 2)

    def test_help_and_bad_arguments(self) -> None:
        completed = self.run_script("--help")
        self.assertEqual(completed.returncode, 0)
        self.assertIn("iDRAC ALTERNATIVE", completed.stdout)
        self.assertNotIn("NVMe drives:", completed.stdout)
        self.assertEqual(self.run_script("--host").returncode, 2)

    def test_is_standalone_and_read_only(self) -> None:
        text = SCRIPT.read_text()
        self.assertNotRegex(text, r"(?m)^\s*(source|\.)\s")
        self.assertNotIn("SCRIPT_DIR", text)
        self.assertNotIn("../lib", text)
        for command in ("ssh", "python3", "zpool", "wipefs", "sgdisk", "mkfs", "rm "):
            self.assertIsNone(re.search(rf"\b{command}", text), command)
        self.assertTrue(os.access(SCRIPT, os.X_OK))

    def test_shell_syntax(self) -> None:
        completed = subprocess.run(
            ["bash", "-n", str(SCRIPT)], text=True, capture_output=True, check=False
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)


if __name__ == "__main__":
    unittest.main()
