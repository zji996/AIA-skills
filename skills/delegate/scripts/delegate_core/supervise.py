"""The background supervisor: attempts, acceptance, and the summary."""

import json
import os
import signal
import subprocess
import threading
import time
from pathlib import Path

from .agents import kill_group, run_agent
from .changes import record_changes, relative_to
from .common import FINISHED_OK, GUARD_VARS, clip, now_iso, read_json, write_json
from .runs import events
from .worktree import prepare_worktree


def accept(meta, run):
    try:
        done = subprocess.run(meta["accept"], shell=True, cwd=meta["workdir"], capture_output=True, text=True,
                              timeout=meta["acceptTimeoutSeconds"],
                              env={k: v for k, v in os.environ.items() if k not in GUARD_VARS})
        code, output = done.returncode, done.stdout + done.stderr
    except subprocess.TimeoutExpired as error:
        code = 124
        parts = [p.decode(errors="replace") if isinstance(p, bytes) else p for p in (error.stdout, error.stderr) if p]
        output = "".join(parts) + "\n[accept timed out]"
    (run / "accept.log").write_text(f"$ {meta['accept']}\n{output}\n[exit {code}]\n", encoding="utf-8")
    result = {"command": meta["accept"], "ok": code == 0, "exitCode": code}
    if code != 0:
        result["tail"] = output.strip()[-1500:]
    return result



def supervise(run):
    (run / "pid").write_text(str(os.getpid()))
    meta = read_json(run / "meta.json")
    stop_flag, holder = threading.Event(), {}

    def on_stop(*_):
        stop_flag.set()
        if holder.get("pgid"):
            threading.Thread(target=kill_group, args=(holder["pgid"],), daemon=True).start()

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
        # Codex runs unsandboxed, so read-only is checked by outcome.
        verdict = "failed"
        summary["readOnlyViolation"] = changed
        state = summary["state"] = "failed"
    if verdict == "ok" and meta.get("accept"):
        summary["accept"] = accept(meta, run)
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
    if state not in FINISHED_OK and state not in ("rejected", "stopped"):
        errors = [e.get("detail") for e in evs if e.get("e") in ("turn_error", "tool_error")]
        tail = (run / "stderr.log").read_text(errors="replace").strip().splitlines()[-3:] \
            if (run / "stderr.log").is_file() else []
        hint = {"malformed": "answer was empty or a leaked tool call",
                "failed": "read-only run changed files" if summary.get("readOnlyViolation") else None,
                "timeout": f"{meta.get('agent', 'pi')} exceeded {meta['timeout']}"}.get(state)
        message = "; ".join(x for x in [setup_error or hint] + errors[-1:] + tail if x)
        if message:
            summary["error"] = clip(message, 600)
    write_json(run / "summary.json", summary)
    (run / "exit_code").write_text(f"{0 if state in FINISHED_OK else 1}\n")
