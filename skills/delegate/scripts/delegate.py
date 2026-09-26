#!/usr/bin/env python3
"""Delegate atomic tasks to Pi or Codex and judge them by their results.

A run is fire-and-collect: `start` returns at once, `run`/`wait` block until the
run finishes and print one outcome line plus the answer. When `--accept` is
given, this script runs that command in the workdir after Pi finishes; its exit
status, not Pi's own report, decides whether the task was delivered.

Nesting: a Pi run cannot delegate at all; a Codex run may delegate to Pi but not to
Codex. The calling agent always reviews and decides whether to adopt a result.

Concurrency: active runs are counted per machine (all projects, nested runs included);
a start beyond DELEGATE_MAX_ACTIVE (default 6) or DELEGATE_MAX_CODEX (default 3) is refused.

Changes: in a git repo, the whole working tree is snapshotted as a tree object before and after
the agent (through a scratch index; the real index is untouched), so the outcome lists exactly
what the run changed, apart from what was dirty already. `--worktree` runs in a detached worktree
seeded with that snapshot; `apply` merges the result back. Read-only runs read such a worktree by default,
so the caller's edits meanwhile are neither seen as theirs nor in their way. `reply` continues a run's session.

Settings are read as DELEGATE_<NAME>, falling back to the pre-4.0 PI_DELEGATE_<NAME>.

Standard library only; Linux (process groups, /proc). Python 3.9+.
"""

import argparse
import os
import signal
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))  # delegate_core/ sits next to this script

from delegate_core.agents import kill_group, missing_tools, nesting_error, session_file
from delegate_core.changes import chain_changes, print_changes
from delegate_core.common import (
    ACTIVE, AGENTS, DEFAULT_TIMEOUT, FINISHED_OK, RUNNING_EXIT, SCRIPT, die, emit, read_json, seconds, setting,
    write_json,
)
from delegate_core.launch import launch, read_prompt, start_run
from delegate_core.runs import (
    agent_alive, all_runs, events, latest_in_chain, remove_run, resolve_run, run_state, status, unmerged_worktree,
)
from delegate_core.supervise import supervise
from delegate_core.worktree import apply_conversation


def print_answer(run, full):
    text = (run / "result.md").read_text(encoding="utf-8")
    limit = int(setting("RESULT_CHARS", "6000"))
    print(f"\n===== result: {run.name} ({len(text)} chars) =====")
    if full or len(text) <= limit:
        print(text.rstrip("\n"))
    else:
        print(f"[showing the last {limit} chars; full answer: {run / 'result.md'}]\n..." + text[-limit:].rstrip("\n"))
    print(f"===== end: {run.name} =====", flush=True)



def show_progress(run, tag):
    """Opt-in: new actions since the last call, without per-read noise."""
    marker = run / ".progress"
    seen = int(marker.read_text()) if marker.is_file() else 0
    evs = events(run)
    for event in evs[seen:]:
        if event.get("e") in ("edit", "write", "tool_error", "turn_error", "retry", "rerun",
                              "compaction_start", "compaction_end"):
            emit({"run": tag, **event} if tag else event)
    marker.write_text(str(len(evs)))



def collect(runs, max_seconds, progress, full, show_result):
    deadline = None if max_seconds is None else time.time() + max_seconds
    poll = float(setting("POLL", "1"))
    while True:
        active = False
        for run in runs:
            if progress:
                show_progress(run, run.name if len(runs) > 1 else "")
            active |= run_state(run) in ACTIVE
        if not active or (deadline is not None and time.time() >= deadline):
            break
        time.sleep(poll)
    code = 0
    for run in runs:
        state = run_state(run)
        emit(status(run))
        if state in ACTIVE:
            code = RUNNING_EXIT
            continue
        if state not in FINISHED_OK and code == 0:
            code = 1
        print_changes(run)
        has_result = (run / "result.md").is_file()
        if has_result and show_result:
            print_answer(run, full)
        if show_result or not has_result:
            (run / ".delivered").touch()
    if code == RUNNING_EXIT:
        print(f"delegate: still running; call wait again (exit {RUNNING_EXIT})", file=sys.stderr)
    return code



