# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Runs a scripted lab scenario and reports each check as PASS or FAIL.

A scenario is a Python file in lab/scenarios/ (or any path) with a
`DESCRIPTION` string and a `run(s)` function. `s.check()` records a result
and carries on; `s.require()` records a result and stops the scenario when
it fails, because the checks after it depend on it.
"""

from __future__ import annotations

import importlib.util
import sys
import time
import traceback
from collections.abc import Callable
from pathlib import Path
from typing import TextIO

SCENARIOS = Path(__file__).resolve().parent.parent / "scenarios"


class Stop(Exception):
    """A required check failed; the remaining checks cannot run."""


def duration(seconds: float) -> str:
    if seconds < 60:
        return f"{seconds:.1f}s"
    minutes, rest = divmod(int(seconds), 60)
    return f"{minutes}m{rest:02d}s"


class Scenario:
    def __init__(self, lab, out: TextIO = sys.stdout, clock=time.monotonic) -> None:
        self.lab = lab
        self.out = out
        self.clock = clock
        self.passed = 0
        self.failed = 0

    def _report(self, ok: bool, description: str, started: float, detail: str = "") -> None:
        if ok:
            self.passed += 1
        else:
            self.failed += 1
        print(f"  {'PASS' if ok else 'FAIL'}  {description} ({duration(self.clock() - started)})", file=self.out, flush=True)
        for line in detail.rstrip().splitlines():
            print(f"        {line}", file=self.out)

    def check(self, description: str, test: Callable[[], object] | bool, detail: str = "") -> bool:
        """`test` passes when it returns a truthy value, and fails when it
        returns a falsy one or raises."""
        started = self.clock()
        try:
            result = test() if callable(test) else test
        except Exception as error:  # noqa: BLE001 - every failure becomes a FAIL line
            self._report(False, description, started, f"{type(error).__name__}: {error}")
            return False
        ok = bool(result)
        self._report(ok, description, started, "" if ok else detail)
        return ok

    def require(self, description: str, test: Callable[[], object] | bool, detail: str = "") -> None:
        if not self.check(description, test, detail):
            raise Stop(description)

    def note(self, text: str) -> None:
        print(f"        {text}", file=self.out, flush=True)


def resolve(name: str) -> Path:
    path = Path(name)
    if path.suffix == ".py" and path.exists():
        return path
    candidate = SCENARIOS / f"{name}.py"
    if candidate.exists():
        return candidate
    available = sorted(p.stem for p in SCENARIOS.glob("*.py"))
    raise FileNotFoundError(f"no scenario {name!r}; available: {', '.join(available)}")


def load(path: Path):
    spec = importlib.util.spec_from_file_location(f"bmac_lab_scenario_{path.stem}", path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    if not callable(getattr(module, "run", None)):
        raise TypeError(f"{path} has no run(s) function")
    return module


def run(lab, name: str, out: TextIO = sys.stdout, clock=time.monotonic) -> int:
    path = resolve(name)
    module = load(path)
    scenario = Scenario(lab, out, clock)
    started = clock()
    print(f"{path.stem}: {getattr(module, 'DESCRIPTION', '')}".rstrip(": "), file=out, flush=True)
    stopped = ""
    try:
        module.run(scenario)
    except Stop as stop:
        stopped = f"; stopped after {stop}"
    except Exception:  # noqa: BLE001 - a crash is reported as a failure
        scenario.failed += 1
        print("  FAIL  the scenario crashed", file=out)
        for line in traceback.format_exc().rstrip().splitlines():
            print(f"        {line}", file=out)
    print(f"{path.stem}: {scenario.passed} passed, {scenario.failed} failed{stopped} ({duration(clock() - started)})", file=out, flush=True)
    return 0 if scenario.failed == 0 and scenario.passed > 0 else 1
