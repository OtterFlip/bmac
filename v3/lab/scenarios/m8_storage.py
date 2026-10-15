# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M8 done-when in the lab, from the three-host cluster (m7_cluster.staged),
every storage change made through mox3's Dashboard API:

- mox2 becomes the raidz2 host: its four spare disks are added as one raidz2
  vdev, and a removal is then refused because the pool has raidz.
- mox1 becomes a mirror host: a mirror of its SATA disks, then one of its
  SCSI disks.
- mox3 (encrypted) gets an encrypted mirror of its SCSI disks, the
  passphrase typed at its console with `bmac luks-enter`.
- Two production VMs are created with the suggested block size: one placed
  on the mirror hosts mox1 and mox3 (16K, local-zfs), one on mox2 and mox3
  (64K, bmac-zfs-64k, because 16K and 32K blocks inflate on a 4-wide
  raidz2).
- The lab can't pull disks while they run, as FiberState's hardware can't,
  so replacements are exercised with Simulate a disk failure: a raidz2
  member on mox2 and an encrypted mirror member on mox3 are failed, erased,
  put back with Replace a disk, and resilvered.
- The SATA mirrors of mox1 and mox3 are removed (trimming and replicating
  the VMs first), and Storage inventory then retires them, closing mox3's
  LUKS mappings and taking them out of crypttab, and lists the disks as
  safe to pull.

The lab's disks are 16 GiB, and a removal must leave at least 50 GiB free,
so the SCSI disks of mox1 and mox3 are first grown to 96 GiB while the VMs
run (QMP block_resize, then a SCSI rescan); their files stay sparse.

When every check passes, the three hosts are saved together as the stage
m8-storage. BMAC_LAB_DRILLS=1 also restarts mox3 at the end, so its new
crypttab and initramfs unlock the pool at the console. Every host is put
back as it was afterwards."""

from __future__ import annotations

import json
import secrets
from pathlib import Path

from bmaclab.scenario import load

DESCRIPTION = "raidz2 and mirror hosts: vdevs added, failures simulated and replaced, mirrors removed, and VMs with suggested 16K and 64K disks, all from the Dashboard"

cluster = load(Path(__file__).with_name("m7_cluster.py"))
prod = load(Path(__file__).with_name("m4_prod.py"))
m6, m7, join = cluster.m6, cluster.m7, cluster.join

STAGE = "m8-storage"
HOSTS = cluster.HOSTS
GROWN_GIB = 96
RESILVER_TIMEOUT = 1800
REMOVAL_TIMEOUT = 3600

# On the host: each pool member's top-level vdev, path, state, and the
# serial of the disk under it (through any LUKS mapping and partition).
MEMBERS = r"""
import json, re, subprocess
out = subprocess.run(["zpool", "status", "-P", "rpool"], capture_output=True, text=True, check=True).stdout
rows, top, section = [], None, "data"
for line in out.splitlines():
    if not line.startswith("\t"):
        continue
    body = line[1:]
    depth = len(body) - len(body.lstrip(" "))
    fields = body.split()
    if not fields or fields[0] in ("NAME", "rpool"):
        continue
    if depth == 0:
        section = fields[0]
        continue
    if depth == 2:
        top = fields[0]
        if not fields[0].startswith("/dev/"):
            continue
    if not fields[0].startswith("/dev/"):
        continue
    serial = ""
    tree = subprocess.run(["lsblk", "-nso", "TYPE,SERIAL", fields[0]], capture_output=True, text=True).stdout
    for row in tree.splitlines():
        parts = row.split()
        if parts and parts[0] == "disk":
            serial = parts[1] if len(parts) > 1 else ""
    rows.append({"top": top, "section": section, "path": fields[0], "state": fields[1] if len(fields) > 1 else "", "serial": serial})
