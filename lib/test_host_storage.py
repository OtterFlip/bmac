#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Fixture tests for the read-only rpool layout collector and renderer."""

from __future__ import annotations

import contextlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest


sys.path.insert(0, str(Path(__file__).resolve().parent))
import host_storage  # noqa: E402


LUKS_STATUS = """\
  pool: rpool
 state: ONLINE
  scan: scrub repaired 0B in 00:04:12 with 0 errors on Sun Sep 14 00:28:13 2026
remove: Evacuation of mirror-1 in progress since Wed Sep 24 10:00:00 2026
\t21.5G copied out of 40.0G at 120M/s, 53.75% done, 0h2m to go
\t4.21M memory used for removed device mappings
config:

\tNAME                                   STATE     READ WRITE CKSUM
\trpool                                  ONLINE       0     0     0
\t  mirror-0                             ONLINE       0     0     0
\t    /dev/mapper/crypt-rpool-a          ONLINE       0     0     0
\t    /dev/mapper/crypt-rpool-b          ONLINE       0     0     0
\t  mirror-1                             ONLINE       0     0     0
\t    /dev/mapper/crypt-rpool-mirror2-1  ONLINE       0     0     0
\t    /dev/mapper/crypt-rpool-mirror2-2  ONLINE       0     0     0
\t  mirror-2                             ONLINE       0     0     0
\t    /dev/mapper/crypt-rpool-mirror3-1  ONLINE       0     0     0
\t    /dev/mapper/crypt-rpool-mirror3-2  ONLINE       0     0     0

errors: No known data errors
"""

# The pool row follows -o; OpenZFS prints vdev and member rows with its fixed
# ten columns (name size alloc free ckpoint expandsz frag cap dedup health).
LUKS_LIST = """\
rpool\t5989000000000\t1200000000000\t4789000000000\tONLINE
\tmirror-0\t1990000000000\t800000000000\t1190000000000\t-\t-\t4\t40\t-\tONLINE
\t/dev/mapper/crypt-rpool-a\t1995000000000\t-\t-\t-\t-\t-\t-\t-\tONLINE
\t/dev/mapper/crypt-rpool-b\t1995000000000\t-\t-\t-\t-\t-\t-\t-\tONLINE
\tmirror-1\t1999000000000\t300000000000\t1699000000000\t-\t-\t2\t15\t-\tONLINE
\t/dev/mapper/crypt-rpool-mirror2-1\t2000000000000\t-\t-\t-\t-\t-\t-\t-\tONLINE
\t/dev/mapper/crypt-rpool-mirror2-2\t2000000000000\t-\t-\t-\t-\t-\t-\t-\tONLINE
\tmirror-2\t2000000000000\t100000000000\t1900000000000\t-\t-\t1\t5\t-\tONLINE
\t/dev/mapper/crypt-rpool-mirror3-1\t2000000000000\t-\t-\t-\t-\t-\t-\t-\tONLINE
\t/dev/mapper/crypt-rpool-mirror3-2\t2000000000000\t-\t-\t-\t-\t-\t-\t-\tONLINE
"""

POOL_PROPERTIES = """\
size\t5989000000000
allocated\t1200000000000
free\t4789000000000
capacity\t20
fragmentation\t3
health\tONLINE
"""

DATASET_PROPERTIES = "available\t4600000000000\nused\t1200000000000\n"


def disk(name, serial, size, children=(), fstype=None, mountpoints=(None,)):
    return {
        "name": name,
        "kname": name,
        "type": "disk",
        "size": size,
        "serial": serial,
        "model": "Dell Ent NVMe",
        "wwn": None,
        "tran": "nvme",
        "fstype": fstype,
        "uuid": None,
        "mountpoints": list(mountpoints),
        "children": list(children),
    }


def part(name, fstype=None, uuid=None, children=(), mountpoints=(None,)):
    return {
        "name": name,
        "kname": name,
        "type": "part",
        "size": 1000,
        "serial": None,
        "model": None,
        "wwn": None,
        "tran": None,
        "fstype": fstype,
        "uuid": uuid,
        "mountpoints": list(mountpoints),
        "children": list(children),
    }


