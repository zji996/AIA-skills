"""What a run changed: working-tree snapshots as git trees, and their differences."""

import os
import shutil
import subprocess
import threading
from pathlib import Path

from .common import CHANGES_SHOWN, LARGE_UNTRACKED, SCRIPT, die, git, read_json, setting, write_json


def snapshot(top, scratch, exclude=()):
    """The working tree as a git tree object: tracked and untracked files, minus ignored ones.

    Works on a copy of the real index (reusing its stat cache), so the repository's own index and
    refs are untouched. Untracked files above DELEGATE_SNAPSHOT_MAX_BYTES are fingerprinted instead
    of hashed into the object store. Returns None outside a git repository.
    """
    index = Path(scratch) / f".snapshot-index-{os.getpid()}-{threading.get_ident()}"
    try:
        real = Path(top) / git(top, "rev-parse", "--git-path", "index").strip()
        if real.is_file():
            shutil.copyfile(real, index)
        limit_bytes = int(setting("SNAPSHOT_MAX_BYTES", str(LARGE_UNTRACKED)))
        large = {}
        for path in git(top, "ls-files", "-z", "--others", "--exclude-standard").split("\0"):
            full = Path(top) / path
            if path and full.is_file() and not full.is_symlink() and full.stat().st_size > limit_bytes:
                info = full.stat()
                large[path] = [info.st_size, info.st_mtime_ns]
        # git refuses a pathspec naming an ignored path, even to exclude it; ignored paths need no exclusion.
        exclude = [path for path in exclude if (Path(top) / path).exists() or (Path(top) / path).is_symlink()]
        ignored = subprocess.run(["git", "-C", str(top), "check-ignore", "-z", "--stdin"], input="\0".join(exclude),
                                 capture_output=True, text=True).stdout.split("\0") if exclude else []
        skip = [f":(exclude,literal){path}" for path in sorted(set(large) | set(exclude) - set(ignored))]
        env = {**os.environ, "GIT_INDEX_FILE": str(index)}
        git(top, "add", "-A", "--", ".", *skip, env=env)
        return {"tree": git(top, "write-tree", env=env).strip(), "large": large}
    except (OSError, ValueError, subprocess.CalledProcessError):
        return None
    finally:
        index.unlink(missing_ok=True)



def tree_changes(top, before, after):
    """[{path, status A|M|D, added, deleted}] between two snapshots; binary and large files count no lines."""
    lines = {}
    out = git(top, "diff", "--numstat", "-z", "--no-renames", before["tree"], after["tree"])
    for record in out.split("\0"):
        if record.count("\t") >= 2:
            added, deleted, path = record.split("\t", 2)
            lines[path] = (None, None) if added == "-" else (int(added), int(deleted))
    changes = []
    out = git(top, "diff", "--name-status", "-z", "--no-renames", before["tree"], after["tree"]).split("\0")
    for status_letter, path in zip(out[0::2], out[1::2]):
        added, deleted = lines.get(path, (None, None))
        changes.append({"path": path, "status": status_letter[:1], "added": added, "deleted": deleted})
    old, new = before.get("large") or {}, after.get("large") or {}
    for path in sorted(set(old) | set(new)):
        if old.get(path) != new.get(path):
            letter = "A" if path not in old else "D" if path not in new else "M"
            changes.append({"path": path, "status": letter, "added": None, "deleted": None, "large": True})
    return sorted(changes, key=lambda change: change["path"])



def record_changes(meta, run):
    """Snapshot after the agent, write changes.json and changes.patch; return (changes, totals) or None."""
    base = meta.get("base")
    if not base:
        return None
    after = snapshot(meta["top"], run, meta.get("snapshotExclude") or ())
    if not after:
        return None
    changes = tree_changes(meta["top"], base, after)
    write_json(run / "changes.json", {"base": base["tree"], "after": after["tree"], "top": meta["top"],
                                       "afterLarge": after["large"], "changes": changes})
    patch = git(meta["top"], "diff", "--binary", "--no-renames", base["tree"], after["tree"], text=False)
    (run / "changes.patch").write_bytes(patch)
    totals = {"files": len(changes), "added": sum(c["added"] or 0 for c in changes),
              "deleted": sum(c["deleted"] or 0 for c in changes), "after": after["tree"]}
    return changes, totals



def relative_to(path, workdir):
    """Codex reports absolute paths, Pi and git relative ones; list each file once."""
    try:
        return str(Path(path).relative_to(workdir)) if os.path.isabs(path) else path
    except ValueError:
        return path



def change_line(change):
    counts = "binary" if change["added"] is None else f"+{change['added']} -{change['deleted']}"
    return f" {change['status']} {change['path']}  {'large file' if change.get('large') else counts}"



def print_changes(run):
    """What the run changed, like `git diff --stat`, so the caller sees edits without reading the transcript."""
    recorded = read_json(run / "changes.json")
    meta = read_json(run / "meta.json", {}) or {}
    if recorded is None:
        if meta.get("mode") == "write" and not meta.get("base"):
            print(f"\n===== changes: {run.name}: not tracked (not a git repository; see files) =====")
        return
    changes = recorded["changes"]
    if not changes:
        if meta.get("mode") == "write":
            print(f"\n===== changes: {run.name}: none =====")
        return
    added, deleted = (sum(c[k] or 0 for c in changes) for k in ("added", "deleted"))
    where = f"; worktree {meta['worktree']['path']}" if meta.get("worktree") else ""
    count = f"{len(changes)} file{'s' if len(changes) != 1 else ''}"
    print(f"\n===== changes: {run.name} ({count}, +{added} -{deleted}{where}) =====")
    for change in changes[:CHANGES_SHOWN]:
        print(change_line(change))
    if len(changes) > CHANGES_SHOWN:
        print(f" ... {len(changes) - CHANGES_SHOWN} more in {run / 'changes.json'}")
    hint = f"{SCRIPT} diff {run.name}"
    if meta.get("worktree"):
        hint += f"; merge into {meta['worktree']['source']}: {SCRIPT} apply {run.name}"
    print(f"diff: {hint}")
    print(f"===== end changes: {run.name} =====", flush=True)



def chain_changes(run):
    """(top, before tree, after tree) for a run, or for its whole conversation with total=True."""
    meta = read_json(run / "meta.json", {}) or {}
    recorded = read_json(run / "changes.json")
    if not recorded:
        die(f"{run.name} has no recorded changes (still running, not a git repository, or before delegate 4.1)")
    top = recorded["top"]
    if not Path(top).is_dir():  # a removed worktree; its objects live in the source repository
        top = (meta.get("worktree") or {}).get("source", top)
    return meta, top, recorded["base"], recorded["after"]