print(json.dumps(rows))
"""


def members(host) -> list[dict]:
    return json.loads(host.on_host(f"python3 -c {quote(MEMBERS)}"))


def quote(text: str) -> str:
    return "'" + text.replace("'", "'\\''") + "'"


def tops(host) -> dict[str, list[dict]]:
    found: dict[str, list[dict]] = {}
    for m in members(host):
        found.setdefault(m["top"], []).append(m)
    return found


def top_with(host, serials: list[str]) -> str | None:
    """The top-level vdev whose members are on exactly these disks."""
    for name, rows in tops(host).items():
        if sorted(r["serial"] for r in rows) == sorted(serials):
            return name
    return None


def spare(vm, bus: str) -> list:
    return [d for d in vm.disks if d.bus == bus]


def names(host) -> dict[str, str]:
    """Kernel names of the host's disks by serial."""
    found = {}
    for row in host.on_host("lsblk -dno NAME,SERIAL").splitlines():
        parts = row.split()
        if len(parts) == 2:
            found[parts[1]] = parts[0]
    return found


def on(option: dict, disk) -> bool:
    text = f"{option['value']} {option.get('label', '')}"
    return disk.serial in text or bool(disk.wwn and disk.wwn[2:] in text)


def options(form: dict, field: str) -> list[dict]:
    return next(f for f in form["fields"] if f["id"] == field).get("options", [])


def healthy(host) -> bool:
    return "is healthy" in host.on_host("zpool status -x rpool")


def grow(s, host) -> None:
    """Grows host's SCSI disks to GROWN_GIB while it runs."""
    lab, vm = s.lab, host.vm
    for disk in spare(vm, "scsi"):
        with lab.qmp(vm) as monitor:
            monitor.command("block_resize", device=f"d-{disk.id}", size=GROWN_GIB << 30)
        name = names(host).get(disk.serial)
        s.require(f"{vm.name} sees SCSI disk {disk.serial}", name is not None)
        host.on_host(f"echo 1 > /sys/block/{name}/device/rescan && udevadm settle")
        s.check(f"{vm.name}'s {name} is {GROWN_GIB} GiB", lambda name=name: m6.wait_until(lambda: int(
            host.on_host(f"blockdev --getsize64 /dev/{name}")) == GROWN_GIB << 30, 60))


class Operator:
    """Runs storage jobs on mox3's Dashboard, answering their forms as an
    operator would."""

    def __init__(self, s, mox3) -> None:
        self.s, self.mox3 = s, mox3
        self.dashboard = join.Dashboard(mox3)
        self.titles: list[str] = []

    def luks_enter(self, host) -> None:
        passphrase = host.carried["luks_passphrase"]
        with m7.console_login(self.s.lab, host.vm, host.password) as console:
            console.line("bmac luks-enter")
            console.expect(r"LUKS passphrase: ", timeout=30)
            console.line(passphrase)
            console.expect(r"Again: ", timeout=30)
            console.line(passphrase)
            console.expect(r"Stored for the job", timeout=30)
            console.line("exit")

    def run(self, operation: str, host, answers: dict | None = None) -> tuple[dict, list[dict]]:
        answers = answers or {}

        def reply(form: dict) -> dict:
            kind, title = form["kind"], form.get("title", "")
            self.titles.append(title)
            if kind == "confirm":
                return {"kind": "confirm", "confirmed": True}
            if kind == "manual_action":
                if title == "Type the LUKS passphrase at the console":
                    self.luks_enter(host)
                return {"kind": "acknowledge"}
            values = {}
            for field in form.get("fields") or [form["field"]]:
                answer = answers.get(field["id"])
                if callable(answer):
                    answer = answer(form)
                if answer is None:
                    answer = field.get("default")
                if answer is None:
                    raise AssertionError(f"no answer for {field['id']} in {title!r}")
                values[field["id"]] = answer
            return {"kind": "values", "values": values}

        summary, events = join.follow(self.s, self.dashboard,
                                      self.dashboard.start(operation, {"host": host.vm.name}), reply)
        return summary, events

    def done(self, what: str, operation: str, host, answers: dict | None = None) -> list[dict]:
        summary, events = self.run(operation, host, answers)
        self.s.check(f"{what} ({summary.get('state')}: {summary.get('message') or ''})",
                     summary.get("state") == "COMPLETED")
        return events


