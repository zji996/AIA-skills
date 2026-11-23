#!/usr/bin/env python3
"""Unified Claude Code Stop hook for delegate (6.36.0).

Replaces legacy `delegate-waiter.sh`, `delegate-unmerged.sh`, and earlier Stop hooks
with a single zero-lock, zero-git turn-end guard across the current repository and all
other repositories on this machine:
0. Event-stream race lock: unread alert events past `<state>/cursors.json`.
1. Questioning runs (waiting on controller decision) or live runs without a running
   `delegate wait` (recognizes both bare `delegate wait` in PATH and `/.../delegate wait`).
2. Unmerged delivered or rejected write runs without a fresh deferral in
   `<state>/work.jsonl` (`delegate work defer <run> --reason "…" --until <time>`).
   Skips zero-change deliveries and runs currently being applied, merged (serial merger
   or `_merge-worker` premerge), or gated (`delegate lane`).

Reads only `<state>/index.json`, `<state>/work.jsonl`, `<state>/events.jsonl`,
`<state>/cursors.json`, `<state>/lane/*.ticket`, `/proc`, and run-dir JSON snapshots
(zero locks, zero git subprocesses). Appends one JSONL record per triggered hook class
to `<state>/stop-hook.jsonl` for `delegate hook stats`.

When `stop_hook_active` is true, exits 0 immediately to avoid blocking loops.
"""

from __future__ import annotations

import calendar
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path

UNMERGED_NEXT_RE = re.compile(r"^(?:review|apply)\b")
UNREAD_ALERT_KINDS = {
    "delivered",
    "question",
    "rejected",
    "failed",
    "stall_warning",
    "queue_paused",
    "needs_attention",
    "premerge_conflict",
    "premerge_failed",
    "work_ready",
    "work_defer_expired",
    "work_stale",
}
KIND_LABELS = {
    "delivered": "交付",
    "question": "提问",
    "rejected": "未通过",
    "failed": "失败",
    "stall_warning": "卡住提醒",
    "queue_paused": "续派暂停",
    "review_started": "开始审查",
    "review_verdict": "审查判定",
    "rework_sent": "退回返工",
    "needs_attention": "需主控关注",
    "premerge_ready": "预合并就绪",
    "premerge_conflict": "预合并冲突",
    "premerge_failed": "预合并失败",
    "merge_started": "开始合入",
    "merge_item": "合入进度",
    "merge_done": "合入完成",
    "radar_unchanged": "侦察未变",
    "radar_done": "侦察完成",
    "work_ready": "工作项就绪",
    "work_dispatched": "工作项自动派发",
    "work_defer_expired": "暂缓到期",
    "work_stale": "工作项停滞",
}
APPLY_VALUE_FLAGS = {"--closes", "--message", "-m", "--work"}


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


def state_dir() -> Path:
    base = os.environ.get("XDG_STATE_HOME")
    return (Path(base) if base else Path.home() / ".local" / "state") / "delegate"


def cache_dir() -> Path:
    base = os.environ.get("XDG_CACHE_HOME")
    return (Path(base) if base else Path.home() / ".cache") / "delegate"


def read_json_file(path: Path) -> dict | None:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        return data if isinstance(data, dict) else None
    except (OSError, ValueError):
        return None


def pid_alive(pid: int) -> bool:
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        return True
    except OSError:
        return False


def git_toplevel(path: str) -> Path | None:
    """Pure filesystem `.git` walk — strictly zero git subprocesses."""
    if not path or not os.path.isdir(path):
        return None
    try:
        cur = Path(path).resolve()
    except OSError:
        cur = Path(path)
    # If invoked inside `.git/...`, step out of `.git` first.
    if ".git" in cur.parts:
        idx = cur.parts.index(".git")
        if idx > 0:
            cur = Path(*cur.parts[:idx])
    for candidate in (cur, *cur.parents):
        if (candidate / ".git").exists():
            return candidate
    return None


def repo_from_runs_dir(runs_dir: Path) -> Path | None:
    parts = runs_dir.parts
    if len(parts) >= 4 and parts[-3:] in ((".local", "run", "delegate"), (".local", "run", "pi")):
        return runs_dir.parent.parent.parent
    return None


def load_index_runs() -> dict[str, dict]:
    """Lock-free read of `<state>/index.json` runs map (zero subprocesses)."""
    doc = read_json_file(state_dir() / "index.json")
    if not doc:
        return {}
    runs_map = doc.get("runs")
    return runs_map if isinstance(runs_map, dict) else {}


