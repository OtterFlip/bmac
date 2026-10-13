# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Background process that reproduces a slow VLAN carrier.

On every guest reset (a reboot, or `lab/lab power VM reset`) it takes the
VM's VLAN link down and raises it once the VM's carrier delay has passed,
the way FiberState's VLAN comes up well after the host has booted. It exits
once no lab VM has been running for a while.
"""

from __future__ import annotations

import os
import signal
import time
from pathlib import Path

from .lab import Lab
from .qmp import Qmp, QmpError
from .topology import Vm

IDLE_EXIT_SECONDS = 15


def read_float(path: Path) -> float | None:
    try:
        return float(path.read_text().strip())
    except (FileNotFoundError, ValueError):
        return None


class Watch:
    """One VM's event connection and carrier schedule."""

    def __init__(self, lab: Lab, vm: Vm, clock=time.time) -> None:
        self.lab = lab
        self.vm = vm
        self.clock = clock
        self.monitor: Qmp | None = None

    def connect(self) -> None:
        if self.monitor is None and self.lab.running(self.vm):
            try:
                self.monitor = Qmp(self.lab.paths.events(self.vm.name), timeout=5)
            except QmpError:
                self.monitor = None

    def drop(self) -> None:
        if self.monitor:
            self.monitor.close()
        self.monitor = None

    def held(self) -> bool:
        return self.lab.paths.link_held(self.vm.name, "vlan").exists()

    def on_reset(self) -> None:
        paths = self.lab.paths
        now = self.clock()
        paths.last_reset(self.vm.name).write_text(f"{now}\n")
        delay = self.lab.carrier_delay(self.vm)
        if delay and not self.held() and self.monitor:
            self.monitor.command("set_link", name="nic-vlan", up=False)
            paths.carrier_until(self.vm.name).write_text(f"{now + delay}\n")
            print(f"{self.vm.name}: reset; VLAN carrier comes up in {delay} seconds", flush=True)

    def raise_when_due(self) -> None:
        until_file = self.lab.paths.carrier_until(self.vm.name)
        until = read_float(until_file)
        if until is None or self.clock() < until or not self.monitor:
            return
        if not self.held():
            self.monitor.command("set_link", name="nic-vlan", up=True)
            print(f"{self.vm.name}: VLAN carrier up", flush=True)
        until_file.unlink(missing_ok=True)

    def poll(self, wait: float) -> None:
        self.connect()
        if not self.monitor:
            return
        try:
            event = self.monitor.event(timeout=wait)
            while event:
                if event.get("event") == "RESET":
                    self.on_reset()
                event = self.monitor.event(timeout=0)
            if self.vm.nic("vlan"):
                self.raise_when_due()
        except (QmpError, OSError):
            self.drop()


def supervise(lab: Lab) -> None:
    paths = lab.paths
    paths.supervisor_pid.write_text(f"{os.getpid()}\n")
    stopping = False

    def stop(*_: object) -> None:
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    watches = [Watch(lab, vm) for vm in lab.topology.vms]
    idle_since: float | None = None
    try:
        while not stopping:
            for watch in watches:
                watch.poll(wait=0.2)
            if any(lab.running(vm) for vm in lab.topology.vms):
                idle_since = None
            elif idle_since is None:
                idle_since = time.monotonic()
            elif time.monotonic() - idle_since > IDLE_EXIT_SECONDS:
                break
            time.sleep(0.1)
    finally:
        for watch in watches:
            watch.drop()
        if read_float(paths.supervisor_pid) == os.getpid():
            paths.supervisor_pid.unlink(missing_ok=True)


def main() -> None:
    import argparse

    from .paths import Paths
    from .topology import load

    parser = argparse.ArgumentParser(prog="python3 -m bmaclab.supervisor")
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    args = parser.parse_args()
    supervise(Lab(load(args.config), Paths(args.root)))


if __name__ == "__main__":
    main()
