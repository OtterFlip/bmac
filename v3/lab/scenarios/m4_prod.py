# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M4's production VM in the lab: from the three-host cluster
(m7_cluster.staged), prod.add runs from mox3's Dashboard and creates prod1
with Ubuntu installed unattended on mox1, replicated to mox2 and mox3, and
under HA. The guest agent attests its SSH host key (`bmac guest-host-key`).
The production VM diagnostic then runs on it, from the stage once it is
saved.

The Ubuntu live-server ISO is downloaded once into the labs' shared folder
and copied into mox1's ISO cache, where prod.add verifies it as it would
its own download. Its SHA-256 is the one in Ubuntu's SHA256SUMS, whose
signature by the Ubuntu CD Image signing key was checked when it was
pinned here.

When every check passes, the three hosts are saved together as the stage
m4-prod, which later scenarios start from (see staged). Every host is put
back as it was afterwards."""

from __future__ import annotations

import json
import secrets
import subprocess
from pathlib import Path

from bmaclab import provision
from bmaclab.scenario import load

DESCRIPTION = "prod.add installs prod1 on mox1 from the Dashboard, replicated to mox2 and mox3 and under HA"

cluster = load(Path(__file__).with_name("m7_cluster.py"))
m6 = cluster.m6
join = cluster.join

ISO_URL = "https://releases.ubuntu.com/26.04.1/ubuntu-26.04.1-live-server-amd64.iso"
ISO_SHA256 = "cc8a95cde20f6ced61a322420de00f10cc3c90ced545daa46cb9c1a117f1d927"
ISO_CACHE = "/var/lib/bmac/iso-cache"
STAGE = "m4-prod"
HOSTS = cluster.HOSTS
PURPOSE = "demo"
DOMAIN = "demo.lab.internal"


def shared(lab) -> Path:
    """Beside every lab's own folder, for what all the labs download."""
    return lab.paths.root.parent / "shared"


def second_admin_key(lab) -> str:
    key = shared(lab) / "admin2_ed25519"
    if not key.exists():
        key.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "bmac-lab-admin2", "-f", str(key)],
                       check=True, stdin=subprocess.DEVNULL)
    return key.with_name(key.name + ".pub").read_text().strip()


def configure(s, dashboard) -> None:
    view = dashboard.call("GET", "/settings")
    record = view["cluster"]
    values = dict(record["values"], PROD_GUEST_OS_ISO_URL=ISO_URL, PROD_GUEST_OS_ISO_SHA256=ISO_SHA256,
                  ADMIN_2_PUBLIC_SSH_KEY=second_admin_key(s.lab))
    dashboard.call("PUT", "/settings/cluster", {"values": values, "expected_revision": record["revision"]})
    s.check("the cluster settings name the guest ISO and both admin keys", lambda: all(
        dashboard.call("GET", "/settings")["cluster"]["values"].get(k) for k in
        ("PROD_GUEST_OS_ISO_URL", "PROD_GUEST_OS_ISO_SHA256", "ADMIN_1_PUBLIC_SSH_KEY", "ADMIN_2_PUBLIC_SSH_KEY")))


def seed_iso(s, host) -> None:
    iso = provision.fetch(ISO_URL, ISO_SHA256, shared(s.lab), log=s.note)
    target = f"{ISO_CACHE}/{ISO_SHA256}.iso"
    with iso.open("rb") as source:
        done = subprocess.run([*host.ssh_args(), f"install -d -o root -g root -m 0700 {ISO_CACHE}"
                               f" && cat > {target}.partial && chmod 0600 {target}.partial && mv {target}.partial {target}"],
                              stdin=source, capture_output=True, text=True, timeout=1800)
    s.require(f"the Ubuntu ISO is in {host.vm.name}'s ISO cache ({done.stderr.strip()})", done.returncode == 0)


def build(s, after=None) -> None:
    """Creates prod1 from the cluster and saves STAGE when every check
    passes, then carries on with `after({name: host})`."""

    def then(hosts) -> None:
        failed = s.failed
        mox1, mox3 = hosts["mox1"], hosts["mox3"]
        dashboard = join.Dashboard(mox3)
        configure(s, dashboard)
        seed_iso(s, mox1)
        password = secrets.token_urlsafe(18)
        add(s, dashboard, password)
        check_prod(s, hosts)
        if s.failed != failed:
            return
        for name in HOSTS:
            hosts[name].carried["prod_root_password"] = password
        m6.save_cluster_stage(s, [hosts[name] for name in HOSTS], STAGE)
        if after is not None:
            after(hosts)

    cluster.staged(s, then)


