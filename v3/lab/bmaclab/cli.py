# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""`lab/lab`: the nested lab's command line."""

from __future__ import annotations

import argparse
import os
import select
import socket
import subprocess
import sys
import termios
import tty
from pathlib import Path

from . import scenario, tailnet
from .bootstrap import Bootstrap, BootstrapError
from .lab import Lab, LabError
from .paths import Paths, default_root
from .provision import ProvisionError
from .qmp import QmpError
from .tailnet import TailnetError
from .topology import TopologyError, load

DEFAULT_CONFIG = Path(__file__).resolve().parent.parent / "lab.toml"
POSTMARK_TOOL = Path(__file__).resolve().parents[2] / "dev" / "postmark_templates.py"
POSTMARK_KEY = "POSTMARKAPP_LAB_API_KEY"
POSTMARK_SENDER = "POSTMARKAPP_LAB_API_SENDER_SIGNATURE"
ESCAPE = b"\x1d"  # Ctrl-]


def parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(prog="lab/lab", description="Build and drive BMAC's nested Proxmox test lab.")
    p.add_argument("--config", type=Path, default=DEFAULT_CONFIG, help="topology file (default: lab/lab.toml)")
    sub = p.add_subparsers(dest="command", required=True, metavar="COMMAND")

    sub.add_parser("check", help="report what this machine is missing to run the lab")
    create = sub.add_parser("create", help="build the lab from nothing and install every VM")
    create.add_argument("--no-install", action="store_true", help="stop after creating blank disks (snapshot `blank`)")
    rebuild = sub.add_parser("rebuild", help="destroy the lab, then create it again")
    rebuild.add_argument("--no-install", action="store_true")
    destroy = sub.add_parser("destroy", help="power off and delete every VM, disk, and snapshot")
    destroy.add_argument("--images", action="store_true", help="also delete downloaded ISOs, images, and tools")
    sub.add_parser("status", help="show each VM, its ports, and its links")

    start = sub.add_parser("start", help="power on VMs (all by default)")
    start.add_argument("vms", nargs="*", metavar="VM")
    start.add_argument("--iso", type=Path, help="boot the VM from this ISO")
    stop = sub.add_parser("stop", help="shut VMs down cleanly (all by default)")
    stop.add_argument("vms", nargs="*", metavar="VM")
    stop.add_argument("--force", action="store_true", help="pull the plug instead")
    power = sub.add_parser("power", help="power a VM on or off, reset it, or power-cycle it")
    power.add_argument("vm")
    power.add_argument("action", choices=["on", "off", "shutdown", "reset", "cycle"])

    link = sub.add_parser("link", help="hold a VM's uplink, management, or VLAN link down, or bring it back")
    link.add_argument("vm")
    link.add_argument("network", choices=["uplink", "mgmt", "vlan"])
    link.add_argument("state", choices=["up", "down"])
    cut = sub.add_parser("cut", help="hold every link of a VM down except its uplink")
    cut.add_argument("vm")
    restore = sub.add_parser("restore", help="bring back every link `cut` or `link down` held")
    restore.add_argument("vm")
    carrier = sub.add_parser("carrier", help="set how long a VM's VLAN carrier stays down after each boot")
    carrier.add_argument("vm")
    carrier.add_argument("seconds", type=int)
    media = sub.add_parser("media", help="insert an ISO into a running VM's CD drive, or eject it")
    media.add_argument("vm")
    media.add_argument("iso", nargs="?", type=Path, help="omit to eject")

    snap = sub.add_parser("snapshot", help="shut the lab down and snapshot every disk")
    snap.add_argument("name")
    sub.add_parser("snapshots", help="list snapshots")
    reset = sub.add_parser("reset", help="power off and return every VM to a snapshot")
    reset.add_argument("name")
    reset.add_argument("--start", action="store_true", help="power every VM on afterwards")
    delete = sub.add_parser("delete-snapshot", help="delete a snapshot")
    delete.add_argument("name")

    console = sub.add_parser("console", help="attach to a VM's serial console (Ctrl-] detaches)")
    console.add_argument("vm")
    ssh = sub.add_parser("ssh", help="SSH to a VM as root")
    ssh.add_argument("vm")
    ssh.add_argument("remote", nargs=argparse.REMAINDER, help="command to run")
    vnc = sub.add_parser("vnc", help="show how to view a VM's screen")
    vnc.add_argument("vm")
    boot = sub.add_parser("bootstrap", help="make the hosts a cluster running a development build of bmacd (M2.13)")
    boot.add_argument("--no-build", action="store_true", help="keep the bmacd already installed on the hosts")
    boot.add_argument("--snapshot", metavar="NAME", help="snapshot the lab afterwards")
    tail = sub.add_parser("tailnet", help="list the lab's devices on the real tailnet, or remove them")
    tail.add_argument("action", choices=["list", "remove"])
    tail.add_argument("vms", nargs="*", metavar="VM", help="only these VMs' devices (default: every lab device)")
    mail = sub.add_parser(
        "postmark",
        help="run dev/postmark_templates.py against the lab's sandbox Postmark server",
    )
    mail.add_argument("tool", nargs=argparse.REMAINDER, help="the tool's arguments, e.g. list or sync --apply")
    run = sub.add_parser("scenario", help="run a scenario from lab/scenarios/ and report PASS or FAIL")
    run.add_argument("name", help="scenario name, or a path to a .py file")
    return p


