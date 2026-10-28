#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Switch a running instant-restored VM from its overlay to its zvol.

An instant restore boots the VM from a qcow2 overlay over its restore point,
mapped read-only from PBS (backup_link.sh), and meanwhile writes the restore
point into the VM's zvol with pbs-restore. Once that copy is complete, this
helper moves the running disk onto the zvol without stopping the guest:

  1. add the zvol to QEMU as a block node, named the way Proxmox names it;
  2. reopen the overlay with the zvol as its backing in place of the mapping
     (both hold the same data, so the guest sees no change);
  3. commit the overlay into the zvol with an active block-commit, and
     complete it, which leaves the zvol as the disk itself;
  4. only then point scsi0 in the VM's config at the zvol.

The config changes last so that a VM started from it can never boot from a
zvol that lacks the guest's writes. Every step is safe to repeat: run again
after a failure, `switch` picks up from what QEMU shows.

  instant_switch.py state VMID OVERLAY_VOLUME TARGET_VOLUME
  instant_switch.py switch VMID OVERLAY_VOLUME TARGET_VOLUME
  instant_switch.py attach VMID OVERLAY_VOLUME TARGET_VOLUME

`attach` is the start of Instant Restore to Existing VM: with the VM
stopped, scsi0 in its config moves from its zvol to its overlay, keeping its
options, with the zvol's size. `qm set` would keep the replaced zvol as an
unusedN disk, whose later removal would destroy it.

`state` prints one word:
  stopped   the VM is not running
  backup    it runs on the overlay over the mapping; the zvol is not in use,
            so it may be freed and copied into
  target    it runs on the overlay over the zvol; the copy is done and the
            zvol must not be touched
  switched  it runs on the zvol alone

`switch` prints `switched` once the config names the zvol, `attach` prints
`attached` once it names the overlay. Exit status: 0 on success, 1 on
failure, 2 for usage errors.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import time
from typing import Any


DEVICE = "drive-scsi0"
DRIVE = "scsi0"
READY_TIMEOUT_SECONDS = 6 * 3600
COMPLETE_TIMEOUT_SECONDS = 600
POLL_SECONDS = 2
PROGRESS_SECONDS = 30
VOLUME_RE = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.+-]{0,63}:[A-Za-z0-9][A-Za-z0-9_./+-]*")

# QMP and config edits go through Proxmox's own modules: the monitor socket
# is shared with pvestatd and qmeventd, and the config may only be written
# under its lock.
PVE_BACKEND = r"""
use strict;
use warnings;
use JSON;
use PVE::QemuConfig;
use PVE::QemuServer::Monitor;

my ($action, $vmid, @rest) = @ARGV;
if ($action eq 'qmp') {
    my ($command, $arguments) = @rest;
    my %params = %{ decode_json($arguments // '{}') };
    my $result = eval { PVE::QemuServer::Monitor::mon_cmd($vmid, $command, %params) };
    if ($@) { print STDERR "$@"; exit 1; }
    print JSON->new->canonical->allow_nonref->encode($result // {}), "\n";
} elsif ($action eq 'config') {
    my $conf = PVE::QemuConfig->load_config($vmid);
    print encode_json({ scsi0 => $conf->{scsi0}, lock => $conf->{lock},
                        pending => $conf->{pending}{scsi0} }), "\n";
} elsif ($action eq 'set-scsi0') {
    my ($expected, $new) = @rest;
    PVE::QemuConfig->lock_config($vmid, sub {
        my $conf = PVE::QemuConfig->load_config($vmid);
        PVE::QemuConfig->check_lock($conf);
        die "scsi0 changed underneath the switch\n" if ($conf->{scsi0} // '') ne $expected;
        die "scsi0 has a pending change\n" if defined($conf->{pending}{scsi0});
        $conf->{scsi0} = $new;
        PVE::QemuConfig->write_config($vmid, $conf);
    });
} else {
    die "unknown action $action\n";
}
"""


class SwitchError(Exception):
    pass


def log(message: str) -> None:
    print(f"instant_switch: {message}", file=sys.stderr, flush=True)


