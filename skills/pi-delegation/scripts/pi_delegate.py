#!/usr/bin/env python3
"""Delegate atomic tasks to Pi and judge them by their results.

A run is fire-and-collect: `start` returns at once, `run`/`wait` block until the
run finishes and print one outcome line plus the answer. When `--accept` is
given, this script runs that command in the workdir after Pi finishes; its exit
status, not Pi's own report, decides whether the task was delivered.

Standard library only; Linux (process groups, /proc). Python 3.9+.
"""

import argparse
import fcntl
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

RUNNING_EXIT = 75
USAGE_EXIT = 2
SCRIPT = Path(__file__).resolve()
FINISHED_OK = ("delivered", "answered")
ACTIVE = ("starting", "running")
STARTING_GRACE = 15  # seconds a run may wait for its supervisor before it counts as crashed
# Pi-side glitch: a tool call printed as plain text ends the run with no work done.
LEAKED_CALL = re.compile(r"\bcall:[\w.-]+(?::[\w-]+)?\{")


def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def die(message, code=USAGE_EXIT):
    print(f"pi-delegate: {message}", file=sys.stderr)
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
    temp.write_text(json.dumps(value, ensure_ascii=False) + "\n")
    temp.replace(path)


# ---------------------------------------------------------------- run storage

def runs_root():
    if os.environ.get("PI_DELEGATE_RUNS"):
        return Path(os.environ["PI_DELEGATE_RUNS"])
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
            "directory where start ran; pass a run directory or set PI_DELEGATE_RUNS)")
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
           "mode": meta.get("mode")}
    if summary:
        for key in ("elapsedSeconds", "attempts", "model", "turns", "files", "accept", "tokens", "error"):
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
    out["dir"] = str(run)
    return out


def emit(value):
    print(json.dumps(value, ensure_ascii=False), flush=True)


# ------------------------------------------------------------- Pi execution

def clip(text, limit):
    text = str(text)
    return text if len(text) <= limit else text[:limit] + "..."


def filter_event(event):
    """Map one raw Pi JSON event to at most two compact log events."""
    kind = event.get("type")
    tool, args = event.get("toolName"), event.get("args") or {}
    if kind == "tool_execution_start":
        if tool == "bash":
            command = args.get("command", "")
            return [{"e": "bash", "cmd": "node -e [inline script]" if re.match(r"node\s+-e", command)
                     else clip(command, 180)}]
        if tool in ("edit", "write", "read"):
            return [{"e": tool, "path": args.get("path", "")}]
        arg = " @ ".join(str(v) for v in (args.get("pattern"), args.get("path")) if v)
        return [{"e": "tool", "tool": tool, "arg": clip(arg, 160)}]
    if kind == "tool_execution_end":
        out = [{"e": "bash_done", "ok": not event.get("isError")}] if tool == "bash" else []
        if event.get("isError"):
            content = (event.get("result") or {}).get("content") or [{}]
            lines = [line for line in str(content[0].get("text", "")).splitlines() if line.strip()]
            out.append({"e": "tool_error", "tool": tool, "detail": clip(lines[-1] if lines else "", 240)})
        return out
    if kind == "message_end" and (event.get("message") or {}).get("role") == "assistant":
        message = event["message"]
        text = "\n".join(c.get("text", "") for c in message.get("content") or [] if c.get("type") == "text")
        usage = message.get("usage") or {}
        out = [{"e": "turn", "provider": message.get("provider"), "model": message.get("model"),
                "stopReason": message.get("stopReason"),
                "usage": {k: usage.get(k) for k in ("input", "output", "cacheRead")}}]
        if message.get("stopReason") == "stop" and text.strip():
            out.append({"e": "result", "text": text})
        elif message.get("errorMessage"):
            out.append({"e": "turn_error", "detail": clip(message["errorMessage"], 300)})
        return out
    if kind == "auto_retry_start":
        return [{"e": "retry", "attempt": event.get("attempt"), "maxAttempts": event.get("maxAttempts"),
                 "errorMessage": clip(event.get("errorMessage") or "", 300)}]
    if kind in ("compaction_start", "compaction_end"):
        return [{"e": kind, "reason": event.get("reason")}]
    if kind == "agent_settled":
        return [{"e": "settled"}]
    return []


def leaked_tool_call(answer):
    tail = answer.strip()[-600:]
    return bool(LEAKED_CALL.search(tail)) and tail.endswith("}")


