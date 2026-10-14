# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The QEMU command line carries every disk identity, NIC, and socket."""

from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

LAB = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(LAB))

from bmaclab import qemu  # noqa: E402
from bmaclab.paths import Paths  # noqa: E402
from bmaclab.topology import load  # noqa: E402


def option_values(args: list[str], option: str) -> list[str]:
    return [args[i + 1] for i, a in enumerate(args) if a == option]


class QemuArgvTest(unittest.TestCase):
    def setUp(self) -> None:
        scratch = tempfile.TemporaryDirectory(prefix="bmac-lab-qemu-")
        self.addCleanup(scratch.cleanup)
        self.paths = Paths(Path(scratch.name))
        self.topology = load(LAB / "lab.toml")
        self.code = Path("/fw/OVMF_CODE_4M.fd")

    def argv(self, name: str, media: Path | None = None) -> list[str]:
        return qemu.argv(self.topology, self.topology.vm(name), self.paths, self.code, media)

    def test_starts_paused_daemonized_with_kvm(self) -> None:
        args = self.argv("mox1")
        self.assertIn("-S", args)
        self.assertIn("-daemonize", args)
        self.assertEqual(option_values(args, "-machine"), ["q35,accel=kvm"])
        self.assertEqual(option_values(args, "-cpu"), ["host"])
        self.assertEqual(option_values(args, "-m"), ["8G"])

    def test_each_disk_carries_its_serial_and_wwn(self) -> None:
        mox1 = self.topology.vm("mox1")
        devices = option_values(self.argv("mox1"), "-device")
        for disk in mox1.disks:
            if disk.bus == "nvme":
                self.assertIn(f"nvme,id=ctl-{disk.id},serial={disk.serial}", devices)
                self.assertTrue(any(d.startswith("nvme-ns,") and f"eui64={disk.wwn}" in d for d in devices))
            else:
                matching = [d for d in devices if f"serial={disk.serial}" in d]
                self.assertEqual(len(matching), 1)
                self.assertIn(f"wwn={disk.wwn}", matching[0])
        sata = [d for d in devices if d.startswith("ide-hd,")]
        self.assertEqual([d.split("bus=")[1].split(",")[0] for d in sata], ["ide.2", "ide.3"])
        self.assertIn("virtio-scsi-pci,id=scsi0", devices)

    def test_the_scsi_lab_attaches_the_nvme_disks_as_scsi(self) -> None:
        topology = load(LAB / "lab-scsi.toml")
        mox1 = topology.vm("mox1")
        devices = option_values(qemu.argv(topology, mox1, self.paths, self.code), "-device")
        self.assertFalse([d for d in devices if d.startswith("nvme")])
        for disk in mox1.disks:
            if disk.bus in ("nvme", "scsi"):
                self.assertIn(f"scsi-hd,drive=d-{disk.id},bus=scsi0.0,serial={disk.serial},wwn={disk.wwn}", devices)
        self.assertEqual(devices.count("virtio-scsi-pci,id=scsi0"), 1)

    def test_the_sata_lab_attaches_the_nvme_disks_as_sata(self) -> None:
        topology = load(LAB / "lab-sata.toml")
        mox1 = topology.vm("mox1")
        devices = option_values(qemu.argv(topology, mox1, self.paths, self.code), "-device")
        self.assertFalse([d for d in devices if d.startswith("nvme")])
        sata = [d for d in devices if d.startswith("ide-hd,")]
        self.assertEqual([d.split("bus=")[1].split(",")[0] for d in sata], ["ide.2", "ide.3", "ide.4", "ide.5"])
        for disk in mox1.disks:
            if disk.bus in ("nvme", "sata"):
                self.assertEqual(len([d for d in sata if f"serial={disk.serial},wwn={disk.wwn}" in d]), 1)

    def test_nics_use_user_networking_and_the_supervisors_switch(self) -> None:
        mox2 = self.topology.vm("mox2")
        netdevs = option_values(self.argv("mox2"), "-netdev")
        self.assertTrue(netdevs[0].startswith("user,id=uplink,hostname=mox2,"))
        self.assertIn(f"hostfwd=tcp:127.0.0.1:{mox2.ports['ssh']}-:22", netdevs[0])
        self.assertIn(f"hostfwd=tcp:127.0.0.1:{mox2.ports['web']}-:8443", netdevs[0])
        for netdev, network in zip(netdevs[1:], ("mgmt", "vlan")):
            self.assertEqual(netdev, f"dgram,id={network},local.type=unix,local.path={self.paths.nic('mox2', network)},"
                                     f"remote.type=unix,remote.path={self.paths.switch(network)}")
        devices = option_values(self.argv("mox2"), "-device")
        self.assertIn(f"virtio-net-pci,netdev=vlan,mac={mox2.nic('vlan').mac},id=nic-vlan,speed=10000,duplex=full", devices)

    def test_only_an_inserted_cd_gets_a_boot_index(self) -> None:
        empty = self.argv("mox1")
        self.assertFalse(any("bootindex" in a for a in empty))
        iso = Path("/isos/install.iso")
        booting = self.argv("mox1", iso)
        self.assertIn("ide-cd,drive=cd0,bus=ide.0,id=cdrom,bootindex=0", booting)
        self.assertEqual([a for a in booting if "bootindex=" in a], ["ide-cd,drive=cd0,bus=ide.0,id=cdrom,bootindex=0"])
        self.assertIn(f"if=none,id=cd0,media=cdrom,readonly=on,file={iso}", booting)

    def test_no_reboot_makes_a_guest_reboot_end_qemu(self) -> None:
        self.assertNotIn("-no-reboot", self.argv("mox1"))
        args = qemu.argv(self.topology, self.topology.vm("mox1"), self.paths, self.code, Path("/isos/host.iso"), no_reboot=True)
        self.assertIn("-no-reboot", args)

    def test_ubuntu_vms_get_their_seed_and_only_appliances_the_vlan(self) -> None:
        for name, vlan in (("appliance", True), ("qdevice", False)):
            args = self.argv(name)
            self.assertTrue(any(a.endswith(f"file={self.paths.seed(name)}") for a in args))
            self.assertEqual(any("id=vlan" in a for a in option_values(args, "-netdev")), vlan, name)

    def test_monitors_console_and_firmware_live_in_the_vm_folder(self) -> None:
        args = self.argv("qdevice")
        self.assertEqual(option_values(args, "-qmp"), [
            f"unix:{self.paths.qmp('qdevice')},server=on,wait=off",
            f"unix:{self.paths.events('qdevice')},server=on,wait=off",
        ])
        self.assertIn(f"if=pflash,format=raw,unit=1,file={self.paths.vars('qdevice')}", args)
        self.assertIn(f"if=pflash,format=raw,unit=0,readonly=on,file={self.code}", args)
        self.assertTrue(option_values(args, "-chardev")[0].startswith(f"socket,id=console,path={self.paths.console('qdevice')}"))

    def test_firmware_search_returns_the_first_complete_pair(self) -> None:
        with tempfile.TemporaryDirectory(prefix="bmac-lab-fw-") as scratch:
            code, variables = Path(scratch, "CODE.fd"), Path(scratch, "VARS.fd")
            code.write_bytes(b"")
            candidates = (("/nonexistent/CODE.fd", "/nonexistent/VARS.fd"), (str(code), str(variables)))
            self.assertIsNone(qemu.find_firmware(candidates))
            variables.write_bytes(b"")
            self.assertEqual(qemu.find_firmware(candidates), (code, variables))

    def test_long_lab_folders_are_refused_for_sockets(self) -> None:
        paths = Paths(Path("/" + "x" * 120))
        with self.assertRaisesRegex(ValueError, "BMAC_LAB_DIR"):
            paths.qmp("mox1")


if __name__ == "__main__":
    unittest.main()