def discover_repos(cwd: str, index_runs: dict[str, dict]) -> list[tuple[Path | None, Path, str, bool]]:
    """Return deduplicated list of (repo_root, runs_dir, label, is_current)."""
    candidates: list[tuple[Path | None, Path, bool]] = []

    current_repo = git_toplevel(cwd)
    env_runs = os.environ.get("DELEGATE_RUNS") or os.environ.get("PI_DELEGATE_RUNS") or ""
    if env_runs.strip():
        runs_p = Path(env_runs.strip())
        repo_p = current_repo or repo_from_runs_dir(runs_p) or (Path(cwd) if cwd else None)
        candidates.append((repo_p, runs_p, True))
    elif current_repo is not None:
        candidates.append((current_repo, current_repo / ".local" / "run" / "delegate", True))
    elif cwd:
        p = Path(cwd)
        if (p / ".local" / "run" / "delegate").is_dir():
            candidates.append((p, p / ".local" / "run" / "delegate", True))

    if os.environ.get("DELEGATE_STOP_HOOK_CURRENT_ONLY") != "1":
        sdir = state_dir()
        # 1) Machine repo registry (`state/repos.json`).
        repos_doc = read_json_file(sdir / "repos.json")
        repos_map = repos_doc.get("repos") if repos_doc else None
        if isinstance(repos_map, dict):
            for entry in repos_map.values():
                if not isinstance(entry, dict):
                    continue
                runs_str = entry.get("runs")
                repo_str = entry.get("repo")
                if not isinstance(runs_str, str) or not runs_str:
                    continue
                runs_p = Path(runs_str)
                repo_p = (
                    Path(repo_str)
                    if isinstance(repo_str, str) and repo_str
                    else repo_from_runs_dir(runs_p)
                )
                candidates.append((repo_p, runs_p, False))

        # 2) Materialized index (`state/index.json`).
        for entry in index_runs.values():
            if not isinstance(entry, dict):
                continue
            runs_str = entry.get("runsRoot")
            repo_str = entry.get("repo")
            if isinstance(runs_str, str) and runs_str:
                runs_p = Path(runs_str)
                repo_p = (
                    Path(repo_str)
                    if isinstance(repo_str, str) and repo_str
                    else repo_from_runs_dir(runs_p)
                )
                candidates.append((repo_p, runs_p, False))

        # 3) Live slot files under state_dir.
        if sdir.is_dir():
            try:
                for entry in sdir.iterdir():
                    if entry.suffix != ".slot":
                        continue
                    try:
                        run_dir = Path(entry.read_text(encoding="utf-8").strip())
                    except OSError:
                        continue
                    if not (run_dir / "meta.json").is_file():
                        continue
                    runs_p = run_dir.parent
                    repo_p = repo_from_runs_dir(runs_p)
                    if repo_p is None:
                        meta = read_json_file(run_dir / "meta.json") or {}
                        wt = meta.get("worktree") if isinstance(meta.get("worktree"), dict) else {}
                        raw = wt.get("source") or meta.get("top") or meta.get("workdir")
                        if isinstance(raw, str) and raw:
                            repo_p = Path(raw)
                    candidates.append((repo_p, runs_p, False))
            except OSError:
                pass

        # 4) Worktree directories under cache_dir.
        wt_root = cache_dir() / "worktrees"
        if wt_root.is_dir():
            try:
                for entry in wt_root.iterdir():
                    git_file = entry / ".git"
                    if not git_file.is_file():
                        continue
                    try:
                        line = git_file.read_text(encoding="utf-8").strip()
                    except OSError:
                        continue
                    if line.startswith("gitdir:"):
                        gitdir = line.split(":", 1)[1].strip()
                        marker = "/.git/worktrees/"
                        if marker in gitdir:
                            repo_p = Path(gitdir.split(marker, 1)[0])
                            runs_p = repo_p / ".local" / "run" / "delegate"
                            if runs_p.is_dir():
                                candidates.append((repo_p, runs_p, False))
            except OSError:
                pass

    # Deduplicate by resolved runs_dir, preserving is_current=True when matched.
    dedup: dict[Path, tuple[Path | None, Path, bool]] = {}
    order: list[Path] = []
    for repo_p, runs_p, is_cur in candidates:
        if not runs_p.is_dir():
            continue
        try:
            key = runs_p.resolve()
        except OSError:
            key = runs_p
        if key in dedup:
            prev_repo, prev_runs, prev_cur = dedup[key]
            dedup[key] = (prev_repo or repo_p, prev_runs, prev_cur or is_cur)
        else:
            dedup[key] = (repo_p, runs_p, is_cur)
            order.append(key)

    # Compute readable labels; disambiguate colliding basenames.
    raw_items = [dedup[k] for k in order]
    base_counts: dict[str, int] = {}
    for repo_p, runs_p, _ in raw_items:
        base = (repo_p.name if repo_p else runs_p.parent.name) or str(runs_p)
        base_counts[base] = base_counts.get(base, 0) + 1

    result: list[tuple[Path | None, Path, str, bool]] = []
    for repo_p, runs_p, is_cur in raw_items:
        base = (repo_p.name if repo_p else runs_p.parent.name) or str(runs_p)
        if base_counts.get(base, 0) > 1 and repo_p is not None and repo_p.parent.name:
            label = f"{repo_p.parent.name}/{base}"
        else:
            label = base
        result.append((repo_p, runs_p, label, is_cur))
    return result


def strip_reply_prefixes(name: str) -> str:
    cur = name.strip()
    while cur.lower().startswith("reply to "):
        cur = cur[9:].strip()
    return cur or name


