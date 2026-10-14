# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The development bmacd package holds what a host needs and installs offline."""

from __future__ import annotations

import importlib.util
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

V3_ROOT = Path(__file__).resolve().parent.parent

# Loaded by path: v2's runner also collects this file, and there `dev` is v2's.
_spec = importlib.util.spec_from_file_location("v3_build_deb", V3_ROOT / "dev" / "build_deb.py")
assert _spec and _spec.loader
tool = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(tool)


def fake_binaries(scratch: Path, glibc: str = "2.34") -> dict[str, Path]:
    binaries = {}
    for name in ("bmacd", "bmac"):
        path = scratch / name
        path.write_bytes(b"\x7fELF fake " + name.encode() + b" GLIBC_2.17\0GLIBC_" + glibc.encode() + b"\0")
        binaries[name] = path
    return binaries


class BuildDebTest(unittest.TestCase):
    def test_versions_sort_before_the_release(self) -> None:
        self.assertEqual(tool.debian_version("3.0.0-dev.0"), "3.0.0~dev.0")
        self.assertEqual(tool.debian_version("3.1.2"), "3.1.2")
        with self.assertRaises(ValueError):
            tool.debian_version("3.0")

    def test_glibc_needed_is_the_newest_symbol_version(self) -> None:
        self.assertEqual(tool.glibc_needed(b"GLIBC_2.2.5\0GLIBC_2.34\0GLIBC_2.17\0GLIBC_PRIVATE"), (2, 34))
        self.assertIsNone(tool.glibc_needed(b"static"))

    def test_control_depends_only_on_proxmox_and_recommends_the_runtime(self) -> None:
        text = tool.control("3.0.0-dev.0", 1234)
        self.assertIn("Version: 3.0.0~dev.0\n", text)
        self.assertIn("Depends: systemd, pve-manager\n", text)
        recommends = next(line for line in text.splitlines() if line.startswith("Recommends: "))
        self.assertIn("corosync-qdevice", recommends)
        self.assertNotIn("#", recommends)

    def test_a_binary_for_a_newer_glibc_is_refused(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bmac-deb-") as scratch:
            root = Path(scratch)
            with self.assertRaises(SystemExit) as raised:
                tool.stage(root / "tree", fake_binaries(root, "2.42"), "3.0.0-dev.0")
            self.assertIn("glibc 2.42", str(raised.exception))

    def test_the_package_lays_out_a_host(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bmac-deb-") as scratch:
            root = Path(scratch)
            tree = root / "tree"
            tool.stage(tree, fake_binaries(root), "3.0.0-dev.0")
            for path in (
                "usr/lib/bmac/bmacd",
                "usr/sbin/bmac",
                "usr/lib/systemd/system/bmacd.service",
                "usr/lib/systemd/system/bmacd-web.service",
                "usr/lib/systemd/system/bmac-deferred-cleanup.timer",
                "usr/lib/systemd/system/bmac-haproxy-ingress.service.d/route-sync.conf",
                "usr/lib/sysusers.d/bmacd.conf",
                "usr/lib/bmac/legacy/scripts/lib/config.sh",
                "usr/lib/bmac/legacy/" + "protocol/bmac-ui-v2.schema.json",
                "usr/lib/bmac/" + tool.HOOK,
                "usr/share/doc/bmacd/README.md",
                "DEBIAN/control",
            ):
                self.assertTrue((tree / path).is_file(), path)
            self.assertFalse(any("__pycache__" in p.parts for p in tree.rglob("*")))
            self.assertEqual((tree / "usr/lib/bmac/bmacd").stat().st_mode & 0o777, 0o755)
            self.assertEqual((tree / "usr/lib/bmac" / tool.HOOK).stat().st_mode & 0o777, 0o755)
            for script in ("postinst", "prerm", "postrm"):
                path = tree / "DEBIAN" / script
                self.assertEqual(path.stat().st_mode & 0o777, 0o755)
                if shutil.which("sh"):
                    subprocess.run(["sh", "-n", str(path)], check=True)
            postinst = (tree / "DEBIAN/postinst").read_text()
            self.assertIn("systemd-sysusers /usr/lib/sysusers.d/bmacd.conf", postinst)
            self.assertIn("/var/lib/vz/snippets/bmac-guest-role-hook.sh", postinst)
            self.assertNotIn("apt", postinst, "first boot has no network")

            if not shutil.which("dpkg-deb"):
                self.skipTest("dpkg-deb is not installed")
            deb = tool.build_deb(tree, root / "out", "3.0.0-dev.0")
            self.assertEqual(deb.name, "bmacd_3.0.0~dev.0_amd64.deb")
            fields = subprocess.run(["dpkg-deb", "-f", str(deb), "Package", "Version", "Architecture"], capture_output=True, text=True, check=True).stdout
            self.assertEqual(fields, "Package: bmacd\nVersion: 3.0.0~dev.0\nArchitecture: amd64\n")
            listing = subprocess.run(["dpkg-deb", "-c", str(deb)], capture_output=True, text=True, check=True).stdout
            self.assertIn("root/root", listing)
            self.assertIn("./usr/lib/bmac/bmacd", listing)

    def test_the_appliance_package_lays_out_the_daemon_and_its_unit(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bmac-deb-") as scratch:
            root = Path(scratch)
            binary = root / "bmac-backupd"
            binary.write_bytes(b"\x7fELF fake GLIBC_2.39\0")
            tree = root / "tree"
            tool.stage_appliance(tree, binary, "3.0.0-dev.0")
            for path in (
                "usr/sbin/bmac-backupd",
                "usr/lib/systemd/system/bmac-backupd.service",
                "usr/share/doc/bmac-backupd/README.md",
                "DEBIAN/control",
            ):
                self.assertTrue((tree / path).is_file(), path)
            self.assertFalse((tree / "usr/lib/systemd/system/bmacd.service").exists())
            control = (tree / "DEBIAN/control").read_text()
            self.assertIn("Package: bmac-backupd\n", control)
            self.assertIn("Depends: systemd, zfsutils-linux, cryptsetup-bin\n", control)
            for script in ("postinst", "prerm", "postrm"):
                path = tree / "DEBIAN" / script
                if shutil.which("sh"):
                    subprocess.run(["sh", "-n", str(path)], check=True)
            self.assertNotIn("zfs destroy", (tree / "DEBIAN/postrm").read_text())

            binary.write_bytes(b"\x7fELF fake GLIBC_2.40\0")
            with self.assertRaises(SystemExit) as raised:
                tool.stage_appliance(root / "newer", binary, "3.0.0-dev.0")
            self.assertIn("Ubuntu 24.04", str(raised.exception))

            if not shutil.which("dpkg-deb"):
                self.skipTest("dpkg-deb is not installed")
            deb = tool.build_deb(tree, root / "out", "3.0.0-dev.0", package=tool.APPLIANCE)
            self.assertEqual(deb.name, "bmac-backupd_3.0.0~dev.0_amd64.deb")

    def test_the_host_package_holds_no_appliance_unit(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bmac-deb-") as scratch:
            root = Path(scratch)
            tree = root / "tree"
            tool.stage(tree, fake_binaries(root), "3.0.0-dev.0")
            self.assertFalse((tree / "usr/lib/systemd/system/bmac-backupd.service").exists())


if __name__ == "__main__":
    unittest.main()
