# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M9.6's acceptance scenario in compressed time: `BACKUP.md` "Acceptance
scenarios", steps 1 to 16, and the security tests the lab can run. It
starts from the cluster with production VMs (m8_storage.staged): VM 1,
"small", placed on mox1 and mox3, is backed up to appliances A ("vault")
and B ("vaultb"); VM 2, "bulk", on mox2 and mox3, to B only.

Compressed time: the cluster's backup settings are short (a check-in every
10 s, LATE after 45 s, delinquent after 600 s), both policies run every 2
minutes, and a test day lasts 300 s on the cluster and on both appliances.
A LATE appliance's warning still holds for the alerts' 5 minutes.

1. Full seeds to A and B; B's seed of bulk is cut by killing its receive
   while its VLAN is throttled, and resumes (step 13).
2. Scheduled incrementals: each from the PIT before, the hosts release
   the snapshots of pulled PITs and keep no bookmark that isn't an
   appliance's head, and every transfer has its throughput (lab item 10).
3. A staging VM of small is created and removed while backups run.
4. A planned owner move to mox3, then mox3 powered off: HA restarts small
   on mox1; each next PIT is an incremental sent by the new owner (mox1's
   bmac-source is stopped for the first, since A tries mox1 first).
5. A stops: its LATE alert; B carries on.
6. A returns within the delinquency period and catches up incrementally
   from its head's bookmark.
7. A and B trust each other on their Trusted peers pages; A, back after
   the delinquency period with its head gone from the cluster, repairs
   from B. B untrusts A; A's next repair is refused, on B's Trusted peers
   page and in its audit log, and A reseeds from the cluster.
8. Neither appliance checks in: the slot is skipped, with no snapshot,
   and the critical alert.
9. A rollback to an older PIT starts a new branch, received on both
   appliances as a clone.
10. and 15. Restores from both branches, with small's captures paused: a
    new VM on mox1 from an incremental over a local older PIT (method 2),
    a restore test on mox2 (method 3), into the production VM from its
    local snapshot (method 1), with the estimates shown first; the next
    PIT continues from the restored one on both appliances.
11. A's retention pruned intermediate PITs.
12. A reboots, stays LOCKED, and is unlocked at its console.
14. A scrub of B finds nothing to repair.
16. bulk is removed and recreated from its newest PIT on B.

Then the security tests: the cluster's credentials get nothing from an
appliance, an untrusted peer gets nothing from B, a mistyped fingerprint
saves no peer, and no LUKS passphrase or grant is in any log.

