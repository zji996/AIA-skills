#!/usr/bin/env python3
"""Measure gates without persisting command arguments or output."""

import argparse
import csv
import datetime
import json
import math
import os
from pathlib import Path
import re
import selectors
import statistics
import subprocess
import sys
import time


NUMBER = r"\d+(?:\.\d+)?"
ANSI = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")


def duration(value):
    if not isinstance(value, str):
        raise ValueError("budget must be a duration string, e.g. 10m or 1m30s")
    if re.fullmatch(NUMBER, value):
        seconds = float(value)
    else:
        parts = list(re.finditer(rf"({NUMBER})(ms|s|m|h|d)", value))
        if not parts or "".join(part.group() for part in parts) != value:
            raise ValueError("invalid budget; use seconds or units ms/s/m/h/d (e.g. 10m)")
        factors = {"ms": .001, "s": 1, "m": 60, "h": 3600, "d": 86400}
        seconds = sum(float(part[1]) * factors[part[2]] for part in parts)
    if not math.isfinite(seconds) or seconds <= 0:
        raise ValueError("budget must be finite and greater than zero")
    return seconds


def load_budget(root, name, explicit):
    if explicit is not None:
        return duration(explicit)
    config = root / ".iteration-speed.json"
    if not config.exists():
        return None
    settings = json.loads(config.read_text(encoding="utf-8"))
    if not isinstance(settings, dict) or not isinstance(settings.get("gates", {}), dict):
        raise ValueError(".iteration-speed.json must contain a gates object")
    value = settings.get("gates", {}).get(name)
    return duration(value) if value is not None else None


class CargoSummary:
    """Pair ordered Cargo launches/results even when they use different streams."""

    def __init__(self):
        self.launches = []
        self.results = []
        self.tests = []

    def feed(self, line):
        line = ANSI.sub("", line).strip()
        running = re.fullmatch(r"Running .+ \((.+)\)", line)
        if running:
            # Keep the binary label for terminal summaries, never its absolute path.
            self.launches.append(re.split(r"[/\\]", running[1])[-1])
        elif re.match(r"Doc-tests\s+", line):
            self.launches.append(None)
        finished = re.search(rf"^test result: .*; finished in ({NUMBER})s\s*$", line)
        if finished:
            self.results.append(float(finished[1]))
        test = re.fullmatch(rf"test (.+?) \.\.\. (?:ok|FAILED|ignored) <({NUMBER})s>", line)
        if test:
            self.tests.append((float(test[2]), test[1]))

    def slowest(self):
        # Missing/custom harness results make ordinal pairing ambiguous; skip them.
        binaries = []
        if len(self.launches) == len(self.results):
            binaries = [(seconds, name) for name, seconds in zip(self.launches, self.results)
                        if name is not None]
        return sorted(binaries, reverse=True)[:5], sorted(self.tests, reverse=True)[:5]


class OutputLines:
    def __init__(self, summary):
        self.summary = summary
        self.pending = b""
        self.discarding = False

    def feed(self, chunk, final=False):
        lines = (self.pending + chunk).split(b"\n")
        self.pending = lines.pop()
        for line in lines:
            if not self.discarding and len(line) <= 65536:
                self.summary.feed(line.decode("utf-8", errors="replace"))
            self.discarding = False
        # An unrecognized enormous line must not turn output capture into a log store.
        if len(self.pending) > 65536:
            self.pending = b""
            self.discarding = True
        if final and not self.discarding:
            self.summary.feed(self.pending.decode("utf-8", errors="replace"))
            self.pending = b""


def run_command(command, summary):
    try:
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    except OSError as error:
        print(f"gate: {error.strerror}", file=sys.stderr)
        return 127 if isinstance(error, FileNotFoundError) else 126
    with selectors.DefaultSelector() as selector:
        for stream, output in ((process.stdout, sys.stdout.buffer),
                               (process.stderr, sys.stderr.buffer)):
            selector.register(stream, selectors.EVENT_READ, (output, OutputLines(summary)))
        while selector.get_map():
            for key, _ in selector.select():
                output, parser = key.data
                chunk = os.read(key.fd, 65536)
                if chunk:
                    output.write(chunk)
                    output.flush()
                    parser.feed(chunk)
                else:
                    parser.feed(b"", final=True)
                    selector.unregister(key.fileobj)
                    key.fileobj.close()
    status = process.wait()
    return status if status >= 0 else 128 - status


def recent_median(log, name):
    if not log.exists():
        return None
    successful = []
    with log.open(encoding="utf-8") as file:
        for row in csv.reader(file, delimiter="\t"):
            if len(row) < 5 or row[2:5] != ["0", "gate", name]:
                continue
            try:
                seconds = float(row[1])
            except ValueError:
                continue
            if math.isfinite(seconds) and seconds >= 0:
                successful.append(seconds)
                successful = successful[-5:]
    return statistics.median(successful) if successful else None


def conclusion(elapsed, budget, status, baseline):
    messages = []
    if baseline is not None and elapsed > baseline * 1.3:
        messages.append(f"Gate regression: {elapsed:.3f}s exceeds recent successful median "
                        f"{baseline:.3f}s by more than 30%; inspect the slowest steps/tests.")
    if status == 0 and budget is not None and elapsed > budget:
        messages.append(f"Gate over budget: {elapsed:.3f}s, budget {budget:.3f}s, "
                        f"exceeded by {(elapsed / budget - 1) * 100:.1f}%; "
                        "next: inspect the slowest steps/tests, then follow SKILL.md decision points.")
        return 3, messages
    return status, messages


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    log, root = Path(argv.pop(0)), Path(argv.pop(0))
    parser = argparse.ArgumentParser(prog="iteration-speed gate")
    parser.add_argument("--budget")
    parser.add_argument("--name", default="default")
    if "--" not in argv:
        parser.error("requires -- COMMAND")
    separator = argv.index("--")
    args = parser.parse_args(argv[:separator])
    command = argv[separator + 1:]
    if not command:
        parser.error("requires a command")
    if not args.name or not args.name.isprintable():
        parser.error("name must be nonempty and contain no control characters")
    try:
        budget = load_budget(root, args.name, args.budget)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    baseline = recent_median(log, args.name)
    summary = CargoSummary()
    started = time.perf_counter()
    status = run_command(command, summary)
    elapsed = time.perf_counter() - started
    binaries, tests = summary.slowest()
    # Persist only numeric output summaries: test/binary labels may contain secrets.
    numeric_summary = {"binary_seconds": [seconds for seconds, _ in binaries],
                       "test_seconds": [seconds for seconds, _ in tests]}
    log.parent.mkdir(parents=True, exist_ok=True)
    with log.open("a", encoding="utf-8", newline="") as file:
        csv.writer(file, delimiter="\t", lineterminator="\n").writerow([
            datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
            f"{elapsed:.6f}", status, "gate", args.name,
            json.dumps(numeric_summary, separators=(",", ":")),
        ])
    print(f"Gate {args.name}: {elapsed:.3f}s, command exit {status}", file=sys.stderr)
    for label, entries in (("Slowest test binaries", binaries), ("Slowest tests", tests)):
        if entries:
            print(f"{label}: " + "; ".join(f"{name} {seconds:.3f}s" for seconds, name in entries),
                  file=sys.stderr)
    code, messages = conclusion(elapsed, budget, status, baseline)
    for message in messages:
        print(message, file=sys.stderr)
    return code


if __name__ == "__main__":
    sys.exit(main())
