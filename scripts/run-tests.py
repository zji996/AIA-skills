#!/usr/bin/env python3
"""Run the unittest suite across worker processes.

Tests are black-box and isolated by their own temporary directories, so whole test methods can
run side by side. Heavy ThreadPoolExecutor stress tests (or tests marked `_stress_serial = True`)
are split out of the parallel worker pool and executed serially afterward so they do not starve
other tests. Workers are forked after discovery, so module-level setup (such as building the
delegate binary) happens once. Slowest tests are listed so the suite can be kept lean.

    run-tests.py [-j N] [-k PATTERN] [--slowest N] [--repeat N]
"""
import argparse
import inspect
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


def is_stress_serial(test):
    """Return True if a test spawns its own ThreadPoolExecutor or opts into `_stress_serial`."""
    method_name = getattr(test, "_testMethodName", None)
    cls = type(test)
    method = getattr(cls, method_name, None) if method_name else None
    if getattr(test, "_stress_serial", False) or getattr(cls, "_stress_serial", False) or getattr(method, "_stress_serial", False):
        return True
    if method is not None:
        try:
            if "ThreadPoolExecutor" in inspect.getsource(method):
                return True
        except (OSError, TypeError):
            pass
    return False


# Filled before the workers fork; each worker takes every N-th item, so nothing is pickled.
ITEMS = []
WORKERS = 1
REPEAT = 1


def run_one(item):
    round_idx, test = item
    fresh = type(test)(test._testMethodName)
    result = unittest.TestResult()
    started = time.monotonic()
    fresh.run(result)
    elapsed = time.monotonic() - started
    label = f"{fresh.id()} [run {round_idx}/{REPEAT}]" if REPEAT > 1 else fresh.id()
    failures = []
    for case, trace in result.errors + result.failures:
        case_label = f"{case.id()} [run {round_idx}/{REPEAT}]" if REPEAT > 1 else case.id()
        failures.append((case_label, trace))
    for case in result.unexpectedSuccesses:
        case_label = f"{case.id()} [run {round_idx}/{REPEAT}]" if REPEAT > 1 else case.id()
        failures.append((case_label, "unexpected success"))
    return failures, (elapsed, label)


def work(index):
    failures, timings = [], []
    for item in ITEMS[index::WORKERS]:
        item_failures, timing = run_one(item)
        failures.extend(item_failures)
        timings.append(timing)
    return failures, timings


def run_phase(items, workers):
    if not items:
        return [], []
    global WORKERS
    ITEMS[:] = items
    WORKERS = min(workers, len(items))
    with multiprocessing.get_context("fork").Pool(WORKERS) as pool:
        results = pool.map(work, range(WORKERS), chunksize=1)
    failures = [f for fs, _ in results for f in fs]
    timings = [t for _, ts in results for t in ts]
    return failures, timings


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("-j", type=int, default=min(8, max(1, (os.cpu_count() or 2) // 2)))
    parser.add_argument("-k", default=None, help="only test ids containing this text")
    parser.add_argument("--slowest", type=int, default=8)
    parser.add_argument("--repeat", type=int, default=1, help="run each selected test N times (any failure fails)")
    opts = parser.parse_args()
    if opts.repeat < 1:
        parser.error("--repeat must be >= 1")
    if opts.j < 1:
        parser.error("-j must be >= 1")
    os.chdir(ROOT)
    suite = unittest.defaultTestLoader.discover(str(ROOT / "tests"))
    tests = [t for t in flatten(suite) if not opts.k or opts.k in t.id()]
    broken = [t for t in tests if type(t).__name__ in ("_FailedTest", "ModuleImportFailure")]
    if broken:
        for test in broken:
            print(f"ERROR: {test.id()}", file=sys.stderr)
        return 1
    regular = [t for t in tests if not is_stress_serial(t)]
    serial = [t for t in tests if is_stress_serial(t)]
    # Deal tests round-robin so slow ones from one class spread over the workers.
    global REPEAT
    REPEAT = opts.repeat
    regular_items = [(r, t) for r in range(1, opts.repeat + 1) for t in regular]
    serial_items = [(r, t) for r in range(1, opts.repeat + 1) for t in serial]
    started = time.monotonic()
    reg_failures, reg_timings = run_phase(regular_items, opts.j)
    ser_failures, ser_timings = run_phase(serial_items, 1)
    failures = reg_failures + ser_failures
    timings = sorted(reg_timings + ser_timings, reverse=True)
    for test_id, trace in failures:
        print(f"FAIL: {test_id}\n{trace}", file=sys.stderr)
    for seconds, test_id in timings[: opts.slowest]:
        print(f"  {seconds:6.1f}s {test_id}")
    serial_note = f" ({len(serial)} serial)" if serial else ""
    repeat_note = f" x {opts.repeat} repeats ({len(tests) * opts.repeat} runs)" if opts.repeat > 1 else ""
    print(f"Ran {len(tests)} tests{serial_note}{repeat_note} in {time.monotonic() - started:.1f}s with {opts.j} workers: "
          f"{'FAILED (' + str(len(failures)) + ')' if failures else 'OK'}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
