# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Verified downloads, the Proxmox answer file, first-boot script, and cloud-init seeds."""

from __future__ import annotations

import fnmatch
import hashlib
import io
import json
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

LAB = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(LAB))

from bmaclab import provision  # noqa: E402
from bmaclab.topology import load  # noqa: E402

HASH = "$6$salt$hash"
KEY = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITest bmac-lab"


class FakeOpener:
    def __init__(self, files: dict[str, bytes]) -> None:
        self.files = files
        self.opened: list[str] = []

    def __call__(self, url: str, timeout: float = 0) -> io.BytesIO:
        self.opened.append(url)
        if url not in self.files:
            raise OSError(f"404 {url}")
        return io.BytesIO(self.files[url])


class FetchTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory(prefix="bmac-lab-fetch-")
        self.addCleanup(scratch.cleanup)
        self.folder = Path(scratch.name)
        self.body = b"image bytes"
        self.sha = hashlib.sha256(self.body).hexdigest()
        self.url = "https://example.test/release/disk.img"

    def test_downloads_verifies_and_then_reuses_the_cache(self) -> None:
        opener = FakeOpener({self.url: self.body})
        path = provision.fetch(self.url, self.sha, self.folder, log=lambda _: None, opener=opener)
        self.assertEqual(path.read_bytes(), self.body)
        provision.fetch(self.url, self.sha, self.folder, log=lambda _: None, opener=opener)
        self.assertEqual(opener.opened, [self.url])

    def test_a_mismatch_keeps_nothing(self) -> None:
        opener = FakeOpener({self.url: b"tampered"})
        with self.assertRaisesRegex(provision.ProvisionError, "SHA-256 mismatch"):
            provision.fetch(self.url, self.sha, self.folder, log=lambda _: None, opener=opener)
        self.assertEqual(list(self.folder.iterdir()), [])

    def test_unpinned_files_are_checked_against_published_sums(self) -> None:
        sums = f"{'0' * 64} *other.img\n{self.sha} *disk.img\n".encode()
        opener = FakeOpener({self.url: self.body, "https://example.test/release/SHA256SUMS": sums})
        provision.fetch(self.url, "", self.folder, log=lambda _: None, opener=opener)
        self.assertEqual(opener.opened, ["https://example.test/release/SHA256SUMS", self.url])

    def test_unlisted_files_and_unpinned_http_are_refused(self) -> None:
        opener = FakeOpener({"https://example.test/release/SHA256SUMS": b"abc *else.img\n"})
        with self.assertRaisesRegex(provision.ProvisionError, "does not list disk.img"):
            provision.fetch(self.url, "", self.folder, opener=opener)
        with self.assertRaisesRegex(provision.ProvisionError, "needs a pinned sha256"):
            provision.fetch("http://example.test/disk.img", "", self.folder, opener=opener)


class AnswerFileTest(unittest.TestCase):
    def setUp(self) -> None:
        self.topology = load(LAB / "lab.toml")
        self.mox2 = self.topology.vm("mox2")

    def test_answer_file_installs_a_zfs_mirror_on_the_two_nvme_disks(self) -> None:
        answer = tomllib.loads(provision.answer_toml(self.topology, self.mox2, HASH, KEY))
        nvme = [d for d in self.mox2.disks if d.bus == "nvme"]
        self.assertEqual(answer["global"]["fqdn"], "mox2.lab.internal")
        self.assertEqual(answer["global"]["root-password-hashed"], HASH)
        self.assertEqual(answer["global"]["root-ssh-keys"], [KEY])
        self.assertEqual(answer["global"]["reboot-mode"], "power-off")
        self.assertEqual(answer["disk-setup"]["zfs"]["raid"], "raid1")
        self.assertEqual(answer["disk-setup"]["filter"]["ID_SERIAL_SHORT"], nvme[0].serial)
        self.assertEqual(answer["disk-setup"]["filter"]["ID_SERIAL"], f"*{nvme[1].serial}*")
        self.assertEqual(answer["network"]["filter"]["ID_NET_NAME_MAC"], "*" + self.mox2.nic("uplink").mac.replace(":", ""))
        self.assertEqual(answer["first-boot"], {"source": "from-iso", "ordering": "before-network"})

    def test_the_scsi_lab_finds_its_boot_disks_by_scsi_serial(self) -> None:
        topology = load(LAB / "lab-scsi.toml")
        mox2 = topology.vm("mox2")
        answer = tomllib.loads(provision.answer_toml(topology, mox2, HASH, KEY))
        pattern = answer["disk-setup"]["filter"]["ID_SCSI_SERIAL"]
        self.assertEqual(list(answer["disk-setup"]["filter"]), ["ID_SCSI_SERIAL"])
        matched = [d.bus for d in mox2.disks if fnmatch.fnmatchcase(d.serial, pattern)]
        self.assertEqual(matched, ["nvme", "nvme"])

    def test_the_sata_lab_finds_its_boot_disks_by_serial_as_the_nvme_lab_does(self) -> None:
        topology = load(LAB / "lab-sata.toml")
        mox2 = topology.vm("mox2")
        answer = tomllib.loads(provision.answer_toml(topology, mox2, HASH, KEY))
        nvme = [d for d in mox2.disks if d.bus == "nvme"]
        self.assertEqual(answer["disk-setup"]["filter"]["ID_SERIAL_SHORT"], nvme[0].serial)
        self.assertEqual(answer["disk-setup"]["filter"]["ID_SERIAL"], f"*{nvme[1].serial}*")
        matched = [d.bus for d in mox2.disks
                   if fnmatch.fnmatchcase(f"QEMU_HARDDISK_{d.serial}", answer["disk-setup"]["filter"]["ID_SERIAL"])]
        self.assertEqual(matched, ["nvme"])

    def test_first_boot_script_addresses_both_private_nics_and_parses(self) -> None:
        script = provision.first_boot_script(self.topology, self.mox2)
        self.assertIn(f"configure {self.mox2.nic('mgmt').mac} 100.64.0.12/24", script)
        self.assertIn(f"configure {self.mox2.nic('vlan').mac} 10.213.0.12/24", script)
        self.assertIn("serial-getty@ttyS0", script)
        subprocess.run(["bash", "-n"], input=script, text=True, check=True)

    def test_cloud_init_seeds_root_access_and_static_management_address(self) -> None:
        appliance = self.topology.vm("appliance")
        files = provision.cloud_init(self.topology, appliance, HASH, KEY)
        self.assertTrue(files["user-data"].startswith("#cloud-config\n"))
        user = json.loads(files["user-data"].split("\n", 1)[1])
        self.assertEqual(user["users"][0]["name"], "root")
        self.assertEqual(user["users"][0]["ssh_authorized_keys"], [KEY])
        network = json.loads(files["network-config"])
        self.assertEqual(network["ethernets"]["uplink"], {"match": {"macaddress": appliance.nic("uplink").mac}, "dhcp4": True})
        self.assertEqual(network["ethernets"]["mgmt"]["addresses"], ["100.64.0.100/24"])
        self.assertEqual(json.loads(files["meta-data"])["instance-id"], "bmaclab-appliance")


if __name__ == "__main__":
    unittest.main()
