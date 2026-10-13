# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Smoke test of the lab itself (IMPLEMENTATION_PLAN.md, M0.4)."""

from __future__ import annotations

import subprocess
import time

from bmaclab.console import Console
from bmaclab.lab import BOOT_TIMEOUT

DESCRIPTION = "build or reuse the lab, reach every VM, check disks, networks, consoles, and the slow carrier, then reset"
CARRIER_DELAY = 90


def nic_by_mac(mac: str) -> str:
    return f"$(ip -o link | awk -v mac={mac} '$0 ~ \"link/ether \" mac {{ sub(\":$\", \"\", $2); print $2; exit }}')"


def run(s) -> None:
    lab = s.lab
    vms = list(lab.topology.vms)
    hosts = lab.topology.hosts

    if not lab.exists():
        s.note("No lab yet; building it from nothing (this downloads images and installs Proxmox)")
        s.require("the lab builds from nothing", lambda: lab.create() or True)
    s.require("the installed snapshot exists", lambda: "installed" in lab.snapshot_names())
    lab.reset("installed")
    lab.start(vms)

    for vm in vms:
        s.require(f"{vm.name} answers SSH on 127.0.0.1:{vm.ports['ssh']}", lambda vm=vm: lab.wait_ssh(vm, BOOT_TIMEOUT) or True)

    for host in hosts:
        s.check(f"{host.name} runs Proxmox VE 9", lambda h=host: "pve-manager/9" in lab.ssh(h, "pveversion").stdout)
        s.check(f"{host.name} has rpool mirrored on its two NVMe disks", lambda h=host: "mirror-0" in lab.ssh(h, "zpool status rpool").stdout)
        listing = lab.ssh(host, "lsblk -dno SERIAL,WWN").stdout
        for disk in host.disks:
            if disk.bus == "nvme":
                # Linux drops QEMU's NVMe identifiers and builds the WWN from the serial.
                expected = disk.serial.encode().hex()
                what = "a WWN built from its serial"
            else:
                expected = disk.wwn[2:]
                what = f"WWN {disk.wwn}"
            s.check(
                f"{host.name} sees {disk.bus} disk {disk.serial} with {what}",
                lambda d=disk, e=expected: any(d.serial in line and e in line.lower() for line in listing.splitlines()),
                detail=listing,
            )

    first = hosts[0]
    for vm in vms:
        if vm is not first and vm.mgmt_ip:
            s.check(f"{first.name} reaches {vm.name} on the management segment ({vm.mgmt_ip})", lambda vm=vm: lab.ssh(first, f"ping -c1 -W3 {vm.mgmt_ip}").returncode == 0)
        if vm is not first and vm.vlan_ip:
            s.check(f"{first.name} reaches {vm.name} on the VLAN ({vm.vlan_ip})", lambda vm=vm: lab.ssh(first, f"ping -c1 -W3 {vm.vlan_ip}").returncode == 0)

    for vm in (first, next(v for v in vms if v.image == "ubuntu")):
        def login_prompt(vm=vm) -> bool:
            with Console(lab.paths.console(vm.name)) as console:
                console.line()
                console.expect(r"login:", timeout=60)
            return True

        s.check(f"{vm.name}'s serial console shows a login prompt", login_prompt)

    s.require("a marker file is written on " + first.name, lambda: lab.ssh(first, "touch /root/bmac-lab-smoke").returncode == 0)

    slow = hosts[1]
    vlan = slow.nic("vlan")
    if vlan:
        lab.set_carrier_delay(slow, CARRIER_DELAY)
        before = lab.paths.last_reset(slow.name).read_text()
        try:
            lab.ssh(slow, "systemctl reboot", timeout=30)
        except subprocess.TimeoutExpired:
            pass  # the connection can hang as sshd stops

        def reset_seen() -> bool:
            deadline = time.monotonic() + 120
            while lab.paths.last_reset(slow.name).read_text() == before:
                if time.monotonic() > deadline:
                    return False
                time.sleep(0.5)
            return True

        s.require(f"the supervisor sees {slow.name} reboot", reset_seen)
        reset_at = float(lab.paths.last_reset(slow.name).read_text())
        s.require(f"{slow.name} answers SSH after the reboot", lambda: lab.wait_ssh(slow, BOOT_TIMEOUT) or True)
        carrier = f"cat /sys/class/net/{nic_by_mac(vlan.mac)}/carrier"
        first_state = lab.ssh(slow, carrier).stdout.strip()
        s.note(f"SSH came back {time.time() - reset_at:.0f}s after the reset; the VLAN carrier read {first_state or 'nothing'}")

        def carrier_up_after_delay() -> bool:
            deadline = time.monotonic() + CARRIER_DELAY + 120
            while lab.ssh(slow, carrier).stdout.strip() != "1":
                if time.monotonic() > deadline:
                    raise AssertionError("the carrier never came up")
                time.sleep(1)
            waited = time.time() - reset_at
            s.note(f"carrier up {waited:.0f}s after the reset (delay {CARRIER_DELAY}s)")
            return CARRIER_DELAY - 1 <= waited <= CARRIER_DELAY + 15

        if time.time() - reset_at < CARRIER_DELAY - 5:
            s.check(f"{slow.name}'s VLAN carrier is down while the delay runs", first_state == "0")
        s.check(f"{slow.name}'s VLAN carrier comes up {CARRIER_DELAY}s after the reboot", carrier_up_after_delay)
        lab.set_carrier_delay(slow, 0)

    lab.reset("installed")
    lab.start([first])
    s.require(f"{first.name} answers SSH after the reset", lambda: lab.wait_ssh(first, BOOT_TIMEOUT) or True)
    s.check(f"the reset removed the marker file on {first.name}", lambda: lab.ssh(first, "test ! -e /root/bmac-lab-smoke").returncode == 0)
    lab.stop(vms)
