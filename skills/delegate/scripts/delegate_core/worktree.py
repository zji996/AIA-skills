"""--worktree: preparing, removing, and merging a worktree back with apply."""

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from .changes import chain_changes, snapshot, tree_changes
from .common import (
    CONFIG_FILE, GUARD_VARS, LANE_HELD, SCRIPT, clip, die, git, now_iso, read_json, run_shell, seconds, setting,
)
from .lane import heavy_slot


def read_config(path):
    if not path.is_file():
        return {}
    try:
        config = json.loads(path.read_text())
    except (OSError, ValueError) as error:
        die(f"cannot read {path}: {error}")
    if not isinstance(config, dict):
        die(f"{path}: expected a JSON object")
    return config



def delegate_env(top):
    """`.delegate.json` "env": variables for agents, acceptance and setup (e.g. CUDA_VISIBLE_DEVICES="")."""
    path = Path(top) / CONFIG_FILE
    env = read_config(path).get("env") or {}
    if not isinstance(env, dict) or not all(isinstance(k, str) and isinstance(v, str) for k, v in env.items()):
        die(f"{path}: env must map names to strings")
    return env



def worktree_config(top):
    """`.delegate.json` at the repo root: which ignored files a worktree copies or links, and setup commands."""
    path = Path(top) / CONFIG_FILE
    config = read_config(path).get("worktree") or {}
    if not isinstance(config, dict):
        die(f"{path}: worktree must be an object")
    out = {}
    for key in ("copy", "link", "setup"):
        value = config.get(key) or []
        value = [value] if isinstance(value, str) else value
        if not isinstance(value, list) or not all(isinstance(v, str) and v.strip() for v in value):
            die(f"{path}: worktree.{key} must be a list of strings")
        if key != "setup" and any(os.path.isabs(v) or ".." in Path(v).parts for v in value):
            die(f"{path}: worktree.{key} entries must be paths inside the repository")
        out[key] = [v.strip().rstrip("/") if key != "setup" else v for v in value]
    return out



def worktrees_dir():
    return Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache") / "delegate/worktrees"



def prepare_worktree(meta, run):
    """Create the detached worktree, seed it with the caller's snapshot, then copy/link/setup. Returns an error."""
    tree, config = meta["worktree"], meta["worktree"]["config"]
    path, source = Path(tree["path"]), tree["source"]
    log = run / "setup.log"
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        # The worktree starts from what the caller sees, uncommitted edits included: a commit of the snapshot
        # on top of HEAD (or parentless in a repository without commits), referenced only by the worktree.
        head = subprocess.run(["git", "-C", source, "rev-parse", "-q", "--verify", "HEAD^{commit}"],
                              capture_output=True, text=True).stdout.strip()
        identity = {f"GIT_{who}_{key}": value for who in ("AUTHOR", "COMMITTER")
                    for key, value in (("NAME", "delegate"), ("EMAIL", "delegate@localhost"))}
        commit = git(source, "commit-tree", meta["base"]["tree"], *(["-p", head] if head else []),
                     "-m", f"delegate: working tree of {source} for {run.name}", env={**os.environ, **identity}).strip()
        git(source, "worktree", "add", "--detach", str(path), commit)
        if meta["mode"] == "read-only" and head:
            # A reader sees what the caller sees: HEAD is the caller's commit, uncommitted work shows in
            # `git diff HEAD` and `git status`. Files stay as they are; changes are measured by snapshot anyway.
            git(str(path), "reset", "-q", head)
    except (OSError, subprocess.CalledProcessError) as error:
        detail = getattr(error, "stderr", None) or str(error)
        return f"worktree setup failed: {clip(str(detail).strip(), 300)}"
    for item in config["copy"] + config["link"]:
        origin, target = Path(source) / item, path / item
        if target.is_dir() and not target.is_symlink() and not any(target.iterdir()):
            target.rmdir()  # e.g. a submodule, which a new worktree leaves as an empty directory
        if not origin.exists() or target.exists() or target.is_symlink():
            continue
        target.parent.mkdir(parents=True, exist_ok=True)
        if item in config["link"]:
            target.symlink_to(origin)
        elif origin.is_dir():
            shutil.copytree(origin, target, symlinks=True)
        else:
            shutil.copy2(origin, target)
    if not config["setup"]:
        return None
    env = {k: v for k, v in os.environ.items() if k not in GUARD_VARS}
    env.update(meta.get("env") or {})
    env[LANE_HELD] = "1"
    timeout = seconds(setting("SETUP_TIMEOUT", "10m"))
    # Installing dependencies is heavy too; the timeout starts once the lane lets it run.
    with heavy_slot(f"setup {run.name}"), open(log, "a", encoding="utf-8") as out:
        for command in config["setup"]:
            out.write(f"$ {command}\n")
            out.flush()
            code, _ = run_shell(command, path, env, timeout, out)
            out.write(f"[exit {code}]\n")
            if code != 0:
                return f"worktree setup command failed (exit {code}): {command}; see {log}"
    return None