def backend(*arguments: str) -> str:
    command = os.environ.get("APP_HA_INSTANT_SWITCH_BACKEND")
    argv = [command, *arguments] if command else ["perl", "-e", PVE_BACKEND, *arguments]
    completed = subprocess.run(argv, text=True, capture_output=True, check=False)
    if completed.returncode != 0:
        raise SwitchError(f"{arguments[0]} {' '.join(arguments[2:3])} failed: {completed.stderr.strip()}")
    return completed.stdout


def qmp(vmid: str, command: str, arguments: dict[str, Any] | None = None) -> Any:
    return json.loads(backend("qmp", vmid, command, json.dumps(arguments or {})))


def run(*argv: str) -> str:
    completed = subprocess.run(argv, text=True, capture_output=True, check=False)
    if completed.returncode != 0:
        raise SwitchError(f"{' '.join(argv)} failed: {completed.stderr.strip()}")
    return completed.stdout


def node_name(prefix: str, volume: str) -> str:
    """The name Proxmox (qemu-server 9) gives a drive's block node."""
    digest = hashlib.sha256(f"drive={DRIVE},volid={volume}".encode()).hexdigest()
    return prefix + digest[:30]


def format_size(size: int) -> str:
    for suffix, unit in (("T", 1 << 40), ("G", 1 << 30), ("M", 1 << 20), ("K", 1 << 10)):
        if size % unit == 0:
            return f"{size // unit}{suffix}"
    return str(size)


def device_size(path: str) -> int:
    with open(path, "rb") as device:
        return device.seek(0, os.SEEK_END)


def is_running(vmid: str) -> bool:
    return run("qm", "status", vmid).strip() == "status: running"


def drive_volume(drive: str) -> str:
    return drive.split(",", 1)[0]


def switched_drive(drive: str, target: str, size: int) -> str:
    """scsi0 with only its volume and size changed."""
    options = drive.split(",")[1:]
    if sum(option.startswith("size=") for option in options) != 1:
        raise SwitchError(f"scsi0 has no single size: {drive}")
    options = [f"size={format_size(size)}" if option.startswith("size=") else option for option in options]
    return ",".join([target, *options])


def scsi0_blockdev(vmid: str) -> dict[str, Any]:
    """The -blockdev Proxmox starts this VM's scsi0 with, from qm showcmd."""
    arguments = shlex.split(run("qm", "showcmd", vmid))
    for flag, value in zip(arguments, arguments[1:]):
        if flag == "-blockdev" and value.startswith("{"):
            blockdev = json.loads(value)
            if blockdev.get("node-name") == DEVICE:
                return blockdev["file"]
    raise SwitchError(f"qm showcmd {vmid} has no -blockdev for {DEVICE}")


def named_nodes(vmid: str) -> dict[str, dict[str, Any]]:
    return {node["node-name"]: node for node in qmp(vmid, "query-named-block-nodes", {"flat": True})}


def child(node: dict[str, Any], role: str) -> str | None:
    for entry in node.get("children", []):
        if entry.get("child") == role:
            return entry["node-name"]
    return None


def node_path(nodes: dict[str, dict[str, Any]], name: str | None) -> str | None:
    """The file or device a format node reads, through its file child."""
    if name is None or name not in nodes:
        return None
    file_node = nodes.get(child(nodes[name], "file") or "")
    return file_node.get("file") if file_node else None


