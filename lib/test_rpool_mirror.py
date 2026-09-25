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


def member_path(member):
    return member if isinstance(member, str) else member["path"]


def member_state(member):
    return "ONLINE" if isinstance(member, str) else member["state"]


def boot_uuids():
    path = Path(os.environ["APP_HA_BOOT_UUIDS"])
    return path.read_text().split() if path.exists() else []


def write_boot_uuids(uuids):
    Path(os.environ["APP_HA_BOOT_UUIDS"]).write_text("".join(u + "\n" for u in uuids))


def esp_config(uuid):
    mode = state.get("esp_mode", {}).get(uuid, "uefi")
    versions = state.get("kernels", "6.14.8-2-pve")
    if uuid in state.get("unsynced_esps", []):
        versions = ""
    return f"{mode} (versions: {versions})"


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
    elif args == ["-nrpo", "NAME"]:
        for disk, row in state["disks"].items():
            print(disk)
            for part in row.get("partitions", []):
                print(part)
        for mapper in state["mappers"]:
            print(f"/dev/mapper/{mapper}")
    elif args[:2] == ["-nrpo", "NAME"]:
        device = args[2]
        print(device)
        if device in state["disks"]:
            for part in state["disks"][device].get("partitions", []):
                print(part)
        for mapper, row in state["mappers"].items():
            if row["device"] == device or (
                device in state["disks"] and disk_of(row["device"]) == device
            ):
                print(f"/dev/mapper/{mapper}")
    elif args[:2] == ["-nrpo", "MOUNTPOINT"]:
        for mountpoint in state["disks"].get(args[2], {}).get("mountpoints", []):
            print(mountpoint)
        for mountpoint in state.get("partition_mountpoints", {}).get(args[2], []):
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
            if vdev.get("single"):
                print(f"\t  {vdev['name']} {vdev.get('state', 'ONLINE')} 0 0 0")
                continue
            print(f"\t  {vdev['name']} ONLINE 0 0 0")
            for member in vdev["members"]:
                if isinstance(member, dict) and "replacing" in member:
                    print(f"\t    {member['name']} DEGRADED 0 0 0")
                    for inner in member["replacing"]:
                        print(f"\t      {member_path(inner)} {member_state(inner)} 0 0 0")
                else:
                    print(f"\t    {member_path(member)} {member_state(member)} 0 0 0")
        print("\nerrors: No known data errors")
    elif args[0] == "replace":
        record()
        old, new = args[2], args[3]
        for vdev in state["pool"]:
            for index, member in enumerate(vdev.get("members", [])):
                if isinstance(member, dict) and "replacing" in member:
                    continue
                if member_path(member) == old:
                    old_member = {"path": old, "state": member_state(member)}
                    if old == new:
                        old_member["path"] = f"{old}/old"
                    vdev["members"][index] = {
                        "name": "replacing-1",
                        "replacing": [old_member, {"path": new, "state": "ONLINE"}],
                    }
                    save()
                    raise SystemExit(0)
        raise SystemExit(f"no such device in pool: {old}")
    elif args[0] == "attach":
        record()
        for index, vdev in enumerate(state["pool"]):
            if vdev.get("single") and vdev["name"] == args[2]:
                state["pool"][index] = {
                    "name": f"mirror-{index}", "members": [args[2], args[3]],
                }
                save()
                raise SystemExit(0)
        raise SystemExit(f"no single-disk vdev {args[2]}")
    elif args[0] == "offline":
        record()
        for vdev in state["pool"]:
            for index, member in enumerate(vdev.get("members", [])):
                if member_path(member) == args[2]:
                    vdev["members"][index] = {"path": args[2], "state": "OFFLINE"}
        save()
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
        print(f"/dev/mapper/{positional[0]} is active.\n  type:    LUKS2\n  device:  {row['device'] or '(null)'}")
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
            "uuid": f"uuid-{state['disks'][disk_of(positional[0])]['serial']}",
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
elif command == "sgdisk":
    if args[0] == "--print":
        print(f"Disk {args[1]}: sectors\nNumber  Start (sector)    End (sector)  Size       Code  Name")
        for number, start, end, code in state["disks"][args[1]].get("gpt", []):
            print(f"   {number}   {start}   {end}   1.0 GiB   {code}  ")
        raise SystemExit(0)
    record()
    if args[0] == "--zap-all":
        state["disks"][args[1]]["gpt"] = []
        state["disks"][args[1]]["partitions"] = []
    elif len(args) == 2 and args[1].startswith("--replicate="):
        target = args[1].split("=", 1)[1]
        state["disks"][target]["gpt"] = [list(row) for row in state["disks"][args[0]]["gpt"]]
        state["disks"][target]["partitions"] = [
            f"{target}p{row[0]}" for row in state["disks"][args[0]]["gpt"]
        ]
    save()