def staged(s, then) -> None:
    """Carries on with `then({name: host})` from the cluster with prod1, from
    STAGE when every host has it, built otherwise."""
    if m6.has_stage(s.lab, HOSTS, STAGE):
        m6.reuse_cluster(s, HOSTS, STAGE, then, unlock=cluster.m7.unlock_at_boot)
    else:
        build(s, then)


def add(s, dashboard, password: str) -> None:
    answers = {"purpose": PURPOSE, "prod_guest_vm_root_password": password, "initial_node": "mox1",
               "primary_domain": DOMAIN, "aliases": ""}
    asked: list[str] = []

    def reply(form: dict) -> dict:
        asked.append(form.get("title") or form.get("kind"))
        if form["kind"] == "confirm":
            return {"kind": "confirm", "confirmed": True}
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
                raise AssertionError(f"no answer for {field['id']} ({field.get('label')}) in {form.get('title')!r}")
        return {"kind": "values", "values": values}

    summary, events = join.follow(s, dashboard, dashboard.start("prod.add", {}), reply)
    s.check(f"prod.add completes ({summary.get('state')}: {summary.get('message') or ''})",
            summary.get("state") == "COMPLETED")
    s.check("the VM's root password is in none of prod.add's events",
            password not in json.dumps(dashboard.call("GET", f"/jobs/{summary['id']}?from_seq=1")))


def prod1(host) -> dict:
    guests = json.loads(host.on_host("bmac guests --json"))["guests"]
    return next((g for g in guests if g["name"] == "prod1"), {})


def check_prod(s, hosts) -> None:
    mox1 = hosts["mox1"]
    for name, host in hosts.items():
        s.check(f"{name}'s bmac guests lists prod1 running on mox1, placed on all three hosts", lambda host=host: (
            (g := prod1(host)).get("kind") == "production" and g.get("node") == "mox1"
            and g.get("status") == "running" and sorted(g.get("placement", [])) == HOSTS_SORTED
            and g.get("primary_domain") == DOMAIN))
    vmid = prod1(mox1).get("vmid")
    s.check(f"HA has prod1 (vm:{vmid}) started", lambda: m6.wait_until(
        lambda: f"service vm:{vmid} (mox1, started)" in mox1.on_host("ha-manager status"), 300))
    for target in ("mox2", "mox3"):
        s.check(f"prod1 replicates to {target}", lambda target=target: m6.wait_until(lambda: any(
            # JobID Enabled Target LastSync NextSync Duration FailCount State
            line.split()[0].startswith(f"{vmid}-") and line.split()[2] == f"local/{target}" and line.split()[-1] == "OK"
            for line in mox1.on_host("pvesr status").splitlines()[1:] if len(line.split()) > 3), 900))
    key = {}
    s.check("bmac guest-host-key attests prod1's Ed25519 host key through the guest agent", lambda: (
        key.update(json.loads(mox1.on_host("bmac guest-host-key prod1 --json")))
        or (key.get("key", "").startswith("ssh-ed25519 ") and key.get("fingerprint", "").startswith("SHA256:")
            and key.get("node") == "mox1")))


def diagnose(s, hosts) -> None:
    dashboard = join.Dashboard(hosts["mox3"])
    summary, events = join.follow(s, dashboard, dashboard.start("diagnostics.prod_vm_state", {"resource": "prod1"}),
                                  lambda form: {"kind": "confirm", "confirmed": True})
    failed = []
    for e in events:
        if e["type"] == "phase" and e.get("label", "").startswith("Hello application"):
            break  # informational: the Deployer deploys the hello app, not prod.add
        if e["type"] == "output" and "[FAIL]" in e.get("text", ""):
            failed.append(e["text"].strip())
    s.check(f"the production VM diagnostic passes on prod1 ({summary.get('state')}"
            + "".join(f"; {line}" for line in failed) + ")", summary.get("state") == "COMPLETED")


HOSTS_SORTED = sorted(HOSTS)


def run(s) -> None:
    staged(s, lambda hosts: diagnose(s, hosts))
