#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Unit tests for the monthly public-interface transfer counter."""

from __future__ import annotations

import datetime as dt
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest


COUNTER_PATH = Path(__file__).resolve().parent.parent / "host_runtime" / "transfer_counter.py"
SPEC = importlib.util.spec_from_file_location("transfer_counter", COUNTER_PATH)
assert SPEC is not None and SPEC.loader is not None
transfer_counter = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = transfer_counter
SPEC.loader.exec_module(transfer_counter)

GIB = 1024**3


def reading(state, day, rx, tx, boot="boot-a", reset_day=24):
    return transfer_counter.tick(
        state,
        today=day,
        reset_day=reset_day,
        rx_bytes=rx,
        tx_bytes=tx,
        boot_id=boot,
        now=f"{day.isoformat()}T00:00:00+00:00",
    )


class PeriodTest(unittest.TestCase):
    def test_reset_day_above_28_is_treated_as_28(self) -> None:
        self.assertEqual(transfer_counter.effective_reset_day("31"), 28)
        self.assertEqual(transfer_counter.effective_reset_day(None), 1)
        self.assertEqual(transfer_counter.effective_reset_day("24"), 24)
        with self.assertRaises(ValueError):
            transfer_counter.effective_reset_day("0")

    def test_period_start_and_next_reset(self) -> None:
        self.assertEqual(
            transfer_counter.period_start(dt.date(2026, 10, 23), 24), dt.date(2026, 9, 24)
        )
        self.assertEqual(
            transfer_counter.period_start(dt.date(2026, 10, 24), 24), dt.date(2026, 10, 24)
        )
        self.assertEqual(
            transfer_counter.period_start(dt.date(2026, 1, 5), 24), dt.date(2025, 12, 24)
        )
        self.assertEqual(
            transfer_counter.next_reset(dt.date(2026, 12, 30), 28), dt.date(2027, 1, 28)
        )
        self.assertEqual(
            transfer_counter.next_reset(dt.date(2026, 2, 1), 1), dt.date(2026, 3, 1)
        )


class TickTest(unittest.TestCase):
    def test_counts_inbound_and_outbound_together(self) -> None:
        state = reading(None, dt.date(2026, 10, 1), 1000, 2000)
        self.assertEqual(state["total_bytes"], 0)
        state = reading(state, dt.date(2026, 10, 1), 1500, 2600)
        self.assertEqual(state["total_bytes"], 500 + 600)

    def test_resets_exactly_once_on_the_reset_day(self) -> None:
        state = reading(None, dt.date(2026, 10, 23), 0, 0)
        state = reading(state, dt.date(2026, 10, 23), 5 * GIB, 0)
        self.assertEqual(state["total_bytes"], 5 * GIB)
        state = reading(state, dt.date(2026, 10, 24), 5 * GIB + 10, 0)
        self.assertEqual(state["total_bytes"], 10)
        self.assertEqual(state["last_reset_date"], "2026-10-24")
        # Later readings on the same day (or after a restart) add, not reset.
        state = reading(state, dt.date(2026, 10, 24), 5 * GIB + 30, 0)
        self.assertEqual(state["total_bytes"], 30)
        state = reading(state, dt.date(2026, 10, 25), 5 * GIB + 40, 0)
        self.assertEqual(state["total_bytes"], 40)

    def test_a_reboot_or_counter_reset_counts_the_new_reading_from_zero(self) -> None:
        state = reading(None, dt.date(2026, 10, 2), 100, 100)
        state = reading(state, dt.date(2026, 10, 2), 300, 300)
        self.assertEqual(state["total_bytes"], 400)
        state = reading(state, dt.date(2026, 10, 2), 50, 25, boot="boot-b")
        self.assertEqual(state["total_bytes"], 475)
        state = reading(state, dt.date(2026, 10, 2), 10, 25, boot="boot-b")
        self.assertEqual(state["total_bytes"], 510)

    def test_cli_persists_state_across_runs(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            settings = root / "guest-network.env"
            settings.write_text(
                "BMAC_PUBLIC_NIC=eno1\nBMAC_TRANSFER_LIMIT_GIB=100\nBMAC_TRANSFER_RESET_DAY=31\n"
            )
            statistics = root / "net" / "eno1" / "statistics"
            statistics.mkdir(parents=True)
            boot = root / "boot_id"
            boot.write_text("boot-a\n")
            state = root / "transfer.json"

            def run(rx, tx, command="tick"):
                (statistics / "rx_bytes").write_text(f"{rx}\n")
                (statistics / "tx_bytes").write_text(f"{tx}\n")
                return transfer_counter.main(
                    [
                        command, "--settings", str(settings), "--state", str(state),
                        "--sysfs-root", str(root / "net"), "--boot-id-file", str(boot),
                    ],
                    clock=lambda: dt.datetime(2026, 10, 20, 12, tzinfo=dt.timezone.utc),
                )

            self.assertEqual(run(10, 10), 0)
            self.assertEqual(run(10 + GIB, 10 + GIB), 0)
            saved = json.loads(state.read_text())
            self.assertEqual(saved["total_bytes"], 2 * GIB)
            self.assertEqual(saved["period_start"], "2026-09-28")
            report = transfer_counter.status(
                saved, transfer_counter.load_settings(settings), dt.date(2026, 10, 20)
            )
            self.assertEqual(report["limit_gib"], 100)
            self.assertEqual(report["used_percent"], 2.0)
            self.assertEqual(report["reset_day"], 28)
            self.assertEqual(report["next_reset"], "2026-10-28")


if __name__ == "__main__":
    unittest.main()
