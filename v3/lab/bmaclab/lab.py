# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Lab operations: create, start, stop, power, links, media, snapshots, reset, destroy."""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

from . import provision, qemu, tailnet
from .paths import Paths
from .qmp import Qmp, QmpError
from .topology import Topology, Vm

SNAPSHOT_NAME = re.compile(r"^[a-z0-9][a-z0-9_-]{0,31}$")
INSTALL_TIMEOUT = 45 * 60
BOOT_TIMEOUT = 10 * 60
# The links an operator (or a scenario) can hold down.
LINKS = ("uplink", "mgmt", "vlan")


class LabError(RuntimeError):
    pass


def kvm_nested() -> bool:
    for vendor in ("kvm_intel", "kvm_amd"):
        flag = Path(f"/sys/module/{vendor}/parameters/nested")
        if flag.exists():
            return flag.read_text().strip() in ("Y", "1")
    return False


class Lab:
    def __init__(self, topology: Topology, paths: Paths, log=print) -> None:
        self.topology = topology
        self.paths = paths
        self.log = log
        self.secrets = tailnet.SECRETS
        self._tailnet_verified = False
        self.qemu_binary = os.environ.get("BMAC_LAB_QEMU", "qemu-system-x86_64")
        self.firmware = qemu.find_firmware()

    # Checks

    def problems(self) -> list[str]:
        found = []
        for command, package in (
            (self.qemu_binary, "qemu-system-x86"), ("qemu-img", "qemu-utils"), ("xorriso", "xorriso"),
            ("openssl", "openssl"), ("ssh", "openssh-client"), ("ssh-keygen", "openssh-client"), ("dpkg-deb", "dpkg"),
        ):
            if not shutil.which(command):
                found.append(f"{command} is missing (sudo apt install {package})")
        if self.firmware is None:
            found.append("OVMF UEFI firmware is missing (sudo apt install ovmf)")
        if not os.access("/dev/kvm", os.R_OK | os.W_OK):
            found.append("/dev/kvm is not usable by this user (add yourself to the kvm group)")
        if not kvm_nested():
            found.append("nested virtualization is off (options kvm_intel nested=1, or kvm_amd nested=1)")
        return found

    def require_tools(self) -> None:
        problems = self.problems()
        if problems:
            raise LabError("the lab cannot run here:\n  " + "\n  ".join(problems))

    @property
    def topology_digest(self) -> str:
        return hashlib.sha256(self.topology.source.read_bytes()).hexdigest()

    def exists(self) -> bool:
        return self.paths.state.exists()

    def require_created(self) -> None:
        if not self.exists():
            raise LabError(f"no lab at {self.paths.root}; run `lab/lab create` first")
        state = json.loads(self.paths.state.read_text())
        if state.get("topology_sha256") != self.topology_digest:
            self.log(f"WARNING: {self.topology.source} changed since the lab was created; `lab/lab rebuild` applies it")

    # Processes

    def pid(self, vm: Vm) -> int | None:
        try:
            pid = int(self.paths.pidfile(vm.name).read_text().strip())
            cmdline = Path(f"/proc/{pid}/cmdline").read_bytes()
        except (FileNotFoundError, ValueError, ProcessLookupError):
            return None
        return pid if f"process={self.topology.name}-{vm.name}".encode() in cmdline else None

    def running(self, vm: Vm) -> bool:
        return self.pid(vm) is not None

    def qmp(self, vm: Vm) -> Qmp:
        if not self.running(vm):
            raise LabError(f"{vm.name} is not running")
        return Qmp(self.paths.qmp(vm.name))

    def carrier_delay(self, vm: Vm) -> int:
        override = self.paths.carrier_delay(vm.name)
        return int(override.read_text()) if override.exists() else vm.carrier_delay_seconds

    def set_carrier_delay(self, vm: Vm, seconds: int) -> None:
        if vm.nic("vlan") is None:
            raise LabError(f"{vm.name} has no VLAN NIC")
        if not 0 <= seconds <= 3600:
            raise LabError("the carrier delay must be from 0 to 3600 seconds")
        self.paths.carrier_delay(vm.name).write_text(f"{seconds}\n")

    def start(self, vms: list[Vm], media: Path | None = None, no_reboot: bool = False) -> None:
        self.require_created()
        self.require_tools()
        assert self.firmware is not None
        for vm in vms:
            if self.running(vm):
                if media:
                    raise LabError(f"{vm.name} is already running; use `lab/lab media` to change its CD")
                continue
            for stale in (self.paths.qmp(vm.name), self.paths.events(vm.name), self.paths.console(vm.name), self.paths.vnc(vm.name)):
                stale.unlink(missing_ok=True)
            args = qemu.argv(self.topology, vm, self.paths, self.firmware[0], media, self.qemu_binary, no_reboot)
            try:
                subprocess.run(args, check=True, capture_output=True, text=True)
            except subprocess.CalledProcessError as error:
                raise LabError(f"QEMU did not start {vm.name}: {error.stderr.strip()}") from error
            with Qmp(self.paths.qmp(vm.name)) as monitor:
                for network in LINKS:
                    if vm.nic(network) and self.paths.link_held(vm.name, network).exists():
                        monitor.command("set_link", name=f"nic-{network}", up=False)
                delay = self.carrier_delay(vm)
                if delay and not self.paths.link_held(vm.name, "vlan").exists():
                    monitor.command("set_link", name="nic-vlan", up=False)
                    self.paths.carrier_until(vm.name).write_text(f"{time.time() + delay}\n")
                self.paths.last_reset(vm.name).write_text(f"{time.time()}\n")
                monitor.command("cont")
            self.log(f"Started {vm.name}" + (f" from {media.name}" if media else ""))
        self.ensure_supervisor()

    def wait_exit(self, vm: Vm, timeout: float) -> bool:
        deadline = time.monotonic() + timeout
        while self.running(vm):
            if time.monotonic() > deadline:
                return False
            time.sleep(0.5)
        return True

    def stop(self, vms: list[Vm], force: bool = False, timeout: float = 120) -> None:
        asked = []
        for vm in vms:
            if not self.running(vm):
                continue
            if force:
                self.kill(vm)
                continue
            try:
                with self.qmp(vm) as monitor:
                    monitor.command("system_powerdown")
                asked.append(vm)
            except (QmpError, LabError):
                self.kill(vm)
        deadline = time.monotonic() + timeout
        for vm in asked:
            if not self.wait_exit(vm, max(0.0, deadline - time.monotonic())):
                self.log(f"{vm.name} did not shut down within {timeout:.0f} seconds; pulling the plug")
                self.kill(vm)
            else:
                self.log(f"Stopped {vm.name}")

    def kill(self, vm: Vm) -> None:
        """Pulls the plug: the guest gets no chance to shut down."""
        pid = self.pid(vm)
        if pid is None:
            return
        try:
            with Qmp(self.paths.qmp(vm.name), timeout=5) as monitor:
                monitor.command("quit")
        except QmpError:
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, signal.SIGKILL)
        if not self.wait_exit(vm, 10):
            with contextlib.suppress(ProcessLookupError):
                os.kill(pid, signal.SIGKILL)
            self.wait_exit(vm, 10)
        self.log(f"Powered off {vm.name}")

    def power(self, vm: Vm, action: str) -> None:
        if action == "on":
            self.start([vm])
        elif action == "off":
            self.kill(vm)
        elif action == "shutdown":
            self.stop([vm])
        elif action == "reset":
            with self.qmp(vm) as monitor:
                monitor.command("system_reset")
            self.log(f"Reset {vm.name}")
        elif action == "cycle":
            self.kill(vm)
            self.start([vm])
        else:
            raise LabError(f"unknown power action {action!r}")

    # Links and media

    def link(self, vm: Vm, network: str, up: bool) -> None:
        if vm.nic(network) is None:
            raise LabError(f"{vm.name} has no {network} NIC")
        held = self.paths.link_held(vm.name, network)
        if up:
            held.unlink(missing_ok=True)
            if network == "vlan":
                self.paths.carrier_until(vm.name).unlink(missing_ok=True)
        else:
            held.write_text("held down by the operator\n")
        if self.running(vm):
            with self.qmp(vm) as monitor:
                monitor.command("set_link", name=f"nic-{network}", up=up)
        self.log(f"{vm.name} {network} link {'up' if up else 'held down'}")

    def media(self, vm: Vm, iso: Path | None) -> None:
        with self.qmp(vm) as monitor:
            if iso is None:
                monitor.command("eject", id="cdrom", force=True)
            else:
                monitor.command("blockdev-change-medium", id="cdrom", filename=str(iso.resolve()), format="raw")
        self.log(f"{vm.name}: {'ejected the CD' if iso is None else f'inserted {iso.name}'}")

    # Supervisor

    def supervisor_pid(self) -> int | None:
        try:
            pid = int(self.paths.supervisor_pid.read_text())
            os.kill(pid, 0)
        except (FileNotFoundError, ValueError, ProcessLookupError, PermissionError):
            return None
        return pid

    def ensure_supervisor(self) -> None:
        if self.supervisor_pid():
            return
        with self.paths.supervisor_log.open("a") as log:
            subprocess.Popen(
                [sys.executable, "-m", "bmaclab.supervisor", "--config", str(self.topology.source.resolve()), "--root", str(self.paths.root)],
                stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True,
                cwd=Path(__file__).resolve().parent.parent,
            )
        for _ in range(50):
            if self.supervisor_pid():
                return
            time.sleep(0.1)
        raise LabError(f"the carrier supervisor did not start; see {self.paths.supervisor_log}")

    def stop_supervisor(self) -> None:
        pid = self.supervisor_pid()
        if pid:
            os.kill(pid, signal.SIGTERM)

    # Snapshots

    def snapshot_names(self) -> list[str]:
        if not self.paths.snapshots.exists():
            return []
        return sorted(p.name for p in self.paths.snapshots.iterdir() if (p / "snapshot.json").exists())

    def _check_snapshot_name(self, name: str) -> None:
        if not SNAPSHOT_NAME.match(name):
            raise LabError("snapshot names are 1-32 lowercase letters, digits, hyphens, and underscores")

    def snapshot(self, name: str) -> None:
        self.require_created()
        self._check_snapshot_name(name)
        was_running = [vm for vm in self.topology.vms if self.running(vm)]
        if was_running:
            self.log("Shutting the lab down for a consistent snapshot")
            self.stop(was_running)
        folder = self.paths.snapshots / name
        folder.mkdir(parents=True, exist_ok=True)
        for vm in self.topology.vms:
            for disk in vm.disks:
                path = str(self.paths.disk(disk))
                if name in self._disk_snapshots(path):
                    provision.run(["qemu-img", "snapshot", "-d", name, path])
                provision.run(["qemu-img", "snapshot", "-c", name, path])
            shutil.copyfile(self.paths.vars(vm.name), folder / f"{vm.name}.fd")
        (folder / "snapshot.json").write_text(json.dumps({"name": name, "taken": time.time(), "topology_sha256": self.topology_digest}) + "\n")
        self.log(f"Snapshot {name} taken")
        if was_running:
            self.start(was_running)

    def _disk_snapshots(self, path: str) -> set[str]:
        output = provision.run(["qemu-img", "snapshot", "-l", path]).stdout
        names = set()
        for line in output.splitlines()[2:]:
            fields = line.split()
            if len(fields) >= 2:
                names.add(fields[1])
        return names

    def reset(self, name: str) -> None:
        self.require_created()
        if name not in self.snapshot_names():
            raise LabError(f"no snapshot named {name!r}; the lab has: {', '.join(self.snapshot_names()) or 'none'}")
        self.stop(list(self.topology.vms), force=True)
        folder = self.paths.snapshots / name
        for vm in self.topology.vms:
            for disk in vm.disks:
                provision.run(["qemu-img", "snapshot", "-a", name, str(self.paths.disk(disk))])
            shutil.copyfile(folder / f"{vm.name}.fd", self.paths.vars(vm.name))
            for stale in (self.paths.carrier_until(vm.name), *(self.paths.link_held(vm.name, n) for n in LINKS)):
                stale.unlink(missing_ok=True)
        self.log(f"Reset the lab to {name}")

    def delete_snapshot(self, name: str) -> None:
        self.require_created()
        if name not in self.snapshot_names():
            raise LabError(f"no snapshot named {name!r}")
        for vm in self.topology.vms:
            for disk in vm.disks:
                path = str(self.paths.disk(disk))
                if name in self._disk_snapshots(path):
                    provision.run(["qemu-img", "snapshot", "-d", name, path])
        shutil.rmtree(self.paths.snapshots / name)
        self.log(f"Deleted snapshot {name}")

    def checkpoint(self, vm: Vm, name: str) -> None:
        """Saves one stopped VM's disks and firmware store under name, so a
        scenario can reinstall it and put it back with restore_checkpoint."""
        self._check_snapshot_name(name)
        if self.running(vm):
            raise LabError(f"{vm.name} must be off to checkpoint it")
        for disk in vm.disks:
            path = str(self.paths.disk(disk))
            if name in self._disk_snapshots(path):
                provision.run(["qemu-img", "snapshot", "-d", name, path])
            provision.run(["qemu-img", "snapshot", "-c", name, path])
        shutil.copyfile(self.paths.vars(vm.name), self.paths.vm(vm.name) / f"checkpoint-{name}.fd")
        self.log(f"Checkpoint {name} of {vm.name} taken")

    def restore_checkpoint(self, vm: Vm, name: str) -> None:
        """Puts vm back as checkpoint left it, powered off, and drops the checkpoint."""
        saved = self.paths.vm(vm.name) / f"checkpoint-{name}.fd"
        if not saved.exists():
            raise LabError(f"{vm.name} has no checkpoint named {name!r}")
        self.kill(vm)
        for disk in vm.disks:
            path = str(self.paths.disk(disk))
            provision.run(["qemu-img", "snapshot", "-a", name, path])
            provision.run(["qemu-img", "snapshot", "-d", name, path])
        shutil.copyfile(saved, self.paths.vars(vm.name))
        saved.unlink()
        for stale in (self.paths.carrier_until(vm.name), *(self.paths.link_held(vm.name, n) for n in LINKS)):
            stale.unlink(missing_ok=True)
        self.log(f"Restored {vm.name} to checkpoint {name}")

    # Building

    def create(self, install: bool = True) -> None:
        if self.exists():
            raise LabError(f"a lab already exists at {self.paths.root}; `lab/lab rebuild` replaces it")
        self.require_tools()
        assert self.firmware is not None
        self.paths.root.mkdir(parents=True, exist_ok=True)
        if not self.paths.ssh_key.exists():
            provision.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", f"bmac-lab-{self.topology.name}", "-f", str(self.paths.ssh_key)])
        public_key = self.paths.ssh_key.with_suffix(".pub").read_text().strip()
        hashed = provision.password_hash(self.topology)
        needed = {vm.image for vm in self.topology.vms}
        images = {image: provision.fetch(self.topology.images[image]["url"], self.topology.images[image]["sha256"], self.paths.images, self.log) for image in needed}
        assistant = provision.assistant(self.topology, self.paths, self.log) if "proxmox" in needed else None

        for vm in self.topology.vms:
            folder = self.paths.vm(vm.name)
            folder.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(self.firmware[1], self.paths.vars(vm.name))
            for position, disk in enumerate(vm.disks):
                target = str(self.paths.disk(disk))
                if vm.image == "ubuntu" and position == 0:
                    provision.run(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "qcow2", "-b", str(images["ubuntu"]), target, f"{disk.size_gib}G"])
                else:
                    provision.run(["qemu-img", "create", "-q", "-f", "qcow2", target, f"{disk.size_gib}G"])
            if vm.image == "ubuntu":
                provision.build_seed(self.topology, vm, self.paths, hashed, public_key)
            else:
                assert assistant is not None
                self.log(f"Building the auto-install ISO for {vm.name}")
                provision.build_install_iso(self.topology, vm, self.paths, images["proxmox"], assistant, hashed, public_key)

        self.paths.state.write_text(json.dumps({"topology_sha256": self.topology_digest, "created": time.time()}) + "\n")
        self.snapshot("blank")
        if install:
            self.install()

    def install(self) -> None:
        """Installs Proxmox on every host and boots the rest, then snapshots `installed`."""
        hosts = [vm for vm in self.topology.vms if vm.image == "proxmox"]
        for vm in hosts:
            self.start([vm], media=self.paths.install_iso(vm.name))
        deadline = time.monotonic() + INSTALL_TIMEOUT
        for vm in hosts:
            self.log(f"Waiting for the Proxmox installer on {vm.name} to finish and power off")
            if not self.wait_exit(vm, max(0.0, deadline - time.monotonic())):
                raise LabError(f"{vm.name} was still installing after {INSTALL_TIMEOUT // 60} minutes; see `lab/lab vnc {vm.name}`")
        self.start(list(self.topology.vms))
        for vm in self.topology.vms:
            self.wait_ssh(vm, BOOT_TIMEOUT)
            self.log(f"{vm.name} answers SSH")
        self.snapshot("installed")

    def destroy(self, images: bool = False) -> None:
        self.stop(list(self.topology.vms), force=True)
        self.forget_tailnet(required=False)
        self.stop_supervisor()
        for folder in (self.paths.root / "vms", self.paths.snapshots):
            shutil.rmtree(folder, ignore_errors=True)
        for leftover in (self.paths.state, self.paths.supervisor_pid, self.paths.known_hosts):
            leftover.unlink(missing_ok=True)
        if images:
            shutil.rmtree(self.paths.root, ignore_errors=True)
        self.log(f"Destroyed the lab at {self.paths.root}" + (" with its downloads" if images else ""))

    # Tailnet

    def tailscale(self) -> tailnet.Tailscale:
        """The lab tailnet's API, once it and this workstation are proved to
        be on the lab's Tailscale account."""
        secrets = tailnet.read_secrets(self.secrets)
        api = tailnet.Tailscale(tailnet.require(secrets, tailnet.API_KEY))
        if not self._tailnet_verified:
            tailnet.verify_lab_account(api, secrets, tailnet.workstation_status())
            self._tailnet_verified = True
        return api

    def tailnet_devices(self, vms: list[Vm] | None = None) -> list[tailnet.Device]:
        labels = None if vms is None else {self.topology.tailnet_name(vm) for vm in vms}
        return tailnet.lab_devices(self.tailscale(), self.topology.tailnet_prefix, labels)

    def forget_tailnet(self, vms: list[Vm] | None = None, required: bool = True, names: set[str] | None = None) -> None:
        """Remove lab devices (those of VMS or with NAMES, or all of them)
        from the real tailnet. A VM must be forgotten before it joins again,
        or Tailscale names it <name>-1 and MagicDNS keeps the bare name on
        the old device."""
        try:
            api = self.tailscale()
            labels = None
            if vms is not None or names is not None:
                labels = {self.topology.tailnet_name(vm) for vm in vms or []} | (names or set())
            tailnet.remove(api, tailnet.lab_devices(api, self.topology.tailnet_prefix, labels), self.log)
        except tailnet.TailnetError as error:
            if required:
                raise
            self.log(f"Left the tailnet alone: {error}")

    # Access

    def ssh_args(self, vm: Vm, command: list[str] | None = None, tty: bool = False) -> list[str]:
        # The lab's VMs are throwaway and reached only on loopback; their host
        # keys change with every rebuild, so they are not pinned.
        args = [
            "ssh", "-i", str(self.paths.ssh_key), "-p", str(vm.ports["ssh"]),
            "-o", "IdentitiesOnly=yes", "-o", "StrictHostKeyChecking=no", "-o", f"UserKnownHostsFile={os.devnull}",
            "-o", "LogLevel=ERROR", "-o", "ConnectTimeout=5", "-o", "BatchMode=yes",
        ]
        if tty:
            args.append("-t")
        return [*args, "root@127.0.0.1", *(command or [])]

    def ssh(self, vm: Vm, command: str, timeout: float = 120) -> subprocess.CompletedProcess:
        return subprocess.run(self.ssh_args(vm, [command]), capture_output=True, text=True, timeout=timeout, stdin=subprocess.DEVNULL)

    def wait_ssh(self, vm: Vm, timeout: float) -> None:
        deadline = time.monotonic() + timeout
        while True:
            try:
                if self.ssh(vm, "true", timeout=15).returncode == 0:
                    return
            except subprocess.TimeoutExpired:
                pass
            if time.monotonic() > deadline:
                raise LabError(f"{vm.name} did not answer SSH on 127.0.0.1:{vm.ports['ssh']} within {timeout:.0f} seconds")
            time.sleep(3)

    def status(self) -> list[dict]:
        rows = []
        for vm in self.topology.vms:
            row = {
                "vm": vm.name, "role": vm.role, "running": self.running(vm),
                "ssh": vm.ports["ssh"], "pve": vm.ports["pve"], "web": vm.ports["web"],
                "mgmt": vm.mgmt_ip, "vlan": vm.vlan_ip or "", "carrier_delay": self.carrier_delay(vm) if vm.vlan_ip else None,
                "held": [n for n in LINKS if self.paths.link_held(vm.name, n).exists()],
            }
            rows.append(row)
        return rows
