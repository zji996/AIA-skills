#!/usr/bin/env python3
"""Record one gate step and skip it when the same inputs already passed.

The inputs digest comes from git: blob ids of tracked files plus hashes of dirty and
untracked (not ignored) files, limited by pathspecs. Records keep the step name,
digests, exit code and seconds; never commands, output or file names.
"""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time


def git(root, *args):
    return subprocess.run(["git", "-C", str(root), *args], capture_output=True, check=True).stdout


def pathspecs(includes, excludes):
    return [*(includes or ["."]), *(f":(exclude){path}" for path in excludes)]


def inputs_digest(root, includes, excludes):
    """Digest of the scoped tree as it is on disk, including uncommitted edits."""
    specs = ["--", *pathspecs(includes, excludes)]
    digest = hashlib.sha256(b"iteration-speed.step.v1\n")
    digest.update(git(root, "ls-files", "-s", "-z", *specs))
    changed = git(root, "diff", "--name-only", "-z", "HEAD", *specs).split(b"\0")
    untracked = git(root, "ls-files", "-o", "--exclude-standard", "-z", *specs).split(b"\0")
    for name in sorted({*changed, *untracked} - {b""}):
        path = root / os.fsdecode(name)
        digest.update(name + b"\0")
        try:
            digest.update(hashlib.sha256(path.read_bytes()).digest() if path.is_file() else b"-")
        except OSError:
            digest.update(b"?")
    return digest.hexdigest()


def key_digest(commands):
    """Tool versions or host facts that also decide whether a pass still holds."""
    digest = hashlib.sha256(b"iteration-speed.key.v1\n")
    for command in commands:
        try:
            output = subprocess.run(command, shell=True, capture_output=True, timeout=30).stdout
        except (OSError, subprocess.TimeoutExpired):
            output = b"?"
        digest.update(command.encode() + b"\0" + output + b"\0")
    return digest.hexdigest()


def repo_identity(root):
    """Same project across clones and worktrees: the root commit(s)."""
    try:
        roots = git(root, "rev-list", "--max-parents=0", "HEAD").split()
    except subprocess.CalledProcessError:
        return hashlib.sha256(str(root).encode()).hexdigest()[:16]
    return hashlib.sha256(b"".join(sorted(roots))).hexdigest()[:16]


def records(log):
    if not log.exists():
        return []
    rows = []
    for line in log.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            row = json.loads(line)
        except ValueError:
            continue
        if isinstance(row, dict):
            rows.append(row)
    return rows


def append(log, row):
    try:
        log.parent.mkdir(parents=True, exist_ok=True)
        # One write per line keeps parallel steps from interleaving records.
        with log.open("a", encoding="utf-8") as file:
            file.write(json.dumps(row, separators=(",", ":")) + "\n")
    except OSError as error:
        print(f"step: cannot record ({error.strerror}); result not saved", file=sys.stderr)


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    log, root = Path(argv.pop(0)), Path(argv.pop(0))
    parser = argparse.ArgumentParser(prog="iteration-speed step")
    parser.add_argument("--name", required=True)
    parser.add_argument("--include", action="append", default=[], help="pathspec in scope (default: all)")
    parser.add_argument("--exclude", action="append", default=[], help="pathspec out of scope")
    parser.add_argument("--key", action="append", default=[], help="command whose output joins the key")
    parser.add_argument("--force", action="store_true")
    if "--" not in argv:
        parser.error("requires -- COMMAND")
    separator = argv.index("--")
    args = parser.parse_args(argv[:separator])
    command = argv[separator + 1:]
    if not command:
        parser.error("requires a command")
    if not args.name.isprintable():
        parser.error("name must contain no control characters")
    force = args.force or os.environ.get("ITERATION_SPEED_FORCE") == "1"
    try:
        inputs = inputs_digest(root, args.include, args.exclude)
    except (OSError, subprocess.CalledProcessError):
        inputs = None  # not a git checkout: always run, still record the time
    key = key_digest(args.key)
    repo = repo_identity(root)
    if inputs and not force:
        latest = next((row for row in reversed(records(log))
                       if row.get("repo") == repo and row.get("name") == args.name), None)
        if latest and latest.get("exit") == 0 and latest.get("inputs") == inputs and latest.get("key") == key:
            print(f"{args.name} reused (passed {latest.get('at')}, {latest.get('seconds', 0):.1f}s)",
                  file=sys.stderr)
            return 0
    started = time.perf_counter()
    try:
        status = subprocess.run(command).returncode
    except OSError as error:
        print(f"step: {error.strerror}", file=sys.stderr)
        status = 127 if isinstance(error, FileNotFoundError) else 126
    status = status if status >= 0 else 128 - status
    seconds = time.perf_counter() - started
    append(log, {
        "at": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "repo": repo, "name": args.name, "exit": status, "seconds": round(seconds, 3),
        "inputs": inputs, "key": key,
    })
    print(f"{args.name} exit={status} {seconds:.1f}s", file=sys.stderr)
    return status


def report(log, root):
    repo = repo_identity(root)
    latest = {}
    for row in records(log):
        if row.get("repo") == repo:
            latest.setdefault(row.get("name"), []).append(row)
    if not latest:
        print("no step records for this repository")
        return 0
    print("step\tlast\texit\tseconds\tmedian(5 passes)")
    for name, rows in sorted(latest.items(), key=lambda item: str(item[0])):
        passes = sorted(r.get("seconds", 0) for r in [r for r in rows if r.get("exit") == 0][-5:])
        median = passes[len(passes) // 2] if passes else None
        last = rows[-1]
        print(f"{name}\t{last.get('at')}\t{last.get('exit')}\t{last.get('seconds')}\t"
              f"{'' if median is None else median}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--report":
        sys.exit(report(Path(sys.argv[2]), Path(sys.argv[3])))
    sys.exit(main())
