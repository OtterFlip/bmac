# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M4's VM operations in the lab, each from mox3's Dashboard, from the
cluster with prod1 (m4_prod.staged):

1. staging.add twice, a staging VM on each of prod1's standbys (mox2 and
   mox3), each a started clone with its own name, address, and host key;
2. a failover drill: mox1 is powered off, HA restarts prod1 on a standby,
   and that standby's staging VM is evicted while the other's runs on;
   mox1 is then powered on again, and the deferred cleanup destroys the
   evicted staging VM;
3. staging.remove removes the other staging VM;
4. prod.change_owner moves prod1 back to mox1, which it refuses to do while
   staging VMs derived from prod1 remain;
5. prod.change_placement removes mox3 from prod1's placement, then adds it
   back;
6. prod.extend_disk grows prod1's disk by 2 GiB, online in the guest;
7. prod.remove removes prod1.

Every host is put back as it was afterwards."""

from __future__ import annotations

import json
import secrets
from pathlib import Path

from bmaclab.scenario import load

DESCRIPTION = "M4's VM operations from the Dashboard: staging VMs, a failover drill with eviction, move, placement, disk, removal"

prod = load(Path(__file__).with_name("m4_prod.py"))
m6 = prod.m6
join = prod.join

STANDBYS = ("mox2", "mox3")
FAILOVER_TIMEOUT = 900
CLEANUP_TIMEOUT = 1200


def answering(answers: dict | None = None, choices: dict | None = None):
    """reply(form) for join.follow: fields by id from answers, a choice by
    the start of its label from choices, the rest their defaults."""
    answers, choices = answers or {}, choices or {}

    def reply(form: dict) -> dict:
        if form["kind"] == "confirm":
            return {"kind": "confirm", "confirmed": True}
        if form["kind"] == "manual_action":
            raise AssertionError(f"unexpected manual step {form.get('title')!r}")
        values = {}
        for field in form.get("fields") or [form["field"]]:
            label = f"{form.get('title') or ''} {field.get('label') or ''}"
            chosen = next((v for k, v in choices.items() if k in label), None)
            if chosen is not None:
                values[field["id"]] = chosen
            elif field["id"] in answers:
                values[field["id"]] = answers[field["id"]]
            elif field.get("default") is not None:
                values[field["id"]] = field["default"]
            elif not field.get("required"):
                values[field["id"]] = ""
            else:
                raise AssertionError(f"no answer for {field['id']} ({label.strip()})")
        return {"kind": "values", "values": values}

    return reply


def operate(s, dashboard, operation: str, parameters: dict, what: str, **reply) -> bool:
    summary, events = join.follow(s, dashboard, dashboard.start(operation, parameters), answering(**reply))
    if summary.get("state") != "COMPLETED":
        for e in [e for e in events if e["type"] == "output"][-8:]:
            s.note(f"output: {e.get('text')}")
    return s.check(f"{operation} {what} ({summary.get('state')}: {summary.get('message') or ''})",
                   summary.get("state") == "COMPLETED")


def guests(host) -> dict[str, dict]:
    return {g["name"]: g for g in json.loads(host.on_host("bmac guests --json"))["guests"]}


def staging_on(host, node: str) -> dict:
    return next((g for g in guests(host).values() if g["kind"] == "staging" and g.get("node") == node), {})


def nodes_online(host) -> bool:
    """Whether the host's pvestatd view, which the scripts check, has every node online."""
    return all(n.get("status") == "online" for n in json.loads(host.on_host("pvesh get /nodes --output-format json")))


def guest_exec(host, vmid: int, *command: str) -> str:
    done = json.loads(host.on_host(f"qm guest exec {vmid} -- {' '.join(command)}"))
    return done.get("out-data", "").strip()


def run(s) -> None:
    prod.staged(s, lambda hosts: operations(s, hosts))


