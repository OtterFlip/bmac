# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M6.3 in the lab: a host given an old raw `rpool` member on a spare disk
stops at the `(initramfs)` prompt; the Deployer's discovery ISO boots it
with the public NIC's carrier held down at first, joins the tailnet, reads
the inventory over pinned SSH, refuses a mismatched clear, clears the
label, and powers off; the host then boots again.

The Deployer side is `examples/lab_discovery.rs`, which runs the
Deployer's own discovery code. The discovery key comes from
`lab/secrets.env` and reaches it on stdin only."""

from __future__ import annotations

import contextlib
import json
import os
import shutil
import subprocess
import time
from pathlib import Path

from bmaclab import tailnet
from bmaclab.bootstrap import V3
from bmaclab.console import Console
from bmaclab.lab import BOOT_TIMEOUT
from bmaclab.provision import fetch

DESCRIPTION = "a stale rpool label stops the boot; discovery with no network settings finds the slow uplink by DHCP, reads the inventory, clears it, and the host boots"
DISCOVERY_KEY = "TAILSCALE_LAB_AUTH_KEY_DISCOVERY"
STALE_POOL = "bmaclabstale"
CARRIER_HELD = 12
STATIC_TARGET = "x86_64-unknown-linux-gnu"


def cargo_executable(args: list[str], env: dict[str, str] | None = None) -> Path:
    """Builds and returns the one executable cargo reports."""
    result = subprocess.run(
        ["cargo", "build", "-q", "--message-format=json-render-diagnostics", *args],
        cwd=V3, env=env, capture_output=True, text=True, timeout=1800,
    )
    if result.returncode != 0:
        raise RuntimeError(f"cargo build {' '.join(args)} failed: {result.stderr.strip()[-2000:]}")
    found = [m["executable"] for m in map(json.loads, result.stdout.splitlines()) if m.get("executable")]
    if len(found) != 1:
        raise RuntimeError(f"cargo build {' '.join(args)} reported {found}")
    return Path(found[0])


class Driver:
    def __init__(self, binary: Path, session: Path) -> None:
        self.binary = binary
        self.session = session

    def run(self, action: str, *args: str, stdin: str = "", timeout: float = 300) -> dict | None:
        result = subprocess.run(
            [str(self.binary), action, "--session", str(self.session), *args],
            input=stdin, capture_output=True, text=True, timeout=timeout,
        )
        if result.returncode != 0:
            raise RuntimeError(f"lab_discovery {action}: {result.stderr.strip()[-2000:]}")
        return json.loads(result.stdout) if result.stdout.strip() else None


def ssh(session: Path, name: str, address: str, command: str) -> str:
    """A shell command on the live system over the session's pinned SSH."""
    result = subprocess.run(
        ["ssh", "-F", "/dev/null", "-i", str(session / "client_key"), "-o", "IdentitiesOnly=yes",
         "-o", f"UserKnownHostsFile={session / 'known_hosts'}", "-o", "GlobalKnownHostsFile=none",
         "-o", "StrictHostKeyChecking=yes", "-o", f"HostKeyAlias={name}", "-o", "HostKeyAlgorithms=ssh-ed25519",
         "-o", "CheckHostIP=no", "-o", "BatchMode=yes", "-o", "ConnectTimeout=20", f"root@{address}", command],
        capture_output=True, text=True, timeout=120,
    )
    if result.returncode != 0:
        raise RuntimeError(f"{command}: {result.stderr.strip()}")
    return result.stdout


def labels(inventory: dict, serial: str) -> list[dict]:
    return next((d["zfs_labels"] for d in inventory["disks"] if d.get("serial") == serial), [])