def run_pi(meta, run, attempt, stop_flag, holder):
    """Run Pi once; return (verdict, answer). Verdicts: ok, malformed, failed, timeout, killed, stopped."""
    command = ["pi", "--no-session", "--mode", "json"]
    for flag in ("provider", "model", "thinking"):
        if meta.get(flag):
            command += [f"--{flag}", meta[flag]]
    if meta["mode"] == "read-only":
        command += ["--tools", "read,grep,find,ls"]
    command.append("-p")
    env = {**os.environ, "PI_DELEGATE_ACTIVE": "1"}
    timed_out = threading.Event()
    with open(run / "prompt.md", "rb") as prompt, open(run / "stderr.log", "ab") as stderr, \
            open(run / "events.jsonl", "a", encoding="utf-8") as log:
        proc = subprocess.Popen(command, cwd=meta["workdir"], stdin=prompt, stdout=subprocess.PIPE,
                                stderr=stderr, env=env, start_new_session=True)
        holder["pgid"] = proc.pid
        (run / "pi.pid").write_text(str(proc.pid))

        def expire():
            timed_out.set()
            kill_group(proc.pid)

        timer = threading.Timer(meta["timeoutSeconds"], expire)
        timer.daemon = True
        timer.start()
        turn, answer, settled = None, "", False
        for raw in proc.stdout:
            try:
                event = json.loads(raw)
            except ValueError:
                continue
            for item in filter_event(event):
                item.update(attempt=attempt, at=now_iso())
                log.write(json.dumps(item, ensure_ascii=False) + "\n")
                log.flush()
                if item["e"] == "turn":
                    turn, answer = item, ""
                elif item["e"] == "result":
                    answer = item["text"]
                elif item["e"] == "settled":
                    settled = True
        code = proc.wait()
        timer.cancel()
    if stop_flag.is_set():
        return "stopped", answer
    if timed_out.is_set():
        return "timeout", answer
    if code != 0:
        return ("killed" if code in (-9, 137) else "failed"), answer
    if turn and turn.get("stopReason") == "stop" and settled:
        if not answer.strip() or leaked_tool_call(answer):
            return "malformed", answer
        return "ok", answer
    return "failed", answer


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


