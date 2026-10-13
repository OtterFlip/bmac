# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""The `bmac` CLI's read-only commands on a real cluster (M2.12), and the
pool parsing behind them on every vdev class (M1.7). Starting from the
three-host cluster (m7_cluster.staged), mox1 reports the cluster with
`bmac status`, its (empty) guest list with `bmac guests`, and its rpool
with `bmac capacity`.

mox1 then builds a throwaway pool on loop devices with a raidz2 data vdev
and a dedup, special, log, cache, and spare vdev, and fills a sparse zvol.
`bmac capacity` reads the pool's geometry, and the space ZFS charges the
zvol is at most what the capacity function says the copy needs. With a
member faulted, the ZFS event daemon (which Proxmox runs) resilvers a hot
spare in by itself, and `bmac capacity` reads the same geometry.

BMAC_LAB_CAPTURE_FIXTURES=1 writes the pool's `zpool status` and
`zpool list` outputs, as the adapters ask for them, healthy and degraded, into
crates/bmac-adapters/tests/fixtures/lab/, which the adapter tests parse.
Every host is put back as it was afterwards."""

from __future__ import annotations

import base64
import json
import os
from pathlib import Path

from bmaclab.scenario import load

DESCRIPTION = "bmac status, guests, and capacity on the cluster, and a raidz2 pool with every vdev class"

cluster = load(Path(__file__).with_name("m7_cluster.py"))

FIXTURES = Path(__file__).resolve().parents[2] / "crates" / "bmac-adapters" / "tests" / "fixtures" / "lab"
# The adapter tests drop it before parsing.
HEADER = "# Copyright (c) 2026 BEENTHERE VENTURES, INC.\n# SPDX-License-Identifier: GPL-3.0-only\n\n"
POOL = "fixture"
FOLDER = "/var/tmp/bmac-fixture"
LOOPS = 13
MIB = 1024 * 1024
VOLSIZE = 256 * MIB
BLOCK = 16 * 1024


def raw(host, command: str) -> str:
    """The command's exact output; on_host strips it."""
    return base64.b64decode(host.on_host(f"set -o pipefail; {command} | base64 -w0")).decode()


def run(s) -> None:
    cluster.staged(s, lambda hosts: checks(s, hosts["mox1"]))


def checks(s, mox1) -> None:
    status = json.loads(mox1.on_host("bmac status --json"))
    s.check(f"bmac status on mox1: a quorate cluster of mox1, mox2, and mox3, all online, one duty host ({status})",
            status["host"] == "mox1" and status.get("engine") and status["quorate"] is True
            and sorted(m["host"] for m in status["members"]) == ["mox1", "mox2", "mox3"]
            and all(m["online"] for m in status["members"])
            and sum(1 for m in status["members"] if m["duty"]) == 1)
    s.check("bmac guests on mox1 lists no guests yet",
            lambda: json.loads(mox1.on_host("bmac guests --json"))["guests"] == [])
    s.check("bmac capacity on mox1 reads rpool as one two-way mirror, with ZFS's used and available",
            lambda: rpool_capacity(mox1))

    loops = mox1.on_host(
        f"mkdir -p {FOLDER} && for n in $(seq 1 {LOOPS}); do truncate -s 1G {FOLDER}/$n; "
        f"losetup --find --show {FOLDER}/$n; done").split()
    try:
        s.require(f"mox1 has {LOOPS} loop devices for the pool", len(loops) == LOOPS)
        data, dedup, special, log, cache, spares = loops[0:4], loops[4:6], loops[6:8], loops[8:10], loops[10], loops[11:13]
        mox1.on_host(
            f"zpool create -f -m none -o ashift=12 -O compression=off {POOL} raidz2 {' '.join(data)} "
            f"dedup mirror {' '.join(dedup)} special mirror {' '.join(special)} log mirror {' '.join(log)} "
            f"cache {cache} spare {' '.join(spares)}")
        s.check("the pool's status lists raidz2, dedup, special, logs, cache, and spares", lambda: all(
            section in raw(mox1, f"zpool status -P -p {POOL}") for section in
            ("raidz2-0", "\tdedup", "\tspecial", "\tlogs", "\tcache", "\tspares")))
        healthy = capture(mox1)

        mox1.on_host(f"zfs create -s -V {VOLSIZE} -o volblocksize={BLOCK} -o compression=off {POOL}/vol && udevadm settle"
                     f" && dd if=/dev/urandom of=/dev/zvol/{POOL}/vol bs=1M count={VOLSIZE // MIB} oflag=direct"
                     f" conv=fsync status=none && zpool sync {POOL}", timeout=600)
        s.check("bmac capacity reads the pool as one raidz2 of 4, and the filled zvol takes at most what it needs",
                lambda: fits(s, mox1))

        mox1.on_host(f"zpool offline -f {POOL} {data[2]}")
        s.check("the ZFS event daemon puts a hot spare in for the faulted member by itself", lambda: cluster.m6.wait_until(
            lambda: "spare-2" in raw(mox1, f"zpool status -P -p {POOL}"), 60))
        mox1.on_host(f"zpool wait -t resilver {POOL}", timeout=600)
        degraded_text = raw(mox1, f"zpool status -P -p {POOL}")
        s.check("the faulted member is in a spare group with the spare resilvered in, and the spare is INUSE",
                "spare-2" in degraded_text and "FAULTED" in degraded_text and "INUSE" in degraded_text)
        degraded = capture(mox1)
        s.check("bmac capacity still reads one raidz2 of 4 with the spare standing in", lambda: fits(s, mox1))
    finally:
        mox1.on_host(f"zpool destroy -f {POOL} 2>/dev/null; for l in {' '.join(loops)}; do losetup -d $l; done;"
                     f" rm -rf {FOLDER}")
    if os.environ.get("BMAC_LAB_CAPTURE_FIXTURES") == "1":
        FIXTURES.mkdir(parents=True, exist_ok=True)
        for state, outputs in (("healthy", healthy), ("degraded", degraded)):
            for kind, text in outputs.items():
                (FIXTURES / f"{kind}_{state}.txt").write_text(HEADER + text)
        s.note(f"wrote the pool's outputs to {FIXTURES}")


def capture(host) -> dict[str, str]:
    return {"zpool_status": raw(host, f"zpool status -P -p {POOL}"),
            "zpool_list": raw(host, f"zpool list -v -H -p -P -o name,size,allocated,free,health {POOL}")}


def rpool_capacity(host) -> bool:
    capacity = json.loads(host.on_host("bmac capacity --json"))
    used, available = (int(v) for v in host.on_host("zfs get -Hp -o value used,available rpool").split())
    return (capacity["pool"] == "rpool" and len(capacity["geometry"]["data"]) == 1
            and capacity["geometry"]["data"][0]["kind"] == "mirror"
            and capacity["geometry"]["data"][0]["width"] == 2
            and capacity["space"] == {"used": used, "available": available})


def fits(s, host) -> bool:
    capacity = json.loads(host.on_host(
        f"bmac capacity --json --pool {POOL} --volsize {VOLSIZE} --block-size {BLOCK}"))
    used = int(host.on_host(f"zfs get -Hp -o value used {POOL}/vol"))
    fit = capacity["fit"]
    s.note(f"the zvol of {VOLSIZE} bytes at {BLOCK}-byte blocks takes {used}; the capacity function needs {fit['needed']}")
    geometry = capacity["geometry"]["data"]
    return (len(geometry) == 1 and geometry[0]["kind"] == "raidz2" and geometry[0]["width"] == 4
            and geometry[0]["ashift"] == 12 and used <= fit["needed"])