def remove_worktree(meta):
    tree = meta.get("worktree") or {}
    path = tree.get("path")
    if not path or not Path(path).exists():
        return
    try:
        git(tree["source"], "worktree", "remove", "--force", path)
    except (OSError, subprocess.CalledProcessError):
        shutil.rmtree(path, ignore_errors=True)
        subprocess.run(["git", "-C", tree["source"], "worktree", "prune"], capture_output=True)



def blob(top, tree, path):
    """(mode, bytes) of path in tree; (None, None) when absent, (mode, None) for a directory or submodule."""
    entry = git(top, "ls-tree", "-z", tree, "--", path).split("\0")[0]
    if not entry:
        return None, None
    mode, kind, sha = entry.split("\t", 1)[0].split()
    return (mode, git(top, "cat-file", "blob", sha, text=False)) if kind == "blob" else (mode, None)



FILE_MODES = ("100644", "100755", "120000")



def entry_mode(target):
    if target.is_symlink():
        return "120000"
    if target.is_file():
        return "100755" if target.stat().st_mode & 0o111 else "100644"
    return "dir" if target.exists() else None



def through_symlink(root, target):
    """True when a directory on the way from root to target is a symlink, so writing could leave the tree."""
    path = Path(root)
    for part in target.relative_to(root).parts[:-1]:
        path = path / part
        if path.is_symlink():
            return True
    return False



def current(target):
    if target.is_symlink():
        return os.readlink(target).encode()
    return target.read_bytes() if target.is_file() else None



def write_entry(target, mode, content):
    if target.is_symlink() or target.exists():
        target.unlink()
    target.parent.mkdir(parents=True, exist_ok=True)
    if mode == "120000":
        target.symlink_to(content.decode())
        return
    target.write_bytes(content)
    target.chmod(0o755 if mode == "100755" else 0o644)



def merge_text(top, before, after, path, target, label):
    """Three-way merge of the current file with the run's version; (exit code, merged bytes)."""
    with tempfile.TemporaryDirectory() as scratch:
        mine, base, theirs = (Path(scratch, name) for name in ("mine", "base", "theirs"))
        mine.write_bytes(target.read_bytes())
        base.write_bytes(blob(top, before, path)[1])
        theirs.write_bytes(blob(top, after, path)[1])
        code = subprocess.run(["git", "merge-file", "-L", "current", "-L", "base", "-L", label,
                               str(mine), str(base), str(theirs)], capture_output=True).returncode
        return code, mine.read_bytes()