def operations(s, hosts) -> None:
    mox3 = hosts["mox3"]
    dashboard = join.Dashboard(mox3)
    s.require("HA has prod1 started and every host online after the restore", lambda: m6.wait_until(lambda: (
        (g := guests(mox3)["prod1"]).get("status") == "running"
        and f"service vm:{g['vmid']} ({g['node']}, started)" in mox3.on_host("ha-manager status")
        and all(nodes_online(h) for h in hosts.values())), 300))
    prod1 = guests(mox3)["prod1"]
    owner = hosts[prod1["node"]]
    prod_key = guest_exec(owner, prod1["vmid"], "cat", "/etc/ssh/ssh_host_ed25519_key.pub")

    staging = {}
    for node in STANDBYS:
        if not operate(s, dashboard, "staging.add", {"source": "prod1"}, f"clones prod1 onto {node}",
                       answers={"staging_guest_vm_root_password": secrets.token_urlsafe(18)},
                       choices={"Staging standby": node, "Snapshot for the new staging VM": "new"}):
            return
        vm = staging[node] = staging_on(mox3, node)
        s.check(f"the staging VM on {node} runs with its own name and address ({vm})", lambda vm=vm: (
            vm.get("status") == "running" and vm.get("name", "").startswith("stage")
            and vm.get("ip") not in (None, "", prod1["ip"])))
        s.check(f"{vm.get('name')} has its own hostname and SSH host key", lambda node=node, vm=vm: m6.wait_until(lambda: (
            guest_exec(hosts[node], vm["vmid"], "hostname", "-s") == vm["name"]
            and guest_exec(hosts[node], vm["vmid"], "cat", "/etc/ssh/ssh_host_ed25519_key.pub") not in ("", prod_key)), 300))

    evicted = drill(s, hosts, staging)

    for node, vm in staging.items():
        if node == evicted:
            continue
        operate(s, dashboard, "staging.remove", {"resource": vm["name"]}, f"removes {vm['name']}")
        s.check(f"{vm['name']} is gone", lambda node=node, vm=vm: vm["name"] not in guests(mox3)
                and hosts[node].on_host(f"qm status {vm['vmid']} >/dev/null 2>&1 && echo there || true") == "")

    operate(s, dashboard, "prod.change_owner", {"resource": "prod1"}, "moves prod1 back to mox1",
            choices={"New owner": "mox1"})
    s.check("prod1 runs on mox1", lambda: m6.wait_until(lambda: (
        (g := guests(mox3)["prod1"]).get("node") == "mox1" and g.get("status") == "running"), 300))

    placement(s, hosts, dashboard)
    extend(s, hosts, dashboard)

    vmid = guests(mox3)["prod1"]["vmid"]
    operate(s, dashboard, "prod.remove", {"resource": "prod1"}, "removes prod1")
    s.check("prod1 is gone from the registry, HA, replication, and every host", lambda: (
        "prod1" not in guests(mox3)
        and f"vm:{vmid}" not in mox3.on_host("ha-manager status")
        and not [line for line in mox3.on_host("pvesr status").splitlines()[1:] if line.split()[0].startswith(f"{vmid}-")]
        and all(h.on_host(f"zfs list -H -o name | grep -c 'vm-{vmid}-' || true") == "0" for h in hosts.values())))


def drill(s, hosts, staging: dict) -> str:
    """Returns the standby prod1 landed on, whose staging VM it evicted."""
    lab, mox3 = s.lab, hosts["mox3"]
    mox1 = lab.topology.vm("mox1")
    lab.power(mox1, "off")
    s.note("mox1 powered off")
    landed = {}
    s.require("HA restarts prod1 on a standby after mox1 is lost", lambda: m6.wait_until(lambda: (
        landed.update(guests(mox3)["prod1"]) or (landed.get("node") in STANDBYS and landed.get("status") == "running")),
        FAILOVER_TIMEOUT))
    evicted, other = staging[landed["node"]], staging[next(n for n in STANDBYS if n != landed["node"])]
    s.check(f"{evicted['name']}, the staging VM on {landed['node']}, was evicted for prod1", lambda: m6.wait_until(
        lambda: (g := guests(mox3).get(evicted["name"], {})).get("state") == "cleanup_pending"
        and g.get("status") != "running", 120))
    s.check(f"{other['name']} on {other['node']} runs on", lambda: guests(mox3)[other["name"]]["status"] == "running")
    lab.power(mox1, "on")
    s.require("mox1 is back in the cluster", lambda: m6.wait_until(lambda: (
        hosts["mox1"].on_host("true") == "" and all(nodes_online(h) for h in hosts.values())), m6.BOOT_TIMEOUT))
    s.check(f"the deferred cleanup destroys the evicted {evicted['name']}", lambda: m6.wait_until(
        lambda: evicted["name"] not in guests(mox3)
        and hosts[landed["node"]].on_host(f"qm status {evicted['vmid']} >/dev/null 2>&1 && echo there || true") == "",
        CLEANUP_TIMEOUT))
    return landed["node"]


def placement(s, hosts, dashboard) -> None:
    mox1, mox3 = hosts["mox1"], hosts["mox3"]
    vmid = guests(mox3)["prod1"]["vmid"]

    def replicates_to(target: str) -> bool:
        return any(line.split()[0].startswith(f"{vmid}-") and line.split()[2] == f"local/{target}"
                   for line in mox1.on_host("pvesr status").splitlines()[1:] if len(line.split()) > 3)

    operate(s, dashboard, "prod.change_placement", {"resource": "prod1"}, "removes mox3 from prod1's placement",
            answers={"remove_hosts": ["mox3"]}, choices={"Change the placement": "r"})
    s.check("prod1 is placed on mox1 and mox2 and no longer replicates to mox3", lambda: (
        sorted(guests(mox3)["prod1"]["placement"]) == ["mox1", "mox2"] and not replicates_to("mox3")))
    operate(s, dashboard, "prod.change_placement", {"resource": "prod1"}, "adds mox3 back",
            answers={"add_hosts": ["mox3"]}, choices={"Change the placement": "a"})
    s.check("prod1 is placed on all three hosts and replicates to mox3 again", lambda: m6.wait_until(lambda: (
        sorted(guests(mox3)["prod1"]["placement"]) == prod.HOSTS_SORTED and replicates_to("mox3")), 900))


def extend(s, hosts, dashboard) -> None:
    mox1 = hosts["mox1"]
    vmid = guests(hosts["mox3"])["prod1"]["vmid"]

    def disk_bytes() -> int:
        return int(guest_exec(mox1, vmid, "lsblk", "-bdno", "SIZE", "/dev/sda") or 0)

    before = disk_bytes()
    operate(s, dashboard, "prod.extend_disk", {"resource": "prod1"}, "grows prod1's disk by 2 GiB",
            answers={"unit": "GiB", "amount": "2"})
    s.check(f"prod1's disk grew by 2 GiB inside the guest (from {before} bytes)", lambda: m6.wait_until(
        lambda: disk_bytes() == before + (2 << 30), 120))
