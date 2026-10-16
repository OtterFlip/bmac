# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M10's done-when in the lab, from the cluster with production VMs
(m8_storage.staged), every step through mox3's Dashboard API. The job
updates the other hosts by slot and the host it runs on last, and shows
that order before it starts:

- Each host keeps the bundle the host ISO installed it from as its
  current one, so it has a bundle to roll back to before any update.
- Two development builds of this source, VERSION+b1 and VERSION+b2, are
  built as bundles signed with the development key (target/m10/).
- b1 is uploaded in chunks and the cluster updated to it, one host at a
  time, with the production VMs running throughout.
- b2 is uploaded, and once the job has shown its order, the second host's
  bmac-source is masked and stopped, so that host installs b2 but fails
  its health check (masking alone isn't enough: systemd drops the restart
  bmacd's restart would pass to a masked unit, and the old process runs
  on). The update stops there: the first host runs b2 and the third is
  untouched on b1. The second host's bmac-source is unmasked and the job's
  offered rollback returns the first two hosts to b1.
- The production VMs never restart: each keeps its QEMU process.

The masked bmac-source is unmasked whatever happens."""

from __future__ import annotations

import base64
import os
import subprocess
from pathlib import Path

from bmaclab.scenario import load

DESCRIPTION = "Cluster updates: seeded bundles, a chunked upload, a rolling update, a failure on the second host, and the rollback"

storage = load(Path(__file__).with_name("m8_storage.py"))
m6, m7, join = storage.m6, storage.m7, storage.join

V3 = Path(__file__).resolve().parent.parent.parent
BASE = (V3 / "dev" / "VERSION").read_text(encoding="utf-8").strip()
BUILDS = ("b1", "b2")
WORK = V3 / "target" / "m10"
HOSTS = storage.HOSTS
GUESTS = (("small", "small.lab.internal"), ("bulk", "bulk.lab.internal"))
ROLLBACK = "Roll back the updated hosts?"
ORDER = "Order: "
SERVICES = ("bmacd.service", "bmacd-web.service", "bmac-source.service")


def debian(version: str) -> str:
    return version.replace("-", "~", 1)


def stale(path: Path) -> bool:
    if not path.exists():
        return True
    newest = max(p.stat().st_mtime for sub in m6.PACKAGE_SOURCES for p in (V3 / sub).rglob("*") if p.is_file())
    return newest > path.stat().st_mtime


def build(s, name: str) -> Path:
    """VERSION+name's bundle, built again when a source is newer, in its own
    target directory so the lab's package is left alone."""
    version = f"{BASE}+{name}"
    out = WORK / name
    bundle = out / f"bmac-{version}.bundle"
    if stale(bundle):
        out.mkdir(parents=True, exist_ok=True)
        s.note(f"building {version} (log: {out / 'build.log'})")
        with open(out / "build.log", "w") as log:
            subprocess.run([str(V3 / "dev" / "build_deb.py"), "--version", version, "--bundle",
                            "--out", str(out / "packages"), "--appliance-out", str(out / "appliance"),
                            "--bundle-out", str(out)],
                           cwd=V3, env=dict(os.environ, CARGO_TARGET_DIR=str(WORK / "cargo")),
                           stdout=log, stderr=subprocess.STDOUT, check=False)
    s.require(f"{version} is built as a signed bundle", lambda: bundle.is_file() and not stale(bundle))
    return bundle


def upload(dashboard, bundle: Path) -> dict:
    data = bundle.read_bytes()
    with dashboard.connected() as api:
        started = api.request("POST", "/updates/uploads", {"size": len(data)})
        upload_id, chunk = started["upload_id"], started["chunk"]
        for offset in range(0, len(data), chunk):
            api.request("POST", f"/updates/uploads/{upload_id}",
                        {"offset": offset, "data": base64.b64encode(data[offset:offset + chunk]).decode()})
        return api.request("POST", f"/updates/uploads/{upload_id}/finish", {})


