# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

import json
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parent.parent / "scripts" / "user_callable" / "guests" / "prod" / "extend_prod_vm_disk.sh"
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
        self.assertIn("10% reserve", completed.stdout)

    def test_growth_limit_comes_from_bmac_growth_over_every_placement_host(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bin_dir = root / "bin"
            bin_dir.mkdir()
            for node, used in (("mox1", 100), ("mox2", 500)):
                (root / f"capacity-{node}.json").write_text(
                    json.dumps({"host": node, "used": used}) + "\n", encoding="utf-8"
                )
            fake = bin_dir / "bmac"
            fake.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "stdin = sys.stdin.read()\n"
                "open(os.environ['GROWTH_CALL'], 'w').write(json.dumps([sys.argv[1:], stdin]))\n"
                "print(json.dumps({'hosts': [\n"
                "  {'host': 'mox1', 'total': 1000, 'available': 900, 'reserve': 100, 'headroom': 800},\n"
                "  {'host': 'mox2', 'total': 1000, 'available': 500, 'reserve': 100, 'headroom': 400}],\n"
                "  'allowed': 3 * 1048576}))\n",
                encoding="utf-8",
            )
            fake.chmod(0o755)
            call = root / "call.json"
            body = (
                f"PATH={str(bin_dir)!r}:$PATH; export GROWTH_CALL={str(call)!r}; RUN_DIR={temporary!r}; "
                "PLACEMENT_NODES=(mox1 mox2); OWNER_NODE=mox1; DISK_ALLOCATION=\"$1\"; "
                "GUEST_FREE_BYTES=7; NODE_VOLSIZE=([mox1]=1073741824); "
                "NODE_REFRESERVATION=([mox1]=1100000000); NODE_BLOCK_SIZE=([mox1]=16384); "
                'compute_allowance && printf "%s %s\\n" "$ALLOWED_BYTES" "$MIN_HEADROOM_BYTES"'
            )
            sparse = self.run_sourced(body, "sparse")
            self.assertEqual(sparse.stdout.split(), [str(3 * MIB), "400"])
            arguments, stdin = json.loads(call.read_text(encoding="utf-8"))
            self.assertEqual(
                arguments,
                ["growth", "--json", "--volsize", str(GIB), "--block-size", "16384",
                 "--guest-free", "7"],
            )
            self.assertEqual(
                [json.loads(line)["host"] for line in stdin.splitlines()], ["mox1", "mox2"]
            )
            self.run_sourced(body, "reserved")
            arguments, _ = json.loads(call.read_text(encoding="utf-8"))
            self.assertEqual(arguments[-2:], ["--refreservation", "1100000000"])
            table = (root / "allowance.tsv").read_text(encoding="utf-8").splitlines()
            self.assertEqual(table[1].split("\t"), ["node", "mox2", "1000", "500", "100", "400"])

    def test_guest_commands_go_through_the_guest_agent_on_the_owner(self) -> None:
        completed = self.run_sourced(
            'guest_agent_run() { printf "%s\\n" "$*"; cat >/dev/null; }; '
            "OWNER_NODE=mox2; VMID=101; guest_helper grow-partition sda 2"
        )
        self.assertEqual(
            completed.stdout.strip(), "--stdin mox2 101 300 python3 - grow-partition sda 2"
        )
        text = SCRIPT.read_text(encoding="utf-8")
        self.assertNotIn("GUEST_ALIAS", text)
        self.assertNotIn("ssh -o", text)

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
                ({"uid": 1000}, "not run as root"),
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
            "  verify_guest_agent\n",
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
