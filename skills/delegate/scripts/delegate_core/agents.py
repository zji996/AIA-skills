"""The colleagues: how Pi and Codex are started, continued and read, and who may delegate."""

import json
import os
import re
import shutil
import sys
import subprocess
import threading
import time
import uuid
from pathlib import Path

from .common import (
    AGENTS, BUSY_WINDOW, DEFAULT_TIMEOUT_GRACE, ENV_AGENT, ENV_LEGACY, ENV_RUN_DIR, GUARD_VARS, SCRIPT, clip, die,
    kill_group, now_iso, setting,
)
from .lane import queued_seconds


# Pi-side glitch: a tool call printed as plain text ends the run with no work done.
LEAKED_CALL = re.compile(r"\bcall:[\w.-]+(?::[\w-]+)?\{")



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
    if kind == "thread.started" and event.get("thread_id"):
        return [{"e": "session", "id": event["thread_id"]}]
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



def agent_command(meta, session):
    """A reply forks its parent's session on every attempt, so the parent is never changed and a rerun
    after a malformed answer starts again from the same conversation instead of the broken one."""
    fork = meta.get("fork")
    if meta["agent"] == "codex":
        # Full access on every host, whatever its own config.toml says: bwrap is often unavailable (AppArmor),
        # git makes writes recoverable, and read-only runs are checked by outcome afterwards.
        # `fork` has no -C; the process cwd is the workdir either way.
        command = ["codex", "exec"] + (["fork", fork] if fork else []) + ["--json", "--skip-git-repo-check"]
        command += ([] if fork else ["-C", meta["workdir"]]) + ["--dangerously-bypass-approvals-and-sandbox"]
        if meta.get("model"):
            command += ["-m", meta["model"]]
        if meta.get("thinking"):
            command += ["-c", f'model_reasoning_effort="{meta["thinking"]}"']
        if meta.get("provider"):
            command += ["-c", f'model_provider="{meta["provider"]}"']
        return command + [f"--image={image}" for image in meta.get("images") or []] + ["-"]
    # Each run keeps its conversation under session/, so that `reply` can fork it.
    command = ["pi"] + (["--fork", fork] if fork else ["--session-id", session])
    command += ["--session-dir", meta["sessionDir"], "--mode", "json"]
    for flag in ("provider", "model", "thinking"):
        if meta.get(flag):
            command += [f"--{flag}", meta[flag]]
    if meta["mode"] == "read-only":
        command += ["--tools", "read,grep,find,ls"]
    return command + ["-p"] + [f"@{image}" for image in meta.get("images") or []]



def session_file(directory, session):
    """Pi's file for a session id, or None."""
    found = sorted(Path(directory or "/-").glob(f"*_{session}.jsonl")) if session else []
    return found[-1] if found else None


def agent_env(meta):
    env = {k: v for k, v in os.environ.items() if k not in GUARD_VARS}
    env.update(meta.get("env") or {})  # .delegate.json, e.g. CUDA_VISIBLE_DEVICES="" to keep agents off the GPU
    env[ENV_AGENT] = meta["agent"]  # marks the process as a delegated agent: it may not delegate in turn
    env[ENV_RUN_DIR] = meta["dir"]
    if meta["agent"] == "pi":
        env[ENV_LEGACY] = "1"  # older scripts only know this flag
    return env



def caller_agent():
    return setting("AGENT") or ("pi" if os.environ.get(ENV_LEGACY) else None)



def nesting_error():
    """One level only: every result comes back to the caller, who can see and review it."""
    caller = caller_agent()
    if caller:
        return (f"refusing nested delegation: this is a delegated {caller} run; do the work yourself and report "
                "back to your caller (`lane` for heavy checks still works)")
    return None



def tier_agent(tier):
    agent = setting(f"{tier.upper()}_AGENT", {"cheap": "pi", "strong": "codex"}[tier])
    if agent not in AGENTS:
        die(f"DELEGATE_{tier.upper()}_AGENT must be one of {', '.join(AGENTS)}, got {agent!r}")
    return agent



