#!/usr/bin/env python3

# Copyright (c) 2026 BEENTHERE VENTURES, INC.
# SPDX-License-Identifier: GPL-3.0-only

"""Run the unittest suite in parallel, one test per worker process.

    dev/run_tests.py                                  every tracked test_*.py
    dev/run_tests.py -j 8 -k reservation              8 workers, filtered
    dev/run_tests.py lib/test_haproxy_routes.py       specific targets
    dev/run_tests.py lib.test_rpool_mirror.RpoolMirrorToolTest.test_add

Targets are test files, modules, classes, or single test ids, as accepted by
``python3 -m unittest``; with none, every tracked test_*.py runs. -k follows
unittest's rules (a plain substring, or an fnmatch pattern if it contains a
wildcard) and may be repeated. Each test runs in its own ``python3`` process
with stdin closed, so no test can see another's interpreter state, and the
output of a failing test is printed with its traceback. The exit status is 0
only if every test passed or was skipped.

No test may depend on running alone or in a fixed order: each builds its
fixtures under its own temporary directory. Keep it that way.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from typing import Any, Iterator

REPO_ROOT = Path(__file__).resolve().parent.parent


def tracked_test_files() -> list[str]:
    # safe.directory: a VM mount can briefly report this checkout as foreign.
    listed = subprocess.run(
        ["git", "-c", f"safe.directory={REPO_ROOT}", "ls-files", "--",
         "test_*.py", "*/test_*.py"],
        cwd=REPO_ROOT, check=True, capture_output=True, text=True,
    ).stdout.split()
    if not listed:
        sys.exit("ERROR: no tracked test_*.py files")
    return listed


def target_name(target: str) -> str:
    if target.endswith(".py"):
        path = Path(target).resolve()
        try:
            relative = path.relative_to(REPO_ROOT)
        except ValueError:
            sys.exit(f"ERROR: {target} is outside {REPO_ROOT}")
        return ".".join(relative.with_suffix("").parts)
    return target


def flatten(suite: unittest.TestSuite) -> Iterator[unittest.TestCase]:
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from flatten(item)
        else:
            yield item


def is_load_failure(test: unittest.TestCase) -> bool:
    return type(test).__name__ == "_FailedTest"


class RecordingResult(unittest.TestResult):
    """Collects outcomes as plain data for the parent process."""

    def __init__(self) -> None:
        super().__init__()
        self.record: dict[str, Any] = {
            "ran": 0, "failures": [], "errors": [], "skipped": [],
            "expected_failures": 0, "unexpected_successes": 0,
        }

    def _add(self, key: str, test: Any, err: Any) -> None:
        self.record[key].append([str(test), self._exc_info_to_string(err, test)])

    def startTest(self, test: unittest.TestCase) -> None:
        super().startTest(test)
        self.record["ran"] += 1

    def addFailure(self, test: Any, err: Any) -> None:
        super().addFailure(test, err)
        self._add("failures", test, err)

    def addError(self, test: Any, err: Any) -> None:
        super().addError(test, err)
        self._add("errors", test, err)

    def addSubTest(self, test: Any, subtest: Any, err: Any) -> None:
        super().addSubTest(test, subtest, err)
        if err is not None:
            failed = issubclass(err[0], test.failureException)
            self._add("failures" if failed else "errors", subtest, err)

    def addSkip(self, test: Any, reason: str) -> None:
        super().addSkip(test, reason)
        self.record["skipped"].append([str(test), reason])

    def addExpectedFailure(self, test: Any, err: Any) -> None:
        super().addExpectedFailure(test, err)
        self.record["expected_failures"] += 1

    def addUnexpectedSuccess(self, test: Any) -> None:
        super().addUnexpectedSuccess(test)
        self.record["unexpected_successes"] += 1


def run_worker(test_id: str, result_path: str) -> None:
    # Match `python3 -m unittest` run from the repository root.
    os.chdir(REPO_ROOT)
    sys.path[0] = str(REPO_ROOT)
    result = RecordingResult()
    unittest.defaultTestLoader.loadTestsFromName(test_id).run(result)
    Path(result_path).write_text(json.dumps(result.record), encoding="utf-8")


def run_one(test_id: str) -> tuple[str, float, dict[str, Any], str]:
    with tempfile.TemporaryDirectory(prefix="bmac-test-") as scratch:
        result_path = os.path.join(scratch, "result.json")
        started = time.monotonic()
        completed = subprocess.run(
            [sys.executable, str(Path(__file__).resolve()), "--worker",
             result_path, test_id],
            cwd=REPO_ROOT, stdin=subprocess.DEVNULL, capture_output=True,
            text=True, env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"},
        )
        elapsed = time.monotonic() - started
        output = completed.stdout + completed.stderr
        try:
            record = json.loads(Path(result_path).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            record = {
                "ran": 1, "failures": [], "skipped": [],
                "errors": [[test_id, f"worker exited {completed.returncode} "
                                     "without reporting a result\n"]],
                "expected_failures": 0, "unexpected_successes": 0,
            }
    return test_id, elapsed, record, output


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Run the unittest suite in parallel.")
    parser.add_argument("targets", nargs="*",
                        help="test files, modules, classes, or test ids")
    parser.add_argument("-j", "--jobs", type=int, default=os.cpu_count() or 1,
                        help="worker processes (default: CPU count)")
    parser.add_argument("-k", dest="patterns", action="append", default=[],
                        help="only run tests matching this pattern")
    parser.add_argument("-v", "--verbose", action="store_true",
                        help="print every test with its duration")
    parser.add_argument("-f", "--failfast", action="store_true",
                        help="start no new tests after the first failure")
    parser.add_argument("--worker", nargs=2, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)

    if args.worker:
        run_worker(args.worker[1], args.worker[0])
        return 0
    if args.jobs < 1:
        parser.error("-j must be at least 1")

    os.chdir(REPO_ROOT)
    sys.path.insert(0, str(REPO_ROOT))
    os.environ["PYTHONDONTWRITEBYTECODE"] = "1"
    sys.dont_write_bytecode = True
    loader = unittest.TestLoader()
    loader.testNamePatterns = [
        pattern if "*" in pattern else f"*{pattern}*" for pattern in args.patterns
    ] or None
    names = [target_name(t) for t in args.targets] or [
        target_name(path) for path in tracked_test_files()]
    tests = list(flatten(loader.loadTestsFromNames(names)))

    started = time.monotonic()
    totals: dict[str, Any] = {
        "ran": 0, "failures": [], "errors": [], "skipped": [],
        "expected_failures": 0, "unexpected_successes": 0,
    }
    outputs: dict[str, str] = {}
    durations: list[tuple[float, str]] = []

    def collect(test_id: str, elapsed: float, record: dict[str, Any],
                output: str) -> bool:
        totals["ran"] += record["ran"]
        for key in ("failures", "errors", "skipped"):
            totals[key].extend(record[key])
        for key in ("expected_failures", "unexpected_successes"):
            totals[key] += record[key]
        durations.append((elapsed, test_id))
        bad = bool(record["failures"] or record["errors"])
        if bad and output.strip():
            outputs[test_id] = output
        if args.verbose:
            status = "FAIL" if bad else "skipped" if record["skipped"] else "ok"
            print(f"{test_id} ... {status} ({elapsed:.1f}s)", flush=True)
        else:
            print("F" if bad else "s" if record["skipped"] else ".",
                  end="", flush=True)
        return bad

    # Import errors surface as placeholder tests that cannot be loaded by id
    # in a worker; running them here reports the original exception.
    runnable = []
    for test in tests:
        if is_load_failure(test):
            result = RecordingResult()
            test.run(result)
            collect(test.id(), 0.0, result.record, "")
        else:
            runnable.append(test.id())

    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        pending = [pool.submit(run_one, test_id) for test_id in runnable]
        try:
            for future in concurrent.futures.as_completed(pending):
                if future.cancelled():
                    continue
                if collect(*future.result()) and args.failfast:
                    for other in pending:
                        other.cancel()
        except KeyboardInterrupt:
            for other in pending:
                other.cancel()
            raise
    elapsed = time.monotonic() - started
    if not args.verbose:
        print()

    rule = "=" * 70
    for kind, key in (("ERROR", "errors"), ("FAIL", "failures")):
        for description, trace in totals[key]:
            print(f"{rule}\n{kind}: {description}\n{'-' * 70}\n{trace}")
    for test_id, output in sorted(outputs.items()):
        print(f"{rule}\nOUTPUT OF FAILED {test_id}\n{'-' * 70}\n{output}")
    if args.verbose:
        print(f"{rule}\nSlowest tests:")
        for seconds, test_id in sorted(durations, reverse=True)[:10]:
            print(f"{seconds:7.1f}s  {test_id}")

    print("-" * 70)
    print(f"Ran {totals['ran']} tests in {elapsed:.1f}s with {args.jobs} workers")
    details = [f"{label}={len(totals[key])}" for label, key in (
        ("failures", "failures"), ("errors", "errors"), ("skipped", "skipped"),
    ) if totals[key]]
    details += [f"{key}={totals[key]}" for key in (
        "expected_failures", "unexpected_successes") if totals[key]]
    ok = not (totals["failures"] or totals["errors"]
              or totals["unexpected_successes"])
    suffix = f" ({', '.join(details)})" if details else ""
    print(("OK" if ok else "FAILED") + suffix)
    if totals["ran"] < len(tests) and not args.failfast:
        print(f"WARNING: loaded {len(tests)} tests but only {totals['ran']} ran")
        return 1
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
