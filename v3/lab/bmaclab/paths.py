# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Where the lab keeps its images, disks, sockets, and snapshots."""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

from .topology import Disk, Topology

# sun_path is 108 bytes on Linux, including the terminating NUL.
MAX_SOCKET_PATH = 107


def default_root(topology: Topology) -> Path:
    override = os.environ.get("BMAC_LAB_DIR")
    if override:
        return Path(override).expanduser()
    data = Path(os.environ.get("XDG_DATA_HOME") or Path.home() / ".local" / "share")
    return data / "bmac-lab" / topology.name


@dataclass(frozen=True)
class Paths:
    root: Path

    @property
    def images(self) -> Path:
        return self.root / "images"

    @property
    def tools(self) -> Path:
        return self.root / "tools"

    @property
    def snapshots(self) -> Path:
        return self.root / "snapshots"

    @property
    def ssh_key(self) -> Path:
        return self.root / "id_ed25519"

    @property
    def known_hosts(self) -> Path:
        return self.root / "known_hosts"

    @property
    def state(self) -> Path:
        return self.root / "lab.json"

    @property
    def supervisor_pid(self) -> Path:
        return self.root / "supervisor.pid"

    @property
    def supervisor_log(self) -> Path:
        return self.root / "supervisor.log"

    def vm(self, name: str) -> Path:
        return self.root / "vms" / name

    def disk(self, disk: Disk) -> Path:
        return self.vm(disk.vm) / disk.file

    def vars(self, name: str) -> Path:
        return self.vm(name) / "OVMF_VARS.fd"

    def qmp(self, name: str) -> Path:
        return self._socket(name, "qmp.sock")

    def events(self, name: str) -> Path:
        return self._socket(name, "events.sock")

    def console(self, name: str) -> Path:
        return self._socket(name, "console.sock")

    def vnc(self, name: str) -> Path:
        return self._socket(name, "vnc.sock")

    def console_log(self, name: str) -> Path:
        return self.vm(name) / "console.log"

    def qemu_log(self, name: str) -> Path:
        return self.vm(name) / "qemu.log"

    def pidfile(self, name: str) -> Path:
        return self.vm(name) / "qemu.pid"

    def seed(self, name: str) -> Path:
        return self.vm(name) / "seed.iso"

    def install_iso(self, name: str) -> Path:
        return self.vm(name) / "install.iso"

    def carrier_until(self, name: str) -> Path:
        """Holds the epoch time at which the supervisor raises the VLAN carrier."""
        return self.vm(name) / "carrier-until"

    def carrier_delay(self, name: str) -> Path:
        """Overrides the topology's carrier_delay_seconds for this VM."""
        return self.vm(name) / "carrier-delay"

    def link_held(self, name: str, network: str) -> Path:
        """Present while an operator holds a link down; the supervisor leaves it alone."""
        return self.vm(name) / f"link-held-{network}"

    def last_reset(self, name: str) -> Path:
        return self.vm(name) / "last-reset"

    def nic(self, name: str, network: str) -> Path:
        """Where the VM's NIC on network sends from and receives at."""
        return self._socket(name, f"net-{network}.sock")

    def switch(self, network: str) -> Path:
        return self._checked(self.root / f"switch-{network}.sock")

    def _socket(self, name: str, leaf: str) -> Path:
        return self._checked(self.vm(name) / leaf)

    def _checked(self, path: Path) -> Path:
        if len(os.fsencode(path)) > MAX_SOCKET_PATH:
            raise ValueError(f"{path} is too long for a Unix socket; set BMAC_LAB_DIR to a shorter folder")
        return path
