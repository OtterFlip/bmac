# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The lab topology loads, derives unique identities, and rejects bad input."""

from __future__ import annotations

import copy
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

LAB = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(LAB))

from bmaclab.topology import TopologyError, load, parse  # noqa: E402

CONFIG = LAB / "lab.toml"


def data() -> dict:
    return tomllib.loads(CONFIG.read_text())


class CheckedInTopologyTest(unittest.TestCase):
    def setUp(self) -> None:
        self.topology = load(CONFIG)

    def test_has_three_hosts_an_appliance_and_a_qdevice(self) -> None:
        roles = [vm.role for vm in self.topology.vms]
        self.assertEqual(roles, ["host", "host", "host", "appliance", "qdevice"])

    def test_each_host_has_nvme_sata_and_scsi_disks(self) -> None:
        for host in self.topology.hosts:
            self.assertEqual({d.bus for d in host.disks}, {"nvme", "sata", "scsi"})
            self.assertEqual(host.nic("vlan").network, "vlan")

    def test_disk_identities_are_unique_and_fit_their_formats(self) -> None:
        disks = [d for vm in self.topology.vms for d in vm.disks]
        serials = [d.serial for d in disks]
        self.assertEqual(len(set(serials)), len(serials))
        wwns = [d.wwn for d in disks if d.wwn]
        self.assertEqual(len(set(wwns)), len(wwns))
        for disk in disks:
            self.assertLessEqual(len(disk.serial), 20)
            self.assertRegex(disk.serial, r"^[A-Z0-9]+$")
            if disk.bus in ("sata", "scsi"):
                self.assertRegex(disk.wwn, r"^0x5525400[0-9a-f]{9}$")
            elif disk.bus == "nvme":
                self.assertRegex(disk.wwn, r"^0x525400[0-9a-f]{10}$")
            else:
                self.assertEqual(disk.wwn, "")

    def test_identities_are_stable_across_loads(self) -> None:
        again = load(CONFIG)
        self.assertEqual([d.serial for d in again.vms[0].disks], [d.serial for d in self.topology.vms[0].disks])

    def test_macs_and_ports_are_unique(self) -> None:
        macs = [n.mac for vm in self.topology.vms for n in vm.nics]
        self.assertEqual(len(set(macs)), len(macs))
        ports = [p for vm in self.topology.vms for p in vm.ports.values()]
        self.assertEqual(len(set(ports)), len(ports))
        self.assertEqual(self.topology.vm("mox1").ports["ssh"], 22002)
        self.assertEqual(self.topology.vm("mox2").ports["web"], 22013)

    def test_tailnet_names_avoid_the_v2_test_clusters_names(self) -> None:
        names = [self.topology.tailnet_name(vm) for vm in self.topology.vms]
        self.assertEqual(names[0], "bmaclab-mox1")
        for reserved in ("qdevice", "mox1", "mox2", "mox3"):
            self.assertIn(reserved, self.topology.reserved_tailnet_names)
            self.assertNotIn(reserved, names)

    def test_unknown_vm_names_the_choices(self) -> None:
        with self.assertRaisesRegex(TopologyError, "mox1, mox2"):
            self.topology.vm("mox9")


class ScsiLabTest(unittest.TestCase):
    def test_it_is_the_nvme_lab_with_its_own_identity_and_scsi_boot_disks(self) -> None:
        nvme, scsi = load(CONFIG), load(LAB / "lab-scsi.toml")
        self.assertTrue(scsi.nvme_as_scsi)
        self.assertFalse(nvme.nvme_as_scsi)
        self.assertEqual([(v.name, v.role, [d.bus for d in v.disks]) for v in scsi.vms],
                         [(v.name, v.role, [d.bus for d in v.disks]) for v in nvme.vms])
        self.assertEqual(scsi.images, nvme.images)
        self.assertNotEqual(scsi.name, nvme.name)
        self.assertFalse(scsi.tailnet_prefix.startswith(nvme.tailnet_prefix))
        self.assertFalse(nvme.tailnet_prefix.startswith(scsi.tailnet_prefix))
        nvme_ports = {p for v in nvme.vms for p in v.ports.values()} | set(nvme.mcast_ports.values())
        scsi_ports = {p for v in scsi.vms for p in v.ports.values()} | set(scsi.mcast_ports.values())
        self.assertFalse(nvme_ports & scsi_ports)
        self.assertNotEqual(scsi.mcast_group, nvme.mcast_group)
        self.assertFalse({n.mac for v in nvme.vms for n in v.nics} & {n.mac for v in scsi.vms for n in v.nics})

    def test_a_base_may_not_have_a_base(self) -> None:
        scratch = tempfile.TemporaryDirectory(prefix="bmac-lab-topology-")
        self.addCleanup(scratch.cleanup)
        folder = Path(scratch.name)
        (folder / "a.toml").write_text('[lab]\nbase = "b.toml"\n')
        (folder / "b.toml").write_text('[lab]\nbase = "c.toml"\n')
        with self.assertRaisesRegex(TopologyError, "can't have a base of its own"):
            load(folder / "a.toml")