def job(s, dashboard, operation: str, answers: dict, confirm=None) -> tuple[dict, list[str]]:
    """Runs operation to its end, answering each field from answers or its
    default and each confirmation with confirm(form) (yes by default); its
    summary and every confirmation's details."""
    details: list[str] = []

    def reply(form: dict) -> dict:
        if form["kind"] == "confirm":
            details.extend(form.get("details") or [])
            return {"kind": "confirm", "confirmed": confirm(form) if confirm else True}
        if form["kind"] == "manual_action":
            raise AssertionError(f"unexpected manual step {form.get('title')!r}")
        values = {}
        for field in form.get("fields") or [form["field"]]:
            if field["id"] in answers:
                values[field["id"]] = answers[field["id"]]
            elif field.get("default") is not None:
                values[field["id"]] = field["default"]
            elif not field.get("required"):
                values[field["id"]] = ""
            else:
                raise AssertionError(f"no answer for {field['id']} in {form.get('title')!r}")
        return {"kind": "values", "values": values}

    summary, events = join.follow(s, dashboard, dashboard.start(operation, {}), reply)
    if summary.get("state") != "COMPLETED":
        for e in [e for e in events if e["type"] == "output"][-8:]:
            s.note(f"output: {e.get('text')}")
    for d in details:
        s.note(f"confirm: {d}")
    return summary, details


def package(host) -> str:
    return host.on_host("dpkg-query -W -f '${Version}' bmacd")


def views(dashboard) -> dict[str, dict]:
    return {h["host"]: h for h in dashboard.call("GET", "/updates")["hosts"]}


def installed(view: dict, which: str) -> str | None:
    found = (view.get("installed") or {}).get(which)
    return found["version"] if found else None


def guests(hosts) -> dict[str, tuple[str, int, str]]:
    """Each production VM's node, VMID, and QEMU process."""
    found = {}
    for purpose, domain in GUESTS:
        vm = storage.guest(hosts["mox3"], domain)
        if vm.get("node") in hosts and vm.get("vmid"):
            node = hosts[vm["node"]]
            pid = node.on_host(f"cat /run/qemu-server/{vm['vmid']}.pid 2>/dev/null || true")
            if pid and vm.get("status") == "running":
                found[purpose] = (vm["node"], vm["vmid"], pid)
    return found


def kept_running(s, hosts, before: dict, when: str) -> None:
    for purpose, (node, vmid, pid) in before.items():
        host = hosts[node]
        s.check(f"the {purpose} VM kept running on {node} {when} (QEMU {pid})", lambda: (
            host.on_host(f"cat /run/qemu-server/{vmid}.pid 2>/dev/null || true") == pid
            and host.on_host(f"qm status {vmid}") == "status: running"))


def on_build(s, hosts, version: str, names: tuple[str, ...]) -> None:
    for name in names:
        s.check(f"{name} runs {version}", lambda: package(hosts[name]) == debian(version))


def healthy(host) -> bool:
    return all(host.on_host(f"systemctl is-active {u} || true") == "active" for u in SERVICES)


def order_of(form: dict) -> list[str]:
    line = next((d for d in form.get("details") or [] if d.startswith(ORDER)), "")
    return line.removeprefix(ORDER).split(", ") if line else []


def unmask(host) -> None:
    host.on_host("systemctl unmask --runtime bmac-source.service; systemctl start bmac-source.service || true")


def quorate(host) -> bool:
    return "Yes" in host.on_host("pvecm status 2>/dev/null | grep '^Quorate:' || true")


