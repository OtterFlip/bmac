# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Release bundles: signed manifests, packages that match them, and builds
that come out the same twice."""

from __future__ import annotations

import importlib.util
import json
import shutil
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

V3_ROOT = Path(__file__).resolve().parent.parent

# Loaded by path: v2's runner also collects this file, and there `dev` is v2's.
_spec = importlib.util.spec_from_file_location("v3_release", V3_ROOT / "dev" / "release.py")
assert _spec and _spec.loader
release = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(release)
build_deb = release.build_deb

TOOLS = all(shutil.which(t) for t in ("openssl", "dpkg-deb"))


def key_pair(scratch: Path) -> tuple[Path, Path]:
    key, public = scratch / "test.key", scratch / "test.pub"
    subprocess.run(["openssl", "genpkey", "-algorithm", "ed25519", "-out", str(key)], check=True, capture_output=True)
    subprocess.run(["openssl", "pkey", "-in", str(key), "-pubout", "-out", str(public)], check=True, capture_output=True)
    return key, public


def deb(scratch: Path, name: str, version: str, epoch: int | None = None, control_version: str | None = None) -> Path:
    tree = scratch / f"tree-{name}"
    (tree / "DEBIAN").mkdir(parents=True)
    (tree / "DEBIAN" / "control").write_text(
        f"Package: {name}\nVersion: {control_version or build_deb.debian_version(version)}\nArchitecture: amd64\n"
        "Maintainer: test\nDescription: test\n test\n")
    (tree / "usr" / "share").mkdir(parents=True)
    (tree / "usr" / "share" / f"{name}.txt").write_text(name)
    return build_deb.build_deb(tree, scratch / "debs", version, package=name, epoch=epoch)


@unittest.skipUnless(TOOLS, "needs openssl and dpkg-deb")
class BundleTest(unittest.TestCase):
    def setUp(self) -> None:
        self.scratch = Path(tempfile.mkdtemp(prefix="bmac-release-"))
        self.addCleanup(shutil.rmtree, self.scratch)
        self.key, self.public = key_pair(self.scratch)
        self.debs = [deb(self.scratch, "bmacd", "3.0.1+b2"), deb(self.scratch, "bmac-backupd", "3.0.1+b2")]

    def test_a_bundle_holds_a_signed_manifest_then_exactly_its_packages(self) -> None:
        out = release.bundle("3.0.1+b2", self.debs, self.key, self.scratch / "b.bundle", 1_760_000_000)
        with tarfile.open(out) as tar:
            names = tar.getnames()
            self.assertEqual(tar.getmembers()[1].size, 64)
            self.assertTrue(all(m.uid == 0 and m.mtime == 1_760_000_000 for m in tar.getmembers()))
        self.assertEqual(names, ["manifest.json", "manifest.sig", "bmacd_3.0.1+b2_amd64.deb",
                                 "bmac-backupd_3.0.1+b2_amd64.deb"])
        body = release.verify(out, self.public)
        self.assertEqual(body["format"], 1)
        self.assertEqual(body["version"], "3.0.1+b2")
        self.assertEqual((body["record_schema"], body["reads_record_schema_from"]), release.record_schemas())
        bmacd = body["packages"][0]
        self.assertEqual((bmacd["name"], bmacd["version"], bmacd["architecture"]), ("bmacd", "3.0.1+b2", "amd64"))
        self.assertEqual(bmacd["sha256"], release.sha256_file(self.debs[0]))

    def test_the_same_packages_make_the_same_bundle(self) -> None:
        a = release.bundle("3.0.1", self.debs, self.key, self.scratch / "a.bundle", 1_760_000_000)
        b = release.bundle("3.0.1", self.debs, self.key, self.scratch / "b.bundle", 1_760_000_000)
        self.assertEqual(a.read_bytes(), b.read_bytes())

    def test_another_key_or_a_changed_package_is_refused(self) -> None:
        out = release.bundle("3.0.1", self.debs, self.key, self.scratch / "b.bundle", 1_760_000_000)
        other = self.scratch / "other"
        other.mkdir()
        _, stranger = key_pair(other)
        with self.assertRaisesRegex(release.ReleaseError, "isn't signed"):
            release.verify(out, stranger)
        with tarfile.open(out) as tar:
            inside = tar.getmembers()[-1].offset_data + 100
        data = bytearray(out.read_bytes())
        data[inside] ^= 0xFF
        tampered = self.scratch / "tampered.bundle"
        tampered.write_bytes(bytes(data))
        with self.assertRaisesRegex(release.ReleaseError, "doesn't match"):
            release.verify(tampered, self.public)

    def test_a_package_given_twice_is_refused(self) -> None:
        with self.assertRaisesRegex(release.ReleaseError, "twice"):
            release.manifest("3.0.1", [self.debs[0], self.debs[0]], 0, (1, 1))

    def test_a_tree_packed_at_another_time_packs_the_same_with_the_epoch(self) -> None:
        tree = self.scratch / "tree-bmacd"
        first = build_deb.build_deb(tree, self.scratch / "first", "3.0.1", package="bmacd", epoch=1_760_000_000)
        tree.chmod(0o775)
        (tree / "usr" / "share" / "bmacd.txt").touch()
        second = build_deb.build_deb(tree, self.scratch / "second", "3.0.1", package="bmacd", epoch=1_760_000_000)
        self.assertEqual(first.read_bytes(), second.read_bytes())
        listing = subprocess.run(["dpkg-deb", "--contents", str(first)], check=True, capture_output=True,
                                 text=True).stdout
        self.assertTrue(listing.startswith("drwxr-xr-x root/root"))
        self.assertTrue(all(" 2025-10-09 " in line for line in listing.splitlines()), listing)
        # The ar members (debian-binary, control, data) carry the epoch too.
        archive = first.read_bytes()
        offset, stamps = 8, []
        while offset < len(archive):
            header = archive[offset:offset + 60]
            stamps.append(int(header[16:28]))
            offset += 60 + int(header[48:58]) + int(header[48:58]) % 2
        self.assertEqual(stamps, [1_760_000_000] * 3)

    def test_a_deployer_package_stamped_at_build_time_repacks_the_same(self) -> None:
        packed, built = [], []
        for n in (1, 2):
            built.append(deb(self.scratch / f"build{n}", "bmac-deployer", "3.0.1-rc.1", epoch=1_770_000_000 + n,
                             control_version="3.0.1-rc.1"))
            packed.append(release.repack(built[-1], self.scratch / f"out{n}", "3.0.1-rc.1", 1_760_000_000))
        self.assertNotEqual(built[0].read_bytes(), built[1].read_bytes())
        self.assertEqual(packed[0].name, "bmac-deployer_3.0.1~rc.1_amd64.deb")
        self.assertEqual(packed[0].read_bytes(), packed[1].read_bytes())
        self.assertEqual(release.deb_fields(packed[0])["Version"], "3.0.1~rc.1")

    def test_signing_needs_the_key_file(self) -> None:
        with self.assertRaisesRegex(release.ReleaseError, "no signing key"):
            release.sign(b"x", self.scratch / "missing.key")


class VersionTest(unittest.TestCase):
    def test_record_schemas_are_read_from_the_source(self) -> None:
        text = "pub const SCHEMA_VERSION: u64 = 3;\npub const OLDEST_READ_SCHEMA: u64 = 2;\n"
        self.assertEqual(release.record_schemas(text), (3, 2))
        with self.assertRaises(release.ReleaseError):
            release.record_schemas("nothing")

    def test_build_numbers_stay_in_the_package_version(self) -> None:
        self.assertEqual(build_deb.debian_version("3.0.0-dev.0+b2"), "3.0.0~dev.0+b2")
        with self.assertRaises(ValueError):
            build_deb.debian_version("3.0.0+b2+b3")

    def test_release_packages_dont_call_themselves_development_builds(self) -> None:
        self.assertIn("development build", build_deb.control("3.0.0", 1))
        self.assertNotIn("development build", build_deb.control("3.0.0", 1, release=True))
        self.assertNotIn("development build", build_deb.appliance_control("3.0.0", 1, release=True))


if __name__ == "__main__":
    unittest.main()