def attach(path: Path) -> None:
    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    sock.connect(str(path))
    print("Attached; press Ctrl-] to detach.\r", file=sys.stderr)
    stdin = sys.stdin.fileno()
    saved = termios.tcgetattr(stdin) if os.isatty(stdin) else None
    try:
        if saved:
            tty.setraw(stdin)
        while True:
            ready, _, _ = select.select([sock, stdin], [], [])
            if sock in ready:
                data = sock.recv(65536)
                if not data:
                    break
                os.write(sys.stdout.fileno(), data)
            if stdin in ready:
                data = os.read(stdin, 1024)
                if not data or ESCAPE in data:
                    break
                sock.sendall(data)
    finally:
        if saved:
            termios.tcsetattr(stdin, termios.TCSADRAIN, saved)
        sock.close()
    print("\nDetached.", file=sys.stderr)


def postmark(secrets: Path, tool_args: list[str]) -> int:
    """The template tool with the lab's server token, which it reads from the
    environment and never prints, and the lab's confirmed sender, if set."""
    values = tailnet.read_secrets(secrets)
    env = {**os.environ, "POSTMARK_SERVER_TOKEN": tailnet.require(values, POSTMARK_KEY)}
    if values.get(POSTMARK_SENDER):
        env["POSTMARK_SENDER"] = values[POSTMARK_SENDER]
    return subprocess.run([sys.executable, str(POSTMARK_TOOL), *tool_args], env=env, check=False).returncode


def main(argv: list[str] | None = None) -> int:
    args = parser().parse_args(argv)
    try:
        if args.command == "postmark":
            return postmark(tailnet.SECRETS, args.tool)
        topology = load(args.config)
        lab = Lab(topology, Paths(default_root(topology)))
        return dispatch(lab, args)
    except (TopologyError, LabError, ProvisionError, QmpError, BootstrapError, FileNotFoundError, ValueError, TailnetError) as error:
        print(f"lab: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        return 130


def dispatch(lab: Lab, args: argparse.Namespace) -> int:
    t = lab.topology
    command = args.command
    if command == "check":
        problems = lab.problems()
        for problem in problems:
            print(f"MISSING  {problem}")
        if not problems:
            print(f"Ready. The lab lives in {lab.paths.root}")
        return 1 if problems else 0
    if command == "create":
        lab.create(install=not args.no_install)
    elif command == "rebuild":
        lab.destroy()
        lab.create(install=not args.no_install)
    elif command == "destroy":
        lab.destroy(images=args.images)
    elif command == "status":
        if not lab.exists():
            print(f"No lab at {lab.paths.root}")
            return 0
        print(f"{'VM':<10} {'ROLE':<10} {'STATE':<8} {'SSH':>6} {'PVE':>6} {'WEB':>6}  {'MGMT':<14} {'VLAN':<14} CARRIER")
        for row in lab.status():
            delay = "-" if row["carrier_delay"] is None else f"{row['carrier_delay']}s delay"
            if row["held"]:
                delay += f" (held down: {', '.join(row['held'])})"
            state = "running" if row["running"] else "off"
            print(f"{row['vm']:<10} {row['role']:<10} {state:<8} {row['ssh']:>6} {row['pve']:>6} {row['web']:>6}  {row['mgmt']:<14} {row['vlan']:<14} {delay}")
        snapshots = lab.snapshot_names()
        print(f"\nSnapshots: {', '.join(snapshots) or 'none'}")
    elif command == "start":
        lab.start(t.select(args.vms), media=args.iso)
    elif command == "stop":
        lab.stop(t.select(args.vms), force=args.force)
    elif command == "power":
        lab.power(t.vm(args.vm), args.action)
    elif command == "link":
        lab.link(t.vm(args.vm), args.network, args.state == "up")
    elif command in ("cut", "restore"):
        vm = t.vm(args.vm)
        for nic in vm.nics:
            if nic.network != "uplink":
                lab.link(vm, nic.network, command == "restore")
    elif command == "carrier":
        lab.set_carrier_delay(t.vm(args.vm), args.seconds)
        print(f"{args.vm}'s VLAN carrier will come up {args.seconds}s after each boot")
    elif command == "media":
        lab.media(t.vm(args.vm), args.iso)
    elif command == "snapshot":
        lab.snapshot(args.name)
    elif command == "snapshots":
        for name in lab.snapshot_names():
            print(name)
    elif command == "reset":
        lab.reset(args.name)
        if args.start:
            lab.start(list(t.vms))
    elif command == "delete-snapshot":
        lab.delete_snapshot(args.name)
    elif command == "console":
        vm = t.vm(args.vm)
        if not lab.running(vm):
            raise LabError(f"{vm.name} is not running")
        attach(lab.paths.console(vm.name))
    elif command == "ssh":
        remote = [a for a in args.remote if a != "--"]
        ssh_args = lab.ssh_args(t.vm(args.vm), remote, tty=not remote)
        os.execvp(ssh_args[0], ssh_args)
    elif command == "vnc":
        vm = t.vm(args.vm)
        print(f"{vm.name}'s screen is a VNC server on the Unix socket {lab.paths.vnc(vm.name)}.")
        print(f"Forward it and point a VNC viewer at 127.0.0.1:5901:\n  socat TCP-LISTEN:5901,bind=127.0.0.1,reuseaddr,fork UNIX-CONNECT:{lab.paths.vnc(vm.name)}")
    elif command == "bootstrap":
        lab.require_created()
        Bootstrap(lab).all(build=not args.no_build)
        if args.snapshot:
            lab.snapshot(args.snapshot)
    elif command == "tailnet":
        vms = t.select(args.vms) if args.vms else None
        if args.action == "remove":
            lab.forget_tailnet(vms)
        else:
            for device in lab.tailnet_devices(vms):
                print(f"{device.label:<32} {'online' if device.online else 'offline':<8} {', '.join(sorted(device.tags))}")
    elif command == "scenario":
        if lab.exists():
            lab.require_created()
        return scenario.run(lab, args.name)
    return 0
