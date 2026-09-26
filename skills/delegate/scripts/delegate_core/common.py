"""Shared constants, settings and small helpers."""

import argparse
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path


RUNNING_EXIT = 75



USAGE_EXIT = 2



SCRIPT = Path(__file__).resolve().parents[1] / "delegate.py"  # the entry point, also for _supervise



FINISHED_OK = ("delivered", "answered")



ACTIVE = ("starting", "running")



STARTING_GRACE = 15  # seconds a run may wait for its supervisor before it counts as crashed



AGENTS = ("pi", "codex")



DEFAULT_TIMEOUT = {"pi": "15m", "codex": "30m"}



DEFAULT_LIMITS = {"MAX_ACTIVE": 6, "MAX_CODEX": 3}



# Environment handed to a delegated agent; used to refuse self-delegation.
ENV_AGENT, ENV_PARENT, ENV_LEGACY = "DELEGATE_AGENT", "DELEGATE_PARENT_RUN", "PI_DELEGATE_ACTIVE"



GUARD_VARS = (ENV_AGENT, ENV_PARENT, ENV_LEGACY, "PI_DELEGATE_AGENT", "PI_DELEGATE_PARENT_RUN")



CONFIG_FILE = ".delegate.json"  # per repo, at its git root: {"worktree": {"copy": [], "link": [], "setup": []}}



LARGE_UNTRACKED = 2 << 20  # untracked files above this are fingerprinted, not stored in the snapshot



CHANGES_SHOWN = 40



def setting(name, default=None):
    return os.environ.get(f"DELEGATE_{name}") or os.environ.get(f"PI_DELEGATE_{name}") or default



def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")



def die(message, code=USAGE_EXIT):
    print(f"delegate: {message}", file=sys.stderr)
    sys.exit(code)



def seconds(text):
    match = re.fullmatch(r"(\d+(?:\.\d+)?)([smhd]?)", text or "")
    if not match or float(match.group(1)) <= 0:
        raise argparse.ArgumentTypeError(f"invalid positive duration: {text} (e.g. 90, 90s, 15m, 1h)")
    return float(match.group(1)) * {"": 1, "s": 1, "m": 60, "h": 3600, "d": 86400}[match.group(2)]



def read_json(path, default=None):
    try:
        return json.loads(Path(path).read_text())
    except (OSError, ValueError):
        return default



def write_json(path, value):
    temp = Path(str(path) + ".tmp")
    try:
        temp.write_text(json.dumps(value, ensure_ascii=False) + "\n", encoding="utf-8")
    except UnicodeEncodeError:  # an undecodable file name; escaped JSON still round-trips
        temp.write_text(json.dumps(value) + "\n", encoding="utf-8")
    temp.replace(path)



def emit(value):
    print(json.dumps(value, ensure_ascii=False), flush=True)



def clip(text, limit):
    text = str(text)
    return text if len(text) <= limit else text[:limit] + "..."



def git(top, *args, env=None, text=True):
    # Paths are bytes on Linux: undecodable file names round-trip through surrogateescape like os.fsdecode.
    return subprocess.run(["git", "-C", str(top), *args], capture_output=True, check=True, env=env,
                          **({"encoding": "utf-8", "errors": "surrogateescape"} if text else {})).stdout



def git_top(path):
    try:
        return git(path, "rev-parse", "--show-toplevel").strip()
    except (OSError, subprocess.CalledProcessError):
        return None
