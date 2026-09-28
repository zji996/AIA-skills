#!/usr/bin/env python3
"""Claude Code PreToolUse hook: blocking delegate calls must run in the background.

`delegate wait`, `delegate run` and `delegate reply --wait` block until colleagues finish,
often for many minutes. In the foreground they hold the conversation; with the Bash tool's
run_in_background the harness notifies on completion instead. Only real invocations are
matched (a path ending in /bin/delegate, or $D / ${D} as in SKILL.md) outside here-document
bodies, so a commit message or a file being written that mentions them is left alone.
"""

import json
import re
import sys

INVOKE = r"(?:\S*/bin/delegate|\$\{?D\}?)"
BLOCKING = re.compile(rf"{INVOKE}\s+(?:wait|run)\b|{INVOKE}\s+reply\b[^\n;&|]*--wait\b")
HELP = re.compile(r"^[^;&|]*\s(?:--help|-h)\b")
HEREDOC = re.compile(r"<<-?\s*(['\"]?)(\w+)\1")


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


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except ValueError:
        return 0
    tool_input = payload.get("tool_input") or {}
    if payload.get("tool_name") != "Bash" or tool_input.get("run_in_background"):
        return 0
    command = strip_heredocs(str(tool_input.get("command", "")))
    # Asking for help does not block.
    if not any(not HELP.search(command[m.start():].split("\n", 1)[0]) for m in BLOCKING.finditer(command)):
        return 0
    json.dump(
        {
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": (
                    "delegate wait/run/reply --wait blocks until colleagues finish. Rerun the same "
                    "command with the Bash tool's run_in_background: true; you are notified when it "
                    "exits, so keep working meanwhile and do not poll. Use start (or reply without "
                    "--wait) to launch, then one background wait. To look at each run as it finishes, "
                    "run `wait --stream` under the Monitor tool (one notification per run), or run "
                    "`wait --any` in the background and repeat it after each notification."
                ),
            }
        },
        sys.stdout,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