def cmd_start(args):
    run = start_run(args)
    emit(status(run))
    print(f"delegate: started {run.name}; collect with: {SCRIPT} wait {run.name}", file=sys.stderr)
    return 0



def cmd_run(args):
    run = start_run(args)
    print(f"delegate: started {run.name}", file=sys.stderr)
    return collect([run], args.max, args.progress, args.full, True)



def cmd_wait(args):
    if args.all:
        runs = [r for r in all_runs() if run_state(r) in ACTIVE or not (r / ".delivered").is_file()]
        if not runs:
            print("delegate: no active or undelivered runs", file=sys.stderr)
            return 0
    else:
        runs = [resolve_run(ref) for ref in (args.runs or ["last"])]
    return collect(runs, args.max, args.progress, args.full, not args.no_result)



def cmd_reply(args):
    parent = latest_in_chain(resolve_run(args.run))
    meta = read_json(parent / "meta.json", {}) or {}
    summary = read_json(parent / "summary.json", {}) or {}
    if run_state(parent) in ACTIVE:
        die(f"{parent.name} is still running; wait for it before replying")
    session = None if args.fresh else summary.get("session")
    if not args.fresh and (not session or (meta.get("agent") == "pi" and
                                           not session_file(meta.get("sessionDir"), session))):
        die(f"{parent.name} has no saved session to continue (runs before delegate 4.1 kept none); "
            "use --fresh to start a new session in the same place")
    if meta.get("worktree") and not Path(meta["worktree"]["path"]).exists():
        die(f"the worktree of {parent.name} no longer exists: {meta['worktree']['path']}")
    error = nesting_error(meta["agent"])
    if error:
        die(error)
    missing_tools(meta["agent"])
    for key in ("agent", "provider", "model", "thinking", "retries"):
        setattr(args, key, meta.get(key))
    args.timeout = args.timeout or meta.get("timeout") or DEFAULT_TIMEOUT[meta["agent"]]
    args.accept = meta.get("accept") if args.accept is None else (args.accept or None)
    args.allow_parallel_writes = False
    prompt = read_prompt(args)
    run = launch(args, prompt, meta["workdir"], meta["mode"], {"parent": meta, "session": session,
                                                                  "worktree": meta.get("worktree")})
    print(f"delegate: started {run.name} (reply to {parent.name})", file=sys.stderr)
    return collect([run], args.max, args.progress, args.full, True)



def cmd_diff(args):
    run = resolve_run(args.run)
    meta, top, before, after = chain_changes(run)
    if args.total:
        before = meta.get("chainBase") or before
    command = ["git", "-C", top, "diff", "--no-renames", f"--color={'always' if sys.stdout.isatty() else 'never'}"]
    command += ["--stat"] if args.stat else []
    result = subprocess.run(command + [before, after, "--", *args.paths])
    if result.returncode != 0 and (run / "changes.patch").is_file() and not args.total:
        sys.stdout.write((run / "changes.patch").read_text(errors="replace"))  # objects pruned; the file remains
        return 0
    return result.returncode



def cmd_apply(args):
    """Merge a worktree run (its whole conversation) into the source working tree; the index is untouched."""
    run = latest_in_chain(resolve_run(args.run))
    if run_state(run) in ACTIVE:
        die(f"{run.name} is still running")
    code = apply_conversation(run, args.merge, args.dry_run)
    if (run / ".applied").is_file():
        # Every run on this worktree is merged now, including dead-end (malformed) replies beside the chain.
        path = read_json(run / "meta.json", {})["worktree"]["path"]
        for other in all_runs():
            if ((read_json(other / "meta.json", {}) or {}).get("worktree") or {}).get("path") == path:
                (other / ".applied").write_text((run / ".applied").read_text())
    return code



def cmd_status(args):
    for run in [resolve_run(ref) for ref in args.runs] if args.runs else all_runs():
        emit(status(run))
    return 0



def cmd_result(args):
    run = resolve_run(args.run)
    result = run / "result.md"
    if not result.is_file():
        print(f"delegate: no result for {run.name} (state: {run_state(run)})", file=sys.stderr)
        return 1
    print(str(result) if args.path else result.read_text(encoding="utf-8"), end="" if not args.path else "\n")
    if run_state(run) not in ACTIVE:
        (run / ".delivered").touch()
    return 0



