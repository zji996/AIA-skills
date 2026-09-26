"""The background supervisor: attempts, acceptance, and the summary."""

import json
import os
import signal
import subprocess
import threading
import time
from pathlib import Path

from .agents import run_agent
from .changes import record_changes, relative_to
from .common import FINISHED_OK, GUARD_VARS, LANE_HELD, clip, kill_group, now_iso, read_json, run_shell, write_json
from .lane import WAITING, Cancelled, heavy_slot, locked_file, queued_seconds
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
    verdict, answer, attempts, setup_error = "failed", "", 0, None
    try:
        if meta.get("worktree") and not Path(meta["worktree"]["path"]).exists():
            setup_error = prepare_worktree(meta, run)
        for attempts in range(1, 0 if setup_error else meta["retries"] + 2):
            if stop_flag.is_set():  # stopped during worktree setup or between attempts
                verdict = "stopped"
                break
            verdict, answer = run_agent(meta, run, attempts, stop_flag, holder)
            if verdict != "malformed" or attempts > meta["retries"]:
                break
            with open(run / "events.jsonl", "a", encoding="utf-8") as log:
                log.write(json.dumps({"e": "rerun", "reason": "malformed answer", "at": now_iso()}) + "\n")
    except Exception as error:  # the summary must always be written
        verdict = "failed"
        with open(run / "stderr.log", "a") as log:
            log.write(f"supervisor error: {error!r}\n")
    state = {"ok": "answered"}.get(verdict, verdict)
    summary = {"state": state, "attempts": attempts}
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
            summary["warning"] = "the read-only run changed files in its own worktree; they stay there, never applied"
        else:
            # In place, the caller's own edits during the run land in the same snapshot; they cannot be told apart.
            summary["workspaceChanged"] = changed
            summary["warning"] = ("the working tree changed during this in-place read-only run; "
                                  "the changes may be the caller's own")
    if verdict == "ok" and meta.get("accept") and not stop_flag.is_set():
        summary["accept"] = accept(meta, run, holder, stop_flag)
        state = summary["state"] = "delivered" if summary["accept"]["ok"] else "rejected"
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
