"""Run records: where they live, their state, the machine-wide pool, removal."""

import json
import os
import shutil
import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path

from .common import (
    ACTIVE, DEFAULT_LIMITS, DEFAULT_MIN_AVAILABLE_MB, FINISHED_OK, SCRIPT, STARTING_GRACE, die, read_json, setting,
)
from .worktree import remove_worktree


def runs_root():
    if setting("RUNS"):
        return Path(setting("RUNS"))
    try:
        top = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True,
                             check=True).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        top = os.getcwd()
    return Path(top) / ".local/run/pi"



def all_runs():
    root = runs_root()
    if not root.is_dir():
        return []
    runs = [d for d in root.iterdir() if (d / "meta.json").is_file()]
    return sorted(runs, key=lambda d: (read_json(d / "meta.json", {}) or {}).get("startedNs", 0))



def resolve_run(ref):
    path = Path(ref)
    if (path / "meta.json").is_file():
        return path.resolve()
    root = runs_root()
    runs = all_runs()
    if ref == "last":
        if not runs:
            die(f"no runs under {root}")
        return runs[-1]
    if (root / ref / "meta.json").is_file():
        return root / ref
    matches = [d for d in runs if ref in d.name]
    if len(matches) != 1:
        die(f"run '{ref}' matched {len(matches)} runs under {root} (runs live under the git root of the "
            "directory where start ran; pass a run directory or set DELEGATE_RUNS)")
    return matches[0]



def pid_alive(pid):
    try:
        os.kill(pid, 0)
    except (ProcessLookupError, PermissionError, TypeError):
        return False
    return True



def supervisor_alive(run):
    try:
        pid = int((run / "pid").read_text())
        return pid_alive(pid) and b"_supervise" in Path(f"/proc/{pid}/cmdline").read_bytes()
    except (OSError, ValueError):
        return False



def agent_alive(run):
    """The agent's process group still exists, e.g. after its supervisor was killed."""
    try:
        pid = int((run / "agent.pid").read_text())
        return pid_alive(pid) and os.getpgid(pid) == pid
    except (OSError, ValueError):
        return False


def run_state(run):
    if (run / "exit_code").is_file():
        return (read_json(run / "summary.json", {}) or {}).get("state", "crashed")
    if supervisor_alive(run):
        return "running"
    if (run / "pid").is_file():
        return "crashed"
    # A supervisor that never started must not block writers in this workdir forever.
    started = (read_json(run / "meta.json", {}) or {}).get("startedEpoch", 0)
    return "starting" if time.time() - started <= STARTING_GRACE else "crashed"



def events(run):
    out = []
    try:
        with open(run / "events.jsonl", encoding="utf-8") as stream:
            for line in stream:
                try:
                    out.append(json.loads(line))
                except ValueError:
                    continue
    except OSError:
        pass
    return out