def read_run_snapshot(rdir: Path) -> dict | None:
    """Zero-lock, zero-git, zero-subprocess snapshot of a single run directory."""
    meta = read_json_file(rdir / "meta.json")
    if not meta:
        return None
    rid = str(meta.get("run") or rdir.name)
    name = str(meta.get("name") or rid)
    mode = str(meta.get("mode") or "")
    parent = meta.get("parent") if isinstance(meta.get("parent"), str) else ""
    summary = read_json_file(rdir / "summary.json") or {}
    exit_file = rdir / "exit_code"
    has_exit = exit_file.is_file()

    if has_exit:
        st = str(summary.get("state") or "")
        if not st:
            try:
                code = exit_file.read_text(encoding="utf-8").strip()
            except OSError:
                code = "1"
            if code == "0":
                st = "delivered" if mode == "write" else "answered"
            else:
                st = "failed"
    else:
        if (rdir / ".waiting").exists():
            st = "waiting"
        else:
            alive = False
            for pf in (rdir / "pid", rdir / "agent.pid"):
                if pf.is_file():
                    try:
                        if pid_alive(int(pf.read_text(encoding="utf-8").strip())):
                            alive = True
                            break
                    except (OSError, ValueError):
                        pass
            if alive:
                st = "running"
            else:
                # Newly starting run may not have written pid yet.
                started_ep = meta.get("startedEpoch")
                if isinstance(started_ep, (int, float)) and time.time() - float(started_ep) < 10.0:
                    st = "starting"
                else:
                    st = "crashed"

    is_applied = (
        (rdir / ".applied").exists()
        or (rdir / ".applied-cleaned").exists()
        or bool(meta.get("appliedAt"))
    ) and not (rdir / ".generate-pending").exists()

    wt = meta.get("worktree") if isinstance(meta.get("worktree"), dict) else {}
    wt_path = str(wt.get("path") or "") if wt else ""
    pm = read_json_file(rdir / "premerge.json")
    rev = read_json_file(rdir / "review.json")
    if rev is None and isinstance(summary.get("review"), dict):
        rev = summary.get("review")

    obj: dict = {
        "run": rid,
        "name": name,
        "chainName": strip_reply_prefixes(name),
        "state": st,
        "mode": mode,
        "parent": parent,
        "applied": is_applied,
        "cleaned": False,
        "question": summary.get("question") is True,
        "worktree": wt_path,
        "dir": str(rdir),
    }
    if isinstance(summary.get("pendingChanges"), dict):
        obj["pendingChanges"] = summary["pendingChanges"]
    if isinstance(summary.get("changes"), dict):
        obj["changes"] = summary["changes"]
    if isinstance(summary.get("files"), list):
        obj["files"] = summary["files"]
    if meta.get("isMerge") is True:
        obj["isMerge"] = True
        if isinstance(meta.get("merge"), dict):
            obj["merge"] = meta["merge"]
    if pm is not None:
        obj["premerge"] = pm
    if rev is not None:
        obj["review"] = rev

    # Synthesize `next` for finished unapplied write runs so downstream classification stays uniform.
    if st == "delivered" and mode == "write" and not is_applied:
        if rev and rev.get("verdict") == "NEEDS_CONTROLLER":
            obj["next"] = f"review {rid}"
        else:
            obj["next"] = f"apply {rid}"
    return obj


def fetch_status(runs_dir: Path, index_runs: dict[str, dict]) -> list[dict]:
    """Strictly lock-free, zero-git, zero-subprocess status reader backed by `index.json`
    with per-run disk snapshot fallback when missing or stale."""
    if not runs_dir.is_dir():
        return []
    try:
        actual_dirs = [
            p.resolve() for p in runs_dir.iterdir() if p.is_dir() and (p / "meta.json").is_file()
        ]
    except OSError:
        return []
    if not actual_dirs:
        return []

    out: list[dict] = []
    for rdir in actual_dirs:
        entry = index_runs.get(str(rdir))
        st_obj: dict | None = None
        if isinstance(entry, dict) and isinstance(entry.get("status"), dict):
            cand = dict(entry["status"])
            st = str(cand.get("state") or "")
            stale = False
            if st in ("running", "starting", "waiting", "queued"):
                if (rdir / "exit_code").is_file():
                    stale = True
                else:
                    pid_file = rdir / "pid"
                    if pid_file.is_file():
                        try:
                            if not pid_alive(int(pid_file.read_text(encoding="utf-8").strip())):
                                stale = True
                        except (OSError, ValueError):
                            stale = True
            else:
                if (rdir / ".applied").exists() and not (rdir / ".generate-pending").exists():
                    if cand.get("applied") is not True:
                        stale = True
            if not stale:
                cand.setdefault("dir", str(rdir))
                # Overlay fresh premerge.json directly from disk (lock-free).
                pm = read_json_file(rdir / "premerge.json")
                if pm is not None:
                    cand["premerge"] = pm
                # Preserve merge metadata for active serial merge runs.
                if st in ("running", "starting", "waiting", "queued") and not cand.get("isMerge"):
                    meta = read_json_file(rdir / "meta.json")
                    if meta and meta.get("isMerge") is True:
                        cand["isMerge"] = True
                        if isinstance(meta.get("merge"), dict):
                            cand["merge"] = meta["merge"]
                st_obj = cand
        if st_obj is None:
            st_obj = read_run_snapshot(rdir)
        if st_obj is not None and isinstance(st_obj.get("run"), str):
            out.append(st_obj)
    return out