elif command == "blkid":
    uuid = state.get("fs_uuid", {}).get(args[-1])
    if uuid is None:
        raise SystemExit(2)
    print(uuid)
elif command == "proxmox-boot-tool":
    if args == ["status"]:
        print("System currently booted with uefi")
        present = set(state.get("fs_uuid", {}).values())
        for uuid in boot_uuids():
            if uuid in present:
                print(f"{uuid} is configured with: {esp_config(uuid)}")
            else:
                print(f"WARN: /dev/disk/by-uuid/{uuid} does not exist - clean '/etc/kernel/proxmox-boot-uuids'! - skipping")
        raise SystemExit(0)
    record()
    if args[0] == "format":
        state.setdefault("fs_uuid", {})[args[1]] = "NEW0-" + args[1][-4:].upper()
    elif args[0] == "init":
        uuid = state["fs_uuid"][args[1]]
        write_boot_uuids(boot_uuids() + [uuid])
        state.setdefault("esp_mode", {})[uuid] = args[2] if len(args) > 2 else "uefi"
    elif args[0] == "clean":
        present = set(state.get("fs_uuid", {}).values())
        write_boot_uuids([uuid for uuid in boot_uuids() if uuid in present])
    save()
elif command in {"wipefs", "udevadm", "partx"}:
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

