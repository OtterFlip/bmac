# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

import json
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parent.parent / "user_callable" / "guests" / "prod" / "extend_prod_vm_disk.sh"
MIB = 2**20
GIB = 2**30


class ExtendProductionDiskTest(unittest.TestCase):
    def run_sourced(
        self, body: str, *arguments: str, expected: int = 0
    ) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [
                "bash",
                "-c",
                'EXTEND_PROD_DISK_SOURCE_ONLY=1 source "$1"; shift; ' + body,
                "bash",
                str(SCRIPT),
                *arguments,
            ],
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(
            completed.returncode,
            expected,
            msg=f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}",
        )
        return completed

    def allowance(self, *arguments: str) -> dict[str, object]:
        completed = self.run_sourced('calculate_allowance "$@"', *arguments)
        result: dict[str, object] = {"nodes": []}
        for line in completed.stdout.splitlines():
            fields = line.split("\t")
            if fields[0] == "node":
                result["nodes"].append(fields[1:])  # type: ignore[union-attr]
            else:
                result[fields[0]] = int(fields[1])
        return result

    def test_shell_syntax_and_help(self) -> None:
        syntax = subprocess.run(
            ["bash", "-n", str(SCRIPT)], text=True, capture_output=True, check=False
        )
        self.assertEqual(syntax.returncode, 0, syntax.stderr)
        completed = subprocess.run(
            [str(SCRIPT), "--help"], text=True, capture_output=True, check=False
        )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertIn("--dry-run", completed.stdout)
        self.assertIn("10% of its total size", completed.stdout)

    def test_sparse_limit_uses_smallest_headroom_guest_free_and_overhead(self) -> None:
        result = self.allowance(
            "sparse", str(64 * GIB), "0", str(20 * GIB), "10", "15",
            f"mox1={1000 * GIB}={300 * GIB}",
            f"mox2={1000 * GIB}={250 * GIB}",
        )
        headroom = 250 * GIB - 100 * GIB
        self.assertEqual(result["headroom"], headroom)
        expected = (headroom - 20 * GIB) * 985 // 1000 // MIB * MIB
        self.assertEqual(result["allowed"], expected)
        self.assertEqual(result["allowed"] % MIB, 0)
        self.assertEqual(
            result["nodes"],
            [
                ["mox1", str(1000 * GIB), str(300 * GIB), str(100 * GIB), str(200 * GIB)],
                ["mox2", str(1000 * GIB), str(250 * GIB), str(100 * GIB), str(150 * GIB)],
            ],
        )

    def test_sparse_limit_is_zero_when_guest_free_covers_headroom(self) -> None:
        result = self.allowance(
            "sparse", str(64 * GIB), "0", str(150 * GIB), "10", "15",
            f"mox1={1000 * GIB}={250 * GIB}",
            f"mox2={1000 * GIB}={300 * GIB}",
        )
        self.assertEqual(result["allowed"], 0)

    def test_reserved_limit_scales_by_refreservation_ratio(self) -> None:
        volsize, refreservation = 64 * GIB, 66 * GIB + 12345
        result = self.allowance(
            "reserved", str(volsize), str(refreservation), str(500 * GIB), "10", "15",
            f"mox1={1000 * GIB}={300 * GIB}",
            f"mox3={2000 * GIB}={260 * GIB}",
        )
        # mox3 keeps 200 GiB of its 2000 GiB pool, leaving the smaller 60 GiB.
        headroom = 60 * GIB
        self.assertEqual(result["headroom"], headroom)
        self.assertEqual(
            result["allowed"], headroom * volsize // refreservation // MIB * MIB
        )

    def test_reserve_is_ceiling_and_headroom_floors_to_mib(self) -> None:
        # 10% of 1000000007 bytes rounds up to 100000001.
        result = self.allowance(
            "reserved", str(GIB), str(GIB), "0", "10", "15",
            f"mox1=1000000007={100000001 + 5 * MIB + 99}",
        )
        self.assertEqual(result["headroom"], 5 * MIB)
        self.assertEqual(result["allowed"], 5 * MIB)

    def test_negative_headroom_allows_nothing(self) -> None:
        result = self.allowance(
            "sparse", str(GIB), "0", "0", "10", "15",
            f"mox1={1000 * GIB}={50 * GIB}",
            f"mox2={1000 * GIB}={500 * GIB}",
        )
        self.assertEqual(result["headroom"], 0)
        self.assertEqual(result["allowed"], 0)

    def test_invalid_allowance_inputs_fail_closed(self) -> None:
        self.run_sourced(
            'calculate_allowance "$@"',
            "reserved", str(GIB), str(GIB - 1), "0", "10", "15",
            f"mox1={GIB}={GIB}",
            expected=1,
        )
        self.run_sourced(
            'calculate_allowance "$@"',
            "sparse", str(GIB), "0", "0", "10", "15", "host1=1=1",
            expected=1,
        )

    def test_increase_parsing_rounds_up_to_mib_and_enforces_limit(self) -> None:
        def parse(unit: str, amount: str, allowed: int, expected: int = 0):
            return self.run_sourced(
                'parse_increase "$@"', unit, amount, str(allowed), expected=expected
            )

        self.assertEqual(parse("GiB", "2.5", 10 * GIB).stdout.split(), [str(5 * GIB // 2)] * 2)
        self.assertEqual(parse("MiB", "3", 10 * GIB).stdout.split(), [str(3 * MIB)] * 2)
        self.assertEqual(
            parse("bytes", "1", 10 * GIB).stdout.split(), ["1", str(MIB)]
        )
        self.assertEqual(
            parse("MiB", "1.0000001", 10 * GIB).stdout.split()[1], str(2 * MIB)
        )
        parse("GiB", "1", GIB)
        over = parse("bytes", str(GIB + 1), GIB, expected=1)
        self.assertIn("exceeds", over.stderr)
        for unit, amount in (("bytes", "1.5"), ("GiB", "-1"), ("GiB", "abc"), ("MiB", "0")):
            parse(unit, amount, 10 * GIB, expected=1)

    def test_format_size_truncates_instead_of_overstating(self) -> None:
        completed = self.run_sourced('format_size "$1"', str(GIB - 1))
        self.assertEqual(
            completed.stdout.strip(), f"{GIB - 1} bytes (1023.99 MiB, 0.999 GiB)"
        )
        exact = self.run_sourced('format_size "$1"', str(3 * GIB))
        self.assertEqual(exact.stdout.strip(), f"{3 * GIB} bytes (3072 MiB, 3.000 GiB)")

    def guest_report(self, **overrides: object) -> str:
        value: dict[str, object] = {
            "hostname": "prod1",
            "uid": 0,
            "fstype": "ext4",
            "options": ["rw", "relatime"],
            "source": "/dev/sda2",
            "partition": "sda2",
            "partition_number": 2,
            "disk": "sda",
            "disk_bytes": 64 * GIB,
            "table_label": "gpt",
            "partition_bytes": 63 * GIB,
            "partition_end_bytes": 64 * GIB - MIB,
            "root_is_last": True,
            "unallocated_tail_bytes": 0,
            "fs_bytes": 63 * GIB,
            "free_bytes": 40 * GIB,
            "rescan_available": True,
            "tools": {
                "growpart": True,
                "resize2fs": True,
                "sfdisk": True,
                "tune2fs": True,
                "findmnt": True,
            },
        }
        value.update(overrides)
        return json.dumps(value)

    def test_guest_report_validation(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            body = (
                f'RUN_DIR={temporary!r}; RESOURCE_NAME=prod1; '
                'load_guest_report "$1" "$2" && '
                'printf "%s %s %s\\n" "$GUEST_DISK" "$GUEST_PARTITION_NUMBER" "$GUEST_FREE_BYTES"'
            )
            valid = self.run_sourced(body, self.guest_report(), str(64 * GIB))
            self.assertEqual(valid.stdout.strip(), f"sda 2 {40 * GIB}")
            for override, message in (
                ({"root_is_last": False}, "not the last partition"),
                ({"fstype": "xfs"}, "not ext4"),
                ({"hostname": "prod2"}, "hostname"),
                ({"uid": 1000}, "not root"),
                ({"disk_bytes": 65 * GIB}, "root zvol"),
                ({"tools": {"growpart": False}}, "cloud-guest-utils"),
            ):
                invalid = self.run_sourced(
                    body, self.guest_report(**override), str(64 * GIB), expected=1
                )
                self.assertIn(message, invalid.stderr)

    def test_mutation_ordering_and_guards(self) -> None:
        text = SCRIPT.read_text(encoding="utf-8")
        for value in (
            "--disk-bytes",
            "update_cluster_runtime.sh",
            "--owner-node",
            "orchestration-acquire",
            "qm resize",
            "refreservation=auto",
            "pvesr schedule-now",
            "growpart",
            "resize2fs",
            "Type GO",
        ):
            self.assertIn(value, text)
        order = [
            "  reconcile_registry_owner\n",
            "  verify_guest_ssh\n",
            "  prompt_increase\n",
            "  confirm_go ",
            "  acquire_orchestration_lease\n",
            "  revalidate_before_resize\n",
            "  resize_zvol\n",
            "  record_registry_size\n",
            "  replicate_new_size\n",
            "  rescan_guest_disk\n",
            "  grow_guest\n\n  log \"Production disk growth complete\"",
        ]
        main = text[text.index("\nmain() {"):]
        positions = [main.index(value) for value in order]
        self.assertEqual(positions, sorted(positions))
        self.assertLess(main.index("DRY_RUN"), main.index("prompt_increase"))


if __name__ == "__main__":
    unittest.main()