def split_cmdline(line: str) -> list[str]:
    try:
        return shlex.split(line)
    except ValueError:
        return line.split()


def list_processes() -> list[tuple[list[str], Path | None]]:
    """List live processes as `(argv, cwd_or_none)` via `/proc` (zero subprocesses on Linux),
    with `ps` fallback."""
    override = os.environ.get("DELEGATE_STOP_HOOK_PROCS")
    if override is not None:
        return [(split_cmdline(ln), None) for ln in override.splitlines() if ln.strip()]

    proc = Path("/proc")
    if proc.is_dir():
        my_pid = os.getpid()
        procs: list[tuple[list[str], Path | None]] = []
        try:
            for entry in proc.iterdir():
                name = entry.name
                if not name.isdigit():
                    continue
                pid = int(name)
                if pid == my_pid:
                    continue
                try:
                    # Skip zombie processes.
                    stat_txt = (entry / "stat").read_text(encoding="utf-8", errors="ignore")
                    rparen = stat_txt.rfind(")")
                    if rparen != -1 and rparen + 2 < len(stat_txt) and stat_txt[rparen + 2] == "Z":
                        continue
                    raw = (entry / "cmdline").read_bytes()
                except OSError:
                    continue
                if not raw:
                    continue
                tokens = [t.decode("utf-8", errors="replace") for t in raw.split(b"\x00") if t]
                if not tokens:
                    continue
                pcwd: Path | None = None
                try:
                    pcwd = Path(os.readlink(str(entry / "cwd")))
                except OSError:
                    pass
                procs.append((tokens, pcwd))
            return procs
        except OSError:
            pass

    try:
        res = subprocess.run(
            ["ps", "-eo", "args="],
            capture_output=True,
            text=True,
            timeout=2,
        )
        if res.returncode == 0:
            return [(split_cmdline(ln), None) for ln in res.stdout.splitlines() if ln.strip()]
    except (OSError, subprocess.SubprocessError):
        pass
    return []


def cwd_in_repo(proc_cwd: Path | None, repo_root: Path | None) -> bool:
    """Return True if `proc_cwd` is unknown (synthetic/override) or inside `repo_root`."""
    if proc_cwd is None:
        return True
    if repo_root is None:
        return False
    try:
        p = proc_cwd.resolve()
        r = repo_root.resolve()
        return p == r or r in p.parents
    except OSError:
        return str(proc_cwd).startswith(str(repo_root))


def match_delegate_subcmd(tokens: list[str], subcmd: str) -> list[str] | None:
    """Match both bare `delegate <subcmd>` and `/path/to/delegate <subcmd>` (and `env ... delegate`).
    Returns the argument list following `<subcmd>`, or `None` if not matched."""
    if not tokens:
        return None
    idx = 0
    # Unwrap `env [VAR=VAL ...] [-u VAR] delegate ...`
    if os.path.basename(tokens[0]) == "env":
        idx = 1
        while idx < len(tokens):
            tok = tokens[idx]
            if tok == "--":
                idx += 1
                break
            if tok in ("-u", "-C", "-S"):
                idx += 2
                continue
            if tok.startswith("-") or "=" in tok:
                idx += 1
                continue
            break
    if idx >= len(tokens):
        return None
    if os.path.basename(tokens[idx]) != "delegate":
        return None
    idx += 1
    # Skip leading global flags such as `--json`.
    while idx < len(tokens) and tokens[idx].startswith("-"):
        idx += 1
    if idx < len(tokens) and tokens[idx] == subcmd:
        return tokens[idx + 1 :]
    return None


def active_waiter_argvs(procs: list[tuple[list[str], Path | None]]) -> list[list[str]]:
    override = os.environ.get("DELEGATE_STOP_HOOK_WAITERS")
    if override is not None:
        matched: list[list[str]] = []
        for ln in override.splitlines():
            if not ln.strip():
                continue
            args = match_delegate_subcmd(split_cmdline(ln), "wait")
            if args is not None:
                matched.append(args)
        return matched
    out: list[list[str]] = []
    for argv, _ in procs:
        args = match_delegate_subcmd(argv, "wait")
        if args is not None:
            out.append(args)
    return out


def extract_positional_targets(args: list[str], value_flags: set[str]) -> list[str]:
    pos: list[str] = []
    i = 0
    while i < len(args):
        tok = args[i]
        if tok == "--":
            pos.extend(args[i + 1 :])
            break
        if tok in value_flags:
            i += 2
            continue
        if tok.startswith("-"):
            i += 1
            continue
        pos.append(tok)
        i += 1
    return pos


