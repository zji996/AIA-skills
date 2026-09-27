#!/usr/bin/env python3
"""Generate or check the ADR index table from each ADR's title and status line."""

import argparse
import re
import subprocess
import sys
from pathlib import Path

DECISION_DIRS = ("docs/decision", "docs/decisions", "docs/adr")
ADR_FILE = re.compile(r"^(\d{3,5})-.+\.md$")
TITLE = re.compile(r"^#\s+(.+?)\s*$")
NUMBER_PREFIX = re.compile(r"^(?:ADR[\s-]*)?(\d+)(?:\s*[:：·.-]\s*|\s+)(.+)$", re.I)
STATUS = re.compile(r"^\s*(?:[-*]\s+)?(?:\*\*)?(?:状态|status)(?:\*\*)?\s*[:：]\s*(?:\*\*\s*)?(.+?)\s*$", re.I)
START, END = "<!-- adr-index:start -->", "<!-- adr-index:end -->"


def decision_dir(repo):
    for name in DECISION_DIRS:
        if (repo / name).is_dir():
            return repo / name
    return None


def adr_row(path):
    number = ADR_FILE.match(path.name).group(1)
    title, status = path.stem, "（未标状态）"
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()
    for line in lines:
        match = TITLE.match(line)
        if match:
            title = match.group(1)
            prefix = NUMBER_PREFIX.match(title)
            if prefix and int(prefix.group(1)) == int(number):  # "ADR 0001: x", "0001 x"; not "2026 plans"
                title = prefix.group(2)
            break
    for line in lines[:20]:
        match = STATUS.match(line)
        if match:
            status = match.group(1).rstrip("*").strip()
            break
    title = title.replace("|", "\\|")
    status = status.replace("|", "\\|")
    return f"| {number} | [{title}]({path.name}) | {status} |"


def table(folder):
    rows = [adr_row(p) for p in sorted(folder.iterdir()) if ADR_FILE.match(p.name)]
    return "\n".join(["| # | 标题 | 状态 |", "| --- | --- | --- |", *rows])


def render(index_text, folder):
    """Return the index with the marked table regenerated, or None when it has no markers."""
    if START not in index_text or END not in index_text:
        return None
    head, rest = index_text.split(START, 1)
    _, tail = rest.split(END, 1)
    return f"{head}{START}\n{table(folder)}\n{END}{tail}"


def check(repo):
    """(index path, expected text) when the marked index is out of date, else None."""
    folder = decision_dir(repo)
    index = folder / "INDEX.md" if folder else None
    if not index or not index.is_file():
        return None
    text = index.read_text(encoding="utf-8")
    expected = render(text, folder)
    return (index, expected) if expected is not None and expected != text else None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", default=".", help="repository root (default: git root of cwd)")
    parser.add_argument("--write", action="store_true", help="rewrite the table between the index markers")
    args = parser.parse_args()
    repo = Path(args.repo).resolve()
    top = subprocess.run(["git", "-C", str(repo), "rev-parse", "--show-toplevel"], capture_output=True, text=True)
    if top.returncode == 0:
        repo = Path(top.stdout.strip())
    folder = decision_dir(repo)
    index = folder / "INDEX.md" if folder else None
    if not index or not index.is_file():
        print(f"no ADR index under {repo}; nothing to do")
        return 0
    text = index.read_text(encoding="utf-8")
    expected = render(text, folder)
    if expected is None:
        print(f"{index.relative_to(repo)} has no {START} / {END} markers; add them around the table to manage it",
              file=sys.stderr)
        return 2
    if expected == text:
        print(f"ok: {index.relative_to(repo)}")
        return 0
    if args.write:
        index.write_text(expected, encoding="utf-8")
        print(f"wrote {index.relative_to(repo)}")
        return 0
    print(f"WARN {index.relative_to(repo)}: out of date with the ADR status lines; run adr-index.py --write")
    return 1


if __name__ == "__main__":
    sys.exit(main())
