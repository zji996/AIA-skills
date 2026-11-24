#!/usr/bin/env python3
"""Unified Claude Code Stop hook for delegate (6.39.0).

Replaces legacy `delegate-waiter.sh`, `delegate-unmerged.sh`, and earlier Stop hooks
with a single zero-lock, zero-git turn-end guard scoped to this session's caller or
the current repository:
0. Event-stream race lock: unread alert events past `<state>/cursors.json`.
1. Questioning runs (waiting on controller decision) or live runs without a running
   waiter (checks `<run>/waiter.lock` via `/proc/locks` AND live `delegate wait` /
   `delegate wait --any` / `delegate wait --machine` / `delegate wait --since` processes).
2. Unmerged delivered or rejected write runs without a fresh deferral in
   `<state>/work.jsonl` (`delegate work defer <run> --reason "…" --until <time>`).
   Skips zero-change deliveries and runs currently being applied, merged (serial merger
   or `_merge-worker` premerge), or gated (`delegate lane`).

Reads only `<state>/index.json`, `<state>/work.jsonl`, `<state>/events.jsonl`,
`<state>/cursors.json`, `<state>/lane/*.ticket`, `/proc`, `/proc/locks`, and run-dir JSON
snapshots (zero locks, zero git subprocesses). Appends one JSONL record per triggered hook
class to `<state>/stop-hook.jsonl` for `delegate hook stats`.

When `stop_hook_active` is true, exits 0 immediately to avoid blocking loops.
"""

from __future__ import annotations

import calendar
try:
    import fcntl
except ImportError:  # pragma: no cover
    fcntl = None
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
    "premerge_cache_healed": "预合并缓存自愈",
    "adopted": "接手会话",
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
CALLER_ENV_KEYS = (
    "DELEGATE_CALLER",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
    "PI_SESSION_ID",
)
DEFAULT_STALE_VERIFY_HOURS = 4.0
DEFAULT_STALE_READY_HOURS = 8.0
DEFAULT_REMIND_COOLDOWN_HOURS = 6.0
DEFAULT_ORPHAN_TAKEOVER_HOURS = 6.0
DEFAULT_MAX_ACTIVE = 32
DEFAULT_REPO_MAX_ACTIVE = 0


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


def config_dir() -> Path:
    base = os.environ.get("XDG_CONFIG_HOME")
    return (Path(base) if base else Path.home() / ".config") / "delegate"


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


def overlay_sign_status(rdir: Path, obj: dict) -> None:
    """Lock-free sign status overlay matching `review::sign_status` JSON comparison."""
    sign_doc = read_json_file(rdir / "sign.json")
    if not sign_doc:
        return
    signed_hash = str(sign_doc.get("hash") or "").strip()
    if not signed_hash:
        return
    rev = read_json_file(rdir / "review.json")
    if rev is None and isinstance(obj.get("review"), dict):
        rev = obj["review"]
    cr = rev.get("controllerReview") if isinstance(rev, dict) and isinstance(rev.get("controllerReview"), dict) else {}
    cur_hash = str(cr.get("hash") or "").strip()
    if not cur_hash:
        return
    valid = signed_hash == cur_hash
    obj["sign"] = {
        "required": True,
        "signed": True,
        "valid": valid,
        "invalidated": not valid,
        "hash": cur_hash,
        "signedHash": signed_hash,
    }


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
        "caller": str(meta.get("caller") or "").strip(),
        "applied": is_applied,
        "cleaned": False,
        "question": summary.get("question") is True,
        "worktree": wt_path,
        "dir": str(rdir),
    }
    for mkey in ("resultReadAt", "appliedAt", "repliedAt", "startedAt"):
        mval = str(meta.get(mkey) or "").strip()
        if mval:
            obj[mkey] = mval
    if isinstance(meta.get("startedEpoch"), (int, float)):
        obj["startedEpoch"] = meta["startedEpoch"]
    fin_at = str(summary.get("finishedAt") or "").strip()
    if fin_at:
        obj["finishedAt"] = fin_at
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
    overlay_sign_status(rdir, obj)

    # Synthesize `next` for finished unapplied write runs so downstream classification stays uniform.
    if st == "delivered" and mode == "write" and not is_applied:
        if rev and rev.get("verdict") == "NEEDS_CONTROLLER":
            obj["next"] = f"review {rid}"
        else:
            obj["next"] = f"apply {rid}"
    return obj


def overlay_index_meta(cand: dict, idx_meta: dict | None, rdir: Path | None = None) -> None:
    """Copy auxiliary metadata (`resultReadAt`, `appliedAt`, `repliedAt`, `appliedCleaned`)
    cached in `index.json` onto a run status dict without re-opening disk files."""
    if not isinstance(idx_meta, dict):
        return
    for key in ("resultReadAt", "appliedAt", "repliedAt"):
        val = str(idx_meta.get(key) or "").strip()
        if val and not cand.get(key):
            cand[key] = val
    gen_pending = rdir is not None and (rdir / ".generate-pending").exists()
    if not gen_pending and (idx_meta.get("appliedCleaned") is True or bool(cand.get("appliedAt"))):
        cand["applied"] = True


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
                has_applied_marker = (
                    (rdir / ".applied").exists() or (rdir / ".applied-cleaned").exists()
                ) and not (rdir / ".generate-pending").exists()
                if has_applied_marker and cand.get("applied") is not True:
                    stale = True
            if not stale:
                cand.setdefault("dir", str(rdir))
                overlay_index_meta(cand, entry.get("meta"), rdir)
                # Overlay fresh premerge.json and sign.json directly from disk (lock-free).
                pm = read_json_file(rdir / "premerge.json")
                if pm is not None:
                    cand["premerge"] = pm
                overlay_sign_status(rdir, cand)
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


def current_caller_id(payload: dict) -> str:
    return (
        os.environ.get("DELEGATE_CALLER")
        or os.environ.get("CLAUDE_CODE_SESSION_ID")
        or os.environ.get("CODEX_THREAD_ID")
        or os.environ.get("PI_SESSION_ID")
        or str(payload.get("session_id") or "")
    ).strip()


def get_run_caller(obj: dict) -> str:
    raw = obj.get("caller")
    if isinstance(raw, str):
        val = raw.strip()
        if val:
            return val
    rdir = obj.get("dir")
    if isinstance(rdir, str) and rdir:
        meta = read_json_file(Path(rdir) / "meta.json")
        if meta and isinstance(meta.get("caller"), str):
            val = meta["caller"].strip()
            obj["caller"] = val
            return val
    return ""


def run_belongs_to_scope(obj: dict, is_current: bool, caller_id: str) -> bool:
    """Only guard runs in the current repo OR launched by this session's caller."""
    if is_current:
        return True
    run_caller = get_run_caller(obj)
    if caller_id:
        return run_caller == caller_id
    # When no session caller id is present (e.g. standalone CLI / legacy tests),
    # only include other-repo runs that also do not belong to a specific session.
    return not run_caller


def read_proc_stat_state_ppid_ticks(pid: int) -> tuple[str, int, int] | None:
    """Return `(state_char, ppid, starttime_ticks)` from `/proc/<pid>/stat` (Linux)."""
    info = read_proc_stat_full(pid)
    if info is None:
        return None
    return info[0], info[1], info[2]