def crypt(mapper, kname):
    return {
        "name": f"/dev/mapper/{mapper}",
        "kname": kname,
        "type": "crypt",
        "size": 1995000000000,
        "serial": None,
        "model": None,
        "wwn": None,
        "tran": None,
        "fstype": "zfs_member",
        "uuid": None,
        "mountpoints": [None],
        "children": [],
    }


def boot_disk(index, serial, esp_uuid, mapper):
    base = f"/dev/nvme{index}n1"
    return disk(
        base,
        serial,
        2000398934016,
        children=[
            part(f"{base}p1"),
            part(f"{base}p2", fstype="vfat", uuid=esp_uuid),
            part(
                f"{base}p3",
                fstype="crypto_LUKS",
                children=[crypt(mapper, f"/dev/dm-{index}")],
            ),
        ],
    )


LUKS_LSBLK = json.dumps(
    {
        "blockdevices": [
            boot_disk(0, "S0BOOTA", "AAAA-1111", "crypt-rpool-a"),
            boot_disk(1, "S1BOOTB", "BBBB-2222", "crypt-rpool-b"),
            disk(
                "/dev/nvme2n1",
                "S2EXTRA1",
                2000398934016,
                fstype="crypto_LUKS",
                children=[crypt("crypt-rpool-mirror2-1", "/dev/dm-2")],
            ),
            disk(
                "/dev/nvme3n1",
                "S3EXTRA2",
                2000398934016,
                fstype="crypto_LUKS",
                children=[crypt("crypt-rpool-mirror2-2", "/dev/dm-3")],
            ),
            disk(
                "/dev/nvme4n1",
                "S4EXTRA3",
                2000398934016,
                fstype="crypto_LUKS",
                children=[crypt("crypt-rpool-mirror3-1", "/dev/dm-4")],
            ),
            disk(
                "/dev/nvme5n1",
                "S5EXTRA4",
                2000398934016,
                fstype="crypto_LUKS",
                children=[crypt("crypt-rpool-mirror3-2", "/dev/dm-5")],
            ),
            disk("/dev/nvme6n1", "S6BLANK", 4000787030016),
            disk(
                "/dev/nvme7n1",
                "S7USED",
                4000787030016,
                children=[part("/dev/nvme7n1p1", fstype="ext4")],
            ),
            {
                **disk("/dev/zd0", None, 1099511627776),
                "tran": None,
            },
        ]
    }
)

LUKS_REALPATHS = {
    "/dev/mapper/crypt-rpool-a": "/dev/dm-0",
    "/dev/mapper/crypt-rpool-b": "/dev/dm-1",
    "/dev/mapper/crypt-rpool-mirror2-1": "/dev/dm-2",
    "/dev/mapper/crypt-rpool-mirror2-2": "/dev/dm-3",
    "/dev/mapper/crypt-rpool-mirror3-1": "/dev/dm-4",
    "/dev/mapper/crypt-rpool-mirror3-2": "/dev/dm-5",
}

VOLUMES = """\
rpool/data/vm-100-disk-0\t1099511627776\t53687091200\t0\t1073741824
rpool/data/vm-100-disk-1\t4194304\t4194304\t0\t0
rpool/data/vm-101-disk-0\t68719476736\t70866960384\t70866960384\t0
rpool/data/vm-200-disk-0\t1099511627776\t2147483648\t0\t0
"""

SNAPSHOTS = """\
rpool/data/vm-100-disk-0@__replicate_100-0_1758700000__\t1048576\t1758700000
rpool/data/vm-100-disk-0@__replicate_100-0_1758703600__\t0\t1758703600
rpool/data/vm-100-disk-0@stg-base-stage1prod1-260914T220000Z\t1073741824\t1758600000
rpool/data/vm-100-disk-1@__replicate_100-0_1758703600__\t0\t1758703600
"""

NOW = 1758704200


