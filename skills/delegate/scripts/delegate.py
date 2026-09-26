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

Settings are read as DELEGATE_<NAME>, falling back to the pre-4.0 PI_DELEGATE_<NAME>.

Standard library only; Linux (process groups, /proc). Python 3.9+.
"""

import argparse
import fcntl
import hashlib
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
AGENTS = ("pi", "codex")
DEFAULT_TIMEOUT = {"pi": "15m", "codex": "30m"}
DEFAULT_LIMITS = {"MAX_ACTIVE": 6, "MAX_CODEX": 3}
# Environment handed to a delegated agent; used to refuse self-delegation.
ENV_AGENT, ENV_PARENT, ENV_LEGACY = "DELEGATE_AGENT", "DELEGATE_PARENT_RUN", "PI_DELEGATE_ACTIVE"
GUARD_VARS = (ENV_AGENT, ENV_PARENT, ENV_LEGACY, "PI_DELEGATE_AGENT", "PI_DELEGATE_PARENT_RUN")


def setting(name, default=None):
    return os.environ.get(f"DELEGATE_{name}") or os.environ.get(f"PI_DELEGATE_{name}") or default


def now_iso():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def die(message, code=USAGE_EXIT):
    print(f"delegate: {message}", file=sys.stderr)
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
    if summary:
        for key in ("elapsedSeconds", "attempts", "model", "turns", "files", "accept", "readOnlyViolation",
                    "tokens", "error"):
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


def filter_codex_event(event):
    """Map one `codex exec --json` event to compact log events."""
    kind, item = event.get("type"), event.get("item") or {}
    itype = item.get("type")
    if kind == "item.started" and itype == "command_execution":
        return [{"e": "bash", "cmd": clip(item.get("command", ""), 180)}]
    if kind == "item.completed":
        if itype == "command_execution":
            return [{"e": "bash_done", "ok": item.get("exit_code") == 0}]
        if itype == "file_change":
            return [{"e": "edit", "path": change.get("path", "")} for change in item.get("changes") or []]
        if itype == "agent_message":
            return [{"e": "message", "text": item.get("text", "")}]
        if itype == "error":  # Codex reports config warnings this way; they do not end the turn
            return [{"e": "warning", "detail": clip(item.get("message", ""), 240)}]
        if itype in ("mcp_tool_call", "web_search", "todo_list"):
            return [{"e": "tool", "tool": itype, "arg": clip(item.get("query") or item.get("tool") or "", 160)}]
        return []
    if kind == "turn.completed":
        usage = event.get("usage") or {}
        return [{"e": "turn", "stopReason": "stop", "usage": {"input": usage.get("input_tokens"),
                 "output": usage.get("output_tokens"), "cacheRead": usage.get("cached_input_tokens")}}]
    if kind in ("turn.failed", "error"):
        message = (event.get("error") or {}).get("message") or event.get("message") or kind
        return [{"e": "turn_error", "detail": clip(message, 300)}]
    return []


def codex_model():
    try:
        text = (Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")) / "config.toml").read_text()
    except OSError:
        return None
    match = re.search(r'^model\s*=\s*"([^"]+)"', text, re.M)
    return match.group(1) if match else None


def agent_command(meta):
    if meta["agent"] == "codex":
        # Full access on every host, whatever its own config.toml says: bwrap is often unavailable (AppArmor),
        # git makes writes recoverable, and read-only runs are checked by outcome afterwards.
        command = ["codex", "exec", "--json", "--skip-git-repo-check", "-C", meta["workdir"],
                   "--dangerously-bypass-approvals-and-sandbox"]
        if meta.get("model"):
            command += ["-m", meta["model"]]
        if meta.get("thinking"):
            command += ["-c", f'model_reasoning_effort="{meta["thinking"]}"']
        if meta.get("provider"):
            command += ["-c", f'model_provider="{meta["provider"]}"']
        return command + [f"--image={image}" for image in meta.get("images") or []] + ["-"]
    command = ["pi", "--no-session", "--mode", "json"]
    for flag in ("provider", "model", "thinking"):
        if meta.get(flag):
            command += [f"--{flag}", meta[flag]]
    if meta["mode"] == "read-only":
        command += ["--tools", "read,grep,find,ls"]
    return command + ["-p"] + [f"@{image}" for image in meta.get("images") or []]


def agent_env(meta):
    env = {k: v for k, v in os.environ.items() if k not in GUARD_VARS}
    env[ENV_AGENT] = meta["agent"]
    env[ENV_PARENT] = meta["run"]
    if meta["agent"] == "pi":
        env[ENV_LEGACY] = "1"  # older scripts only know this flag
    return env


def caller_agent():
    return setting("AGENT") or ("pi" if os.environ.get(ENV_LEGACY) else None)


def nesting_error(agent):
    caller = caller_agent()
    if caller == "pi":
        return "refusing nested delegation: a delegated Pi run cannot delegate further"
    if caller == "codex" and agent == "codex":
        return "refusing nested delegation: a delegated Codex run may delegate to Pi (--agent pi) but not to Codex"
    return None


def leaked_tool_call(answer):
    tail = answer.strip()[-600:]
    return bool(LEAKED_CALL.search(tail)) and tail.endswith("}")


def run_agent(meta, run, attempt, stop_flag, holder):
    """Run the agent once; return (verdict, answer). Verdicts: ok, malformed, failed, timeout, killed, stopped."""
    codex = meta["agent"] == "codex"
    command, env = agent_command(meta), agent_env(meta)
    convert = filter_codex_event if codex else filter_event
    timed_out = threading.Event()
    with open(run / "prompt.md", "rb") as prompt, open(run / "stderr.log", "ab") as stderr, \
            open(run / "events.jsonl", "a", encoding="utf-8") as log:
        proc = subprocess.Popen(command, cwd=meta["workdir"], stdin=prompt, stdout=subprocess.PIPE,
                                stderr=stderr, env=env, start_new_session=True)
        holder["pgid"] = proc.pid
        (run / "agent.pid").write_text(str(proc.pid))

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
            for item in convert(event):
                if codex and item["e"] == "turn":
                    item["model"] = meta.get("model") or codex_model()
                item.update(attempt=attempt, at=now_iso())
                log.write(json.dumps(item, ensure_ascii=False) + "\n")
                log.flush()
                if item["e"] == "turn":
                    # Codex reports usage once per turn, after its last message.
                    turn = item
                    settled = settled or codex
                    if not codex:
                        answer = ""
                elif item["e"] in ("result", "message"):
                    answer = item["text"]
                elif item["e"] == "settled":
                    settled = True
                elif item["e"] == "turn_error":
                    turn = {"stopReason": "error"}
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


def relative_to(path, workdir):
    """Codex reports absolute paths, Pi and git relative ones; list each file once."""
    try:
        return str(Path(path).relative_to(workdir)) if os.path.isabs(path) else path
    except ValueError:
        return path


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
    verdict, answer, attempts = "failed", "", 0
    try:
        for attempts in range(1, meta["retries"] + 2):
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
    after = git_changes(meta["workdir"])
    before = meta.get("gitBefore")
    changed = sorted(line[3:] for line in set(after) - set(before)) if before is not None and after is not None else []
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
    files = {relative_to(e["path"], meta["workdir"]) for e in evs if e.get("e") in ("edit", "write") and e.get("path")}
    files |= set(changed)
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
        message = "; ".join(x for x in [hint] + errors[-1:] + tail if x)
        if message:
            summary["error"] = clip(message, 600)
    write_json(run / "summary.json", summary)
    (run / "exit_code").write_text(f"{0 if state in FINISHED_OK else 1}\n")


# ------------------------------------------------------------------ commands

def missing_tools(agent):
    if shutil.which(agent):
        return
    if agent == "codex":
        die("missing required tools: codex\n  codex: see https://github.com/openai/codex (npm i -g @openai/codex), then log in")
    kit = SCRIPT.parents[3] / "third_party/pi-kit/install.sh"
    hint = f"sh {kit} --additive" if kit.is_file() else (
        "curl -fsSL https://git.aiatechco.com:31443/zji996/pi-kit/raw/branch/main/install.sh | sh -s -- --additive"
        "\n    (GitHub: https://raw.githubusercontent.com/zji996/pi-kit/main/install.sh)")
    die(f"missing required tools: pi\n  pi: {hint}")


def prune_expired():
    days = setting("KEEP_DAYS", "7")
    if not days.isdigit() or int(days) == 0:
        return
    cutoff = time.time() - int(days) * 86400
    for run in all_runs():
        done = run / "exit_code"
        if (run / ".delivered").is_file() and done.is_file() and done.stat().st_mtime < cutoff:
            shutil.rmtree(run, ignore_errors=True)


def state_dir():
    # Deliberately not a DELEGATE_* setting: a delegated agent must not opt out of the machine's pool.
    return Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state") / "delegate"


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
        if (run / "meta.json").is_file() and run_state(run) in ACTIVE:
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


def start_run(args):
    error = nesting_error(args.agent)
    if error:
        die(error)
    if args.timeout is None:
        args.timeout = DEFAULT_TIMEOUT[args.agent]
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
    images = []
    for image in args.image or []:
        if not Path(image).is_file():
            die(f"image does not exist: {image}")
        images.append(str(Path(image).resolve()))
    args.image = images
    missing_tools(args.agent)
    mode = "read-only" if args.read_only else "write"
    root = runs_root()
    root.mkdir(parents=True, exist_ok=True)
    slots = state_dir()
    slots.mkdir(parents=True, exist_ok=True)
    # Concurrent starts would each miss the other's run in the capacity and exclusivity checks; serialize them.
    with open(slots / ".start.lock", "w") as machine_lock, open(root / ".start.lock", "w") as lock:
        fcntl.flock(machine_lock, fcntl.LOCK_EX)
        fcntl.flock(lock, fcntl.LOCK_EX)
        error = capacity_error(args.agent, machine_runs(slots))
        if error:
            die(error)
        run = create_run(args, prompt, workdir, mode, root)
        (slots / (hashlib.sha1(str(run).encode()).hexdigest()[:16] + ".slot")).write_text(f"{run}\n")
        return run


def with_contract(prompt, accept=None, read_only=False):
    """State the task boundary and definition of done, as a delegator would, in the prompt's language."""
    zh = bool(re.search(r"[\u3040-\u30ff\u4e00-\u9fff]", prompt))
    notes = []
    if read_only:
        notes.append("只读任务：不要创建、修改或删除任何文件；结束后会核对工作目录，任何改动都会使任务判为失败。"
                     "用 delegate 委派子任务产生的记录在 git 忽略的目录里，不算改动，无需改动其存放位置。" if zh else
                     "Read-only task: do not create, modify or delete files; the working directory is checked afterwards "
                     "and any change fails the task. Records of subtasks delegated with delegate live in git-ignored "
                     "directories and do not count; leave their location as it is.")
    if accept:
        notes.append(("完成标准：你结束后，委派方会在工作目录中运行下面的命令，退出码为 0 即视为完成。" if zh else
                      "Definition of done: after you finish, the delegator runs this command in the working directory; "
                      "exit code 0 counts as complete.") + f"\n\n```sh\n{accept}\n```")
    if not notes:
        return prompt
    return prompt.rstrip("\n") + "\n\n---\n" + "\n\n".join(notes) + "\n"


