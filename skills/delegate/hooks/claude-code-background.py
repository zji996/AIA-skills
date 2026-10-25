#!/usr/bin/env python3
"""Claude Code PreToolUse hook: wait for background work the way the harness supports.

`delegate wait`, `delegate run` and `delegate reply --wait` block until colleagues finish,
often for many minutes. In the foreground they hold the conversation; with the Bash tool's
run_in_background the harness notifies on completion instead. Only real invocations are
matched (a path ending in /bin/delegate, or $D / ${D} as in SKILL.md) outside here-document
bodies, so a commit message or a file being written that mentions them is left alone.

The Bash tool runs each command as `bash -c "... eval '<command>'"`, so an unanchored
`pgrep -f PATTERN` always finds that shell: a wait loop never ends, and `pkill -f` kills the
caller. A `[x]` bracket does not help, because the pattern text appears elsewhere in the line.
Anchored (`^`) and exact (`-x`) patterns are allowed. This rule covers background commands too.
"""

from __future__ import annotations

import json
import re
import shlex
import sys

INVOKE = r"(?:\S*/bin/delegate|\$\{?D\}?)"
BLOCKING = re.compile(rf"{INVOKE}\s+(?:wait|run)\b|{INVOKE}\s+reply\b[^\n;&|]*--wait\b")
HELP = re.compile(r"^[^;&|]*\s(?:--help|-h)\b")
HEREDOC = re.compile(r"<<-?\s*(['\"]?)(\w+)\1")
SEPARATORS = {";", "&&", "||", "|", "&", "(", ")", "|&", ";;"}
COMMAND_START = SEPARATORS | {"!", "do", "then", "else", "elif", "if", "until", "while", "{",
                              "time", "sudo", "exec", "xargs", "watch", "timeout", "$("}
ARG_OPTIONS = {"-u", "-U", "-g", "-G", "-P", "-s", "-t", "-F", "-d", "--uid", "--euid", "--group",
               "--pgroup", "--parent", "--session", "--terminal", "--pidfile", "--delimiter",
               "--signal", "--ns", "--nslist", "--older", "--cgroup"}
SELF_MATCH_REASON = (
    "`{call}` also matches the shell running this command: its command line contains the "
    "pattern, so a wait loop never ends and pkill kills its own shell. Anchor the pattern to "
    "the program (`pgrep -f '^python3 .*serve.py'`), or match the process name without -f "
    "(`pgrep -x cargo`). To wait for a background command, do not poll: its completion "
    "notification arrives by itself; otherwise wait on a PID (`tail --pid=$pid -f /dev/null`) "
    "or a marker line in a log file.")


def strip_heredocs(command: str) -> str:
    """Drop here-document bodies: they are data (a prompt, a file being written), not commands."""
    lines, out, ends = command.split("\n"), [], []
    for line in lines:
        if ends:
            if line.strip() == ends[0]:
                ends.pop(0)
            continue
        out.append(line)
        ends.extend(match.group(2) for match in HEREDOC.finditer(line))
    return "\n".join(out)


def self_matching_pgrep(command: str) -> str | None:
    """Return the first `pgrep -f` / `pkill -f` call whose pattern would match its own shell."""
    try:
        lexer = shlex.shlex(command.replace("\n", " ; "), posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return None
    for i, token in enumerate(tokens):
        if token.rsplit("/", 1)[-1] not in ("pgrep", "pkill"):
            continue
        if i and tokens[i - 1] not in COMMAND_START and not tokens[i - 1].endswith("$("):
            continue
        full = exact = False
        pattern = None
        j = i + 1
        while j < len(tokens) and tokens[j] not in SEPARATORS:
            arg = tokens[j]
            if arg == "--":
                pattern = tokens[j + 1] if j + 1 < len(tokens) else None
                break
            if arg.startswith("--"):
                name = arg.split("=", 1)[0]
                full |= name == "--full"
                exact |= name == "--exact"
                j += 2 if name in ARG_OPTIONS and "=" not in arg else 1
            elif arg.startswith("-") and len(arg) > 1:
                full |= "f" in arg[1:]
                exact |= "x" in arg[1:]
                j += 2 if arg in ARG_OPTIONS else 1
            else:
                pattern = arg
                break
        if full and not exact and pattern is not None and not pattern.startswith("^"):
            return " ".join(tokens[i:j + 1])
    return None


def deny(reason: str) -> int:
    json.dump(
        {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": reason,
            }
        },
        sys.stdout,
    )
    return 0


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except ValueError:
        return 0
    tool_input = payload.get("tool_input") or {}
    if payload.get("tool_name") != "Bash":
        return 0
    command = strip_heredocs(str(tool_input.get("command", "")))
    call = self_matching_pgrep(command)
    if call:
        return deny(SELF_MATCH_REASON.format(call=call))
    if tool_input.get("run_in_background"):
        return 0
    # Asking for help does not block.
    matches = [m for m in BLOCKING.finditer(command)
               if not HELP.search(command[m.start():].split("\n", 1)[0])]
    if not matches:
        return 0
    wait_command = command[matches[0].start():].split("\n", 1)[0]
    wait_command = re.split(r"\s*(?:&&|\|\||[;&|])\s*", wait_command, maxsplit=1)[0].strip()
    rest = command[:matches[0].start()] + command[matches[0].start() + len(wait_command):]
    mixed = bool(rest.strip(" \t\r\n;&|"))
    reason = (f"把 wait 单独作为一条后台命令发出：{wait_command}；其余部分另发一条。"
              if mixed else
              "delegate wait/run/reply --wait blocks until colleagues finish. Rerun the same "
              "command with the Bash tool's run_in_background: true; you are notified when it "
              "exits, so keep working meanwhile; it returns on completion or after 50 minutes with a progress glance, then rerun the printed command. Use start (or reply without "
              "--wait) to launch, then one background wait. To look at each run as it finishes, "
              "run `wait --stream` under the Monitor tool (one notification per run), or run "
              "`wait --any` in the background and repeat it after each notification.")
    return deny(reason)


if __name__ == "__main__":
    sys.exit(main())
