# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""M9.0's lab spike: the ZFS behaviour BACKUP.md relies on, proven before
any code does (its "To prove in the lab" items 1 to 4). It starts from the
cluster with production VMs (m8_storage.staged) and uses the "small" VM,
placed on mox1 and mox3, as the backed-up VM. The lab's appliance VM
(Ubuntu, its own OpenZFS) plays the appliance with a plain pool on its
backup disk. Streams go host to appliance through this workstation, since
the scenario only plays BMAC's part.

- Item 1: replication carries a BMAC snapshot to the replica, with its GUID,
  and removes it there once the owner destroys it.
- Item 2: a bookmark made on each placement host from its own copy of a
  snapshot has the snapshot's GUID and survives later replication runs, an
  HA move, and a forced failover (the owner powered off). An incremental
  from a bookmark applies on the appliance when sent by the replica, by the
  new owner after the move, and by the owner HA recovered the VM on.
- Item 3: an incremental from the branch point, received with `-o origin=`
  as a clone of the appliance's own copy (a new branch), and on a host as a
  clone of a local snapshot (restore method 2).
- Item 4: a restored volume keeps its PIT's GUID through `zfs rename` into
  the Proxmox storage, `zfs promote`, and a local send and receive;
  replication's first sync carries the PIT snapshot to the other placement
  host, for a full receive and for a method 2 clone; and the next PIT, sent
  from the restored PIT's bookmark, is received on the appliance as a clone
  of its copy of the restored PIT, though the restored VM is another VM.