BOOT_GPT = [
    [1, 34, 2047, "EF02"],
    [2, 2048, 2099199, "EF00"],
    [3, 2099200, 3907029134, "BF01"],
]

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
                        "gpt": BOOT_GPT,
                        "partitions": [
                            "/dev/nvme0n1p1", "/dev/nvme0n1p2", "/dev/nvme0n1p3",
                        ],
                    },
                    "/dev/nvme1n1": {
                        "serial": "BOOTB",
                        "size": DISK_BYTES,
                        "gpt": BOOT_GPT,
                        "partitions": [
                            "/dev/nvme1n1p1", "/dev/nvme1n1p2", "/dev/nvme1n1p3",
                        ],
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
                "fs_uuid": {
                    "/dev/nvme0n1p2": "AAAA-1111",
                    "/dev/nvme1n1p2": "BBBB-2222",
                },
            }
        )
        self.boot_uuids = self.root / "proxmox-boot-uuids"
        self.boot_uuids.write_text("AAAA-1111\nBBBB-2222\n")
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
            "blkid",
            "partx",
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
                "APP_HA_BOOT_UUIDS": str(self.boot_uuids),
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

    # -- replacing a pulled member ------------------------------------------

    def pull_boot_b(self, rebooted: bool) -> None:
        """Disk BOOTB failed and was pulled; NEW1 was installed in its place."""
        state = self.state()
        del state["disks"]["/dev/nvme1n1"]
        del state["fs_uuid"]["/dev/nvme1n1p2"]
        if rebooted:
            del state["mappers"]["crypt-rpool-b"]
            gone = {"path": "8093158935163316232", "state": "UNAVAIL"}
        else:
            state["mappers"]["crypt-rpool-b"] = {"device": None}
            gone = {"path": "/dev/mapper/crypt-rpool-b", "state": "REMOVED"}
        state["pool"][0]["members"][1] = gone
        self.write_state(state)

    def replace_boot_member(self) -> None:
        self.run_tool("boot-partition", "--survivor", "BOOTA", "NEW1")
        self.run_tool("boot-esp", "--survivor", "BOOTA", "NEW1")
        self.run_tool(
            "luks-prepare-member", "--member", "B", "--prompt", "NEW1",
            stdin=f"GO\n{PASSPHRASE}\n",
        )
        self.run_tool("luks-check-member", "--member", "B", "NEW1")

    def test_boot_member_replacement_after_reboot(self) -> None:
        self.pull_boot_b(rebooted=True)
        output = self.run_tool("boot-partition", "--survivor", "BOOTA", "NEW1").stdout
        self.assertIn("now has the partition table of BOOTA", output)
        self.assertIn(["sgdisk", "/dev/nvme0n1", "--replicate=/dev/nvme2n1"], self.actions("sgdisk"))
        self.assertIn(["sgdisk", "--randomize-guids", "/dev/nvme2n1"], self.actions("sgdisk"))
        self.assertEqual(self.state()["disks"]["/dev/nvme2n1"]["gpt"], BOOT_GPT)
        # The survivor's table is only ever read.
        self.assertFalse(any(
            "/dev/nvme0n1" in row[1:] and "--print" not in row
            for row in self.actions("sgdisk") if row[1] != "/dev/nvme0n1"
        ))
        wipes = len(self.actions("wipefs"))
        rerun = self.run_tool("boot-partition", "--survivor", "BOOTA", "NEW1").stdout
        self.assertIn("already has the partition table", rerun)
        self.assertEqual(len(self.actions("wipefs")), wipes)

        output = self.run_tool("boot-esp", "--survivor", "BOOTA", "NEW1").stdout
        self.assertIn("hold the same boot files: uefi (versions: 6.14.8-2-pve)", output)
        tool = [row[1] for row in self.actions("proxmox-boot-tool")]
        self.assertEqual(tool, ["format", "init", "clean", "refresh"])
        self.assertEqual(self.boot_uuids.read_text().split(), ["AAAA-1111", "NEW0-N1P2"])
        self.run_tool("boot-esp", "--survivor", "BOOTA", "NEW1")
        self.assertEqual(
            [row[1] for row in self.actions("proxmox-boot-tool")].count("format"), 1
        )

        completed = self.run_tool(
            "luks-prepare-member", "--member", "B", "--prompt", "NEW1",
            stdin=f"GO\n{PASSPHRASE}\n",
        )
        self.assertIn("LUKS member B prepared successfully", completed.stdout)
        self.assertNotIn(PASSPHRASE, completed.stdout + completed.stderr)
        state = self.state()
        self.assertEqual(state["luks"]["/dev/nvme2n1p3"]["passphrase"], PASSPHRASE)
        self.assertEqual(state["mappers"]["crypt-rpool-b"]["device"], "/dev/nvme2n1p3")
        self.assertTrue((self.headers / "luks-header-B.bin").exists())
        # Only partition 3 is formatted; the new partition table is kept.
        self.assertNotIn(["sgdisk", "--zap-all", "/dev/nvme2n1"], self.actions("sgdisk")[2:])

        output = self.run_tool(
            "replace-member", "--survivor", "BOOTA", "--member", "B", "NEW1"
        ).stdout
        self.assertIn("ZFS is resilvering it in the background", output)
        self.assertIn(
            "crypt-rpool-b\tUUID=uuid-NEW1\tapp-ha-rpool\t"
            "luks,initramfs,nofail,keyscript=decrypt_keyctl",
            self.crypttab.read_text().splitlines(),
        )
        self.assertNotIn("UUID=boot-b", self.crypttab.read_text())
        self.assertEqual(
            self.actions("zpool"),
            [["zpool", "replace", "rpool", "8093158935163316232", "/dev/mapper/crypt-rpool-b"]],
        )
        order = [row[0] for row in self.actions()]
        self.assertLess(order.index("update-initramfs"), order.index("zpool"))

        rerun = self.run_tool("replace-member", "--survivor", "BOOTA", "--member", "B", "NEW1")
        self.assertIn("already part of mirror-0", rerun.stdout)
        self.assertEqual(len(self.actions("zpool")), 1)

    def test_boot_member_replacement_closes_the_pulled_disks_mapping(self) -> None:
        self.pull_boot_b(rebooted=False)
        self.run_tool("boot-partition", "--survivor", "BOOTA", "NEW1")
        self.run_tool("boot-esp", "--survivor", "BOOTA", "NEW1")
        self.update_state(busy_mappers=["crypt-rpool-b"])
        completed = self.run_tool(
            "luks-prepare-member", "--member", "B", "--prompt", "NEW1",
            stdin=f"GO\n{PASSPHRASE}\n", expected=1,
        )
        self.assertIn("gracefully reboot it, and rerun", completed.stderr)
        self.assertEqual(self.actions("cryptsetup"), [])

        self.update_state(busy_mappers=[])
        completed = self.run_tool(
            "luks-prepare-member", "--member", "B", "--prompt", "NEW1",
            stdin=f"GO\n{PASSPHRASE}\n",
        )
        self.assertIn("Closed the leftover mapping crypt-rpool-b", completed.stdout)
        self.assertEqual(
            self.actions("zpool"), [["zpool", "offline", "rpool", "/dev/mapper/crypt-rpool-b"]]
        )
        self.assertEqual(self.state()["mappers"]["crypt-rpool-b"]["device"], "/dev/nvme2n1p3")
        self.run_tool("replace-member", "--survivor", "BOOTA", "--member", "B", "NEW1")
        self.assertEqual(
            self.actions("zpool")[-1],
            ["zpool", "replace", "rpool", "/dev/mapper/crypt-rpool-b",
             "/dev/mapper/crypt-rpool-b"],
        )

    def test_replace_refuses_a_failed_disk_that_is_still_installed(self) -> None:
        state = self.state()
        state["pool"][0]["members"][1] = {"path": "/dev/mapper/crypt-rpool-b", "state": "FAULTED"}
        self.write_state(state)
        completed = self.run_tool(
            "replace-member", "--survivor", "BOOTA", "--member", "B", "NEW1", expected=1
        )
        self.assertIn("is still installed", completed.stderr)
        self.assertEqual(self.actions(), [])
        # A healthy mirror has nothing to replace.
        self.update_state(pool=[{"name": "mirror-0", "members": [
            "/dev/mapper/crypt-rpool-a", "/dev/mapper/crypt-rpool-b"]}])
        self.assertIn(
            "is not missing a member",
            self.run_tool(
                "replace-member", "--survivor", "BOOTA", "--member", "B", "NEW1", expected=1
            ).stderr,
        )

    def test_boot_member_joins_only_with_a_synced_esp(self) -> None:
        self.pull_boot_b(rebooted=True)
        self.run_tool("boot-partition", "--survivor", "BOOTA", "NEW1")
        self.update_state(unsynced_esps=["NEW0-N1P2"])
        self.assertIn(
            "does not match the surviving ESP",
            self.run_tool("boot-esp", "--survivor", "BOOTA", "NEW1", expected=1).stderr,
        )
        self.run_tool(
            "luks-prepare-member", "--member", "B", "--prompt", "NEW1",
            stdin=f"GO\n{PASSPHRASE}\n",
        )
        self.assertIn(
            "does not match the surviving ESP",
            self.run_tool(
                "replace-member", "--survivor", "BOOTA", "--member", "B", "NEW1", expected=1
            ).stderr,
        )
        self.assertEqual(self.actions("zpool"), [])

    def test_prepare_member_refuses_a_mounted_partition(self) -> None:
        self.pull_boot_b(rebooted=True)
        self.run_tool("boot-partition", "--survivor", "BOOTA", "NEW1")
        self.update_state(partition_mountpoints={"/dev/nvme2n1p3": ["/mnt/x"]})
        self.assertIn(
            "has a mounted filesystem",
            self.run_tool(
                "luks-prepare-member", "--member", "B", "--prompt", "NEW1",
                stdin=f"GO\n{PASSPHRASE}\n", expected=1,
            ).stderr,
        )
        self.assertEqual(self.actions("cryptsetup"), [])

    def test_grub_survivor_gets_a_grub_esp(self) -> None:
        self.pull_boot_b(rebooted=True)
        self.update_state(esp_mode={"AAAA-1111": "grub"})
        self.run_tool("boot-partition", "--survivor", "BOOTA", "NEW1")
        self.run_tool("boot-esp", "--survivor", "BOOTA", "NEW1")
        self.assertIn(
            ["proxmox-boot-tool", "init", "/dev/nvme2n1p2", "grub"],
            self.actions("proxmox-boot-tool"),
        )

    def test_boot_partition_refuses_unsafe_disks(self) -> None:
        self.pull_boot_b(rebooted=True)
        state = self.state()
        state["disks"]["/dev/nvme2n1"]["size"] = DISK_BYTES - 512
        self.write_state(state)
        self.assertIn(
            "not the expected",
            self.run_tool("boot-partition", "--survivor", "BOOTA", "NEW1", expected=1).stderr,
        )
        state["disks"]["/dev/nvme2n1"]["size"] = DISK_BYTES
        state["luks"]["/dev/nvme2n1"] = {"passphrase": "someone else", "uuid": "x"}
        self.write_state(state)
        self.assertIn(
            "is not erased automatically",
            self.run_tool("boot-partition", "--survivor", "BOOTA", "NEW1", expected=1).stderr,
        )
        self.assertIn(
            "surviving disk NEW2 has no boot-mirror partition table",
            self.run_tool("boot-partition", "--survivor", "NEW2", "NEW1", expected=1).stderr,
        )
        self.assertEqual(self.actions("wipefs"), [])

    def test_extra_member_replacement(self) -> None:
        self.prepare()
        self.run_tool("luks-add", "--pair", "2", "NEW1", "NEW2")
        state = self.state()
        del state["disks"]["/dev/nvme3n1"]
        del state["mappers"]["crypt-rpool-mirror2-2"]
        state["pool"][1]["members"][1] = {"path": "5550001112223334445", "state": "UNAVAIL"}
        state["disks"]["/dev/nvme4n1"] = {"serial": "NEW3", "size": DISK_BYTES}
        state["actions"] = []
        self.write_state(state)
        self.assertIn(
            "not the expected",
            self.run_tool(
                "luks-prepare-member", "--member", "2-2", "--prompt",
                "--expect-bytes", str(DISK_BYTES + 1), "--match", "exact", "NEW3",
                stdin=f"GO\n{PASSPHRASE}\n", expected=1,
            ).stderr,
        )
        self.run_tool(
            "luks-prepare-member", "--member", "2-2", "--prompt",
            "--expect-bytes", str(DISK_BYTES), "--match", "exact", "NEW3",
            stdin=f"GO\n{PASSPHRASE}\n",
        )
        self.assertEqual(self.state()["mappers"]["crypt-rpool-mirror2-2"]["device"], "/dev/nvme4n1")
        self.assertEqual([row[-1] for row in self.actions("wipefs")], ["/dev/nvme4n1"])
        self.assertTrue((self.headers / "luks-header-mirror2-2.bin").exists())
        self.run_tool("replace-member", "--survivor", "NEW1", "--member", "2-2", "NEW3")
        self.assertEqual(
            self.actions("zpool"),
            [["zpool", "replace", "rpool", "5550001112223334445",
              "/dev/mapper/crypt-rpool-mirror2-2"]],
        )
        self.assertIn("crypt-rpool-mirror2-2\tUUID=uuid-NEW3", self.crypttab.read_text())
        self.assertEqual(self.actions("proxmox-boot-tool")[-1][1], "refresh")
        self.assertIn(
            "is an extra-mirror member but BOOTA is a boot-mirror disk",
            self.run_tool(
                "replace-member", "--survivor", "BOOTA", "--member", "2-2", "NEW3", expected=1
            ).stderr,
        )

    def test_clear_boot_member_replacement(self) -> None:
        self.make_clear_pool()
        state = self.state()
        state["pool"][0]["members"][1] = {"path": "8093158935163316232", "state": "UNAVAIL"}
        del state["disks"]["/dev/nvme1n1"]
        del state["fs_uuid"]["/dev/nvme1n1p2"]
        self.write_state(state)
        (self.by_id / "nvme-Dell_BOOTA-part3").symlink_to("/dev/nvme0n1p3")
        (self.by_id / "nvme-Dell_NEW1-part3").symlink_to("/dev/nvme2n1p3")
        self.state_path.write_text(self.state_path.read_text().replace(
            '"/dev/disk/by-id/nvme-Dell_BOOTA-part3"', json.dumps(str(self.by_id / "nvme-Dell_BOOTA-part3"))
        ))
        self.assertIn(
            "run boot-partition first",
            self.run_tool("replace-member", "--survivor", "BOOTA", "NEW1", expected=1).stderr,
        )
        self.run_tool("boot-partition", "--survivor", "BOOTA", "NEW1")
        self.assertIn(
            "run boot-esp first",
            self.run_tool("replace-member", "--survivor", "BOOTA", "NEW1", expected=1).stderr,
        )
        self.run_tool("boot-esp", "--survivor", "BOOTA", "NEW1")
        self.run_tool("replace-member", "--survivor", "BOOTA", "NEW1")
        self.assertEqual(
            self.actions("zpool"),
            [["zpool", "replace", "rpool", "8093158935163316232",
              str(self.by_id / "nvme-Dell_NEW1-part3")]],
        )
        self.assertEqual(self.actions("update-initramfs"), [])

    def test_clear_mirror_detached_to_one_disk_gets_attached(self) -> None:
        self.make_clear_pool()
        state = self.state()
        state["pool"].append(
            {"name": str(self.by_id / "nvme-Dell_NEW1"), "single": True, "members": []}
        )
        state["disks"]["/dev/nvme4n1"] = {"serial": "NEW3", "size": DISK_BYTES}
        self.write_state(state)
        (self.by_id / "nvme-Dell_NEW3").symlink_to("/dev/nvme4n1")
        self.run_tool("replace-member", "--survivor", "NEW1", "NEW3")
        self.assertEqual(
            self.actions("zpool"),
            [["zpool", "attach", "rpool", str(self.by_id / "nvme-Dell_NEW1"),
              str(self.by_id / "nvme-Dell_NEW3")]],
        )
        self.assertEqual([row[-1] for row in self.actions("wipefs")], ["/dev/nvme4n1"])
        self.assertIn(
            "already part of mirror-1",
            self.run_tool("replace-member", "--survivor", "NEW1", "NEW3").stdout,
        )

    def test_setup_converts_a_boot_member_with_its_key_file(self) -> None:
        # Setup detached raw member A; B is still raw in the pool.
        key_file = self.root / "key"
        key_file.write_text(PASSPHRASE)
        key_file.chmod(0o600)
        state = self.state()
        state["luks"] = {}
        state["mappers"] = {}
        state["pool"] = [{"name": "/dev/nvme1n1p3", "single": True, "members": []}]
        self.write_state(state)
        self.crypttab.write_text("")
        self.run_tool(
            "luks-prepare-member", "--member", "A", "--key-file", str(key_file), "BOOTA",
            stdin="GO\n",
        )
        state = self.state()
        self.assertEqual(state["mappers"]["crypt-rpool-a"]["device"], "/dev/nvme0n1p3")
        self.assertTrue((self.headers / "luks-header-A.bin").exists())
        self.assertEqual(self.actions("wipefs"), [])
        self.run_tool("luks-register", "--member", "A", "BOOTA")
        self.assertEqual(
            self.crypttab.read_text(),
            "crypt-rpool-a UUID=uuid-BOOTA none luks,initramfs,nofail\n",
        )
        # A prompted run needs an encrypted pool to prove the passphrase.
        self.assertIn(
            "this host is not encrypted",
            self.run_tool(
                "luks-prepare-member", "--member", "A", "--prompt", "BOOTA",
                stdin=f"GO\n{PASSPHRASE}\n", expected=1,
            ).stderr,
        )

    def test_shell_syntax(self) -> None:
        completed = subprocess.run(
            ["bash", "-n", str(TOOL)], text=True, capture_output=True, check=False
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)


if __name__ == "__main__":
    unittest.main()
