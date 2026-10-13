# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M7.3's storage and boot-drill phases in the lab: the Deployer installs
mox3 (as in m6) with LUKS2 chosen and its two SATA disks as a data mirror;
then the host setup job, started from the Dashboard API as root@pam in
setup mode, encrypts the NVMe boot mirror in place, adds the encrypted SATA
mirror, makes the block-size storages, and runs every boot drill. The LUKS
passphrase is made up for the run, typed only at the serial console (with
`bmac luks-enter`, then at each boot), and never printed.

The private network phase then bridges the VLAN NIC (the lab's VLAN NIC
reports 10 Gb/s); mox3 creates a one-host cluster, gets guest egress, the
runtime, and its HAProxy LXC, backs up its LUKS headers, passes the final
verification, and leaves setup mode. mox3 is put back afterwards.

The lab's installed host has no serial console of its own (a real host is
unlocked at its VGA console, and a serial console would take the prompt
from it), so the scenario adds one over SSH before the job starts.

The three boot drills take most of the run's time, so the scenario declines
them unless BMAC_LAB_DRILLS=1 is set:

    BMAC_LAB_DRILLS=1 lab/lab scenario m7_storage"""

from __future__ import annotations

import contextlib
import os
import secrets as tokens
import socket
import subprocess
import threading
import time
import uuid
from pathlib import Path

from bmaclab.api import Api, free_port
from bmaclab.console import Console, ConsoleTimeout
from bmaclab.lab import BOOT_TIMEOUT
from bmaclab.scenario import load

DESCRIPTION = "host setup encrypts mox3's boot mirror in place, adds an encrypted SATA mirror and the block-size storages, and passes the boot drills"
JOB_TIMEOUT = 5400
TERMINAL = {"COMPLETED", "FAILED", "CANCELLED"}
UNLOCK_PROMPT = r"Please unlock disk|Enter passphrase|Caching passphrase for \S+: "
DRILLS = os.environ.get("BMAC_LAB_DRILLS") == "1"

m6 = load(Path(__file__).with_name("m6.py"))


def sata_pair(form: dict) -> list[str]:
    return m6.choice(form, "data", lambda c: c["label"].startswith("ata"))[:2]


LAYOUT = {"data_kind": "mirror", "data": sata_pair, "encryption": "luks2"}

SERIAL_CONSOLE = r"""
set -e
if [ -f /etc/kernel/cmdline ] && ! grep -q console=ttyS0 /etc/kernel/cmdline; then
  sed -i 's/$/ console=tty0 console=ttyS0,115200/' /etc/kernel/cmdline
fi
if [ -f /etc/default/grub ] && ! grep -q console=ttyS0 /etc/default/grub; then
  sed -i 's/^GRUB_CMDLINE_LINUX="\(.*\)"/GRUB_CMDLINE_LINUX="\1 console=tty0 console=ttyS0,115200"/' /etc/default/grub