Both appliances are put back as they were."""

from __future__ import annotations

import json
import re
import secrets
import subprocess
import time
from pathlib import Path

from bmaclab.api import ApiError
from bmaclab.scenario import load

DESCRIPTION = "The backup acceptance scenario in compressed time: two appliances, failovers, peers, restores, recreation"

restore = load(Path(__file__).with_name("m9_restore.py"))
ops = load(Path(__file__).with_name("m4_ops.py"))
backup = restore.backup
storage, m6, join = backup.storage, backup.m6, backup.join
m7 = storage.m7

TEST_DAY_S = 300
SETTINGS = {
    "checkin_interval_s": 10,
    "late_after_s": 45,
    "ready_window_s": 60,
    "claim_window_s": 90,
    "stalled_after_s": 180,
    "delinquency_s": 600,
    "streams_per_appliance": 1,
    "min_healthy_appliances": 1,
    "test_day_s": TEST_DAY_S,
}
EVERY = {"kind": "interval", "minutes": 2}
# Only the newest PIT stays on the cluster, so an absent appliance's head
# is soon a bookmark, then gone; the rollback and restores add three days
# back, enough to keep the rollback target until the restores.
NEWEST = {"newest": 1, "days": 0, "weeks": 0}
DAYS = {"newest": 1, "days": 3, "weeks": 0}
B_NAME = "vaultb"
THROTTLE = "40mbit"
# A warning becomes an alert after it has held for this long.
WARNING_HOLD_S = 300
STEP_TIMEOUT = 900
BUSY_TIMEOUT = 600
RECEIVING = re.compile(r"^bmac-backupd: receiving (\S+) \((.+)\) from (\S+)$")


def fingerprint(text: str) -> str:
    return text.replace(":", "").lower()


class Waiting:
    """The Dashboard, whose jobs start once a scheduled backup has let go
    of their VM: with a slot every two minutes, one often holds it."""

    def __init__(self, dashboard) -> None:
        self.inner = dashboard

    def __getattr__(self, name: str):
        return getattr(self.inner, name)

    def start(self, operation: str, parameters: dict) -> str:
        deadline = time.monotonic() + BUSY_TIMEOUT
        while True:
            try:
                return self.inner.start(operation, parameters)
            except ApiError as e:
                if e.status != 409 or "is in use by" not in str(e) or time.monotonic() > deadline:
                    raise
                time.sleep(5)


class Accept:
    def __init__(self, s, hosts) -> None:
        self.s, self.lab, self.hosts = s, s.lab, hosts
        self.a = backup.Backup(s, hosts)
        self.b = backup.Backup(s, hosts, vm="applianceb", name=B_NAME)
        # mox2 is never powered off here.
        self.dashboard = Waiting(join.Dashboard(hosts["mox2"]))
        for b in (self.a, self.b):
            b.dashboard = self.dashboard
            b.password = secrets.token_urlsafe(18)
        self.vms: dict[str, dict] = {}
        self.cluster_uuid = ""
        self.grants: list[str] = []

    # Plumbing

    def on(self, name: str, command: str, timeout: float = 300) -> str:
        return self.hosts[name].on_host(command, timeout=timeout)

    def wait(self, what: str, test, timeout: float = STEP_TIMEOUT) -> bool:
        return self.s.check(what, lambda: m6.wait_until(test, timeout))

    def view(self) -> dict:
        return self.dashboard.call("GET", "/backups")

    def vm_view(self, purpose: str) -> dict:
        return next((v for v in self.view()["vms"] if v["vm"] == self.vms[purpose]["name"]), {})

    def alerts(self) -> list[dict]:
        found = self.dashboard.call("GET", "/alerts")
        return found.get("alerts", []) if isinstance(found, dict) else found

    def alerted(self, condition: str, resource: str) -> bool:
        return any(a["condition"] == condition and a["resource"] == resource and a["state"] != "resolved"
                   for a in self.alerts())

    def pits(self, b, purpose: str) -> list[dict]:
        """The VM's PITs published on b, oldest first."""
        uuid = self.vms[purpose]["vm_uuid"]
        return sorted((p for p in b.pits() if p["vm_uuid"] == uuid), key=lambda p: p["published_at_ms"])

    def newest(self, b, purpose: str) -> dict:
        pits = self.pits(b, purpose)
        return pits[-1] if pits else {}

    def newer(self, b, purpose: str, than: dict, timeout: float = STEP_TIMEOUT) -> dict:
        """Waits for b to publish a PIT of the VM after `than`; the first one."""
        after = than.get("published_at_ms", 0)
        found: list[dict] = []

        def published() -> bool:
            found[:] = [p for p in self.pits(b, purpose) if p["published_at_ms"] > after]
            return bool(found)
        if not m6.wait_until(published, timeout):
            b.diagnose()
            return {}
        return found[0]

    def taken_after(self, b, purpose: str, at_s: float, timeout: float = STEP_TIMEOUT) -> tuple[dict, dict]:
        """Waits for b to publish a PIT of the VM taken after at_s; the first
        one, and the PIT b published before it."""
        found: list[tuple[dict, dict]] = []

        def published() -> bool:
            pits = self.pits(b, purpose)
            found[:] = [(p, pits[i - 1] if i else {}) for i, p in enumerate(pits) if p["taken_s"] > at_s][:1]
            return bool(found)
        if not m6.wait_until(published, timeout):
            b.diagnose()
            return {}, {}
        return found[0]

    def audit(self, b, event: str) -> list[dict]:
        text = b.app("cat /var/lib/bmac-backupd/metadata/audit.jsonl 2>/dev/null || true")
        entries = [json.loads(l) for l in text.splitlines() if l.strip()]
        return [e for e in entries if e.get("event") == event]

    def published_entry(self, b, pit: dict) -> dict:
        return next((e["details"] for e in reversed(self.audit(b, "publish"))
                     if e["details"].get("pit_uuid") == pit.get("pit_uuid")), {})

    def received(self, b) -> list[tuple[str, str, str]]:
        """(dataset, method, host) of every receive in b's service log."""
        out = []
        for line in b.app("journalctl -u bmac-backupd.service --no-pager -o cat").splitlines():
            if found := RECEIVING.match(line.strip()):
                out.append(found.groups())
        return out

    def ui(self, b, calls: list) -> list:
        b.app(f"install -m 0600 /dev/stdin {restore.UI_CALLS}", input=restore.UI_CALLS_SCRIPT.encode())
        try:
            text = b.app(f"python3 {restore.UI_CALLS}", timeout=180,
                         input=f"{b.password}\n{json.dumps(calls)}\n".encode())
        finally:
            b.app(f"rm -f {restore.UI_CALLS}")
        out = json.loads(text)
        if out.get("login") != 200:
            raise RuntimeError(f"{b.name}'s UI refused the login ({out.get('login')})")
        return out["answers"]

    def job(self, what: str, operation: str, parameters: dict, answers: dict | None = None) -> tuple[dict, list[str]]:
        """Runs a job to its end, answering each field from `answers` (a
        value, or a function of the field) or its default; its result and
        the details its confirmations showed."""
        answers = answers or {}
        details: list[str] = []

        def reply(form: dict) -> dict:
            if form["kind"] == "confirm":
                details.extend(form.get("details") or [])
                return {"kind": "confirm", "confirmed": True}
            if form["kind"] == "manual_action":
                raise AssertionError(f"unexpected manual step {form.get('title')!r}")
            values = {}
            for field in form.get("fields") or [form["field"]]:
                answer = answers.get(field["id"])
                if callable(answer):
                    answer = answer(field)
                if answer is None:
                    answer = field.get("default")
                if answer is None:
                    raise AssertionError(f"no answer for {field['id']} in {form.get('title')!r}")
                values[field["id"]] = answer
            return {"kind": "values", "values": values}

        summary, events = join.follow(self.s, self.dashboard, self.dashboard.start(operation, parameters), reply)
        if summary.get("state") != "COMPLETED":
            for e in [e for e in events if e["type"] == "output"][-8:]:
                self.s.note(f"output: {e.get('text')}")
        self.s.check(f"{what} ({summary.get('state')}: {summary.get('message') or ''})",
                     summary.get("state") == "COMPLETED")
        for d in details:
            self.s.note(f"confirm: {d}")
        result = next((e["data"] for e in reversed(events) if e["type"] == "result"), {})
        self.s.note(f"result: {json.dumps(result)}")
        return result, details

    def operate(self, operation: str, parameters: dict, what: str, **reply) -> bool:
        """ops.operate, again while a capture holds the VM's orchestration
        lease: the job then fails before it changes anything."""
        deadline = time.monotonic() + BUSY_TIMEOUT
        while True:
            summary, events = join.follow(self.s, self.dashboard, self.dashboard.start(operation, parameters),
                                          ops.answering(**reply))
            leased = "orchestration lease" in (summary.get("message") or "")
            if summary.get("state") == "COMPLETED" or not leased or time.monotonic() > deadline:
                break
            self.s.note(f"{operation}: a capture holds the lease; trying again")
            time.sleep(20)
        if summary.get("state") != "COMPLETED":
            for e in [e for e in events if e["type"] == "output"][-8:]:
                self.s.note(f"output: {e.get('text')}")
        return self.s.check(f"{operation} {what} ({summary.get('state')}: {summary.get('message') or ''})",
                            summary.get("state") == "COMPLETED")

    def policy(self, purpose: str, appliances: list, local: dict, enabled: bool = True) -> None:
        current = self.vm_view(purpose).get("policy")
        body = {"vm": self.vms[purpose]["name"], "every": EVERY, "appliances": [a.appliance_uuid for a in appliances],
                "local_retention": local, "enabled": enabled,
                "revision": current["revision"] if current else None}
        saved = self.dashboard.call("POST", "/backups/policies", body)
        self.s.check(f"{self.vms[purpose]['name']}'s policy: every {EVERY['minutes']} minutes to "
                     f"{', '.join(a.name for a in appliances)}, local {local}, "
                     f"{'enabled' if enabled else 'disabled'}",
                     saved.get("appliances") == body["appliances"] and saved.get("enabled") == enabled)

    def disks(self, host: str, vmid: int) -> list[str]:
        out = []
        for line in self.on(host, f"qm config {vmid}").splitlines():
            found = restore.DISK_LINE.match(line)
            if found and "media=cdrom" not in line and found.group(2) != "none":
                out.append(self.on(host, f"pvesm path {found.group(2)}").removeprefix("/dev/zvol/"))
        return out

    def owner(self, purpose: str) -> str:
        return ops.guests(self.hosts["mox2"])[self.vms[purpose]["name"]].get("node", "")

    def on_cluster(self, purpose: str, pit: dict) -> dict[str, str]:
        """Where the PIT is on each of the VM's placement hosts: "snapshot",
        "bookmark", or ""."""
        vmid, name = self.vms[purpose]["vmid"], pit["name"]
        # Only the owner has the VM's config; replicas keep the same datasets.
        disks = self.disks(self.owner(purpose), vmid)
        out = {}
        for host in self.vms[purpose]["placement"]:
            kinds = set()
            for d in disks:
                listed = self.on(host, f"zfs list -H -o name -t snapshot,bookmark -d 1 {d} 2>/dev/null || true")
                kinds |= {"snapshot" for l in listed.splitlines() if l == f"{d}@{name}"}
                kinds |= {"bookmark" for l in listed.splitlines() if l == f"{d}#{name}"}
            out[host] = "snapshot" if "snapshot" in kinds else "bookmark" if "bookmark" in kinds else ""
        return out

    def stop(self, *appliances) -> float:
        for b in appliances:
            b.app("systemctl stop bmac-backupd.service")
        self.s.note(f"stopped bmac-backupd on {', '.join(b.name for b in appliances)}")
        return time.monotonic()

    def start(self, *appliances) -> None:
        for b in appliances:
            b.app("systemctl start bmac-backupd.service")
        self.s.note(f"started bmac-backupd on {', '.join(b.name for b in appliances)}")

    def vlan_interface(self, b) -> str:
        return b.app(f"ip -o -4 addr show | awk '$4 ~ /^{re.escape(b.address)}\\// {{print $2}}'")

    # Steps

    def prepare(self) -> None:
        for b in (self.a, self.b):
            b.prepare()
        small = storage.guest(self.hosts["mox1"], "small.lab.internal")
        bulk = storage.guest(self.hosts["mox2"], "bulk.lab.internal")
        self.s.require(f"the small and bulk VMs are there ({small.get('name')}, {bulk.get('name')})",
                       bool(small.get("vmid")) and bool(bulk.get("vmid")))
        self.vms = {
            "small": {"name": small["name"], "vmid": int(small["vmid"]), "placement": ["mox1", "mox3"]},
            "bulk": {"name": bulk["name"], "vmid": int(bulk["vmid"]), "placement": ["mox2", "mox3"]},
        }
        self.a.vm = small
        self.b.vm = small
        join.ready(self.s, self.dashboard)
        for b in (self.a, self.b):
            b.enroll()
            b.app("bmac-backupd admin-password", input=f"{b.password}\n{b.password}\n".encode())
            out = b.at_console(f"bmac-backupd test-day-seconds {TEST_DAY_S}", "")
            self.s.check(f"{b.name}'s days last {TEST_DAY_S} s ({out})", f"a day now lasts {TEST_DAY_S} seconds" in out)
        saved = self.dashboard.call("POST", "/backups/settings",
                                    {"settings": SETTINGS, "revision": self.view()["settings_revision"]})
        self.s.require(f"the cluster's backup settings are compressed ({saved})",
                       all(saved.get("settings", {}).get(k) == v for k, v in SETTINGS.items()))
        self.cluster_uuid = self.on("mox1", "python3 -c \"import json; "
                                            "print(json.load(open('/etc/pve/priv/bmac/cluster.json'))['cluster_uuid'])\"")

    def seeds(self) -> None:
        """Steps 1 and 13: bulk's seed to B is cut and resumes; small's seeds
        to A and B are each a full stream."""
        a, b = self.a, self.b
        interface = self.vlan_interface(b)
        self.s.require(f"{b.name} has its VLAN interface ({interface})", interface != "")
        # The seed flows into B, so its ingress is policed; a root qdisc
        # would shape only B's ACKs.
        b.app(f"tc qdisc replace dev {interface} handle ffff: ingress && tc filter add dev {interface} parent ffff: "
              f"protocol ip prio 1 u32 match u32 0 0 police rate {THROTTLE} burst 256k drop flowid :1")
        try:
            self.policy("bulk", [b], NEWEST)
            self.vms["bulk"]["vm_uuid"] = self.vm_view("bulk").get("vm_uuid") or ""
            self.s.require("the Backups page names bulk's VM UUID", bool(self.vms["bulk"]["vm_uuid"]))
            bulk = self.vms["bulk"]["vm_uuid"]
            receiving = lambda method: [r for r in self.received(b) if f"/vms/{bulk}/" in r[0] and r[1] == method]
            self.wait(f"{b.name} starts bulk's seed", lambda: bool(receiving("full")), STEP_TIMEOUT)
            time.sleep(30)
            killed = b.app("pkill -TERM -x zfs -e || true")
            self.s.check(f"{b.name}'s receive of bulk's seed was still running when it was cut ({killed})", killed != "")
        finally:
            b.app(f"tc qdisc del dev {interface} handle ffff: ingress 2>/dev/null || true")
        self.wait(f"step 13: {b.name} resumes bulk's seed", lambda: bool(receiving("resumed")), STEP_TIMEOUT)
        first = self.newer(b, "bulk", {}, backup.SEED_TIMEOUT)
        self.s.check("step 1: bulk's first PIT on B is a full seed", bool(first) and first.get("received_from") is None)

        self.policy("small", [a, b], NEWEST)
        self.vms["small"]["vm_uuid"] = self.vm_view("small").get("vm_uuid") or ""
        self.s.require("the Backups page names small's VM UUID", bool(self.vms["small"]["vm_uuid"]))
        for appliance in (a, b):
            seeded = self.newer(appliance, "small", {}, backup.SEED_TIMEOUT)
            self.s.check(f"step 1: small's first PIT on {appliance.name} is a full seed",
                         bool(seeded) and seeded.get("received_from") is None)
        self.s.check("step 1: bulk isn't on A", lambda: self.pits(a, "bulk") == [])
        for appliance in (a, b):
            appliance.over_the_vlan(f"the seeds to {appliance.name}")

    def incrementals(self) -> None:
        """Step 2 and lab item 10."""
        for b, purposes in ((self.a, ("small",)), (self.b, ("small", "bulk"))):
            for purpose in purposes:
                self.wait(f"step 2: {b.name} publishes three PITs of {purpose}",
                          lambda b=b, purpose=purpose: len(self.pits(b, purpose)) >= 3)
                pits = self.pits(b, purpose)
                chained = all(p.get("received_from") == q["pit_uuid"] for q, p in zip(pits, pits[1:]))
                self.s.check(f"step 2: each later PIT of {purpose} on {b.name} is an incremental from the one before",
                             len(pits) >= 3 and chained)
        pits = self.pits(self.a, "small")
        newest = pits[-1]["pit_uuid"]
        # A bookmark is kept only for an appliance's head, here the newest
        # PIT, which local retention keeps as a snapshot; step 6 has one.
        older = pits[:-2]
        self.wait("step 2: once both appliances pulled them, small's older PITs have no snapshot or bookmark left",
                  lambda: older != [] and all(set(self.on_cluster("small", p).values()) == {""} for p in older), 300)
        self.s.note(f"newest PIT of small: {newest}")
        transfers = [t for t in self.view()["transfers"] if not t.get("error")]
        for t in transfers:
            rate = t["bytes"] / max(t["duration_ms"], 1) * 1000 / 2 ** 20
            self.s.note(f"item 10: {t['appliance']} pulled {t['bytes']} bytes of VM {t['vm_uuid']} from {t['host']} "
                        f"in {t['duration_ms'] / 1000:.1f} s ({rate:.1f} MiB/s)")
        self.s.check(f"step 2: every finished transfer has its bytes and duration ({len(transfers)})",
                     len(transfers) >= 7 and all(t["bytes"] > 0 and t["duration_ms"] > 0 for t in transfers))
        seeds = [t for t in transfers if t["bytes"] > 512 * 2 ** 20]
        self.s.check(f"item 10: the large seeds' throughput is recorded ({len(seeds)})", bool(seeds))

    def staging(self) -> None:
        """Step 3."""
        name = self.vms["small"]["name"]
        before = self.newest(self.a, "small")
        self.operate("staging.add", {"source": name}, f"clones {name} onto mox3",
                    answers={"staging_guest_vm_root_password": secrets.token_urlsafe(18)},
                    choices={"Staging standby": "mox3", "Snapshot for the new staging VM": "new"})
        staged = ops.staging_on(self.hosts["mox2"], "mox3")
        self.s.check(f"step 3: the staging VM runs on mox3 ({staged.get('name')})", staged.get("status") == "running")
        during = self.newer(self.a, "small", before)
        self.s.check("step 3: small is backed up while its staging VM runs",
                     bool(during) and during.get("received_from") == before.get("pit_uuid"))
        if staged.get("name"):
            self.operate("staging.remove", {"resource": staged["name"]},
                        f"removes {staged['name']}")
        after = self.newer(self.a, "small", during)
        self.s.check("step 3: and after it is removed, incrementally",
                     bool(after) and after.get("received_from") == during.get("pit_uuid"))
        self.s.check("step 3: no PIT snapshot of small is blocked from release",
                     not (self.vm_view("small").get("status") or {}).get("release_blocked"))

    def sent_by(self, pit: dict) -> set[str]:
        return {t["host"] for t in self.view()["transfers"]
                if t["pit_uuid"] == pit.get("pit_uuid") and not t.get("error")}

    def moves(self) -> None:
        """Step 4."""
        name = self.vms["small"]["name"]
        self.operate("prod.change_owner", {"resource": name}, f"moves {name} to mox3",
                    choices={"New owner": "mox3"})
        self.wait(f"{name} runs on mox3", lambda: self.owner("small") == "mox3", 300)
        # A tries the placement hosts in order, and mox1 has a replica of
        # every PIT; without its bmac-source, mox3 sends from its own copy of
        # A's head. A PIT mox1 took before the move may still be on its way.
        self.on("mox1", "systemctl stop bmac-source.service")
        try:
            moved, before = self.taken_after(self.a, "small", time.time())
        finally:
            self.on("mox1", "systemctl start bmac-source.service")
        self.s.check(f"step 4: after the move, the next PIT is an incremental sent by mox3 ({self.sent_by(moved)})",
                     bool(moved) and moved.get("received_from") == before.get("pit_uuid")
                     and self.sent_by(moved) == {"mox3"})

        mox3 = self.hosts["mox3"]
        self.lab.power(mox3.vm, "off")
        self.s.note("mox3 powered off")
        try:
            self.s.require(f"step 4: HA restarts {name} on mox1 after mox3 is lost", lambda: m6.wait_until(
                lambda: (g := ops.guests(self.hosts["mox2"])[name]).get("node") == "mox1"
                and g.get("status") == "running", ops.FAILOVER_TIMEOUT))
        finally:
            with m6.booting(self.s, lambda: m7.unlock_at_boot(self.lab, mox3.vm, mox3.carried["luks_passphrase"])):
                self.lab.power(mox3.vm, "on")
                self.s.require("mox3 comes back", lambda: m7.wait_back(mox3) != "")
        self.s.require("mox3 is back in the cluster", lambda: m6.wait_until(
            lambda: all(ops.nodes_online(h) for h in self.hosts.values()), m6.BOOT_TIMEOUT))
        failed_over, before = self.taken_after(self.a, "small", time.time())
        self.s.check(f"step 4: after the failover, the next PIT is an incremental sent by mox1 "
                     f"({self.sent_by(failed_over)})",
                     bool(failed_over) and failed_over.get("received_from") == before.get("pit_uuid")
                     and self.sent_by(failed_over) == {"mox1"})

    def away_briefly(self) -> None:
        """Steps 5 and 6."""
        a, b = self.a, self.b
        head = self.newest(a, "small")
        stopped_s = time.time()
        since = self.stop(a)
        self.wait(f"step 5: {a.name}'s LATE alert", lambda: self.alerted(
            "backup.appliance.late", f"appliance/{a.appliance_uuid}"), SETTINGS["late_after_s"] + WARNING_HOLD_S + 120)
        places = self.on_cluster("small", head)
        self.s.check(f"step 6: the cluster keeps {a.name}'s head ({places})", "bookmark" in places.values()
                     or "snapshot" in places.values())
        away = time.monotonic() - since
        self.s.require(f"step 6: {a.name} returns within the delinquency period ({away:.0f} s)",
                       away < SETTINGS["delinquency_s"])
        self.start(a)
        carried, _ = self.taken_after(b, "small", stopped_s)
        self.s.check(f"step 5: {b.name} carried on while {a.name} was away", bool(carried))
        caught = self.newer(a, "small", head)
        entry = self.published_entry(a, caught)
        self.s.check(f"step 6: {a.name} catches up incrementally from its head, from the cluster ({entry})",
                     bool(caught) and caught.get("received_from") == head.get("pit_uuid")
                     and entry.get("from_peer") is None)
        self.wait(f"step 6: {a.name}'s LATE alert resolves", lambda: not self.alerted(
            "backup.appliance.late", f"appliance/{a.appliance_uuid}"), 300)

    def peers(self) -> None:
        """Step 7, with the security test of a mistyped fingerprint."""
        a, b = self.a, self.b
        wrong = ":".join("00" for _ in range(32))
        [(status, result), (_, listed)] = self.ui(a, [
            ["POST", "/peers/trust", {"appliance_uuid": b.appliance_uuid, "fingerprint": wrong, "address": b.address}],
            ["GET", "/peers", None]])
        self.s.check(f"security: a typed fingerprint that doesn't match {b.name}'s certificate saves no entry "
                     f"({status}: {result})", not (result or {}).get("trusted") and not (listed or {}).get("peers"))
        self.s.check(f"security: {a.name} says what {b.name}'s address presented",
                     ((result or {}).get("presented") or "").replace(":", "").upper() == b.fingerprint.replace(":", ""))
        for one, other in ((a, b), (b, a)):
            [(status, result)] = self.ui(one, [["POST", "/peers/trust", {
                "appliance_uuid": other.appliance_uuid, "fingerprint": other.fingerprint, "address": other.address}]])
            self.s.check(f"step 7: {one.name}'s administrator trusts {other.name} ({status}: {result})",
                         status == 200 and (result or {}).get("trusted") is True)

    def head_gone(self, head: dict) -> bool:
        return set(self.on_cluster("small", head).values()) == {""}

    def away_long(self, repaired_by) -> dict:
        """A stops until it is delinquent and its head is gone from the
        cluster, then returns; its first PIT after."""
        a = self.a
        head = self.newest(a, "small")
        self.wait(f"{self.b.name} has {a.name}'s head", lambda: any(
            p["pit_uuid"] == head["pit_uuid"] for p in self.pits(self.b, "small")), 300)
        held = self.b.at_console(f"bmac-backupd hold {head['pit_uuid']}", "")
        self.s.note(f"{self.b.name}: {held}")
        try:
            self.stop(a)
            self.wait(f"{a.name} is delinquent", lambda: self.alerted(
                "backup.appliance.delinquent", f"appliance/{a.appliance_uuid}"), SETTINGS["delinquency_s"] + 300)
            self.wait(f"the cluster released {a.name}'s head, snapshot and bookmark", lambda: self.head_gone(head), 600)
            self.start(a)
            first = self.newer(a, "small", head)
        finally:
            self.b.at_console(f"bmac-backupd unhold {head['pit_uuid']}", "")
        entry = self.published_entry(a, first)
        self.s.note(f"{a.name}'s first PIT after its return: {entry}")
        repaired_by(head, first, entry)
        return first

    def repair(self) -> None:
        """Step 7."""
        a, b = self.a, self.b

        def from_b(head: dict, first: dict, entry: dict) -> None:
            self.s.check(f"step 7: {a.name} repairs from {b.name} with an incremental over its head",
                         bool(first) and entry.get("from_peer") == b.appliance_uuid
                         and first.get("received_from") == head.get("pit_uuid"))
        self.away_long(from_b)
        self.wait(f"step 7: {a.name}'s peers page shows that {b.name} trusts it", lambda: any(
            p["appliance_uuid"] == b.appliance_uuid and p.get("trusts_us") is True
            for p in (self.ui(a, [["GET", "/peers", None]])[0][1] or {}).get("peers", [])), 300)

        [(status, result)] = self.ui(b, [["POST", "/peers/untrust", {
            "appliance_uuid": a.appliance_uuid, "password": b.password}]])
        self.s.check(f"step 7: {b.name}'s administrator untrusts {a.name} ({status})", status == 200)
        before = self.refused_at(b, a)

        def refused(head: dict, first: dict, entry: dict) -> None:
            self.s.check(f"step 7: {a.name}'s next repair from {b.name} is refused, and it reseeds from the cluster",
                         bool(first) and entry.get("from_peer") is None and first.get("received_from") is None)
            self.s.check(f"step 7: {b.name}'s Trusted peers page lists the refusal", self.refused_at(b, a) > before)
            # A peer's refusals reach the audit log at most hourly; A's
            # first, before B trusted it, is there.
            self.s.check(f"step 7: {b.name}'s audit log has {a.name}'s refusal", any(
                fingerprint(e["details"].get("fingerprint", "")) == fingerprint(a.fingerprint)
                for e in self.audit(b, "refused_peer")))
        self.away_long(refused)

    def refused_at(self, b, peer) -> int:
        """When b last refused peer's certificate, from its Trusted peers page."""
        [(_, listed)] = self.ui(b, [["GET", "/peers", None]])
        return max((r["at_ms"] for r in (listed or {}).get("refusals", [])
                    if fingerprint(r["fingerprint"]) == fingerprint(peer.fingerprint)), default=0)

    def no_appliance(self) -> None:
        """Step 8."""
        name = self.vms["small"]["name"]
        self.stop(self.a, self.b)
        stopped_s = time.time()
        try:
            self.wait("step 8: a slot with no appliance available raises the alert", lambda: self.alerted(
                "backup.skipped", f"vm/{name}"), SETTINGS["ready_window_s"] + 2 * 60 * EVERY["minutes"] + 120)
            # A slot within the ready window still finds the appliances ready.
            taken = [p for p in self.vm_view("small").get("pits", [])
                     if p["taken_s"] > stopped_s + SETTINGS["ready_window_s"] + 5]
            self.s.check(f"step 8: and no snapshot ({[p['name'] for p in taken]})", taken == [])
        finally:
            self.start(self.a, self.b)
        self.wait("step 8: the alert resolves once a PIT is taken again", lambda: not self.alerted(
            "backup.skipped", f"vm/{name}"))

    def rollback(self) -> dict:
        """Step 9: the target is the newest PIT of a test day before today,
        which local retention keeps."""
        self.policy("small", [self.a, self.b], DAYS)
        vmid, owner = self.vms["small"]["vmid"], self.owner("small")
        target: dict = {}

        def older_local() -> bool:
            pits = self.pits(self.b, "small")
            on_a = {p["pit_uuid"] for p in self.pits(self.a, "small")}
            local = [p for p in pits[:-1] if p["pit_uuid"] in on_a
                     and self.on_cluster("small", p).get(owner) == "snapshot"]
            if local and any(p["published_at_ms"] > local[-1]["published_at_ms"] for p in pits):
                target.update(local[-1])
                return True
            return False
        self.s.require("an older PIT of small is still on the cluster", lambda: m6.wait_until(
            older_local, 3 * TEST_DAY_S + STEP_TIMEOUT))
        self.s.note(f"rolling back to {target['name']}")
        before = {b.name: self.newest(b, "small") for b in (self.a, self.b)}
        self.job("step 9: small rolls back to an older PIT", "backup.rollback", {"vm": self.vms["small"]["name"]},
                 {"pit": target["pit_uuid"]})
        self.s.check("small runs after the rollback", lambda: m6.wait_until(
            lambda: self.on(owner, f"qm status {vmid}") == "status: running", 300))
        for b in (self.a, self.b):
            first = self.newer(b, "small", before[b.name])
            self.s.check(f"step 9: the next PIT on {b.name} starts a new branch from the rollback target",
                         bool(first) and first.get("branch_uuid") != target.get("branch_uuid")
                         and first.get("received_from") == target.get("pit_uuid"))
            clones = [r for r in self.received(b) if r[1].startswith("a clone of ")
                      and f"/vms/{self.vms['small']['vm_uuid']}/{first.get('branch_uuid')}/" in r[0]]
            self.s.check(f"step 9: {b.name} received it as a clone ({clones[-1:] if clones else None})", bool(clones))
        return target

    def issue_grant(self) -> None:
        pits = [p["pit_uuid"] for p in self.pits(self.b, "small")]
        [(status, issued)] = self.ui(self.b, [["POST", "/grants", {"cluster_uuid": self.cluster_uuid, "hours": 8,
                                                                  "pits": pits}]])
        self.s.require(f"{self.b.name}'s administrator issues a restore grant for small's PITs ({status})",
                       status == 200 and str((issued or {}).get("token", "")).startswith("bmac-grant-"))
        self.grants.append(issued["token"])

    def source(self) -> dict:
        return {"appliance": self.b.appliance_uuid, "grant": self.grants[-1]}

    def restores(self, target: dict) -> None:
        """Steps 10 and 15."""
        b, small = self.b, self.vms["small"]
        # No capture releases the local snapshots the restores start from.
        self.policy("small", [self.a, b], DAYS, enabled=False)
        self.issue_grant()
        pits = self.pits(b, "small")
        old_branch = [p for p in pits if p["branch_uuid"] == target["branch_uuid"]]
        later = [p for p in old_branch if p["published_at_ms"] > target["published_at_ms"]]
        self.s.require(f"{b.name} holds a PIT of the old branch after the rollback target ({len(later)})", bool(later))

        owner = self.owner("small")
        self.s.require(f"the rollback target is still a snapshot on {owner}",
                       lambda: self.on_cluster("small", target).get(owner) == "snapshot")
        result, details = self.job(f"step 10: a new VM on {owner} from an old-branch PIT", "backup.restore",
                                   {"host": owner}, {**self.source(), "pit": later[0]["pit_uuid"],
                                                     "isolate": True, "start": False})
        self.s.check(f"step 10: on {owner}, an incremental over the rollback target's snapshot (method 2) "
                     f"({result.get('methods')})", set((result.get("methods") or {}).values()) == {2})
        self.s.check("step 10: the estimate is shown first", any("method 2" in d and "to transfer" in d for d in details))
        restored = int(result.get("vmid") or 0)
        if restored:
            self.on(owner, f"qm destroy {restored} --purge 1 --destroy-unreferenced-disks 1")

        result, details = self.job("step 15: a restore test of an old-branch PIT on mox2", "backup.restore_test",
                                   {"host": "mox2"}, {**self.source(), "pit": later[-1]["pit_uuid"]})
        self.s.check(f"step 10: on mox2, each volume is a full stream (method 3) ({result.get('methods')})",
                     bool(result.get("methods")) and set(result["methods"].values()) == {3})
        self.s.check("step 10: the estimate is shown first", any("method 3" in d and "to transfer" in d for d in details))
        self.s.check("step 15: the scratch VM booted and is destroyed",
                     result.get("booted") is True and result.get("destroyed") is True)

        local = [p for p in self.pits(b, "small") if p["branch_uuid"] != target["branch_uuid"]
                 and self.on_cluster("small", p).get(owner) == "snapshot"]
        self.issue_grant()
        self.s.require("a new-branch PIT is on the cluster", bool(local))
        restored_pit = local[-1]
        before = {x.name: self.newest(x, "small") for x in (self.a, b)}
        result, details = self.job("step 10: a new-branch PIT restored into small", "backup.restore_into",
                                   {"vm": small["name"]}, {**self.source(), "pit": restored_pit["pit_uuid"]})
        self.s.check(f"step 10: into small from its local snapshot (method 1) ({result.get('methods')})",
                     set((result.get("methods") or {}).values()) == {1})
        self.s.check("step 10: the plan is shown first", any("method 1" in d for d in details))
        self.s.check("small runs after the restore", lambda: m6.wait_until(
            lambda: self.on(owner, f"qm status {small['vmid']}") == "status: running", 300))
        self.policy("small", [self.a, b], DAYS)
        for x in (self.a, b):
            first = self.newer(x, "small", before[x.name])
            self.s.check(f"step 10: the first PIT after the restore is an incremental from the restored PIT on {x.name}",
                         bool(first) and first.get("received_from") == restored_pit["pit_uuid"])
        self.job("the set-aside disks are removed", "backup.cleanup", {"host": owner},
                 {"disks": lambda field: [o["value"] for o in field["options"]]})

    def pruned(self) -> None:
        """Step 11."""
        a = self.a
        published = {e["details"]["pit_uuid"] for e in self.audit(a, "publish")
                     if e["details"].get("vm_uuid") == self.vms["small"]["vm_uuid"]}
        kept = {p["pit_uuid"] for p in self.pits(a, "small")}
        self.wait(f"step 11: {a.name}'s retention prunes intermediate PITs", lambda: bool(self.audit(a, "prune")), 600)
        self.s.check(f"step 11: {a.name} keeps fewer PITs of small than it published ({len(kept)} of {len(published)})",
                     len(kept) < len(published))
        out = a.at_console("bmac-backupd prune", "")
        self.s.check(f"step 11: a prune runs at the console ({out.splitlines()[0] if out else ''})",
                     out.startswith("pruned ") and "stay after a failure" in out)

    def reboot(self) -> None:
        """Step 12."""
        a = self.a
        count = len(a.pits())
        before = a.boot_id()
        a.app("systemd-run --on-active=2 systemctl reboot >/dev/null")
        self.s.require(f"step 12: {a.name} reboots", lambda: m6.wait_until(
            lambda: a.boot_id() not in ("", before), m6.BOOT_TIMEOUT))
        self.wait(f"step 12: the Dashboard shows {a.name} LOCKED", lambda: a.listed().get("locked") is True, 300)
        out = a.at_console("bmac-backupd unlock", f"{a.passphrase}\n")
        self.s.check(f"step 12: {a.name} is unlocked at its console", "is unlocked" in out)
        self.wait(f"step 12: the Dashboard shows {a.name} unlocked", lambda: a.listed().get("locked") is False, 300)
        self.s.check(f"step 12: {a.name} keeps its PITs ({count})", len(a.pits()) >= count - 2)
        self.s.check(f"step 12: {a.name} carries on", bool(self.newer(a, "small", self.newest(a, "small"))))

    def scrub(self) -> None:
        """Step 14."""
        b = self.b
        out = b.at_console("bmac-backupd scrub now", "")
        self.s.note(f"{b.name}: {out}")
        report = {}

        def finished() -> bool:
            text = b.app("bmac-backupd scrub")
            report["text"] = text
            return ", finished " in text
        self.wait(f"step 14: a scrub of {b.name} finishes", finished, 1800)
        self.s.check(f"step 14: it finds nothing to repair ({report.get('text', '').splitlines()[-1:]})",
                     "0 bytes repaired, 0 unrepairable errors" in report.get("text", ""))
        self.s.check(f"step 14: no scrub alert for {b.name}", not self.alerted(
            "backup.appliance.scrub_errors", f"appliance/{b.appliance_uuid}"))

    def recreation(self) -> None:
        """Step 16: bulk is removed and recreated from its newest PIT on B."""
        b, bulk = self.b, self.vms["bulk"]
        keys = ("bios", "machine", "cores", "memory", "efidisk0", "tpmstate0", "scsihw", "ostype")
        config = lambda host, vmid: {l.split(":", 1)[0]: l.split(":", 1)[1].strip()
                                     for l in self.on(host, f"qm config {vmid}").splitlines() if ":" in l}
        before = config("mox2", bulk["vmid"])
        newest = self.newest(b, "bulk")
        [(status, issued)] = self.ui(b, [["POST", "/grants", {"cluster_uuid": self.cluster_uuid, "hours": 8,
                                                             "pits": [newest["pit_uuid"]]}]])
        self.s.require(f"a restore grant for bulk's newest PIT ({status})", status == 200)
        self.grants.append(issued["token"])
        self.dashboard.call("POST", f"/backups/policies/{bulk['vm_uuid']}/delete",
                            {"revision": self.vm_view("bulk")["policy"]["revision"]})
        self.operate("prod.remove", {"resource": bulk["name"]}, f"removes {bulk['name']}")
        result, _ = self.job("step 16: bulk is recreated from its newest PIT on mox2", "backup.restore", {"host": "mox2"},
                             {**self.source(), "pit": newest["pit_uuid"], "isolate": True, "start": True})
        vmid = int(result.get("vmid") or 0)
        self.s.require(f"the recreated VM is VM {vmid}", vmid > 0)
        after = config("mox2", vmid)
        # Disks are new volumes, so only their presence must match.
        differences = {k: (before.get(k), after.get(k)) for k in keys if (k in before) != (k in after)
                       or (not k.endswith("0") and before.get(k) != after.get(k))}
        self.s.check(f"step 16: the recreated VM has bulk's firmware, machine, CPUs, memory, EFI and TPM disks "
                     f"(differences: {differences})", differences == {})
        self.s.check("step 16: the recreated VM runs", lambda: m6.wait_until(
            lambda: self.on("mox2", f"qm status {vmid}") == "status: running", 300))

    def security(self) -> None:
        a, b = self.a, self.b
        for appliance in (a, b):
            for path in ("/v1/export", "/v1/peer-index", "/v1/granted"):
                code = self.on("mox1", f"curl -sk -o /dev/null -w '%{{http_code}}' --max-time 20 "
                                       f"--cert /etc/pve/local/pve-ssl.pem --key /etc/pve/local/pve-ssl.key "
                                       f"https://{appliance.address}:8445{path} || true")
                self.s.check(f"security: a host's certificate gets nothing from {appliance.name}'s {path} ({code})",
                             not code.startswith("2"))
            for method, path in (("GET", "/api/v1/pits"), ("POST", "/api/v1/retention"), ("POST", "/api/v1/prune"),
                                 ("POST", "/api/v1/peers/trust"), ("POST", "/api/v1/holds")):
                code = self.on("mox1", f"curl -sk -o /dev/null -w '%{{http_code}}' --max-time 20 -X {method} "
                                       f"-H 'Content-Type: application/json' -d '{{}}' "
                                       f"--cert /etc/pve/local/pve-ssl.pem --key /etc/pve/local/pve-ssl.key "
                                       f"https://{appliance.address}:8443{path} || true")
                self.s.check(f"security: the cluster can't {method} {appliance.name}'s {path} ({code})",
                             code in ("401", "403"))
        before = self.refused_at(b, a)
        code = a.app(f"curl -sk -o /dev/null -w '%{{http_code}}' --max-time 20 --cert /var/lib/bmac-backupd/tls/appliance.pem "
                     f"--key /var/lib/bmac-backupd/tls/appliance.key https://{b.address}:8445/v1/peer-index || true")
        self.s.check(f"security: {a.name}, which {b.name} no longer trusts, gets nothing from its 8445 ({code})",
                     not code.startswith("2"))
        self.s.check(f"security: {b.name}'s Trusted peers page lists the refusal", lambda: m6.wait_until(
            lambda: self.refused_at(b, a) > before, 30))

        hidden = [x for x in [a.passphrase, b.passphrase, *self.grants] + [
            h.carried.get("luks_passphrase") or "" for h in self.hosts.values()] if x]
        patterns = ("\n".join(hidden) + "\n").encode()
        for appliance in (a, b):
            found = self.leaks(appliance.appliance, patterns, "cat /var/lib/bmac-backupd/metadata/audit.jsonl* "
                                                              "/var/log/syslog 2>/dev/null")
            self.s.check(f"security: no LUKS passphrase or grant in {appliance.name}'s logs ({found} lines)", found == "0")
        for name in ("mox1", "mox2", "mox3"):
            found = self.leaks(self.hosts[name].vm, patterns, "cat /var/log/bmac/*.log 2>/dev/null; "
                                                              "find /etc/pve/priv/bmac -name '*.json*' -exec cat {} + 2>/dev/null")
            self.s.check(f"security: no LUKS passphrase or grant in {name}'s logs ({found} lines)", found == "0")
        pages = json.dumps([self.view(), self.dashboard.call("GET", "/appliances"), self.alerts()])
        self.s.check("security: no LUKS passphrase or grant on the Dashboard's pages",
                     not any(x in pages for x in hidden))

    def leaks(self, vm, patterns: bytes, also: str) -> str:
        """How many lines of vm's journal and `also`'s output hold one of
        the patterns, which come on stdin and are never printed."""
        done = subprocess.run(self.lab.ssh_args(vm, [
            f"exec 3<&0; {{ journalctl --no-pager -o cat; {also}; }} </dev/null | grep -cFf /dev/fd/3 || true"]),
            input=patterns, capture_output=True, timeout=300)
        return done.stdout.decode(errors="replace").strip()


def then(hosts) -> None:
    s = hosts["mox3"].s
    t = Accept(s, hosts)
    try:
        t.prepare()
        t.seeds()
        t.incrementals()
        t.staging()
        t.moves()
        t.away_briefly()
        t.peers()
        t.repair()
        t.no_appliance()
        target = t.rollback()
        t.restores(target)
        t.pruned()
        t.reboot()
        t.scrub()
        t.recreation()
        t.security()
    finally:
        for b in (t.a, t.b):
            try:
                s.note(f"{b.name}'s service log:\n" + b.app(
                    "journalctl -u bmac-backupd.service --no-pager -o cat 2>/dev/null | tail -40 || true"))
            except Exception as e:  # diagnostics only
                s.note(f"{b.name}'s service log: {e}")
            if (s.lab.paths.vm(b.appliance.name) / f"checkpoint-{backup.CHECKPOINT}.fd").exists():
                s.lab.restore_checkpoint(b.appliance, backup.CHECKPOINT)
            s.lab.forget_tailnet(names={b.tailnet_name}, required=False)


def run(s) -> None:
    storage.staged(s, then)
