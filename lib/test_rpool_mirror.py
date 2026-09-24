#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Fake-host tests for lib/rpool_mirror.sh, the shared rpool mirror tool."""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


LIB_DIR = Path(__file__).resolve().parent
TOOL = LIB_DIR / "rpool_mirror.sh"
PASSPHRASE = "correct horse battery staple"
DISK_BYTES = 2000398934016

FAKE_HOST = r'''#!/usr/bin/env python3
"""One fake for every host command the rpool mirror tool runs."""
import json
import os
from pathlib import Path
import sys

command = Path(sys.argv[0]).name
args = sys.argv[1:]
state_path = Path(os.environ["FAKE_STATE"])
state = json.loads(state_path.read_text(encoding="utf-8"))


def save():
    state_path.write_text(json.dumps(state, indent=1, sort_keys=True), encoding="utf-8")


def record(*extra):
    state["actions"].append([command, *args, *extra])
    save()


def disk_of(device):
    for disk, row in state["disks"].items():
        if device == disk or device in row.get("partitions", []):
            return disk
    return None


def leaves():
    return [member for vdev in state["pool"] for member in vdev["members"]]


def key_from(options):
    for option in options:
        if option.startswith("--key-file="):
            source = option.split("=", 1)[1]
            return sys.stdin.read() if source == "-" else Path(source).read_text()
    raise SystemExit("no key file")


if command == "lsblk":
    if args == ["-dnpo", "PATH,SERIAL"]:
        for disk, row in state["disks"].items():
            print(disk, row["serial"])
    elif args[:2] == ["-nrpo", "NAME"]:
        disk = args[2]
        print(disk)
        for part in state["disks"][disk].get("partitions", []):
            print(part)
        for mapper, row in state["mappers"].items():
            if disk_of(row["device"]) == disk:
                print(f"/dev/mapper/{mapper}")
    elif args[:2] == ["-nrpo", "MOUNTPOINT"]:
        for mountpoint in state["disks"][args[2]].get("mountpoints", []):
            print(mountpoint)
    else:
        raise SystemExit(f"unexpected lsblk {args}")
elif command == "blockdev":
    device = args[-1]
    if device.startswith("/dev/mapper/"):
        backing = state["mappers"][device.rsplit("/", 1)[1]]["device"]
        print(state["disks"][disk_of(backing)]["size"] - 16777216)
    else:
        print(state["disks"][device]["size"])
elif command == "zpool":
    if args[:2] == ["status", "-x"]:
        print("pool 'rpool' is healthy")
    elif args[0] == "status":
        print("  pool: rpool\n state: ONLINE\nconfig:\n")
        print("\tNAME STATE READ WRITE CKSUM\n\trpool ONLINE 0 0 0")
        for vdev in state["pool"]:
            print(f"\t  {vdev['name']} ONLINE 0 0 0")
            for member in vdev["members"]:
                print(f"\t    {member} ONLINE 0 0 0")
        print("\nerrors: No known data errors")
    elif args[0] == "add":
        record()
        members = args[args.index("mirror") + 1:]
        state["pool"].append({"name": f"mirror-{len(state['pool'])}", "members": members})
        state["pool_ashift"].append(int(args[args.index("-o") + 1].split("=")[1]))
        save()
    else:
        raise SystemExit(f"unexpected zpool {args}")
elif command == "zdb":
    for value in state["pool_ashift"]:
        print(f"            ashift: {value}")
elif command == "cryptsetup":
    action = args[0]
    options, positional = [], []
    rest = iter(args[1:])
    for value in rest:
        if value in ("--type", "--header-backup-file"):
            next(rest)
        elif value.startswith("--"):
            options.append(value)
        else:
            positional.append(value)
    if action == "status":
        row = state["mappers"].get(positional[0])
        if row is None:
            raise SystemExit(4)
        print(f"/dev/mapper/{positional[0]} is active.\n  type:    LUKS2\n  device:  {row['device']}")
    elif action == "isLuks":
        raise SystemExit(0 if positional[0] in state["luks"] else 1)
    elif action == "luksUUID":
        print(state["luks"][positional[0]]["uuid"])
    elif action == "open":
        device = positional[0]
        key = key_from(options)
        if state["luks"].get(device, {}).get("passphrase") != key:
            raise SystemExit(2)
        if "--test-passphrase" not in options:
            record()
            state["mappers"][positional[1]] = {"device": device}
            save()
    elif action == "luksFormat":
        key = key_from(options)
        record()
        state["luks"][positional[0]] = {
            "passphrase": key,
            "uuid": f"uuid-{state['disks'][positional[0]]['serial']}",
        }
        save()
    elif action == "luksHeaderBackup":
        target = args[args.index("--header-backup-file") + 1]
        Path(target).write_text(f"header of {positional[0]}\n")
        record()
    elif action == "close":
        if state.get("busy_mappers") and positional[0] in state["busy_mappers"]:
            raise SystemExit(5)
        record()
        del state["mappers"][positional[0]]
        save()
    else:
        raise SystemExit(f"unexpected cryptsetup {args}")
elif command in {"wipefs", "sgdisk", "udevadm", "proxmox-boot-tool"}:
    record()
elif command == "update-initramfs":
    record()
    crypttab = Path(os.environ["APP_HA_CRYPTTAB"])
    lines = crypttab.read_text().splitlines() if crypttab.exists() else []
    kept = [line for line in lines if "initramfs" in line]
    if state.get("initramfs_drops"):
        kept = [line for line in kept if line.split()[0] not in state["initramfs_drops"]]
    state["initrd_crypttab"] = kept
    save()
elif command == "unmkinitramfs":
    target = Path(args[1]) / "main"
    (target / "cryptroot").mkdir(parents=True)
    (target / "cryptroot" / "crypttab").write_text(
        "".join(line + "\n" for line in state["initrd_crypttab"])
    )
    if state.get("initrd_keyctl", True):
        scripts = target / "usr" / "lib" / "cryptsetup" / "scripts"
        scripts.mkdir(parents=True)
        (scripts / "decrypt_keyctl").write_text("#!/bin/sh\n")
else:
    raise SystemExit(f"unexpected fake command {command} {args}")
'''