fi
proxmox-boot-tool refresh >/dev/null
"""


class Tunnel:
    """`ssh -L` over the tailnet to bmacd's tailnet address."""

    def __init__(self, host, address: str) -> None:
        self.host, self.address, self.local = host, address, free_port()
        self.process: subprocess.Popen | None = None

    def __enter__(self) -> Tunnel:
        forward = ["-N", "-o", "ExitOnForwardFailure=yes", "-L", f"127.0.0.1:{self.local}:{self.address}:8443"]
        self.process = subprocess.Popen(self.host.ssh_args(forward), stdin=subprocess.DEVNULL,
                                        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        deadline = time.monotonic() + 30
        while True:
            if self.process.poll() is not None:
                raise RuntimeError(f"the tunnel failed: {self.process.stderr.read().decode().strip()}")
            try:
                socket.create_connection(("127.0.0.1", self.local), timeout=1).close()
                return self
            except OSError:
                if time.monotonic() > deadline:
                    raise RuntimeError("the tunnel did not open") from None
                time.sleep(0.2)

    def __exit__(self, *exc) -> None:
        if self.process:
            self.process.terminate()
            self.process.wait(10)


def boot_id(host) -> str:
    return host.on_host("cat /proc/sys/kernel/random/boot_id")


def wait_back(host, before: str | None = None, timeout: float = BOOT_TIMEOUT) -> str:
    """The host's boot ID once it answers, or once it differs from `before`."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            now = boot_id(host)
            if before is None or now != before:
                return now
        except (RuntimeError, subprocess.TimeoutExpired):
            pass
        time.sleep(5)
    raise TimeoutError("the host didn't come back" + (" with a new boot ID" if before else ""))


def console_login(lab, vm, password: str) -> Console:
    console = Console(lab.paths.console(vm.name))
    console.line()
    if console.expect(r"login: |root@\S+:~# ", timeout=120).group(0) == "login: ":
        console.line("root")
        console.expect(r"Password: ", timeout=30)
        console.line(password)
        console.expect(r"root@\S+:~# ", timeout=60)
    return console


def console_check(lab, vm, password: str) -> bool:
    with console_login(lab, vm, password) as console:
        console.line("exit")
    return True


def then(host) -> None:
    s, lab, vm = host.s, host.lab, host.vm
    passphrase = tokens.token_urlsafe(24)

    before = boot_id(host)
    host.on_host(SERIAL_CONSOLE)
    host.on_host("systemd-run --on-active=2s --collect /usr/bin/systemctl reboot")
    s.require("mox3 restarts with a serial console", lambda: wait_back(host, before))
    s.require("the serial console logs in", lambda: console_check(lab, vm, host.password))

    address = host.on_host("tailscale ip -4 | head -n 1")
    fingerprint = host.on_host(
        "openssl x509 -in /etc/bmac/tls/https.pem -noout -fingerprint -sha256").split("=", 1)[1].replace(":", "").lower()
    token: list[str | None] = [None]
    prompts: list[str] = []
    unlock_failures: list[str] = []
    boots: list[str] = [boot_id(host)]

    @contextlib.contextmanager
    def connected():
        with Tunnel(host, address) as tunnel:
            api = Api(tunnel.local, fingerprint, vm.name)
            api.token = token[0]
            yield api

    def login(api) -> bool:
        # bmacd may still be starting after the restart.
        deadline = time.monotonic() + 180
        while True:
            try:
                return api.login("root@pam", host.password)["role"] == "admin"
            except OSError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(3)

    with connected() as api:
        s.require("root@pam logs in in setup mode", lambda: login(api))
        token[0] = api.token
        view = api.request("GET", "/setup")
        s.check("the host is in setup mode", view.get("active") is True and view.get("operation") == "hosts.setup")
        started = api.request("POST", "/jobs", {"operation": "hosts.setup", "parameters": {},
                                                "idempotency_key": str(uuid.uuid4()), "dry_run": False})
    job = started["id"]
    s.note(f"the setup job is {job}")

    def enter_passphrase() -> None:
        with console_login(lab, vm, host.password) as console:
            console.line("bmac luks-enter")
            console.expect(r"LUKS passphrase: ", timeout=30)
            console.line(passphrase)
            console.expect(r"Again: ", timeout=30)
            console.line(passphrase)
            console.expect(r"Stored for the setup job", timeout=30)
            console.line("exit")

    def unlock_at_boot() -> None:
        # The lab starts a new QEMU when the guest restarts, so the console
        # closes once on the way down and is reopened for the new boot.
        deadline = time.monotonic() + BOOT_TIMEOUT
        try:
            while True:
                try:
                    with Console(lab.paths.console(vm.name)) as console:
                        prompt = console.expect(UNLOCK_PROMPT, timeout=max(1.0, deadline - time.monotonic()))
                        prompts.append(prompt.group(0))
                        # askpass can discard what arrives before it turns
                        # echo off, so a passphrase typed too soon is lost
                        # without a word; typing it again is what an
                        # operator would do.
                        for _ in range(3):
                            time.sleep(1)
                            console.line(passphrase)
                            try:
                                answer = console.expect(r"set up successfully|No key available", timeout=30)
                            except ConsoleTimeout:
                                continue
                            if answer.group(0) != "set up successfully":
                                raise AssertionError("the passphrase was refused at boot")
                            return
                        raise AssertionError("the boot never took the passphrase")
                except OSError:
                    if time.monotonic() > deadline:
                        raise
                    time.sleep(0.2)
        except Exception as e:
            unlock_failures.append(f"{type(e).__name__}: {e}")

    events: list[dict] = []
    answered: set[str] = set()
    watchers: list[threading.Thread] = []
    summary: dict = {}
    deadline = time.monotonic() + JOB_TIMEOUT
    while time.monotonic() < deadline:
        try:
            with connected() as api:
                while time.monotonic() < deadline:
                    after = events[-1]["seq"] + 1 if events else 1
                    view = api.request("GET", f"/jobs/{job}?from_seq={after}")
                    events.extend(view["events"])
                    summary = view["job"]
                    for e in view["events"]:
                        if e["type"] == "phase" and e.get("status") == "running":
                            s.note(f"phase: {e.get('label')}")
                        elif e["type"] in ("warning", "error"):
                            s.note(f"{e['type']}: {e.get('message')}")
                        elif e["type"] == "output":
                            s.note(f"output: {e.get('text')}")
                    if summary["state"] in TERMINAL:
                        break
                    request = summary.get("pending_request")
                    if request and request not in answered:
                        form = next(e for e in reversed(events) if e["type"] == "request" and e["request_id"] == request)
                        title = form.get("title", "")
                        s.note(f"request: {title}")
                        if form["kind"] == "confirm":
                            reply = {"kind": "confirm", "confirmed": DRILLS or title != "Run the boot drills?"}
                        elif form["kind"] == "manual_action" and title == "Type the LUKS passphrase at the console":
                            enter_passphrase()
                            reply = {"kind": "acknowledge"}
                        elif form["kind"] == "manual_action" and title.endswith("before it restarts"):
                            watcher = threading.Thread(target=unlock_at_boot, daemon=True)
                            watcher.start()
                            watchers.append(watcher)
                            time.sleep(1)
                            reply = {"kind": "acknowledge"}
                        else:
                            raise AssertionError(f"unexpected request {form['kind']} {title!r}")
                        api.request("POST", f"/jobs/{job}/requests/{request}", reply)
                        answered.add(request)
                    time.sleep(1)
            if summary.get("state") in TERMINAL:
                break
        except (AssertionError, ConsoleTimeout):
            raise
        except Exception as e:  # the host is restarting, or its network changing
            s.note(f"lost the Dashboard ({type(e).__name__}); waiting for mox3")
            try:
                now = wait_back(host)
            except TimeoutError:
                break
            if now != boots[-1]:
                boots.append(now)
                s.note("mox3 is back")
            else:
                time.sleep(5)
    for watcher in watchers:
        watcher.join(timeout=5)

    phases = {}
    for e in events:
        if e["type"] == "phase":
            phases[e["id"]] = e.get("status")
    message = summary.get("message") or ""
    s.check(f"the setup job completes ({summary.get('state')}: {message})", summary.get("state") == "COMPLETED")
    for phase in ("identity", "settings", "storage", "drills", "network", "cluster", "retire",
                  "orchestration", "headers", "verify"):
        s.check(f"the {phase} phase is complete", phases.get(phase) == "complete")
    if DRILLS:
        s.check(f"three boots were unlocked at the console ({len(prompts)}; {'; '.join(unlock_failures) or 'no failures'})",
                len(prompts) == 3)
        s.check(f"each drill boot changed the boot ID ({len(set(boots))} boots)", len(set(boots)) >= 4)
    else:
        s.note("the boot drills were declined; BMAC_LAB_DRILLS=1 runs them")
        s.check("the boot drills were skipped", any(
            e["type"] == "output" and e.get("text") == "The boot drills were skipped." for e in events))

    leaves = host.on_host("zpool status -P rpool | awk '$1 ~ /^\\/dev\\// {print $1, $2}'").splitlines()
    s.check(f"rpool's four members are its LUKS mappers ({len(leaves)})",
            len(leaves) == 4 and all(l.startswith("/dev/mapper/bmac-rpool-") and l.endswith(" ONLINE") for l in leaves),
            "\n".join(leaves))
    s.check("rpool is healthy", lambda: "is healthy" in host.on_host("zpool status -x rpool"))
    s.check("every member is listed once in the single-prompt form", lambda: host.on_host(
        "grep -c '^bmac-rpool-.*bmac-rpool.*keyscript=decrypt_keyctl' /etc/crypttab") == "4")
    s.check("the passphrase file is gone", lambda: host.on_host(
        "test ! -e /run/bmac/luks-passphrase && echo gone") == "gone")
    s.check("each member's LUKS header is backed up", lambda: host.on_host(
        "ls /var/lib/bmac/luks-headers/*.luks-header | wc -l") == "4")
    s.check("the block-size datasets exist", lambda: host.on_host(
        "zfs list -H -o name rpool/data-32k rpool/data-64k rpool/data-128k | wc -l") == "3")
    s.check("the block-size storages are defined", lambda: all(
        name in host.on_host("pvesm status") for name in ("bmac-zfs-32k", "bmac-zfs-64k", "bmac-zfs-128k")))
    s.check("both ESPs are enabled again", lambda: host.on_host("proxmox-boot-tool status | grep -c 'is configured'") == "2")

    bridged = host.on_host("ip -4 -o address show dev vmbr-private | awk '{print $4}'").split()
    s.check(f"vmbr-private has the private and replication addresses ({' '.join(bridged)})",
            f"{vm.vlan_ip}/24" in bridged and len(bridged) == 2)
    s.check("the private NIC is vmbr-private's only port", lambda: host.on_host(
        "for p in /sys/class/net/vmbr-private/brif/*; do cat /sys/class/net/${p##*/}/address; done")
        == vm.nic("vlan").mac.lower())
    s.check("the bridge is in the interfaces files", lambda: "auto vmbr-private" in host.on_host(
        "cat /etc/network/interfaces.d/bmac-private.cfg"))

    s.check("mox3 is a quorate one-host cluster", lambda: "Quorate:          Yes" in host.on_host("pvecm status"))
    s.check("corosync's first link is the private address", lambda: f"ring0_addr: {vm.vlan_ip}" in host.on_host(
        "cat /etc/pve/corosync.conf"))
    s.check("keepalived owns the guest-egress VIP", lambda: host.on_host(
        "systemctl is-active keepalived.service bmac-guest-egress.service | sort -u") == "active")
    s.check("the HAProxy LXC runs", lambda: "running" in host.on_host("pct status 9113"))
    s.check("the HAProxy ingress and route timer are active", lambda: host.on_host(
        "systemctl is-active bmac-haproxy-ingress.service bmac-haproxy-route-sync.timer | sort -u") == "active")
    s.check("setup is marked complete", lambda: host.on_host(
        "test -e /var/lib/bmac/setup-complete && echo done") == "done")
    with connected() as api:
        s.check("the host has left setup mode", lambda: api.request("GET", "/setup").get("active") is False)


def run(s) -> None:
    m6.install(s, LAYOUT, then)
