#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Tests for authoritative host-side storage workflow records."""

import tempfile
from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "utilities"))
import storage_state


MEMBERS = [
    {
        "path": "/dev/mapper/crypt-rpool-mirror1-1",
        "mapper": "/dev/mapper/crypt-rpool-mirror1-1",
        "luks": True,
        "disk": "/dev/nvme2n1",
        "serial": "SERIAL1",
        "model": "NVMe",
        "disk_size": 1000,
    },
    {
        "path": "/dev/mapper/crypt-rpool-mirror1-2",
        "mapper": "/dev/mapper/crypt-rpool-mirror1-2",
        "luks": True,
        "disk": "/dev/nvme3n1",
        "serial": "SERIAL2",
        "model": "NVMe",
        "disk_size": 1000,
    },
]


class StorageStateTest(unittest.TestCase):
    def test_removal_record_uses_only_live_host_identity(self) -> None:
        state = storage_state.empty_state()
        removal_id = storage_state.record_removal(
            state, {"vdev": "mirror-1", "members": MEMBERS}, 100
        )
        self.assertEqual(removal_id, "removal-100-mirror-1")
        self.assertNotIn("conf_pair", state["removals"][0])
        storage_state.set_removal(state, removal_id, "retired", "done", 101)
        self.assertEqual(state["removals"][0]["state"], "retired")

    def test_resume_records_round_trip_atomically(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "state.json"
            storage_state.mutate(
                path,
                lambda state: storage_state.record_addition(
                    state, 2, ["SERIAL1", "SERIAL2"], [1000, 1000], 100
                ),
            )
            storage_state.mutate(
                path,
                lambda state: storage_state.record_replacement(
                    state, "SURVIVOR", "REPLACEMENT", "mirror-1", 101
                ),
            )
            state = storage_state.read_state(path)
            self.assertEqual(state["additions"]["2"]["serials"], ["SERIAL1", "SERIAL2"])
            self.assertEqual(
                state["replacements"]["SURVIVOR"]["serial"], "REPLACEMENT"
            )

    def test_additions_use_extra_mirror_pairs_from_1_without_upper_limit(self) -> None:
        for pair in (0, -1, True):
            with self.assertRaisesRegex(storage_state.StateError, "pair must be a positive integer"):
                storage_state.record_addition(
                    storage_state.empty_state(), pair, ["SERIAL1", "SERIAL2"], [1000, 1000], 100
                )
        state = storage_state.empty_state()
        storage_state.record_addition(state, 5, ["SERIAL1", "SERIAL2"], [1000, 1000], 100)
        self.assertIn("5", state["additions"])

    def test_removal_may_include_a_member_whose_disk_is_gone(self) -> None:
        state = storage_state.empty_state()
        gone = {**MEMBERS[1], "disk": None, "serial": None, "model": None, "disk_size": None}
        storage_state.record_removal(
            state, {"vdev": "mirror-1", "members": [MEMBERS[0], gone]}, 100
        )
        self.assertIsNone(state["removals"][0]["members"][1]["serial"])

        for members in ([gone], [MEMBERS[0], {**MEMBERS[1], "serial": None}]):
            with self.assertRaises(storage_state.StateError):
                storage_state.record_removal(
                    storage_state.empty_state(),
                    {"vdev": "mirror-1", "members": members},
                    100,
                )

    def test_rejects_malformed_removal(self) -> None:
        with self.assertRaises(storage_state.StateError):
            storage_state.record_removal(
                storage_state.empty_state(),
                {"vdev": "mirror-1", "members": [], "workstation_conf": "mox1.conf"},
                100,
            )


if __name__ == "__main__":
    unittest.main()