def run(s) -> None:
    lab = s.lab
    vm = lab.topology.vm("mox3")
    s.require(f"{vm.name} answers SSH", lambda: lab.wait_ssh(vm, BOOT_TIMEOUT) or True)
    secrets = tailnet.read_secrets(lab.secrets)
    s.require(f"lab/secrets.env has {DISCOVERY_KEY}", lambda: secrets.get(DISCOVERY_KEY))
    s.require("the API key and this workstation are on the lab's Tailscale account", lambda: lab.tailscale() and True)
    work = lab.paths.images.parent / "discovery"
    session = work / "session"
    work.mkdir(parents=True, exist_ok=True)

    s.note("building the lab driver and a static bmac-inventory")
    # Its own target dir: a workspace build would replace it without the lab feature.
    driver_bin = cargo_executable(["-p", "bmac-deployer", "--features", "lab", "--example", "lab_discovery",
                                   "--target-dir", str(work / "driver-target")])
    static_env = dict(os.environ, RUSTFLAGS="-C target-feature=+crt-static")
    inventory_bin = cargo_executable(
        ["--release", "-p", "bmac-inventory", "--target", STATIC_TARGET, "--target-dir", str(work / "static-target")],
        static_env,
    )
    s.require("bmac-inventory is statically linked", lambda: "static" in subprocess.run(
        ["file", "-b", str(inventory_bin)], capture_output=True, text=True).stdout.split(",")[0:4][-1])
    driver = Driver(driver_bin, session)
    pins = driver.run("pins")
    alpine = fetch(pins["alpine"]["url"], pins["alpine"]["sha256"], lab.paths.images, s.note)
    tailscale_tgz = fetch(pins["tailscale"]["url"], pins["tailscale"]["sha256"], lab.paths.images, s.note)

    uplink = vm.nic("uplink")
    iso = work / "discovery.iso"
    built = driver.run(
        "build", "--source", str(alpine), "--tailscale", str(tailscale_tgz), "--inventory", str(inventory_bin),
        "--out", str(iso), stdin=secrets[DISCOVERY_KEY] + "\n", timeout=600,
    )
    name = built["name"]
    s.check(f"the discovery ISO is built for {name}", iso.exists() and name.startswith(lab.topology.tailnet_prefix))

    # A raw member of an earlier rpool on a spare disk, as a reused server has.
    spare = next((d for d in vm.disks if d.bus != "nvme" and d.wwn
                  and lab.ssh(vm, f"lsblk -rno NAME /dev/disk/by-id/wwn-{d.wwn} | wc -l").stdout.strip() == "1"), None)
    s.require(f"{vm.name} has an unused SATA or SCSI disk", spare is not None)
    device = f"/dev/disk/by-id/wwn-{spare.wwn}"
    guid = lab.ssh(vm, f"zpool create -f -m none -t {STALE_POOL} rpool {device} && "
                       f"zpool get -Hp -o value guid {STALE_POOL} && zpool export {STALE_POOL}").stdout.strip()
    s.require(f"{spare.serial} carries a label of another pool named rpool (GUID {guid})", guid.isdigit())

    with contextlib.ExitStack() as cleanup:
        cleanup.callback(lambda: lab.link(vm, "uplink", True))
        cleanup.callback(lambda: lab.forget_tailnet(names={name}, required=False))

        lab.power(vm, "reset")
        with Console(lab.paths.console(vm.name)) as console:
            s.check("the reboot stops at the (initramfs) prompt", lambda: console.expect(r"\(initramfs\)", timeout=BOOT_TIMEOUT))

        lab.stop([vm], force=True)
        # A Dell's one-time boot leaves its boot order alone; OVMF instead drops the installed
        # system's NVRAM entry, so its store is put back after discovery.
        nvram = work / f"{vm.name}-nvram.fd"
        shutil.copyfile(lab.paths.vars(vm.name), nvram)
        cleanup.callback(lambda: shutil.copyfile(nvram, lab.paths.vars(vm.name)))
        cleanup.callback(lambda: lab.stop([vm], force=True))
        lab.link(vm, "uplink", False)
        since_ms = int(time.time() * 1000) - 60_000
        lab.start([vm], media=iso)
        with Console(lab.paths.console(vm.name)) as console:
            nic = console.expect(r"bmac-discovery: trying (\S+)\r?\n", timeout=BOOT_TIMEOUT).group(1)
            s.note(f"the discovery OS is waiting for carrier on {nic}; raising it in {CARRIER_HELD}s")
            time.sleep(CARRIER_HELD)
            lab.link(vm, "uplink", True)
            up = console.expect(r"bmac-discovery: public network up on (\S+) \(", timeout=180).group(1)
            s.check(f"with no settings, DHCP brings up the first port once its carrier arrives ({up})", up == nic)
            s.check("the console says the live system is ready", lambda: console.expect(r"bmac-discovery: ready", timeout=300))

        peer = driver.run("wait", "--since-ms", str(since_ms), timeout=660)
        address = peer["ip"]
        s.check(f"the Deployer finds {name} on the tailnet with the discovery tag ({address})", address)

        inventory = driver.run("inventory", "--address", address)
        serials = {d.get("serial") for d in inventory["disks"]}
        s.check("the inventory lists every disk by serial", {d.serial for d in vm.disks if d.bus != "virtio"} <= serials)
        s.check("the inventory lists every NIC", {n.mac for n in vm.nics} <= {n["mac"] for n in inventory["nics"]})
        s.check(f"the port DHCP brought up is the uplink ({uplink.mac})",
                any(n["name"] == up and n["mac"] == uplink.mac for n in inventory["nics"]))
        s.check(f"{spare.serial}'s old rpool label is reported", any(
            l["pool"] == "rpool" and l["pool_guid"] == guid for l in labels(inventory, spare.serial)))

        addresses = ssh(session, name, address, "ip -4 -o addr show scope global | awk '$2 != \"tailscale0\" {print $2}'").split()
        s.check(f"only the public NIC has an address ({addresses})", addresses == [nic])
        record = dict(line.split("=", 1) for line in ssh(session, name, address,
                      "cat /run/bmac-discovery/public-network").splitlines() if "=" in line)
        s.check(f"the public network is recorded for the host's network form ({record})",
                record.get("MAC", "").lower() == uplink.mac.lower() and record.get("METHOD") == "dhcp"
                and record.get("ADDRESS", "").startswith("10.0.2.") and record.get("GATEWAY") == "10.0.2.2")
        backing = ssh(session, name, address,
                      "cat /sys/block/$(awk '$2 == \"/.modloop\" {print $1}' /proc/mounts | sed 's|/dev/||')/loop/backing_file").strip()
        s.check(f"the kernel modules run from RAM ({backing})", backing.startswith("/run/"))
        s.check("udev has the disks' identities", ssh(session, name, address,
                "ls /run/udev/data | grep -c '^b' || true").strip() not in ("", "0"))
        s.check("no swap is enabled", ssh(session, name, address, "swapon --noheadings").strip() == "")
        s.check("no disk is mounted", ssh(session, name, address,
                "awk '$1 ~ /^\\/dev\\/(sd|nvme|vd)/' /proc/mounts").strip() == "")

        target = {"serial": spare.serial, "wwn": spare.wwn, "pool_guid": guid}
        report = driver.run("clear", "--address", address, stdin=json.dumps({"disks": [dict(target, pool_guid="1")]}))
        s.check("a clear with the wrong pool GUID is refused", report["refused"] and not report["cleared"])
        again = driver.run("inventory", "--address", address)
        s.check("the refused clear left the label alone", labels(again, spare.serial) == labels(inventory, spare.serial))

        report = driver.run("clear", "--address", address, stdin=json.dumps({"disks": [target]}))
        s.check(f"the label is cleared ({report['cleared']})", report["cleared"] and not report["survived"] and not report["errors"])
        after = driver.run("inventory", "--address", address)
        s.check(f"{spare.serial} carries no label now", labels(after, spare.serial) == [])
        s.check("the other disks' labels are unchanged", all(
            labels(after, d["serial"]) == d["zfs_labels"] for d in inventory["disks"] if d.get("serial") != spare.serial))

        driver.run("poweroff", "--address", address)
        s.check("Power off stops the machine", lambda: lab.wait_exit(vm, 180))

    lab.start([vm])
    s.check(f"{vm.name} boots from its disks again", lambda: lab.wait_ssh(vm, BOOT_TIMEOUT) or True)
    s.check("no other pool is left to import", lambda: "no pools available" in lab.ssh(vm, "zpool import 2>&1 || true").stdout)
