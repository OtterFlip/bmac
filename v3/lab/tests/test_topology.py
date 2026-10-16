# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The lab topology loads, derives unique identities, and rejects bad input."""

from __future__ import annotations

import copy
import itertools
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

    def test_has_three_hosts_two_appliances_a_qdevice_and_a_spare(self) -> None:
        roles = [vm.role for vm in self.topology.vms]
        self.assertEqual(roles, ["host", "host", "host", "appliance", "qdevice", "appliance", "spare"])
        self.assertEqual([h.name for h in self.topology.hosts], ["mox1", "mox2", "mox3"])

    def test_the_appliances_share_the_hosts_vlan_outside_the_clusters_ranges(self) -> None:
        for name, address in (("appliance", "10.213.0.220"), ("applianceb", "10.213.0.221")):
            self.assertEqual(self.topology.vm(name).vlan_ip, address)
            self.assertEqual(self.topology.vm(name).nic("vlan").network, "vlan")

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


class LiveLabTest(unittest.TestCase):
    def test_the_live_labs_are_the_nvme_lab_with_other_boot_buses(self) -> None:
        nvme = load(CONFIG)
        self.assertIsNone(nvme.nvme_as)
        for bus in ("scsi", "sata"):
            live = load(LAB / f"lab-{bus}.toml")
            self.assertEqual(live.nvme_as, bus)
            self.assertEqual([(v.name, v.role, [d.bus for d in v.disks]) for v in live.vms],
                             [(v.name, v.role, [d.bus for d in v.disks]) for v in nvme.vms])
            self.assertEqual(live.images, nvme.images)
            self.assertEqual({live.bus(d) for v in live.vms for d in v.disks if d.bus == "nvme"}, {bus})

    def test_no_two_labs_share_a_name_port_group_mac_or_tailnet_prefix(self) -> None:
        labs = [load(CONFIG), load(LAB / "lab-scsi.toml"), load(LAB / "lab-sata.toml")]
        for a, b in itertools.combinations(labs, 2):
            with self.subTest(a=a.name, b=b.name):
                self.assertNotEqual(a.name, b.name)
                self.assertFalse(a.tailnet_prefix.startswith(b.tailnet_prefix))
                self.assertFalse(b.tailnet_prefix.startswith(a.tailnet_prefix))
                ports = [{p for v in t.vms for p in v.ports.values()} | set(t.mcast_ports.values()) for t in (a, b)]
                self.assertFalse(ports[0] & ports[1])
                self.assertNotEqual(a.mcast_group, b.mcast_group)
                self.assertFalse({n.mac for v in a.vms for n in v.nics} & {n.mac for v in b.vms for n in v.nics})

    def test_nvme_disks_as_sata_must_fit_the_sata_ports(self) -> None:
        raw = data()
        raw["lab"]["nvme_as"] = "sata"
        raw["roles"]["host"]["disks"].append({"bus": "sata", "size_gib": 16})
        with self.assertRaisesRegex(TopologyError, "at most 4 SATA disks"):
            parse(raw, CONFIG)
        raw["lab"]["nvme_as"] = "ide"
        with self.assertRaisesRegex(TopologyError, "nvme_as must be"):
            parse(raw, CONFIG)

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
        raw["vm"][4]["vlan_ip"] = "10.213.0.99"
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
        raw["vm"][4]["carrier_delay_seconds"] = 30
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