def run_matches_target(rid: str, obj: dict, target: str) -> bool:
    if not target:
        return False
    name = str(obj.get("name") or "")
    chain = str(obj.get("chainName") or "")
    if target in (rid, name, chain):
        return True
    if rid.endswith(f"-{target}") or (len(target) >= 4 and target in rid):
        return True
    return False


def active_lane_state(
    procs: list[tuple[list[str], Path | None]],
) -> tuple[list[Path | None], set[str]]:
    """Return (list_of_gate_cwds, set_of_run_ids_with_running_lane_ticket).
    `None` inside `gate_cwds` means synthetic/unscoped gate (matches current repo).
    Strictly lock-free: reads ticket JSON and checks PID liveness via `kill(pid, 0)`."""
    gate_cwds: list[Path | None] = []
    for argv, pcwd in procs:
        if match_delegate_subcmd(argv, "lane") is not None:
            gate_cwds.append(pcwd)

    lane_runs: set[str] = set()
    sdir = state_dir()
    for sub in ("lane", "lane-remote"):
        ldir = sdir / sub
        if not ldir.is_dir():
            continue
        try:
            for ent in ldir.iterdir():
                if ent.suffix != ".ticket":
                    continue
                doc = read_json_file(ent)
                if not doc or doc.get("running") is not True:
                    continue
                pid = doc.get("pid")
                if isinstance(pid, int) and pid > 0 and not pid_alive(pid):
                    continue
                run_val = str(doc.get("run") or "").strip()
                label = str(doc.get("label") or "").strip()
                if not run_val or label.startswith(("premerge-", "merge-", "apply-")):
                    tcwd: Path | None = None
                    if isinstance(pid, int) and pid > 0:
                        try:
                            tcwd = Path(os.readlink(f"/proc/{pid}/cwd"))
                        except OSError:
                            pass
                    gate_cwds.append(tcwd)
                if run_val:
                    lane_runs.add(run_val)
                    lane_runs.add(Path(run_val).name)
        except OSError:
            pass
    return gate_cwds, lane_runs


def is_premerge_running(obj: dict) -> bool:
    pm = obj.get("premerge")
    if not isinstance(pm, dict):
        return False
    if str(pm.get("status") or "") != "running":
        return False
    wpid = pm.get("workerPid")
    if isinstance(wpid, int) and wpid > 0:
        return pid_alive(wpid)
    return True


def is_run_being_handled(
    rid: str,
    obj: dict,
    repo_root: Path | None,
    applying_procs: list[tuple[list[str], Path | None]],
    merging_procs: list[tuple[list[str], Path | None]],
    active_merge_source_runs: set[str],
    gate_cwds: list[Path | None],
    lane_runs: set[str],
) -> bool:
    """True when `apply`, serial `merge`, background `_merge-worker`, or `lane` gate is in flight."""
    if any(cwd_in_repo(gcwd, repo_root) for gcwd in gate_cwds):
        return True
    if rid in lane_runs or any(rid in lr for lr in lane_runs):
        return True
    if rid in active_merge_source_runs:
        return True
    if is_premerge_running(obj):
        return True
    for args, pcwd in applying_procs:
        targets = extract_positional_targets(args, APPLY_VALUE_FLAGS)
        if not targets:
            if cwd_in_repo(pcwd, repo_root):
                return True
        elif any(
            (t in ("last", "--all") and cwd_in_repo(pcwd, repo_root))
            or run_matches_target(rid, obj, t)
            for t in targets
        ):
            return True
    for args, pcwd in merging_procs:
        if "--ready" in args:
            if cwd_in_repo(pcwd, repo_root):
                return True
            continue
        targets = extract_positional_targets(args, {"--name", "--base"})
        if not targets:
            if cwd_in_repo(pcwd, repo_root):
                return True
        elif any(run_matches_target(rid, obj, t) for t in targets):
            return True
    return False


def is_zero_change_delivery(obj: dict, all_by_id: dict[str, dict]) -> bool:
    """Return True if a delivered run has zero pending file changes across its unapplied chain."""
    if str(obj.get("state") or "") != "delivered":
        return False
    pending = obj.get("pendingChanges")
    if isinstance(pending, dict) and "files" in pending:
        try:
            return int(pending.get("files") or 0) == 0
        except (TypeError, ValueError):
            pass
    changes = obj.get("changes")
    if isinstance(changes, dict) and "files" in changes:
        try:
            if int(changes.get("files") or 0) > 0:
                return False
        except (TypeError, ValueError):
            pass
    # Walk unapplied ancestors in the same worktree when `pendingChanges` is absent.
    wt = str(obj.get("worktree") or "")
    parent = str(obj.get("parent") or "")
    visited: set[str] = set()
    while parent and parent not in visited:
        visited.add(parent)
        anc = all_by_id.get(parent)
        if not anc:
            break
        if anc.get("applied") is True:
            break
        if wt and str(anc.get("worktree") or "") != wt:
            break
        ap = anc.get("pendingChanges")
        if isinstance(ap, dict) and int(ap.get("files") or 0) > 0:
            return False
        ac = anc.get("changes")
        if isinstance(ac, dict) and int(ac.get("files") or 0) > 0:
            return False
        parent = str(anc.get("parent") or "")
    if isinstance(changes, dict) and "files" in changes:
        try:
            return int(changes.get("files") or 0) == 0
        except (TypeError, ValueError):
            return False
    files = obj.get("files")
    if isinstance(files, list):
        return len(files) == 0
    return False


