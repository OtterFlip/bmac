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


def then(mox3) -> None:
    s = mox3.s
    dashboard = Dashboard(mox3)
    s.require("mox3 is a cluster member and issues join bundles",
              lambda: m7.wait_for(lambda: "joins" in dashboard.call("GET", "/joins")))

    issued = dashboard.call("POST", "/joins", {"host": "mox2", "max_prod_vms": 1, "max_staging_vms": 2})
    bundle_file = mox3.work / issued["file_name"]
    fd = os.open(bundle_file, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as out:
        out.write(issued["content"])
    bundle = json.loads(issued["content"])
    urls = [h["dashboard_url"] for h in bundle["hosts"]]
    s.check(f"the bundle links mox3's Dashboard at its tailnet name ({' '.join(urls)})",
            len(urls) == 1 and urls[0].startswith(f"https://{mox3.host_name}.") and urls[0].endswith(":8443"))
    s.check("the bundle reserves mox2", lambda: [j["host"] for j in dashboard.call("GET", "/joins")["joins"]] == ["mox2"])

    m6.install(s, m6.NVME_MIRROR, lambda mox2: join(s, dashboard, mox3, mox2, bundle["setup_ssh_public_key"]), vm_name="mox2", bundle=bundle_file)


def join(s, dashboard: Dashboard, mox3, mox2, setup_key: str) -> None:
    def request():
        joins = dashboard.call("GET", "/joins")["joins"]
        return next((j["request"] for j in joins if j["host"] == "mox2" and j.get("request")), None)

    deadline = time.monotonic() + REQUEST_TIMEOUT
    pending = request()
    while pending is None and time.monotonic() < deadline:
        time.sleep(10)
        pending = request()
    s.require("mox2 asks mox3 to join", pending is not None)
    ssh = mox2.on_host("ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub").split()[1]
    https = mox2.on_host("openssl x509 -in /etc/bmac/tls/https.pem -noout -fingerprint -sha256").split("=", 1)[1]
    s.check("the request shows mox2's SSH host key", pending.get("ssh_host_key_fingerprint") == ssh)
    s.check("the request shows mox2's certificate", colonless(pending.get("https_fingerprint")) == colonless(https))

    password_asked: list[str] = []

    def joining(form: dict) -> dict:
        kind, title = form["kind"], form.get("title", "")
        if kind == "confirm":
            return {"kind": "confirm", "confirmed": m7.DRILLS or title != "Run the boot drills?"}
        if kind == "manual_action":
            return {"kind": "acknowledge"}
        if kind == "input_group" and any(f["id"] == "password" for f in form.get("fields", [])):
            password_asked.append(title)
            return {"kind": "values", "values": {"password": mox3.password}}
        raise AssertionError(f"unexpected request {kind} {title!r}")

    summary, events = follow(s, dashboard, dashboard.start("hosts.join", {"host": "mox2"}), joining)
    s.check(f"the join completes ({summary.get('state')}: {summary.get('message') or ''})",
            summary.get("state") == "COMPLETED")
    phases = phases_of(events)
    for phase in SETUP_PHASES:
        s.check(f"mox2's {phase} phase is complete", phases.get(phase) == "complete")
    s.check(f"the root@pam password was asked once ({len(password_asked)})", len(password_asked) == 1)
    s.check("the password is in none of the job's events",
            mox3.password not in json.dumps(dashboard.call("GET", f"/jobs/{summary['id']}?from_seq=1")))

    for host in (mox3, mox2):
        name = host.vm.name
        s.check(f"{name} sees a quorate two-host cluster", lambda h=host: (
            lambda st: "Quorate:          Yes" in st and "Nodes:            2" in st)(h.on_host("pvecm status")))
        s.check(f"{name} pins both hosts' private addresses in /etc/hosts", lambda h=host: all(
            any(line.split()[:1] == [vm.vlan_ip] and vm.name in line.split() for line in h.on_host("cat /etc/hosts").splitlines())
            for vm in (mox3.vm, mox2.vm)))
    s.check("corosync's first link for mox2 is its private address",
            lambda: f"ring0_addr: {mox2.vm.vlan_ip}" in mox3.on_host("cat /etc/pve/corosync.conf"))
    body = setup_key.split()[1]
    for host in (mox2, mox3):
        for path in ("/root/.ssh/authorized_keys", "/etc/pve/priv/authorized_keys"):
            s.check(f"the setup key is gone from {host.vm.name}'s {path}", lambda h=host, p=path: h.on_host(
                f"grep -cF '{body}' {p} || true") == "0")
    s.check("mox2's setup is marked complete", lambda: mox2.on_host(
        "test -e /var/lib/bmac/setup-complete && echo done") == "done")
    s.check("the reservation is gone", lambda: dashboard.call("GET", "/joins")["joins"] == [])
    s.check("bmacd runs on mox2 after the join", lambda: mox2.on_host(
        "systemctl is-active bmacd.service bmacd-web.service").split() == ["active", "active"])


def run(s) -> None:
    m7.staged(s, then)
