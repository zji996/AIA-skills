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
POLL_REASON = (
    "不要在前台用 while/until + sleep 轮询 delegate 输出或 run 目录；请改用 "
    "`delegate wait --any` 配合 Bash 工具的 `run_in_background: true` 放后台等完成通知。"
    " Do not poll delegate output or run directories in a foreground while/until sleep loop; "
    "run `delegate wait --any` with `run_in_background: true` to be notified on completion.")
AGY_POLL_REASON = (
    "不要在前台用 while/until + sleep 轮询 delegate 输出或 run 目录；请改用 "
    "`delegate wait --any` 配合 `run_command` 的 `WaitMsBeforeAsync: 500, IsDaemon: false` 放后台等自动唤醒。"
    " Do not poll delegate output or run directories in a foreground while/until sleep loop; "
    "run `delegate wait --any` alone with `run_command` (`WaitMsBeforeAsync: 500, IsDaemon: false`) "
    "so Reactive Wakeup notifies you on completion.")
AGY_MAX_BG_WAIT_MS = 2000
DELEGATE_RUN_ARTIFACTS = (
    "exit_code", "result.md", "summary.json", "events.jsonl",
    "meta.json", "changes.json", ".waiting", ".delivered", "accept.log",
)
RUN_ID_RE = re.compile(r"(?:^|/)20\d{6}-\d{6}-")
VAR_REF_RE = re.compile(r"\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?")
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
    if spans:
        before_tokens = [t for t in tokens[:spans[0][0]] if t not in SEPARATORS]
        after_tokens = [t for t in tokens[spans[0][1]:] if t not in SEPARATORS]
        only_env_prefix = all(t == "env" or ASSIGNMENT.fullmatch(t) for t in before_tokens)
        mixed = bool(after_tokens or not only_env_prefix)
    else:
        mixed = False
    return calls, mixed


def allow(fmt: str = "claude") -> int:
    if fmt == "agy":
        json.dump({"decision": "allow"}, sys.stdout, ensure_ascii=False)
    return 0


def deny(reason: str, fmt: str = "claude") -> int:
    if fmt == "agy":
        json.dump({"decision": "deny", "reason": reason}, sys.stdout, ensure_ascii=False)
        return 0
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


def _is_separator_end(token: str) -> bool:
    return token in COMMAND_START or token.endswith("$(") or token.endswith(";")


def _token_at_start(tokens: list[str], i: int) -> bool:
    j = i - 1
    while j >= 0 and (tokens[j] == "env" or ASSIGNMENT.fullmatch(tokens[j])):
        j -= 1
    return j < 0 or _is_separator_end(tokens[j])


def _is_bounded_retry_loop(loop_tokens: list[str]) -> bool:
    """True when a while/until loop has both a counter update and a numeric bound."""
    has_bound = any(tok in {"-lt", "-le", "-gt", "-ge"} for tok in loop_tokens)
    has_update = False
    in_arith = False
    for idx, tok in enumerate(loop_tokens):
        if tok in ("((", "$((") or tok.endswith("(("):
            in_arith = True
            continue
        if in_arith:
            if any(op in tok for op in ("<", ">", "<=", ">=")):
                has_bound = True
            if any(op in tok for op in ("++", "--", "+=", "-=", "+", "-")):
                has_update = True
            if tok.startswith("))") or "))" in tok:
                in_arith = False
            continue
        if tok.rsplit("/", 1)[-1] in ("let", "expr") and _token_at_start(loop_tokens, idx):
            has_update = True
        if any(op in tok for op in ("++", "--", "+=", "-=")):
            has_update = True
        if tok.endswith("=$") and idx + 1 < len(loop_tokens) and loop_tokens[idx + 1] == "((":
            has_update = True
    return has_bound and has_update


def _targets_delegate(loop_tokens: list[str], env_vars: dict[str, str]) -> bool:
    """True when the loop inspects delegate output or a delegate run directory."""
    expanded = list(loop_tokens)
    for tok in loop_tokens:
        for var in VAR_REF_RE.findall(tok):
            if var in env_vars:
                expanded.append(env_vars[var])
    for idx, tok in enumerate(loop_tokens):
        if _token_at_start(loop_tokens, idx) and (
            re.fullmatch(INVOKE, tok) or tok == "delegate"
        ):
            return True
    has_run_id = False
    has_delegate_word = False
    has_artifact = False
    for tok in expanded:
        if any(marker in tok for marker in (
            ".local/run/delegate",
            ".local/run/pi",
            "DELEGATE_RUNS",
            ".cache/delegate",
            "supervisor.lock",
            "HANDOFF.md",
            "/delegate/",
        )):
            return True
        if RUN_ID_RE.search(tok):
            has_run_id = True
        if "delegate" in tok.lower():
            has_delegate_word = True
        if any(tok == art or tok.endswith("/" + art) or art in tok for art in DELEGATE_RUN_ARTIFACTS):
            has_artifact = True
    return has_artifact and (has_run_id or has_delegate_word)