def read_proc_stat_full(pid: int) -> tuple[str, int, int, int] | None:
    """Return `(state_char, ppid, starttime_ticks, tty_nr)` from `/proc/<pid>/stat` (Linux)."""
    try:
        txt = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8", errors="ignore")
    except OSError:
        return None
    rparen = txt.rfind(")")
    if rparen == -1:
        return None
    fields = txt[rparen + 2 :].split()
    if len(fields) < 20:
        return None
    try:
        # fields[0]=state, fields[1]=ppid, fields[4]=tty_nr (field 7), fields[19]=starttime (field 22).
        return fields[0], int(fields[1]), int(fields[19]), int(fields[4])
    except ValueError:
        return None


def _norm_path_str(raw: str) -> str:
    raw = raw.strip()
    if not raw:
        return ""
    try:
        return str(Path(raw).resolve())
    except OSError:
        return os.path.normpath(raw)


def current_terminal_id(harness_tty_nr: int = 0) -> str:
    """Identify the enclosing terminal (tmux pane / terminal session / controlling tty_nr)."""
    for env_key, prefix in (
        ("TMUX_PANE", "tmux"),
        ("TERM_SESSION_ID", "term"),
        ("WEZTERM_PANE", "wezterm"),
        ("KITTY_WINDOW_ID", "kitty"),
        ("WINDOWID", "win"),
    ):
        val = os.environ.get(env_key, "").strip()
        if val:
            return f"{prefix}:{val}"
    if harness_tty_nr != 0:
        return f"tty:{harness_tty_nr}"
    self_info = read_proc_stat_full(os.getpid())
    if self_info is not None and self_info[3] != 0:
        return f"tty:{self_info[3]}"
    return ""


def pid_alive_with_ticks(pid: int, expected_ticks: int | None = None) -> bool:
    """Check PID liveness, excluding zombies and guarding against PID reuse via startTicks."""
    if not pid_alive(pid):
        return False
    stat_info = read_proc_stat_state_ppid_ticks(pid)
    if stat_info is not None:
        state_ch, _ppid, actual_ticks = stat_info
        if state_ch == "Z":
            return False
        if isinstance(expected_ticks, int) and expected_ticks > 0 and actual_ticks != expected_ticks:
            return False
    return True


def _is_harness_cmdline(pid: int) -> bool:
    try:
        raw = Path(f"/proc/{pid}/cmdline").read_bytes()
    except OSError:
        return False
    if not raw:
        return False
    tokens = [t.decode("utf-8", errors="replace") for t in raw.split(b"\x00") if t]
    if not tokens:
        return False
    base = os.path.basename(tokens[0])
    if base in ("claude", "codex", "pi", "rpi"):
        return True
    if base in ("node", "bun", "deno"):
        for arg in tokens[1:4]:
            low = arg.lower()
            if (
                "claude" in low
                or "codex" in low
                or low.endswith("/pi")
                or "pi-coding-agent" in low
            ):
                return True
    return False


def find_session_harness_pid() -> tuple[int, int, int] | None:
    """Walk ancestor processes up to 8 hops to locate an enclosing Claude/Codex/Pi harness.
    Returns `(pid, start_ticks, tty_nr)`."""
    pid = os.getppid()
    for _ in range(8):
        if pid <= 1:
            break
        stat_info = read_proc_stat_full(pid)
        if stat_info is None:
            break
        _state, ppid, ticks, tty_nr = stat_info
        if _is_harness_cmdline(pid):
            return pid, ticks, tty_nr
        if ppid == pid:
            break
        pid = ppid
    return None


