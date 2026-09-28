#!/usr/bin/env python3
"""Read-only audit of a repository's agent-facing context files."""

import argparse
import importlib.util
import json
import re
import subprocess
import sys
import time
from pathlib import Path
from urllib.parse import unquote

ENTRY_FILES = ("AGENTS.md", "CLAUDE.md")
KINDS = ("entry", "current", "budget", "names", "links", "gitignore", "adr-index")
DEFAULTS = {"maxDatedItems": 5, "maxNextActions": 5, "staleDays": 30}
CJK_RANGES = ((0x2E80, 0x9FFF), (0xAC00, 0xD7AF), (0xF900, 0xFAFF),
              (0xFF00, 0xFFEF), (0x3000, 0x303F))
NEXT_HEADING = re.compile(r"^#{1,6}\s*.*(下一步|next\s*actions?|next\s*steps?)", re.I)
HEADING = re.compile(r"^#{1,6}\s")
LIST_ITEM = re.compile(r"^\s{0,3}(?:[-*+]|\d+[.)])\s+\S")
LINK = re.compile(r"!?\[[^\]]*\]\(([^\s)]+)(?:\s+[^)]*)?\)")
DATED_ITEM = re.compile(r"^\s{0,3}(?:[-*+]|\d+[.)])\s+\**\d{4}-\d{2}-\d{2}")
FENCE = re.compile(r"^\s*(```|~~~)")
INLINE_CODE = re.compile(r"`([^`\n]+)`")
PATHLIKE = re.compile(r"^\.?[\w.-]+(?:/[\w.@-]+)+/?$")  # bare file names often live outside the repo
MAKE_CALL = re.compile(r"\bmake\s+((?:[A-Z_]+=\S+\s+)*)([a-z][\w.-]*)(\*?)")
MAKE_TARGET = re.compile(r"^([A-Za-z0-9_.-]+(?:\s+[A-Za-z0-9_.-]+)*)\s*:(?!=)", re.M)


def estimate_tokens(text):
    """Estimate a reading budget, not an exact count for any model."""
    cjk = sum(any(start <= ord(char) <= end for start, end in CJK_RANGES) for char in text)
    return cjk + (len(text) - cjk + 3) // 4


def load_config(repo):
    path = repo / ".repo-governance.json"
    if not path.is_file():
        return {}
    try:
        config = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, UnicodeError, json.JSONDecodeError) as exc:
        raise ValueError(f"{path}: invalid JSON: {exc}") from exc
    if not isinstance(config, dict):
        raise ValueError(f"{path}: expected a JSON object")
    budgets = config.get("budgets", {})
    if not isinstance(budgets, dict):
        raise ValueError(f"{path}: budgets must be an object")
    for pattern, limit in budgets.items():
        if not isinstance(pattern, str) or not pattern or Path(pattern).is_absolute() or ".." in Path(pattern).parts:
            raise ValueError(f"{path}: budget pattern must be a relative path without '..': {pattern!r}")
        if type(limit) is not int or limit <= 0:
            raise ValueError(f"{path}: budget for {pattern!r} must be a positive integer")
    for key in DEFAULTS:
        if key in config and (type(config[key]) is not int or config[key] < 0):
            raise ValueError(f"{path}: {key} must be a nonnegative integer")
    return config


def budgeted_files(repo, config, entry_limit, current_limit):
    budgets = {}
    seen = set()
    for name in ENTRY_FILES:
        path = repo / name
        # CLAUDE.md is often a symlink to AGENTS.md; count one file once.
        if path.is_file() and path.resolve() not in seen:
            seen.add(path.resolve())
            budgets[name] = ("entry", 5000)
    if (repo / "docs/current.md").is_file():
        budgets["docs/current.md"] = ("current", 2500)
    # Exact paths override globs whatever their order in the file, so a per-file ceiling is never lost.
    patterns = sorted(config.get("budgets", {}).items(), key=lambda item: not any(c in item[0] for c in "*?["))
    for pattern, limit in patterns:
        for path in repo.glob(pattern):
            if path.is_file() and repo in path.resolve().parents:
                name = path.relative_to(repo).as_posix()
                kind = budgets[name][0] if name in budgets else "budget"
                budgets[name] = (kind, limit)
    for name, (kind, limit) in budgets.items():
        if kind == "entry" and entry_limit is not None:
            limit = entry_limit
        elif kind == "current" and current_limit is not None:
            limit = current_limit
        yield name, kind, estimate_tokens((repo / name).read_text(encoding="utf-8", errors="replace")), limit


def selected_kinds(parser, value, option):
    kinds = set(KINDS) if value == "all" else {kind.strip() for kind in value.split(",") if kind.strip()}
    unknown = kinds - set(KINDS)
    if not kinds or unknown:
        parser.error(f"invalid {option} kinds: {', '.join(sorted(unknown)) or value}")
    return kinds