class RejectedTopologyTest(unittest.TestCase):
    def assertRejected(self, raw: dict, message: str) -> None:
        with self.assertRaisesRegex(TopologyError, message):
            parse(raw, CONFIG)

    def test_duplicate_vm_name(self) -> None:
        raw = data()
        raw["vm"].append(copy.deepcopy(raw["vm"][0]))
        self.assertRejected(raw, "duplicate VM name")

    def test_address_outside_its_network(self) -> None:
        raw = data()
        raw["vm"][0]["vlan_ip"] = "10.214.0.11"
        self.assertRejected(raw, "not a usable address in 10.213.0.0/24")

    def test_address_used_twice(self) -> None:
        raw = data()
        raw["vm"][1]["mgmt_ip"] = raw["vm"][0]["mgmt_ip"]
        self.assertRejected(raw, "also used by mox1")

    def test_vlan_address_on_a_role_without_the_vlan(self) -> None:
        raw = data()
        raw["vm"][3]["vlan_ip"] = "10.213.0.99"
        self.assertRejected(raw, "has no vlan network")

    def test_too_many_sata_disks(self) -> None:
        raw = data()
        raw["roles"]["host"]["disks"] += [{"bus": "sata", "size_gib": 8}] * 3
        self.assertRejected(raw, "at most 4 SATA disks")

    def test_ubuntu_vm_needs_a_virtio_first_disk(self) -> None:
        raw = data()
        raw["roles"]["qdevice"]["disks"] = [{"bus": "scsi", "size_gib": 10}]
        self.assertRejected(raw, "must be virtio")

    def test_carrier_delay_needs_a_vlan(self) -> None:
        raw = data()
        raw["vm"][3]["carrier_delay_seconds"] = 30
        self.assertRejected(raw, "needs a vlan network")

    def test_networks_must_start_with_the_uplink(self) -> None:
        raw = data()
        raw["roles"]["qdevice"]["networks"] = ["mgmt", "uplink"]
        self.assertRejected(raw, "must start with uplink")

    def test_multicast_group_must_be_multicast(self) -> None:
        raw = data()
        raw["lab"]["mcast_group"] = "10.0.0.1"
        self.assertRejected(raw, "multicast")

    def test_proxmox_image_must_be_pinned(self) -> None:
        raw = data()
        raw["images"]["proxmox"]["sha256"] = ""
        self.assertRejected(raw, "sha256 is required")

    def test_tailnet_prefix_is_required(self) -> None:
        raw = data()
        del raw["lab"]["tailnet_prefix"]
        self.assertRejected(raw, "tailnet_prefix must be a non-empty string")

    def test_tailnet_prefix_must_end_in_a_hyphen(self) -> None:
        raw = data()
        raw["lab"]["tailnet_prefix"] = "lab"
        self.assertRejected(raw, "ending in a hyphen")

    def test_reserved_tailnet_name_is_refused(self) -> None:
        raw = data()
        raw["lab"]["tailnet_prefix"] = "m-"
        raw["lab"]["reserved_tailnet_names"] = ["M-Mox1"]
        self.assertRejected(raw, "tailnet name m-mox1 is reserved")

    def test_bad_toml_names_the_file(self) -> None:
        with self.assertRaisesRegex(TopologyError, "does not exist"):
            load(LAB / "missing.toml")


if __name__ == "__main__":
    unittest.main()