def touch_caller_session(
    sdir: Path,
    caller_id: str,
    now: int,
    cwd: str = "",
) -> tuple[tuple[int, int] | None, str, str]:
    """Lock-free atomic update of `<state>/sessions.json` for the current caller.
    Returns `(cur_harness_pid_ticks, cur_tty, cur_cwd)`."""
    harness = find_session_harness_pid()
    harness_pt: tuple[int, int] | None = (harness[0], harness[1]) if harness is not None else None
    cur_tty = current_terminal_id(harness[2] if harness is not None else 0)
    cur_cwd = _norm_path_str(cwd) if cwd else ""
    if not caller_id:
        return harness_pt, cur_tty, cur_cwd
    try:
        sdir.mkdir(parents=True, exist_ok=True)
        path = sdir / "sessions.json"
        doc = read_json_file(path)
        sessions = dict(doc) if isinstance(doc, dict) else {}
        prev = sessions.get(caller_id) if isinstance(sessions.get(caller_id), dict) else {}
        rec: dict = {
            "lastSeenAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now)),
        }
        if harness is not None:
            rec["pid"] = harness[0]
            rec["startTicks"] = harness[1]
        elif isinstance(prev.get("pid"), int) and prev["pid"] > 0:
            prev_ticks = prev.get("startTicks") if isinstance(prev.get("startTicks"), int) else None
            if pid_alive_with_ticks(prev["pid"], prev_ticks):
                rec["pid"] = prev["pid"]
                if prev_ticks is not None:
                    rec["startTicks"] = prev_ticks
                if harness_pt is None and prev_ticks is not None:
                    harness_pt = (prev["pid"], prev_ticks)
        if cur_tty:
            rec["tty"] = cur_tty
        elif isinstance(prev.get("tty"), str) and prev["tty"].strip():
            rec["tty"] = prev["tty"].strip()
            cur_tty = rec["tty"]
        if cur_cwd:
            rec["cwd"] = cur_cwd
        elif isinstance(prev.get("cwd"), str) and prev["cwd"].strip():
            rec["cwd"] = _norm_path_str(prev["cwd"])
            cur_cwd = rec["cwd"]
        sessions[caller_id] = rec
        cutoff = now - 14 * 86400
        pruned: dict = {}
        for k, v in sessions.items():
            if not isinstance(k, str) or not isinstance(v, dict):
                continue
            if k == caller_id:
                pruned[k] = v
                continue
            ts = parse_iso_utc(str(v.get("lastSeenAt") or ""))
            if ts is None or ts >= cutoff:
                pruned[k] = v
        tmp = sdir / f"sessions.json.{os.getpid()}.tmp"
        tmp.write_text(json.dumps(pruned, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        os.replace(tmp, path)
    except OSError:
        pass
    return harness_pt, cur_tty, cur_cwd


def find_alive_callers_in_proc(target_callers: set[str]) -> set[str]:
    """Check whether any live user process carries one of `target_callers` in its
    caller environment variables or `--session-id`/`--resume` argv."""
    if not target_callers:
        return set()
    found: set[str] = set()
    proc = Path("/proc")
    if not proc.is_dir():
        return found
    my_pid = os.getpid()
    my_uid = os.getuid() if hasattr(os, "getuid") else None
    env_prefixes = tuple(f"{k}=".encode("ascii") for k in CALLER_ENV_KEYS)
    target_bytes = {c.encode("utf-8", errors="ignore"): c for c in target_callers if c}
    if not target_bytes:
        return found
    try:
        for entry in proc.iterdir():
            name = entry.name
            if not name.isdigit():
                continue
            pid = int(name)
            if pid == my_pid:
                continue
            try:
                if my_uid is not None and entry.stat().st_uid != my_uid:
                    continue
                stat_txt = (entry / "stat").read_text(encoding="utf-8", errors="ignore")
                rparen = stat_txt.rfind(")")
                if rparen != -1 and rparen + 2 < len(stat_txt) and stat_txt[rparen + 2] == "Z":
                    continue
            except OSError:
                continue
            # Check `/proc/<pid>/environ` for exact `KEY=<caller>` match.
            try:
                env_raw = (entry / "environ").read_bytes()
                if env_raw:
                    for item in env_raw.split(b"\x00"):
                        for pfx in env_prefixes:
                            if item.startswith(pfx):
                                val = item[len(pfx) :].strip()
                                matched = target_bytes.get(val)
                                if matched:
                                    found.add(matched)
                                    if len(found) == len(target_callers):
                                        return found
            except OSError:
                pass
            # Check `/proc/<pid>/cmdline` for `--session-id <id>` / `--resume <id>`.
            try:
                cmd_raw = (entry / "cmdline").read_bytes()
                if cmd_raw:
                    toks = [t for t in cmd_raw.split(b"\x00") if t]
                    for i, tok in enumerate(toks):
                        if tok in (b"--session-id", b"--resume") and i + 1 < len(toks):
                            matched = target_bytes.get(toks[i + 1].strip())
                            if matched:
                                found.add(matched)
                                if len(found) == len(target_callers):
                                    return found
            except OSError:
                pass
    except OSError:
        pass
    return found


def get_run_unclaimed_ts(obj: dict) -> int | None:
    """Return epoch timestamp when a run became unclaimed (finishedAt for terminal runs,
    startedAt/startedEpoch for active runs)."""
    st = str(obj.get("state") or "")
    is_active = st in ("running", "starting", "waiting", "queued")
    if not is_active:
        fin = parse_iso_utc(str(obj.get("finishedAt") or ""))
        if fin is not None:
            return fin
        rdir = obj.get("dir")
        if isinstance(rdir, str) and rdir:
            summary = read_json_file(Path(rdir) / "summary.json") or {}
            fin = parse_iso_utc(str(summary.get("finishedAt") or ""))
            if fin is not None:
                obj["finishedAt"] = str(summary.get("finishedAt") or "").strip()
                return fin
    st_at = parse_iso_utc(str(obj.get("startedAt") or ""))
    if st_at is not None:
        return st_at
    ep = obj.get("startedEpoch")
    if isinstance(ep, (int, float)) and ep > 0:
        return int(ep)
    rdir = obj.get("dir")
    if isinstance(rdir, str) and rdir:
        meta = read_json_file(Path(rdir) / "meta.json") or {}
        st_at = parse_iso_utc(str(meta.get("startedAt") or ""))
        if st_at is not None:
            obj["startedAt"] = str(meta.get("startedAt") or "").strip()
            return st_at
        ep = meta.get("startedEpoch")
        if isinstance(ep, (int, float)) and ep > 0:
            obj["startedEpoch"] = ep
            return int(ep)
    return None


class CallerLivenessChecker:
    """Determines whether another caller session is still alive, whether it has expired past
    `DELEGATE_ORPHAN_TAKEOVER_HOURS` (default 6h) for takeover, or whether it is dead in the
    same terminal/working directory so this session should be prompted to `delegate adopt`."""

    def __init__(
        self,
        sdir: Path,
        cursors_doc: dict,
        now: int,
        orphan_secs: float,
        cur_caller: str = "",
        cur_harness: tuple[int, int] | None = None,
        cur_tty: str = "",
        cur_cwd: str = "",
    ) -> None:
        self.sdir = sdir
        self.cursors_doc = cursors_doc
        self.now = now
        self.orphan_secs = orphan_secs
        self.cur_caller = cur_caller
        self.cur_harness = cur_harness
        self.cur_tty = cur_tty
        self.cur_cwd = _norm_path_str(cur_cwd) if cur_cwd else ""
        raw_sessions = read_json_file(sdir / "sessions.json")
        self.sessions: dict = raw_sessions if isinstance(raw_sessions, dict) else {}
        self._alive_cache: dict[str, bool] = {}
        self._proc_checked: set[str] = set()
        self._proc_alive: set[str] = set()

    def _check_proc(self, caller: str) -> bool:
        if caller not in self._proc_checked:
            found = find_alive_callers_in_proc({caller})
            self._proc_checked.add(caller)
            self._proc_alive.update(found)
        return caller in self._proc_alive

    def _is_superseded_same_harness(self, orig_caller: str, rec: dict) -> bool:
        """True if another session (e.g. after `/clear`) now occupies the exact same
        enclosing harness `(pid, startTicks)`."""
        pid = rec.get("pid")
        if not isinstance(pid, int) or pid <= 0:
            return False
        ticks = rec.get("startTicks") if isinstance(rec.get("startTicks"), int) else None
        if self.cur_harness is not None and self.cur_caller and self.cur_caller != orig_caller:
            if self.cur_harness[0] == pid and (ticks is None or self.cur_harness[1] == ticks):
                return True
        orig_ts = parse_iso_utc(str(rec.get("lastSeenAt") or ""))
        for other_id, other_rec in self.sessions.items():
            if other_id == orig_caller or not isinstance(other_rec, dict):
                continue
            if other_rec.get("pid") != pid:
                continue
            other_ticks = (
                other_rec.get("startTicks")
                if isinstance(other_rec.get("startTicks"), int)
                else None
            )
            if ticks is not None and other_ticks is not None and other_ticks != ticks:
                continue
            if other_id == self.cur_caller:
                return True
            other_ts = parse_iso_utc(str(other_rec.get("lastSeenAt") or ""))
            if other_ts is not None and (orig_ts is None or other_ts > orig_ts):
                return True
        return False

    def is_alive(self, orig_caller: str) -> bool:
        if not orig_caller:
            return False
        if orig_caller in self._alive_cache:
            return self._alive_cache[orig_caller]

        alive = False
        rec = self.sessions.get(orig_caller)
        has_recorded_pid = False
        superseded = False
        if isinstance(rec, dict) and isinstance(rec.get("pid"), int) and rec["pid"] > 0:
            has_recorded_pid = True
            if self._is_superseded_same_harness(orig_caller, rec):
                superseded = True
            else:
                ticks = rec.get("startTicks") if isinstance(rec.get("startTicks"), int) else None
                if pid_alive_with_ticks(rec["pid"], ticks):
                    alive = True

        if not alive and not superseded and self._check_proc(orig_caller):
            alive = True

        # When no explicit session PID was recorded, fall back to recent activity timestamps
        # (`sessions.json` lastSeenAt or `cursors.json` updatedAt within orphan_secs).
        if not alive and not has_recorded_pid:
            last_ts: int | None = None
            if isinstance(rec, dict):
                ts = parse_iso_utc(str(rec.get("lastSeenAt") or ""))
                if ts is not None:
                    last_ts = ts
            callers_map = self.cursors_doc.get("callers")
            if isinstance(callers_map, dict):
                centry = callers_map.get(orig_caller)
                if isinstance(centry, dict):
                    cts = parse_iso_utc(str(centry.get("updatedAt") or ""))
                    if cts is not None and (last_ts is None or cts > last_ts):
                        last_ts = cts
            if last_ts is not None and (self.now - last_ts) < self.orphan_secs:
                alive = True

        self._alive_cache[orig_caller] = alive
        return alive

    def same_terminal(self, orig_caller: str) -> bool:
        rec = self.sessions.get(orig_caller)
        if not isinstance(rec, dict):
            return False
        pid = rec.get("pid")
        if self.cur_harness is not None and isinstance(pid, int) and pid > 0:
            ticks = rec.get("startTicks") if isinstance(rec.get("startTicks"), int) else None
            if self.cur_harness[0] == pid and (ticks is None or self.cur_harness[1] == ticks):
                return True
        if self.cur_tty:
            orig_tty = str(rec.get("tty") or "").strip()
            if orig_tty and orig_tty == self.cur_tty:
                return True
        return False

    def same_cwd(self, orig_caller: str) -> bool:
        if not self.cur_cwd:
            return False
        rec = self.sessions.get(orig_caller)
        if not isinstance(rec, dict):
            return False
        orig_cwd = _norm_path_str(str(rec.get("cwd") or ""))
        return bool(orig_cwd) and orig_cwd == self.cur_cwd

    def same_terminal_or_cwd(self, orig_caller: str) -> bool:
        return self.same_terminal(orig_caller) or self.same_cwd(orig_caller)

    def is_adoptable(self, orig_caller: str) -> bool:
        """True if `orig_caller` is a different, now-dead/superseded session in the same
        terminal or same working directory (e.g. right after `/clear`)."""
        if not self.cur_caller or not orig_caller or orig_caller == self.cur_caller:
            return False
        if self.is_alive(orig_caller):
            return False
        return self.same_terminal_or_cwd(orig_caller)

    def can_takeover(self, orig_caller: str, unclaimed_since_ts: int | None) -> bool:
        """True only if `orig_caller` is empty (ownerless), OR `orig_caller` is NOT alive
        AND the item has remained unclaimed for at least `orphan_secs`."""
        if not orig_caller:
            return True
        if self.is_alive(orig_caller):
            return False
        if unclaimed_since_ts is None:
            return True
        return (self.now - unclaimed_since_ts) >= self.orphan_secs


def read_locked_inodes() -> set[tuple[int, int, int]] | None:
    """Strictly read-only parse of `/proc/locks` returning held `(major, minor, inode)` tuples."""
    proc_locks = Path("/proc/locks")
    if not proc_locks.is_file():
        return None
    inodes: set[tuple[int, int, int]] = set()
    try:
        for line in proc_locks.read_text(encoding="utf-8", errors="ignore").splitlines():
            fields = line.split()
            if len(fields) < 6:
                continue
            # Skip blocked waiter entries (`1: -> FLOCK ...`).
            if fields[1] == "->":
                continue
            dev_ino = fields[5].split(":")
            if len(dev_ino) != 3:
                continue
            try:
                maj = int(dev_ino[0], 16)
                minor = int(dev_ino[1], 16)
                ino = int(dev_ino[2], 10)
            except ValueError:
                continue
            inodes.add((maj, minor, ino))
    except OSError:
        return None
    return inodes


def has_active_waiter_lock(
    run_dir: Path | None,
    locked_inodes: set[tuple[int, int, int]] | None,
) -> bool:
    """Zero-lock check whether `<run>/waiter.lock` is actively held by a live waiter."""
    if run_dir is None:
        return False
    lock_path = run_dir / "waiter.lock"
    if not lock_path.is_file():
        return False
    try:
        st = lock_path.stat()
    except OSError:
        return False
    if locked_inodes is not None:
        return (os.major(st.st_dev), os.minor(st.st_dev), st.st_ino) in locked_inodes
    # Non-Linux fallback: check recorded PID + non-blocking shared lock probe.
    try:
        pid = int(lock_path.read_text(encoding="utf-8").strip())
    except (OSError, ValueError):
        pid = 0
    if pid > 0 and not pid_alive(pid):
        return False
    if fcntl is not None:
        try:
            with lock_path.open("rb") as fh:
                try:
                    fcntl.flock(fh.fileno(), fcntl.LOCK_SH | fcntl.LOCK_NB)
                    fcntl.flock(fh.fileno(), fcntl.LOCK_UN)
                    return False
                except BlockingIOError:
                    return True
        except OSError:
            pass
    return pid > 0 and pid_alive(pid)


def active_waiters(
    procs: list[tuple[list[str], Path | None]],
) -> list[tuple[list[str], Path | None]]:
    override = os.environ.get("DELEGATE_STOP_HOOK_WAITERS")
    if override is not None:
        matched: list[tuple[list[str], Path | None]] = []
        for ln in override.splitlines():
            if not ln.strip():
                continue
            args = match_delegate_subcmd(split_cmdline(ln), "wait")
            if args is not None:
                matched.append((args, None))
        return matched
    out: list[tuple[list[str], Path | None]] = []
    for argv, pcwd in procs:
        args = match_delegate_subcmd(argv, "wait")
        if args is not None:
            out.append((args, pcwd))
    return out


def waiter_covers_run(
    run_dir: Path | None,
    rid: str,
    obj: dict,
    repo_root: Path | None,
    is_current: bool,
    waiters: list[tuple[list[str], Path | None]],
    locked_inodes: set[tuple[int, int, int]] | None,
) -> bool:
    """Return True if a run is already covered by `<run>/waiter.lock` or a live `delegate wait`."""
    if has_active_waiter_lock(run_dir, locked_inodes):
        return True
    for args, pcwd in waiters:
        if "--machine" in args:
            return True
        targets = extract_positional_targets(args, {"--max", "--since"})
        if "--since" in args and not targets:
            # Unfiltered `delegate wait --since SEQ` watches the global event stream.
            return True
        if not targets:
            # Bare `delegate wait`, `delegate wait --any`, `delegate wait --all`, etc.
            if pcwd is not None:
                if cwd_in_repo(pcwd, repo_root):
                    return True
            elif is_current:
                return True
        elif any(run_matches_target(rid, obj, t) for t in targets):
            return True
    return False


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


def load_work_items() -> list[dict]:
    """Lock-free read of `<state>/work.jsonl`."""
    wfile = state_dir() / "work.jsonl"
    if not wfile.is_file():
        return []
    items: list[dict] = []
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
                if isinstance(item, dict):
                    items.append(item)
    except OSError:
        return []
    return items


def work_deferred_runs(work_items: list[dict], now: int) -> tuple[set[str], set[str]]:
    """Return (fresh_deferred_run_ids, expired_deferred_run_ids) from parsed work items."""
    fresh: set[str] = set()
    expired: set[str] = set()
    for item in work_items:
        if item.get("state") == "done":
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
    return fresh, expired


def load_queue_items_by_name() -> dict[str, list[dict]] | None:
    """Lock-free read of `<state>/dag-queue.json` grouped by queue item name."""
    doc = read_json_file(state_dir() / "dag-queue.json")
    if doc is None:
        return None
    raw_items = doc.get("items")
    if not isinstance(raw_items, list):
        return None
    by_name: dict[str, list[dict]] = {}
    for item in raw_items:
        if not isinstance(item, dict):
            continue
        name = str(item.get("name") or "").strip()
        if name:
            by_name.setdefault(name, []).append(item)
    return by_name


def inspect_run_resolution(
    rid: str,
    seq: int,
    run_obj: dict | None,
    replied_parents: set[str],
    resolving: dict[str, dict[str, int]],
) -> tuple[bool, bool, bool, bool]:
    """Return `(is_applied, is_cleaned, is_replied, has_result_read)` for a run-scoped event."""
    is_applied = resolving["applied"].get(rid, 0) > seq or (
        run_obj is not None
        and (run_obj.get("applied") is True or bool(str(run_obj.get("appliedAt") or "").strip()))
    )
    is_cleaned = resolving["cleaned"].get(rid, 0) > seq or (
        run_obj is not None and run_obj.get("cleaned") is True
    )
    is_replied = (
        rid in replied_parents
        or resolving["replied"].get(rid, 0) > seq
        or (run_obj is not None and bool(str(run_obj.get("repliedAt") or "").strip()))
    )
    has_result_read = (
        run_obj is not None and bool(str(run_obj.get("resultReadAt") or "").strip())
    )

    if run_obj is None:
        if not is_cleaned and (cache_dir() / "archive" / rid / "record").is_dir():
            is_cleaned = True
        return is_applied, is_cleaned, is_replied, has_result_read

    rdir_str = run_obj.get("dir")
    if isinstance(rdir_str, str) and rdir_str and not (
        is_applied and is_replied and has_result_read
    ):
        rdir = Path(rdir_str)
        if not is_applied and (
            (rdir / ".applied").exists() or (rdir / ".applied-cleaned").exists()
        ) and not (rdir / ".generate-pending").exists():
            is_applied = True
            run_obj["applied"] = True
        if not (is_applied and is_replied and has_result_read) and not run_obj.get("_metaChecked"):
            meta = read_json_file(rdir / "meta.json")
            run_obj["_metaChecked"] = True
            if isinstance(meta, dict):
                rra = str(meta.get("resultReadAt") or "").strip()
                if rra:
                    run_obj["resultReadAt"] = rra
                    has_result_read = True
                apa = str(meta.get("appliedAt") or "").strip()
                if apa:
                    run_obj["appliedAt"] = apa
                    if not (rdir / ".generate-pending").exists():
                        run_obj["applied"] = True
                        is_applied = True
                rpa = str(meta.get("repliedAt") or "").strip()
                if rpa:
                    run_obj["repliedAt"] = rpa
                    is_replied = True
    return is_applied, is_cleaned, is_replied, has_result_read


def is_alert_event_stale(
    ev: dict,
    seq: int,
    now: int,
    work_by_id: dict[str, dict],
    all_runs_by_id: dict[str, dict],
    replied_parents: set[str],
    resolving: dict[str, dict[str, int]],
    queue_items_by_name: dict[str, list[dict]] | None,
) -> bool:
    """Return True when an unread alert event has already been resolved or superseded."""
    kind = str(ev.get("kind") or "")
    pl = ev.get("payload") if isinstance(ev.get("payload"), dict) else {}

    # 1) Work-item state events (`work_ready`, `work_defer_expired`, `work_stale`).
    if kind in ("work_ready", "work_defer_expired", "work_stale"):
        wid = str(pl.get("workId") or "").strip()
        w = work_by_id.get(wid) if wid else None
        if w is None:
            return False
        wstate = str(w.get("state") or "")
        if kind == "work_ready":
            # Stale once closed/dropped (`done`) or no longer `ready` (e.g. `verify`, `active`, `deferred`).
            return wstate != "ready"
        if wstate == "done":
            return True
        if wstate == "deferred":
            until_ts = parse_iso_utc(str(w.get("deferUntil") or ""))
            if until_ts is not None and until_ts > now:
                return True
        if kind == "work_defer_expired":
            return wstate == "active"
        if kind == "work_stale":
            ev_state = str(pl.get("state") or "").strip()
            if ev_state and wstate != ev_state:
                return True
            return False

    # 2) DAG queue paused events (`queue_paused`).
    if kind == "queue_paused":
        qname = str(pl.get("queueName") or pl.get("name") or "").strip()
        if qname and queue_items_by_name is not None and qname in queue_items_by_name:
            entries = queue_items_by_name[qname]
            if entries and not any(str(e.get("status") or "") == "paused" for e in entries):
                return True
        return False

    # 3) Run-scoped alert events.
    rid = str(ev.get("run") or "").strip()
    if not rid:
        return False

    run_obj = all_runs_by_id.get(rid)
    is_applied, is_cleaned, is_replied, has_result_read = inspect_run_resolution(
        rid,
        seq,
        run_obj,
        replied_parents,
        resolving,
    )

    if is_applied or is_cleaned or is_replied:
        return True

    # Delivery / question / terminal-outcome events already read by any session (`resultReadAt`).
    if has_result_read and (
        kind in ("delivered", "question", "rejected", "failed")
        or (kind == "needs_attention" and pl.get("policyRefused") is True)
    ):
        return True

    if kind == "stall_warning":
        if resolving["stopped"].get(rid, 0) > seq or resolving["terminal"].get(rid, 0) > seq:
            return True
        if run_obj is not None:
            rst = str(run_obj.get("state") or "")
            if rst and rst not in ("running", "starting", "waiting", "queued"):
                return True
        return False

    if kind in ("premerge_conflict", "premerge_failed"):
        if resolving["premerge_ready"].get(rid, 0) > seq:
            return True
        if run_obj is not None:
            pm = run_obj.get("premerge")
            if isinstance(pm, dict) and str(pm.get("status") or "") == "ready":
                return True
        return False

    if kind == "needs_attention" or (
        kind == "review_verdict" and pl.get("verdict") == "NEEDS_CONTROLLER"
    ):
        if resolving["review_pass"].get(rid, 0) > seq:
            return True
        if run_obj is not None:
            sign = run_obj.get("sign")
            if isinstance(sign, dict) and sign.get("valid") is True:
                return True
        return False

    return False


def run_label(obj: dict, repo_label: str, is_current: bool) -> str:
    name = obj.get("chainName") or obj.get("name") or obj.get("run") or "?"
    name = str(name).replace("\n", " ").replace("\r", " ")
    return name if is_current else f"【{repo_label}】{name}"


def _format_event_preview(ev: dict, is_orphan: bool) -> tuple[str, str, str]:
    kind = str(ev.get("kind") or "")
    klabel = KIND_LABELS.get(kind, kind)
    pl = ev.get("payload") if isinstance(ev.get("payload"), dict) else {}
    wid = str(pl.get("workId") or "").strip()
    raw_label = str(pl.get("name") or pl.get("queueName") or ev.get("run") or "?").replace("\n", " ")
    label = f"{wid}:{raw_label}" if wid else raw_label
    rid = str(ev.get("run") or "").strip()
    prefix = "[接盘]" if is_orphan else ""
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
        preview = f"{prefix}{klabel}:{label}(风险{rt},{ff_tag})"
    else:
        preview = f"{prefix}{klabel}:{label}"
    return preview, label, rid


def check_unread_events(
    payload: dict,
    cwd: str,
    now: int,
    work_by_id: dict[str, dict],
    all_runs_by_id: dict[str, dict],
    replied_parents: set[str],
    cursors_doc: dict,
    liveness: CallerLivenessChecker,
) -> tuple[str | None, list[str], list[str], int, list[tuple[str, str]]]:
    """Race lock: block if unread delivery/question/attention events exist past the cursor.
    Non-initiator runs only block when the original caller has expired past the orphan timeout;
    runs whose caller is dead in the same terminal/cwd are returned for `delegate adopt` hints.
    Returns (reason, display_names, run_ids, other_caller_count, adoptable_runs)."""
    sdir = state_dir()
    events_file = sdir / "events.jsonl"
    if not events_file.is_file():
        return None, [], [], 0, []

    caller_id = current_caller_id(payload)

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
    cur = git_toplevel(cwd)
    if cur is not None:
        try:
            current_repo_str = str(cur.resolve())
        except OSError:
            current_repo_str = str(cur)

    candidates: list[tuple[int, dict, bool]] = []
    resolving: dict[str, dict[str, int]] = {
        "applied": {},
        "cleaned": {},
        "stopped": {},
        "terminal": {},
        "premerge_ready": {},
        "review_pass": {},
        "replied": {},
    }

    def bump(bucket: str, key: str, seq_val: int) -> None:
        if key and seq_val > resolving[bucket].get(key, 0):
            resolving[bucket][key] = seq_val

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
                run_id = str(ev.get("run") or "").strip()

                # Record resolving events so superseded/resolved alerts are suppressed.
                if kind == "applied":
                    bump("applied", run_id, seq)
                elif kind == "cleaned":
                    bump("cleaned", run_id, seq)
                elif kind == "stopped":
                    bump("stopped", run_id, seq)
                elif kind in ("delivered", "rejected", "failed", "question"):
                    bump("terminal", run_id, seq)
                elif kind == "premerge_ready":
                    bump("premerge_ready", run_id, seq)
                elif kind == "review_verdict" and pl.get("verdict") == "PASS":
                    bump("review_pass", run_id, seq)
                elif kind == "started":
                    parent_id = str(pl.get("parent") or "").strip()
                    if parent_id:
                        bump("replied", parent_id, seq)

                is_alert = kind in UNREAD_ALERT_KINDS or (
                    kind == "review_verdict" and pl.get("verdict") == "NEEDS_CONTROLLER"
                )
                if not is_alert:
                    continue
                ev_repo = str(ev.get("repo") or "")
                if ev_repo:
                    try:
                        ev_repo = str(Path(ev_repo).resolve())
                    except OSError:
                        pass
                ev_is_current = current_repo_str is not None and ev_repo == current_repo_str
                if current_only and current_repo_str is not None and not ev_is_current:
                    continue
                if run_id and not ev_is_current:
                    run_obj = all_runs_by_id.get(run_id)
                    if run_obj is not None and not run_belongs_to_scope(run_obj, False, caller_id):
                        rc = get_run_caller(run_obj)
                        if not (liveness.is_adoptable(rc) and liveness.same_terminal(rc)):
                            continue
                if run_id and run_id in read_runs and seq <= read_runs[run_id]:
                    continue
                candidates.append((seq, ev, ev_is_current))
    except OSError:
        return None, [], [], 0, []

    if not candidates:
        return None, [], [], 0, []

    queue_items_by_name = (
        load_queue_items_by_name()
        if any(str(ev.get("kind") or "") == "queue_paused" for _, ev, _ in candidates)
        else None
    )

    own_unread: list[dict] = []
    orphan_unread: list[dict] = []
    adoptable_runs: list[tuple[str, str]] = []
    adoptable_seen: set[str] = set()
    other_caller_count = 0

    for seq, ev, ev_is_current in candidates:
        if is_alert_event_stale(
            ev,
            seq,
            now,
            work_by_id,
            all_runs_by_id,
            replied_parents,
            resolving,
            queue_items_by_name,
        ):
            continue

        kind = str(ev.get("kind") or "")
        pl = ev.get("payload") if isinstance(ev.get("payload"), dict) else {}
        rid = str(ev.get("run") or "").strip()

        if kind == "work_stale":
            wid = str(pl.get("workId") or "").strip()
            w = work_by_id.get(wid) if wid else None
            w_caller = str(w.get("caller") or "").strip() if isinstance(w, dict) else ""
            if w_caller and w_caller != caller_id:
                since_ts = parse_iso_utc(
                    str((w or {}).get("stateSince") or ev.get("at") or "")
                )
                if liveness.can_takeover(w_caller, since_ts):
                    orphan_unread.append(ev)
                else:
                    other_caller_count += 1
                continue
            own_unread.append(ev)
            continue

        if rid and kind not in ("work_ready", "work_defer_expired"):
            run_obj = all_runs_by_id.get(rid)
            run_caller = get_run_caller(run_obj) if run_obj is not None else ""
            if run_caller and run_caller != caller_id:
                adoptable = liveness.is_adoptable(run_caller)
                if not ev_is_current and not (adoptable and liveness.same_terminal(run_caller)):
                    continue
                if adoptable and rid not in adoptable_seen:
                    adoptable_seen.add(rid)
                    disp_name = strip_reply_prefixes(
                        str(pl.get("name") or (run_obj or {}).get("name") or rid)
                    )
                    adoptable_runs.append((disp_name, rid))
                ev_ts = parse_iso_utc(str(ev.get("at") or ""))
                if ev_ts is None and run_obj is not None:
                    ev_ts = get_run_unclaimed_ts(run_obj)
                if ev_is_current and liveness.can_takeover(run_caller, ev_ts):
                    orphan_unread.append(ev)
                elif not adoptable:
                    other_caller_count += 1
                continue

        own_unread.append(ev)

    if not own_unread and not orphan_unread:
        return None, [], [], other_caller_count, adoptable_runs

    ordered: list[tuple[dict, bool]] = [
        *((ev, False) for ev in own_unread),
        *((ev, True) for ev in orphan_unread),
    ]
    previews: list[str] = []
    ev_names: list[str] = []
    ev_run_ids: list[str] = []
    for ev, is_orphan in ordered[:5]:
        preview, label, rid = _format_event_preview(ev, is_orphan)
        previews.append(preview)
        ev_names.append(label)
        if rid and rid not in ev_run_ids:
            ev_run_ids.append(rid)

    total = len(ordered)
    more = f" 等共 {total} 条" if total > 5 else ""
    summary = "、".join(previews) + more
    other_note = (
        f"（另 {other_caller_count} 条属其他活跃会话，未拦截）"
        if other_caller_count > 0
        else ""
    )
    if orphan_unread and not own_unread:
        reason = (
            f"有原会话已失效且超时未认领的事件，请接盘：先看 delegate events --since {cursor_seq}"
            f"（待接盘 {len(orphan_unread)} 条：{summary}）。{other_note}"
        )
    else:
        reason = (
            f"有新事件，先看 delegate events --since {cursor_seq}"
            f"（未读 {total} 条：{summary}）。{other_note}"
        )
    return reason, ev_names, ev_run_ids, other_caller_count, adoptable_runs


def _parse_pos_float_env(name: str, default: float) -> float:
    raw = os.environ.get(name, "").strip()
    if not raw:
        return default
    try:
        val = float(raw)
        return val if val > 0 else default
    except ValueError:
        return default


def _parse_nonneg_int_env(name: str) -> int | None:
    raw = os.environ.get(name, "").strip()
    if not raw or not raw.isdigit():
        return None
    return int(raw)


def read_capacity_limits(current_repo: Path | None) -> tuple[int, int]:
    """Read `(max_active, repo_max_active)` from env / repo `.delegate.json` / user `config.json`."""
    user_cfg = read_json_file(config_dir() / "config.json") or {}
    repo_cfg = (
        read_json_file(current_repo / ".delegate.json") or {}
        if current_repo is not None
        else {}
    )

    def resolve(env_key: str, cfg_key: str, default: int) -> int:
        ev = _parse_nonneg_int_env(env_key)
        if ev is not None:
            return ev
        for cfg in (repo_cfg, user_cfg):
            val = cfg.get(cfg_key)
            if isinstance(val, int) and not isinstance(val, bool) and val >= 0:
                return val
        return default

    return (
        resolve("DELEGATE_MAX_ACTIVE", "maxActive", DEFAULT_MAX_ACTIVE),
        resolve("DELEGATE_REPO_MAX_ACTIVE", "repoMaxActive", DEFAULT_REPO_MAX_ACTIVE),
    )


def has_idle_slot(
    current_repo: Path | None,
    repo_items: list[tuple[Path | None, Path, str, bool, list[dict]]],
    index_runs: dict[str, dict],
) -> bool:
    """Return True if the current repository (and machine) still has a free run slot."""
    max_active, repo_max_active = read_capacity_limits(current_repo)
    active_states = ("running", "starting", "waiting", "queued")
    cur_active: set[str] = set()
    cur_queued: set[str] = set()
    total_active: set[str] = set()

    for _repo_root, _runs_dir, _label, is_current, items in repo_items:
        for obj in items:
            rid = str(obj.get("run") or "").strip()
            if not rid:
                continue
            st = str(obj.get("state") or "")
            if st in active_states:
                total_active.add(rid)
                if is_current:
                    cur_active.add(rid)
                    if st in ("waiting", "queued"):
                        cur_queued.add(rid)

    # Also include live runs from `<state>/*.slot` not yet covered by `repo_items`.
    sdir = state_dir()
    if sdir.is_dir():
        try:
            for ent in sdir.iterdir():
                if ent.suffix != ".slot":
                    continue
                try:
                    rdir = Path(ent.read_text(encoding="utf-8").strip())
                except OSError:
                    continue
                if not rdir.is_dir() or (rdir / "exit_code").is_file():
                    continue
                snap = read_run_snapshot(rdir)
                if snap is None:
                    continue
                rid = str(snap.get("run") or "").strip()
                st = str(snap.get("state") or "")
                if rid and st in active_states:
                    total_active.add(rid)
        except OSError:
            pass

    if cur_queued:
        return False
    if repo_max_active > 0 and len(cur_active) >= repo_max_active:
        return False
    if max_active > 0 and len(total_active) >= max_active:
        return False
    return True


def _parse_remind_ts(v: object) -> int | None:
    if isinstance(v, (int, float)) and not isinstance(v, bool):
        return int(v)
    if isinstance(v, str):
        return parse_iso_utc(v)
    return None


def load_work_remind(sdir: Path) -> dict[str, dict[str, int]]:
    """Read `<state>/work-remind.json` -> `{work_id: {caller_key: epoch_secs}}`.
    Accepts legacy flat `{work_id: iso_ts}` entries under the `"default"` caller key."""
    doc = read_json_file(sdir / "work-remind.json")
    if not doc:
        return {}
    out: dict[str, dict[str, int]] = {}
    for k, v in doc.items():
        if not isinstance(k, str) or not k.strip():
            continue
        wid = k.strip()
        if isinstance(v, dict):
            per_caller: dict[str, int] = {}
            for ck, cv in v.items():
                if not isinstance(ck, str) or not ck.strip():
                    continue
                ts = _parse_remind_ts(cv)
                if ts is not None:
                    per_caller[ck.strip()] = ts
            if per_caller:
                out[wid] = per_caller
        else:
            ts = _parse_remind_ts(v)
            if ts is not None:
                out[wid] = {"default": ts}
    return out


def save_work_remind(
    sdir: Path,
    remind_map: dict[str, dict[str, int]],
    now: int,
) -> None:
    """Atomically persist `<state>/work-remind.json` keyed by `(work_id, caller_key)`
    without acquiring `.state.lock`."""
    try:
        sdir.mkdir(parents=True, exist_ok=True)
        # Drop entries older than 7 days to keep file bounded.
        cutoff = now - 7 * 86400
        payload: dict[str, dict[str, str]] = {}
        for wid, per_caller in sorted(remind_map.items()):
            if not isinstance(per_caller, dict):
                continue
            kept = {
                ck: time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts))
                for ck, ts in sorted(per_caller.items())
                if isinstance(ts, int) and ts >= cutoff
            }
            if kept:
                payload[wid] = kept
        target = sdir / "work-remind.json"
        tmp = sdir / f"work-remind.json.{os.getpid()}.tmp"
        tmp.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        os.replace(tmp, target)
    except OSError:
        pass