def parse_iso_utc(ts_str: str) -> int | None:
    if not ts_str or not isinstance(ts_str, str):
        return None
    s = ts_str.strip()
    if len(s) == 10 and s[4:5] == "-" and s[7:8] == "-":
        s = f"{s}T00:00:00Z"
    try:
        tm = time.strptime(s, "%Y-%m-%dT%H:%M:%SZ")
        return int(calendar.timegm(tm))
    except ValueError:
        return None


def work_deferred_runs(now: int) -> tuple[set[str], set[str]]:
    """Read `<state>/work.jsonl` and return (fresh_deferred_run_ids, expired_deferred_run_ids)."""
    wfile = state_dir() / "work.jsonl"
    if not wfile.is_file():
        return set(), set()
    fresh: set[str] = set()
    expired: set[str] = set()
    try:
        with wfile.open("r", encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    item = json.loads(line)
                except ValueError:
                    continue
                if not isinstance(item, dict) or item.get("state") == "done":
                    continue
                runs = [r for r in (item.get("runs") or []) if isinstance(r, str) and r]
                if not runs:
                    continue
                st = str(item.get("state") or "")
                until_raw = str(item.get("deferUntil") or "").strip()
                reason_raw = str(item.get("deferReason") or "").strip()
                if st == "deferred":
                    until_ts = parse_iso_utc(until_raw)
                    if until_ts is not None and until_ts > now:
                        fresh.update(runs)
                    else:
                        expired.update(runs)
                elif reason_raw and not until_raw:
                    expired.update(runs)
    except OSError:
        return set(), set()
    return fresh, expired


def run_label(obj: dict, repo_label: str, is_current: bool) -> str:
    name = obj.get("chainName") or obj.get("name") or obj.get("run") or "?"
    name = str(name).replace("\n", " ").replace("\r", " ")
    return name if is_current else f"【{repo_label}】{name}"


def check_unread_events(payload: dict, cwd: str) -> tuple[str | None, list[str], list[str]]:
    """Race lock: block if unread delivery/question/attention events exist past the cursor.
    Returns (reason, display_names, run_ids)."""
    sdir = state_dir()
    events_file = sdir / "events.jsonl"
    if not events_file.is_file():
        return None, [], []

    cursors_doc = read_json_file(sdir / "cursors.json") or {}
    caller_id = (
        os.environ.get("DELEGATE_CALLER")
        or os.environ.get("CLAUDE_CODE_SESSION_ID")
        or os.environ.get("CODEX_THREAD_ID")
        or os.environ.get("PI_SESSION_ID")
        or str(payload.get("session_id") or "")
    ).strip()

    callers_map = cursors_doc.get("callers") if isinstance(cursors_doc.get("callers"), dict) else {}
    if caller_id and isinstance(callers_map.get(caller_id), dict):
        entry = callers_map[caller_id]
    elif isinstance(cursors_doc.get("default"), dict):
        entry = cursors_doc["default"]
    else:
        entry = {}

    try:
        cursor_seq = int(entry.get("seq") or 0)
    except (TypeError, ValueError):
        cursor_seq = 0
    raw_read = entry.get("readRuns") if isinstance(entry.get("readRuns"), dict) else {}
    read_runs: dict[str, int] = {}
    for k, v in raw_read.items():
        if isinstance(k, str) and isinstance(v, int):
            read_runs[k] = v

    current_only = os.environ.get("DELEGATE_STOP_HOOK_CURRENT_ONLY") == "1"
    current_repo_str: str | None = None
    if current_only:
        cur = git_toplevel(cwd)
        if cur is not None:
            try:
                current_repo_str = str(cur.resolve())
            except OSError:
                current_repo_str = str(cur)

    unread: list[dict] = []
    try:
        with events_file.open("r", encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    ev = json.loads(line)
                except ValueError:
                    continue
                if not isinstance(ev, dict):
                    continue
                seq = ev.get("seq")
                if not isinstance(seq, int) or seq <= cursor_seq:
                    continue
                kind = str(ev.get("kind") or "")
                pl = ev.get("payload") if isinstance(ev.get("payload"), dict) else {}
                is_alert = kind in UNREAD_ALERT_KINDS or (
                    kind == "review_verdict" and pl.get("verdict") == "NEEDS_CONTROLLER"
                )
                if not is_alert:
                    continue
                if current_only and current_repo_str is not None:
                    ev_repo = str(ev.get("repo") or "")
                    if ev_repo:
                        try:
                            ev_repo = str(Path(ev_repo).resolve())
                        except OSError:
                            pass
                    if ev_repo != current_repo_str:
                        continue
                run_id = str(ev.get("run") or "")
                if run_id and run_id in read_runs and seq <= read_runs[run_id]:
                    continue
                unread.append(ev)
    except OSError:
        return None, [], []

    if not unread:
        return None, [], []

    previews: list[str] = []
    ev_names: list[str] = []
    ev_run_ids: list[str] = []
    for ev in unread[:5]:
        kind = str(ev.get("kind") or "")
        klabel = KIND_LABELS.get(kind, kind)
        pl = ev.get("payload") if isinstance(ev.get("payload"), dict) else {}
        wid = str(pl.get("workId") or "").strip()
        raw_label = str(pl.get("name") or pl.get("queueName") or ev.get("run") or "?").replace("\n", " ")
        label = f"{wid}:{raw_label}" if wid else raw_label
        ev_names.append(label)
        rid = str(ev.get("run") or "").strip()
        if rid and rid not in ev_run_ids:
            ev_run_ids.append(rid)
        card = pl.get("card") if isinstance(pl.get("card"), dict) else None
        pm = pl.get("premerge") if isinstance(pl.get("premerge"), dict) else None
        if kind == "delivered" and card is not None and isinstance(card.get("riskTotal"), int):
            rt = card["riskTotal"]
            pm_st = str((pm or {}).get("status") or "")
            if pm_st == "ready":
                ff_tag = "快进✓"
            elif pm_st in ("running", "pending"):
                ff_tag = "预合并中"
            elif pm_st == "stale":
                ff_tag = "快进✗"
            elif pm_st == "conflict":
                ff_tag = "预合并冲突"
            elif pm_st == "failed":
                ff_tag = "预合并失败"
            else:
                ff_tag = "快进—"
            previews.append(f"{klabel}:{label}(风险{rt},{ff_tag})")
        else:
            previews.append(f"{klabel}:{label}")
    more = f" 等共 {len(unread)} 条" if len(unread) > 5 else ""
    summary = "、".join(previews) + more
    return (
        f"有新事件，先看 delegate events --since {cursor_seq}（未读 {len(unread)} 条：{summary}）。",
        ev_names,
        ev_run_ids,
    )


def append_stop_logs(records: list[dict]) -> None:
    """Zero-lock O_APPEND log to `<state>/stop-hook.jsonl` for `delegate hook stats`."""
    if not records:
        return
    try:
        sdir = state_dir()
        sdir.mkdir(parents=True, exist_ok=True)
        log_path = sdir / "stop-hook.jsonl"
        ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        fd = os.open(str(log_path), os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
        try:
            for rec in records:
                row = {"ts": ts, **rec}
                line = (json.dumps(row, ensure_ascii=False, separators=(",", ":")) + "\n").encode("utf-8")
                os.write(fd, line)
        finally:
            os.close(fd)
    except OSError:
        pass


def evaluate(payload: dict) -> dict | None:
    if payload.get("stop_hook_active") in (True, "true"):
        return None

    cwd = str(payload.get("cwd") or os.getcwd())
    reasons: list[str] = []
    log_records: list[dict] = []

    unread_reason, ev_names, ev_run_ids = check_unread_events(payload, cwd)
    if unread_reason:
        reasons.append(unread_reason)
        log_records.append(
            {
                "hook": "events",
                "runs": "、".join(ev_names[:8]),
                "runIds": ev_run_ids[:8],
            }
        )

    index_runs = load_index_runs()
    delegate_cmd = find_delegate_bin() or "delegate"
    now = int(time.time())

    repos = discover_repos(cwd, index_runs)
    if not repos:
        if reasons:
            append_stop_logs(log_records)
            return {"decision": "block", "reason": "\n".join(reasons)}
        return None

    procs = list_processes()
    applying_procs = [
        (a, pcwd) for argv, pcwd in procs if (a := match_delegate_subcmd(argv, "apply")) is not None
    ]
    merging_procs = [
        (a, pcwd) for argv, pcwd in procs if (a := match_delegate_subcmd(argv, "merge")) is not None
    ]
    gate_cwds, lane_runs = active_lane_state(procs)

    questions: list[tuple[str, str, bool]] = []  # (display, run_id, is_current)
    live_current: list[tuple[str, str]] = []  # (display, run_id)
    live_other: list[tuple[str, str]] = []  # (display, run_id)
    unmerged_items: list[tuple[str, str]] = []  # (display, run_id)
    stale_run_ids: list[str] = []
    work_fresh_def, work_expired_def = work_deferred_runs(now)

    for repo_root, runs_dir, label, is_current in repos:
        items = fetch_status(runs_dir, index_runs)
        if not items:
            continue

        all_by_id = {obj["run"]: obj for obj in items if isinstance(obj.get("run"), str)}
        parents = {obj["parent"] for obj in items if isinstance(obj.get("parent"), str) and obj["parent"]}
        leaves = [obj for obj in items if obj["run"] not in parents]

        # Collect source runs currently being merged by an active serial merge run.
        active_merge_sources: set[str] = set()
        for obj in items:
            if obj.get("isMerge") is True and str(obj.get("state") or "") in (
                "running",
                "waiting",
                "starting",
                "queued",
            ):
                m = obj.get("merge") if isinstance(obj.get("merge"), dict) else {}
                for src in m.get("sourceRuns") or []:
                    if isinstance(src, str) and src:
                        active_merge_sources.add(src)

        for obj in leaves:
            rid = obj["run"]
            st = str(obj.get("state") or "")
            is_applied = obj.get("applied") is True
            is_cleaned = obj.get("cleaned") is True
            is_question = obj.get("question") is True
            disp = run_label(obj, label, is_current)

            if is_cleaned or is_applied:
                continue

            # 1) Questioning runs (stopped waiting for controller decision).
            if is_question:
                questions.append((disp, rid, is_current))
                continue

            # 2) Live runs (running / waiting / starting / queued).
            if st in ("running", "waiting", "starting", "queued"):
                if is_current:
                    live_current.append((disp, rid))
                else:
                    live_other.append((disp, rid))
                continue

            # 3) Unmerged delivered or rejected write runs.
            if is_zero_change_delivery(obj, all_by_id):
                continue
            next_str = str(obj.get("next") or "")
            is_unmerged_candidate = st == "rejected" or (
                st == "delivered"
                and (str(obj.get("mode") or "") == "write" or bool(UNMERGED_NEXT_RE.search(next_str)))
            )
            if not is_unmerged_candidate:
                continue

            # Skip runs currently being applied, merged, premerged, or gated.
            if is_run_being_handled(
                rid,
                obj,
                repo_root,
                applying_procs,
                merging_procs,
                active_merge_sources,
                gate_cwds,
                lane_runs,
            ):
                continue

            # Check work-table deferrals (`delegate work defer`).
            if rid in work_fresh_def:
                continue
            if rid in work_expired_def:
                stale_run_ids.append(rid)
            unmerged_items.append((disp, rid))

    # Build questioning reminder (always blocks — needs controller reply now).
    if questions:
        names = "、".join(d for d, _, _ in questions[:8])
        cmds = "、".join(f"`delegate reply {rid} '<决定>'`" for _, rid, _ in questions[:3])
        reasons.append(f"有委派正在提问等决定（{names}）：先回答再结束回合（{cmds}）。")
        log_records.append(
            {
                "hook": "question",
                "runs": names,
                "runIds": [rid for _, rid, _ in questions[:8]],
            }
        )

    # Build waiter reminder for live runs.
    if live_current or live_other:
        waiter_argvs = active_waiter_argvs(procs)
        has_any_waiter = bool(waiter_argvs)
        has_machine_waiter = any("--machine" in args for args in waiter_argvs)

        uncovered_cur = [] if has_any_waiter else live_current
        uncovered_other = [
            (disp, rid)
            for disp, rid in live_other
            if not has_machine_waiter and not any(any(rid in tok for tok in args) for args in waiter_argvs)
        ]
        waiter_log_names: list[str] = []
        waiter_log_ids: list[str] = []
        if uncovered_cur:
            names = "、".join(d for d, _ in uncovered_cur[:5])
            reasons.append(
                f"还有委派在跑（{names}），但没有 delegate wait 挂着：在后台挂上 `{delegate_cmd} wait` 再结束回合。"
            )
            waiter_log_names.extend(d for d, _ in uncovered_cur[:5])
            waiter_log_ids.extend(rid for _, rid in uncovered_cur[:5])
        if uncovered_other:
            names = "、".join(d for d, _ in uncovered_other[:5])
            reasons.append(
                f"其他仓库还有委派在跑（{names}），但没有对应 waiter：在后台挂上 `{delegate_cmd} wait --machine` 或切到对应仓库处理。"
            )
            waiter_log_names.extend(d for d, _ in uncovered_other[:5])
            waiter_log_ids.extend(rid for _, rid in uncovered_other[:5])
        if waiter_log_names:
            log_records.append(
                {
                    "hook": "waiter",
                    "runs": "、".join(waiter_log_names[:8]),
                    "runIds": waiter_log_ids[:8],
                }
            )

    # Build unmerged reminder.
    if unmerged_items:
        names = "、".join(d for d, _ in unmerged_items[:8])
        stale_note = (
            "其中暂缓已到期的要重新决定：合、清掉，或重新暂缓。"
            if stale_run_ids
            else ""
        )
        reasons.append(
            f"有已交付的委派还没合也没写明原因（{names}）。{stale_note}"
            "现在就合（delegate apply），或合不了就执行 `delegate work defer <run> --reason \"…\" --until 4h` 再结束回合。"
        )
        log_records.append(
            {
                "hook": "unmerged",
                "runs": names,
                "runIds": [rid for _, rid in unmerged_items[:8]],
                "stale": ",".join(stale_run_ids[:8]),
            }
        )

    if not reasons:
        return None
    append_stop_logs(log_records)
    return {"decision": "block", "reason": "\n".join(reasons[:8])}


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except ValueError:
        return 0
    if not isinstance(payload, dict):
        return 0
    decision = evaluate(payload)
    if decision is not None:
        json.dump(decision, sys.stdout, ensure_ascii=False)
    return 0


if __name__ == "__main__":
    sys.exit(main())
