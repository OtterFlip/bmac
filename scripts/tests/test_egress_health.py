#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Unit tests for the egress health rotation and fallback state machine."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import unittest
from urllib.parse import urlsplit


HEALTH_PATH = Path(__file__).resolve().parent.parent / "host_runtime" / "egress_health.py"
SPEC = importlib.util.spec_from_file_location("egress_health", HEALTH_PATH)
assert SPEC is not None and SPEC.loader is not None
egress_health = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = egress_health
SPEC.loader.exec_module(egress_health)


def targets(count: int) -> list[dict]:
    return [{"url": f"https://t{index}.example/", "addresses": ["192.0.2.1"]} for index in range(count)]


class FakeNetwork:
    """Decides each probe's result by target and by path (own or a peer's mark)."""

    def __init__(self) -> None:
        self.own_up = True
        self.dead_targets: set[str] = set()
        self.peer_up: dict[int, bool] = {}
        self.calls: list[tuple[str, int | None]] = []

    def probe(self, target: dict, mark: int | None) -> bool:
        self.calls.append((target["url"], mark))
        if target["url"] in self.dead_targets:
            return False
        if mark is None:
            return self.own_up
        return self.peer_up.get(mark - egress_health.PEER_MARK_BASE, False)


class RotationTest(unittest.TestCase):
    def test_each_check_starts_at_the_next_target(self) -> None:
        network = FakeNetwork()
        rotation = egress_health.Rotation(targets(5), network.probe)
        for _ in range(7):
            self.assertTrue(rotation.check())
        hosts = [urlsplit(url).hostname.split(".")[0] for url, _ in network.calls]
        self.assertEqual(hosts, ["t0", "t1", "t2", "t3", "t4", "t0", "t1"])

    def test_a_failing_target_advances_and_only_all_failing_is_down(self) -> None:
        network = FakeNetwork()
        network.dead_targets = {"https://t0.example/", "https://t1.example/"}
        rotation = egress_health.Rotation(targets(6), network.probe)
        self.assertTrue(rotation.check())
        network.own_up = False
        self.assertFalse(rotation.check())
        tried = {url for url, _ in network.calls[-6:]}
        self.assertEqual(len(tried), 6)

    def test_a_persistently_failing_target_is_retired(self) -> None:
        network = FakeNetwork()
        network.dead_targets = {"https://t2.example/"}
        rotation = egress_health.Rotation(targets(8), network.probe)
        for _ in range(8 * egress_health.STRIKES_BEFORE_RETIREMENT):
            rotation.check()
        retired = [target["url"] for target in rotation.targets if target.get("retired")]
        self.assertEqual(retired, ["https://t2.example/"])
        self.assertEqual(len(rotation.active()), 7)

    def test_a_dead_connection_never_retires_targets(self) -> None:
        network = FakeNetwork()
        network.own_up = False
        rotation = egress_health.Rotation(targets(8), network.probe)
        for _ in range(20):
            self.assertFalse(rotation.check())
        self.assertEqual(len(rotation.active()), 8)

    def test_retirement_keeps_a_minimum_rotation(self) -> None:
        network = FakeNetwork()
        network.dead_targets = {f"https://t{index}.example/" for index in range(5)}
        rotation = egress_health.Rotation(targets(7), network.probe)
        for _ in range(200):
            rotation.check()
        self.assertEqual(len(rotation.active()), egress_health.MIN_ACTIVE_TARGETS)

    def test_peer_probes_never_count_strikes(self) -> None:
        network = FakeNetwork()
        network.dead_targets = {"https://t0.example/"}
        network.peer_up = {2: True}
        rotation = egress_health.Rotation(targets(6), network.probe)
        for _ in range(60):
            rotation.check(egress_health.PEER_MARK_BASE + 2, account=False)
        self.assertEqual(len(rotation.active()), 6)


class HealthTest(unittest.TestCase):
    def setUp(self) -> None:
        self.network = FakeNetwork()
        self.routes: list[int | None] = []
        self.health = egress_health.Health(
            egress_health.Rotation(targets(6), self.network.probe),
            own_index=2,
            max_hosts=4,
            set_route=self.routes.append,
        )

    def tick(self, count: int = 1) -> None:
        for _ in range(count):
            self.health.tick()

    def test_fallback_order_starts_after_this_host_and_wraps(self) -> None:
        self.assertEqual(egress_health.fallback_order(2, 4), [3, 4, 1])
        self.assertEqual(egress_health.fallback_order(10, 10), [1, 2, 3, 4, 5, 6, 7, 8, 9])
        self.assertEqual(egress_health.fallback_order(1, 1), [])

    def test_falls_back_after_three_failed_checks_and_returns_after_five_good(self) -> None:
        self.network.peer_up = {1: True, 3: False, 4: True}
        self.network.own_up = False
        self.tick(2)
        self.assertEqual(self.health.mode, "own")
        self.assertEqual(self.routes, [])
        self.tick()
        self.assertEqual((self.health.mode, self.health.fallback), ("fallback", 4))
        self.assertEqual(self.routes, [4])

        self.network.own_up = True
        self.tick(4)
        self.assertEqual(self.health.mode, "fallback")
        self.tick()
        self.assertEqual((self.health.mode, self.health.fallback), ("own", None))
        self.assertEqual(self.routes, [4, None])

    def test_a_failing_fallback_host_is_replaced(self) -> None:
        self.network.peer_up = {3: True, 4: True}
        self.network.own_up = False
        self.tick(3)
        self.assertEqual(self.health.fallback, 3)
        self.network.peer_up[3] = False
        self.tick()
        self.assertEqual(self.health.fallback, 4)
        self.assertEqual(self.routes, [3, 4])

    def test_no_working_host_leaves_egress_on_this_host_and_keeps_trying(self) -> None:
        self.network.own_up = False
        self.tick(3)
        self.assertEqual(self.health.mode, "none")
        self.assertEqual(self.routes, [None])
        self.network.peer_up = {1: True}
        self.tick()
        self.assertEqual((self.health.mode, self.health.fallback), ("fallback", 1))


if __name__ == "__main__":
    unittest.main()