def create_run(args, prompt, workdir, mode, root):
    if mode == "write" and not args.allow_parallel_writes:
        for run in all_runs():
            meta = read_json(run / "meta.json", {}) or {}
            if run.name == setting("PARENT_RUN"):
                continue  # the caller's own run is waiting on this helper
            if meta.get("mode") == "write" and meta.get("workdir") == workdir and run_state(run) in ACTIVE:
                die(f"write run {run.name} is still active in {workdir}; wait for it, use --read-only, "
                    "or pass --allow-parallel-writes")
    if not setting("RUNS") and not (root / ".gitignore").exists():
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
    first_line = next((line.strip() for line in prompt.splitlines() if line.strip()), "")[:120]
    # Pi's read-only mode removes write tools; Codex gets the boundary in writing and a result check.
    prompt = with_contract(prompt, None if args.hide_accept else args.accept,
                           read_only=mode == "read-only" and args.agent == "codex")
    # prompt.md is exactly what Pi receives.
    (run / "prompt.md").write_text(prompt if prompt.endswith("\n") else prompt + "\n", encoding="utf-8")
    meta = {"run": run.name, "dir": str(run), "workdir": workdir, "mode": mode, "agent": args.agent,
            "name": args.name or first_line,
            "provider": args.provider, "model": args.model, "thinking": args.thinking,
            "timeout": args.timeout, "timeoutSeconds": seconds(args.timeout),
            "accept": args.accept, "acceptTimeoutSeconds": seconds(args.accept_timeout),
            "retries": args.retries, "images": args.image, "gitBefore": git_changes(workdir),
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
        if state in ACTIVE:
            print(f"delegate: skip active run {run.name}", file=sys.stderr)
            continue
        shutil.rmtree(run, ignore_errors=True)
        print(f"removed {run.name} ({state})")
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
                       help="no writes: Pi loses write tools; Codex (unsandboxed) is told and checked by git status afterwards")
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
