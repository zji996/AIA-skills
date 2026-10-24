#!/usr/bin/env python3
"""Run the unittest suite across worker processes.

Tests are black-box and isolated by their own temporary directories, so whole test methods can
run side by side. Workers are forked after discovery, so module-level setup (such as building the
delegate binary) happens once. Slowest tests are listed so the suite can be kept lean.

    run-tests.py [-j N] [-k PATTERN] [--slowest N]
"""
import argparse
import multiprocessing
import os
import sys
import time
import unittest
from pathlib import Path

ROOT = Path(os.environ.get("AIA_SKILLS_ROOT") or Path(__file__).resolve().parent.parent)


def flatten(suite):
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from flatten(item)
        else:
            yield item


# Filled before the workers fork; each worker takes every N-th test, so nothing is pickled.
TESTS = []
WORKERS = 1


def work(index):
    failures, timings = [], []
    for test in TESTS[index::WORKERS]:
        result = unittest.TestResult()
        started = time.monotonic()
        test.run(result)
        timings.append((time.monotonic() - started, test.id()))
        for case, trace in result.errors + result.failures:
            failures.append((case.id(), trace))
        for case in result.unexpectedSuccesses:
            failures.append((case.id(), "unexpected success"))
    return failures, timings


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("-j", type=int, default=min(8, max(1, (os.cpu_count() or 2) // 2)))
    parser.add_argument("-k", default=None, help="only test ids containing this text")
    parser.add_argument("--slowest", type=int, default=8)
    opts = parser.parse_args()
    os.chdir(ROOT)
    suite = unittest.defaultTestLoader.discover(str(ROOT / "tests"))
    tests = [t for t in flatten(suite) if not opts.k or opts.k in t.id()]
    broken = [t for t in tests if type(t).__name__ in ("_FailedTest", "ModuleImportFailure")]
    if broken:
        for test in broken:
            print(f"ERROR: {test.id()}", file=sys.stderr)
        return 1
    # Deal tests round-robin so slow ones from one class spread over the workers.
    global WORKERS
    TESTS[:] = tests
    WORKERS = opts.j
    started = time.monotonic()
    with multiprocessing.get_context("fork").Pool(opts.j) as pool:
        results = pool.map(work, range(opts.j), chunksize=1)
    failures = [f for fs, _ in results for f in fs]
    timings = sorted((t for _, ts in results for t in ts), reverse=True)
    for test_id, trace in failures:
        print(f"FAIL: {test_id}\n{trace}", file=sys.stderr)
    for seconds, test_id in timings[: opts.slowest]:
        print(f"  {seconds:6.1f}s {test_id}")
    print(f"Ran {len(tests)} tests in {time.monotonic() - started:.1f}s with {opts.j} workers: "
          f"{'FAILED (' + str(len(failures)) + ')' if failures else 'OK'}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
