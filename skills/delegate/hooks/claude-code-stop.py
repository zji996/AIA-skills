#!/usr/bin/env python3
"""Claude Code Stop hook for delegate.

Graded turn-end guard across the current repository and all other repositories on
this machine — blocks ONLY on immediate blockers (never on blocked/deferred/non-stale
verify/backlog ready items):
0. Event-stream race lock: unread alert events (delivered, question, rejected, failed,
   stall_warning, queue_paused, needs_attention, NEEDS_CONTROLLER, and one-shot work
   transitions work_ready / work_defer_expired / work_stale) past `cursors.json`.
1. Questioning runs (waiting on controller decision) or live runs without a
   running `delegate wait`.
2. Unmerged delivered or rejected write runs without a fresh deferral in
   `<state>/work.jsonl` (or legacy `<repo>/.local/delegate/deferred.tsv`).

When `stop_hook_active` is true, exits 0 immediately to avoid blocking loops.
"""

from __future__ import annotations

import calendar
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

WAIT_RE = re.compile(r"(?:^|/|\s)delegate\s+wait(?:\s|$)")
UNMERGED_NEXT_RE = re.compile(r"^(?:review|apply)\b")
UNREAD_ALERT_KINDS = {
    "delivered",
    "question",
    "rejected",
    "failed",
    "stall_warning",
    "queue_paused",
    "needs_attention",
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


def git_toplevel(path: str) -> Path | None:
    if not path or not os.path.isdir(path):
        return None
    # Pure-filesystem lookup first (zero subprocesses, <0.1ms).
    try:
        cur = Path(path).resolve()
    except OSError:
        cur = Path(path)
    if ".git" not in cur.parts:
        for candidate in (cur, *cur.parents):
            if (candidate / ".git").exists():
                return candidate
    env = {**os.environ, "GIT_OPTIONAL_LOCKS": "0"}
    try:
        res = subprocess.run(
            ["git", "-C", path, "rev-parse", "--path-format=absolute", "--show-toplevel"],
            capture_output=True,
            text=True,
            env=env,
            timeout=2,
        )
        if res.returncode == 0 and res.stdout.strip():
            return Path(res.stdout.strip())
    except (OSError, subprocess.SubprocessError):
        pass
    return None


def repo_from_runs_dir(runs_dir: Path) -> Path | None:
    parts = runs_dir.parts
    if len(parts) >= 4 and parts[-3:] in ((".local", "run", "delegate"), (".local", "run", "pi")):
        return runs_dir.parent.parent.parent
    return None


def discover_repos(cwd: str) -> list[tuple[Path | None, Path, str, bool]]:
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
        # 1) Machine repo registry written by delegate launch / note / status.
        repos_file = sdir / "repos.json"
        if repos_file.is_file():
            try:
                data = json.loads(repos_file.read_text(encoding="utf-8"))
                repos_map = data.get("repos") if isinstance(data, dict) else None
                if isinstance(repos_map, dict):
                    for entry in repos_map.values():
                        if not isinstance(entry, dict):
                            continue
                        runs_str = entry.get("runs")
                        repo_str = entry.get("repo")
                        if not isinstance(runs_str, str) or not runs_str:
                            continue
                        runs_p = Path(runs_str)
                        repo_p = Path(repo_str) if isinstance(repo_str, str) and repo_str else repo_from_runs_dir(runs_p)
                        candidates.append((repo_p, runs_p, False))
            except (OSError, ValueError):
                pass

        # 2) Live slot files under state_dir (covers pre-6.25.0 active runs).
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
                        try:
                            meta = json.loads((run_dir / "meta.json").read_text(encoding="utf-8"))
                            src = (meta.get("worktree") or {}).get("source") if isinstance(meta.get("worktree"), dict) else None
                            raw = src or meta.get("top") or meta.get("workdir")
                            if isinstance(raw, str) and raw:
                                repo_p = Path(raw)
                        except (OSError, ValueError):
                            pass
                    candidates.append((repo_p, runs_p, False))
            except OSError:
                pass

        # 3) Worktree directories under cache_dir (fallback for older repos with unmerged worktrees).
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


def load_index_runs() -> dict[str, dict]:
    """Lock-free read of `<state>/index.json` runs map (zero subprocesses)."""
    idx_file = state_dir() / "index.json"
    if not idx_file.is_file():
        return {}
    try:
        doc = json.loads(idx_file.read_text(encoding="utf-8"))
        runs_map = doc.get("runs") if isinstance(doc, dict) else None
        return runs_map if isinstance(runs_map, dict) else {}
    except (OSError, ValueError):
        return {}


def fetch_status_from_index(runs_dir: Path, index_runs: dict[str, dict]) -> list[dict] | None:
    """Try reading all runs under `runs_dir` directly from `index.json` (lock-free, zero forks).
    Returns `None` to trigger fallback if any run directory is missing from the index or stale."""
    if not index_runs or not runs_dir.is_dir():
        return None
    try:
        actual_dirs = [
            p.resolve() for p in runs_dir.iterdir() if p.is_dir() and (p / "meta.json").is_file()
        ]
    except OSError:
        return None
    if not actual_dirs:
        return []
    out: list[dict] = []
    for rdir in actual_dirs:
        entry = index_runs.get(str(rdir))
        if not isinstance(entry, dict) or not isinstance(entry.get("status"), dict):
            return None
        st_obj = entry["status"]
        st = str(st_obj.get("state") or "")
        # If the run was active in the index, check if it has since exited or been applied.
        if st in ("running", "starting", "waiting", "queued"):
            if (rdir / "exit_code").is_file():
                return None
            # Verify supervisor is still alive; if crashed, fall back to full status.
            pid_file = rdir / "pid"
            if pid_file.is_file():
                try:
                    pid = int(pid_file.read_text(encoding="utf-8").strip())
                    os.kill(pid, 0)
                except (OSError, ValueError):
                    return None
        else:
            # Verify `.applied` wasn't added directly on disk after index write.
            if (rdir / ".applied").exists() and not (rdir / ".generate-pending").exists():
                if st_obj.get("applied") is not True:
                    return None
        out.append(st_obj)
    return out


def fetch_status(
    delegate_bin: str | None,
    repo_root: Path | None,
    runs_dir: Path,
    index_runs: dict[str, dict] | None = None,
) -> list[dict]:
    if index_runs:
        fast = fetch_status_from_index(runs_dir, index_runs)
        if fast is not None:
            return fast
    if not delegate_bin:
        return []
    env = {**os.environ, "DELEGATE_RUNS": str(runs_dir)}
    cwd = str(repo_root) if repo_root and repo_root.is_dir() else None
    try:
        res = subprocess.run(
            [delegate_bin, "status", "--json"],
            capture_output=True,
            text=True,
            env=env,
            cwd=cwd,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return []
    if res.returncode != 0:
        return []
    out: list[dict] = []
    for line in res.stdout.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        if isinstance(obj, dict) and isinstance(obj.get("run"), str):
            out.append(obj)
    return out


def active_waiter_cmdlines() -> list[str]:
    override = os.environ.get("DELEGATE_STOP_HOOK_WAITERS")
    if override is not None:
        return [ln for ln in override.splitlines() if ln.strip()]
    try:
        res = subprocess.run(
            ["ps", "-eo", "args="],
            capture_output=True,
            text=True,
            timeout=2,
        )
        if res.returncode != 0:
            return []
        return [ln.strip() for ln in res.stdout.splitlines() if WAIT_RE.search(ln)]
    except (OSError, subprocess.SubprocessError):
        return []


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
                if not isinstance(item, dict) or item.get("state") != "deferred":
                    continue
                runs = [r for r in (item.get("runs") or []) if isinstance(r, str) and r]
                if not runs:
                    continue
                until_ts = parse_iso_utc(str(item.get("deferUntil") or ""))
                if until_ts is not None and until_ts > now:
                    fresh.update(runs)
                else:
                    expired.update(runs)
    except OSError:
        return set(), set()
    return fresh, expired


def tidy_and_check_deferred(
    repo_root: Path | None,
    pending_ids: set[str],
    now: int,
    defer_hours: float,
) -> tuple[set[str], set[str]]:
    """Return (fresh_deferred_ids, stale_deferred_ids) and tidy deferred.tsv in place."""
    if repo_root is None:
        return set(), set()
    deferred_path = repo_root / ".local" / "delegate" / "deferred.tsv"
    if not deferred_path.is_file():
        return set(), set()
    try:
        original = deferred_path.read_text(encoding="utf-8")
    except OSError:
        return set(), set()

    kept_lines: list[str] = []
    fresh: set[str] = set()
    stale: set[str] = set()
    limit_secs = defer_hours * 3600.0

    for raw in original.splitlines():
        if not raw.strip():
            continue
        cols = raw.split("\t")
        run_id = cols[0].strip()
        if not run_id or run_id not in pending_ids:
            continue
        reason = cols[1] if len(cols) >= 2 else ""
        ts_str = cols[2].strip() if len(cols) >= 3 else ""
        if ts_str.isdigit():
            ts = int(ts_str)
        else:
            ts = now
        kept_lines.append(f"{run_id}\t{reason}\t{ts}")
        if now - ts > limit_secs:
            stale.add(run_id)
        else:
            fresh.add(run_id)

    # Strictly read-only: never rewrite deferred.tsv from the Stop hook.
    return fresh, stale


def run_label(obj: dict, repo_label: str, is_current: bool) -> str:
    name = obj.get("chainName") or obj.get("name") or obj.get("run") or "?"
    name = str(name).replace("\n", " ").replace("\r", " ")
    return name if is_current else f"【{repo_label}】{name}"


def check_unread_events(payload: dict, cwd: str) -> str | None:
    """Race lock: block if unread delivery/question/attention events exist past the cursor."""
    sdir = state_dir()
    events_file = sdir / "events.jsonl"
    if not events_file.is_file():
        return None

    cursors_file = sdir / "cursors.json"
    cursors_doc: dict = {}
    if cursors_file.is_file():
        try:
            loaded = json.loads(cursors_file.read_text(encoding="utf-8"))
            if isinstance(loaded, dict):
                cursors_doc = loaded
        except (OSError, ValueError):
            cursors_doc = {}

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
        return None

    if not unread:
        return None

    previews: list[str] = []
    for ev in unread[:5]:
        kind = str(ev.get("kind") or "")
        klabel = KIND_LABELS.get(kind, kind)
        pl = ev.get("payload") if isinstance(ev.get("payload"), dict) else {}
        wid = str(pl.get("workId") or "").strip()
        raw_label = str(pl.get("name") or pl.get("queueName") or ev.get("run") or "?").replace("\n", " ")
        label = f"{wid}:{raw_label}" if wid else raw_label
        card = pl.get("card") if isinstance(pl.get("card"), dict) else None
        if kind == "delivered" and card is not None and isinstance(card.get("riskTotal"), int):
            rt = card["riskTotal"]
            previews.append(f"{klabel}:{label}(风险{rt},快进—)")
        else:
            previews.append(f"{klabel}:{label}")
    more = f" 等共 {len(unread)} 条" if len(unread) > 5 else ""
    summary = "、".join(previews) + more
    return f"有新事件，先看 delegate events --since {cursor_seq}（未读 {len(unread)} 条：{summary}）。"


def evaluate(payload: dict) -> dict | None:
    if payload.get("stop_hook_active") in (True, "true"):
        return None

    cwd = str(payload.get("cwd") or os.getcwd())
    reasons: list[str] = []

    unread_reason = check_unread_events(payload, cwd)
    if unread_reason:
        reasons.append(unread_reason)

    index_runs = load_index_runs()
    delegate_bin = find_delegate_bin()
    if not delegate_bin and not index_runs:
        return {"decision": "block", "reason": "\n".join(reasons)} if reasons else None

    try:
        defer_hours = float(os.environ.get("DEFER_HOURS", "4"))
    except ValueError:
        defer_hours = 4.0
    now = int(time.time())

    repos = discover_repos(cwd)
    if not repos:
        return {"decision": "block", "reason": "\n".join(reasons)} if reasons else None

    questions: list[tuple[str, str, bool]] = []  # (display, run_id, is_current)
    live_current: list[str] = []
    live_other: list[tuple[str, str]] = []  # (display, run_id)
    unmerged_items: list[str] = []
    has_stale_defer = False
    work_fresh_def, work_expired_def = work_deferred_runs(now)

    for repo_root, runs_dir, label, is_current in repos:
        items = fetch_status(delegate_bin, repo_root, runs_dir, index_runs)
        if not items:
            tidy_and_check_deferred(repo_root, set(), now, defer_hours)
            continue

        parents = {obj["parent"] for obj in items if isinstance(obj.get("parent"), str) and obj["parent"]}
        leaves = [obj for obj in items if obj["run"] not in parents]

        repo_unmerged: dict[str, str] = {}

        for obj in leaves:
            rid = obj["run"]
            st = str(obj.get("state") or "")
            is_applied = obj.get("applied") is True
            is_cleaned = obj.get("cleaned") is True
            is_question = obj.get("question") is True
            disp = run_label(obj, label, is_current)

            # Non-stale post-merge notes / verify items do NOT block turn end (6.31 graded guard;
            # stale >24h items emit one-shot `work_stale` events consumed via events.jsonl).
            if is_cleaned or is_applied:
                continue

            # 1) Questioning runs (stopped waiting for controller decision).
            if is_question:
                questions.append((disp, rid, is_current))
                continue

            # 2) Live runs (running / waiting / starting).
            if st in ("running", "waiting", "starting", "queued"):
                if is_current:
                    live_current.append(disp)
                else:
                    live_other.append((disp, rid))
                continue

            # 3) Unmerged delivered or rejected write runs.
            next_str = str(obj.get("next") or "")
            if st == "rejected" or (st == "delivered" and UNMERGED_NEXT_RE.search(next_str)):
                repo_unmerged[rid] = disp

        fresh_def, stale_def = tidy_and_check_deferred(
            repo_root, set(repo_unmerged.keys()), now, defer_hours
        )
        fresh_all = (fresh_def | work_fresh_def) - stale_def
        if stale_def or (set(repo_unmerged.keys()) & work_expired_def):
            has_stale_defer = True
        for rid, disp in repo_unmerged.items():
            if rid not in fresh_all:
                unmerged_items.append(disp)

    # Build questioning reminder (always blocks — needs controller reply now).
    if questions:
        names = "、".join(d for d, _, _ in questions[:8])
        cmds = "、".join(f"`delegate reply {rid} '<决定>'`" for _, rid, _ in questions[:3])
        reasons.append(f"有委派正在提问等决定（{names}）：先回答再结束回合（{cmds}）。")

    # Build waiter reminder for live runs.
    if live_current or live_other:
        waiters = active_waiter_cmdlines()
        has_any_waiter = bool(waiters)
        has_machine_waiter = any("--machine" in w for w in waiters)

        uncovered_cur = [] if has_any_waiter else live_current
        uncovered_other = [
            disp
            for disp, rid in live_other
            if not has_machine_waiter and not any(rid in w for w in waiters)
        ]
        if uncovered_cur:
            names = "、".join(uncovered_cur[:5])
            reasons.append(
                f"还有委派在跑（{names}），但没有 delegate wait 挂着：在后台挂上 `{delegate_bin} wait` 再结束回合。"
            )
        if uncovered_other:
            names = "、".join(uncovered_other[:5])
            reasons.append(
                f"其他仓库还有委派在跑（{names}），但没有对应 waiter：在后台挂上 `{delegate_bin} wait --machine` 或切到对应仓库处理。"
            )

    # Build unmerged reminder.
    if unmerged_items:
        names = "、".join(unmerged_items[:8])
        hours_str = str(int(defer_hours)) if defer_hours == int(defer_hours) else str(defer_hours)
        stale_note = (
            f"其中延后超过 {hours_str} 小时的要重新决定：合、清掉，或重新暂缓。"
            if has_stale_defer
            else ""
        )
        reasons.append(
            f"有已交付的委派还没合也没写明原因（{names}）。{stale_note}"
            "现在就合（delegate apply），或合不了就执行 `delegate work defer <run> --reason \"…\" --until 4h` 再结束回合。"
        )

    if not reasons:
        return None
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