def check_stale_work_items(
    work_items: list[dict],
    cwd: str,
    now: int,
    current_repo: Path | None,
    caller_id: str,
    liveness: CallerLivenessChecker,
) -> tuple[str | None, list[str]]:
    """Pick up to 3 oldest stale `ready`/`verify` or missing-brief `ready` work items
    owned by `caller_id` (or ownerless / whose owner caller expired past orphan timeout),
    excluding owner items and items reminded to this caller within the 6h cooldown.
    Updates `<state>/work-remind.json` keyed by `(work_id, caller_key)`."""
    if not work_items:
        return None, []

    verify_stale_secs = _parse_pos_float_env("DELEGATE_WORK_STALE_VERIFY_HOURS", DEFAULT_STALE_VERIFY_HOURS) * 3600.0
    ready_stale_secs = _parse_pos_float_env("DELEGATE_WORK_STALE_READY_HOURS", DEFAULT_STALE_READY_HOURS) * 3600.0
    cooldown_secs = _parse_pos_float_env("DELEGATE_WORK_REMIND_COOLDOWN_HOURS", DEFAULT_REMIND_COOLDOWN_HOURS) * 3600.0

    current_only = os.environ.get("DELEGATE_STOP_HOOK_CURRENT_ONLY") == "1"
    current_repo_str: str | None = None
    if current_repo is not None:
        try:
            current_repo_str = str(current_repo.resolve())
        except OSError:
            current_repo_str = str(current_repo)

    sdir = state_dir()
    remind_map = load_work_remind(sdir)
    caller_key = caller_id or "default"
    candidates: list[tuple[int, str, str, str, bool]] = []  # (age_secs, id, title, label, is_orphan)

    for item in work_items:
        if item.get("owner"):
            continue
        wid = str(item.get("id") or "").strip()
        if not wid:
            continue
        st = str(item.get("state") or "")
        if st not in ("ready", "verify"):
            continue
        if current_only and current_repo_str is not None:
            item_repo = str(item.get("repo") or "").strip()
            if item_repo:
                try:
                    item_repo = str(Path(item_repo).resolve())
                except OSError:
                    pass
                if item_repo != current_repo_str:
                    continue

        since_raw = str(item.get("stateSince") or item.get("createdAt") or "").strip()
        since_ts = parse_iso_utc(since_raw)
        age_secs = max(0, now - since_ts) if since_ts is not None else 0

        label: str | None = None
        if st == "verify":
            if age_secs >= verify_stale_secs:
                label = "待闭环"
        elif st == "ready":
            brief = str(item.get("brief") or "").strip()
            if not brief:
                label = "缺说明"
            elif age_secs >= ready_stale_secs:
                label = "就绪"

        if label is None:
            continue

        item_caller = str(item.get("caller") or "").strip()
        is_orphan = False
        if item_caller and item_caller != caller_id:
            if not liveness.can_takeover(item_caller, since_ts):
                continue
            is_orphan = True

        last_rem = remind_map.get(wid, {}).get(caller_key)
        if last_rem is not None and (now - last_rem) < cooldown_secs:
            continue

        title = str(item.get("title") or wid).replace("\n", " ").replace("\r", " ").strip()
        candidates.append((age_secs, wid, title, label, is_orphan))

    if not candidates:
        return None, []

    candidates.sort(key=lambda c: (-c[0], c[1]))
    picked = candidates[:3]

    for _, wid, _, _, _ in picked:
        remind_map.setdefault(wid, {})[caller_key] = now
    save_work_remind(sdir, remind_map, now)

    lines = [f"工作表有 {len(picked)} 项停着没动，本回合给每项一个去向："]
    for age_secs, wid, title, label, is_orphan in picked:
        hours = int(age_secs // 3600)
        suffix = "（原会话已失效待接手）" if is_orphan else ""
        lines.append(f"[{wid}] {title} · 停 {hours} 小时 · {label}{suffix}")
    lines.append(
        '可选：派发 `delegate start --work Wxx …` · 等所有者/运行/时间 `delegate work wait Wxx --needs-owner nod|--after-applied RUN|--after-time TIME` · 暂缓 `delegate work defer Wxx --until … --reason "…"` · 关闭 `delegate work close Wxx --evidence "…"`'
    )
    return "\n".join(lines), [wid for _, wid, _, _, _ in picked]


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
    now = int(time.time())
    sdir = state_dir()
    caller_id = current_caller_id(payload)
    cur_harness, cur_tty, cur_cwd = touch_caller_session(sdir, caller_id, now, cwd)
    orphan_secs = (
        _parse_pos_float_env("DELEGATE_ORPHAN_TAKEOVER_HOURS", DEFAULT_ORPHAN_TAKEOVER_HOURS)
        * 3600.0
    )
    cursors_doc = read_json_file(sdir / "cursors.json") or {}
    liveness = CallerLivenessChecker(
        sdir,
        cursors_doc,
        now,
        orphan_secs,
        cur_caller=caller_id,
        cur_harness=cur_harness,
        cur_tty=cur_tty,
        cur_cwd=cur_cwd,
    )

    reasons: list[str] = []
    log_records: list[dict] = []

    index_runs = load_index_runs()
    work_items = load_work_items()
    work_by_id = {
        str(item.get("id") or "").strip(): item
        for item in work_items
        if str(item.get("id") or "").strip()
    }
    repos = discover_repos(cwd, index_runs)

    # Fetch per-repo run snapshots once; share with both unread-event staleness filtering and run checks.
    repo_items: list[tuple[Path | None, Path, str, bool, list[dict]]] = []
    all_runs_by_id: dict[str, dict] = {}
    replied_parents: set[str] = set()
    for rdir_key, entry in index_runs.items():
        if isinstance(entry, dict) and isinstance(entry.get("status"), dict):
            st_obj = dict(entry["status"])
            st_obj.setdefault("dir", str(rdir_key))
            overlay_index_meta(st_obj, entry.get("meta"), Path(rdir_key) if rdir_key else None)
            rid = st_obj.get("run")
            if isinstance(rid, str) and rid:
                all_runs_by_id[rid] = st_obj
            par = st_obj.get("parent")
            if isinstance(par, str) and par:
                replied_parents.add(par)
    for repo_root, runs_dir, label, is_current in repos:
        items = fetch_status(runs_dir, index_runs)
        repo_items.append((repo_root, runs_dir, label, is_current, items))
        for obj in items:
            rid = obj.get("run")
            if isinstance(rid, str) and rid:
                all_runs_by_id[rid] = obj
            par = obj.get("parent")
            if isinstance(par, str) and par:
                replied_parents.add(par)

    unread_reason, ev_names, ev_run_ids, _other_ev_count, adoptable_from_events = check_unread_events(
        payload,
        cwd,
        now,
        work_by_id,
        all_runs_by_id,
        replied_parents,
        cursors_doc,
        liveness,
    )
    if unread_reason:
        reasons.append(unread_reason)
        log_records.append(
            {
                "hook": "events",
                "runs": "、".join(ev_names[:8]),
                "runIds": ev_run_ids[:8],
            }
        )

    adoptable_runs: list[tuple[str, str]] = []
    adoptable_seen: set[str] = set()
    for disp_ev, rid_ev in adoptable_from_events:
        if rid_ev not in adoptable_seen:
            adoptable_seen.add(rid_ev)
            adoptable_runs.append((disp_ev, rid_ev))

    delegate_cmd = find_delegate_bin() or "delegate"
    if repos:
        procs = list_processes()
        applying_procs = [
            (a, pcwd) for argv, pcwd in procs if (a := match_delegate_subcmd(argv, "apply")) is not None
        ]
        merging_procs = [
            (a, pcwd) for argv, pcwd in procs if (a := match_delegate_subcmd(argv, "merge")) is not None
        ]
        gate_cwds, lane_runs = active_lane_state(procs)

        questions: list[tuple[str, str, bool]] = []  # (display, run_id, is_current)
        live_current: list[tuple[str, str, dict, Path | None]] = []  # (display, run_id, obj, repo_root)
        live_other: list[tuple[str, str, dict, Path | None]] = []  # (display, run_id, obj, repo_root)
        unmerged_items: list[tuple[str, str]] = []  # (display, run_id)
        stale_run_ids: list[str] = []
        other_caller_run_count = 0
        work_fresh_def, work_expired_def = work_deferred_runs(work_items, now)

        for repo_root, _runs_dir, label, is_current, items in repo_items:
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

                run_caller = get_run_caller(obj)
                is_adoptable = liveness.is_adoptable(run_caller)
                if not run_belongs_to_scope(obj, is_current, caller_id) and not (
                    is_adoptable and liveness.same_terminal(run_caller)
                ):
                    continue

                adopt_only = False
                if run_caller and run_caller != caller_id:
                    if is_current and liveness.can_takeover(run_caller, get_run_unclaimed_ts(obj)):
                        disp = f"{disp}(原会话已失效待接盘)"
                    elif is_adoptable:
                        adopt_only = True
                    else:
                        if is_current:
                            other_caller_run_count += 1
                        continue

                def _record_adoptable() -> None:
                    if is_adoptable and rid not in adoptable_seen:
                        adoptable_seen.add(rid)
                        adoptable_runs.append((run_label(obj, label, is_current), rid))

                # 1) Questioning runs (stopped waiting for controller decision).
                if is_question:
                    _record_adoptable()
                    if not adopt_only:
                        questions.append((disp, rid, is_current))
                    continue

                # 2) Live runs (running / waiting / starting / queued).
                if st in ("running", "waiting", "starting", "queued"):
                    _record_adoptable()
                    if not adopt_only:
                        if is_current:
                            live_current.append((disp, rid, obj, repo_root))
                        else:
                            live_other.append((disp, rid, obj, repo_root))
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
                _record_adoptable()
                if adopt_only:
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
            waiters = active_waiters(procs)
            locked_inodes = read_locked_inodes()

            def _rdir(obj: dict) -> Path | None:
                d = obj.get("dir")
                return Path(d) if isinstance(d, str) and d else None

            uncovered_cur = [
                (disp, rid)
                for disp, rid, obj, rroot in live_current
                if not waiter_covers_run(_rdir(obj), rid, obj, rroot, True, waiters, locked_inodes)
            ]
            uncovered_other = [
                (disp, rid)
                for disp, rid, obj, rroot in live_other
                if not waiter_covers_run(_rdir(obj), rid, obj, rroot, False, waiters, locked_inodes)
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
            other_note = (
                f"（另 {other_caller_run_count} 项属其他活跃会话，未拦截）"
                if other_caller_run_count > 0
                else ""
            )
            reasons.append(
                f"有已交付的委派还没合也没写明原因（{names}）。{stale_note}"
                f"现在就合（delegate apply），或合不了就执行 `delegate work defer <run> --reason \"…\" --until 4h` 再结束回合。{other_note}"
            )
            log_records.append(
                {
                    "hook": "unmerged",
                    "runs": names,
                    "runIds": [rid for _, rid in unmerged_items[:8]],
                    "stale": ",".join(stale_run_ids[:8]),
                }
            )

    if adoptable_runs:
        names = "、".join(d for d, _ in adoptable_runs[:8])
        rids_cmd = " ".join(rid for _, rid in adoptable_runs[:5])
        reasons.append(
            f"同终端/同工作目录的原会话已失效，有未接手的委派（{names}）：执行 `delegate adopt {rids_cmd}` 接手。"
        )
        log_records.append(
            {
                "hook": "adopt",
                "runs": names,
                "runIds": [rid for _, rid in adoptable_runs[:8]],
            }
        )

    # Low-priority work-hygiene reminder: only when no higher-priority block AND current repo has idle slot.
    if not reasons:
        current_repo = git_toplevel(cwd)
        if has_idle_slot(current_repo, repo_items, index_runs):
            work_reason, work_ids = check_stale_work_items(
                work_items,
                cwd,
                now,
                current_repo,
                caller_id,
                liveness,
            )
            if work_reason:
                reasons.append(work_reason)
                log_records.append(
                    {
                        "hook": "work",
                        "runs": "、".join(work_ids),
                        "workIds": work_ids,
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