def then(hosts, bundles: dict[str, Path]) -> None:
    mox3 = hosts["mox3"]
    s = mox3.s
    # In the NVMe lab a stage boots when restored, so hosts and HA's guests
    # may still be starting. Logins fail until the cluster is up, and each
    # counts against the login limit, so the API waits until it is.
    s.require("every host is quorate with its BMAC services running", lambda: m6.wait_until(
        lambda: all(quorate(hosts[name]) and healthy(hosts[name]) for name in HOSTS), 600))
    dashboard = join.Dashboard(mox3)
    join.ready(s, dashboard)
    b1, b2 = f"{BASE}+b1", f"{BASE}+b2"
    s.require("every host reports its version and bundles", lambda: m6.wait_until(
        lambda: all(v.get("version") and not v.get("error") for v in views(dashboard).values())
        and sorted(views(dashboard)) == sorted(HOSTS), 600))
    s.require("both production VMs are running", lambda: m6.wait_until(
        lambda: sorted(guests(hosts)) == sorted(p for p, _ in GUESTS), 900))
    seeded = views(dashboard)
    for name in HOSTS:
        s.check(f"{name} keeps the bundle it was installed from ({installed(seeded.get(name, {}), 'current')})",
                installed(seeded.get(name, {}), "current") == BASE)

    running = guests(hosts)

    first = upload(dashboard, bundles["b1"])
    s.check(f"b1 is uploaded in chunks and checked ({first.get('version')}, signed by {first.get('signed_by')})",
            first.get("version") == b1 and first.get("signed_by") == "development")
    shown: list[list[str]] = []
    summary, _ = job(s, dashboard, "cluster.update", {"bundle": first["sha256"], "url": "", "migrate": False},
                     lambda form: shown.append(order_of(form)) or True)
    s.check(f"cluster.update takes every host to b1 ({summary.get('state')}: {summary.get('message') or ''})",
            summary.get("state") == "COMPLETED")
    order = next((o for o in shown if o), [])
    s.check(f"the update's order covers every host once ({', '.join(order)})", sorted(order) == sorted(HOSTS))
    on_build(s, hosts, b1, HOSTS)
    after = views(dashboard)
    for name in HOSTS:
        view = after.get(name, {})
        s.check(f"{name} has b1 installed and keeps its first bundle for a rollback",
                installed(view, "current") == b1 and installed(view, "previous") == BASE)
        s.check(f"{name} is healthy on b1", lambda: m6.wait_until(lambda: healthy(hosts[name]), 120))
    kept_running(s, hosts, running, "through the update to b1")

    second = upload(dashboard, bundles["b2"])
    s.check("b2 is uploaded and checked", second.get("version") == b2)
    planned: list[str] = []
    stopped: dict[str, bool] = {}

    def answer(form: dict) -> bool:
        if order_of(form) and not planned:
            planned.extend(order_of(form))
            if len(planned) == 3:
                hosts[planned[1]].on_host("systemctl mask --runtime --now bmac-source.service")
        elif form.get("title") == ROLLBACK and len(planned) == 3:
            first_, second_, third = (hosts[name] for name in planned)
            stopped["first"] = package(first_) == debian(b2)
            stopped["second"] = package(second_) == debian(b2)
            stopped["third"] = package(third) == debian(b1)
            stopped["second unhealthy"] = second_.on_host("systemctl is-active bmac-source.service || true") != "active"
            unmask(second_)
        return True

    try:
        summary, _ = job(s, dashboard, "cluster.update", {"bundle": second["sha256"], "url": "", "migrate": False},
                         answer)
    finally:
        for name in planned[1:2]:
            unmask(hosts[name])
    s.require(f"the update to b2 shows an order of three hosts ({', '.join(planned)})", len(planned) == 3)
    first_name, second_name, third_name = planned
    message = summary.get("message") or ""
    s.check(f"the update to b2 fails at {second_name}, the second host ({summary.get('state')}: {message})",
            summary.get("state") == "FAILED" and second_name in message)
    s.check("the job offered to roll back the updated hosts", bool(stopped))
    s.check(f"when the update stopped, {first_name}, the first host, ran b2", stopped.get("first") is True)
    s.check(f"when the update stopped, {second_name} had installed b2 and its bmac-source was down",
            stopped.get("second") is True and stopped.get("second unhealthy") is True)
    s.check(f"when the update stopped, {third_name}, the third host, was untouched on b1", stopped.get("third") is True)
    on_build(s, hosts, b1, HOSTS)
    after = views(dashboard)
    for name in (first_name, second_name):
        view = after.get(name, {})
        s.check(f"{name} is rolled back to b1 and keeps b2 for a second try",
                installed(view, "current") == b1 and installed(view, "previous") == b2)
    s.check(f"{third_name} still has b1 installed after its first bundle",
            installed(after.get(third_name, {}), "current") == b1
            and installed(after.get(third_name, {}), "previous") == BASE)
    for name in HOSTS:
        s.check(f"{name} is healthy after the rollback", lambda: m6.wait_until(lambda: healthy(hosts[name]), 120))
    kept_running(s, hosts, running, "through the failed update and the rollback")


def run(s) -> None:
    bundles = {name: build(s, name) for name in BUILDS}
    storage.staged(s, lambda hosts: then(hosts, bundles))