def git_changes(workdir):
    try:
        out = subprocess.run(["git", "-C", workdir, "status", "--porcelain", "--untracked-files=all", "."],
                             capture_output=True, text=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        return None
    return sorted(set(out.splitlines()))


def accept(meta, run):
    try:
        done = subprocess.run(meta["accept"], shell=True, cwd=meta["workdir"], capture_output=True, text=True,
                              timeout=meta["acceptTimeoutSeconds"],
                              env={k: v for k, v in os.environ.items() if k != "PI_DELEGATE_ACTIVE"})
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
    verdict, answer, attempts = "failed", "", 0
    try:
        for attempts in range(1, meta["retries"] + 2):
            verdict, answer = run_pi(meta, run, attempts, stop_flag, holder)
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
    # Measure Pi's changes before acceptance, whose own byproducts (caches, reports) are not Pi's work.
    after = git_changes(meta["workdir"])
    if verdict == "ok" and meta.get("accept"):
        summary["accept"] = accept(meta, run)
        state = summary["state"] = "delivered" if summary["accept"]["ok"] else "rejected"
    if answer.strip():
        (run / "result.md").write_text(answer.rstrip("\n") + "\n", encoding="utf-8")
    evs = events(run)
    turns = [e for e in evs if e.get("e") == "turn"]
    files = {e["path"] for e in evs if e.get("e") in ("edit", "write") and e.get("path")}
    before = meta.get("gitBefore")
    if before is not None and after is not None:
        files |= {line[3:] for line in set(after) - set(before)}
    tokens = {k: sum((t.get("usage") or {}).get(k) or 0 for t in turns) for k in ("input", "output", "cacheRead")}
    summary.update(elapsedSeconds=int(time.time() - started), model=turns[-1].get("model") if turns else None,
                   turns=len(turns), files=sorted(files), tokens=tokens)
    if state not in FINISHED_OK and state not in ("rejected", "stopped"):
        errors = [e.get("detail") for e in evs if e.get("e") in ("turn_error", "tool_error")]
        tail = (run / "stderr.log").read_text(errors="replace").strip().splitlines()[-3:] \
            if (run / "stderr.log").is_file() else []
        hint = {"malformed": "answer was empty or a leaked tool call",
                "timeout": f"Pi exceeded {meta['timeout']}"}.get(state)
        message = "; ".join(x for x in [hint] + errors[-1:] + tail if x)
        if message:
            summary["error"] = clip(message, 600)
    write_json(run / "summary.json", summary)
    (run / "exit_code").write_text(f"{0 if state in FINISHED_OK else 1}\n")


# ------------------------------------------------------------------ commands

def missing_tools():
    if shutil.which("pi"):
        return
    kit = SCRIPT.parents[3] / "third_party/pi-kit/install.sh"
    hint = f"sh {kit} --additive" if kit.is_file() else (
        "curl -fsSL https://git.aiatechco.com:31443/zji996/pi-kit/raw/branch/main/install.sh | sh -s -- --additive"
        "\n    (GitHub: https://raw.githubusercontent.com/zji996/pi-kit/main/install.sh)")
    die(f"missing required tools: pi\n  pi: {hint}")


def prune_expired():
    days = os.environ.get("PI_DELEGATE_KEEP_DAYS", "7")
    if not days.isdigit() or int(days) == 0:
        return
    cutoff = time.time() - int(days) * 86400
    for run in all_runs():
        done = run / "exit_code"
        if (run / ".delivered").is_file() and done.is_file() and done.stat().st_mtime < cutoff:
            shutil.rmtree(run, ignore_errors=True)


def start_run(args):
    if os.environ.get("PI_DELEGATE_ACTIVE"):
        die("refusing nested delegation: already running inside a delegated Pi")
    if args.prompt_file == "-":
        prompt = sys.stdin.read()
    elif args.prompt_file:
        if not Path(args.prompt_file).is_file():
            die(f"prompt file does not exist: {args.prompt_file}")
        prompt = Path(args.prompt_file).read_text(encoding="utf-8")
    else:
        prompt = args.prompt_text or " ".join(args.words)
    if not prompt.strip():
        die("empty prompt; pass --prompt-file, --prompt, or trailing text")
    workdir = Path(args.workdir or os.getcwd())
    if not workdir.is_dir():
        die(f"workdir does not exist: {workdir}")
    workdir = str(workdir.resolve())
    missing_tools()
    mode = "read-only" if args.read_only else "write"
    root = runs_root()
    root.mkdir(parents=True, exist_ok=True)
    # Concurrent starts would each miss the other's run in the exclusivity check; serialize them.
    with open(root / ".start.lock", "w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        return create_run(args, prompt, workdir, mode, root)


def create_run(args, prompt, workdir, mode, root):
    if mode == "write" and not args.allow_parallel_writes:
        for run in all_runs():
            meta = read_json(run / "meta.json", {}) or {}
            if meta.get("mode") == "write" and meta.get("workdir") == workdir and run_state(run) in ACTIVE:
                die(f"write run {run.name} is still active in {workdir}; wait for it, use --read-only, "
                    "or pass --allow-parallel-writes")
    if not os.environ.get("PI_DELEGATE_RUNS") and not (root / ".gitignore").exists():
        (root / ".gitignore").write_text("*\n")
    prune_expired()
    slug = re.sub(r"[^A-Za-z0-9._-]+", "-", args.name or "").strip("-")[:40] or os.urandom(2).hex()
    stamp = time.strftime("%Y%m%d-%H%M%S")
    run = root / f"{stamp}-{slug}"
    while True:
        try:
            run.mkdir(mode=0o700)
            break
        except FileExistsError:
            run = root / f"{stamp}-{slug}-{os.urandom(2).hex()}"
    (run / "prompt.md").write_text(prompt if prompt.endswith("\n") else prompt + "\n", encoding="utf-8")
    first_line = next((line.strip() for line in prompt.splitlines() if line.strip()), "")[:120]
    meta = {"run": run.name, "dir": str(run), "workdir": workdir, "mode": mode, "name": args.name or first_line,
            "provider": args.provider, "model": args.model, "thinking": args.thinking,
            "timeout": args.timeout, "timeoutSeconds": seconds(args.timeout),
            "accept": args.accept, "acceptTimeoutSeconds": seconds(args.accept_timeout),
            "retries": args.retries, "gitBefore": git_changes(workdir) if mode == "write" else None,
            "startedAt": now_iso(), "startedEpoch": int(time.time()), "startedNs": time.time_ns()}
    write_json(run / "meta.json", meta)
    with open(run / "supervisor.log", "wb") as log:
        subprocess.Popen([sys.executable, str(SCRIPT), "_supervise", str(run)], stdin=subprocess.DEVNULL,
                         stdout=log, stderr=log, start_new_session=True)
    for _ in range(50):
        if (run / "pid").is_file():
            return run
        time.sleep(0.1)
    write_json(run / "summary.json", {"state": "crashed", "error": "supervisor did not start"})
    (run / "exit_code").write_text("1\n")
    die(f"supervisor did not start; see {run / 'supervisor.log'}")


def print_answer(run, full):
    text = (run / "result.md").read_text(encoding="utf-8")
    limit = int(os.environ.get("PI_DELEGATE_RESULT_CHARS", "6000"))
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
    poll = float(os.environ.get("PI_DELEGATE_POLL", "1"))
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
        has_result = (run / "result.md").is_file()
        if has_result and show_result:
            print_answer(run, full)
        if show_result or not has_result:
            (run / ".delivered").touch()
    if code == RUNNING_EXIT:
        print(f"pi-delegate: still running; call wait again (exit {RUNNING_EXIT})", file=sys.stderr)
    return code


def cmd_start(args):
    run = start_run(args)
    emit(status(run))
    print(f"pi-delegate: started {run.name}; collect with: {SCRIPT} wait {run.name}", file=sys.stderr)
    return 0


def cmd_run(args):
    run = start_run(args)
    print(f"pi-delegate: started {run.name}", file=sys.stderr)
    return collect([run], args.max, args.progress, args.full, True)


def cmd_wait(args):
    if args.all:
        runs = [r for r in all_runs() if run_state(r) in ACTIVE or not (r / ".delivered").is_file()]
        if not runs:
            print("pi-delegate: no active or undelivered runs", file=sys.stderr)
            return 0
    else:
        runs = [resolve_run(ref) for ref in (args.runs or ["last"])]
    return collect(runs, args.max, args.progress, args.full, not args.no_result)


def cmd_status(args):
    for run in [resolve_run(ref) for ref in args.runs] if args.runs else all_runs():
        emit(status(run))
    return 0


def cmd_result(args):
    run = resolve_run(args.run)
    result = run / "result.md"
    if not result.is_file():
        print(f"pi-delegate: no result for {run.name} (state: {run_state(run)})", file=sys.stderr)
        return 1
    print(str(result) if args.path else result.read_text(encoding="utf-8"), end="" if not args.path else "\n")
    if run_state(run) not in ACTIVE:
        (run / ".delivered").touch()
    return 0


def cmd_stop(args):
    for ref in args.runs:
        run = resolve_run(ref)
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
                for name in ("pi.pid", "pid"):
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
                print(f"pi-delegate: keep unreported run {run.name}; read it with wait/result or pass --force",
                      file=sys.stderr)
    for run in dict.fromkeys(targets):
        state = run_state(run)
        if state in ACTIVE:
            print(f"pi-delegate: skip active run {run.name}", file=sys.stderr)
            continue
        shutil.rmtree(run, ignore_errors=True)
        print(f"removed {run.name} ({state})")
    return 0


def parser():
    top = argparse.ArgumentParser(
        prog="pi-delegate",
        description="Delegate atomic tasks to Pi; judge them by results.",
        epilog="States: running | delivered (accept passed) | answered (no --accept) | rejected (accept "
               "failed) | malformed (empty or leaked tool call after reruns) | failed | timeout | killed | "
               "stopped | crashed. Exit: 0 delivered/answered, 1 other finished, 2 usage, 75 still running "
               "at --max. Runs live in $PI_DELEGATE_RUNS or <git root of cwd>/.local/run/pi.")
    sub = top.add_subparsers(dest="command", required=True)

    def launch(p):
        p.add_argument("words", nargs="*", help="prompt text (or use --prompt / --prompt-file)")
        p.add_argument("--prompt", dest="prompt_text")
        p.add_argument("--prompt-file", help="file with the prompt, or - for stdin")
        p.add_argument("--name", help="short label used in the run id")
        p.add_argument("--workdir", help="directory Pi works in (default: cwd)")
        p.add_argument("--read-only", action="store_true", help="only read/grep/find/ls tools")
        p.add_argument("--accept", help="shell command run in the workdir after Pi; exit 0 = delivered")
        p.add_argument("--accept-timeout", default="10m")
        p.add_argument("--timeout", default="15m", help="limit for each Pi attempt (default 15m)")
        p.add_argument("--retries", type=int, default=1, choices=range(0, 4), metavar="N",
                       help="reruns after a malformed answer (default 1)")
        p.add_argument("--provider")
        p.add_argument("--model")
        p.add_argument("--thinking")
        p.add_argument("--allow-parallel-writes", action="store_true")

    def collecting(p):
        p.add_argument("--max", type=seconds, help="stop waiting after this long (exit 75 if still running)")
        p.add_argument("--progress", action="store_true", help="also print writes, errors and retries")
        p.add_argument("--full", action="store_true", help="print the whole answer, not just its tail")

    launch(sub.add_parser("start", help="launch in the background and return at once"))
    run = sub.add_parser("run", help="start, then block until the outcome")
    launch(run)
    collecting(run)
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
    handler = {"start": cmd_start, "run": cmd_run, "wait": cmd_wait, "status": cmd_status, "list": cmd_status,
               "result": cmd_result, "stop": cmd_stop, "clean": cmd_clean}[args.command]
    return handler(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