def luks_layout(**overrides):
    arguments = {
        "status_text": LUKS_STATUS,
        "list_text": LUKS_LIST,
        "pool_properties_text": POOL_PROPERTIES,
        "dataset_properties_text": DATASET_PROPERTIES,
        "lsblk_text": LUKS_LSBLK,
        "boot_uuids_text": "# managed by proxmox-boot-tool\nAAAA-1111\nBBBB-2222\n",
        "volume_text": VOLUMES,
        "snapshot_text": SNAPSHOTS,
        "realpath": lambda path: LUKS_REALPATHS.get(path, path),
        "hostname": "mox1",
        "collected_at": NOW,
    }
    arguments.update(overrides)
    return host_storage.build_layout(**arguments)


class ZpoolStatusTest(unittest.TestCase):
    def test_luks_mirrors_and_removal_progress(self) -> None:
        status = host_storage.parse_zpool_status(LUKS_STATUS)
        self.assertEqual(status["pool"], "rpool")
        self.assertEqual(status["state"], "ONLINE")
        self.assertEqual(
            [vdev["name"] for vdev in status["vdevs"]],
            ["mirror-0", "mirror-1", "mirror-2"],
        )
        self.assertTrue(all(vdev["type"] == "mirror" for vdev in status["vdevs"]))
        self.assertEqual(
            [member["path"] for member in status["vdevs"][1]["members"]],
            [
                "/dev/mapper/crypt-rpool-mirror2-1",
                "/dev/mapper/crypt-rpool-mirror2-2",
            ],
        )
        self.assertTrue(status["removal_in_progress"])
        self.assertEqual(
            [vdev["removing"] for vdev in status["vdevs"]], [False, True, False]
        )
        self.assertIn("53.75% done", status["remove"])
        self.assertEqual(status["errors"], "No known data errors")

    def test_resilver_nesting_single_disk_and_log_section(self) -> None:
        text = """\
  pool: rpool
 state: DEGRADED
status: One or more devices is currently being resilvered.  The pool will
\tcontinue to function, possibly in a degraded state.
action: Wait for the resilver to complete.
  scan: resilver in progress since Wed Sep 24 09:00:00 2026
config:

\tNAME                                           STATE     READ WRITE CKSUM
\trpool                                          DEGRADED     0     0     0
\t  mirror-0                                     DEGRADED     0     0     0
\t    /dev/disk/by-id/nvme-A-part3               ONLINE       0     0     0
\t    replacing-1                                DEGRADED     0     0     0
\t      /dev/disk/by-id/nvme-B-part3             FAULTED      0     0     0  too many errors
\t      /dev/disk/by-id/nvme-C-part3             ONLINE       0     0     0  (resilvering)
\t  /dev/disk/by-id/nvme-D                       ONLINE       0     0     0
\tlogs
\t  /dev/disk/by-id/nvme-E                       ONLINE       0     0     0

errors: No known data errors
"""
        status = host_storage.parse_zpool_status(text)
        self.assertEqual(status["state"], "DEGRADED")
        self.assertIn("continue to function", status["status"])
        self.assertFalse(status["removal_in_progress"])
        mirror, single = status["vdevs"]
        self.assertEqual(
            [member["path"] for member in mirror["members"]],
            [
                "/dev/disk/by-id/nvme-A-part3",
                "/dev/disk/by-id/nvme-B-part3",
                "/dev/disk/by-id/nvme-C-part3",
            ],
        )
        self.assertEqual(mirror["members"][1]["state"], "FAULTED")
        self.assertTrue(mirror["replacing"])
        self.assertFalse(single["replacing"])
        self.assertEqual(single["type"], "disk")
        self.assertEqual(single["members"][0]["path"], "/dev/disk/by-id/nvme-D")
        self.assertEqual(
            [vdev["name"] for vdev in status["auxiliary"]["logs"]],
            ["/dev/disk/by-id/nvme-E"],
        )

    def test_output_without_pool_is_rejected(self) -> None:
        with self.assertRaises(host_storage.StorageError):
            host_storage.parse_zpool_status("no pools available\n")


