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
import os
import re
import shlex
import subprocess
import sys
from pathlib import Path

INVOKE = r"(?:\S*/bin/delegate|\$\{?D\}?)"
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
PRUNE_REASON = (
    "`{call}` immediately deletes unreachable Git objects that uncleaned delegate runs depend on "
    "(needed by diff/apply); pinned refs under `refs/delegate/` make plain `git gc` safe, and "
    "to clear git's `run 'git prune'` notice you can just remove `.git/gc.log` instead.")
GIT_GLOBAL_ARG_OPTS = {"-C", "-c", "--git-dir", "--work-tree", "--namespace",
                       "--super-prefix", "--exec-path", "--config-env", "--list-cmds"}
REPACK_ARG_OPTS = {"--window", "--window-memory", "--depth", "--threads", "--max-pack-size",
                   "--max-cruft-size", "--filter", "--keep-pack", "--geometric", "-g",
                   "--expire-to", "--filter-to"}
PREFIX_KEYWORDS = {"!", "do", "then", "else", "elif", "if", "until", "while", "{",
                   "time", "exec", "command", "sudo"}


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


ASSIGNMENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*=.*")


def at_command_start(tokens: list[str], i: int) -> bool:
    """True when tokens[i] is the command word: `NAME=value` prefixes and `env` don't count."""
    j = i - 1
    while j >= 0 and (tokens[j] == "env" or ASSIGNMENT.fullmatch(tokens[j])):
        j -= 1
    return j < 0 or tokens[j] in COMMAND_START or tokens[j].endswith("$(")


