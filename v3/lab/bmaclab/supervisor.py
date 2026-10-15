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
import select
import signal
import socket
import threading
import time
from pathlib import Path

from .lab import LINKS, Lab
from .qmp import Qmp, QmpError
from .topology import Vm

IDLE_EXIT_SECONDS = 15
MAX_FRAME = 65536 + 64
DROP_REPORT_SECONDS = 10
SWITCHED = tuple(network for network in LINKS if network != "uplink")


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


class Switch:
    """The lab's management and VLAN networks. Every frame goes to each
    other VM on its network and never back to its sender, the way a switch
    floods it. QEMU's multicast socket also hands a VM its own frames, which
    teaches a host's bridge that its guests' MACs are out on the uplink.

    Each member is sent to from a socket of its own. The kernel charges a
    datagram waiting in a receiver's queue to the socket that sent it, so a
    VM that stops reading (its NIC down while it boots or waits) would
    otherwise fill one shared send buffer and silence the network for all."""

    def __init__(self, lab: Lab, clock=time.monotonic) -> None:
        self.clock = clock
        self.drops: dict[tuple[str, str], int] = {}
        self.reported = clock()
        self.ports: dict[socket.socket, list[str]] = {}
        self.egress: dict[str, socket.socket] = {}
        for network in SWITCHED:
            members = [str(lab.paths.nic(vm.name, network)) for vm in lab.topology.vms if vm.nic(network)]
            if not members:
                continue
            path = lab.paths.switch(network)
            path.unlink(missing_ok=True)
            port = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
            port.bind(str(path))
            port.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 8 << 20)
            port.setblocking(False)
            self.ports[port] = members
            for member in members:
                out = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
                out.setblocking(False)
                self.egress[member] = out

    def serve(self, stopping: threading.Event) -> None:
        while not stopping.is_set():
            ready, _, _ = select.select(list(self.ports), [], [], 0.5)
            for port in ready:
                self.forward(port)
            self.report()

    def report(self) -> None:
        """Prints the frames dropped since the last report, at most every
        DROP_REPORT_SECONDS, so that a silent VLAN shows why."""
        if not self.drops or self.clock() - self.reported < DROP_REPORT_SECONDS:
            return
        for (member, reason), count in sorted(self.drops.items()):
            print(f"switch: dropped {count} frames to {member}: {reason}", flush=True)
        self.drops.clear()
        self.reported = self.clock()

    def forward(self, port: socket.socket) -> None:
        for _ in range(256):
            try:
                frame, sender = port.recvfrom(MAX_FRAME)
            except BlockingIOError:
                return
            for member in self.ports[port]:
                if member != sender:
                    try:
                        self.egress[member].sendto(frame, member)
                    except (FileNotFoundError, ConnectionRefusedError):  # a stopped VM
                        pass
                    except OSError as error:  # too far behind: dropped, as a switch would
                        key = (member, error.strerror or type(error).__name__)
                        self.drops[key] = self.drops.get(key, 0) + 1

    def close(self) -> None:
        for port in [*self.ports, *self.egress.values()]:
            port.close()


def supervise(lab: Lab) -> None:
    paths = lab.paths
    paths.supervisor_pid.write_text(f"{os.getpid()}\n")
    stopping = False

    def stop(*_: object) -> None:
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    switch = Switch(lab)
    switched = threading.Event()
    threading.Thread(target=switch.serve, args=(switched,), daemon=True).start()
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
        switched.set()
        for watch in watches:
            watch.drop()
        switch.close()
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