class LayoutTest(unittest.TestCase):
    def test_luks_members_resolve_to_serials_and_esp(self) -> None:
        layout = luks_layout()
        vdevs = {vdev["name"]: vdev for vdev in layout["vdevs"]}
        self.assertTrue(vdevs["mirror-0"]["holds_esp"])
        self.assertFalse(vdevs["mirror-1"]["holds_esp"])
        self.assertFalse(vdevs["mirror-2"]["holds_esp"])
        self.assertTrue(all(vdev["luks"] for vdev in layout["vdevs"]))
        self.assertTrue(vdevs["mirror-1"]["removing"])
        self.assertEqual(vdevs["mirror-1"]["size"], 1999000000000)
        self.assertEqual(vdevs["mirror-1"]["allocated"], 300000000000)

        boot_member = vdevs["mirror-0"]["members"][0]
        self.assertEqual(boot_member["mapper"], "/dev/mapper/crypt-rpool-a")
        self.assertEqual(boot_member["backing"], "/dev/nvme0n1p3")
        self.assertEqual(boot_member["disk"], "/dev/nvme0n1")
        self.assertEqual(boot_member["serial"], "S0BOOTA")
        self.assertEqual(boot_member["disk_size"], 2000398934016)
        self.assertEqual(
            [member["serial"] for member in vdevs["mirror-2"]["members"]],
            ["S4EXTRA3", "S5EXTRA4"],
        )
        self.assertEqual(
            layout["esp_partitions"], ["/dev/nvme0n1p2", "/dev/nvme1n1p2"]
        )
        self.assertEqual(layout["pool"]["dataset_available"], 4600000000000)
        self.assertTrue(layout["pool"]["removal_in_progress"])

    def test_luks_mappings_and_crypttab_are_reported(self) -> None:
        layout = luks_layout(
            crypttab_text=(
                "# comment\n"
                "crypt-rpool-a\tUUID=1\tapp-ha-rpool\tluks,initramfs,nofail,keyscript=decrypt_keyctl\n"
                "crypt-rpool-mirror4-1 UUID=2 none luks,initramfs,nofail\n"
                "cryptswap UUID=3 none swap\n"
            )
        )
        self.assertEqual(
            layout["crypttab"],
            [
                {"mapper": "crypt-rpool-a", "shared_unlock": True},
                {"mapper": "crypt-rpool-mirror4-1", "shared_unlock": False},
            ],
        )
        mappings = {row["mapper"]: row for row in layout["luks_mappings"]}
        self.assertEqual(
            sorted(mappings),
            [
                "crypt-rpool-a",
                "crypt-rpool-b",
                "crypt-rpool-mirror2-1",
                "crypt-rpool-mirror2-2",
                "crypt-rpool-mirror3-1",
                "crypt-rpool-mirror3-2",
            ],
        )
        self.assertTrue(all(row["in_pool"] for row in mappings.values()))
        self.assertEqual(mappings["crypt-rpool-mirror3-2"]["serial"], "S5EXTRA4")
        self.assertEqual(mappings["crypt-rpool-a"]["backing"], "/dev/nvme0n1p3")

        # A prepared pair that was never added shows as open but not in rpool.
        status = LUKS_STATUS.replace(
            "\t  mirror-2                             ONLINE       0     0     0\n"
            "\t    /dev/mapper/crypt-rpool-mirror3-1  ONLINE       0     0     0\n"
            "\t    /dev/mapper/crypt-rpool-mirror3-2  ONLINE       0     0     0\n",
            "",
        )
        mappings = {
            row["mapper"]: row for row in luks_layout(status_text=status)["luks_mappings"]
        }
        self.assertFalse(mappings["crypt-rpool-mirror3-1"]["in_pool"])
        self.assertTrue(mappings["crypt-rpool-mirror2-1"]["in_pool"])

    def test_unassigned_disks_exclude_members_esps_and_zvols(self) -> None:
        layout = luks_layout()
        unassigned = {disk["disk"]: disk for disk in layout["unassigned_disks"]}
        self.assertEqual(set(unassigned), {"/dev/nvme6n1", "/dev/nvme7n1"})
        self.assertEqual(unassigned["/dev/nvme6n1"]["partitions_or_holders"], 0)
        self.assertEqual(unassigned["/dev/nvme7n1"]["partitions_or_holders"], 1)
        self.assertFalse(unassigned["/dev/nvme7n1"]["mounted"])

    def test_clear_pool_resolves_by_id_paths(self) -> None:
        status = """\
  pool: rpool
 state: ONLINE
config:

\tNAME                                  STATE     READ WRITE CKSUM
\trpool                                 ONLINE       0     0     0
\t  mirror-0                            ONLINE       0     0     0
\t    /dev/disk/by-id/nvme-SAMSUNG_S0BOOTA-part3  ONLINE       0     0     0
\t    /dev/disk/by-id/nvme-SAMSUNG_S1BOOTB-part3  ONLINE       0     0     0

errors: No known data errors
"""
        lsblk = json.dumps(
            {
                "blockdevices": [
                    disk(
                        "/dev/nvme0n1",
                        "S0BOOTA",
                        2000398934016,
                        children=[
                            part("/dev/nvme0n1p2", fstype="vfat", uuid="AAAA-1111"),
                            part("/dev/nvme0n1p3", fstype="zfs_member"),
                        ],
                    ),
                    disk(
                        "/dev/nvme1n1",
                        "S1BOOTB",
                        2000398934016,
                        children=[
                            part("/dev/nvme1n1p2", fstype="vfat", uuid="BBBB-2222"),
                            part("/dev/nvme1n1p3", fstype="zfs_member"),
                        ],
                    ),
                ]
            }
        )
        realpaths = {
            "/dev/disk/by-id/nvme-SAMSUNG_S0BOOTA-part3": "/dev/nvme0n1p3",
            "/dev/disk/by-id/nvme-SAMSUNG_S1BOOTB-part3": "/dev/nvme1n1p3",
        }
        layout = luks_layout(
            status_text=status,
            list_text="",
            lsblk_text=lsblk,
            realpath=lambda path: realpaths.get(path, path),
            volume_text="",
            snapshot_text="",
        )
        (vdev,) = layout["vdevs"]
        self.assertFalse(vdev["luks"])
        self.assertTrue(vdev["holds_esp"])
        self.assertEqual(
            [(member["backing"], member["serial"]) for member in vdev["members"]],
            [("/dev/nvme0n1p3", "S0BOOTA"), ("/dev/nvme1n1p3", "S1BOOTB")],
        )
        self.assertIsNone(vdev["size"])
        self.assertEqual(layout["unassigned_disks"], [])

    def test_volumes_report_allocation_and_snapshots(self) -> None:
        volumes = {volume["name"]: volume for volume in luks_layout()["volumes"]}
        production = volumes["rpool/data/vm-100-disk-0"]
        self.assertEqual(production["vmid"], 100)
        self.assertEqual(production["refreservation"], 0)
        self.assertEqual(production["snapshots"], 3)
        self.assertEqual(production["replication_snapshots"], 2)
        self.assertEqual(production["latest_replication_epoch"], 1758703600)
        self.assertEqual(
            production["staging_snapshots"],
            ["stg-base-stage1prod1-260914T220000Z"],
        )
        self.assertEqual(
            volumes["rpool/data/vm-101-disk-0"]["refreservation"], 70866960384
        )