def cmd_stop(args):
    for ref in args.runs:
        run = resolve_run(ref)
        if run_state(run) not in ACTIVE and agent_alive(run):  # its supervisor died; the agent did not
            kill_group(int((run / "agent.pid").read_text()), grace=1)
        if run_state(run) in ACTIVE:
            try:
                os.kill(int((run / "pid").read_text()), signal.SIGTERM)
            except (OSError, ValueError):
                pass
            for _ in range(60):
                if (run / "exit_code").is_file():
                    break
                time.sleep(0.2)
            if not (run / "exit_code").is_file():  # supervisor is gone or wedged; finish the record here
                for name in ("agent.pid", "pi.pid", "pid"):
                    try:
                        kill_group(int((run / name).read_text()), grace=1)
                    except (OSError, ValueError):
                        pass
                write_json(run / "summary.json", {"state": "stopped"})
                (run / "exit_code").write_text("1\n")
            summary = read_json(run / "summary.json", {}) or {}
            if summary.get("state") != "stopped":
                summary["state"] = "stopped"
                write_json(run / "summary.json", summary)
        if not (run / "result.md").is_file():
            (run / ".delivered").touch()
        emit(status(run))
    return 0



def cmd_clean(args):
    if not args.finished and not args.runs:
        die("clean requires runs or --finished")
    targets = [resolve_run(ref) for ref in args.runs]
    if args.finished:
        for run in all_runs():
            if (run / ".delivered").is_file() or args.force:
                targets.append(run)
            elif run_state(run) not in ACTIVE:
                print(f"delegate: keep unreported run {run.name}; read it with wait/result or pass --force",
                      file=sys.stderr)
    for run in dict.fromkeys(targets):
        state = run_state(run)
        if state in ACTIVE or agent_alive(run):
            print(f"delegate: skip active run {run.name}" + ("" if state in ACTIVE else
                  f" (its agent outlived the supervisor; stop it first: {SCRIPT} stop {run.name})"), file=sys.stderr)
            continue
        note = "; its worktree was never applied" if unmerged_worktree(run) else ""
        remove_run(run)
        print(f"removed {run.name} ({state}{note})")
    return 0



