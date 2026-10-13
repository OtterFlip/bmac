# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M4 in the lab, for the script operations that need no guest network: the
diagnostics run on every host, each request answered with its default, and
each job's events are kept for reading. The VM operations (and the
production VM diagnostic, which needs a VM) need the host setup job's bridge
and ingress, so they run end to end with M7."""

from __future__ import annotations

import contextlib
import json

from bmaclab.api import Api, Codes, Tunnel, pinned_fingerprint
from bmaclab.bootstrap import Bootstrap
from bmaclab.jobs import run_job
from bmaclab.lab import BOOT_TIMEOUT

DESCRIPTION = "the diagnostics script operations run end to end on each host"
DIAGNOSTICS = (
    "diagnostics.cluster_health",
    "diagnostics.cluster_state",
    "diagnostics.host_state",
    "diagnostics.qdevice_state",
)


def run(s) -> None:
    lab = s.lab
    hosts = lab.topology.hosts
    for vm in hosts:
        s.require(f"{vm.name} answers SSH", lambda vm=vm: lab.wait_ssh(vm, BOOT_TIMEOUT) or True)
    bootstrap = Bootstrap(lab)
    s.require("the lab was bootstrapped (lab/lab bootstrap)", bootstrap.users_file.exists)
    users = json.loads(bootstrap.users_file.read_text())
    codes = Codes(users["alice_totp_secret"])
    password = users["users"]["alice@pve"]["password"]
    transcripts = bootstrap.users_file.parent / "m4-jobs"
    transcripts.mkdir(exist_ok=True)

    for vm in hosts:
        s.check(
            f"{vm.name} has the script config and the deferred-cleanup timer",
            lambda vm=vm: lab.ssh(
                vm,
                "test -s /run/bmac/config/cluster.conf && test -s /etc/bmac/ssh_config"
                " && test -x /var/lib/vz/snippets/bmac-guest-role-hook.sh"
                " && systemctl is-active --quiet bmac-deferred-cleanup.timer",
            ).returncode == 0,
        )

    with contextlib.ExitStack() as stack:
        for vm in hosts:
            tunnel = stack.enter_context(Tunnel(lab, vm))
            api = Api(tunnel.local, pinned_fingerprint(lab, vm), vm.name)
            s.require(
                f"alice logs in on {vm.name}",
                lambda api=api: api.login("alice@pve", password, codes)["role"] == "admin",
            )
            stack.callback(api.logout)
            for operation in DIAGNOSTICS:
                parameters = {"host": vm.name} if operation == "diagnostics.host_state" else {}

                attention: list[str] = []

                def ran(operation=operation, vm=vm, api=api, parameters=parameters, attention=attention):
                    summary, events = run_job(api, operation, parameters)
                    path = transcripts / f"{operation}-{vm.name}.json"
                    path.write_text(json.dumps({"job": summary, "events": events}, indent=1))
                    if summary["host"] != vm.name:
                        raise AssertionError(f"it ran on {summary['host']}")
                    # A diagnostic exits non-zero when its verdict needs attention.
                    verdicts = [e["data"].get("verdict") for e in events if e["type"] == "result"]
                    if summary["state"] != "COMPLETED" and not verdicts:
                        raise AssertionError(f"{summary['state']}: {summary.get('message', '')}")
                    for event in events:
                        if event["type"] in ("output", "log") and "[ATTENTION]" in event.get("text", ""):
                            attention.append(event["text"].strip())
                    return True

                s.check(f"{operation} runs to its verdict on {vm.name}", ran)
                for line in attention:
                    s.note(line)
        s.note(f"Each job's events are in {transcripts}")