class RenderTest(unittest.TestCase):
    def test_report_shows_units_boot_vdev_and_removal(self) -> None:
        text = host_storage.render(luks_layout(), now=NOW)
        self.assertIn(
            "4,600,000,000,000 bytes = 4,386,901.86 MiB = 4,284.08 GiB", text
        )
        self.assertIn("boot/ESP (never removable)", text)
        self.assertIn("REMOVAL IN PROGRESS", text)
        self.assertIn("S6BLANK", text)
        self.assertIn("blank", text)
        self.assertIn("has partitions/signatures", text)
        self.assertIn("reserved 66.00 GiB", text)
        self.assertIn("10m ago", text)
        self.assertIn(
            "a top-level vdev removal is in progress: Evacuation of mirror-1", text
        )
        self.assertIn("          21.5G copied out of 40.0G", text)

    def test_zvols_are_named_as_local_guests_or_replicas(self) -> None:
        guests = [
            {"vmid": 100, "name": "prod1", "node": "mox1", "type": "qemu"},
            {"vmid": 101, "name": "prod2", "node": "mox2", "type": "qemu"},
        ]
        text = host_storage.render(luks_layout(), now=NOW, guests=guests)
        self.assertIn("rpool/data/vm-100-disk-0  prod1 (here)", text)
        self.assertIn("prod2 (replica; runs on mox2)", text)
        self.assertIn("rpool/data/vm-200-disk-0  unknown VMID", text)

    def test_configured_serials_are_compared_with_rpool(self) -> None:
        configured = [
            (1, 1, "S0BOOTA"),
            (1, 2, "S1BOOTB"),
            (2, 1, "S2EXTRA1"),
            (2, 2, "S2GONE"),
        ]
        items = host_storage.attention_items(luks_layout(), configured)
        self.assertIn(
            "NVME_MIRROR_2_SERIAL_2=S2GONE is configured but is not an rpool member",
            items,
        )
        for serial in ("S3EXTRA2", "S4EXTRA3", "S5EXTRA4"):
            self.assertIn(
                f"rpool member disk {serial} is not recorded in the host's .conf file",
                items,
            )
        text = host_storage.render(luks_layout(), configured, now=NOW)
        self.assertIn("NVME_MIRROR_2_SERIAL_2  S2GONE    NOT IN RPOOL", text)

    def test_registered_esp_outside_rpool_is_a_pending_replacement(self) -> None:
        devices = json.loads(LUKS_LSBLK)
        devices["blockdevices"].append(
            boot_disk(8, "S8NEWBOOT", "CCCC-3333", "crypt-rpool-unused")
        )
        layout = luks_layout(
            lsblk_text=json.dumps(devices),
            boot_uuids_text="AAAA-1111\nBBBB-2222\nCCCC-3333\n",
        )
        self.assertEqual(
            [disk["serial"] for disk in layout["esp_only_disks"]], ["S8NEWBOOT"]
        )
        self.assertNotIn(
            "S8NEWBOOT", [disk["serial"] for disk in layout["unassigned_disks"]]
        )
        self.assertTrue(any(
            "S8NEWBOOT) holds a registered ESP but is not an rpool member" in item
            for item in host_storage.attention_items(layout, ())
        ))

    def test_unresolved_member_and_missing_esp_need_attention(self) -> None:
        layout = luks_layout(boot_uuids_text="", realpath=lambda path: path)
        items = host_storage.attention_items(layout, ())
        self.assertIn(
            "no rpool vdev could be matched to a proxmox-boot-tool ESP", items
        )
        self.assertTrue(
            any("could not be resolved to a physical disk serial" in item for item in items)
        )

    def test_render_cli_exit_status(self) -> None:
        healthy = luks_layout(
            status_text=LUKS_STATUS.replace(
                "remove: Evacuation of mirror-1 in progress since Wed Sep 24 10:00:00 2026\n"
                "\t21.5G copied out of 40.0G at 120M/s, 53.75% done, 0h2m to go\n"
                "\t4.21M memory used for removed device mappings\n",
                "",
            )
        )
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "layout.json"
            path.write_text(json.dumps(healthy), encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(
                    host_storage.main(["render", str(path), "--exit-status"]), 0
                )
            path.write_text(json.dumps(luks_layout()), encoding="utf-8")
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(
                    host_storage.main(["render", str(path), "--exit-status"]), 1
                )
            path.write_text(json.dumps({"schema_version": 99}), encoding="utf-8")
            with contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(host_storage.main(["render", str(path)]), 2)


if __name__ == "__main__":
    unittest.main()
