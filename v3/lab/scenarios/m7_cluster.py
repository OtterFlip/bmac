# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M7's walking skeleton in the lab: three hosts from blank VMs to a
cluster through the Deployer and the Dashboard API alone. mox3 starts as
m7_storage leaves it (the stage m7-first-host): an encrypted one-host
cluster with a SATA data mirror. mox3 issues join bundles for mox1 and
mox2 at once, and the Deployer installs both from them. Both hosts then
ask to join together. Approving mox1 starts its hosts.join, and approving
mox2 while that runs is refused, because membership changes take turns
(the cluster-membership lock); approved again once mox1 is in, mox2 joins
the same way. Every setup phase completes on both, final verification
included, and the three hosts are a quorate cluster.

When every check passes, the three hosts are saved together as the stage
m7-cluster, which later scenarios start from (see staged).

BMAC_LAB_DRILLS=1 runs each joining host's boot drills (declined
otherwise). Every host is put back as it was afterwards."""

from __future__ import annotations

from pathlib import Path

from bmaclab.api import ApiError
from bmaclab.scenario import load

DESCRIPTION = "mox3 issues two join bundles; mox1 and mox2 ask to join at once, take turns, and make a quorate three-host cluster"

m6 = load(Path(__file__).with_name("m6.py"))
m7 = load(Path(__file__).with_name("m7_storage.py"))
join = load(Path(__file__).with_name("m7_join.py"))

# The three hosts as a successful run leaves them; mox3 keeps its LUKS
# passphrase from m7-first-host.
STAGE = "m7-cluster"
HOSTS = ["mox3", "mox1", "mox2"]


def build(s, after=None) -> None:
    """Builds the cluster from m7-first-host and saves it as STAGE when
    every check passes, then carries on with `after({name: host})`."""

    def then(mox3) -> None:
        failed = s.failed
        dashboard = join.Dashboard(mox3)
        join.ready(s, dashboard)
        bundles = {host: join.issue(s, dashboard, mox3, host) for host in ("mox1", "mox2")}
        s.check("the bundles reserve mox1 and mox2", lambda: join.reserved(dashboard) == ["mox1", "mox2"])

        def joined(mox1, mox2) -> None:
            take_turns(s, dashboard, mox3, mox1, mox2, bundles)
            if s.failed != failed:
                return
            hosts = {"mox3": mox3, "mox1": mox1, "mox2": mox2}
            m6.save_cluster_stage(s, [hosts[name] for name in HOSTS], STAGE)
            if after is not None:
                after(hosts)

        def both(mox2) -> None:
            m6.install(s, m6.NVME_MIRROR, lambda mox1: joined(mox1, mox2), vm_name="mox1", bundle=bundles["mox1"][0])

        m6.install(s, m6.NVME_MIRROR, both, vm_name="mox2", bundle=bundles["mox2"][0])

    m7.staged(s, then)


def staged(s, then) -> None:
    """Carries on with `then({name: host})` from the three-host cluster, from
    STAGE when every host has it, built otherwise."""
    if m6.has_stage(s.lab, HOSTS, STAGE):
        m6.reuse_cluster(s, HOSTS, STAGE, then, unlock=m7.unlock_at_boot)
    else:
        build(s, then)


def take_turns(s, dashboard, mox3, mox1, mox2, bundles: dict) -> None:
    for newcomer in (mox1, mox2):
        join.await_request(s, dashboard, mox3, newcomer)

    first = dashboard.start("hosts.join", {"host": "mox1"})
    try:
        dashboard.start("hosts.join", {"host": "mox2"})
        refused = None
    except ApiError as e:
        refused = e
    s.check(f"approving mox2 while mox1 joins is refused ({refused})",
            refused is not None and refused.status == 409 and "cluster-membership" in str(refused))
    join.approve(s, dashboard, mox3, mox1, job=first)
    s.check("mox2's request is still waiting", lambda: any(
        j["host"] == "mox2" and j.get("request") for j in dashboard.call("GET", "/joins")["joins"]))
    join.approve(s, dashboard, mox3, mox2)

    join.check_members(s, dashboard, [mox3, mox1, mox2], [b["setup_ssh_public_key"] for _, b in bundles.values()])
    for host in (mox1, mox2):
        s.check(f"{host.vm.name} joined in the cluster's time zone, {m7.LAB_ZONE}", lambda: host.on_host(
            "timedatectl show -p Timezone --value") == m7.LAB_ZONE)
    s.check("mox3's pool is still on LUKS", lambda: int(mox3.on_host(
        "zpool status -P rpool | grep -c /dev/mapper/ || true")) >= 2)


def run(s) -> None:
    build(s)
