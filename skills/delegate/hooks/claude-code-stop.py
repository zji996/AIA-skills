#!/usr/bin/env python3
"""Claude Code Stop hook for delegate.

Merges three turn-end guards across the current repository and all other
repositories on this machine with live, questioning, unmerged, or noted runs:
1. Questioning runs (waiting on controller decision) or live runs without a
   running `delegate wait`.
2. Unmerged delivered or rejected write runs without a fresh deferral in
   `<repo>/.local/delegate/deferred.tsv` (expiring after `DEFER_HOURS`, default 4).
3. Unfinished post-merge todos recorded via `delegate note` (including runs
   whose directories were already removed by `delegate clean`).

When `stop_hook_active` is true, exits 0 immediately to avoid blocking loops.
"""

from __future__ import annotations

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


def fetch_status(delegate_bin: str, repo_root: Path | None, runs_dir: Path) -> list[dict]:
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

    new_content = ("\n".join(kept_lines) + "\n") if kept_lines else ""
    if new_content != original:
        try:
            tmp = deferred_path.with_suffix(".tsv.tmp")
            tmp.write_text(new_content, encoding="utf-8")
            os.replace(tmp, deferred_path)
        except OSError:
            pass
    return fresh, stale


def run_label(obj: dict, repo_label: str, is_current: bool) -> str:
    name = obj.get("chainName") or obj.get("name") or obj.get("run") or "?"
    name = str(name).replace("\n", " ").replace("\r", " ")
    return name if is_current else f"【{repo_label}】{name}"


def evaluate(payload: dict) -> dict | None:
    if payload.get("stop_hook_active") in (True, "true"):
        return None
    delegate_bin = find_delegate_bin()
    if not delegate_bin:
        return None

    cwd = str(payload.get("cwd") or os.getcwd())
    try:
        defer_hours = float(os.environ.get("DEFER_HOURS", "4"))
    except ValueError:
        defer_hours = 4.0
    now = int(time.time())

    repos = discover_repos(cwd)
    if not repos:
        return None

    questions: list[tuple[str, str, bool]] = []  # (display, run_id, is_current)
    live_current: list[str] = []
    live_other: list[tuple[str, str]] = []  # (display, run_id)
    unmerged_items: list[str] = []
    has_stale_defer = False
    pending_notes: list[tuple[str, str, list[str]]] = []  # (display, run_id, pending_texts)

    for repo_root, runs_dir, label, is_current in repos:
        items = fetch_status(delegate_bin, repo_root, runs_dir)
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

            # 1) Pending post-merge todos (even if applied or cleaned).
            if (obj.get("pendingNotes") or 0) > 0:
                texts = [
                    str(n.get("text", "")).strip()
                    for n in (obj.get("notes") or [])
                    if isinstance(n, dict) and not n.get("done") and str(n.get("text", "")).strip()
                ]
                pending_notes.append((disp, rid, texts))

            if is_cleaned or is_applied:
                continue

            # 2) Questioning runs (stopped waiting for controller decision).
            if is_question:
                questions.append((disp, rid, is_current))
                continue

            # 3) Live runs (running / waiting / starting).
            if st in ("running", "waiting", "starting", "queued"):
                if is_current:
                    live_current.append(disp)
                else:
                    live_other.append((disp, rid))
                continue

            # 4) Unmerged delivered or rejected write runs.
            next_str = str(obj.get("next") or "")
            if st == "rejected" or (st == "delivered" and UNMERGED_NEXT_RE.search(next_str)):
                repo_unmerged[rid] = disp

        fresh_def, stale_def = tidy_and_check_deferred(
            repo_root, set(repo_unmerged.keys()), now, defer_hours
        )
        if stale_def:
            has_stale_defer = True
        for rid, disp in repo_unmerged.items():
            if rid not in fresh_def:
                unmerged_items.append(disp)

    reasons: list[str] = []

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

    # Build unmerged reminder (matching delegate-unmerged.sh tone & rules).
    if unmerged_items:
        names = "、".join(unmerged_items[:8])
        hours_str = str(int(defer_hours)) if defer_hours == int(defer_hours) else str(defer_hours)
        stale_note = (
            f"其中延后超过 {hours_str} 小时的要重新决定：合、清掉，或更新原因（删掉该行第三列重新计时）。"
            if has_stale_defer
            else ""
        )
        reasons.append(
            f"有已交付的委派还没合也没写明原因（{names}）。{stale_note}"
            "现在就合（delegate apply），或合不了就在 .local/delegate/deferred.tsv "
            "写一行「run<Tab>原因」并告诉所有者，再结束回合。续做链只看最末一环，合完把整条链 clean 掉。"
        )

    # Build post-merge todo reminder (delegate note).
    if pending_notes:
        summary_parts: list[str] = []
        cmd_parts: list[str] = []
        for disp, rid, texts in pending_notes[:8]:
            if texts:
                preview = "；".join(texts[:2])
                if len(texts) > 2:
                    preview += f" 等共 {len(texts)} 条"
                summary_parts.append(f"{disp}（{preview}）")
            else:
                summary_parts.append(disp)
            cmd_parts.append(f"`delegate note {rid} --done`")
        items_str = "、".join(summary_parts)
        cmds_str = "、".join(cmd_parts[:3])
        reasons.append(
            f"有未完成的合入后待办（{items_str}）：处理完请执行 {cmds_str}（多条指定序号或 `--all`）再结束回合。"
        )

    if not reasons:
        return None
    return {"decision": "block", "reason": "\n".join(reasons)}


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
