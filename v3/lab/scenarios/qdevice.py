# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M4's membership operations in the lab: QDevice add and remove, and the
host removal that needs one, through the Web Dashboard's API. The operator's
part is played from here: the QDevice's host-key fingerprint is read on the
QDevice itself, as on its console, and the cluster's QDevice access key is
added to its authorized_keys when a job asks.

It starts from the three-host cluster that `lab/lab bootstrap` makes and
removes mox3, so it leaves two hosts and the QDevice. `lab/lab reset m4`
(a snapshot of the bootstrapped lab) brings mox3 back."""

from __future__ import annotations

import contextlib
import json
import re
import time

from bmaclab.api import Api, Codes, Tunnel, pinned_fingerprint
from bmaclab.bootstrap import Bootstrap
from bmaclab.jobs import answer, instruction_texts, output_lines, run_job
from bmaclab.lab import BOOT_TIMEOUT

DESCRIPTION = "QDevice add and remove, and removing mox3 down to two hosts and a QDevice"
KEY = re.compile(r"ssh-ed25519 [A-Za-z0-9+/]+={0,2}( \S+)?")


def run(s) -> None:
    lab = s.lab
    topology = lab.topology
    mox1, mox2, mox3 = (topology.vm(name) for name in ("mox1", "mox2", "mox3"))
    qdevice = next(vm for vm in topology.vms if vm.role == "qdevice")
    for vm in (mox1, mox2, mox3, qdevice):
        s.require(f"{vm.name} answers SSH", lambda vm=vm: lab.wait_ssh(vm, BOOT_TIMEOUT) or True)
    bootstrap = Bootstrap(lab)
    s.require("the lab was bootstrapped (lab/lab bootstrap)", bootstrap.users_file.exists)
    users = json.loads(bootstrap.users_file.read_text())
    codes = Codes(users["alice_totp_secret"])
    password = users["users"]["alice@pve"]["password"]
    transcripts = bootstrap.users_file.parent / "qdevice-jobs"
    transcripts.mkdir(exist_ok=True)

    def sh(vm, command: str) -> str:
        result = lab.ssh(vm, command)
        if result.returncode != 0:
            raise AssertionError(f"{vm.name}: {command}: {result.stderr.strip()}")
        return result.stdout

    def votes(vm=mox1) -> dict:
        status = sh(vm, "pvecm status")
        value = {}
        for line in status.splitlines():
            key, _, rest = line.partition(":")
            if key.strip() in ("Nodes", "Expected votes", "Total votes", "Quorate", "Flags"):
                value[key.strip()] = rest.strip()
        return value

    def authorized() -> str:
        return sh(qdevice, "cat /root/.ssh/authorized_keys 2>/dev/null || true")

    s.require(
        "the cluster has three hosts and no QDevice (otherwise `lab/lab reset m4`)",
        lambda: votes()["Nodes"] == "3" and "Qdevice" not in votes().get("Flags", ""),
    )
    sh(qdevice, "touch /root/.ssh/authorized_keys && sed -i '/bmac-qdevice@/d' /root/.ssh/authorized_keys")
    asked: list[str] = []

    def was_asked(request: str) -> bool:
        """bmacd prefixes the script's request id to make it unique."""
        return any(r.endswith(f"-{request}") for r in asked)

    def operator(form: dict) -> dict:
        asked.append(form["request_id"])
        if form["kind"] == "manual_action":
            for text in instruction_texts(form):
                if "authorized_keys" in text and (key := KEY.search(text)):
                    sh(qdevice, f"grep -qxF '{key.group(0)}' /root/.ssh/authorized_keys || echo '{key.group(0)}' >>/root/.ssh/authorized_keys")
            return {"kind": "acknowledge"}
        field = form.get("field") or {}
        if field.get("id") == "qdevice_host_key":
            fingerprint = sh(qdevice, "ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub").split()[1]
            return {"kind": "values", "values": {"qdevice_host_key": fingerprint}}
        return answer(form)

    with contextlib.ExitStack() as stack:
        apis = {}
        for vm in (mox1, mox2):
            tunnel = stack.enter_context(Tunnel(lab, vm))
            api = Api(tunnel.local, pinned_fingerprint(lab, vm), vm.name)
            s.require(f"alice logs in on {vm.name}", lambda api=api: api.login("alice@pve", password, codes)["role"] == "admin")
            stack.callback(api.logout)
            apis[vm.name] = api

        def job(name: str, operation: str, parameters: dict, on: str = "mox1", timeout: float = 900) -> list[str]:
            asked.clear()
            summary, events = run_job(apis[on], operation, parameters, timeout=timeout, answerer=operator)
            (transcripts / f"{name}.json").write_text(json.dumps({"job": summary, "events": events}, indent=1))
            if summary["state"] != "COMPLETED":
                tail = "\n".join(output_lines(events)[-15:])
                raise AssertionError(f"{summary['state']}: {summary.get('message', '')}\n{tail}")
            return output_lines(events)

        def odd_cluster_needs_none():
            lines = job("add-on-three", "qdevice.add", {})
            if not any("does not use a QDevice" in line for line in lines):
                raise AssertionError("it did not say the odd cluster needs no QDevice")
            return "Qdevice" not in votes().get("Flags", "")

        s.check("Add QDevice on three hosts changes nothing", odd_cluster_needs_none)

        def remove_mox3():
            job("remove-mox3", "hosts.remove", {"host": "mox3"}, timeout=1800)
            if not was_asked("qdevice_access") or not was_asked("qdevice_host_key"):
                raise AssertionError(f"the operator was not asked for QDevice access: {asked}")
            state = votes()
            return state["Nodes"] == "2" and state["Expected votes"] == "3" and "Qdevice" in state["Flags"]

        s.require("Remove mox3 leaves two hosts and adds the QDevice vote", remove_mox3)
        def powered_off():
            deadline = time.monotonic() + 180
            while lab.running(mox3):
                if time.monotonic() > deadline:
                    return False
                time.sleep(5)
            return True

        s.check("mox3 is powered off", powered_off)
        s.check("the QDevice runs corosync-qnetd", lambda: sh(qdevice, "systemctl is-active corosync-qnetd").strip() == "active")

        def diagnostic_from(host: str):
            lines = job(f"qdevice-state-{host}", "diagnostics.qdevice_state", {}, on=host, timeout=300)
            return any("alive and voting" in line for line in lines)

        s.check("the QDevice diagnostic finds it healthy from mox1", lambda: diagnostic_from("mox1"))
        s.check("and from mox2, with the same cluster key", lambda: diagnostic_from("mox2"))

        def remove():
            job("remove", "qdevice.remove", {})
            state = votes()
            return state["Expected votes"] == "2" and "Qdevice" not in state.get("Flags", "")

        s.require("Remove QDevice takes the vote away", remove)
        s.check("and purges corosync-qnetd", lambda: sh(qdevice, "command -v corosync-qnetd || echo gone").strip() == "gone")
        s.check("and no longer authorizes the cluster key", lambda: "bmac-qdevice@" not in authorized())

        def add():
            job("add", "qdevice.add", {})
            if not was_asked("qdevice_ready") or not was_asked("qdevice_host_key"):
                raise AssertionError(f"the operator was not asked to prepare the QDevice: {asked}")
            state = votes(mox2)
            return state["Expected votes"] == "3" and state["Total votes"] == "3" and "Qdevice" in state["Flags"]

        s.check("Add QDevice on two hosts adds the vote back", add)
        s.check("the setup key is gone and the cluster key stays", lambda: "ssh-rsa" not in authorized() and "bmac-qdevice@" in authorized())
        s.note(f"Each job's events are in {transcripts}; `lab/lab reset m4` brings mox3 back")