def result(events: list[dict]) -> dict:
    return next((e["data"] for e in reversed(events) if e["type"] == "result"), {})


def disks_answer(disks: list):
    return lambda form: [o["value"] for o in options(form, "disks") if any(on(o, d) for d in disks)]


def add_vdev(s, op: Operator, host, kind: str, disks: list) -> None:
    serials = [d.serial for d in disks]
    chosen = disks_answer(disks)
    op.done(f"{host.vm.name}: Add a vdev makes a {kind} of {', '.join(serials)}", "storage.add_vdev", host,
            {"class": "data", "type": kind, "disks": chosen})
    s.check(f"{host.vm.name}'s rpool has the new {kind}", lambda: (top_with(host, serials) or "").startswith(
        "mirror" if kind == "mirror" else kind))
    s.check(f"{host.vm.name}'s rpool is healthy", lambda: healthy(host))


def simulate_and_replace(s, op: Operator, host, disks: list) -> None:
    name = host.vm.name
    vdev = top_with(host, [d.serial for d in disks])
    s.require(f"{name} has a vdev of {', '.join(d.serial for d in disks)}", vdev is not None)
    victim = tops(host)[vdev][-1]
    s.note(f"failing {victim['path']} ({victim['serial']}) in {name}'s {vdev}")
    member = lambda form: next(o["value"] for o in options(form, "member") if victim["serial"] in o.get("label", "")
                               or o["value"] == victim["path"])
    op.done(f"{name}: Simulate a disk failure erases {victim['serial']}", "storage.simulate_failure", host,
            {"member": member, "choice": "erase"})
    s.check(f"{name}'s {vdev} runs degraded during the drill", lambda: "DEGRADED" in host.on_host("zpool status rpool"))
    disk = next(d for d in host.vm.disks if d.serial == victim["serial"])
    op.done(f"{name}: Replace a disk puts the erased disk back", "storage.replace_disk", host,
            {"disk": lambda form: next(o["value"] for o in options(form, "disk") if on(o, disk))})
    s.check(f"{name}'s pool resilvers and is healthy", lambda: m6.wait_until(lambda: healthy(host), RESILVER_TIMEOUT))
    s.check(f"{victim['serial']} is back in {name}'s {vdev}", lambda: victim["serial"] in
            [r["serial"] for r in tops(host).get(vdev, [])])
    events = op.done(f"{name}: Storage inventory finishes the drill", "storage.inventory", host)
    s.check(f"no disk of {name} is left to pull after the drill", result(events).get("safe_to_pull") == [])


def remove_mirror(s, op: Operator, host, disks: list) -> None:
    name = host.vm.name
    serials = [d.serial for d in disks]
    vdev = top_with(host, serials)
    s.require(f"{name} has a mirror of {', '.join(serials)}", vdev is not None)
    op.done(f"{name}: Remove a vdev starts removing {vdev}", "storage.remove_vdev", host, {"vdev": vdev})

    def gone() -> bool:
        status = host.on_host("zpool status rpool")
        return vdev not in tops(host) and "in progress" not in status

    s.check(f"ZFS finishes removing {name}'s {vdev}", lambda: m6.wait_until(gone, REMOVAL_TIMEOUT))
    kernel = names(host)
    events = op.done(f"{name}: Storage inventory retires {vdev}", "storage.inventory", host)
    pull = sorted(result(events).get("safe_to_pull") or [])
    s.check(f"{name}'s inventory says the removed disks can be pulled ({' '.join(pull)})",
            pull == sorted(f"/dev/{kernel[sn]}" for sn in serials))
    s.check(f"{name}'s rpool is healthy after the removal", lambda: m6.wait_until(lambda: healthy(host), 600))