BOOT_LINES = [
    "crypt-rpool-a\tUUID=boot-a\tapp-ha-rpool\tluks,initramfs,nofail,keyscript=decrypt_keyctl",
    "crypt-rpool-b\tUUID=boot-b\tapp-ha-rpool\tluks,initramfs,nofail,keyscript=decrypt_keyctl",
]


# rpool_mirror.sh only ever runs on a Proxmox host (GNU readlink and stat).
# Gate on the platform so a Linux run can never silently skip it.
@unittest.skipUnless(
    sys.platform.startswith("linux"), "rpool_mirror.sh is Proxmox-host only"
)
class RpoolMirrorToolTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.bin_dir = self.root / "bin"
        self.headers = self.root / "headers"
        self.by_id = self.root / "by-id"
        self.work = self.root / "work"
        for path in (self.bin_dir, self.headers, self.by_id, self.work):
            path.mkdir()
        self.crypttab = self.root / "crypttab"
        self.crypttab.write_text("".join(line + "\n" for line in BOOT_LINES))
        self.state_path = self.root / "state.json"
        self.write_state(
            {
                "actions": [],
                "disks": {
                    "/dev/nvme0n1": {
                        "serial": "BOOTA",
                        "size": DISK_BYTES,
                        "partitions": ["/dev/nvme0n1p2", "/dev/nvme0n1p3"],
                    },
                    "/dev/nvme1n1": {
                        "serial": "BOOTB",
                        "size": DISK_BYTES,
                        "partitions": ["/dev/nvme1n1p2", "/dev/nvme1n1p3"],
                    },
                    "/dev/nvme2n1": {"serial": "NEW1", "size": DISK_BYTES},
                    "/dev/nvme3n1": {"serial": "NEW2", "size": DISK_BYTES},
                },
                "luks": {
                    "/dev/nvme0n1p3": {"passphrase": PASSPHRASE, "uuid": "boot-a"},
                    "/dev/nvme1n1p3": {"passphrase": PASSPHRASE, "uuid": "boot-b"},
                },
                "mappers": {
                    "crypt-rpool-a": {"device": "/dev/nvme0n1p3"},
                    "crypt-rpool-b": {"device": "/dev/nvme1n1p3"},
                },
                "pool": [
                    {
                        "name": "mirror-0",
                        "members": [
                            "/dev/mapper/crypt-rpool-a",
                            "/dev/mapper/crypt-rpool-b",
                        ],
                    }
                ],
                "pool_ashift": [12],
                "initrd_crypttab": list(BOOT_LINES),
            }
        )
        fake = self.bin_dir / "fake-host"
        fake.write_text(FAKE_HOST, encoding="utf-8")
        fake.chmod(0o755)
        for name in (
            "lsblk",
            "blockdev",
            "zpool",
            "zdb",
            "cryptsetup",
            "wipefs",
            "sgdisk",
            "udevadm",
            "update-initramfs",
            "proxmox-boot-tool",
            "unmkinitramfs",
        ):
            (self.bin_dir / name).symlink_to(fake)
        self.environment = os.environ.copy()
        self.environment.update(
            {
                "APP_HA_RPOOL_MIRROR_TEST_MODE": "1",
                "APP_HA_CRYPTTAB": str(self.crypttab),
                "APP_HA_HEADER_DIR": str(self.headers),
                "APP_HA_BY_ID_DIR": str(self.by_id),
                "APP_HA_INITRD": str(self.root / "initrd.img"),
                "APP_HA_WORK_DIR": str(self.work),
                "FAKE_STATE": str(self.state_path),
                "PATH": f"{self.bin_dir}:{os.environ['PATH']}",
            }
        )

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_state(self, value: dict) -> None:
        self.state_path.write_text(json.dumps(value, sort_keys=True), encoding="utf-8")

    def state(self) -> dict:
        return json.loads(self.state_path.read_text(encoding="utf-8"))

    def update_state(self, **changes) -> None:
        value = self.state()
        value.update(changes)
        self.write_state(value)

    def run_tool(
        self, *arguments: str, stdin: str = "", expected: int = 0
    ) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [str(TOOL), *arguments],
            input=stdin,
            env=self.environment,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            expected,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        return completed

    def actions(self, command: str | None = None) -> list[list[str]]:
        rows = self.state()["actions"]
        return [row for row in rows if command is None or row[0] == command]

    def prepare(self, stdin: str = f"GO\n{PASSPHRASE}\n") -> None:
        self.run_tool(
            "luks-prepare", "--pair", "2", "--prompt", "--require-equal",
            "NEW1", "NEW2", stdin=stdin,
        )

    # -- check-new --------------------------------------------------------

    def test_check_new_reports_fresh_disks_of_equal_size(self) -> None:
        output = self.run_tool("check-new", "--require-equal", "NEW1", "NEW2").stdout
        self.assertEqual(
            output.splitlines(),
            [
                f"NEW1\t/dev/nvme2n1\t{DISK_BYTES}\tfresh",
                f"NEW2\t/dev/nvme3n1\t{DISK_BYTES}\tfresh",
            ],
        )

    def test_check_new_rejects_unsafe_disks(self) -> None:
        self.assertIn(
            "already part of rpool",
            self.run_tool("check-new", "--require-equal", "NEW1", "BOOTA", expected=1).stderr,
        )
        state = self.state()
        state["disks"]["/dev/nvme3n1"]["size"] = DISK_BYTES + 4096
        self.write_state(state)
        self.assertIn(
            "differ in capacity",
            self.run_tool("check-new", "--require-equal", "NEW1", "NEW2", expected=1).stderr,
        )
        self.assertIn(
            "not the expected",
            self.run_tool(
                "check-new", "--expect-bytes", str(DISK_BYTES), "--match", "exact",
                "NEW1", "NEW2", expected=1,
            ).stderr,
        )
        state["disks"]["/dev/nvme3n1"]["size"] = DISK_BYTES
        state["disks"]["/dev/nvme3n1"]["mountpoints"] = ["/mnt/old"]
        self.write_state(state)
        self.assertIn(
            "mounted filesystem",
            self.run_tool("check-new", "--require-equal", "NEW1", "NEW2", expected=1).stderr,
        )
        self.assertIn(
            "found 0",
            self.run_tool("check-new", "--require-equal", "NEW1", "MISSING", expected=1).stderr,
        )
        self.run_tool("check-new", "--require-equal", "NEW1", "NEW1", expected=1)
        self.run_tool("check-new", "--require-equal", "NEW1", "bad;serial", expected=1)

    # -- luks-prepare -----------------------------------------------------

    def test_prepare_proves_passphrase_before_formatting(self) -> None:
        completed = self.run_tool(
            "luks-prepare", "--pair", "2", "--prompt", "--require-equal", "NEW1", "NEW2",
            stdin=f"GO\nwrong\n{PASSPHRASE}\n",
        )
        self.assertIn("does not unlock existing rpool member crypt-rpool-a", completed.stderr)
        self.assertIn("prepared successfully", completed.stdout)
        state = self.state()
        self.assertEqual(state["luks"]["/dev/nvme2n1"]["passphrase"], PASSPHRASE)
        self.assertEqual(state["luks"]["/dev/nvme3n1"]["passphrase"], PASSPHRASE)
        self.assertEqual(state["mappers"]["crypt-rpool-mirror2-1"]["device"], "/dev/nvme2n1")
        self.assertEqual(state["mappers"]["crypt-rpool-mirror2-2"]["device"], "/dev/nvme3n1")
        for index in (1, 2):
            header = self.headers / f"luks-header-mirror2-{index}.bin"
            self.assertTrue(header.exists())
            self.assertEqual(header.stat().st_mode & 0o777, 0o600)
        self.assertEqual(len(self.actions("wipefs")), 2)
        self.assertNotIn(PASSPHRASE, completed.stdout + completed.stderr)

    def test_prepare_changes_nothing_without_the_shared_passphrase(self) -> None:
        completed = self.run_tool(
            "luks-prepare", "--pair", "2", "--prompt", "--require-equal", "NEW1", "NEW2",
            stdin="GO\nwrong\nstill wrong\n\nagain wrong\n",
            expected=1,
        )
        self.assertIn("nothing was changed", completed.stderr)
        self.assertEqual(self.actions(), [])

    def test_prepare_requires_go(self) -> None:
        self.run_tool(
            "luks-prepare", "--pair", "2", "--prompt", "--require-equal", "NEW1", "NEW2",
            stdin="no\n", expected=1,
        )
        self.assertEqual(self.actions(), [])

    def test_prepare_reuses_matching_luks_and_refuses_foreign_luks(self) -> None:
        state = self.state()
        state["luks"]["/dev/nvme2n1"] = {"passphrase": PASSPHRASE, "uuid": "uuid-NEW1"}
        self.write_state(state)
        self.prepare()
        self.assertEqual(
            [row[-1] for row in self.actions("wipefs")], ["/dev/nvme3n1"]
        )

        self.setUp_fresh_with_foreign_luks()
        completed = self.run_tool(
            "luks-prepare", "--pair", "2", "--prompt", "--require-equal", "NEW1", "NEW2",
            stdin=f"GO\n{PASSPHRASE}\n", expected=1,
        )
        self.assertIn("refusing to reuse or erase it", completed.stderr)
        self.assertEqual(self.actions("wipefs"), [])

    def setUp_fresh_with_foreign_luks(self) -> None:
        state = self.state()
        state["actions"] = []
        state["mappers"] = {
            key: value for key, value in state["mappers"].items()
            if not key.startswith("crypt-rpool-mirror")
        }
        state["luks"]["/dev/nvme2n1"] = {"passphrase": "someone else", "uuid": "x"}
        state["luks"].pop("/dev/nvme3n1", None)
        self.write_state(state)

    def test_prepare_with_setup_key_file(self) -> None:
        key_file = self.root / "key"
        key_file.write_text(PASSPHRASE)
        key_file.chmod(0o600)
        self.run_tool(
            "luks-prepare", "--pair", "3", "--key-file", str(key_file),
            "--expect-bytes", str(DISK_BYTES), "--match", "within-1pct",
            "NEW1", "NEW2", stdin="GO\n",
        )
        self.assertIn("crypt-rpool-mirror3-1", self.state()["mappers"])
        self.run_tool(
            "luks-check-prepared", "--pair", "3", "--expect-bytes", str(DISK_BYTES),
            "NEW1", "NEW2",
        )

    # -- luks-add ---------------------------------------------------------

    def test_add_uses_shared_crypttab_form_and_verifies_boot_unlock(self) -> None:
        self.prepare()
        output = self.run_tool("luks-add", "--pair", "2", "NEW1", "NEW2").stdout
        self.assertIn("shared crypttab form", output)
        lines = self.crypttab.read_text().splitlines()
        self.assertIn(
            "crypt-rpool-mirror2-1\tUUID=uuid-NEW1\tapp-ha-rpool\t"
            "luks,initramfs,nofail,keyscript=decrypt_keyctl",
            lines,
        )
        self.assertEqual(self.crypttab.stat().st_mode & 0o777, 0o600)
        pool = self.state()["pool"]
        self.assertEqual(
            pool[-1]["members"],
            ["/dev/mapper/crypt-rpool-mirror2-1", "/dev/mapper/crypt-rpool-mirror2-2"],
        )
        order = [row[0] for row in self.actions()]
        self.assertLess(order.index("update-initramfs"), order.index("zpool"))
        self.assertIn(["zpool", "add", "-o", "ashift=12", "rpool", "mirror",
                       "/dev/mapper/crypt-rpool-mirror2-1",
                       "/dev/mapper/crypt-rpool-mirror2-2"], self.actions("zpool"))

        # A rerun converges without adding the pair twice.
        self.run_tool("luks-add", "--pair", "2", "NEW1", "NEW2")
        self.assertEqual(len(self.actions("zpool")), 1)
        self.assertEqual(
            sum(line.startswith("crypt-rpool-mirror2-1") for line in
                self.crypttab.read_text().splitlines()),
            1,
        )

    def test_add_keeps_plain_form_during_initial_setup(self) -> None:
        self.crypttab.write_text(
            "crypt-rpool-a UUID=boot-a none luks,initramfs,nofail\n"
            "crypt-rpool-b UUID=boot-b none luks,initramfs,nofail\n"
        )
        self.prepare()
        self.run_tool("luks-add", "--pair", "2", "NEW1", "NEW2")
        self.assertIn(
            "crypt-rpool-mirror2-2 UUID=uuid-NEW2 none luks,initramfs,nofail",
            self.crypttab.read_text().splitlines(),
        )

    def test_add_stops_before_zpool_when_initramfs_would_not_unlock(self) -> None:
        self.prepare()
        self.update_state(initramfs_drops=["crypt-rpool-mirror2-2"])
        completed = self.run_tool("luks-add", "--pair", "2", "NEW1", "NEW2", expected=1)
        self.assertIn("would not unlock crypt-rpool-mirror2-2 at boot", completed.stderr)
        self.assertEqual(self.actions("zpool"), [])

    def test_add_refuses_mixed_ashift_and_mixed_crypttab(self) -> None:
        self.prepare()
        self.update_state(pool_ashift=[12, 9])
        self.assertIn(
            "do not share one ashift",
            self.run_tool("luks-add", "--pair", "2", "NEW1", "NEW2", expected=1).stderr,
        )
        self.update_state(pool_ashift=[12])
        with self.crypttab.open("a") as stream:
            stream.write("crypt-rpool-mirror3-1 UUID=z none luks,initramfs,nofail\n")
        self.assertIn(
            "mixes shared-unlock and plain",
            self.run_tool("luks-add", "--pair", "2", "NEW1", "NEW2", expected=1).stderr,
        )
        self.assertEqual(self.actions("zpool"), [])

    # -- clear-add --------------------------------------------------------

    def make_clear_pool(self) -> None:
        state = self.state()
        state["luks"] = {}
        state["mappers"] = {}
        state["pool"] = [
            {
                "name": "mirror-0",
                "members": [
                    "/dev/disk/by-id/nvme-Dell_BOOTA-part3",
                    "/dev/disk/by-id/nvme-Dell_BOOTB-part3",
                ],
            }
        ]
        self.write_state(state)
        for serial, disk in (("NEW1", "/dev/nvme2n1"), ("NEW2", "/dev/nvme3n1")):
            (self.by_id / f"nvme-Dell_{serial}").symlink_to(disk)
            (self.by_id / f"nvme-Dell_{serial}-part1").symlink_to(f"{disk}p1")

    def test_clear_add_uses_stable_paths_and_is_idempotent(self) -> None:
        self.make_clear_pool()
        self.run_tool("clear-add", "--require-equal", "NEW1", "NEW2")
        self.assertEqual(
            self.state()["pool"][-1]["members"],
            [str(self.by_id / "nvme-Dell_NEW1"), str(self.by_id / "nvme-Dell_NEW2")],
        )
        self.assertEqual(len(self.actions("wipefs")), 2)
        output = self.run_tool("clear-add", "--require-equal", "NEW1", "NEW2").stdout
        self.assertIn("already one rpool mirror", output)
        self.assertEqual(len(self.actions("zpool")), 1)

    def test_clear_add_refuses_encrypted_pool_and_luks_disks(self) -> None:
        self.assertIn(
            "rpool is encrypted",
            self.run_tool("clear-add", "--require-equal", "NEW1", "NEW2", expected=1).stderr,
        )
        self.make_clear_pool()
        state = self.state()
        state["luks"]["/dev/nvme2n1"] = {"passphrase": "x", "uuid": "x"}
        self.write_state(state)
        self.assertIn(
            "refusing to erase existing LUKS disk NEW1",
            self.run_tool("clear-add", "--require-equal", "NEW1", "NEW2", expected=1).stderr,
        )
        self.assertEqual(self.actions("wipefs"), [])

    # -- retire-luks ------------------------------------------------------

    def test_retire_closes_removes_and_rebuilds_only_after_removal(self) -> None:
        self.prepare()
        self.run_tool("luks-add", "--pair", "2", "NEW1", "NEW2")
        self.assertIn(
            "still part of rpool",
            self.run_tool(
                "retire-luks", "crypt-rpool-mirror2-1", "crypt-rpool-mirror2-2",
                expected=1,
            ).stderr,
        )
        # The vdev removal completed: the mirror left the pool.
        self.update_state(pool=self.state()["pool"][:1])
        self.run_tool("retire-luks", "crypt-rpool-mirror2-1", "crypt-rpool-mirror2-2")
        state = self.state()
        self.assertNotIn("crypt-rpool-mirror2-1", state["mappers"])
        self.assertNotIn("crypt-rpool-mirror2", self.crypttab.read_text())
        self.assertIn("crypt-rpool-a", self.crypttab.read_text())
        self.assertFalse(
            any(line.startswith("crypt-rpool-mirror2") for line in state["initrd_crypttab"])
        )
        rebuilds = len(self.actions("update-initramfs"))
        self.run_tool("retire-luks", "crypt-rpool-mirror2-1", "crypt-rpool-mirror2-2")
        self.assertEqual(len(self.actions("update-initramfs")), rebuilds)

    def test_retire_refuses_boot_mirror_and_busy_mapper(self) -> None:
        self.assertIn(
            "only crypt-rpool-mirrorN-M",
            self.run_tool("retire-luks", "crypt-rpool-a", expected=1).stderr,
        )
        self.prepare()
        self.update_state(busy_mappers=["crypt-rpool-mirror2-1"])
        self.assertIn(
            "something still holds it open",
            self.run_tool("retire-luks", "crypt-rpool-mirror2-1", expected=1).stderr,
        )

    def test_shell_syntax(self) -> None:
        completed = subprocess.run(
            ["bash", "-n", str(TOOL)], text=True, capture_output=True, check=False
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)


if __name__ == "__main__":
    unittest.main()