def choose_agent(args):
    """--agent names the colleague outright; otherwise the tier does (read-only: cheap, write: strong).
    A cheap colleague that is not installed gives way to the strong one, so hosts without it still work."""
    if args.agent and args.tier:
        die("--agent and --tier contradict each other; give one")
    if args.agent:
        args.tier = None
        return
    args.tier = args.tier or ("cheap" if args.read_only else "strong")
    args.agent = tier_agent(args.tier)
    strong = tier_agent("strong")
    if args.tier == "cheap" and not shutil.which(args.agent) and shutil.which(strong):
        print(f"delegate: {args.agent} is not installed; using the strong tier ({strong})", file=sys.stderr)
        args.agent, args.tier = strong, "strong"



def leaked_tool_call(answer):
    tail = answer.strip()[-600:]
    return bool(LEAKED_CALL.search(tail)) and tail.endswith("}")



def run_agent(meta, run, attempt, stop_flag, holder):
    """Run the agent once; return (verdict, answer). Verdicts: ok, malformed, failed, timeout, killed, stopped."""
    codex = meta["agent"] == "codex"
    # A fresh Pi attempt names its session; a forked one gets its id from Pi, and Codex reports its own.
    session = None if codex or meta.get("fork") else str(uuid.uuid4())
    known = set(Path(meta["sessionDir"]).glob("*.jsonl")) if not codex else set()
    command, env = agent_command(meta, session), agent_env(meta)
    convert = filter_codex_event if codex else filter_event
    timed_out, finished = threading.Event(), threading.Event()
    activity = {"last": time.monotonic(), "commands": 0}  # monotonic: a clock step must not end a run
    with open(run / "prompt.md", "rb") as prompt, open(run / "stderr.log", "ab") as stderr, \
            open(run / "events.jsonl", "a", encoding="utf-8") as log:
        if session:
            log.write(json.dumps({"e": "session", "id": session, "attempt": attempt, "at": now_iso()}) + "\n")
        proc = subprocess.Popen(command, cwd=meta["workdir"], stdin=prompt, stdout=subprocess.PIPE,
                                stderr=stderr, env=env, start_new_session=True)
        holder["pgid"] = proc.pid
        (run / "agent.pid").write_text(str(proc.pid))

        def watch():
            """--timeout, not counting time queued in the heavy lane; past it, grace while visibly at work."""
            started, soft = time.monotonic(), meta["timeoutSeconds"]
            grace = setting("TIMEOUT_GRACE", str(DEFAULT_TIMEOUT_GRACE))
            hard = soft * (1 + (int(grace) if grace.isdigit() else DEFAULT_TIMEOUT_GRACE) / 100)
            pause = soft
            # Wake only when the verdict could change: time queued only ever pushes the deadline later.
            while not finished.wait(max(pause, 0.05)):
                spent = time.monotonic() - started - queued_seconds(run)
                if spent < soft:
                    pause = soft - spent
                    continue
                idle = time.monotonic() - activity["last"]
                if spent < hard and (activity["commands"] > 0 or idle < BUSY_WINDOW):
                    holder["graceSeconds"] = round(spent - soft, 1)
                    pause = min(hard - spent, BUSY_WINDOW - idle if not activity["commands"] else hard - spent)
                    continue
                timed_out.set()
                kill_group(proc.pid)
                return

        threading.Thread(target=watch, daemon=True).start()
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
                activity["last"] = time.monotonic()
                if item["e"] in ("bash", "bash_done"):
                    activity["commands"] = max(0, activity["commands"] + (1 if item["e"] == "bash" else -1))
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
        holder.pop("pgid", None)  # reaped: its pid may be reused, so a later stop must not signal it
        finished.set()
        # Pi names a forked session itself (<time>_<id>.jsonl): record the one this attempt created.
        created = sorted(set(Path(meta["sessionDir"]).glob("*.jsonl")) - known) if not codex else []
        if created:
            log.write(json.dumps({"e": "session", "id": created[-1].stem.split("_", 1)[-1], "attempt": attempt,
                                  "at": now_iso()}) + "\n")
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