def apply_conversation(run, merge=False, dry_run=False):
    """Merge a finished worktree conversation into the source working tree; the index is untouched.

    What is merged is the worktree as it is now, so touch-ups made there after the last run (by hand, or by a
    run started with --workdir inside it) are included; without the worktree, the last recorded state is used.
    """
    meta, top, _, after = chain_changes(run)
    tree = meta.get("worktree")
    if not tree:
        die(f"{run.name} worked in place; its changes are already in {meta.get('workdir')}")
    if meta.get("mode") == "read-only":
        die(f"{run.name} is read-only; its worktree is a snapshot to read, with nothing to apply")
    before, source, worktree = meta["chainBase"], tree["source"], Path(tree["path"])
    after_large = (read_json(run / "changes.json", {}) or {}).get("afterLarge") or {}
    now_snapshot = snapshot(worktree, run, meta.get("snapshotExclude") or ()) if worktree.is_dir() else None
    if now_snapshot:
        top, after, after_large = str(worktree), now_snapshot["tree"], now_snapshot["large"]
    changes = tree_changes(top, {"tree": before}, {"tree": after})
    actions, conflicts = [], []
    for change in changes:
        path, target = change["path"], Path(source) / change["path"]
        old_mode, old = blob(top, before, path)
        mode, new = blob(top, after, path)
        now, now_mode = current(target), entry_mode(target)
        if through_symlink(source, target) or now_mode == "dir" or \
                not {old_mode, mode} <= set(FILE_MODES) | {None}:
            conflicts.append(path)  # outside the tree via a symlink, or a file/directory swap
        elif (now, now_mode) == (new, mode):
            continue  # already there
        elif (now, now_mode) == (old, old_mode) or (now == old and now_mode == mode):
            actions.append(("deleted" if new is None else "applied", path, target, mode, new))
        elif None not in (old, new, now) and "120000" not in (old_mode, mode, now_mode) \
                and b"\0" not in old + new + now:
            code, merged = merge_text(top, before, after, path, target, run.name)
            if code == 0:
                actions.append(("merged", path, target, mode if old_mode != mode else None, merged))
            elif merge:
                actions.append(("conflict-markers", path, target, None, merged))
            else:
                conflicts.append(path)
        else:
            conflicts.append(path)  # binary, symlink, or deleted on one side and edited on the other
    # Large untracked files were fingerprinted, not stored: copy them from the worktree itself.
    for path in sorted(after_large):
        origin, target = worktree / path, Path(source) / path
        if not origin.is_file() or through_symlink(source, target) or entry_mode(target) == "dir":
            conflicts.append(path)
        elif not target.exists() and not target.is_symlink():
            actions.append(("copied", path, target, "large", origin))
        elif not (target.is_file() and target.stat().st_size == origin.stat().st_size
                  and target.read_bytes() == origin.read_bytes()):
            conflicts.append(path)
    if not actions and not conflicts:
        print("delegate: no changes to apply", file=sys.stderr)
        return 0
    if conflicts and not merge:
        for path in conflicts:
            print(f" conflict         {path}")
        print(f"delegate: nothing applied; {len(conflicts)} file(s) were also changed in {source} since the run "
              "started. Rerun with --merge to apply the rest and write conflict markers into text files, "
              f"or inspect with: {SCRIPT} diff {run.name} --total", file=sys.stderr)
        return 1
    for kind, path, target, mode, content in actions:
        if not dry_run:
            if kind == "deleted":
                target.unlink()
            elif kind == "copied":
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(content, target)
            elif mode:
                write_entry(target, mode, content)
            else:
                target.write_bytes(content)  # merged: keep the file's current mode
        print(f" {kind:<16} {path}")
    for path in conflicts:
        print(f" {'skipped':<16} {path}  (binary, symlink, type change or large; take it from {worktree})")
    if dry_run:
        print("delegate: dry run; nothing written", file=sys.stderr)
    elif not conflicts:
        chain = run
        while chain:  # the whole conversation is merged, not just its last run
            (chain / ".applied").write_text(now_iso() + "\n")
            parent = (read_json(chain / "meta.json", {}) or {}).get("parent")
            chain = chain.parent / parent if parent and (chain.parent / parent).is_dir() else None
    return 1 if conflicts or any(a[0] == "conflict-markers" for a in actions) else 0
