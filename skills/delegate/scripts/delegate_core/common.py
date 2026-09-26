"""Shared constants, settings and small helpers."""

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import threading
import time
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



ENV_RUN_DIR = "DELEGATE_RUN_DIR"  # handed to an agent: where its `lane` waits are accounted



LANE_HELD = "DELEGATE_LANE_HELD"  # set inside a heavy slot, so a nested `lane` runs at once instead of deadlocking



# Never inherited by an agent, acceptance, setup or supervisor: each is set afresh where it applies.
GUARD_VARS = (ENV_AGENT, ENV_PARENT, ENV_LEGACY, ENV_RUN_DIR, LANE_HELD, "PI_DELEGATE_AGENT", "PI_DELEGATE_PARENT_RUN")



DEFAULT_MAX_HEAVY = 1  # heavy commands (checks, acceptance, setup) running at once on this machine



DEFAULT_MIN_AVAILABLE_MB = 4096  # a start is refused below this much available memory



DEFAULT_TIMEOUT_GRACE = 50  # percent of --timeout an agent may run over while it is visibly still at work



BUSY_WINDOW = 120  # seconds since its last event within which an agent counts as at work



CONFIG_FILE = ".delegate.json"  # per repo, at its git root: {"worktree": {"copy": [], "link": [], "setup": []}, "env": {}}



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



def state_dir():
    # Deliberately not a DELEGATE_* setting: a delegated agent must not opt out of the machine's pool.
    return Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state") / "delegate"



def kill_group(pgid, grace=5.0):
    try:
        os.killpg(pgid, signal.SIGTERM)
    except ProcessLookupError:
        return
    deadline = time.time() + grace
    while time.time() < deadline:
        try:
            os.killpg(pgid, 0)
        except ProcessLookupError:
            return
        time.sleep(0.1)
    try:
        os.killpg(pgid, signal.SIGKILL)
    except ProcessLookupError:
        pass



def group_members(pgid):
    """Processes of a process group that are still running (zombies excluded)."""
    members = []
    for entry in os.listdir("/proc"):
        if entry.isdigit():
            try:
                fields = Path(f"/proc/{entry}/stat").read_text().rsplit(")", 1)[1].split()
            except (OSError, IndexError):
                continue
            if fields[2] == str(pgid) and fields[0] != "Z":
                members.append(int(entry))
    return members



def end_group(pgid, grace):
    """SIGTERM a process group, then SIGKILL whatever is left after grace seconds."""
    for sig in (signal.SIGTERM, signal.SIGKILL):
        try:
            os.killpg(pgid, sig)
        except ProcessLookupError:
            return
        deadline = time.monotonic() + grace
        while group_members(pgid) and time.monotonic() < deadline:
            time.sleep(0.05)
        if not group_members(pgid):
            return



def run_shell(command, cwd, env, timeout, out, holder=None):
    """Run a shell command in its own process group; returns (exit code, timed out).

    The whole group goes on timeout and after the command, so neither a timed-out check nor what it left
    running in the background (servers, watchers, stray test workers) keeps holding memory. The shell is reaped
    only after that: until then its pid still names the group, so no signal can reach a process that reused it.
    """
    proc = subprocess.Popen(command, shell=True, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=out,
                            stderr=subprocess.STDOUT, start_new_session=True)
    if holder is not None:
        holder["pgid"] = proc.pid
    exited = threading.Event()

    def watch():
        try:
            os.waitid(os.P_PID, proc.pid, os.WEXITED | os.WNOWAIT)  # wait without reaping
        except ChildProcessError:
            pass
        exited.set()

    threading.Thread(target=watch, daemon=True).start()
    timed_out = not exited.wait(timeout)
    end_group(proc.pid, 5 if timed_out else 2)
    if holder is not None:
        holder.pop("pgid", None)  # a stop after this must not signal a pid the system may hand out again
    code = proc.wait()
    return (124 if timed_out else code), timed_out