def blocking_calls(command: str) -> tuple[list[str], bool]:
    """Read command/argv positions, never search the reply's message body."""
    try:
        lexer = shlex.shlex(command.replace("\n", " ; "), posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return [], False
    calls = []
    spans = []
    for i, token in enumerate(tokens):
        if token.rsplit("/", 1)[-1] == "ssh" and (not i or tokens[i - 1] in COMMAND_START):
            end = i + 1
            while end < len(tokens) and tokens[end] not in SEPARATORS:
                end += 1
            if end > i + 2:
                nested, _ = blocking_calls(tokens[end - 1])
                if nested:
                    calls.extend(nested)
                    spans.append((i, end))
        if not re.fullmatch(INVOKE, token):
            continue
        if not at_command_start(tokens, i):
            continue
        end = i + 1
        while end < len(tokens) and tokens[end] not in SEPARATORS:
            end += 1
        argv = tokens[i + 1:end]
        if not argv:
            continue
        subcommand, args = argv[0], argv[1:]
        # The second positional of reply begins the message; it is opaque data,
        # including quoted --wait/--help or other delegate command examples.
        if subcommand == "reply":
            positional = 0
            options = []
            takes_value = {"--prompt-file", "--tier", "--model", "--thinking", "--timeout",
                           "--accept", "--accept-also", "--accept-timeout", "--over-limit",
                           "--max-answer", "--evidence", "--evidence-timeout", "--sync"}
            skip = False
            for arg in args:
                if skip:
                    skip = False
                    continue
                if arg.startswith("-") and positional < 2:
                    options.append(arg)
                    skip = arg in takes_value - {"--sync"}
                else:
                    positional += 1
                    if positional == 2:
                        break
            args = options
        if "--help" in args or "-h" in args:
            continue
        if subcommand not in {"wait", "run"} and not (subcommand == "reply" and "--wait" in args):
            continue
        calls.append(token + " " + shlex.join(argv))
        spans.append((i, end))
    mixed = bool(spans and any(t not in SEPARATORS for t in tokens[:spans[0][0]] + tokens[spans[0][1]:]))
    return calls, mixed


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


def _is_immediate_expire(value: str) -> bool:
    return value.strip().lower() in {"now", "all", "0", "0.seconds.ago", "0 seconds ago"}


def _repo_has_delegate(repo_dir: str, git_dir: str | None) -> bool:
    env = {**os.environ, "GIT_OPTIONAL_LOCKS": "0"}
    top: Path | None = None
    common: Path | None = None
    try:
        if git_dir:
            gd = os.path.normpath(os.path.join(repo_dir, os.path.expanduser(git_dir)))
            res = subprocess.run(
                ["git", "--git-dir", gd, "rev-parse", "--path-format=absolute", "--git-common-dir"],
                capture_output=True, text=True, env=env, timeout=2,
            )
            if res.returncode != 0 or not res.stdout.strip():
                return False
            common = Path(res.stdout.strip())
            top_res = subprocess.run(
                ["git", "--git-dir", gd, "rev-parse", "--path-format=absolute", "--show-toplevel"],
                capture_output=True, text=True, env=env, timeout=2,
            )
            if top_res.returncode == 0 and top_res.stdout.strip():
                top = Path(top_res.stdout.strip())
        else:
            d = os.path.normpath(os.path.expanduser(repo_dir))
            if not os.path.isdir(d):
                return False
            res = subprocess.run(
                ["git", "-C", d, "rev-parse", "--path-format=absolute", "--show-toplevel", "--git-common-dir"],
                capture_output=True, text=True, env=env, timeout=2,
            )
            if res.returncode == 0 and len(res.stdout.splitlines()) >= 2:
                lines = res.stdout.splitlines()
                top, common = Path(lines[0]), Path(lines[1])
            else:
                res2 = subprocess.run(
                    ["git", "-C", d, "rev-parse", "--path-format=absolute", "--git-common-dir"],
                    capture_output=True, text=True, env=env, timeout=2,
                )
                if res2.returncode != 0 or not res2.stdout.strip():
                    return False
                common = Path(res2.stdout.strip())
        if top is not None and (top / ".local/run/delegate").exists():
            return True
        if common.name == ".git" and (common.parent / ".local/run/delegate").exists():
            return True
        refs = subprocess.run(
            ["git", "--git-dir", str(common), "for-each-ref", "--count=1", "refs/delegate/"],
            capture_output=True, text=True, env=env, timeout=2,
        )
        return refs.returncode == 0 and bool(refs.stdout.strip())
    except (OSError, subprocess.SubprocessError):
        return False


def _is_dangerous_git(subcmd: str, args: list[str], configs: dict[str, str]) -> bool:
    if subcmd == "help":
        return False
    # Check help flags before "--".
    before_dd = args[:args.index("--")] if "--" in args else args
    if "-h" in before_dd or "--help" in before_dd:
        return False
    if subcmd == "prune":
        for arg in before_dd:
            if arg == "--dry-run" or (arg.startswith("-") and not arg.startswith("--") and "n" in arg[1:]):
                return False
        return True
    if subcmd == "gc":
        prune_mode: str | None = configs.get("gc.pruneexpire")
        i = 0
        while i < len(before_dd):
            arg = before_dd[i]
            if arg == "--no-prune":
                prune_mode = "no"
            elif arg == "--prune":
                if i + 1 < len(before_dd) and _is_immediate_expire(before_dd[i + 1]):
                    prune_mode = before_dd[i + 1]
                    i += 1
                else:
                    prune_mode = "default"
            elif arg.startswith("--prune="):
                prune_mode = arg.split("=", 1)[1]
            i += 1
        return prune_mode is not None and prune_mode != "no" and _is_immediate_expire(prune_mode)
    if subcmd == "reflog":
        if not args:
            return False
        action = None
        rest_start = 0
        for idx, arg in enumerate(args):
            if not arg.startswith("-"):
                action = arg
                rest_start = idx + 1
                break
        if action != "expire":
            return False
        rest = args[rest_start:]
        rest_dd = rest[:rest.index("--")] if "--" in rest else rest
        if "-h" in rest_dd or "--help" in rest_dd:
            return False
        dry_run = False
        expire_unreachable: str | None = configs.get("gc.reflogexpireunreachable")
        i = 0
        while i < len(rest_dd):
            arg = rest_dd[i]
            if arg == "--dry-run" or (arg.startswith("-") and not arg.startswith("--") and "n" in arg[1:]):
                dry_run = True
            elif arg == "--no-dry-run":
                dry_run = False
            elif arg == "--expire-unreachable" and i + 1 < len(rest_dd):
                expire_unreachable = rest_dd[i + 1]
                i += 1
            elif arg.startswith("--expire-unreachable="):
                expire_unreachable = arg.split("=", 1)[1]
            elif arg == "--expire" and i + 1 < len(rest_dd):
                i += 1
            i += 1
        return not dry_run and expire_unreachable is not None and _is_immediate_expire(expire_unreachable)
    if subcmd == "repack":
        pack_all = False
        pack_loosen = False
        delete_redundant = False
        keep_unreachable = False
        cruft = False
        unpack_unreachable: str | None = None
        cruft_expiration: str | None = None
        explicit_prune = False
        i = 0
        while i < len(before_dd):
            arg = before_dd[i]
            if arg == "--keep-unreachable":
                keep_unreachable = True
            elif arg == "--no-keep-unreachable":
                keep_unreachable = False
            elif arg == "--cruft":
                cruft = True
            elif arg == "--no-cruft":
                cruft = False
            elif arg == "--unpack-unreachable" and i + 1 < len(before_dd):
                unpack_unreachable = before_dd[i + 1]
                i += 1
            elif arg.startswith("--unpack-unreachable="):
                unpack_unreachable = arg.split("=", 1)[1]
            elif arg == "--no-unpack-unreachable":
                unpack_unreachable = None
            elif arg == "--cruft-expiration" and i + 1 < len(before_dd):
                cruft_expiration = before_dd[i + 1]
                i += 1
            elif arg.startswith("--cruft-expiration="):
                cruft_expiration = arg.split("=", 1)[1]
            elif arg == "--no-cruft-expiration":
                cruft_expiration = None
            elif arg == "--prune" or arg.startswith("--prune="):
                explicit_prune = True
            elif arg in REPACK_ARG_OPTS and i + 1 < len(before_dd):
                i += 1
            elif arg.startswith("-") and not arg.startswith("--") and len(arg) > 1:
                flags = arg[1:]
                if "a" in flags:
                    pack_all = True
                if "A" in flags:
                    pack_loosen = True
                if "d" in flags:
                    delete_redundant = True
                if "k" in flags:
                    keep_unreachable = True
            i += 1
        if explicit_prune:
            return True
        if not delete_redundant:
            return False
        if pack_all and not keep_unreachable and not cruft:
            return True
        if pack_loosen and unpack_unreachable is not None and _is_immediate_expire(unpack_unreachable):
            return True
        if cruft and cruft_expiration is not None and _is_immediate_expire(cruft_expiration):
            return True
        return False
    return False


def dangerous_git_prune(command: str, base_cwd: str) -> str | None:
    """Return the first Git command that immediately drops unreachable objects in a delegate repo."""
    try:
        lexer = shlex.shlex(command.replace("\n", " ; "), posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return None
    segments: list[list[str]] = []
    current: list[str] = []
    for token in tokens:
        if token in SEPARATORS:
            if current:
                segments.append(current)
                current = []
        else:
            current.append(token)
    if current:
        segments.append(current)
    current_dir = base_cwd
    for seg in segments:
        idx = 0
        env_git_dir: str | None = None
        while idx < len(seg):
            tok = seg[idx]
            if tok in PREFIX_KEYWORDS:
                idx += 1
                continue
            if tok == "env":
                idx += 1
                while idx < len(seg):
                    if seg[idx] in ("-u", "--unset", "-C", "--chdir") and idx + 1 < len(seg):
                        idx += 2
                    elif seg[idx] == "--":
                        idx += 1
                        break
                    elif seg[idx].startswith("-"):
                        idx += 1
                    elif ASSIGNMENT.fullmatch(seg[idx]):
                        if seg[idx].startswith("GIT_DIR="):
                            env_git_dir = seg[idx].split("=", 1)[1]
                        idx += 1
                    else:
                        break
                continue
            if ASSIGNMENT.fullmatch(tok):
                if tok.startswith("GIT_DIR="):
                    env_git_dir = tok.split("=", 1)[1]
                idx += 1
                continue
            break
        if idx >= len(seg):
            continue
        cmd = seg[idx].rsplit("/", 1)[-1]
        if cmd == "cd":
            for arg in seg[idx + 1:]:
                if arg in ("-L", "-P", "--"):
                    continue
                if arg and arg != "-" and not arg.startswith("$"):
                    current_dir = os.path.normpath(os.path.join(current_dir, os.path.expanduser(arg)))
                break
            continue
        if cmd in ("sh", "bash"):
            for k in range(idx + 1, len(seg) - 1):
                if seg[k] in ("-c", "-lc", "-ec", "-xc"):
                    nested = dangerous_git_prune(seg[k + 1], current_dir)
                    if nested:
                        return nested
                    break
            continue
        if cmd != "git":
            continue
        repo_dir = current_dir
        git_dir = env_git_dir
        configs: dict[str, str] = {}
        j = idx + 1
        while j < len(seg) and seg[j].startswith("-"):
            opt = seg[j]
            if opt == "-C" and j + 1 < len(seg):
                repo_dir = os.path.normpath(os.path.join(repo_dir, os.path.expanduser(seg[j + 1])))
                j += 2
            elif opt == "-c" and j + 1 < len(seg):
                if "=" in seg[j + 1]:
                    k, v = seg[j + 1].split("=", 1)
                    configs[k.strip().lower()] = v
                j += 2
            elif opt == "--git-dir" and j + 1 < len(seg):
                git_dir = seg[j + 1]
                j += 2
            elif opt.startswith("--git-dir="):
                git_dir = opt.split("=", 1)[1]
                j += 1
            elif opt in GIT_GLOBAL_ARG_OPTS and j + 1 < len(seg):
                j += 2
            else:
                j += 1
        if j >= len(seg):
            continue
        subcmd = seg[j]
        subargs = seg[j + 1:]
        if _is_dangerous_git(subcmd, subargs, configs) and _repo_has_delegate(repo_dir, git_dir):
            return " ".join(seg[idx:])
    return None


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
    prune_call = dangerous_git_prune(command, str(payload.get("cwd") or os.getcwd()))
    if prune_call:
        return deny(PRUNE_REASON.format(call=prune_call))
    if tool_input.get("run_in_background"):
        return 0
    calls, mixed = blocking_calls(command)
    if not calls:
        return 0
    wait_command = calls[0]
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