The outcome is written to m9-spike.json in the lab's folder. The hosts and
the appliance are put back as they were."""

from __future__ import annotations

import json
import subprocess
import time
from pathlib import Path

from bmaclab.lab import BOOT_TIMEOUT
from bmaclab.scenario import load

DESCRIPTION = "bookmarks, clone receives, and restored-volume GUIDs behave as BACKUP.md needs (lab items 1-4)"

storage = load(Path(__file__).with_name("m8_storage.py"))
m6, m7 = storage.m6, storage.m7

CHECKPOINT = "before-m9"
POOL = "bk"
NAMESPACE = f"{POOL}/bmac/clusters/spike/vms"
REPLICATION_TIMEOUT = 600
FAILOVER_TIMEOUT = 900
WRITE_MIB = 32


class Spike:
    def __init__(self, s, hosts) -> None:
        self.s, self.lab, self.hosts = s, s.lab, hosts
        self.appliance = self.lab.topology.vm("appliance")
        self.results: dict[str, bool] = {}
        self.vmid = 0
        self.volumes: list[str] = []

    # Plumbing

    def check(self, item: str, what: str, test) -> bool:
        ok = self.s.check(f"item {item}: {what}", test)
        self.results[f"{item}: {what}"] = bool(ok)
        return bool(ok)

    def app(self, command: str, timeout: float = 300) -> str:
        done = self.lab.ssh(self.appliance, command, timeout=timeout)
        if done.returncode != 0:
            raise RuntimeError(f"appliance: {command}: {done.stderr.strip()}")
        return done.stdout.strip()

    def pipe(self, source: list[str], target: list[str], timeout: float = 1800) -> None:
        """source's stdout into target's stdin, both ssh commands."""
        sender = subprocess.Popen(source, stdout=subprocess.PIPE, stderr=subprocess.PIPE, stdin=subprocess.DEVNULL)
        receiver = subprocess.run(target, stdin=sender.stdout, capture_output=True, timeout=timeout)
        sender.stdout.close()
        sent_err = sender.stderr.read().decode(errors="replace")
        sender.wait(timeout=60)
        if sender.returncode or receiver.returncode:
            raise RuntimeError(f"send exit {sender.returncode} ({sent_err.strip()[-300:]}), receive exit "
                               f"{receiver.returncode} ({receiver.stderr.decode(errors='replace').strip()[-300:]})")

    def host_cmd(self, name: str, command: str) -> list[str]:
        return [*self.hosts[name].ssh_args(), command]

    def app_cmd(self, command: str) -> list[str]:
        return self.lab.ssh_args(self.appliance, [command])

    def on(self, name: str, command: str, timeout: float = 300) -> str:
        return self.hosts[name].on_host(command, timeout=timeout)

    def guid(self, name: str, dataset: str) -> str:
        return self.on(name, f"zfs get -Hp -o value guid {dataset}")

    def app_guid(self, dataset: str) -> str:
        return self.app(f"zfs get -Hp -o value guid {dataset}")

    def exists(self, name: str, dataset: str) -> bool:
        return self.on(name, f"zfs list -H -t all -o name {dataset} >/dev/null 2>&1 && echo yes || true") == "yes"

    # The VM

    def owner(self) -> tuple[str, str]:
        rows = json.loads(self.on("mox2", "pvesh get /cluster/resources --type vm --output-format json"))
        row = next(r for r in rows if r.get("vmid") == self.vmid)
        return row["node"], row.get("status", "")

    def write(self) -> None:
        """New data in the guest, so each PIT differs from the last."""
        node, _ = self.owner()
        # After a relocate or failover the guest is still booting.
        deadline = time.monotonic() + 300
        while self.on(node, f"qm agent {self.vmid} ping >/dev/null 2>&1 && echo up || true") != "up":
            if time.monotonic() > deadline:
                raise RuntimeError(f"the guest agent of {self.vmid} on {node} never answered")
            time.sleep(5)
        out = self.on(node, f"qm guest exec {self.vmid} --timeout 120 -- /bin/sh -c "
                            f"'dd if=/dev/urandom of=/var/tmp/spike-{time.time_ns()} bs=1M count={WRITE_MIB} "
                            f"conv=fsync status=none && sync'", timeout=180)
        self.s.require(f"the guest wrote {WRITE_MIB} MiB", json.loads(out).get("exitcode") == 0)

    def scribble(self, node: str, volume: str, seek_mib: int) -> str:
        """A shell command writing new data into a zvol: WRITE_MIB at
        seek_mib, or into its start when it is smaller (an EFI vars disk)."""
        size_mib = int(self.on(node, f"zfs get -Hp -o value volsize {volume}")) >> 20
        count, seek = (WRITE_MIB, seek_mib) if size_mib >= seek_mib + WRITE_MIB else (0, 0)
        if count == 0:
            return (f"dd if=/dev/urandom of=/dev/zvol/{volume} bs=4k count=1 conv=fsync,notrunc "
                    f"status=none")
        return f"dd if=/dev/urandom of=/dev/zvol/{volume} bs=1M count={count} seek={seek} conv=fsync status=none"

    def snapshot(self, node: str, pit: str) -> None:
        self.on(node, "zfs snapshot " + " ".join(f"{v}@{pit}" for v in self.volumes))

    def jobs(self) -> list[dict]:
        rows = json.loads(self.on("mox2", "pvesh get /cluster/replication --output-format json"))
        return [r for r in rows if int(r.get("guest", 0)) == self.vmid]

    def replicate(self, why: str) -> bool:
        """One replication run of every job of the VM, after now, from the
        current owner."""
        node, _ = self.owner()
        since = int(self.on(node, "date +%s"))
        for job in self.jobs():
            for _ in range(20):
                try:
                    self.on(node, f"pvesr schedule-now {job['id']}")
                    break
                except RuntimeError:
                    time.sleep(5)

        def synced() -> bool:
            rows = json.loads(self.on(node, f"pvesh get /nodes/{node}/replication --guest {self.vmid} --output-format json"))
            return bool(rows) and all(int(r.get("last_sync", 0)) >= since and not r.get("error") for r in rows)

        return self.s.check(f"replication runs from {node} ({why})", lambda: m6.wait_until(synced, REPLICATION_TIMEOUT))

    def bookmark(self, pit: str) -> None:
        """Bookmarks every volume on every placement host from that host's
        own copy, and checks the bookmarks' GUIDs."""
        for name in ("mox1", "mox3"):
            for v in self.volumes:
                self.on(name, f"zfs bookmark {v}@{pit} {v}#{pit}")
            self.check("2", f"{pit}'s bookmarks on {name} have its snapshots' GUIDs", lambda name=name: all(
                self.guid(name, f"{v}#{pit}") == self.guid(name, f"{v}@{pit}") for v in self.volumes))

    def release(self, pit: str) -> None:
        """Destroys the PIT's snapshots on the owner; replication follows."""
        node, _ = self.owner()
        for v in self.volumes:
            self.on(node, f"zfs destroy {v}@{pit}")
        self.replicate(f"after {pit}'s snapshots are destroyed")
        other = "mox3" if node == "mox1" else "mox1"
        self.check("1", f"replication removes {pit}'s snapshots from {other}", lambda: m6.wait_until(
            lambda: not any(self.exists(other, f"{v}@{pit}") for v in self.volumes), REPLICATION_TIMEOUT))

    def target(self, branch: str, volume: str) -> str:
        return f"{NAMESPACE}/{self.vmid}/{branch}/{volume.rsplit('/', 1)[-1]}"

    def send(self, node: str, pit: str, base: str | None, branch: str = "A", origin: str | None = None) -> None:
        """Each volume's PIT from node to the appliance: full, or from base
        (`#name` for a bookmark, `@name` for a snapshot)."""
        for v in self.volumes:
            to = self.target(branch, v)
            self.app(f"zfs create -p -o mountpoint=none {to.rsplit('/', 1)[0]}")
            send = f"zfs send -i {v}{base} {v}@{pit}" if base else f"zfs send {v}@{pit}"
            receive = "zfs receive -s -u" + (" -o volmode=none" if base is None or origin else "")
            if origin:
                receive += f" -o origin={self.target('A', v)}@{origin}"
            self.pipe(self.host_cmd(node, send), self.app_cmd(f"{receive} {to}"))

    def received(self, item: str, what: str, node: str, pit: str, branch: str = "A") -> bool:
        return self.check(item, what, lambda: all(
            self.app_guid(f"{self.target(branch, v)}@{pit}") == self.guid(node, f"{v}#{pit}" if not self.exists(
                node, f"{v}@{pit}") else f"{v}@{pit}") for v in self.volumes))

    # Steps

    def prepare_appliance(self) -> None:
        lab, vm = self.lab, self.appliance
        lab.stop([vm])
        lab.checkpoint(vm, CHECKPOINT)
        lab.start([vm])
        self.s.require("the appliance answers SSH", lambda: lab.wait_ssh(vm, BOOT_TIMEOUT) or True)
        self.s.require("the appliance installs OpenZFS", lambda: self.app(
            "DEBIAN_FRONTEND=noninteractive apt-get -qq update && "
            "DEBIAN_FRONTEND=noninteractive apt-get -qq install -y zfsutils-linux >/dev/null && zfs version",
            timeout=900) != "")
        disk = next(d for d in vm.disks if d.bus == "scsi")
        name = next(row.split()[0] for row in self.app("lsblk -dno NAME,SERIAL").splitlines()
                    if row.split()[1:] == [disk.serial])
        self.app(f"zpool create -f -o ashift=12 -O compression=lz4 -O mountpoint=none {POOL} /dev/{name}")
        self.s.note("appliance " + self.app("zfs version | head -1") + "; hosts "
                    + self.on("mox1", "zfs version | head -1"))

    def find_vm(self) -> None:
        vm = storage.guest(self.hosts["mox1"], "small.lab.internal")
        self.s.require("the small VM is there", bool(vm.get("vmid")))
        self.vmid = int(vm["vmid"])
        node, status = self.owner()
        self.s.require(f"the small VM runs on mox1 ({node}, {status})", node == "mox1" and status == "running")
        self.volumes = [v for v in self.on("mox1", "zfs list -H -o name -t volume").splitlines()
                        if v.rsplit("/", 1)[-1].startswith(f"vm-{self.vmid}-disk-")]
        self.s.require(f"the small VM has volumes ({', '.join(self.volumes)})", bool(self.volumes))
        self.s.require("the small VM replicates to mox3", lambda: [j["target"] for j in self.jobs()] == ["mox3"])

    def bookmarks(self) -> None:
        """Items 1 and 2."""
        # P1: full send, bookmarked, released.
        self.write()
        self.snapshot("mox1", "bmac-pit-p1")
        self.replicate("P1")
        self.check("1", "replication carries P1 to mox3 with its GUIDs", lambda: all(
            self.guid("mox3", f"{v}@bmac-pit-p1") == self.guid("mox1", f"{v}@bmac-pit-p1") for v in self.volumes))
        self.send("mox1", "bmac-pit-p1", None)
        self.received("2", "the appliance has P1, full", "mox1", "bmac-pit-p1")
        self.check("6", "received zvols with volmode=none have no device node", lambda: self.app(
            f"ls /dev/zvol/{self.target('A', self.volumes[0])} 2>/dev/null || echo none") == "none")
        self.bookmark("bmac-pit-p1")
        self.release("bmac-pit-p1")
        self.check("2", "P1's bookmarks outlive its snapshots on both hosts", lambda: all(
            self.exists(n, f"{v}#bmac-pit-p1") for n in ("mox1", "mox3") for v in self.volumes))

        # P2: sent by the replica from P1's bookmark, after more replication runs.
        self.write()
        self.snapshot("mox1", "bmac-pit-p2")
        for run in range(3):
            self.replicate(f"P2, run {run + 1}")
        self.check("2", "P1's bookmarks on mox3 survive replication runs", lambda: all(
            self.guid("mox3", f"{v}#bmac-pit-p1") == self.guid("mox1", f"{v}#bmac-pit-p1") for v in self.volumes))
        self.send("mox3", "bmac-pit-p2", "#bmac-pit-p1")
        self.received("2", "the replica's incremental from P1's bookmark applies on the appliance", "mox1", "bmac-pit-p2")
        self.bookmark("bmac-pit-p2")
        self.release("bmac-pit-p2")

        # P3: after an HA move, sent by the new owner from P2's bookmark. The
        # move is BMAC's owner change (change_prod_vm_owner.sh): a
        # relocation, because the guest hook refuses an online migration's
        # start on the target, where the VM's configuration isn't yet.
        self.on("mox2", f"ha-manager relocate vm:{self.vmid} mox3")
        self.s.require("HA moves the small VM to mox3", lambda: m6.wait_until(
            lambda: self.owner() == ("mox3", "running"), FAILOVER_TIMEOUT))
        self.write()
        self.snapshot("mox3", "bmac-pit-p3")
        self.replicate("P3, from mox3 after the move")
        self.check("2", "P1's and P2's bookmarks on mox1 survive the move and replication the other way", lambda: all(
            self.exists("mox1", f"{v}#{p}") for v in self.volumes for p in ("bmac-pit-p1", "bmac-pit-p2")))
        self.send("mox3", "bmac-pit-p3", "#bmac-pit-p2")
        self.received("2", "the new owner's incremental from P2's bookmark applies", "mox3", "bmac-pit-p3")
        self.bookmark("bmac-pit-p3")
        self.release("bmac-pit-p3")

        # P4: after a forced failover, sent by the owner HA recovered the VM on.
        mox3 = self.hosts["mox3"]
        self.lab.power(mox3.vm, "off")
        self.s.note("mox3 powered off; waiting for HA to fence it and recover the VM on mox1")
        self.s.require("HA recovers the small VM on mox1", lambda: m6.wait_until(
            lambda: self.owner() == ("mox1", "running"), FAILOVER_TIMEOUT))
        self.write()
        self.snapshot("mox1", "bmac-pit-p4")
        self.send("mox1", "bmac-pit-p4", "#bmac-pit-p3")
        self.received("2", "after a failover, the incremental from P3's bookmark applies", "mox1", "bmac-pit-p4")
        with m6.booting(self.s, mox3.unlock):
            self.lab.start([mox3.vm])
        self.s.require("mox3 comes back", lambda: m6.wait_until(lambda: mox3.on_host("true") == "", BOOT_TIMEOUT))
        self.s.require("mox3 rejoins the cluster", lambda: m6.wait_until(lambda: all(
            r.get("status") == "online" for r in json.loads(self.on(
                "mox1", "pvesh get /cluster/resources --type node --output-format json"))), BOOT_TIMEOUT))
        self.replicate("P4, to mox3 after it returns")
        self.check("1", "replication carries P4 to the returned mox3", lambda: m6.wait_until(lambda: all(
            self.exists("mox3", f"{v}@bmac-pit-p4") for v in self.volumes), REPLICATION_TIMEOUT))
        self.check("2", "the bookmarks on mox3 survive its failover and resync", lambda: all(
            self.guid("mox3", f"{v}#{p}") == self.guid("mox1", f"{v}#{p}")
            for v in self.volumes for p in ("bmac-pit-p1", "bmac-pit-p2", "bmac-pit-p3")))
        self.bookmark("bmac-pit-p4")

    def clones(self) -> None:
        """Item 3. P4's snapshots stay on mox1, as local retention would
        keep them."""
        # P5 on branch A, then its snapshots released (bookmarks remain).
        self.write()
        self.snapshot("mox1", "bmac-pit-p5")
        self.send("mox1", "bmac-pit-p5", "@bmac-pit-p4")
        self.received("2", "an incremental from P4's snapshot applies", "mox1", "bmac-pit-p5")
        self.replicate("P5")
        self.bookmark("bmac-pit-p5")
        self.release("bmac-pit-p5")

        # Branch B forks at P4: a clone of P4 written to, its P6 received on
        # the appliance as a clone of the appliance's own @P4.
        for v in self.volumes:
            fork = f"rpool/bmac-spike/{v.rsplit('/', 1)[-1]}"
            self.on("mox1", f"zfs create -p rpool/bmac-spike && zfs clone {v}@bmac-pit-p4 {fork} && udevadm settle")
            self.on("mox1", f"{self.scribble('mox1', fork, 64)} && zfs snapshot {fork}@bmac-pit-p6")
            to = self.target("B", v)
            self.app(f"zfs create -p -o mountpoint=none {to.rsplit('/', 1)[0]}")
            self.pipe(self.host_cmd("mox1", f"zfs send -i {v}@bmac-pit-p4 {fork}@bmac-pit-p6"),
                      self.app_cmd(f"zfs receive -s -u -o volmode=none -o origin={self.target('A', v)}@bmac-pit-p4 {to}"))
            self.check("3", f"branch B's {to.rsplit('/', 1)[-1]} is a clone of the appliance's @P4 with P6's GUID",
                       lambda v=v, fork=fork, to=to: self.app(f"zfs get -H -o value origin {to}")
                       == f"{self.target('A', v)}@bmac-pit-p4"
                       and self.app_guid(f"{to}@bmac-pit-p6") == self.guid("mox1", f"{fork}@bmac-pit-p6"))
            self.check("3", "the branch stores only what changed after P4", lambda to=to: int(self.app(
                f"zfs get -Hp -o value used {to}")) < 2 * WRITE_MIB << 20)

        # Restore method 2 on a host: P5 from the appliance as a clone of
        # mox1's own P4 snapshot.
        for v in self.volumes:
            staged = f"rpool/bmac-restore/m2/{v.rsplit('/', 1)[-1]}"
            self.on("mox1", "zfs create -p -o mountpoint=none rpool/bmac-restore/m2")
            self.pipe(self.app_cmd(f"zfs send -i {self.target('A', v)}@bmac-pit-p4 {self.target('A', v)}@bmac-pit-p5"),
                      self.host_cmd("mox1", f"zfs receive -s -u -o volmode=none -o origin={v}@bmac-pit-p4 {staged}"))
            self.check("3", f"method 2 makes {staged} a clone of mox1's @P4 at P5's GUID",
                       lambda v=v, staged=staged: self.on("mox1", f"zfs get -H -o value origin {staged}") == f"{v}@bmac-pit-p4"
                       and self.guid("mox1", f"{staged}@bmac-pit-p5") == self.guid("mox1", f"{v}#bmac-pit-p5"))

    def restored(self) -> None:
        """Item 4: restored volumes become another VM's disks."""
        first = self.volumes[0]
        p5 = self.guid("mox1", f"{first}#bmac-pit-p5")

        # Method 3: a full receive from the appliance, renamed into local-zfs.
        full = "rpool/bmac-restore/m3/disk"
        self.on("mox1", "zfs create -p -o mountpoint=none rpool/bmac-restore/m3")
        self.pipe(self.app_cmd(f"zfs send {self.target('A', first)}@bmac-pit-p5"),
                  self.host_cmd("mox1", f"zfs receive -s -u -o volmode=none {full}"))
        method2 = f"rpool/bmac-restore/m2/{first.rsplit('/', 1)[-1]}"
        made = {}
        for label, staged in (("full", full), ("method 2", method2)):
            vmid = int(self.on("mox1", "pvesh get /cluster/nextid"))
            disk = f"rpool/data/vm-{vmid}-disk-0"
            self.on("mox1", f"zfs rename {staged} {disk} && zfs inherit volmode {disk} && udevadm settle && "
                            f"qm create {vmid} --name spike-{vmid} --memory 512 --scsihw virtio-scsi-pci && "
                            f"qm set {vmid} --scsi0 local-zfs:vm-{vmid}-disk-0 >/dev/null")
            self.check("4", f"the {label} restore keeps P5's GUID through the rename into local-zfs",
                       lambda disk=disk: self.guid("mox1", f"{disk}@bmac-pit-p5") == p5)
            self.on("mox1", f"pvesr create-local-job {vmid}-0 mox3 --schedule '*/1'")
            made[label] = (vmid, disk)
        for label, (vmid, disk) in made.items():
            self.check("4", f"replication's first sync carries the {label} restore's P5 to mox3 with its GUID",
                       lambda disk=disk: m6.wait_until(lambda: self.exists("mox3", f"{disk}@bmac-pit-p5")
                                                       and self.guid("mox3", f"{disk}@bmac-pit-p5") == p5,
                                                       REPLICATION_TIMEOUT))

        # A local send and receive, and promote (of a clone of that copy,
        # not of a production disk), keep the GUID.
        p4 = self.guid("mox1", f"{first}@bmac-pit-p4")
        self.on("mox1", f"zfs send {first}@bmac-pit-p4 | zfs receive -u -o volmode=none rpool/bmac-spike/copied")
        self.check("4", "a local send and receive keeps P4's GUID",
                   lambda: self.guid("mox1", "rpool/bmac-spike/copied@bmac-pit-p4") == p4)
        self.on("mox1", "zfs clone rpool/bmac-spike/copied@bmac-pit-p4 rpool/bmac-spike/promoted && "
                        "zfs promote rpool/bmac-spike/promoted")
        self.check("4", "zfs promote moves P4's snapshot to the clone with its GUID",
                   lambda: self.guid("mox1", "rpool/bmac-spike/promoted@bmac-pit-p4") == p4
                   and self.on("mox1", "zfs get -H -o value origin rpool/bmac-spike/copied") == "rpool/bmac-spike/promoted@bmac-pit-p4")

        # The restored VM's next PIT: from P5's bookmark, received on the
        # appliance as a clone of its own @P5.
        vmid, disk = made["full"]
        for name in ("mox1", "mox3"):
            self.on(name, f"zfs bookmark {disk}@bmac-pit-p5 {disk}#bmac-pit-p5")
        self.check("4", "the restored P5's bookmarks have its GUID on both hosts", lambda: all(
            self.guid(n, f"{disk}#bmac-pit-p5") == p5 for n in ("mox1", "mox3")))
        self.on("mox1", f"zfs destroy {disk}@bmac-pit-p5 && udevadm settle && {self.scribble('mox1', disk, 128)} "
                        f"&& zfs snapshot {disk}@bmac-pit-p7")
        to = f"{NAMESPACE}/{vmid}/C/disk"
        self.app(f"zfs create -p -o mountpoint=none {to.rsplit('/', 1)[0]}")
        self.pipe(self.host_cmd("mox1", f"zfs send -i {disk}#bmac-pit-p5 {disk}@bmac-pit-p7"),
                  self.app_cmd(f"zfs receive -s -u -o volmode=none -o origin={self.target('A', first)}@bmac-pit-p5 {to}"))
        self.check("4", "the restored VM's next PIT, from P5's bookmark, is received as a clone of the appliance's P5",
                   lambda: self.app(f"zfs get -H -o value origin {to}") == f"{self.target('A', first)}@bmac-pit-p5"
                   and self.app_guid(f"{to}@bmac-pit-p7") == self.guid("mox1", f"{disk}@bmac-pit-p7"))


def then(hosts) -> None:
    s = hosts["mox3"].s
    spike = Spike(s, hosts)
    try:
        spike.prepare_appliance()
        spike.find_vm()
        for step in (spike.bookmarks, spike.clones, spike.restored):
            step()
    finally:
        out = s.lab.paths.root / "m9-spike.json"
        out.write_text(json.dumps(spike.results, indent=1) + "\n")
        s.note(f"the spike's results are in {out}")
        if (spike.lab.paths.vm(spike.appliance.name) / f"checkpoint-{CHECKPOINT}.fd").exists():
            spike.lab.restore_checkpoint(spike.appliance, CHECKPOINT)


def run(s) -> None:
    storage.staged(s, then)
