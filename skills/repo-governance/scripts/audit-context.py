#!/usr/bin/env python3
"""Read-only audit of a repository's agent-facing context files."""

import argparse
import re
import subprocess
import sys
import time
from pathlib import Path
from urllib.parse import unquote

ENTRY_FILES = ("AGENTS.md", "CLAUDE.md")
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


def markdown_files(repo):
    files = [repo / name for name in ENTRY_FILES if (repo / name).is_file()]
    docs = repo / "docs"
    if docs.is_dir():
        files += sorted(docs.rglob("*.md"))
    return files


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=".", help="repository root (default: git root of cwd)")
    parser.add_argument("--max-agents-lines", type=int, default=150)
    parser.add_argument("--stale-days", type=int, default=30)
    parser.add_argument("--max-current-kb", type=int, default=8)
    parser.add_argument("--max-dated-items", type=int, default=5)
    args = parser.parse_args()

    repo = Path(args.repo).resolve()
    if not repo.is_dir():
        parser.error(f"not a directory: {repo}")
    top = git(repo, "rev-parse", "--show-toplevel")
    in_git = top is not None
    if in_git:
        repo = Path(top)

    findings = []

    def warn(path, message):
        findings.append(f"WARN {path}: {message}")

    entries = [repo / name for name in ENTRY_FILES if (repo / name).is_file()]
    if not entries:
        warn("AGENTS.md", "missing; agents start without project commands or boundaries")
    for entry in entries:
        lines = len(entry.read_text(encoding="utf-8", errors="replace").splitlines())
        if lines > args.max_agents_lines:
            warn(entry.name, f"{lines} lines (> {args.max_agents_lines}); move stable details into docs/reference/")

    current = repo / "docs/current.md"
    if current.is_file():
        text = current.read_text(encoding="utf-8", errors="replace")
        actions = count_next_actions(text)
        if actions > 5:
            warn("docs/current.md", f"{actions} next actions (> 5); archive finished items or split the focus")
        age_days = (time.time() - last_change_epoch(repo, current, in_git)) // 86400
        if age_days > args.stale_days:
            warn("docs/current.md", f"last changed {int(age_days)} days ago; confirm it still reflects the work")
        size_kb = len(text.encode("utf-8")) / 1024
        if size_kb > args.max_current_kb:
            warn("docs/current.md", f"{size_kb:.0f} KB (> {args.max_current_kb}); every session reads it, keep only "
                 "what the next step needs")
        dated = sum(1 for line in text.splitlines() if DATED_ITEM.match(line))
        if dated > args.max_dated_items:
            warn("docs/current.md", f"{dated} dated entries (> {args.max_dated_items}); shipped work is recorded in "
                 "git, keep only open state (unverified, not yet live, known risks)")

    targets = make_targets(repo)
    for doc in [*entries, *([current] if current.is_file() else [])]:
        prose, code = split_fences(doc.read_text(encoding="utf-8", errors="replace"))
        for token in missing_paths(repo, doc.parent, prose, in_git):
            warn(doc.relative_to(repo), f"names `{token}`, which does not exist")
        if targets is not None:
            called = {m.group(2) for m in MAKE_CALL.finditer(prose + "\n" + code) if not m.group(3)}
            for target in sorted(called - targets):
                warn(doc.relative_to(repo), f"names `make {target}`, which the Makefile does not define")

    if in_git and git(repo, "check-ignore", "-q", ".local/probe") is None:
        warn(".gitignore", ".local/ is not ignored; drafts and run artifacts may be committed")

    for doc in markdown_files(repo):
        text = doc.read_text(encoding="utf-8", errors="replace")
        for target in LINK.findall(text):
            if target.startswith(("http://", "https://", "mailto:", "#")):
                continue
            path = unquote(target.split("#", 1)[0])
            if path and not (doc.parent / path).exists():
                warn(doc.relative_to(repo), f"broken link: {target}")

    for line in findings:
        print(line)
    print(f"{len(findings)} finding(s) in {repo}" if findings else f"ok: {repo}")
    return 1 if findings else 0


if __name__ == "__main__":
    sys.exit(main())
