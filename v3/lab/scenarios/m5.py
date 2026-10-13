# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M5's done-when check (IMPLEMENTATION_PLAN.md): pulling a virtual disk,
filling a pool, stopping corosync on a host, and powering a host off each
raise their condition and alert, an email reaches Postmark's sandbox, and
the recovery message follows. It also checks the journal collector and log
catch-up on real hosts.

Email goes only to Postmark's sandbox server, and only for critical `zfs`
and `host` alerts: alerts already open when the scenario starts are
silenced first, because the lab server allows 100 emails a month."""

from __future__ import annotations

import contextlib
import json
import time
import urllib.request

from bmaclab import tailnet
from bmaclab.api import Api, Codes, Tunnel, pinned_fingerprint
from bmaclab.bootstrap import Bootstrap
from bmaclab.lab import BOOT_TIMEOUT

DESCRIPTION = "a pulled disk, a full pool, stopped corosync, and a powered-off host each alert by email, then recover"
RECIPIENT = "test@blackhole.postmarkapp.com"
POSTMARK_KEY = "POSTMARKAPP_LAB_API_KEY"
POSTMARK_SENDER = "POSTMARKAPP_LAB_API_SENDER_SIGNATURE"
MIRROR, FILL = "labmirror", "labfill"
FILL_IMAGE = "/var/tmp/bmaclab-fill.img"
FAULT_TIMEOUT = 300


def wait_for(condition, timeout: float):
    deadline = time.monotonic() + timeout
    while True:
        try:
            value = condition()
        except Exception:
            value = None
        if value:
            return value
        if time.monotonic() > deadline:
            return None
        time.sleep(3)


def now_ms() -> int:
    return int(time.time() * 1000)


def postmark_total(token: str) -> int:
    """How many outbound messages the Postmark server has recorded."""
    request = urllib.request.Request(
        "https://api.postmarkapp.com/messages/outbound?count=1&offset=0",
        headers={"Accept": "application/json", "X-Postmark-Server-Token": token},
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        return int(json.load(response)["TotalCount"])


class Alerts:
    def __init__(self, api: Api) -> None:
        self.api = api

    def view(self) -> dict:
        return self.api.request("GET", "/alerts")

    def matching(self, condition: str, resource: str) -> list[dict]:
        view = self.view()
        return [a for a in view["alerts"] + view["history"] if a["condition"] == condition and a["resource"] == resource]

    def generations(self, condition: str, resource: str) -> dict[str, tuple[int, str]]:
        return {a["id"]: (a["generation"], a["state"]) for a in self.matching(condition, resource)}

    def opened_since(self, condition: str, resource: str, before: dict[str, tuple[int, str]]) -> dict | None:
        """An alert that is new, or that reopened, since `before` was read:
        a return within ten minutes reopens the same alert, keeping its
        `opened_ms` and counting a generation."""
        for alert in self.matching(condition, resource):
            if alert["state"] == "resolved":
                continue
            generation, state = before.get(alert["id"], (0, "resolved"))
            if alert["generation"] > generation or state == "resolved":
                return alert
        return None

    def get(self, alert: dict) -> dict | None:
        for found in self.matching(alert["condition"], alert["resource"]):
            if found["id"] == alert["id"]:
                return found
        return None

    def sent_since(self, when_ms: int) -> bool:
        outbox = self.view()["outbox"]
        return outbox["pending"] == 0 and (outbox["last_sent_ms"] or 0) >= when_ms


def fault(s, alerts: Alerts, title: str, condition: str, resource: str, inject, repair) -> None:
    """Injects a fault, waits for its alert and opening email, repairs it,
    and waits for the resolution and recovery email."""
    before = alerts.generations(condition, resource)
    started = time.monotonic()
    started_ms = now_ms() - 5_000
    s.note(f"{title}: injecting")
    inject()
    alert = wait_for(lambda: alerts.opened_since(condition, resource, before), FAULT_TIMEOUT)
    s.check(f"{title}: alert {condition} on {resource} opens", alert is not None)
    if alert:
        s.note(f"open {time.monotonic() - started:.0f}s after the fault (generation {alert['generation']}): {alert['message']}")
        notified = wait_for(lambda: ((alerts.get(alert) or {}).get("notified_ms") or 0) >= started_ms and alerts.get(alert)["notified_ms"], 120)
        s.check(f"{title}: the opening email is queued", notified)
        if notified:
            s.check(f"{title}: the opening email is sent", lambda: wait_for(lambda: alerts.sent_since(notified), 120))
    s.note(f"{title}: repairing")
    repairing_ms = now_ms() - 5_000
    repair()
    if alert:
        resolved = wait_for(lambda: ((alerts.get(alert) or {}).get("resolved_ms") or 0) >= repairing_ms and alerts.get(alert)["resolved_ms"], FAULT_TIMEOUT)
        s.check(f"{title}: the alert resolves after the repair", resolved)
        if resolved:
            s.check(f"{title}: the recovery email is sent", lambda: wait_for(lambda: alerts.sent_since(resolved), 120))


def block_devices(lab, vm) -> dict[str, str]:
    """QEMU drive id -> the QOM path of the device using it."""
    with lab.qmp(vm) as monitor:
        return {b["device"]: b.get("qdev", "") for b in monitor.command("query-block") if b.get("inserted")}


def pull_disk(lab, vm, disk) -> None:
    drive = f"d-{disk.id}"
    path = block_devices(lab, vm)[drive]
    with lab.qmp(vm) as monitor:
        monitor.command("device_del", id=path)
    if not wait_for(lambda: drive not in block_devices(lab, vm), 60):
        raise RuntimeError(f"{vm.name}: {drive} was not unplugged")


def insert_disk(lab, vm, disk) -> None:
    drive = f"d-{disk.id}"
    with lab.qmp(vm) as monitor:
        monitor.command("human-monitor-command", **{"command-line": f"drive_add 0 if=none,id={drive},format=qcow2,file={lab.paths.disk(disk)},discard=unmap"})
        monitor.command("device_add", driver="scsi-hd", drive=drive, bus="scsi0.0", serial=disk.serial, wwn=int(disk.wwn, 16))


def run(s) -> None:
    lab = s.lab
    hosts = lab.topology.hosts
    s.require("the lab has three hosts", len(hosts) >= 3)
    duty, storage_host, victim = hosts[0], hosts[1], hosts[2]
    for vm in hosts:
        s.require(f"{vm.name} answers SSH", lambda vm=vm: lab.wait_ssh(vm, BOOT_TIMEOUT) or True)
    users_file = Bootstrap(lab).users_file
    s.require("the lab was bootstrapped (lab/lab bootstrap)", users_file.exists)
    users = json.loads(users_file.read_text())
    codes = Codes(users["alice_totp_secret"])
    password = users["users"]["alice@pve"]["password"]
    secrets = tailnet.read_secrets(lab.secrets)
    s.require(f"lab/secrets.env has {POSTMARK_KEY} and {POSTMARK_SENDER}", lambda: secrets.get(POSTMARK_KEY) and secrets.get(POSTMARK_SENDER))
    token = secrets[POSTMARK_KEY]

    scsi = [d for d in storage_host.disks if d.bus == "scsi"]
    s.require(f"{storage_host.name} has two SCSI disks for a test pool", len(scsi) >= 2)
    by_id = [f"/dev/disk/by-id/wwn-{d.wwn}" for d in scsi[:2]]

    with contextlib.ExitStack() as stack:
        tunnel = stack.enter_context(Tunnel(lab, duty))
        api = Api(tunnel.local, pinned_fingerprint(lab, duty), duty.name)
        s.require(f"alice logs in on {duty.name}", lambda: api.login("alice@pve", password, codes)["role"] == "admin")
        stack.callback(api.logout)
        alerts = Alerts(api)

        # Email only this scenario's faults.
        view = alerts.view()
        silences = []
        for alert in view["alerts"]:
            if alert["state"] != "resolved":
                silence = api.request("POST", "/silences", {
                    "condition": alert["condition"], "resource": alert["resource"], "host": None,
                    "until_ms": now_ms() + 86_400_000, "reason": "lab m5: open before the scenario",
                })
                silences.append(silence.get("id"))
        s.note(f"silenced {len(silences)} alert(s) already open")
        s.check("the Postmark server token is stored", lambda: api.request("PUT", "/notify/token", {"token": token}) is not None)
        stack.callback(lambda: api.request("PUT", "/notify/token", {"token": ""}))
        templates = api.request("GET", "/notify/templates")
        s.note(f"Postmark server {templates['server_name']} ({templates['delivery_type']})")
        if any(t["presence"] != "present" for t in templates["templates"]):
            templates = api.request("POST", "/notify/templates", {})
        s.require("the email templates exist on the Postmark server", all(t["presence"] == "present" for t in templates["templates"]))
        settings = {
            "sender": secrets[POSTMARK_SENDER],
            "recipients": [RECIPIENT],
            "routes": [{"categories": ["zfs", "host"], "min_severity": "critical", "recipients": [], "recovery": True}],
            "throttle_minutes": 1,
            "digest_hour": None,
            "dashboard_url": f"https://{duty.name}:8443/",
        }
        s.require("the notification settings are saved", lambda: api.request(
            "PUT", "/notify/settings", {"settings": settings, "expected_revision": alerts.view()["settings_revision"]}))
        before = postmark_total(token)

        # A pulled disk degrades a mirror.
        def make_mirror():
            lab.ssh(storage_host, f"zpool destroy -f {MIRROR} 2>/dev/null; for d in {' '.join(by_id)}; do wipefs -aq $d; done; "
                    f"zpool create -f {MIRROR} mirror {' '.join(by_id)}").check_returncode()
        s.require(f"{storage_host.name}: a mirror pool on two SCSI disks", lambda: make_mirror() or True)
        stack.callback(lambda: lab.ssh(storage_host, f"zpool destroy -f {MIRROR}"))
        s.note("waiting for the storage collector to see the healthy pool")
        time.sleep(70)

        def pull():
            pull_disk(lab, storage_host, scsi[1])
            lab.ssh(storage_host, f"dd if=/dev/urandom of=/{MIRROR}/x bs=1M count=8 status=none; zpool sync {MIRROR}")

        def reinsert():
            insert_disk(lab, storage_host, scsi[1])
            wait_for(lambda: lab.ssh(storage_host, f"test -e {by_id[1]}").returncode == 0, 60)
            lab.ssh(storage_host, f"zpool online {MIRROR} {by_id[1]}; zpool clear {MIRROR}")
            wait_for(lambda: lab.ssh(storage_host, f"zpool list -Ho health {MIRROR}").stdout.strip() == "ONLINE", 120)

        fault(s, alerts, "pulled disk", "zfs.pool.health", f"{storage_host.name}/{MIRROR}", pull, reinsert)

        # A full pool. ZFS keeps at least 128 MiB of slop that `zpool list`
        # counts as free, so the pool is big enough for that to be a few
        # percent, as on a real pool, rather than a warning-level 13%.
        def fill():
            lab.ssh(storage_host, f"zpool destroy -f {FILL} 2>/dev/null; rm -f {FILL_IMAGE}; truncate -s 4G {FILL_IMAGE} && "
                    f"zpool create -f -O compression=off {FILL} {FILL_IMAGE} && "
                    f"(dd if=/dev/zero of=/{FILL}/blob bs=1M status=none 2>/dev/null; zpool sync {FILL}; true)", timeout=300).check_returncode()

        def empty():
            lab.ssh(storage_host, f"rm -f /{FILL}/blob; zpool sync {FILL}")

        stack.callback(lambda: lab.ssh(storage_host, f"zpool destroy -f {FILL}; rm -f {FILL_IMAGE}"))
        fault(s, alerts, "full pool", "zfs.pool.capacity", f"{storage_host.name}/{FILL}", fill, empty)

        # Stopping corosync drops the host from the cluster.
        corosync_ms = now_ms()
        fault(s, alerts, "stopped corosync", "host.offline", victim.name,
              lambda: lab.ssh(victim, "systemctl stop corosync").check_returncode(),
              lambda: lab.ssh(victim, "systemctl start corosync").check_returncode())

        def journal(on: Api, host: str) -> list[dict]:
            return on.request("POST", "/logs/search", {"host": host, "kinds": ["journal"], "from_ms": corosync_ms - 60_000, "limit": 50})["records"]

        records = wait_for(lambda: journal(api, victim.name), 180)
        s.check(f"the journal collector recorded {victim.name}'s warnings, readable from {duty.name}", records)
        for record in (records or [])[:5]:
            s.note(f"{record['unit'] or 'kernel'}: {record['summary']}")

        # A powered-off host; its records catch up when it returns.
        def power_on():
            lab.power(victim, "on")
            lab.wait_ssh(victim, BOOT_TIMEOUT)
            wait_for(lambda: lab.ssh(victim, "systemctl is-active bmacd bmacd-web").returncode == 0, 180)

        s.note("waiting out the one-minute throttle: host.offline reopens the same alert")
        time.sleep(65)
        fault(s, alerts, "powered-off host", "host.offline", victim.name, lambda: lab.power(victim, "off"), power_on)

        def caught_up():
            view = api.request("GET", "/logs/replication")
            peer = next((p for p in view["peers"] if p["peer"] == victim.name), None)
            if not peer or peer["error"] or not peer["origins"]:
                return None
            return all(o["gaps"] == 0 and o["behind_records"] <= 5 for o in peer["origins"]) and peer

        peer = wait_for(caught_up, 300)
        s.check(f"{victim.name} caught up on the records written while it was off", peer)
        if peer:
            for origin in peer["origins"]:
                s.note(f"{origin['origin']}: {victim.name} holds to seq {origin['last_seq']}, {origin['behind_records']} behind")

        sent = wait_for(lambda: postmark_total(token) - before >= 8 or None, 120)
        s.check("Postmark's sandbox recorded the eight emails", sent, detail=f"{postmark_total(token) - before} new")

        for silence in silences:
            if silence:
                with contextlib.suppress(Exception):
                    api.request("POST", f"/silences/{silence}/delete", {})