def git(repo, *args):
    result = subprocess.run(["git", "-C", str(repo), *args], capture_output=True, text=True)
    return result.stdout.strip() if result.returncode == 0 else None


def last_change_epoch(repo, path, in_git):
    if in_git:
        stamp = git(repo, "log", "-1", "--format=%ct", "--", str(path.relative_to(repo)))
        if stamp:
            return int(stamp)
    return int(path.stat().st_mtime)


def count_next_actions(text):
    count, inside = 0, False
    for line in text.splitlines():
        if HEADING.match(line):
            inside = bool(NEXT_HEADING.match(line))
        elif inside and LIST_ITEM.match(line):
            count += 1
    return count


def split_fences(text):
    """Return (prose, code) text: inline code is checked in prose, commands also in fenced blocks."""
    prose, code, inside = [], [], False
    for line in text.splitlines():
        if FENCE.match(line):
            inside = not inside
        else:
            (code if inside else prose).append(line)
    return "\n".join(prose), "\n".join(code)


def missing_paths(repo, base, text, in_git):
    tracked = git(repo, "ls-files", "--cached", "--others", "--exclude-standard").splitlines() if in_git else []
    dirs = {str(Path(f).parent) for f in tracked}
    for token in sorted(set(INLINE_CODE.findall(text))):
        token = token.strip()
        if not PATHLIKE.match(token) or token.startswith(("~", "/")) or "..." in token \
                or not re.search(r"[A-Za-z]", token):
            continue
        path = token.rstrip("/")
        if (repo / path).exists() or (base / path).exists():
            continue
        if not Path(path).suffix and not any((root / path.split("/")[0]).exists() for root in (repo, base)):
            continue  # a directory-like spelling outside the known tree is more likely prose (e.g. async/await)
        if in_git and git(repo, "check-ignore", "-q", path) is not None:
            continue  # local or generated; absent on a clean checkout by design
        # Module-relative spellings such as pkg/sub are fine when some tracked path ends with them.
        if any(f.endswith("/" + path) for f in tracked) or any(d.endswith("/" + path) for d in dirs):
            continue
        yield token


def make_targets(repo):
    makefile = repo / "Makefile"
    if not makefile.is_file():
        return None
    targets = set()
    for names in MAKE_TARGET.findall(makefile.read_text(encoding="utf-8", errors="replace")):
        targets.update(names.split())
    return targets