def parser():
    top = argparse.ArgumentParser(
        prog="delegate",
        description="Delegate atomic tasks to Pi (e.g. Gemini) or Codex (GPT); judge them by results.",
        epilog="States: running | delivered (accept passed) | answered (no --accept) | rejected (accept "
               "failed) | malformed (empty or leaked tool call after reruns) | failed | timeout | killed | "
               "stopped | crashed. Exit: 0 delivered/answered, 1 other finished, 2 usage, 75 still running "
               "at --max. Runs live in $DELEGATE_RUNS or <git root of cwd>/.local/run/pi.")
    sub = top.add_subparsers(dest="command", required=True)

    def launch(p):
        p.add_argument("words", nargs="*", help="prompt text (or use --prompt / --prompt-file)")
        p.add_argument("--prompt", dest="prompt_text")
        p.add_argument("--prompt-file", help="file with the prompt, or - for stdin")
        p.add_argument("--agent", choices=AGENTS, default="pi", help="who does the work (default pi)")
        p.add_argument("--name", help="short label used in the run id")
        p.add_argument("--workdir", help="directory the agent works in (default: cwd)")
        p.add_argument("--image", action="append", metavar="PATH",
                       help="attach an image to the prompt (repeatable); both agents can read images")
        p.add_argument("--read-only", action="store_true",
                       help="no writes: Pi loses write tools; Codex (unsandboxed) is told and checked afterwards. "
                            "In git, it reads a worktree snapshot, so your own edits meanwhile are not taken for its")
        p.add_argument("--in-place", action="store_true",
                       help="read-only only: read the working tree itself instead of a worktree snapshot")
        p.add_argument("--accept", help="shell command run in the workdir after Pi; exit 0 = delivered")
        p.add_argument("--hide-accept", action="store_true",
                       help="do not tell Pi the acceptance command (blind verification)")
        p.add_argument("--accept-timeout", default="10m")
        p.add_argument("--timeout", help="limit for each attempt (default 15m for pi, 30m for codex)")
        p.add_argument("--retries", type=int, default=1, choices=range(0, 4), metavar="N",
                       help="reruns after a malformed answer (default 1)")
        p.add_argument("--provider")
        p.add_argument("--model")
        p.add_argument("--thinking")
        p.add_argument("--allow-parallel-writes", action="store_true")
        p.add_argument("--worktree", action="store_true",
                       help="work in a detached git worktree seeded with the current working tree; merge back with apply")

    def collecting(p):
        p.add_argument("--max", type=seconds, help="stop waiting after this long (exit 75 if still running)")
        p.add_argument("--progress", action="store_true", help="also print writes, errors and retries")
        p.add_argument("--full", action="store_true", help="print the whole answer, not just its tail")

    launch(sub.add_parser("start", help="launch in the background and return at once"))
    run = sub.add_parser("run", help="start, then block until the outcome")
    launch(run)
    collecting(run)
    reply = sub.add_parser("reply", help="continue a finished run's conversation (same agent, workdir, worktree)")
    reply.add_argument("run")
    reply.add_argument("words", nargs="*", help="the follow-up message (or use --prompt / --prompt-file)")
    reply.add_argument("--prompt", dest="prompt_text")
    reply.add_argument("--prompt-file", help="file with the message, or - for stdin")
    reply.add_argument("--name")
    reply.add_argument("--image", action="append", metavar="PATH")
    reply.add_argument("--accept", help="replace the acceptance command ('' drops it); default: the parent's")
    reply.add_argument("--hide-accept", action="store_true")
    reply.add_argument("--accept-timeout", default="10m")
    reply.add_argument("--timeout")
    reply.add_argument("--fresh", action="store_true",
                       help="new session in the same workdir/worktree and conversation; the message must stand alone")
    collecting(reply)
    diff = sub.add_parser("diff", help="show a run's changes as a git diff")
    diff.add_argument("run", nargs="?", default="last")
    diff.add_argument("paths", nargs="*")
    diff.add_argument("--stat", action="store_true")
    diff.add_argument("--total", action="store_true", help="the whole conversation, not only this run")
    apply = sub.add_parser("apply", help="merge a worktree run's conversation into the source working tree")
    apply.add_argument("run", nargs="?", default="last")
    apply.add_argument("--merge", action="store_true", help="write conflict markers instead of stopping")
    apply.add_argument("--dry-run", action="store_true")
    wait = sub.add_parser("wait", help="block until runs finish; print outcome and answer")
    wait.add_argument("runs", nargs="*")
    wait.add_argument("--all", action="store_true", help="active runs plus finished unreported ones")
    wait.add_argument("--no-result", action="store_true", help="print only the outcome line")
    collecting(wait)
    status_cmd = sub.add_parser("status", aliases=["list"], help="one JSON line per run")
    status_cmd.add_argument("runs", nargs="*")
    result = sub.add_parser("result", help="print the full answer")
    result.add_argument("run", nargs="?", default="last")
    result.add_argument("--path", action="store_true")
    stop = sub.add_parser("stop", help="terminate runs")
    stop.add_argument("runs", nargs="+")
    clean = sub.add_parser("clean", help="delete finished runs")
    clean.add_argument("runs", nargs="*")
    clean.add_argument("--finished", action="store_true")
    clean.add_argument("--force", action="store_true")
    return top



def main(argv):
    if argv[:1] == ["_supervise"]:
        supervise(Path(argv[1]))
        return 0
    args = parser().parse_args(argv)
    for option in ("timeout", "accept_timeout"):
        if getattr(args, option, None) is not None:
            try:
                seconds(getattr(args, option))
            except argparse.ArgumentTypeError as error:
                die(str(error))
    handler = {"start": cmd_start, "run": cmd_run, "reply": cmd_reply, "diff": cmd_diff, "apply": cmd_apply,
               "wait": cmd_wait, "status": cmd_status, "list": cmd_status,
               "result": cmd_result, "stop": cmd_stop, "clean": cmd_clean}[args.command]
    return handler(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