class Switch:
    def __init__(self, vmid: str, overlay: str, target: str) -> None:
        for volume in (overlay, target):
            if not VOLUME_RE.fullmatch(volume) or ".." in volume:
                raise SwitchError(f"invalid volume {volume}")
        self.vmid = vmid
        self.overlay = overlay
        self.target = target
        self.overlay_fmt = node_name("f", overlay)
        self.overlay_file = node_name("e", overlay)
        self.target_fmt = node_name("f", target)
        self.target_file = node_name("e", target)
        self.target_path = run("pvesm", "path", target).strip()
        self.job = f"bmac-switch-{vmid}"

    def config(self) -> dict[str, Any]:
        config = json.loads(backend("config", self.vmid))
        if config.get("lock"):
            raise SwitchError(f"VM {self.vmid} is locked ({config['lock']})")
        if config.get("pending") is not None:
            raise SwitchError(f"VM {self.vmid} has a pending scsi0 change")
        drive = config.get("scsi0") or ""
        if drive_volume(drive) not in (self.overlay, self.target):
            raise SwitchError(f"VM {self.vmid}'s scsi0 is neither the overlay nor the target: {drive}")
        return config

    def state(self) -> str:
        self.config()
        if not is_running(self.vmid):
            return "stopped"
        nodes = named_nodes(self.vmid)
        top = child(nodes.get(DEVICE, {}), "file")
        if top == self.target_fmt and node_path(nodes, top) == self.target_path:
            if child(nodes[top], "backing") is not None:
                raise SwitchError(f"the zvol of VM {self.vmid} still has a backing node")
            return "switched"
        if top != self.overlay_fmt or node_path(nodes, top) is None:
            raise SwitchError(
                f"VM {self.vmid}'s {DEVICE} runs on {top}, not on the overlay node {self.overlay_fmt} "
                "Proxmox would name; this Proxmox version names its block nodes differently"
            )
        backing = child(nodes[top], "backing")
        if backing is not None and node_path(nodes, backing) == self.target_path:
            return "target"
        if backing is None:
            raise SwitchError(f"the overlay of VM {self.vmid} has no backing node")
        return "backup"

    def add_target_node(self, overlay_blockdev: dict[str, Any]) -> None:
        nodes = named_nodes(self.vmid)
        if self.target_fmt in nodes:
            if node_path(nodes, self.target_fmt) != self.target_path:
                raise SwitchError(f"block node {self.target_fmt} exists but does not read {self.target_path}")
            return
        # The zvol's node takes the overlay's options, as Proxmox would give
        # the same drive options on a raw zvol.
        target = json.loads(json.dumps(overlay_blockdev))
        target["driver"] = "raw"
        target["node-name"] = self.target_fmt
        target["file"]["driver"] = "host_device"
        target["file"]["filename"] = self.target_path
        target["file"]["node-name"] = self.target_file
        for key in ("backing", "size"):
            target.pop(key, None)
        log(f"adding {self.target_path} as block node {self.target_fmt}")
        qmp(self.vmid, "blockdev-add", target)

    def reopen_onto_target(self, overlay_blockdev: dict[str, Any]) -> None:
        # blockdev-reopen resets every option it is not given, so it gets
        # the overlay's own options, with its file by name and the new backing.
        options = json.loads(json.dumps(overlay_blockdev))
        options["file"] = options["file"]["node-name"]
        options["backing"] = self.target_fmt
        log(f"reopening the overlay over {self.target_path}")
        qmp(self.vmid, "blockdev-reopen", {"options": [options]})

    def overlay_blockdev(self) -> dict[str, Any]:
        overlay = scsi0_blockdev(self.vmid)
        if (
            overlay.get("driver") != "qcow2"
            or overlay.get("node-name") != self.overlay_fmt
            or not isinstance(overlay.get("file"), dict)
            or overlay["file"].get("node-name") != self.overlay_file
        ):
            raise SwitchError(f"qm showcmd {self.vmid} does not describe the overlay as the switch expects")
        return overlay

    def job_status(self) -> dict[str, Any] | None:
        for job in qmp(self.vmid, "query-jobs"):
            if job.get("id") == self.job:
                return job
        return None

    def wait_for(self, wanted: set[str], timeout: int) -> dict[str, Any]:
        """Wait until the job reaches a wanted status or concludes."""
        started = time.monotonic()
        reported = started
        while True:
            job = self.job_status()
            if job is None:
                raise SwitchError(f"block job {self.job} disappeared")
            if job["status"] in wanted or job["status"] == "concluded":
                return job
            now = time.monotonic()
            if now - started > timeout:
                raise SwitchError(f"block job {self.job} is still {job['status']} after {timeout}s")
            if now - reported >= PROGRESS_SECONDS:
                reported = now
                total = job.get("total-progress") or 0
                done = job.get("current-progress") or 0
                percent = f"{100 * done // total}%" if total else "starting"
                log(f"committing the overlay: {percent}")
            time.sleep(POLL_SECONDS)

    def commit(self) -> None:
        """Run the commit job to its end, starting it unless one is underway.

        Whether it switched the disk is read from QEMU afterwards: a job that
        failed or was cancelled leaves the overlay on top.
        """
        job = self.job_status()
        if job is not None and job["status"] == "concluded":
            log(f"dismissing the earlier job {self.job}" + (f", which failed: {job['error']}" if job.get("error") else ""))
            qmp(self.vmid, "job-dismiss", {"id": self.job})
            job = None
        if job is None:
            log("committing the overlay into the zvol")
            qmp(self.vmid, "block-commit", {
                "job-id": self.job, "device": DEVICE, "top-node": self.overlay_fmt,
                "base-node": self.target_fmt, "auto-finalize": True, "auto-dismiss": False,
            })
        job = self.wait_for({"ready"}, READY_TIMEOUT_SECONDS)
        if job["status"] == "ready":
            log("the commit is in sync; completing the switch")
            qmp(self.vmid, "job-complete", {"id": self.job})
            job = self.wait_for(set(), COMPLETE_TIMEOUT_SECONDS)
        qmp(self.vmid, "job-dismiss", {"id": self.job})
        if job.get("error"):
            raise SwitchError(f"block job {self.job} failed: {job['error']}")

    def point_config_at_target(self) -> None:
        drive = self.config()["scsi0"]
        if drive_volume(drive) == self.target:
            return
        new = switched_drive(drive, self.target, device_size(self.target_path))
        log(f"pointing scsi0 at {self.target}")
        backend("set-scsi0", self.vmid, drive, new)

    def check_proxmox_agrees(self) -> None:
        """Proxmox's own -blockdev for the new config must match the live nodes."""
        expected = scsi0_blockdev(self.vmid)
        if expected.get("node-name") != self.target_fmt or expected.get("file", {}).get("node-name") != self.target_file:
            raise SwitchError(f"Proxmox names the zvol's block node {expected.get('node-name')}, not {self.target_fmt}")
        nodes = named_nodes(self.vmid)
        if child(nodes.get(DEVICE, {}), "file") != self.target_fmt or node_path(nodes, self.target_fmt) != self.target_path:
            raise SwitchError(f"VM {self.vmid}'s {DEVICE} is not on {self.target_path}")

    def attach(self) -> None:
        drive = self.config()["scsi0"]
        if drive_volume(drive) == self.overlay:
            return
        if is_running(self.vmid):
            raise SwitchError(f"VM {self.vmid} must be stopped before its overlay is attached")
        new = switched_drive(drive, self.overlay, device_size(self.target_path))
        log(f"pointing scsi0 at {self.overlay}")
        backend("set-scsi0", self.vmid, drive, new)

    def switch(self) -> None:
        state = self.state()
        if state == "stopped":
            raise SwitchError(f"VM {self.vmid} is not running")
        if state == "backup":
            overlay = self.overlay_blockdev()
            self.add_target_node(overlay)
            self.reopen_onto_target(overlay)
            state = self.state()
            if state != "target":
                raise SwitchError(f"the overlay of VM {self.vmid} is still on its {state} after the reopen")
        if state == "target":
            self.commit()
            state = self.state()
            if state != "switched":
                raise SwitchError(f"VM {self.vmid} is still on its overlay after the commit; run the switch again")
        self.point_config_at_target()
        self.check_proxmox_agrees()


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("command", choices=("state", "switch", "attach"))
    parser.add_argument("vmid")
    parser.add_argument("overlay")
    parser.add_argument("target")
    arguments = parser.parse_args(argv)
    if not re.fullmatch(r"[1-9][0-9]{0,8}", arguments.vmid):
        parser.error("VMID must be a number")
    try:
        switch = Switch(arguments.vmid, arguments.overlay, arguments.target)
        if arguments.command == "state":
            print(switch.state())
        elif arguments.command == "attach":
            switch.attach()
            print("attached")
        else:
            switch.switch()
            print("switched")
    except SwitchError as error:
        log(str(error))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
