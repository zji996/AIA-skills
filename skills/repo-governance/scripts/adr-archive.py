#!/usr/bin/env python3
"""Archive ADRs merged into main ADRs: move them to <decision dir>/history/, mark them
Superseded with links to the ADRs that absorbed them, and repair relative links.

  adr-archive.py 0088=0079,0080 0092=0081          # dry run: print the plan
  adr-archive.py --write 0088=0079,0080 0092=0081  # move, rewrite, relink

Each argument is MOVED=TARGET[,TARGET...] by ADR number. Write the merged text into the
targets first; this script only does the mechanical part. Links are recomputed from the
real file locations, inside the moved files and in every tracked Markdown/HTML file.
Afterwards regenerate the index with adr-index.py --write.
"""

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path

DECISION_DIRS = ("docs/decision", "docs/decisions", "docs/adr")
ADR_FILE = re.compile(r"^(\d{3,5})-.+\.md$")
TITLE = re.compile(r"^#\s+(.+?)\s*$")
STATUS = re.compile(r"^(\s*(?:[-*]\s+)?(?:\*\*)?(状态|status)(?:\*\*)?\s*[:：]\s*)(.+?)\s*$", re.I)
MD_LINK = re.compile(r"(\]\()([^)\s#]+)(#[^)\s]*)?(\))")
HTML_LINK = re.compile(r'((?:href|src)=")([^"#]+)(#[^"]*)?(")')
EXTERNAL = re.compile(r"^(?:[a-z][a-z0-9+.-]*:|/|#)", re.I)


def decision_dir(repo):
    for name in DECISION_DIRS:
        if (repo / name).is_dir():
            return repo / name
    return None


def by_number(folder):
    files = {}
    for path in folder.iterdir():
        match = ADR_FILE.match(path.name)
        if match:
            files[int(match.group(1))] = path
    return files


def parse_moves(args, files):
    moves = {}
    for arg in args:
        moved, sep, targets = arg.partition("=")
        if not sep or not targets:
            raise SystemExit(f"expected MOVED=TARGET[,TARGET], got {arg!r}")
        source = files.get(int(moved))
        if source is None:
            raise SystemExit(f"no ADR {moved} in {next(iter(files.values())).parent}")
        resolved = []
        for target in targets.split(","):
            path = files.get(int(target))
            if path is None:
                raise SystemExit(f"no target ADR {target}")
            if int(target) in {int(a.partition('=')[0]) for a in args}:
                raise SystemExit(f"target {target} is itself being archived")
            resolved.append(path)
        moves[source] = resolved
    return moves


def status_line(text, targets, chinese):
    """Rewrite the first status line within the first 20 lines; None when absent."""
    lines = text.splitlines(keepends=True)
    for i, line in enumerate(lines[:20]):
        match = STATUS.match(line.rstrip("\n"))
        if not match:
            continue
        original = match.group(3).rstrip("*").strip()
        links = ("、" if chinese else ", ").join(f"[ADR {p.name[:4]}](../{p.name})" for p in targets)
        if chinese:
            value = f"Superseded（原状态：{original}；并入 {links}）"
        else:
            value = f"Superseded (was: {original}; merged into {links})"
        lines[i] = match.group(1) + value + ("\n" if line.endswith("\n") else "")
        return "".join(lines), original
    return None, None


def relink(text, base_old, base_new, moved_map, is_html):
    """Recompute relative links of a file that moves from base_old to base_new (dirs)."""

    def fix(match):
        target = match.group(2)
        if EXTERNAL.match(target):
            return match.group(0)
        original = Path(os.path.normpath(base_old / target))
        resolved = moved_map.get(original, original)
        if resolved == original and base_old == base_new:
            return match.group(0)
        rel = os.path.relpath(resolved, base_new)
        return f"{match.group(1)}{rel}{match.group(3) or ''}{match.group(4)}"

    text = MD_LINK.sub(fix, text)
    return HTML_LINK.sub(fix, text) if is_html else text