def raise_limit(s, dashboard, host: str, limit: int) -> None:
    view = dashboard.call("GET", "/settings")
    record = next(h for h in view["hosts"] if h["host"] == host)
    if int(record["values"].get("MAX_PROD_VM_COUNT_ON_THIS_HOST", "0")) >= limit:
        return
    values = dict(record["values"], MAX_PROD_VM_COUNT_ON_THIS_HOST=str(limit))
    dashboard.call("PUT", f"/settings/hosts/{host}", {"values": values, "expected_revision": record["revision"]})


def create_vm(s, dashboard, purpose: str, domain: str, placement: list[str], initial: str) -> dict:
    password = secrets.token_urlsafe(18)
    answers = {"purpose": purpose, "prod_guest_vm_root_password": password, "initial_node": initial,
               "primary_domain": domain, "aliases": "", "placement": placement}
    storage_field: list[dict] = []

    def reply(form: dict) -> dict:
        if form["kind"] == "confirm":
            return {"kind": "confirm", "confirmed": True}
        if form["kind"] == "manual_action":
            raise AssertionError(f"unexpected manual step {form.get('title')!r}")
        values = {}
        for field in form.get("fields") or [form["field"]]:
            if field["id"] == "storage":
                storage_field.append(field)
            if field["id"] in answers:
                values[field["id"]] = answers[field["id"]]
            elif field.get("default") is not None:
                values[field["id"]] = field["default"]
            elif not field.get("required"):
                values[field["id"]] = ""
            else:
                raise AssertionError(f"no answer for {field['id']} in {form.get('title')!r}")
        return {"kind": "values", "values": values}

    summary, events = join.follow(s, dashboard, dashboard.start("prod.add", {}), reply)
    s.check(f"prod.add creates the {purpose} VM on {' and '.join(placement)} ({summary.get('state')}: "
            f"{summary.get('message') or ''})", summary.get("state") == "COMPLETED")
    s.check(f"the {purpose} VM's form defaults to the suggested block size",
            bool(storage_field) and storage_field[0].get("default") == "suggested")
    s.check(f"the {purpose} VM's root password is in none of its events",
            password not in json.dumps(dashboard.call("GET", f"/jobs/{summary['id']}?from_seq=1")))
    return {"events": events, "password": password}


def guest(host, domain: str) -> dict:
    guests = json.loads(host.on_host("bmac guests --json"))["guests"]
    return next((g for g in guests if g.get("primary_domain") == domain), {})


def root_disk(host, vmid: int) -> tuple[str, str]:
    """The VM's root disk storage and its zvol's volblocksize."""
    config = host.on_host(f"qm config {vmid}")
    line = next(l for l in config.splitlines() if l.startswith("scsi0: "))
    volume = line.split()[1].split(",")[0]
    path = host.on_host(f"pvesm path {volume}")
    block = host.on_host(f"zfs get -H -o value volblocksize {path.removeprefix('/dev/zvol/')}")
    return volume.split(":")[0], block


def vms(s, hosts, dashboard) -> None:
    prod.configure(s, dashboard)
    raise_limit(s, dashboard, "mox3", 2)
    for purpose, domain, placement, initial, storage, block in (
        ("small", "small.lab.internal", ["mox1", "mox3"], "mox1", "local-zfs", "16K"),
        ("bulk", "bulk.lab.internal", ["mox2", "mox3"], "mox2", "bmac-zfs-64k", "64K"),
    ):
        owner = hosts[initial]
        prod.seed_iso(s, owner)
        create_vm(s, dashboard, purpose, domain, placement, initial)
        vm = guest(owner, domain)
        s.check(f"the {purpose} VM runs on {initial}", vm.get("node") == initial and vm.get("status") == "running")
        if vm.get("vmid"):
            found = root_disk(owner, vm["vmid"])
            s.check(f"the {purpose} VM's root disk is on {storage} with {block} blocks ({found[0]}, {found[1]})",
                    found == (storage, block))
            other = hosts["mox3"]
            s.check(f"the {purpose} VM's replica on mox3 keeps {block} blocks", lambda: m6.wait_until(
                lambda: other.on_host(f"zfs list -H -o name,volblocksize -t volume -r rpool | grep -- '-{vm['vmid']}-disk-' "
                                      f"| awk '{{print $2}}' | sort -u") == block, 900))


