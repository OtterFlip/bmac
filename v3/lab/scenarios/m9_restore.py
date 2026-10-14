# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M9.4's lab proof: every way back from a PIT, on the "small" VM, placed on
mox1 and mox3, with its policy keeping the newest two PITs on the cluster.

It starts as m9_backup does (the appliance enrolled, P1 pulled in full and
P2 incrementally), then, with a restore grant the appliance's administrator
issued for both:

- a restore test of P2 on mox2, which has none of the VM's snapshots, so
  every volume comes as a full stream (method 3); the scratch VM boots and
  is destroyed, and the record keeps the outcome;
- a restore of P1 as a new VM on mox1, cloned from the snapshot there
  (method 1), with its network disconnected;
- the promotion of that VM to production: the production VM keeps its
  VMID, its disks are set aside, replication to mox3 converges again, and
  the next PIT, P3, is pulled incrementally from P1 on a new branch;
- a rollback to P3 in place, while its snapshots are still on the cluster;
- a restore of P2 into the production VM, its disks swapped in;
- a cleanup of everything set aside, after which the VM still runs;
- P4, pulled incrementally from P2, then an online growth of the root disk
  and P5, pulled incrementally from P4 at the new size (lab item 7);
- the PITs of the running VM froze its filesystems through the guest agent
  (lab item 8);
- the Dashboard's Backups page shows the PITs on both branches, the
  restores, the transfers, and the local snapshot space, and the
  appliance's Recovery page shows the reported restore test, the recovery
  manifests, and every host's LUKS header backups.

