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
        actions = count_next_actions(current.read_text(encoding="utf-8", errors="replace"))
        if actions > 5:
            warn("docs/current.md", f"{actions} next actions (> 5); archive finished items or split the focus")
        age_days = (time.time() - last_change_epoch(repo, current, in_git)) // 86400
        if age_days > args.stale_days:
            warn("docs/current.md", f"last changed {int(age_days)} days ago; confirm it still reflects the work")

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