def then(hosts) -> None:
    mox1, mox2, mox3 = hosts["mox1"], hosts["mox2"], hosts["mox3"]
    s = mox3.s
    failed = s.failed
    op = Operator(s, mox3)
    join.ready(s, op.dashboard)

    for host in (mox1, mox3):
        grow(s, host)

    # The raidz2 host.
    add_vdev(s, op, mox2, "raidz2", spare(mox2.vm, "sata") + spare(mox2.vm, "scsi"))
    summary, _ = op.run("storage.remove_vdev", mox2)
    s.check(f"mox2 refuses every vdev removal once it has raidz ({summary.get('message')})",
            summary.get("state") == "FAILED" and "No vdev can be removed" in (summary.get("message") or ""))

    # The mirror hosts.
    add_vdev(s, op, mox1, "mirror", spare(mox1.vm, "sata"))
    add_vdev(s, op, mox1, "mirror", spare(mox1.vm, "scsi"))
    add_vdev(s, op, mox3, "mirror", spare(mox3.vm, "scsi"))
    s.check("mox3's new members are LUKS mappings", lambda: all(
        r["path"].startswith("/dev/mapper/bmac-rpool-") for r in tops(mox3)[top_with(mox3, [d.serial for d in spare(mox3.vm, "scsi")])]))
    s.check("mox3's crypttab lists its six members", lambda: mox3.on_host("grep -c '^bmac-rpool-' /etc/crypttab") == "6")
    s.check("the passphrase file is gone from mox3", lambda: mox3.on_host(
        "test ! -e /run/bmac/luks-passphrase && echo gone") == "gone")

    vms(s, hosts, op.dashboard)

    simulate_and_replace(s, op, mox2, spare(mox2.vm, "sata") + spare(mox2.vm, "scsi"))
    simulate_and_replace(s, op, mox3, spare(mox3.vm, "scsi"))

    remove_mirror(s, op, mox1, spare(mox1.vm, "sata"))
    remove_mirror(s, op, mox3, spare(mox3.vm, "sata"))
    s.check("mox3's crypttab lists its four members", lambda: mox3.on_host("grep -c '^bmac-rpool-' /etc/crypttab") == "4")
    s.check("mox3 has no stray LUKS mapping", lambda: mox3.on_host(
        "ls /dev/mapper | grep -c '^bmac-rpool-' || true") == "4")

    if m7.DRILLS:
        before = m7.boot_id(mox3)
        with m6.booting(s, lambda: m7.unlock_at_boot(s.lab, mox3.vm, mox3.carried["luks_passphrase"])):
            mox3.on_host("systemd-run --on-active=2s --collect /usr/bin/systemctl reboot")
            s.require("mox3 restarts", lambda: m7.wait_back(mox3, before))
        s.check("mox3's pool comes back whole after the restart", lambda: m6.wait_until(lambda: healthy(mox3), 300))
    else:
        s.note("mox3 isn't restarted; BMAC_LAB_DRILLS=1 proves its crypttab and initramfs at boot")

    if s.failed == failed:
        m6.save_cluster_stage(s, [hosts[name] for name in HOSTS], STAGE)


def staged(s, then_) -> None:
    """Carries on with `then_({name: host})` from STAGE when every host has
    it, built otherwise."""
    if m6.has_stage(s.lab, HOSTS, STAGE):
        m6.reuse_cluster(s, HOSTS, STAGE, then_, unlock=m7.unlock_at_boot)
    else:
        cluster.staged(s, lambda hosts: (then(hosts), then_(hosts)))


def run(s) -> None:
    cluster.staged(s, then)
