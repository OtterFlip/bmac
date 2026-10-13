# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M7.2's join in the lab: mox3 starts as m7_storage leaves it, an encrypted
one-host cluster (the lab stage m7-first-host, built by running m7_storage's
setup when it isn't saved). mox3's
Dashboard API then issues a join bundle for mox2, the Deployer installs
mox2 from that bundle, and mox2 asks mox3 to let it join. Approved on mox3,
the hosts.join job sets mox2 up over SSH, relaying its questions, and keeps
going across mox2's boot drills when BMAC_LAB_DRILLS=1 is set (they are
declined otherwise, as in m7_storage). The root@pam password for
`pvecm add` is answered once, in the job, and must not appear in its events.

Starting from the stage (see m6.staged), mox3 gets the current bmacd
package instead of being installed and set up again. BMAC_LAB_FRESH=1
builds the stage again.

Both hosts are put back as they were afterwards. The root passwords are
made up for the run."""

from __future__ import annotations

import contextlib
import json
import os
import time
import uuid
from pathlib import Path

from bmaclab.api import Api, ApiError
from bmaclab.scenario import load

DESCRIPTION = "mox3 starts a cluster, issues a join bundle, and its hosts.join job sets up mox2 from blank to a quorate two-host cluster"
JOB_TIMEOUT = 5400
REQUEST_TIMEOUT = 900
TERMINAL = {"COMPLETED", "FAILED", "CANCELLED"}
SETUP_PHASES = ("identity", "settings", "storage", "drills", "network", "cluster", "retire",
                "orchestration", "headers", "verify")

m6 = load(Path(__file__).with_name("m6.py"))
m7 = load(Path(__file__).with_name("m7_storage.py"))


class Dashboard:
    """mox3's API through an SSH tunnel to its tailnet address, logged in
    as root@pam, logging in again when a restart drops the session."""

    def __init__(self, host) -> None:
        self.host = host
        self.address = host.on_host("tailscale ip -4 | head -n 1")
        self.fingerprint = host.on_host(
            "openssl x509 -in /etc/bmac/tls/https.pem -noout -fingerprint -sha256"
        ).split("=", 1)[1].replace(":", "").lower()
        self.token: str | None = None

    @contextlib.contextmanager
    def connected(self):
        with m7.Tunnel(self.host, self.address) as tunnel:
            api = Api(tunnel.local, self.fingerprint, self.host.vm.name)
            api.token = self.token
            if api.token is None:
                self._login(api)
            yield api

    def _login(self, api) -> None:
        # bmacd may still be starting after a restart.
        deadline = time.monotonic() + 180
        while True:
            try:
                api.login("root@pam", self.host.password)
                self.token = api.token
                return
            except OSError:
                if time.monotonic() > deadline:
                    raise
                time.sleep(3)

    def call(self, method: str, path: str, body: dict | None = None) -> dict:
        with self.connected() as api:
            try:
                return api.request(method, path, body)
            except ApiError as e:
                if e.status != 401:
                    raise
                self._login(api)
                return api.request(method, path, body)

    def start(self, operation: str, parameters: dict) -> str:
        return self.call("POST", "/jobs", {"operation": operation, "parameters": parameters,
                                           "idempotency_key": str(uuid.uuid4()), "dry_run": False})["id"]


def follow(s, dashboard: Dashboard, job: str, reply) -> tuple[dict, list[dict]]:
    """Follows a job to its end, answering each request with `reply(form)`."""
    events: list[dict] = []
    answered: set[str] = set()
    summary: dict = {}
    deadline = time.monotonic() + JOB_TIMEOUT
    while time.monotonic() < deadline and summary.get("state") not in TERMINAL:
        try:
            after = events[-1]["seq"] + 1 if events else 1
            view = dashboard.call("GET", f"/jobs/{job}?from_seq={after}")
        except ApiError:
            raise
        except Exception as e:  # mox3 is restarting
            s.note(f"lost mox3's Dashboard ({type(e).__name__}); retrying")
            time.sleep(10)
            continue
        events.extend(view["events"])
        summary = view["job"]
        for e in view["events"]:
            if e["type"] == "phase" and e.get("status") == "running":
                s.note(f"phase: {e.get('label')}")
            elif e["type"] in ("warning", "error"):
                s.note(f"{e['type']}: {e.get('message')}")
        request = summary.get("pending_request")
        if summary.get("state") not in TERMINAL and request and request not in answered:
            form = next(e for e in reversed(events) if e["type"] == "request" and e["request_id"] == request)
            s.note(f"request: {form.get('title')}")
            dashboard.call("POST", f"/jobs/{job}/requests/{request}", reply(form))
            answered.add(request)
        time.sleep(2)
    return summary, events


def phases_of(events: list[dict]) -> dict[str, str]:
    return {e["id"]: e.get("status") for e in events if e["type"] == "phase"}


def colonless(fingerprint: str | None) -> str:
    return (fingerprint or "").replace(":", "").lower()


def ready(s, dashboard: Dashboard) -> None:
    s.require("mox3 is a cluster member and issues join bundles",
              lambda: m7.wait_for(lambda: "joins" in dashboard.call("GET", "/joins")))


def issue(s, dashboard: Dashboard, first, host: str) -> tuple[Path, dict]:
    """Issues a join bundle for `host` on `first` and keeps it, private, in
    first's work folder."""
    issued = dashboard.call("POST", "/joins", {"host": host, "max_prod_vms": 1, "max_staging_vms": 2})
    bundle_file = first.work / issued["file_name"]
    fd = os.open(bundle_file, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as out:
        out.write(issued["content"])
    bundle = json.loads(issued["content"])
    urls = [h["dashboard_url"] for h in bundle["hosts"]]
    s.check(f"{host}'s bundle links {first.vm.name}'s Dashboard at its tailnet name ({' '.join(urls)})",
            len(urls) == 1 and urls[0].startswith(f"https://{first.host_name}.") and urls[0].endswith(":8443"))
    return bundle_file, bundle


def reserved(dashboard: Dashboard) -> list[str]:
    return sorted(j["host"] for j in dashboard.call("GET", "/joins")["joins"])


def then(mox3) -> None:
    s = mox3.s
    dashboard = Dashboard(mox3)
    ready(s, dashboard)
    bundle_file, bundle = issue(s, dashboard, mox3, "mox2")
    s.check("the bundle reserves mox2", lambda: reserved(dashboard) == ["mox2"])

    def join(mox2) -> None:
        await_request(s, dashboard, mox3, mox2)
        approve(s, dashboard, mox3, mox2)
        check_members(s, dashboard, [mox3, mox2], [bundle["setup_ssh_public_key"]])

    m6.install(s, m6.NVME_MIRROR, join, vm_name="mox2", bundle=bundle_file)


def await_request(s, dashboard: Dashboard, first, newcomer) -> None:
    """Waits for newcomer's join request on first and checks that it shows
    newcomer's fingerprints."""
    name = newcomer.vm.name

    def request():
        joins = dashboard.call("GET", "/joins")["joins"]
        return next((j["request"] for j in joins if j["host"] == name and j.get("request")), None)

    deadline = time.monotonic() + REQUEST_TIMEOUT
    pending = request()
    while pending is None and time.monotonic() < deadline:
        time.sleep(10)
        pending = request()
    s.require(f"{name} asks {first.vm.name} to join", pending is not None)
    ssh = newcomer.on_host("ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub").split()[1]
    https = newcomer.on_host("openssl x509 -in /etc/bmac/tls/https.pem -noout -fingerprint -sha256").split("=", 1)[1]
    s.check(f"the request shows {name}'s SSH host key", pending.get("ssh_host_key_fingerprint") == ssh)
    s.check(f"the request shows {name}'s certificate", colonless(pending.get("https_fingerprint")) == colonless(https))


def approve(s, dashboard: Dashboard, first, newcomer, job: str | None = None) -> None:
    """Approves newcomer's request on first (or follows `job`, the approval
    already started) and checks that hosts.join set it up."""
    name = newcomer.vm.name
    password_asked: list[str] = []

    def joining(form: dict) -> dict:
        kind, title = form["kind"], form.get("title", "")
        if kind == "confirm":
            return {"kind": "confirm", "confirmed": m7.DRILLS or title != "Run the boot drills?"}
        if kind == "manual_action":
            return {"kind": "acknowledge"}
        if kind == "input_group" and any(f["id"] == "password" for f in form.get("fields", [])):
            password_asked.append(title)
            return {"kind": "values", "values": {"password": first.password}}
        raise AssertionError(f"unexpected request {kind} {title!r}")

    summary, events = follow(s, dashboard, job or dashboard.start("hosts.join", {"host": name}), joining)
    s.check(f"{name}'s join completes ({summary.get('state')}: {summary.get('message') or ''})",
            summary.get("state") == "COMPLETED")
    phases = phases_of(events)
    for phase in SETUP_PHASES:
        s.check(f"{name}'s {phase} phase is complete", phases.get(phase) == "complete")
    s.check(f"the root@pam password was asked once for {name} ({len(password_asked)})", len(password_asked) == 1)
    s.check(f"the password is in none of {name}'s join events",
            first.password not in json.dumps(dashboard.call("GET", f"/jobs/{summary['id']}?from_seq=1")))
    s.check(f"{name}'s setup is marked complete", lambda: newcomer.on_host(
        "test -e /var/lib/bmac/setup-complete && echo done") == "done")
    s.check(f"bmacd runs on {name} after the join", lambda: newcomer.on_host(
        "systemctl is-active bmacd.service bmacd-web.service").split() == ["active", "active"])


def check_members(s, dashboard: Dashboard, members: list, setup_keys: list[str]) -> None:
    """Checks that `members` (the first host first) make up the cluster."""
    count = len(members)
    for host in members:
        name = host.vm.name
        s.check(f"{name} sees a quorate {count}-host cluster", lambda h=host: (
            lambda st: "Quorate:          Yes" in st and f"Nodes:            {count}" in st)(h.on_host("pvecm status")))
        s.check(f"{name} pins every host's private address in /etc/hosts", lambda h=host: all(
            any(line.split()[:1] == [m.vm.vlan_ip] and m.vm.name in line.split() for line in h.on_host("cat /etc/hosts").splitlines())
            for m in members))
    corosync = members[0].on_host("cat /etc/pve/corosync.conf")
    for host in members[1:]:
        s.check(f"corosync's first link for {host.vm.name} is its private address", f"ring0_addr: {host.vm.vlan_ip}" in corosync)
    for key in setup_keys:
        body = key.split()[1]
        for host in members:
            for path in ("/root/.ssh/authorized_keys", "/etc/pve/priv/authorized_keys"):
                s.check(f"setup key {body[-8:]} is gone from {host.vm.name}'s {path}", lambda h=host, p=path: h.on_host(
                    f"grep -cF '{body}' {p} || true") == "0")
    s.check("no reservation is left", lambda: reserved(dashboard) == [])


def run(s) -> None:
    m7.staged(s, then)