The appliance and the hosts are put back as they were."""

from __future__ import annotations

import json
import re
import secrets
from pathlib import Path

from bmaclab.scenario import load

DESCRIPTION = "Restores from the appliance: a restore test, a new VM, promotion, rollback, restore into, cleanup"

backup = load(Path(__file__).with_name("m9_backup.py"))
storage, m6, join = backup.storage, backup.m6, backup.join

UI_CALLS = "/root/ui_calls.py"
# Runs on the appliance: logs in with the password on stdin's first line,
# makes the calls on its second, logs out, and prints the answers.
UI_CALLS_SCRIPT = r'''
import http.client, json, ssl, sys

password = sys.stdin.readline().rstrip("\n")
calls = json.loads(sys.stdin.readline())
context = ssl._create_unverified_context()
origin = "https://127.0.0.1:8443"

def call(method, path, body=None, token=None):
    connection = http.client.HTTPSConnection("127.0.0.1", 8443, context=context, timeout=60)
    headers = {"Origin": origin}
    if body is not None:
        headers["Content-Type"] = "application/json"
    if token:
        headers["Authorization"] = "Bearer " + token
    connection.request(method, path, body=None if body is None else json.dumps(body), headers=headers)
    response = connection.getresponse()
    data = response.read()
    connection.close()
    return [response.status, json.loads(data) if data else None]

status, login = call("POST", "/api/v1/login", {"password": password})
if status != 200:
    print(json.dumps({"login": status}))
    sys.exit(0)
token = login["token"]
answers = [call(m, "/api/v1" + p, b, token) for m, p, b in calls]
call("POST", "/api/v1/logout", {}, token)
print(json.dumps({"login": status, "answers": answers}))
'''
DISK_LINE = re.compile(r"^(scsi|virtio|sata|ide|efidisk|tpmstate)\d+: ([^,\s]+)")


class Restores:
    def __init__(self, s, hosts) -> None:
        self.s, self.hosts = s, hosts
        self.b = backup.Backup(s, hosts)
        self.b.local_retention = {"newest": 2, "days": 0, "weeks": 0}
        self.password = secrets.token_urlsafe(18)
        self.grant = ""
        self.cluster_uuid = ""
        self.pits: dict[str, dict] = {}
        self.shown: dict[str, dict] = {}

    # Plumbing

    def on(self, name: str, command: str, timeout: float = 300) -> str:
        return self.hosts[name].on_host(command, timeout=timeout)

    def ui(self, calls: list) -> list:
        self.b.app(f"install -m 0600 /dev/stdin {UI_CALLS}", input=UI_CALLS_SCRIPT.encode())
        try:
            text = self.b.app(f"python3 {UI_CALLS}", timeout=180,
                              input=f"{self.password}\n{json.dumps(calls)}\n".encode())
        finally:
            self.b.app(f"rm -f {UI_CALLS}")
        out = json.loads(text)
        if out.get("login") != 200:
            raise RuntimeError(f"the appliance's UI refused the login ({out.get('login')})")
        return out["answers"]

    def job(self, what: str, operation: str, parameters: dict, answers: dict | None = None) -> dict:
        """Runs a job to its end, answering each field from `answers` (a
        value, or a function of the form) or its default; the job's result."""
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

        summary, events = join.follow(self.s, self.b.dashboard, self.b.dashboard.start(operation, parameters), reply)
        self.s.check(f"{what} ({summary.get('state')}: {summary.get('message') or ''})",
                     summary.get("state") == "COMPLETED")
        for d in details:
            self.s.note(f"confirm: {d}")
        result = next((e["data"] for e in reversed(events) if e["type"] == "result"), {})
        self.s.note(f"result: {json.dumps(result)}")
        return result

    def source(self) -> dict:
        return {"appliance": self.b.appliance_uuid, "grant": self.grant}

    def vmid(self) -> int:
        return int(self.b.vm["vmid"])

    def disks(self, host: str, vmid: int) -> list[str]:
        """The datasets of the VM's disks."""
        out = []
        for line in self.on(host, f"qm config {vmid}").splitlines():
            found = DISK_LINE.match(line)
            if found and "media=cdrom" not in line and found.group(2) != "none":
                out.append(self.on(host, f"pvesm path {found.group(2)}").removeprefix("/dev/zvol/"))
        return out

    def origin(self, dataset: str) -> str:
        return self.on("mox1", f"zfs get -H -o value origin {dataset}")

    def set_aside(self) -> list[str]:
        return [d for d in self.on("mox1", "zfs list -H -o name -t volume").splitlines() if "/bmac-replaced/" in d]

    def running(self, host: str, vmid: int) -> bool:
        return self.on(host, f"qm status {vmid}") == "status: running"

    def replicated(self) -> bool:
        """Every replication job of the VM has synced and none is failing."""
        jobs = json.loads(self.on("mox1", "pvesh get /nodes/mox1/replication --output-format json"))
        mine = [j for j in jobs if int(j.get("guest", 0)) == self.vmid()]
        return bool(mine) and all(j.get("last_sync", 0) > 0 and not j.get("fail_count") and not j.get("error")
                                  for j in mine)

    def restore_records(self) -> list[dict]:
        text = self.on("mox1", "for f in /etc/pve/priv/bmac/backup/restores/*.json; do [ -f \"$f\" ] && cat \"$f\" "
                               "&& echo; done; true")
        return [json.loads(l) for l in text.splitlines() if l.strip()]

    # Steps

    def start(self) -> None:
        b = self.b
        b.prepare()
        b.find_vm()
        join.ready(self.s, b.dashboard)
        b.enroll()
        b.policy()
        b.capture("P1")
        first = b.pulled("P1", 1, backup.SEED_TIMEOUT)
        b.capture("P2")
        second = b.pulled("P2", 2)
        self.s.require("P1 and P2 are on the appliance", bool(first) and bool(second))
        b.over_the_vlan("P1 and P2")
        self.pits = {"P1": first, "P2": second}
        self.note_shown()
        self.cluster_uuid = self.on("mox1", "python3 -c \"import json; "
                                            "print(json.load(open('/etc/pve/priv/bmac/cluster.json'))['cluster_uuid'])\"")
        b.app("bmac-backupd admin-password", input=f"{self.password}\n{self.password}\n".encode())

    def issue_grant(self) -> None:
        pits = [self.pits["P1"]["pit_uuid"], self.pits["P2"]["pit_uuid"]]
        [(status, issued)] = self.ui([["POST", "/grants", {"cluster_uuid": self.cluster_uuid, "hours": 8, "pits": pits}]])
        self.s.require(f"the appliance's administrator issues a restore grant for P1 and P2 ({status})",
                       status == 200 and str((issued or {}).get("token", "")).startswith("bmac-grant-"))
        self.grant = issued["token"]
        self.s.check("the appliance's audit log records the grant", lambda: self.b.app(
            "grep -c '\"grant\"' /var/lib/bmac-backupd/metadata/audit.jsonl || true") != "0")

    def restore_test(self) -> None:
        before = len(self.restore_records())
        result = self.job("a restore test of P2 on mox2 completes", "backup.restore_test", {"host": "mox2"},
                          {**self.source(), "pit": self.pits["P2"]["pit_uuid"]})
        self.s.check(f"every volume came as a full stream to mox2 ({result.get('methods')})",
                     bool(result.get("methods")) and set(result["methods"].values()) == {3})
        self.s.check("the scratch VM booted", result.get("booted") is True)
        self.s.note(f"the scratch VM's guest agent answered: {result.get('agent')} after {result.get('agent_after_s')} s")
        self.s.check("the scratch VM is destroyed", result.get("destroyed") is True and "bmac-test-" not in
                     self.on("mox2", "qm list"))
        self.s.check("mox2 keeps no restored volume", lambda: not [
            d for d in self.on("mox2", "zfs list -H -o name -t volume").splitlines()
            if "/bmac-restore/" in d or d.rsplit("/", 1)[-1].startswith(f"vm-{result.get('vmid')}-")])
        records = self.restore_records()
        test = [r for r in records if r.get("mode") == "test"]
        self.s.check(f"the restore record keeps the test's outcome ({test[-1].get('test') if test else None})",
                     len(records) == before + 1 and bool(test) and (test[-1].get("test") or {}).get("booted") is True
                     and test[-1].get("ended_at_ms") is not None)

    def restore_new(self) -> int:
        result = self.job("a restore of P1 as a new VM on mox1 completes", "backup.restore", {"host": "mox1"},
                          {**self.source(), "pit": self.pits["P1"]["pit_uuid"], "isolate": True, "start": True})
        vmid = int(result.get("vmid") or 0)
        self.s.require(f"the restored VM is VM {vmid} on mox1", vmid > 0 and vmid != self.vmid())
        self.s.check(f"P1's snapshot on mox1 is cloned, with nothing over the network ({result.get('methods')})",
                     set((result.get("methods") or {}).values()) == {1})
        config = self.on("mox1", f"qm config {vmid}")
        nets = [l for l in config.splitlines() if re.match(r"^net\d+:", l)]
        self.s.check(f"the restored VM's network starts disconnected ({len(nets)} NICs)",
                     bool(nets) and all("link_down=1" in l for l in nets))
        macs = lambda text: re.findall(r"^net\d+: [a-z0-9]+=([0-9A-Fa-f:]{17})", text, re.M)
        production_macs, restored_macs = macs(self.on("mox1", f"qm config {self.vmid()}")), macs(config)
        self.s.check(f"the restored VM gets new MAC addresses ({restored_macs}, production {production_macs})",
                     bool(production_macs) and len(restored_macs) == len(production_macs)
                     and not set(restored_macs) & set(production_macs))
        self.s.check("the restored VM runs", lambda: self.running("mox1", vmid))
        self.s.check("the restored VM's disks are named for it", all(
            d.rsplit("/", 1)[-1].startswith(f"vm-{vmid}-disk-") for d in self.disks("mox1", vmid)))
        return vmid

    def promote(self, restored: int) -> None:
        vmid = self.vmid()
        result = self.job("the restored VM is promoted to production", "backup.promote", {"vm": self.b.vm["name"]},
                          {"restored": lambda field: next(o["value"] for o in field["options"]
                                                          if f"VM {restored} " in o["label"])})
        self.s.check(f"the production VM keeps VMID {vmid} and HA starts it", lambda: m6.wait_until(
            lambda: self.running("mox1", vmid), 300))
        self.s.check(f"the restored VM {restored} is gone", lambda: "does not exist" in self.on(
            "mox1", f"qm config {restored} 2>&1 || true"))
        self.s.check(f"the production disks were set aside ({len(self.set_aside())})", lambda: len(self.set_aside()) >= 1)
        self.s.check("replication to mox3 converged again", lambda: m6.wait_until(self.replicated, 1800))
        self.s.note(f"the promotion's result: {json.dumps(result)}")

    def continues(self) -> None:
        """P3 continues from P1 on a new branch, incrementally."""
        self.b.capture("P3")
        third = self.b.pulled("P3", 3)
        self.pits["P3"] = third
        self.note_shown()
        self.s.check(f"P3 is pulled incrementally from P1 ({third.get('received_from')})",
                     bool(third) and third.get("received_from") == self.pits["P1"]["pit_uuid"])
        self.s.check("P3 is on a new branch", bool(third) and third.get("branch_uuid") != self.pits["P1"].get("branch_uuid"))
        self.s.check("P3 moved less than P1", bool(third) and third.get("bytes", 0) < self.pits["P1"].get("bytes", 0))

    def rollback(self) -> None:
        vmid = self.vmid()
        p3 = self.pits["P3"]
        self.s.require("P3's snapshots are on mox1", lambda: m6.wait_until(lambda: all(
            self.on("mox1", f"zfs list -H -o name {d}@{p3['name']} 2>/dev/null || true") != ""
            for d in self.disks("mox1", vmid)), 300))
        self.job("the production VM rolls back to P3", "backup.rollback", {"vm": self.b.vm["name"]}, {"pit": p3["pit_uuid"]})
        self.s.check("after the rollback, P3 is each disk's newest snapshot", all(
            self.on("mox1", f"zfs list -H -o name -t snapshot -s createtxg -d 1 {d} | grep -v __replicate_ | tail -1")
            == f"{d}@{p3['name']}" for d in self.disks("mox1", vmid)))
        self.s.check("the VM runs after the rollback", lambda: m6.wait_until(lambda: self.running("mox1", vmid), 300))
        self.s.check("replication to mox3 converged after the rollback", lambda: m6.wait_until(self.replicated, 1800))

    def restore_into(self) -> None:
        vmid = self.vmid()
        before = set(self.set_aside())
        self.job("P2 is restored into the production VM", "backup.restore_into", {"vm": self.b.vm["name"]},
                 {**self.source(), "pit": self.pits["P2"]["pit_uuid"]})
        p2 = self.pits["P2"]["name"]
        self.s.check("the VM's disks hold P2", all(
            self.on("mox1", f"zfs get -H -o value guid {d}@{p2} 2>/dev/null || zfs get -H -o value guid "
                            f"$(zfs get -H -o value origin {d}) 2>/dev/null || true")
            in {str(v["guid"]) for v in self.pits["P2"]["volumes"]} for d in self.disks("mox1", vmid)))
        self.s.check("the disks it replaced were set aside", lambda: len(set(self.set_aside()) - before) >= 1)
        self.s.check("the VM runs after the restore", lambda: m6.wait_until(lambda: self.running("mox1", vmid), 300))
        self.s.check("replication to mox3 converged after the restore", lambda: m6.wait_until(self.replicated, 1800))

    def cleanup(self) -> None:
        vmid = self.vmid()
        self.job("the set-aside disks are removed", "backup.cleanup", {"host": "mox1"},
                 {"disks": lambda field: [o["value"] for o in field["options"]]})
        self.s.check(f"nothing is set aside on mox1 ({self.set_aside()})", lambda: self.set_aside() == [])
        self.s.check("no disk of the VM is a clone of a removed one", all(
            "/bmac-replaced/" not in self.origin(d) for d in self.disks("mox1", vmid)))
        self.s.check("the VM still runs", lambda: self.running("mox1", vmid))
        self.s.check("each restore is recorded", lambda: len(self.restore_records()) >= 4)

    def grown(self) -> None:
        """P4 continues from P2, restored into the VM; P5 follows an online
        growth of its root disk (`BACKUP.md` lab item 7)."""
        vmid = self.vmid()
        self.b.capture("P4")
        fourth = self.pits["P4"] = self.b.pulled("P4", 4)
        self.s.check(f"P4 is pulled incrementally from P2, restored into the VM ({fourth.get('received_from')})",
                     bool(fourth) and fourth.get("received_from") == self.pits["P2"]["pit_uuid"])
        volume = next(l.split()[1].split(",")[0] for l in self.on("mox1", f"qm config {vmid}").splitlines()
                      if l.startswith("scsi0: "))
        root = self.on("mox1", f"pvesm path {volume}").removeprefix("/dev/zvol/")
        size = lambda: int(self.on("mox1", f"zfs get -Hp -o value volsize {root}"))
        before = size()
        self.on("mox1", f"qm resize {vmid} scsi0 +1G")
        grown = size()
        self.s.check(f"the running VM's root disk grew by 1 GiB ({before} to {grown})",
                     grown == before + 1024 ** 3 and self.running("mox1", vmid))
        self.b.capture("P5")
        fifth = self.pits["P5"] = self.b.pulled("P5", 5)
        self.s.check(f"P5 is pulled incrementally from P4 after the growth ({fifth.get('received_from')})",
                     bool(fifth) and fifth.get("received_from") == fourth.get("pit_uuid"))
        sizes = {int(self.b.app(f"zfs get -Hp -o value volsize {v['dataset']}@{fifth['name']}"))
                 for v in fifth.get("volumes", [])}
        self.s.check(f"the appliance's copy of P5 has the grown size ({sorted(sizes)})", grown in sizes)

    def note_shown(self) -> tuple[dict, dict]:
        """The VM on the Backups page, noting each PIT shown: the catalog drops a
        PIT once the cluster keeps neither its snapshot nor its bookmark."""
        view = self.b.dashboard.call("GET", "/backups")
        mine = next((v for v in view["vms"] if v["vm"] == self.b.vm["name"]), {})
        for p in mine.get("pits", []):
            self.shown[p["pit_uuid"]] = p
        return view, mine

    def reported(self) -> None:
        """What the Dashboard's Backups page and the appliance's Recovery page show."""
        view, mine = self.note_shown()
        shown = {p["pit_uuid"] for p in mine.get("pits", [])}
        published = {p["pit_uuid"] for p in self.b.pits()}
        uuid = lambda label: self.pits[label].get("pit_uuid")
        dropped = sorted(label for label in self.pits if uuid(label) not in shown)
        self.s.check(f"the Backups page shows the VM's policy and the PITs the cluster keeps (dropped: {dropped})",
                     bool(mine.get("policy")) and {uuid("P4"), uuid("P5")} <= shown
                     and all(uuid(label) in published for label in dropped))
        self.s.check("the Backups page showed two branches",
                     len({p["branch_uuid"] for p in self.shown.values()}) >= 2)
        levels = {label: self.shown.get(p.get("pit_uuid"), {}).get("consistency")
                  for label, p in sorted(self.pits.items())}
        # P3 and P4 follow a restart closely, before the agent may answer.
        self.s.check(f"the PITs of a running VM froze its filesystems through the agent ({levels})",
                     all(levels.get(p) == "filesystem" for p in ("P1", "P2", "P5")))
        self.s.check(f"the Backups page shows the restores ({len(view['restores'])})", len(view["restores"]) >= 4 and any(
            r["mode"] == "test" and (r.get("test") or {}).get("booted") for r in view["restores"]))
        moved = [t for t in view["transfers"] if t["appliance"] == backup.APPLIANCE_NAME and not t.get("error")]
        self.s.check(f"the Backups page shows the finished transfers ({len(moved)})",
                     bool(moved) and all(t["bytes"] > 0 and t["duration_ms"] > 0 for t in moved))
        self.s.check("the Backups page shows the cluster's local PIT snapshot space",
                     any(l.get("bytes", 0) > 0 for l in mine.get("local", [])))

        tested = {r["restore_uuid"] for r in self.restore_records() if r.get("mode") == "test"}

        def recovery() -> dict:
            [(status, page)] = self.ui([["GET", "/recovery", None]])
            return next((c for c in (page or {}).get("clusters", []) if c["cluster_uuid"] == self.cluster_uuid), {})

        self.s.check("the appliance's Recovery page shows the restore test the cluster reported", lambda: m6.wait_until(
            lambda: tested <= {t["restore_uuid"] for t in recovery().get("restore_tests", [])}, 600))
        cluster = recovery()
        self.s.check(f"the appliance keeps recovery manifests ({len(cluster.get('manifests', []))})",
                     bool(cluster.get("manifests")))
        kept = {(h["host"], h["file"]) for h in cluster.get("headers", [])}
        on_hosts = {(name, f) for name in ("mox1", "mox2", "mox3") for f in self.on(
            name, "ls /var/lib/bmac/luks-headers/ 2>/dev/null | grep '\\.luks-header$' || true").split()}
        self.s.check(f"the appliance keeps every host's LUKS header backups ({sorted(kept)}; on the hosts "
                     f"{sorted(on_hosts)})", bool(on_hosts) and on_hosts <= kept)
        self.s.check("no LUKS header fetch overran a frame", self.b.app(
            "journalctl -u bmac-backupd.service --no-pager -o cat 2>/dev/null | grep -c 'frame longer' || true") == "0")


def then(hosts) -> None:
    s = hosts["mox3"].s
    r = Restores(s, hosts)
    try:
        r.start()
        r.issue_grant()
        r.restore_test()
        restored = r.restore_new()
        r.promote(restored)
        r.continues()
        r.rollback()
        r.restore_into()
        r.cleanup()
        r.grown()
        r.reported()
    finally:
        s.note("the appliance's service log:\n" + r.b.app(
            "journalctl -u bmac-backupd.service --no-pager -o cat 2>/dev/null | tail -40 || true"))
        if (s.lab.paths.vm(r.b.appliance.name) / f"checkpoint-{backup.CHECKPOINT}.fd").exists():
            s.lab.restore_checkpoint(r.b.appliance, backup.CHECKPOINT)
        s.lab.forget_tailnet(names={r.b.tailnet_name}, required=False)


def run(s) -> None:
    storage.staged(s, then)
