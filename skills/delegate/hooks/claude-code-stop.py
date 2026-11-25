#!/usr/bin/env python3
"""Thin Claude Code Stop hook wrapper for delegate.

Keeps `~/.claude/settings.json` hook path stable while delegating all zero-lock,
zero-git Stop evaluation (W058 read-state/caller liveness, W060 progressive
single-item disclosure, stall-chain escalation, cross-repo penetration,
per-run deferrals, verbatim source HEAD detection, and fastest-ETA clock in
existing waiter reminders) to `delegate hook check --format claude`.
"""

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path


def find_delegate_bin() -> str | None:
    override = os.environ.get("DELEGATE_BIN", "").strip()
    if override and os.access(override, os.X_OK):
        return override
    local_bin = Path(__file__).resolve().parent.parent / "bin" / "delegate"
    if local_bin.is_file() and os.access(local_bin, os.X_OK):
        return str(local_bin)
    claude_bin = Path.home() / ".claude" / "skills" / "delegate" / "bin" / "delegate"
    if claude_bin.is_file() and os.access(claude_bin, os.X_OK):
        return str(claude_bin)
    return shutil.which("delegate")


def main() -> int:
    bin_path = find_delegate_bin()
    if not bin_path:
        return 0
    try:
        res = subprocess.run([bin_path, "hook", "check", "--format", "claude"], check=False)
        return res.returncode
    except OSError:
        return 0


if __name__ == "__main__":
    sys.exit(main())
