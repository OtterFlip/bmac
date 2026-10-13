# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The QEMU command line for one lab VM."""

from __future__ import annotations

from pathlib import Path

from .paths import Paths
from .topology import FORWARDS, Topology, Vm

OVMF_CANDIDATES = (
    ("/usr/share/OVMF/OVMF_CODE_4M.fd", "/usr/share/OVMF/OVMF_VARS_4M.fd"),
    ("/usr/share/edk2/x64/OVMF_CODE.4m.fd", "/usr/share/edk2/x64/OVMF_VARS.4m.fd"),
    ("/usr/share/edk2/ovmf/OVMF_CODE.fd", "/usr/share/edk2/ovmf/OVMF_VARS.fd"),
)
# no-flush: a live save writes the guest's memory into the image, and with
# host flushes each cluster it allocates waits on the host filesystem's
# journal, so an 8 GiB guest took over ten minutes. Without them a host
# crash can corrupt a lab image; BMAC_LAB_FRESH=1 rebuilds it.
DISK_OPTIONS = "discard=unmap,cache.no-flush=on"


def find_firmware(candidates=OVMF_CANDIDATES) -> tuple[Path, Path] | None:
    for code, variables in candidates:
        if Path(code).is_file() and Path(variables).is_file():
            return Path(code), Path(variables)
    return None


def _hostfwd(vm: Vm) -> str:
    return ",".join(f"hostfwd=tcp:127.0.0.1:{vm.ports[k]}-:{guest}" for k, (_, guest) in FORWARDS.items())


def argv(topology: Topology, vm: Vm, paths: Paths, firmware_code: Path, media: Path | None = None, qemu: str = "qemu-system-x86_64", no_reboot: bool = False) -> list[str]:
    """Starts paused (-S) so the caller can set link states before the guest runs.
    With no_reboot, QEMU exits when the guest reboots, so a scenario can take
    out the CD the way an operator does once an installer finishes."""
    name = f"{topology.name}-{vm.name}"
    args = [
        qemu,
        "-name", f"{name},process={name}",
        "-machine", "q35,accel=kvm",
        "-cpu", "host",
        "-smp", str(vm.cpus),
        "-m", f"{vm.memory_gib}G",
        "-drive", f"if=pflash,format=raw,unit=0,readonly=on,file={firmware_code}",
        "-drive", f"if=pflash,format=raw,unit=1,file={paths.vars(vm.name)}",
        "-display", "none",
        "-vnc", f"unix:{paths.vnc(vm.name)}",
        "-chardev", f"socket,id=console,path={paths.console(vm.name)},server=on,wait=off,logfile={paths.console_log(vm.name)},logappend=on",
        "-serial", "chardev:console",
        "-qmp", f"unix:{paths.qmp(vm.name)},server=on,wait=off",
        "-qmp", f"unix:{paths.events(vm.name)},server=on,wait=off",
        "-pidfile", str(paths.pidfile(vm.name)),
        "-D", str(paths.qemu_log(vm.name)),
        "-daemonize",
        "-S",
    ]
    if no_reboot:
        args.append("-no-reboot")
    for nic in vm.nics:
        if nic.network == "uplink":
            netdev = f"user,id={nic.network},hostname={vm.name},{_hostfwd(vm)}"
        else:
            # The supervisor's switch (supervisor.Switch) carries the frames.
            netdev = (f"dgram,id={nic.network},local.type=unix,local.path={paths.nic(vm.name, nic.network)},"
                      f"remote.type=unix,remote.path={paths.switch(nic.network)}")
        device = f"virtio-net-pci,netdev={nic.network},mac={nic.mac},id={nic.id}"
        if nic.network == "vlan":
            # The private VLAN is 10 Gbps, which host setup requires.
            device += ",speed=10000,duplex=full"
        args += ["-netdev", netdev, "-device", device]

    # Only an inserted install CD gets a boot index; otherwise the firmware
    # follows the boot entries the installer wrote to NVRAM. OVMF then drops
    # NVRAM entries that QEMU's boot order doesn't list, so booting a CD
    # loses an installed system's entry (scenarios that boot a live CD save
    # and restore the firmware store around it).
    if media:
        args += ["-drive", f"if=none,id=cd0,media=cdrom,readonly=on,file={media}", "-device", "ide-cd,drive=cd0,bus=ide.0,id=cdrom,bootindex=0"]
    else:
        args += ["-drive", "if=none,id=cd0,media=cdrom,readonly=on", "-device", "ide-cd,drive=cd0,bus=ide.0,id=cdrom"]
    if vm.image == "ubuntu":
        args += ["-drive", f"if=none,id=seed,media=cdrom,readonly=on,file={paths.seed(vm.name)}", "-device", "ide-cd,drive=seed,bus=ide.1"]

    bus = topology.bus
    if any(bus(d) == "scsi" for d in vm.disks):
        args += ["-device", "virtio-scsi-pci,id=scsi0"]
    sata_port = 2
    for disk in vm.disks:
        drive = f"d-{disk.id}"
        args += ["-drive", f"if=none,id={drive},format=qcow2,file={paths.disk(disk)},{DISK_OPTIONS}"]
        if bus(disk) == "nvme":
            # One controller per disk, like separate physical NVMe drives.
            args += [
                "-device", f"nvme,id=ctl-{disk.id},serial={disk.serial}",
                "-device", f"nvme-ns,drive={drive},bus=ctl-{disk.id},nsid=1,eui64={disk.wwn}",
            ]
        elif bus(disk) == "sata":
            args += ["-device", f"ide-hd,drive={drive},bus=ide.{sata_port},serial={disk.serial},wwn={disk.wwn}"]
            sata_port += 1
        elif bus(disk) == "scsi":
            args += ["-device", f"scsi-hd,drive={drive},bus=scsi0.0,serial={disk.serial},wwn={disk.wwn}"]
        else:
            args += ["-device", f"virtio-blk-pci,drive={drive},serial={disk.serial}"]
    return args