def foreground_delegate_poll(command: str) -> str | None:
    """Return the first foreground while/until + sleep loop polling delegate output or run dirs."""
    try:
        lexer = shlex.shlex(command.replace("\n", " ; "), posix=True, punctuation_chars=True)
        lexer.whitespace_split = True
        tokens = list(lexer)
    except ValueError:
        return None
    env_vars: dict[str, str] = {}
    i = 0
    while i < len(tokens):
        tok = tokens[i]
        if _token_at_start(tokens, i) and ASSIGNMENT.fullmatch(tok):
            k, v = tok.split("=", 1)
            env_vars[k] = v
            i += 1
            continue
        if _token_at_start(tokens, i) and tok.rsplit("/", 1)[-1] in ("sh", "bash"):
            j = i + 1
            while j < len(tokens) and tokens[j] not in SEPARATORS:
                if tokens[j] in ("-c", "-lc", "-ec", "-xc") and j + 1 < len(tokens):
                    nested = foreground_delegate_poll(tokens[j + 1])
                    if nested:
                        return nested
                    break
                j += 1
            i += 1
            continue
        if tok in ("while", "until") and _token_at_start(tokens, i):
            depth = 1
            do_idx = None
            done_idx = None
            j = i + 1
            while j < len(tokens):
                t = tokens[j]
                if t in ("while", "until", "for", "select") and _token_at_start(tokens, j):
                    depth += 1
                elif t == "do" and depth == 1 and do_idx is None and _token_at_start(tokens, j):
                    do_idx = j
                elif t == "done" and _token_at_start(tokens, j):
                    depth -= 1
                    if depth == 0:
                        done_idx = j
                        break
                j += 1
            if do_idx is not None and done_idx is not None:
                loop_tokens = tokens[i:done_idx + 1]
                inner_tokens = tokens[i + 1:done_idx]
                has_sleep = any(
                    bt.rsplit("/", 1)[-1] == "sleep" and _token_at_start(inner_tokens, k)
                    for k, bt in enumerate(inner_tokens)
                )
                if (
                    has_sleep
                    and not _is_bounded_retry_loop(loop_tokens)
                    and _targets_delegate(loop_tokens, env_vars)
                ):
                    return " ".join(loop_tokens[:12])
        i += 1
    return None


def _parse_format(argv: list[str]) -> str | None:
    i = 0
    while i < len(argv):
        arg = argv[i]
        if arg == "--format" and i + 1 < len(argv):
            val = argv[i + 1].strip().lower()
            if val in ("claude", "agy"):
                return val
            i += 2
            continue
        if arg.startswith("--format="):
            val = arg.split("=", 1)[1].strip().lower()
            if val in ("claude", "agy"):
                return val
        i += 1
    return None


def _is_colleague_process() -> bool:
    return bool(
        os.environ.get("PI_DELEGATE_ACTIVE", "").strip()
        or os.environ.get("DELEGATE_AGENT", "").strip()
    )


def _parse_int(value: object) -> int | None:
    if isinstance(value, bool) or value is None:
        return None
    if isinstance(value, (int, float)):
        return int(value)
    if isinstance(value, str):
        try:
            return int(float(value.strip()))
        except ValueError:
            return None
    return None


BUILD_WORDS = {"cargo", "pnpm", "npm", "npx", "yarn", "xtask", "make", "vitest", "playwright"}
CAPPED_BUILD_REASON = (
    "`{call}` 给编译/测试命令套了前台 timeout。cargo、pnpm 这类命令可能要排队等别的构建释放锁，"
    "要等多久无法预估，时限一到就杀掉一个健康的等待，只能重跑。去掉 timeout，用 Bash 的 "
    "run_in_background: true 运行（结束会通知），重的构建用 `delegate lane '<命令>'` 排队。 / "
    "`{call}` wraps a build/test command in a foreground timeout. cargo/pnpm may queue behind another "
    "build's lock for an unpredictable time; the timeout kills a healthy wait and forces a rerun. Drop "
    "the timeout and run it with run_in_background: true (you are notified on exit); queue heavy builds "
    "with `delegate lane '<cmd>'`.")
_SEGMENT_SPLIT = re.compile(r"(?:&&|\|\||[;|&(\n])")
_ASSIGN = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")


