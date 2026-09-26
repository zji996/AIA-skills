"""Creating runs: prompt, contract, snapshot, worktree and session metadata."""

import fcntl
import hashlib
import os
import re
import shlex
import shutil
import subprocess
import sys
import time
from pathlib import Path

from .agents import choose_agent, missing_tools, nesting_error, session_file
from .changes import snapshot
from .common import (
    ACTIVE, DEFAULT_TIMEOUT, LANE_HELD, SCRIPT, die, git_top, now_iso, read_json, seconds, setting, state_dir, write_json,
)
from .runs import (
    agent_alive, all_runs, capacity_error, machine_runs, memory_error, prune_expired, replies_to, run_state, runs_root,
)
from .worktree import delegate_env, worktree_config, worktrees_dir


def read_prompt(args):
    if args.prompt_file == "-":
        prompt = sys.stdin.read()
    elif args.prompt_file:
        if not Path(args.prompt_file).is_file():
            die(f"prompt file does not exist: {args.prompt_file}")
        prompt = Path(args.prompt_file).read_text(encoding="utf-8")
    else:
        prompt = args.prompt_text or " ".join(args.words)
    if not prompt.strip():
        die("empty prompt; pass --prompt-file, --prompt, or trailing text")
    images = []
    for image in args.image or []:
        if not Path(image).is_file():
            die(f"image does not exist: {image}")
        images.append(str(Path(image).resolve()))
    args.image = images
    return prompt



def start_run(args):
    error = nesting_error()
    if error:
        die(error)
    choose_agent(args)
    if args.timeout is None:
        args.timeout = DEFAULT_TIMEOUT[args.agent]
    prompt = read_prompt(args)
    workdir = Path(args.workdir or os.getcwd())
    if not workdir.is_dir():
        die(f"workdir does not exist: {workdir}")
    workdir = str(workdir.resolve())
    missing_tools(args.agent)
    mode = "read-only" if args.read_only else "write"
    if args.in_place and not args.read_only:
        die("--in-place is for --read-only runs; write runs work in place unless --worktree is given")
    if args.in_place and args.worktree:
        die("--in-place and --worktree contradict each other")
    repo = git_top(workdir)
    extra = {"env": delegate_env(repo) if repo else {}}
    # A read-only run reads a snapshot of the working tree, so the caller may keep editing meanwhile without
    # its edits being taken for the run's; outside git there is nothing to snapshot and it reads in place.
    top = repo if args.worktree or (args.read_only and not args.in_place) else None
    if args.worktree and not top:
        die(f"--worktree needs a git repository: {workdir}")
    if top:
        config = worktree_config(top)
        if args.read_only and args.agent == "pi":
            config = {**config, "setup": []}  # read-only Pi has no shell; dependencies are of no use to it
        extra["worktree"] = {"source": top, "sourceWorkdir": workdir, "config": config}
    return launch(args, prompt, workdir, mode, extra)



def launch(args, prompt, workdir, mode, extra):
    root = runs_root()
    root.mkdir(parents=True, exist_ok=True)
    slots = state_dir()
    slots.mkdir(parents=True, exist_ok=True)
    # Concurrent starts would each miss the other's run in the capacity and exclusivity checks; serialize them.
    with open(slots / ".start.lock", "w") as machine_lock, open(root / ".start.lock", "w") as lock:
        fcntl.flock(machine_lock, fcntl.LOCK_EX)
        fcntl.flock(lock, fcntl.LOCK_EX)
        active = machine_runs(slots)
        error = capacity_error(args.agent, active) or memory_error(active)
        if error:
            die(error)
        run = create_run(args, prompt, workdir, mode, root, extra)
        (slots / (hashlib.sha1(str(run).encode()).hexdigest()[:16] + ".slot")).write_text(f"{run}\n")
        return run



def with_contract(prompt, accept=None, read_only=False, revoked=False):
    """State the task boundary and definition of done, as a delegator would, in the prompt's language."""
    zh = bool(re.search(r"[\u3040-\u30ff\u4e00-\u9fff]", prompt))
    notes = []
    if revoked:
        notes.append("完成标准有变：之前给出的验收命令不再适用。" if zh else
                     "The definition of done has changed: the earlier acceptance command no longer applies.")
    if read_only:
        notes.append("只读任务：不要创建、修改或删除任何文件；结束后会核对工作目录，改动不会被采纳，并会报告给委派方。"
                     "用 delegate 委派子任务产生的记录在 git 忽略的目录里，不算改动，无需改动其存放位置。" if zh else
                     "Read-only task: do not create, modify or delete files; the working directory is checked afterwards, "
                     "and any change is reported to the delegator and never adopted. Records of subtasks delegated with "
                     "delegate live in git-ignored directories and do not count; leave their location as it is.")
    if accept:
        notes.append(("完成标准：你结束后，委派方会在工作目录中运行下面的命令，退出码为 0 即视为完成。" if zh else
                      "Definition of done: after you finish, the delegator runs this command in the working directory; "
                      "exit code 0 counts as complete.") + f"\n\n```sh\n{accept}\n```\n\n" +
                     (f"自己跑这条命令或其他耗时的检查时，前面加 `{SCRIPT} lane`（如 `{SCRIPT} lane {shlex.quote(accept)}`）："
                      "它与本机其他检查排队、一次只跑一个，排队时间不计入你的时限。" if zh else
                      f"When you run this or another heavy check yourself, prefix it with `{SCRIPT} lane` (e.g. "
                      f"`{SCRIPT} lane {shlex.quote(accept)}`): it queues with the other checks on this machine, one at a time, "
                      "and time spent queued does not count against your time limit."))
    if not notes:
        return prompt
    return prompt.rstrip("\n") + "\n\n---\n" + "\n\n".join(notes) + "\n"



