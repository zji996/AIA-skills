"""The background supervisor: attempts, acceptance, and the summary."""

import fcntl
import json
import os
import shutil
import signal
import subprocess
import threading
import time
from pathlib import Path

from .agents import run_agent, tier_agent
from .changes import record_changes, relative_to
from .common import (
    FINISHED_OK, GUARD_VARS, LANE_HELD, clip, kill_group, now_iso, read_json, run_shell, state_dir, write_json,
)
from .lane import WAITING, Cancelled, heavy_slot, locked_file, queued_seconds
from .launch import with_contract
from .runs import capacity_error, machine_runs
from .runs import events
from .worktree import prepare_worktree


def log_event(run, event):
    with open(run / "events.jsonl", "a", encoding="utf-8") as log:
        log.write(json.dumps({**event, "at": now_iso()}, ensure_ascii=False) + "\n")



def accept(meta, run, holder, stop_flag):
    """Run the acceptance command in the heavy lane; its timeout starts once it has a slot."""
    env = {k: v for k, v in os.environ.items() if k not in GUARD_VARS}
    env.update(meta.get("env") or {})
    env[LANE_HELD] = "1"
    log = run / "accept.log"
    result = {"command": meta["accept"]}
    try:
        with heavy_slot(f"accept {run.name}", lambda ahead, labels: log_event(run, {"e": "queue", "ahead": ahead}),
                        cancelled=stop_flag.is_set) as queued:
            if stop_flag.is_set():  # stopped just as the slot came free
                raise Cancelled()
            with open(log, "w", encoding="utf-8") as out:
                out.write(f"$ {meta['accept']}\n")
                out.flush()
                code, timed_out = run_shell(meta["accept"], meta["workdir"], env, meta["acceptTimeoutSeconds"], out,
                                            holder)
                out.write(("\n[accept timed out]" if timed_out else "") + f"\n[exit {code}]\n")
    except Cancelled:
        return {**result, "ok": False, "exitCode": None, "tail": "stopped while queued for the heavy lane"}
    if queued >= 1:
        result["queuedSeconds"] = int(queued)
    result.update(ok=code == 0, exitCode=code)
    if code != 0:
        output = log.read_text(encoding="utf-8", errors="replace").split("\n", 1)[-1].rsplit("\n[exit ", 1)[0]
        result["tail"] = output.strip()[-1500:]
    return result