def capped_build(command: str) -> str | None:
    """Return the first foreground `timeout N <build>` call (cargo, pnpm, xtask, delegate lane, ...)."""
    for segment in _SEGMENT_SPLIT.split(command):
        try:
            tokens = shlex.split(segment, comments=True)
        except ValueError:
            tokens = segment.split()
        i = 0
        while i < len(tokens) and (_ASSIGN.match(tokens[i]) or tokens[i] in ("env", "nice", "time", "command")):
            i += 1
        if i >= len(tokens) or tokens[i].rsplit("/", 1)[-1] != "timeout":
            continue
        j = i + 1
        while j < len(tokens) and tokens[j].startswith("-"):
            j += 2 if tokens[j] in ("-s", "-k", "--signal", "--kill-after") else 1
        j += 1  # the duration
        while j < len(tokens) and (_ASSIGN.match(tokens[j]) or tokens[j] == "env"):
            j += 1
        if j >= len(tokens):
            continue
        word = tokens[j].rsplit("/", 1)[-1]
        nxt = tokens[j + 1] if j + 1 < len(tokens) else ""
        if word in BUILD_WORDS or (word in ("delegate", "$D", "${D}") and nxt == "lane") \
                or (word.startswith("python") and nxt.endswith(("gate.sh", "check.py"))):
            return " ".join(tokens[i:j + 2])
    return None


def main(argv: list[str] | None = None) -> int:
    cli_fmt = _parse_format(sys.argv[1:] if argv is None else argv)
    try:
        payload = json.load(sys.stdin)
    except ValueError:
        return allow(cli_fmt or "claude")
    if not isinstance(payload, dict):
        return allow(cli_fmt or "claude")
    fmt = cli_fmt or ("agy" if isinstance(payload.get("toolCall"), dict) else "claude")
    if fmt == "agy":
        tool_call = payload.get("toolCall")
        if not isinstance(tool_call, dict) or tool_call.get("name") != "run_command":
            return allow(fmt)
        tool_args = tool_call.get("args")
        if not isinstance(tool_args, dict):
            tool_args = {}
        raw_command = str(tool_args.get("CommandLine", ""))
        wait_ms = _parse_int(tool_args.get("WaitMsBeforeAsync"))
        is_daemon = bool(tool_args.get("IsDaemon"))
        is_bg = wait_ms is not None and wait_ms <= AGY_MAX_BG_WAIT_MS and not is_daemon
    else:
        tool_input = payload.get("tool_input") or {}
        if payload.get("tool_name") != "Bash":
            return allow(fmt)
        raw_command = str(tool_input.get("command", ""))
        wait_ms = None
        is_daemon = False
        is_bg = bool(tool_input.get("run_in_background"))
    command = strip_heredocs(raw_command)
    call = self_matching_pgrep(command)
    if call:
        return deny(SELF_MATCH_REASON.format(call=call), fmt)
    prune_call = dangerous_git_prune(command, str(payload.get("cwd") or os.getcwd()))
    if prune_call:
        return deny(PRUNE_REASON.format(call=prune_call), fmt)
    if _is_colleague_process():
        return allow(fmt)
    if fmt == "agy":
        calls, mixed = blocking_calls(command)
        if calls:
            wait_command = calls[0]
            if mixed:
                return deny(
                    f"把 wait 单独作为一条后台命令发出：{wait_command}"
                    "（run_command 设 WaitMsBeforeAsync: 500, IsDaemon: false）；其余部分另发一条。",
                    fmt,
                )
            if not is_bg:
                return deny(
                    "delegate wait/run/reply --wait blocks until colleagues finish. Rerun the same "
                    "command alone with run_command WaitMsBeforeAsync: 500, IsDaemon: false; the "
                    "system wakes you automatically when it exits (Reactive Wakeup), or after 50 "
                    "minutes with a progress glance. Use start (or reply without --wait) to launch, "
                    "then one background wait.",
                    fmt,
                )
            return allow(fmt)
        if not is_bg:
            poll_call = foreground_delegate_poll(command)
            if poll_call:
                return deny(AGY_POLL_REASON, fmt)
        return allow(fmt)
    if is_bg:
        return allow(fmt)
    capped = capped_build(command)
    if capped:
        return deny(CAPPED_BUILD_REASON.format(call=capped), fmt)
    poll_call = foreground_delegate_poll(command)
    if poll_call:
        return deny(POLL_REASON, fmt)
    calls, mixed = blocking_calls(command)
    if not calls:
        return allow(fmt)
    wait_command = calls[0]
    reason = (f"把 wait 单独作为一条后台命令发出：{wait_command}；其余部分另发一条。"
              if mixed else
              "delegate wait/run/reply --wait blocks until colleagues finish. Rerun the same "
              "command with the Bash tool's run_in_background: true; you are notified when it "
              "exits, so keep working meanwhile; it returns on completion or after 50 minutes with a progress glance, then rerun the printed command. Use start (or reply without "
              "--wait) to launch, then one background wait. To look at each run as it finishes, "
              "run `wait --stream` under the Monitor tool (one notification per run), or run "
              "`wait --any` in the background and repeat it after each notification.")
    return deny(reason, fmt)


if __name__ == "__main__":
    sys.exit(main())