def create_run(args, prompt, workdir, mode, root, extra):
    if mode == "write" and not args.allow_parallel_writes and not extra.get("worktree"):
        for run in all_runs():
            meta = read_json(run / "meta.json", {}) or {}
            if meta.get("mode") == "write" and meta.get("workdir") == workdir and \
                    (run_state(run) in ACTIVE or agent_alive(run)):
                die(f"write run {run.name} is still active in {workdir}; wait for it, use --read-only, "
                    "or pass --allow-parallel-writes")
    parent, fork = extra.get("parent"), extra.get("session")
    if parent:
        # Checked again under the start lock: two replies racing for one conversation would share its worktree.
        later = [r.name for r in replies_to(parent["run"])]
        if later:
            die(f"{parent['run']} already has a reply ({later[-1]}); wait for it and reply to that")
    if not setting("RUNS") and not (root / ".gitignore").exists():
        (root / ".gitignore").write_text("*\n")
    slug = re.sub(r"[^A-Za-z0-9._-]+", "-", args.name or "").strip("-")[:40] or os.urandom(2).hex()
    stamp = time.strftime("%Y%m%d-%H%M%S")
    run = root / f"{stamp}-{slug}"
    while True:
        try:
            run.mkdir(mode=0o700)
            break
        except FileExistsError:
            run = root / f"{stamp}-{slug}-{os.urandom(2).hex()}"
    first_line = next((line.strip() for line in prompt.splitlines() if line.strip()), "")[:120]
    if parent and fork:
        # The session already holds the contract; restate it only when the reply changes it.
        changed = args.accept != parent.get("accept")
        shown = args.accept if changed and not args.hide_accept else None
        prompt = with_contract(prompt, shown, revoked=changed and not shown)
    else:
        # Pi's read-only mode removes write tools; Codex gets the boundary in writing and a result check.
        prompt = with_contract(prompt, None if args.hide_accept else args.accept,
                               read_only=mode == "read-only" and args.agent == "codex")
    # prompt.md is exactly what the agent receives.
    (run / "prompt.md").write_text(prompt if prompt.endswith("\n") else prompt + "\n", encoding="utf-8")
    if parent and fork and parent["agent"] == "pi":
        # Fork from a copy: cleaning or pruning an earlier round must not break this reply or its reruns.
        source = session_file(parent["sessionDir"], fork)
        (run / "fork").mkdir()
        fork = str(shutil.copy2(source, run / "fork" / source.name))
    tree = extra.get("worktree")
    if tree and not tree.get("path"):
        # Outside the repository, so the caller's tools (test runners, linters, watchers) never see it.
        tree["path"] = str(worktrees_dir() / f"{Path(tree['source']).name}-{run.name}")
        workdir = str(Path(tree["path"]) / os.path.relpath(tree["sourceWorkdir"], tree["source"]))
    top = tree["path"] if tree and Path(tree["path"]).exists() else tree["source"] if tree else git_top(workdir)
    config = tree["config"] if tree else {"copy": [], "link": []}
    exclude = config["copy"] + config["link"]
    base = snapshot(top, run, exclude) if top else None
    if tree and base and not Path(tree["path"]).exists():
        base["large"] = base["submodules"] = {}  # neither is carried over; the worktree starts without them
        top = tree["path"]
    if tree and not base:
        shutil.rmtree(run, ignore_errors=True)
        die(f"cannot snapshot {tree['source']} for the worktree")
    meta = {"run": run.name, "dir": str(run), "workdir": workdir, "mode": mode, "agent": args.agent,
            "tier": getattr(args, "tier", None),
            "name": args.name or (f"reply to {parent['name']}" if parent else first_line),
            "provider": args.provider, "model": args.model, "thinking": args.thinking,
            "timeout": args.timeout, "timeoutSeconds": seconds(args.timeout),
            "accept": args.accept, "acceptTimeoutSeconds": seconds(args.accept_timeout),
            "retries": args.retries, "images": args.image,
            "top": top, "base": base, "snapshotExclude": exclude, "worktree": tree,
            "env": extra.get("env") if "env" in extra else (parent or {}).get("env") or {},
            "chainBase": (parent or {}).get("chainBase") or (base or {}).get("tree"),
            "sessionDir": str(run / "session"),
            "parent": (parent or {}).get("run"), "fork": fork,
            "startedAt": now_iso(), "startedEpoch": int(time.time()), "startedNs": time.time_ns()}
    write_json(run / "meta.json", meta)
    # Only now: pruning may remove a reply's parent, whose session and worktree this run has taken over.
    prune_expired()
    with open(run / "supervisor.log", "wb") as log:
        # Not LANE_HELD, when started from inside a `lane` command: the run outlives that slot.
        subprocess.Popen([sys.executable, str(SCRIPT), "_supervise", str(run)], stdin=subprocess.DEVNULL,
                         stdout=log, stderr=log, start_new_session=True,
                         env={k: v for k, v in os.environ.items() if k != LANE_HELD})
    for _ in range(50):
        if (run / "pid").is_file():
            return run
        time.sleep(0.1)
    write_json(run / "summary.json", {"state": "crashed", "error": "supervisor did not start"})
    (run / "exit_code").write_text("1\n")
    die(f"supervisor did not start; see {run / 'supervisor.log'}")