def tracked_docs(repo):
    out = subprocess.run(["git", "-C", str(repo), "ls-files", "*.md", "*.html"],
                         capture_output=True, text=True)
    if out.returncode == 0:
        return [repo / line for line in out.stdout.splitlines() if line]
    return [p for p in repo.rglob("*") if p.suffix in (".md", ".html") and ".git" not in p.parts]


def title_of(text, fallback):
    for line in text.splitlines():
        match = TITLE.match(line)
        if match:
            return re.sub(r"^(?:ADR[\s-]*)?\d+\s*[:：·.-]?\s*", "", match.group(1)).replace("|", "\\|")
    return fallback


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("moves", nargs="+", metavar="MOVED=TARGET[,TARGET]")
    parser.add_argument("--repo", default=".", help="repository root (default: git root of cwd)")
    parser.add_argument("--write", action="store_true", help="perform the moves (default: dry run)")
    args = parser.parse_args()
    repo = Path(args.repo).resolve()
    top = subprocess.run(["git", "-C", str(repo), "rev-parse", "--show-toplevel"], capture_output=True, text=True)
    git = top.returncode == 0
    if git:
        repo = Path(top.stdout.strip())
    folder = decision_dir(repo)
    if folder is None:
        print(f"no decision directory under {repo}", file=sys.stderr)
        return 2
    history = folder / "history"
    moves = parse_moves(args.moves, by_number(folder))
    moved_map = {src: history / src.name for src in moves}
    for src, dst in moved_map.items():
        if dst.exists():
            print(f"{dst.relative_to(repo)} already exists", file=sys.stderr)
            return 2

    plans, rows = [], []
    for src, targets in moves.items():
        text = src.read_text(encoding="utf-8")
        chinese = bool(re.search(r"^\s*(?:[-*]\s+)?(?:\*\*)?状态", text[:2000], re.M))
        # Relink first: the new status line is already written relative to history/.
        updated, original = status_line(relink(text, folder, history, moved_map, False), targets, chinese)
        if updated is None:
            print(f"{src.relative_to(repo)}: no status line in the first 20 lines", file=sys.stderr)
            return 2
        plans.append((src, moved_map[src], updated))
        rows.append(f"| {src.name[:4]} {title_of(text, src.stem)} | {original} | {'、'.join(p.name[:4] for p in targets)} |")
        print(f"move {src.relative_to(repo)} -> {moved_map[src].relative_to(repo)} (was {original}; into "
              f"{', '.join(p.name[:4] for p in targets)})")

    relinked = []
    for path in tracked_docs(repo):
        if path in moved_map or not path.is_file():
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        new = relink(text, path.parent, path.parent, moved_map, path.suffix == ".html")
        if new != text:
            relinked.append((path, new))
            print(f"relink {path.relative_to(repo)}")

    if not args.write:
        print("dry run; add --write to apply")
        print("history index rows:\n" + "\n".join(rows))
        return 0
    history.mkdir(exist_ok=True)
    for src, dst, text in plans:
        if git:
            subprocess.run(["git", "-C", str(repo), "mv", str(src), str(dst)], check=True)
        else:
            src.rename(dst)
        dst.write_text(text, encoding="utf-8")
    for path, text in relinked:
        path.write_text(text, encoding="utf-8")
    index = history / "INDEX.md"
    if index.is_file():
        lines = index.read_text(encoding="utf-8").splitlines()
        last = max((i for i, l in enumerate(lines) if l.startswith("| ")), default=None)
        if last is not None:
            lines[last + 1:last + 1] = rows
            index.write_text("\n".join(lines) + "\n", encoding="utf-8")
            print(f"appended {len(rows)} rows to {index.relative_to(repo)}")
    else:
        print("history index rows:\n" + "\n".join(rows))
    print("next: adr-index.py --write, then repoint plain-text mentions (rg 'ADR <number>')")
    return 0


if __name__ == "__main__":
    sys.exit(main())