def escalate(meta, run, state, recorded, changed):
    """Hand a failed cheap-tier run to the strong tier, once, when that cannot build on half-done work.

    Only runs whose tier was chosen (not --agent) qualify, and only when read-only or nothing was changed.
    The strong tier must be installed and have room; meta.json is updated under the start lock, so the machine
    pool counts the run as the strong agent from now on, and a reply continues the strong agent's session.
    """
    if meta.get("tier") != "cheap" or state not in ("malformed", "failed", "timeout", "rejected"):
        return False
    if meta["mode"] != "read-only" and (not recorded or changed):
        return False
    strong = tier_agent("strong")
    if strong == meta["agent"] or not shutil.which(strong):
        return False
    slots = state_dir()
    with open(slots / ".start.lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if capacity_error(strong, [r for r in machine_runs(slots) if r != run]):
            log_event(run, {"e": "escalate_skipped", "reason": f"no room for {strong}"})
            return False
        if meta["mode"] == "read-only" and strong == "codex":
            # Pi's read-only mode needed no words; Codex is unsandboxed and must be told.
            prompt = (run / "prompt.md").read_text(encoding="utf-8")
            (run / "prompt.md").write_text(with_contract(prompt, read_only=True), encoding="utf-8")
        meta.update(agent=strong, tier="strong", fork=None, escalatedFrom=meta["agent"])
        write_json(run / "meta.json", meta)
    return True



def supervise(run):
    # Held until this process exits, however it exits: `wait` sleeps on it instead of polling.
    lifetime = locked_file(run / "supervisor.lock", str(os.getpid()))  # noqa: F841 (kept open on purpose)
    (run / "pid").write_text(str(os.getpid()))
    meta = read_json(run / "meta.json")
    stop_flag, holder = threading.Event(), {}

    def stop_group():
        stop_flag.wait()
        if holder.get("pgid"):
            kill_group(holder["pgid"])

    threading.Thread(target=stop_group, daemon=True).start()

    def on_stop(*_):
        stop_flag.set()
        if WAITING["now"]:
            raise Cancelled()  # out of the blocking wait in the heavy lane

    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(sig, on_stop)
    started = time.time()

    def attempts_from(first):
        """Run the agent until its answer is not malformed or the reruns are spent: (verdict, answer, attempt)."""
        verdict, answer, attempt = "failed", "", first - 1
        try:
            for attempt in range(first, first + meta["retries"] + 1):
                if stop_flag.is_set():  # stopped during worktree setup or between attempts
                    return "stopped", answer, attempt
                verdict, answer = run_agent(meta, run, attempt, stop_flag, holder)
                if verdict != "malformed" or attempt >= first + meta["retries"]:
                    break
                log_event(run, {"e": "rerun", "reason": "malformed answer"})
        except Exception as error:  # the summary must always be written
            verdict = "failed"
            with open(run / "stderr.log", "a") as log:
                log.write(f"supervisor error: {error!r}\n")
        return verdict, answer, attempt

    def settle(verdict):
        """Snapshot, check read-only and accept: (state, summary, recorded, changed)."""
        state = {"ok": "answered"}.get(verdict, verdict)
        summary = {"state": state}
        # Measure the agent's changes before acceptance, whose own byproducts (caches, reports) are not its work.
        try:
            recorded = None if setup_error else record_changes(meta, run)
        except (OSError, subprocess.CalledProcessError):
            recorded = None
        changed = [c["path"] for c in recorded[0]] if recorded else []
        if recorded:
            summary["changes"] = recorded[1]
        elif meta.get("top") and not setup_error:
            # Say so rather than report "no changes": a read-only run could not be verified either.
            summary["warning"] = "could not snapshot the working tree; changes are unknown"
        if meta["mode"] == "read-only" and changed and verdict == "ok":
            # Codex runs unsandboxed, so read-only is checked by outcome. The answer still stands on its own: the
            # state says whether there is one, and the changes are reported beside it.
            if meta.get("worktree"):
                summary["readOnlyViolation"] = changed
                summary["warning"] = ("the read-only run changed files in its own worktree; "
                                      "they stay there, never applied")
            else:
                # In place, the caller's own edits during the run land in the same snapshot; they cannot be told
                # apart.
                summary["workspaceChanged"] = changed
                summary["warning"] = ("the working tree changed during this in-place read-only run; "
                                      "the changes may be the caller's own")
        if verdict == "ok" and meta.get("accept") and not stop_flag.is_set():
            summary["accept"] = accept(meta, run, holder, stop_flag)
            state = summary["state"] = "delivered" if summary["accept"]["ok"] else "rejected"
        return state, summary, recorded, changed

    setup_error = None
    try:
        if meta.get("worktree") and not Path(meta["worktree"]["path"]).exists():
            setup_error = prepare_worktree(meta, run)
    except Exception as error:
        setup_error = clip(f"worktree setup failed: {error!r}", 300)
    verdict, answer, attempts = ("failed", "", 0) if setup_error else attempts_from(1)
    state, summary, recorded, changed = settle(verdict)
    cheap = meta["agent"]
    if not stop_flag.is_set() and escalate(meta, run, state, recorded, changed):
        log_event(run, {"e": "escalate", "from": cheap, "to": meta["agent"], "after": state})
        verdict, answer, attempts = attempts_from(attempts + 1)
        state, summary, recorded, changed = settle(verdict)
        summary["escalatedFrom"] = cheap
    summary["attempts"] = attempts
    if answer.strip():
        (run / "result.md").write_text(answer.rstrip("\n") + "\n", encoding="utf-8")
    evs = events(run)
    turns = [e for e in evs if e.get("e") == "turn"]
    # A snapshot is the truth (edits that were reverted do not count); edit events are the fallback outside git.
    files = set(changed) if recorded else {relative_to(e["path"], meta["workdir"]) for e in evs
                                           if e.get("e") in ("edit", "write") and e.get("path")}
    sessions = [e["id"] for e in evs if e.get("e") == "session" and e.get("id")]
    if sessions:
        summary["session"] = sessions[-1]
    tokens = {k: sum((t.get("usage") or {}).get(k) or 0 for t in turns) for k in ("input", "output", "cacheRead")}
    summary.update(elapsedSeconds=int(time.time() - started), model=turns[-1].get("model") if turns else None,
                   turns=len(turns), files=sorted(files), tokens=tokens)
    if queued_seconds(run) >= 1:
        summary["queuedSeconds"] = int(queued_seconds(run))  # the agent's own waits in the heavy lane
    if holder.get("graceSeconds") is not None:
        summary["graceSeconds"] = holder["graceSeconds"]  # ran past --timeout while visibly at work
    if state not in FINISHED_OK and state not in ("rejected", "stopped"):
        errors = [e.get("detail") for e in evs if e.get("e") in ("turn_error", "tool_error")]
        tail = (run / "stderr.log").read_text(errors="replace").strip().splitlines()[-3:] \
            if (run / "stderr.log").is_file() else []
        hint = {"malformed": "answer was empty or a leaked tool call",
                "timeout": f"{meta.get('agent', 'pi')} exceeded {meta['timeout']}"}.get(state)
        message = "; ".join(x for x in [setup_error or hint] + errors[-1:] + tail if x)
        if message:
            summary["error"] = clip(message, 600)
    write_json(run / "summary.json", summary)
    (run / "exit_code").write_text(f"{0 if state in FINISHED_OK else 1}\n")