def status(run):
    meta = read_json(run / "meta.json", {}) or {}
    state = run_state(run)
    summary = read_json(run / "summary.json", {}) or {}
    out = {"run": meta.get("run", run.name), "name": meta.get("name"), "state": state,
           "agent": meta.get("agent", "pi"), "mode": meta.get("mode")}
    if meta.get("tier"):
        out["tier"] = meta["tier"]
    for key in ("parent", "worktree"):
        if meta.get(key):
            out[key] = meta[key]["path"] if key == "worktree" else meta[key]
    if summary:
        for key in ("elapsedSeconds", "attempts", "model", "turns", "files", "changes", "accept", "readOnlyViolation",
                    "workspaceChanged", "escalatedFrom", "queuedSeconds", "graceSeconds", "tokens", "warning",
                    "error"):
            if summary.get(key) not in (None, [], {}):
                out[key] = summary[key]
    else:
        evs = events(run)
        turns = [e for e in evs if e.get("e") == "turn"]
        out["elapsedSeconds"] = int(time.time() - meta.get("startedEpoch", time.time()))
        out["turns"] = len(turns)
        files = sorted({e["path"] for e in evs if e.get("e") in ("edit", "write") and e.get("path")})
        if files:
            out["files"] = files
        acts = [e for e in evs if e.get("e") not in ("turn", "result", "settled", "agent_end", "bash_done")]
        if state == "running" and acts:
            last = acts[-1]
            detail = last.get("cmd") or last.get("path") or last.get("arg") or last.get("detail") or ""
            out["last"] = last["e"] + (f": {detail}" if detail else "")
            try:
                at = datetime.strptime(last["at"], "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc)
                out["idleSeconds"] = int(time.time() - at.timestamp())
            except (KeyError, ValueError):
                pass
    result = run / "result.md"
    if result.is_file() and result.stat().st_size:
        out["result"] = str(result)
        out["resultChars"] = len(result.read_text(encoding="utf-8"))
    step = next_step(run, meta, state, summary)
    if step:
        out["next"] = step
    out["dir"] = str(run)
    return out



def next_step(run, meta, state, summary):
    """What the caller does with this outcome, so it need not keep the state table in mind."""
    name, script = meta.get("run", run.name), SCRIPT
    if state in ACTIVE:
        return f"{script} wait {name}"
    if state in FINISHED_OK:
        if meta.get("mode") != "write":
            return None  # the answer is the deliverable
        if not (summary.get("changes") or {}).get("files"):
            return None
        if not meta.get("worktree"):
            return f"review {script} diff {name}; the changes are already in the working tree"
        if not (run / ".applied").exists():
            return f"review {script} diff {name} --total, then merge with {script} apply {name}"
        return None
    return {"rejected": f"read accept.tail; {script} reply {name} '<what to fix>' or take it over",
            "malformed": "hand it to the other agent or do it yourself",
            "failed": "read error; fix the cause or take it over",
            "timeout": "split the task smaller or take it over",
            "killed": "take it over or start it again", "crashed": "take it over or start it again"}.get(state)



def unmerged_worktree(run):
    meta = read_json(run / "meta.json", {}) or {}
    tree = meta.get("worktree") or {}
    # A read-only run's worktree is only a snapshot to read; it has nothing to merge.
    return meta.get("mode") == "write" and bool(tree.get("path")) and Path(tree["path"]).exists() \
        and not (run / ".applied").exists()



def remove_run(run):
    """Delete a run directory; its worktree goes with the last run that uses it."""
    meta = read_json(run / "meta.json", {}) or {}
    shutil.rmtree(run, ignore_errors=True)
    path = (meta.get("worktree") or {}).get("path")
    if path and not any(((read_json(r / "meta.json", {}) or {}).get("worktree") or {}).get("path") == path
                        for r in all_runs()):
        remove_worktree(meta)



def prune_expired():
    days = setting("KEEP_DAYS", "7")
    if not days.isdigit() or int(days) == 0:
        return
    cutoff = time.time() - int(days) * 86400
    for run in all_runs():
        done = run / "exit_code"
        if (run / ".delivered").is_file() and done.is_file() and done.stat().st_mtime < cutoff \
                and not unmerged_worktree(run):
            remove_run(run)



def limit(name):
    value = setting(name, str(DEFAULT_LIMITS[name]))
    if not value.isdigit():
        die(f"DELEGATE_{name} must be a non-negative integer (0 = unlimited), got {value!r}")
    return int(value)



def machine_runs(slots):
    """Active runs on this machine, across projects; forget slots whose run has ended or vanished."""
    active = []
    for slot in slots.glob("*.slot"):
        try:
            run = Path(slot.read_text().strip())
        except OSError:
            continue
        if (run / "meta.json").is_file() and (run_state(run) in ACTIVE or agent_alive(run)):
            active.append(run)
        else:
            slot.unlink(missing_ok=True)
    return active



def capacity_error(agent, active):
    agents = [(read_json(run / "meta.json", {}) or {}).get("agent", "pi") for run in active]
    total, codex = limit("MAX_ACTIVE"), limit("MAX_CODEX")
    if total and len(active) >= total:
        reason = f"{len(active)} runs are active on this machine (DELEGATE_MAX_ACTIVE={total})"
    elif agent == "codex" and codex and agents.count("codex") >= codex:
        reason = f"{agents.count('codex')} Codex runs are active on this machine (DELEGATE_MAX_CODEX={codex})"
    else:
        return None
    listing = "".join(f"\n  {status(run)['elapsedSeconds']:>5}s {kind:<5} {run}" for run, kind in zip(active, agents))
    return (f"refusing to start: {reason}; collect results with wait before starting more, or stop runs "
            f"no longer needed{listing}")



def available_mb():
    try:
        for line in Path("/proc/meminfo").read_text().splitlines():
            if line.startswith("MemAvailable:"):
                return int(line.split()[1]) // 1024
    except (OSError, ValueError, IndexError):
        pass
    return None



def memory_error(active):
    """Refuse a start that would leave the machine short of memory, before the OOM killer picks a victim."""
    floor = setting("MIN_AVAILABLE_MB", str(DEFAULT_MIN_AVAILABLE_MB))
    if not floor.isdigit():
        die(f"DELEGATE_MIN_AVAILABLE_MB must be a non-negative integer (0 = no check), got {floor!r}")
    available = available_mb()
    if not int(floor) or available is None or available >= int(floor):
        return None
    listing = "".join(f"\n  {status(run)['elapsedSeconds']:>5}s {run}" for run in active)
    return (f"refusing to start: only {available} MB of memory available (DELEGATE_MIN_AVAILABLE_MB={floor}); "
            f"collect results with wait, or stop runs no longer needed{listing}")



def replies_to(name, runs=None):
    """Replies that continue a run. A malformed one is a dead end: the conversation goes on from its parent."""
    return [r for r in (runs if runs is not None else all_runs())
            if (read_json(r / "meta.json", {}) or {}).get("parent") == name and run_state(r) != "malformed"]


def latest_in_chain(run):
    """A conversation is named by its first run; replies and apply act on its latest one."""
    runs = all_runs()
    while True:
        replies = replies_to(run.name, runs)
        if not replies:
            return run
        run = replies[-1]
