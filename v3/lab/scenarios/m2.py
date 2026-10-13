# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M2's done-when check (IMPLEMENTATION_PLAN.md): three lab hosts run bmacd;
a user logs in on each with TOTP; a mock operation started on one host runs
on another, and its live output and requests, followed from a third host,
are answered there until it completes."""

from __future__ import annotations

import contextlib
import json
import time
import uuid

from bmaclab.api import Api, Codes, Tunnel, pinned_fingerprint
from bmaclab.bootstrap import Bootstrap
from bmaclab.lab import BOOT_TIMEOUT

DESCRIPTION = "TOTP logins on every host; a job started on mox1 runs on mox2 and is answered from mox3"
TERMINAL = {"COMPLETED", "FAILED", "CANCELLED"}


def wait_for(condition, timeout: float) -> bool:
    deadline = time.monotonic() + timeout
    while not condition():
        if time.monotonic() > deadline:
            return False
        time.sleep(1)
    return True


def run(s) -> None:
    lab = s.lab
    hosts = lab.topology.hosts
    s.require("the lab has three hosts", len(hosts) >= 3)
    origin, runner, watcher = hosts[0], hosts[1], hosts[2]
    for vm in hosts:
        s.require(f"{vm.name} answers SSH", lambda vm=vm: lab.wait_ssh(vm, BOOT_TIMEOUT) or True)
    users_file = Bootstrap(lab).users_file
    s.require("the lab was bootstrapped (lab/lab bootstrap)", users_file.exists)
    users = json.loads(users_file.read_text())
    codes = Codes(users["alice_totp_secret"])
    password = users["users"]["alice@pve"]["password"]

    for vm in hosts:
        s.check(f"bmacd and its front end run on {vm.name}", lambda vm=vm: lab.ssh(vm, "systemctl is-active bmacd bmacd-web").returncode == 0)

    with contextlib.ExitStack() as stack:
        apis = {}
        for vm in hosts:
            tunnel = stack.enter_context(Tunnel(lab, vm))
            apis[vm.name] = Api(tunnel.local, pinned_fingerprint(lab, vm), vm.name)
        for vm in hosts:
            api = apis[vm.name]
            s.require(
                f"alice logs in on {vm.name} with her password and a TOTP code",
                lambda api=api: api.login("alice@pve", password, codes)["role"] == "admin",
            )
            stack.callback(api.logout)

        def leftovers() -> list[dict]:
            listed = apis[origin.name].request("GET", "/jobs")
            jobs = listed["jobs"] if isinstance(listed, dict) else listed
            return [j for j in jobs if j["operation"].startswith("lab.") and j["state"] in ("ADMITTED", "RUNNING")]

        for old in leftovers():
            apis[origin.name].request("POST", f"/jobs/{old['id']}/cancel", {"force": False})
        s.require("no lab job from an earlier run is unfinished", lambda: wait_for(lambda: not leftovers(), 60))

        started = apis[origin.name].request("POST", "/jobs", {
            "operation": "lab.interactive",
            "parameters": {"host": runner.name},
            "idempotency_key": str(uuid.uuid4()),
            "dry_run": False,
        })
        job = started["id"]
        s.require(f"a job started on {origin.name} targets {runner.name}", started["host"] == runner.name, detail=json.dumps(started))

        far = apis[watcher.name]
        seen: list[dict] = []
        answered: set[str] = set()
        answers = []

        def follow_until(predicate, timeout: float = 120):
            deadline = time.monotonic() + timeout
            while True:
                view = far.request("GET", f"/jobs/{job}?from_seq={len(seen) and seen[-1]['seq'] + 1 or 1}")
                seen.extend(view["events"])
                result = predicate(view["job"])
                if result:
                    return result
                if view["job"]["state"] in TERMINAL:
                    raise AssertionError(f"the job ended {view['job']['state']}: {view['job'].get('message')}")
                if time.monotonic() > deadline:
                    raise AssertionError(f"timed out; the job is {view['job']['state']}")
                time.sleep(0.5)

        def pending(summary):
            request = summary.get("pending_request")
            if request and request not in answered:
                form = next(e for e in reversed(seen) if e["type"] == "request" and e["request_id"] == request)
                return request, form
            return None

        s.require(
            f"{watcher.name} sees the job start on {runner.name}",
            lambda: follow_until(lambda j: any(e["type"] == "started" and e["host"] == runner.name for e in seen)),
        )
        request, form = None, None

        def first_request():
            nonlocal request, form
            request, form = follow_until(pending)
            return form["kind"] == "input_group"

        s.require(f"the job's first request reaches {watcher.name}", first_request)
        s.check(
            f"{runner.name}'s live output shows on {watcher.name}",
            lambda: any(e["type"] == "output" and "Checking the cluster" in e["text"] for e in seen),
        )
        for reply in (
            {"kind": "values", "values": {"cores": 4, "hosts": ["mox1", "mox3"]}},
            {"kind": "confirm", "confirmed": True},
            {"kind": "acknowledge"},
        ):
            if form is None:
                request, form = follow_until(pending)
            far.request("POST", f"/jobs/{job}/requests/{request}", reply)
            answered.add(request)
            answers.append(form["kind"])
            form = None
        s.check("the input group, confirmation, and manual action were answered from " + watcher.name, answers == ["input_group", "confirm", "manual_action"], detail=str(answers))

        final = follow_until(lambda j: j if j["state"] in TERMINAL else None)
        s.check("the job completes", final["state"] == "COMPLETED", detail=json.dumps(final))
        s.check(
            "its result reached " + watcher.name,
            lambda: any(e["type"] == "result" and e["data"] == {"vmid": 100, "name": "prod9"} for e in seen),
        )
        s.check(
            "the job's own host and the starting host agree on the outcome",
            lambda: apis[origin.name].request("GET", f"/jobs/{job}")["job"]["state"] == "COMPLETED"
            and apis[runner.name].request("GET", f"/jobs/{job}")["job"]["state"] == "COMPLETED",
        )
