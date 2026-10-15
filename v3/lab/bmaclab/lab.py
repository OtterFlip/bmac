# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Lab operations: create, start, stop, power, links, media, snapshots, reset, destroy."""

from __future__ import annotations

import contextlib
import fcntl
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
STAGE_TIMEOUT = 10 * 60
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


def process_gone(pid: int) -> bool:
    """A zombie has already closed its files."""
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
    except (FileNotFoundError, ProcessLookupError):
        return True
    return stat.rsplit(")", 1)[1].split()[0] in ("Z", "X")


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

    @contextlib.contextmanager
    def scenario_lock(self, name: str):
        """Two scenarios on one lab would stop and restore each other's VMs."""
        self.paths.root.mkdir(parents=True, exist_ok=True)
        fd = os.open(self.paths.root / "scenario.lock", os.O_RDWR | os.O_CREAT, 0o600)
        with os.fdopen(fd, "r+") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise LabError(f"another scenario is running on this lab: {lock.read().strip()}") from None
            lock.truncate(0)
            lock.write(f"{name} (pid {os.getpid()})\n")
            lock.flush()
            yield

    def require_created(self) -> None:
        if not self.exists():
            raise LabError(f"no lab at {self.paths.root}; run `lab/lab create` first")
        state = json.loads(self.paths.state.read_text())
        if state.get("topology_sha256") != self.topology_digest:
            self.log(f"WARNING: {self.topology.source} changed since the lab was created; `lab/lab update` applies an added VM or an Ubuntu VM's network, `lab/lab rebuild` anything else")

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

    def pause(self, vms: list[Vm]) -> None:
        """Stops the VMs' CPUs, so that a live save of each one finds the
        others at the same moment."""
        for vm in vms:
            with self.qmp(vm) as monitor:
                monitor.command("stop")

    def resume(self, vms: list[Vm]) -> None:
        for vm in vms:
            if self.running(vm):
                with self.qmp(vm) as monitor:
                    monitor.command("cont")

    def carrier_delay(self, vm: Vm) -> int:
        override = self.paths.carrier_delay(vm.name)
        return int(override.read_text()) if override.exists() else vm.carrier_delay_seconds

    def set_carrier_delay(self, vm: Vm, seconds: int) -> None:
        if vm.nic("vlan") is None:
            raise LabError(f"{vm.name} has no VLAN NIC")
        if not 0 <= seconds <= 3600:
            raise LabError("the carrier delay must be from 0 to 3600 seconds")
        self.paths.carrier_delay(vm.name).write_text(f"{seconds}\n")

    def start(self, vms: list[Vm], media: Path | None = None, no_reboot: bool = False, stage: str | None = None) -> None:
        """Starts vms; with stage, each resumes from that live stage instead
        of booting."""
        self.require_created()
        self.require_tools()
        assert self.firmware is not None
        # Before any VM, so their first frames have a switch to reach.
        self.ensure_supervisor()
        for vm in vms:
            if self.running(vm):
                if media:
                    raise LabError(f"{vm.name} is already running; use `lab/lab media` to change its CD")
                continue
            nics = [self.paths.nic(vm.name, nic.network) for nic in vm.nics if nic.network != "uplink"]
            for stale in (self.paths.qmp(vm.name), self.paths.events(vm.name), self.paths.console(vm.name),
                          self.paths.vnc(vm.name), *nics):
                stale.unlink(missing_ok=True)
            args = qemu.argv(self.topology, vm, self.paths, self.firmware[0], media, self.qemu_binary, no_reboot)
            try:
                subprocess.run(args, check=True, capture_output=True, text=True)
            except subprocess.CalledProcessError as error:
                raise LabError(f"QEMU did not start {vm.name}: {error.stderr.strip()}") from error
            with Qmp(self.paths.qmp(vm.name), timeout=30 if stage else 10) as monitor:
                if stage:
                    self._snapshot_job(monitor, vm, "snapshot-load", f"stage-{stage}")
                for network in LINKS:
                    if vm.nic(network) and self.paths.link_held(vm.name, network).exists():
                        monitor.command("set_link", name=f"nic-{network}", up=False)
                # A resumed VM was already past its boot's carrier delay.
                delay = None if stage else self.carrier_delay(vm)
                if delay and not self.paths.link_held(vm.name, "vlan").exists():
                    monitor.command("set_link", name="nic-vlan", up=False)
                    self.paths.carrier_until(vm.name).write_text(f"{time.time() + delay}\n")
                self.paths.last_reset(vm.name).write_text(f"{time.time()}\n")
                monitor.command("cont")
            self.log(f"Started {vm.name}" + (f" from {media.name}" if media else ""))
        self.ensure_supervisor()

    def wait_exit(self, vm: Vm, timeout: float) -> bool:
        # A dying QEMU's cmdline reads empty while the kernel frees its
        # memory, long before it lets go of its disk images' locks.
        pid = self.pid(vm)
        deadline = time.monotonic() + timeout
        while pid is not None and not process_gone(pid):
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
            # A process with gigabytes of guest memory takes a while to go.
            if not self.wait_exit(vm, 60):
                raise LabError(f"{vm.name} (pid {pid}) is still running after SIGKILL")
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

    def restart_supervisor(self) -> None:
        """Starts the supervisor again, if it runs, so its switch knows the
        topology's current VMs."""
        if not self.supervisor_pid():
            return
        self.stop_supervisor()
        for _ in range(100):
            if not self.supervisor_pid():
                break
            time.sleep(0.05)
        else:
            raise LabError("the carrier supervisor did not stop")
        self.ensure_supervisor()

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

    # Stages

    def _stage_dir(self, vm: Vm, name: str) -> Path:
        return self.paths.vm(vm.name) / "stages" / name

    def stage(self, vm: Vm, name: str) -> dict | None:
        """What save_stage recorded for vm's stage name, or None."""
        try:
            return json.loads((self._stage_dir(vm, name) / "stage.json").read_text())
        except (OSError, ValueError):
            return None

    def stage_names(self, vm: Vm) -> list[str]:
        folder = self.paths.vm(vm.name) / "stages"
        return sorted(p.name for p in folder.iterdir() if (p / "stage.json").exists()) if folder.exists() else []

    def can_save_live(self, vm: Vm) -> bool:
        """QEMU can't save a running NVMe controller."""
        return all(self.topology.bus(disk) != "nvme" for disk in vm.disks)

    def _snapshot_job(self, monitor: Qmp, vm: Vm, kind: str, tag: str) -> None:
        """Runs QMP's snapshot-save, -load, or -delete over vm's disks, with
        the VM's memory in the first. The firmware store is raw, so it is
        left out and copied beside the stage instead."""
        nodes = {d["device"]: d["inserted"]["node-name"] for d in monitor.command("query-block") if d.get("inserted")}
        devices = [nodes[f"d-{disk.id}"] for disk in vm.disks]
        job = f"{kind}-{vm.name}"
        arguments: dict = {"job-id": job, "tag": tag, "devices": devices}
        if kind != "snapshot-delete":
            arguments["vmstate"] = devices[0]
        monitor.command(kind, **arguments)
        deadline = time.monotonic() + STAGE_TIMEOUT
        while True:
            state = next((j for j in monitor.command("query-jobs") if j["id"] == job), None)
            if state is not None and state["status"] == "concluded":
                monitor.command("job-dismiss", id=job)
                if state.get("error"):
                    raise LabError(f"{kind} of {vm.name}: {state['error']}")
                return
            if time.monotonic() > deadline:
                raise LabError(f"{kind} of {vm.name} did not finish within {STAGE_TIMEOUT} seconds")
            time.sleep(0.5)

    def save_stage(self, vm: Vm, name: str, files: Path, meta: dict, live: bool = False) -> None:
        """Saves vm's disks and firmware store, a copy of files, and meta as
        stage name. Unlike a checkpoint it outlives the scenario, so a later
        run can start from it with restore_stage. A live stage is saved
        while vm runs, its memory with it; otherwise a running vm is shut
        down for the save and started again."""
        tag = f"stage-{name}"
        self._check_snapshot_name(tag)
        if live and not self.running(vm):
            raise LabError(f"{vm.name} must be running to save it live")
        if live and not self.can_save_live(vm):
            raise LabError(f"{vm.name} has NVMe disks, which QEMU can't save live; use a topology with nvme_as")
        restart = self.running(vm) and not live
        if restart:
            self.stop([vm])
            if self.running(vm):
                raise LabError(f"{vm.name} did not shut down for its stage")
        folder = self._stage_dir(vm, name)
        shutil.rmtree(folder, ignore_errors=True)
        folder.mkdir(parents=True, mode=0o700)
        if live:
            # QEMU's monitor doesn't answer until it has written the guest's
            # memory, which takes minutes for a guest that has used it all.
            with Qmp(self.paths.qmp(vm.name), timeout=STAGE_TIMEOUT) as monitor:
                with contextlib.suppress(LabError):
                    self._snapshot_job(monitor, vm, "snapshot-delete", tag)
                self._snapshot_job(monitor, vm, "snapshot-save", tag)
        else:
            for disk in vm.disks:
                path = str(self.paths.disk(disk))
                if tag in self._disk_snapshots(path):
                    provision.run(["qemu-img", "snapshot", "-d", tag, path])
                provision.run(["qemu-img", "snapshot", "-c", tag, path])
        shutil.copyfile(self.paths.vars(vm.name), folder / "vars.fd")
        shutil.copytree(files, folder / "files", symlinks=True)
        fd = os.open(folder / "stage.json", os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w") as out:
            json.dump({**meta, "name": name, "taken": time.time(), "live": live}, out)
        self.log(f"Stage {name} of {vm.name} saved" + (" live" if live else ""))
        if restart:
            self.start([vm])

    def restore_stage(self, vm: Vm, name: str, files: Path) -> dict:
        """Puts vm back as stage name left it, with its files copied back to
        files, and keeps the stage. A live stage is running again where it
        was, its clock still at the save's time (the caller sets it); any
        other is powered off."""
        meta = self.stage(vm, name)
        if meta is None:
            raise LabError(f"{vm.name} has no stage named {name!r}")
        self.kill(vm)
        folder = self._stage_dir(vm, name)
        shutil.copyfile(folder / "vars.fd", self.paths.vars(vm.name))
        shutil.rmtree(files, ignore_errors=True)
        shutil.copytree(folder / "files", files, symlinks=True)
        for stale in (self.paths.carrier_until(vm.name), *(self.paths.link_held(vm.name, n) for n in LINKS)):
            stale.unlink(missing_ok=True)
        if meta.get("live"):
            self.start([vm], stage=name)
        else:
            for disk in vm.disks:
                provision.run(["qemu-img", "snapshot", "-a", f"stage-{name}", str(self.paths.disk(disk))])
        self.log(f"Restored {vm.name} to stage {name}")
        return meta

    def drop_stages(self, vm: Vm) -> None:
        """Deletes every stage of vm, as when what they depend on, such as
        the host's tailnet device, is gone. vm must be off."""
        if self.running(vm):
            raise LabError(f"{vm.name} must be off to drop its stages")
        for name in self.stage_names(vm):
            for disk in vm.disks:
                path = str(self.paths.disk(disk))
                if f"stage-{name}" in self._disk_snapshots(path):
                    provision.run(["qemu-img", "snapshot", "-d", f"stage-{name}", path])
            shutil.rmtree(self._stage_dir(vm, name))
            self.log(f"Dropped stage {name} of {vm.name}")

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

    def update(self) -> None:
        """Applies lab.toml changes that need no reinstall: creates each VM
        added since the lab was built, with every lab snapshot taken of it as
        created, and writes each Ubuntu VM's cloud-init seed again, which its
        next boot applies. Another change to a Proxmox host needs a rebuild."""
        self.require_created()
        self.require_tools()
        assert self.firmware is not None
        public_key = self.paths.ssh_key.with_suffix(".pub").read_text().strip()
        hashed = provision.password_hash(self.topology)
        added = [vm for vm in self.topology.vms if not self.paths.disk(vm.disks[0]).exists()]
        for vm in added:
            if vm.image != "ubuntu":
                raise LabError(f"{vm.name} is a new Proxmox host; `lab/lab rebuild` installs it")
        image = None
        if any(vm.image == "ubuntu" for vm in self.topology.vms):
            spec = self.topology.images["ubuntu"]
            image = provision.fetch(spec["url"], spec["sha256"], self.paths.images, self.log)
        snapshots = self.snapshot_names()
        for vm in added:
            self.paths.vm(vm.name).mkdir(parents=True, exist_ok=True)
            shutil.copyfile(self.firmware[1], self.paths.vars(vm.name))
            for position, disk in enumerate(vm.disks):
                target = str(self.paths.disk(disk))
                if position == 0:
                    provision.run(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "qcow2", "-b", str(image), target, f"{disk.size_gib}G"])
                else:
                    provision.run(["qemu-img", "create", "-q", "-f", "qcow2", target, f"{disk.size_gib}G"])
                for name in snapshots:
                    provision.run(["qemu-img", "snapshot", "-c", name, target])
            for name in snapshots:
                shutil.copyfile(self.paths.vars(vm.name), self.paths.snapshots / name / f"{vm.name}.fd")
            self.log(f"Created {vm.name}")
        for vm in self.topology.vms:
            if vm.image == "ubuntu":
                provision.build_seed(self.topology, vm, self.paths, hashed, public_key)
        state = json.loads(self.paths.state.read_text())
        state["topology_sha256"] = self.topology_digest
        self.paths.state.write_text(json.dumps(state) + "\n")
        self.restart_supervisor()
        self.log("Updated the lab; a running Ubuntu VM takes its new seed when it boots again")

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