def load_adr_index():
    spec = importlib.util.spec_from_file_location("adr_index", Path(__file__).with_name("adr-index.py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def markdown_files(repo):
    files = [repo / name for name in ENTRY_FILES if (repo / name).is_file()]
    docs = repo / "docs"
    if docs.is_dir():
        files += sorted(docs.rglob("*.md"))
    return files


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=".", help="repository root (default: git root of cwd)")
    parser.add_argument("--max-entry-tokens", type=int)
    parser.add_argument("--max-current-tokens", type=int)
    parser.add_argument("--max-agents-lines", type=int, help="deprecated: use --max-entry-tokens")
    parser.add_argument("--max-current-kb", type=int, help="deprecated: use --max-current-tokens")
    parser.add_argument("--stale-days", type=int)
    parser.add_argument("--max-dated-items", type=int)
    parser.add_argument("--report", action="store_true", help="show token budgets and exit 0")
    parser.add_argument("--only", default="all", help=f"run only these comma-separated kinds: {','.join(KINDS)}")
    parser.add_argument("--fail-on", default="all", help="comma-separated kinds that set exit 1: "
                        f"{','.join(KINDS)} (default: all; others are still printed)")
    args = parser.parse_args()
    for flag, limit in (("--max-entry-tokens", args.max_entry_tokens),
                        ("--max-current-tokens", args.max_current_tokens)):
        if limit is not None and limit <= 0:
            parser.error(f"{flag} must be positive")

    repo = Path(args.repo).resolve()
    if not repo.is_dir():
        parser.error(f"not a directory: {repo}")
    top = git(repo, "rev-parse", "--show-toplevel")
    in_git = top is not None
    if in_git:
        repo = Path(top)

    try:
        config = load_config(repo)
        budget_rows = list(budgeted_files(repo, config, args.max_entry_tokens, args.max_current_tokens))
    except (ValueError, OSError) as exc:
        print(f"ERROR {exc}", file=sys.stderr)
        return 2
    only = selected_kinds(parser, args.only, "--only")
    fail_on = selected_kinds(parser, args.fail_on, "--fail-on")
    if args.report:
        for name, _kind, tokens, limit in sorted(budget_rows, key=lambda row: (-row[2] / row[3], row[0])):
            print(f"{name}: ~{tokens} tokens / {limit} ({tokens / limit:.0%})")
        baseline = sum(tokens for _name, kind, tokens, _limit in budget_rows if kind in ("entry", "current"))
        print(f"every-session baseline: ~{baseline} tokens (entry + current)")
        return 0

    for flag, value in (("--max-agents-lines", args.max_agents_lines),
                        ("--max-current-kb", args.max_current_kb)):
        if value is not None:
            print(f"DEPRECATED {flag}: use token budgets instead", file=sys.stderr)
    stale_days = args.stale_days if args.stale_days is not None else config.get("staleDays", DEFAULTS["staleDays"])
    max_dated = (args.max_dated_items if args.max_dated_items is not None
                 else config.get("maxDatedItems", DEFAULTS["maxDatedItems"]))
    max_actions = config.get("maxNextActions", DEFAULTS["maxNextActions"])

    findings = []

    def warn(kind, path, message):
        if kind in only:
            findings.append((kind, f"WARN {path}: {message}"))

    for name, kind, tokens, limit in budget_rows:
        if tokens > limit:
            advice = ("move stable details into docs/reference/" if kind == "entry" else
                      "keep only what the next step needs" if kind == "current" else
                      "split or move details into references/")
            warn(kind, name, f"~{tokens} tokens (> {limit}); {advice}")

    entries = [repo / name for name in ENTRY_FILES if (repo / name).is_file()]
    if "entry" in only and not entries:
        warn("entry", "AGENTS.md", "missing; agents start without project commands or boundaries")
    if "entry" in only and args.max_agents_lines is not None:
        for entry in entries:
            lines = len(entry.read_text(encoding="utf-8", errors="replace").splitlines())
            if lines > args.max_agents_lines:
                warn("entry", entry.name, f"{lines} lines (> {args.max_agents_lines}); move stable details into docs/reference/")

    current = repo / "docs/current.md"
    if "current" in only and current.is_file():
        text = current.read_text(encoding="utf-8", errors="replace")
        actions = count_next_actions(text)
        if actions > max_actions:
            warn("current", "docs/current.md", f"{actions} next actions (> {max_actions}); archive finished items or split the focus")
        age_days = (time.time() - last_change_epoch(repo, current, in_git)) // 86400
        if age_days > stale_days:
            warn("current", "docs/current.md", f"last changed {int(age_days)} days ago; confirm it still reflects the work")
        if args.max_current_kb is not None:
            size_kb = len(text.encode("utf-8")) / 1024
            if size_kb > args.max_current_kb:
                warn("current", "docs/current.md", f"{size_kb:.0f} KB (> {args.max_current_kb}); every session reads it, "
                     "keep only what the next step needs")
        dated = sum(1 for line in text.splitlines() if DATED_ITEM.match(line))
        if dated > max_dated:
            warn("current", "docs/current.md", f"{dated} dated entries (> {max_dated}); shipped work is recorded in "
                 "git, keep only open state (unverified, not yet live, known risks)")

    if "names" in only:
        targets = make_targets(repo)
        for doc in [*entries, *([current] if current.is_file() else [])]:
            prose, code = split_fences(doc.read_text(encoding="utf-8", errors="replace"))
            for token in missing_paths(repo, doc.parent, prose, in_git):
                warn("names", doc.relative_to(repo), f"names `{token}`, which does not exist")
            if targets is not None:
                called = {m.group(2) for m in MAKE_CALL.finditer(prose + "\n" + code) if not m.group(3)}
                for target in sorted(called - targets):
                    warn("names", doc.relative_to(repo), f"names `make {target}`, which the Makefile does not define")

    if "gitignore" in only and in_git and git(repo, "check-ignore", "-q", ".local/probe") is None:
        warn("gitignore", ".gitignore", ".local/ is not ignored; drafts and run artifacts may be committed")

    if "links" in only:
        for doc in markdown_files(repo):
            text = doc.read_text(encoding="utf-8", errors="replace")
            for target in LINK.findall(text):
                if target.startswith(("http://", "https://", "mailto:", "#")):
                    continue
                path = unquote(target.split("#", 1)[0])
                if path and not (doc.parent / path).exists():
                    warn("links", doc.relative_to(repo), f"broken link: {target}")

    if "adr-index" in only:
        stale_index = load_adr_index().check(repo)
        if stale_index:
            warn("adr-index", stale_index[0].relative_to(repo), "out of date with the ADR status lines; run adr-index.py --write")

    for _, line in findings:
        print(line)
    print(f"{len(findings)} finding(s) in {repo}" if findings else f"ok: {repo}")
    return 1 if any(kind in fail_on for kind, _ in findings) else 0


if __name__ == "__main__":
    sys.exit(main())
