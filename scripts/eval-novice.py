#!/usr/bin/env python3
"""Deterministic memoryless-novice replay harness for the delegate skill.

Implements section 4 of `.local/plan/delegate-memoryless-plan.md`:
- Isolated temporary git repository + isolated HOME / XDG_* / DELEGATE_RUNS
- Deterministic fake_pi colleague driven by fixture rules (zero tokens by default)
- Pluggable controller runner:
  - Default: ScriptedSkillController (zero-token replay strictly using SKILL.md top-screen commands)
  - Optional (`--model <model>`): ModelController (real LLM controller, excluded from verify.sh)
- Deterministic trajectory & repo-state evaluator reporting wasted steps:
  - exit_2_unknown_arg
  - exit_76_duplicate_wait
  - manual_polling (sleep / while / until)
  - test_on_unapplied_tree (running tests after apply/land failed without applying)
  - commit_missing_trailer (git commit / landed commits missing Delegate-Run trailer)
  - redundant_file_read (re-reading the same file unchanged)
"""

from __future__ import annotations

import argparse
import dataclasses
import fcntl
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_FIXTURES_DIR = ROOT / "evals" / "novice-fixtures"
SKILL_MD_PATH = ROOT / "skills" / "delegate" / "SKILL.md"

CALLER_ENV = (
    "DELEGATE_CALLER",
    "ANTIGRAVITY_CONVERSATION_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
    "PI_SESSION_ID",
)

REGISTERED_MODELS = {
    "main": "p/m",
    "scout": "p/scout-m",
    "review": "p/review-m",
    "write": "p/write-m",
    "ds": "p/ds-m",
    "gpt": "p/gpt-m",
    "named": "x/named",
    "env": "q/env-m",
}

RUN_ID_RE = re.compile(r"\b(20\d{6}-\d{6}-[^\s/,:;()\[\]【】\"']+)")
POLL_WORDS = {"sleep", "while", "until"}
READ_COMMANDS = {"cat", "head", "tail", "less", "more", "nl", "sed"}
TEST_CMD_PATTERNS = (
    re.compile(r"\bpytest\b"),
    re.compile(r"\bunittest\b"),
    re.compile(r"\bcargo\s+test\b"),
    re.compile(r"\b(npm|pnpm|yarn)\s+test\b"),
    re.compile(r"\bgo\s+test\b"),
    re.compile(r"\bscripts/verify\.sh\b"),
)
REQUIRED_SKILL_PATTERNS = (
    ("start --read-only", re.compile(r"\$D\s+start\s+--read-only\b")),
    ("start --worktree --accept", re.compile(r"\$D\s+start\s+--worktree\s+--accept\b")),
    ("wait", re.compile(r"\$D\s+wait\b")),
    ("reply", re.compile(r"\$D\s+reply\b")),
    ("card", re.compile(r"\$D\s+card\b")),
    ("land", re.compile(r"\$D\s+land\b")),
)


@dataclasses.dataclass
class CommandStep:
    index: int
    command: str
    argv: list[str]
    exit_code: int
    stdout: str
    stderr: str
    cwd: str


@dataclasses.dataclass
class WastedStep:
    category: str
    step_index: int
    command: str
    exit_code: int
    reason: str

    def to_dict(self) -> dict[str, Any]:
        return dataclasses.asdict(self)


@dataclasses.dataclass
class ScenarioResult:
    id: str
    title: str
    passed: bool
    checks: dict[str, bool]
    failures: list[str]
    total_steps: int
    input_tokens: int
    wasted_steps: list[WastedStep]
    failed_commands: list[dict[str, Any]]

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "title": self.title,
            "passed": self.passed,
            "checks": self.checks,
            "failures": self.failures,
            "total_steps": self.total_steps,
            "input_tokens": self.input_tokens,
            "wasted_step_count": len(self.wasted_steps),
            "wasted_steps": [w.to_dict() for w in self.wasted_steps],
            "failed_commands": self.failed_commands,
        }


def resolve_delegate_binary(repo_root: Path = ROOT) -> Path:
    env_bin = os.environ.get("DELEGATE_BIN")
    if env_bin:
        candidate = Path(env_bin).resolve()
        if os.access(candidate, os.X_OK):
            return candidate
        raise RuntimeError(f"DELEGATE_BIN is not executable: {candidate}")

    crate = repo_root / "crates" / "delegate"
    if (crate / "Cargo.toml").is_file() and shutil.which("cargo"):
        built = subprocess.run(
            ["cargo", "build", "--locked", "--quiet", "--message-format=json-render-diagnostics"],
            cwd=crate,
            stdout=subprocess.PIPE,
            text=True,
        )
        if built.returncode != 0:
            raise RuntimeError("cargo build of crates/delegate failed")
        for line in built.stdout.splitlines():
            event = json.loads(line)
            if (
                event.get("reason") == "compiler-artifact"
                and event.get("executable")
                and event["target"]["name"] == "delegate"
            ):
                return Path(event["executable"]).resolve()
        raise RuntimeError("cargo build of crates/delegate produced no delegate executable")

    fallback = (repo_root / "skills" / "delegate" / "bin" / "delegate").resolve()
    if os.access(fallback, os.X_OK):
        return fallback
    raise RuntimeError(
        f"{fallback} is missing: install cargo, set DELEGATE_BIN, or run scripts/fetch-binary.sh delegate"
    )


def _process_gone(pid_file: Path) -> bool:
    try:
        pid = int(pid_file.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return True
    try:
        return Path(f"/proc/{pid}/stat").read_text(encoding="utf-8").rsplit(")", 1)[1].split()[0] == "Z"
    except OSError:
        return True


def _live_state_workers(state_home: Path) -> list[int]:
    proc = Path("/proc")
    if not proc.is_dir():
        return []
    needle = f"XDG_STATE_HOME={state_home}\0".encode()
    self_pid = os.getpid()
    live: list[int] = []
    try:
        entries = os.listdir(proc)
    except OSError:
        return []
    for name in entries:
        if not name.isdigit():
            continue
        pid = int(name)
        if pid == self_pid:
            continue
        try:
            comm = (proc / name / "comm").read_bytes().strip()
            if not comm.startswith(b"delegate"):
                continue
            env = (proc / name / "environ").read_bytes()
            if needle in env:
                live.append(pid)
        except OSError:
            continue
    return live


def settle_supervisors(runs_dir: Path, state_home: Path, seconds: float = 10.0) -> None:
    deadline = time.monotonic() + seconds
    pids = list(runs_dir.glob("*/pid")) if runs_dir.is_dir() else []
    while pids and time.monotonic() < deadline:
        pids = [p for p in pids if not _process_gone(p)]
        if pids:
            time.sleep(0.05)
    while time.monotonic() < deadline and _live_state_workers(state_home):
        time.sleep(0.03)
    gc_lock = state_home / "delegate" / ".gc-worker.lock"
    premerge_dir = state_home / "delegate" / "premerge"
    premerge_locks = list(premerge_dir.glob("*.lock")) if premerge_dir.is_dir() else []
    for lock_file in [p for p in [gc_lock, *premerge_locks] if p.is_file()]:
        while time.monotonic() < deadline:
            try:
                with open(lock_file, "r+", encoding="utf-8") as fh:
                    fcntl.flock(fh.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                    fcntl.flock(fh.fileno(), fcntl.LOCK_UN)
                break
            except BlockingIOError:
                time.sleep(0.03)
            except OSError:
                break
    while time.monotonic() < deadline and _live_state_workers(state_home):
        time.sleep(0.03)


def validate_skill_md(skill_md_path: Path = SKILL_MD_PATH) -> list[str]:
    text = skill_md_path.read_text(encoding="utf-8")
    missing = [name for name, pat in REQUIRED_SKILL_PATTERNS if not pat.search(text)]
    return missing


def load_scenarios(fixtures_dir: Path, selected: set[str] | None = None) -> list[dict[str, Any]]:
    scenarios: list[dict[str, Any]] = []
    if not fixtures_dir.is_dir():
        raise FileNotFoundError(f"fixtures directory not found: {fixtures_dir}")
    for entry in sorted(fixtures_dir.iterdir()):
        manifest = entry / "scenario.json"
        if not entry.is_dir() or not manifest.is_file():
            continue
        data = json.loads(manifest.read_text(encoding="utf-8"))
        data["_dir"] = entry
        if selected and data["id"] not in selected:
            continue
        scenarios.append(data)
    if selected:
        found = {s["id"] for s in scenarios}
        unknown = sorted(selected - found)
        if unknown:
            raise ValueError(f"unknown scenario id(s): {', '.join(unknown)}")
    return scenarios


def install_fake_pi(
    bin_dir: Path,
    rules_path: Path,
    log_path: Path,
    main_repo: Path,
    hook_commits_path: Path,
    hook_marker_path: Path,
) -> Path:
    script = f"""#!{sys.executable}
import json
import os
import subprocess
import sys
from pathlib import Path

args = sys.argv[1:]
if args and args[0] == "--version":
    print("fake-pi 1.0")
    sys.exit(0)
if len(args) >= 2 and args[1] == "--list-models":
    print("provider model context max-out thinking images")
    print("p m 1M 1K yes yes")
    print("p scout-m 64k 1K no no")
    print("p review-m 1M 1K yes yes")
    print("p write-m 1M 1K yes yes")
    print("p ds-m 64k 1K yes no")
    print("p gpt-m 1M 1K yes yes")
    print("x named 1M 1K yes yes")
    print("q env-m 1M 1K yes yes")
    sys.exit(0)

stdin_text = "" if sys.stdin.isatty() else sys.stdin.read()

session_dir = None
session_id = None
prev = ""
for arg in args:
    if prev == "--session-dir":
        session_dir = arg
    elif prev == "--session-id":
        session_id = arg
    prev = arg
if session_dir:
    sd = Path(session_dir)
    sd.mkdir(parents=True, exist_ok=True)
    (sd / f"t_{{session_id or f'f{{os.getpid()}}'}}.jsonl").write_text("", encoding="utf-8")

rules = json.loads(Path({json.dumps(str(rules_path))}).read_text(encoding="utf-8"))
haystack = stdin_text + "\\n" + " ".join(args)

chosen = None
for rule in rules:
    tokens = rule.get("match") or []
    if all(tok in haystack for tok in tokens):
        chosen = rule
        break
if chosen is None and rules:
    chosen = rules[-1]
if chosen is None:
    chosen = {{"answer": "ok"}}

for rel_path, content in (chosen.get("writes") or {{}}).items():
    target = Path.cwd() / rel_path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content, encoding="utf-8")

advance = chosen.get("advance_mainline")
marker = Path({json.dumps(str(hook_marker_path))})
if advance and not marker.exists():
    main_repo = Path({json.dumps(str(main_repo))})
    git_env = dict(os.environ)
    git_env.setdefault("GIT_AUTHOR_NAME", "Novice Eval")
    git_env.setdefault("GIT_AUTHOR_EMAIL", "novice@example.com")
    git_env.setdefault("GIT_COMMITTER_NAME", "Novice Eval")
    git_env.setdefault("GIT_COMMITTER_EMAIL", "novice@example.com")
    for rel_path, content in (advance.get("files") or {{}}).items():
        target = main_repo / rel_path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(content, encoding="utf-8")
    subprocess.run(["git", "-C", str(main_repo), "add", "-A"], env=git_env, check=True, capture_output=True)
    subprocess.run(
        ["git", "-C", str(main_repo), "commit", "-q", "-m", advance["commit_message"]],
        env=git_env,
        check=True,
        capture_output=True,
    )
    sha = subprocess.check_output(
        ["git", "-C", str(main_repo), "rev-parse", "HEAD"],
        env=git_env,
        text=True,
    ).strip()
    with open({json.dumps(str(hook_commits_path))}, "a", encoding="utf-8") as hfh:
        hfh.write(sha + "\\n")
    marker.write_text(sha + "\\n", encoding="utf-8")

with open({json.dumps(str(log_path))}, "a", encoding="utf-8") as fh:
    fh.write(json.dumps({{"cwd": os.getcwd(), "args": args, "matched": chosen.get("match")}}) + "\\n")

answer_event = {{
    "type": "message_end",
    "message": {{
        "role": "assistant",
        "stopReason": "stop",
        "model": "fake-model",
        "usage": {{"input": 10, "output": 5, "cacheRead": 0}},
        "content": [{{"type": "text", "text": chosen.get("answer", "ok")}}],
    }},
}}
print(json.dumps(answer_event))
print(json.dumps({{"type": "agent_settled"}}))
"""
    pi_path = bin_dir / "pi"
    pi_path.write_text(script, encoding="utf-8")
    pi_path.chmod(0o755)
    return pi_path


class ScenarioContext:
    def __init__(
        self,
        scenario: dict[str, Any],
        delegate_bin: Path,
        work_root: Path,
        skill_md: str,
    ) -> None:
        self.scenario = scenario
        self.delegate_bin = delegate_bin
        self.work = work_root
        self.skill_md = skill_md
        self.repo = work_root / "repo"
        self.bin_dir = work_root / "bin"
        self.home_dir = work_root / "home"
        self.runs_dir = work_root / "runs"
        self.state_home = work_root / "state"
        self.cache_home = work_root / "cache"
        self.config_home = work_root / "config"
        self.pi_log = work_root / "pi.log"
        self.rules_path = work_root / "fake_pi_rules.json"
        self.hook_commits_path = work_root / "hook_commits.txt"
        self.hook_marker_path = work_root / ".hook-advanced"
        self.steps: list[CommandStep] = []
        self.vars: dict[str, str] = {}
        self.hook_commits: set[str] = set()
        self.base_head: str = ""
        self.work_id: str | None = None
        self.env: dict[str, str] = {}

        self._setup_isolated_env()
        self._setup_repo()
        self._setup_work_item()

    def _setup_isolated_env(self) -> None:
        for d in (
            self.bin_dir,
            self.home_dir,
            self.runs_dir,
            self.state_home,
            self.cache_home,
            self.config_home / "delegate",
        ):
            d.mkdir(parents=True, exist_ok=True)

        self.env = {
            k: v
            for k, v in os.environ.items()
            if not k.startswith(("DELEGATE_", "PI_DELEGATE_", "ANTIGRAVITY_")) and k not in CALLER_ENV
        }
        clean_path = os.pathsep.join(
            p
            for p in os.environ.get("PATH", "").split(os.pathsep)
            if Path(p).name not in {"agent-shims", "measure-shims", "resource-shims", "remote-shims"}
        )
        self.env.update(
            HOME=str(self.home_dir),
            PATH=f"{self.bin_dir}:{clean_path}",
            DELEGATE_POLL="0.1",
            DELEGATE_RUNS=str(self.runs_dir),
            XDG_STATE_HOME=str(self.state_home),
            XDG_CACHE_HOME=str(self.cache_home),
            XDG_CONFIG_HOME=str(self.config_home),
            DELEGATE_PREMERGE="0",
            DELEGATE_SCOUT_FIRST="0",
            DELEGATE_SESSION_HARNESS_PID="0",
            DELEGATE_SCOUT_MODEL="p/m",
            DELEGATE_REVIEW_MODEL="p/m",
            DELEGATE_WRITE_MODEL="p/m",
            GIT_AUTHOR_NAME="Novice Eval",
            GIT_AUTHOR_EMAIL="novice@example.com",
            GIT_COMMITTER_NAME="Novice Eval",
            GIT_COMMITTER_EMAIL="novice@example.com",
            PYTHONDONTWRITEBYTECODE="1",
        )
        (self.config_home / "delegate" / "config.json").write_text(
            json.dumps({"models": REGISTERED_MODELS}),
            encoding="utf-8",
        )
        for stub in ("docker", "systemctl"):
            stub_path = self.bin_dir / stub
            stub_path.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            stub_path.chmod(0o755)

        self.rules_path.write_text(
            json.dumps(self.scenario.get("fake_pi_rules", []), ensure_ascii=False, indent=2),
            encoding="utf-8",
        )
        install_fake_pi(
            self.bin_dir,
            self.rules_path,
            self.pi_log,
            self.repo,
            self.hook_commits_path,
            self.hook_marker_path,
        )

    def _setup_repo(self) -> None:
        src_repo = Path(self.scenario["_dir"]) / "repo"
        shutil.copytree(src_repo, self.repo)
        self._git("init", "-q")
        self._git("config", "user.name", "Novice Eval")
        self._git("config", "user.email", "novice@example.com")
        self._git("add", "-A")
        self._git("commit", "-q", "-m", "chore: initial fixture")
        self.base_head = self._git("rev-parse", "HEAD").strip()

    def _setup_work_item(self) -> None:
        title = self.scenario.get("work_item")
        if not title:
            return
        res = subprocess.run(
            [str(self.delegate_bin), "work", "add", title, "--repo", str(self.repo), "--json"],
            cwd=self.repo,
            env=self.env,
            capture_output=True,
            text=True,
            check=True,
        )
        self.work_id = json.loads(res.stdout)["id"]
        self.vars["work_id"] = self.work_id

    def _git(self, *args: str) -> str:
        res = subprocess.run(
            ["git", "-C", str(self.repo), *args],
            env=self.env,
            capture_output=True,
            text=True,
            check=True,
        )
        return res.stdout

    def advance_mainline(self, files: dict[str, str], commit_message: str) -> str:
        for rel, content in files.items():
            target = self.repo / rel
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")
        self._git("add", "-A")
        self._git("commit", "-q", "-m", commit_message)
        sha = self._git("rev-parse", "HEAD").strip()
        self.hook_commits.add(sha)
        return sha

    def run_delegate(self, args: list[str], timeout: int = 60) -> CommandStep:
        argv = [str(self.delegate_bin), *args]
        display = "delegate " + " ".join(shlex.quote(a) for a in args)
        res = subprocess.run(
            argv,
            cwd=self.repo,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        step = CommandStep(
            index=len(self.steps) + 1,
            command=display,
            argv=["delegate", *args],
            exit_code=res.returncode,
            stdout=res.stdout,
            stderr=res.stderr,
            cwd=str(self.repo),
        )
        self.steps.append(step)
        return step

    def sync_hook_commits(self) -> None:
        if self.hook_commits_path.is_file():
            for line in self.hook_commits_path.read_text(encoding="utf-8").splitlines():
                sha = line.strip()
                if sha:
                    self.hook_commits.add(sha)

    def run_concurrent_delegate_waits(self, runs: list[str], timeout: int = 60) -> list[CommandStep]:
        args = ["wait", "--until-all", *runs]
        argv = [str(self.delegate_bin), *args]
        display = "delegate " + " ".join(shlex.quote(a) for a in args)
        p1 = subprocess.Popen(
            argv,
            cwd=self.repo,
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        p2 = subprocess.Popen(
            argv,
            cwd=self.repo,
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        out1, err1 = p1.communicate(timeout=timeout)
        out2, err2 = p2.communicate(timeout=timeout)
        steps: list[CommandStep] = []
        for proc, out, err in ((p1, out1, err1), (p2, out2, err2)):
            step = CommandStep(
                index=len(self.steps) + 1,
                command=display,
                argv=["delegate", *args],
                exit_code=proc.returncode,
                stdout=out,
                stderr=err,
                cwd=str(self.repo),
            )
            self.steps.append(step)
            steps.append(step)
        return steps

    def run_shell(self, command: str, timeout: int = 60) -> CommandStep:
        res = subprocess.run(
            ["bash", "-lc", command],
            cwd=self.repo,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        try:
            argv = shlex.split(command)
        except ValueError:
            argv = [command]
        step = CommandStep(
            index=len(self.steps) + 1,
            command=command,
            argv=argv,
            exit_code=res.returncode,
            stdout=res.stdout,
            stderr=res.stderr,
            cwd=str(self.repo),
        )
        self.steps.append(step)
        return step

    def settle(self) -> None:
        settle_supervisors(self.runs_dir, self.state_home)


def extract_run_id(output: str) -> str | None:
    match = RUN_ID_RE.search(output)
    return match.group(1) if match else None


def render_template(template: str, vars_map: dict[str, str]) -> str:
    return re.sub(
        r"\{([a-zA-Z_][a-zA-Z0-9_]*)\}",
        lambda m: vars_map.get(m.group(1), m.group(0)),
        template,
    )


class ControllerRunner:
    """Pluggable controller interface for running a single novice scenario."""

    def run(self, ctx: ScenarioContext) -> tuple[int, list[str]]:
        raise NotImplementedError


class ScriptedSkillController(ControllerRunner):
    """Zero-token deterministic replay strictly following SKILL.md top-screen commands."""

    def run(self, ctx: ScenarioContext) -> tuple[int, list[str]]:
        errors: list[str] = []
        for spec in ctx.scenario.get("scripted_steps", []):
            op = spec["op"]
            if op == "hook_advance_mainline":
                ctx.advance_mainline(spec["files"], spec["commit_message"])
                continue

            if op == "start_read_only":
                prompt = render_template(spec["prompt"], ctx.vars)
                step = ctx.run_delegate(["start", "--read-only", prompt])
                if step.exit_code != 0:
                    errors.append(f"step {step.index} failed: {step.command} (exit {step.exit_code})")
                    break
                run_id = extract_run_id(step.stdout + "\n" + step.stderr)
                if not run_id:
                    errors.append(f"step {step.index} produced no run id: {step.stdout!r}")
                    break
                if spec.get("save_as"):
                    ctx.vars[spec["save_as"]] = run_id

            elif op == "start_worktree":
                prompt = render_template(spec["prompt"], ctx.vars)
                accept = render_template(spec["accept"], ctx.vars)
                step = ctx.run_delegate(["start", "--worktree", "--accept", accept, prompt])
                if step.exit_code != 0:
                    errors.append(f"step {step.index} failed: {step.command} (exit {step.exit_code})")
                    break
                run_id = extract_run_id(step.stdout + "\n" + step.stderr)
                if not run_id:
                    errors.append(f"step {step.index} produced no run id: {step.stdout!r}")
                    break
                if spec.get("save_as"):
                    ctx.vars[spec["save_as"]] = run_id

            elif op == "wait":
                args = ["wait"]
                if spec.get("run"):
                    args.append(render_template(spec["run"], ctx.vars))
                step = ctx.run_delegate(args)
                if step.exit_code != 0:
                    errors.append(f"step {step.index} failed: {step.command} (exit {step.exit_code})")
                    break

            elif op == "wait_concurrent":
                runs = [render_template(r, ctx.vars) for r in spec["runs"]]
                steps = ctx.run_concurrent_delegate_waits(runs)
                failed = [s for s in steps if s.exit_code != 0]
                if failed:
                    for s in failed:
                        errors.append(f"step {s.index} failed: {s.command} (exit {s.exit_code})")
                    break

            elif op == "card":
                run_id = render_template(spec["run"], ctx.vars)
                step = ctx.run_delegate(["card", run_id])
                if step.exit_code != 0:
                    errors.append(f"step {step.index} failed: {step.command} (exit {step.exit_code})")
                    break

            elif op == "reply":
                run_id = render_template(spec["run"], ctx.vars)
                prompt = render_template(spec["prompt"], ctx.vars)
                args = ["reply"]
                if spec.get("sync"):
                    args.append(f"--sync={spec['sync']}")
                args.extend([run_id, prompt])
                step = ctx.run_delegate(args)
                if step.exit_code != 0:
                    errors.append(f"step {step.index} failed: {step.command} (exit {step.exit_code})")
                    break
                new_run_id = extract_run_id(step.stdout + "\n" + step.stderr)
                if spec.get("save_as"):
                    if not new_run_id:
                        errors.append(f"step {step.index} reply produced no run id: {step.stdout!r}")
                        break
                    ctx.vars[spec["save_as"]] = new_run_id

            elif op == "land":
                run_id = render_template(spec["run"], ctx.vars)
                message = render_template(spec["message"], ctx.vars)
                step = ctx.run_delegate(["land", run_id, "-m", message])
                if spec.get("expect_conflict"):
                    lines = [ln for ln in step.stdout.splitlines() if ln.strip()]
                    if step.exit_code != 3:
                        errors.append(
                            f"step {step.index} expected conflict exit 3, got {step.exit_code}: {step.stdout}"
                        )
                        break
                    if not lines or "LAND FAILED（未合入：文件冲突）" not in lines[0]:
                        errors.append(
                            f"step {step.index} missing expected LAND FAILED header: {lines[:1]!r}"
                        )
                        break
                elif step.exit_code != 0:
                    errors.append(f"step {step.index} failed: {step.command} (exit {step.exit_code})")
                    break

            elif op == "work_close":
                work_id = render_template(spec["work_id"], ctx.vars)
                reason = render_template(spec["reason"], ctx.vars)
                step = ctx.run_delegate(["work", "close", work_id, "--reason", reason])
                if step.exit_code != 0:
                    errors.append(f"step {step.index} failed: {step.command} (exit {step.exit_code})")
                    break

            else:
                errors.append(f"unknown scripted op: {op}")
                break

        return 0, errors


def build_model_controller_env(
    ctx_env: dict[str, str],
    bin_dir: Path,
    base_env: dict[str, str] | None = None,
) -> dict[str, str]:
    """Build isolated env for real-model controller: strip host delegate/caller vars, keep auth."""
    source_env = os.environ if base_env is None else base_env
    controller_env = {
        k: v
        for k, v in source_env.items()
        if not k.startswith(("DELEGATE_", "PI_DELEGATE_", "ANTIGRAVITY_")) and k not in CALLER_ENV
    }
    clean_path = os.pathsep.join(
        p
        for p in source_env.get("PATH", "").split(os.pathsep)
        if Path(p).name not in {"agent-shims", "measure-shims", "resource-shims", "remote-shims"}
    )
    for key in (
        "DELEGATE_POLL",
        "DELEGATE_RUNS",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "XDG_CONFIG_HOME",
        "DELEGATE_PREMERGE",
        "DELEGATE_SCOUT_FIRST",
        "DELEGATE_SESSION_HARNESS_PID",
        "DELEGATE_SCOUT_MODEL",
        "DELEGATE_REVIEW_MODEL",
        "DELEGATE_WRITE_MODEL",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "PYTHONDONTWRITEBYTECODE",
    ):
        if key in ctx_env:
            controller_env[key] = ctx_env[key]
    controller_env["PATH"] = f"{bin_dir}:{clean_path}" if clean_path else str(bin_dir)
    return controller_env


def build_model_controller_argv(
    host_pi: str,
    model: str,
    session_dir: Path,
    prompt: str,
) -> list[str]:
    """Build pi CLI argv for memoryless novice controller (no host skills/context/extensions)."""
    return [
        host_pi,
        "--mode",
        "json",
        "--model",
        model,
        "--no-skills",
        "--no-context-files",
        "--no-extensions",
        "--session-dir",
        str(session_dir),
        "-p",
        prompt,
    ]


class ModelController(ControllerRunner):
    """Real LLM controller via host `pi` CLI (used only with --model, never in verify.sh)."""

    def __init__(self, model: str, host_pi: str | None = None) -> None:
        self.model = model
        self.host_pi = host_pi or shutil.which("pi")
        if not self.host_pi:
            raise RuntimeError("--model requires `pi` CLI on PATH")

    def run(self, ctx: ScenarioContext) -> tuple[int, list[str]]:
        description = render_template(ctx.scenario["description"], ctx.vars)
        prompt = (
            "你是一个没有任何历史记忆的新手主控，只能依据下方 SKILL.md 完成任务。\n"
            f"工作仓库：{ctx.repo}\n"
            f"delegate 二进制：D={ctx.delegate_bin}\n"
            f"任务说明：{description}\n\n"
            "=== SKILL.md ===\n"
            f"{ctx.skill_md}\n"
        )
        controller_env = build_model_controller_env(ctx.env, ctx.bin_dir)
        session_dir = ctx.work / "controller-session"
        session_dir.mkdir(parents=True, exist_ok=True)
        res = subprocess.run(
            build_model_controller_argv(self.host_pi, self.model, session_dir, prompt),
            cwd=ctx.repo,
            env=controller_env,
            capture_output=True,
            text=True,
            timeout=300,
        )
        input_tokens = 0
        errors: list[str] = []
        pending_cmds: dict[str, str] = {}
        for line in res.stdout.splitlines():
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if ev.get("type") == "message_end":
                usage = (ev.get("message") or {}).get("usage") or {}
                input_tokens += int(usage.get("input") or 0)
            elif ev.get("type") == "tool_execution_start":
                tool_id = str(ev.get("toolCallId") or "")
                args = ev.get("args") or {}
                cmd = args.get("command") or args.get("cmd")
                if cmd:
                    pending_cmds[tool_id] = str(cmd)
            elif ev.get("type") == "tool_execution_end":
                tool_id = str(ev.get("toolCallId") or "")
                cmd = pending_cmds.pop(tool_id, None)
                if cmd is not None:
                    result = ev.get("result") or {}
                    details = result.get("details") or {}
                    exit_code = int(details.get("exitCode", 1 if ev.get("isError") else 0))
                    out_text = ""
                    for block in result.get("content") or []:
                        if isinstance(block, dict) and block.get("type") == "text":
                            out_text += block.get("text", "")
                    try:
                        argv = shlex.split(cmd)
                    except ValueError:
                        argv = [cmd]
                    ctx.steps.append(
                        CommandStep(
                            index=len(ctx.steps) + 1,
                            command=cmd,
                            argv=argv,
                            exit_code=exit_code,
                            stdout=out_text,
                            stderr="",
                            cwd=str(ctx.repo),
                        )
                    )
        if res.returncode != 0:
            errors.append(f"model controller exited with {res.returncode}: {res.stderr.strip()}")
        return input_tokens, errors


ENV_ASSIGN_RE = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*=")
SHELL_SEPARATORS = {";", "&&", "||", "|", "&"}
GIT_GLOBAL_VALUE_FLAGS = {"-C", "-c", "--git-dir", "--work-tree", "--namespace", "--config-env"}


def _split_shell_clauses(command: str) -> list[tuple[list[str], str | None]]:
    """Split a shell command into (tokens, trailing_separator) clauses using punctuation-aware shlex."""
    lexer = shlex.shlex(command, posix=True, punctuation_chars=True)
    lexer.whitespace_split = True
    lexer.commenters = "#"
    try:
        tokens = list(lexer)
    except ValueError:
        tokens = command.split()

    clauses: list[tuple[list[str], str | None]] = []
    current: list[str] = []
    for tok in tokens:
        if tok in SHELL_SEPARATORS:
            if current:
                clauses.append((current, tok))
                current = []
            continue
        current.append(tok)
    if current:
        clauses.append((current, None))
    return clauses


def _strip_env_prefix(tokens: list[str]) -> list[str]:
    idx = 0
    while idx < len(tokens):
        tok = tokens[idx]
        if ENV_ASSIGN_RE.match(tok):
            idx += 1
            continue
        if Path(tok).name == "env":
            idx += 1
            while idx < len(tokens):
                t = tokens[idx]
                if t in {"-u", "--unset"} and idx + 1 < len(tokens):
                    idx += 2
                elif t.startswith("-") or ENV_ASSIGN_RE.match(t):
                    idx += 1
                else:
                    break
            continue
        break
    return tokens[idx:]


def _extract_shell_clauses(command: str) -> list[tuple[list[str], str | None]]:
    clauses = _split_shell_clauses(command)
    expanded: list[tuple[list[str], str | None]] = []
    for seg, sep in clauses:
        stripped = _strip_env_prefix(seg)
        if (
            len(stripped) >= 3
            and Path(stripped[0]).name in {"sh", "bash", "zsh"}
            and stripped[1] in {"-c", "-lc"}
        ):
            inner = _split_shell_clauses(stripped[2])
            if inner:
                for i, (iseg, isep) in enumerate(inner):
                    expanded.append((iseg, isep if i + 1 < len(inner) else sep))
                continue
        expanded.append((seg, sep))
    return expanded


def _clause_delegate_invocation(tokens: list[str]) -> tuple[bool, str | None]:
    stripped = _strip_env_prefix(tokens)
    if not stripped:
        return False, None
    head = Path(stripped[0]).name
    if head == "delegate" or stripped[0] in {"$D", '"$D"'}:
        for tok in stripped[1:]:
            if not tok.startswith("-"):
                return True, tok
        return True, None
    return False, None


def _has_manual_polling(clauses: list[tuple[list[str], str | None]]) -> str | None:
    for seg, _ in clauses:
        is_del, _ = _clause_delegate_invocation(seg)
        if is_del:
            continue
        for tok in seg:
            if tok in POLL_WORDS:
                return tok
    return None


def _is_test_clause(tokens: list[str], test_cmd: str | None = None) -> bool:
    is_del, _ = _clause_delegate_invocation(tokens)
    if is_del:
        return False
    stripped = _strip_env_prefix(tokens)
    if not stripped:
        return False
    joined = " ".join(stripped)
    if test_cmd and test_cmd.strip() and test_cmd.strip() in joined:
        return True
    for pat in TEST_CMD_PATTERNS:
        if pat.search(joined):
            return True
    return False


def _has_unapplied_failure_marker(step: CommandStep, single_apply_or_land: bool) -> bool:
    combined = step.stdout + "\n" + step.stderr
    if "FAILED（未合入" in combined or "nothing applied" in combined:
        return True
    return single_apply_or_land and step.exit_code == 3


def _git_commit_clause_info(tokens: list[str]) -> tuple[bool, bool]:
    """Return (is_git_commit, uses_message_file) for a shell clause."""
    stripped = _strip_env_prefix(tokens)
    if len(stripped) < 2 or Path(stripped[0]).name != "git":
        return False, False
    idx = 1
    while idx < len(stripped):
        tok = stripped[idx]
        if tok in GIT_GLOBAL_VALUE_FLAGS:
            idx += 2
            continue
        if tok.startswith("-"):
            idx += 1
            continue
        if tok == "commit":
            rest = stripped[idx + 1 :]
            uses_file = any(
                t in {"-F", "--file"} or t.startswith(("-F", "--file="))
                for t in rest
            )
            return True, uses_file
        return False, False
    return False, False


def _read_file_target(clauses: list[tuple[list[str], str | None]]) -> str | None:
    if len(clauses) != 1:
        return None
    stripped = _strip_env_prefix(clauses[0][0])
    if not stripped or Path(stripped[0]).name not in READ_COMMANDS:
        return None
    paths = [tok for tok in stripped[1:] if not tok.startswith("-")]
    return paths[-1] if len(paths) == 1 else None


def analyze_trajectory(
    steps: list[CommandStep],
    *,
    repo_path: Path | None = None,
    base_head: str | None = None,
    hook_commits: set[str] | None = None,
    test_cmd: str | None = None,
    env: dict[str, str] | None = None,
) -> list[WastedStep]:
    """Deterministically detect wasted steps from recorded command trajectory and final git log."""
    wasted: list[WastedStep] = []
    unapplied_failed = False
    read_files_seen: set[str] = set()
    has_git_ground_truth = bool(repo_path and base_head)

    for step in steps:
        clauses = _extract_shell_clauses(step.command)
        delegate_invocations = [_clause_delegate_invocation(seg) for seg, _ in clauses]
        has_delegate = any(is_del for is_del, _ in delegate_invocations)
        delegate_subcmds = [sub for is_del, sub in delegate_invocations if is_del]

        # 1. exit 2 unknown argument / usage error (including `delegate --bogus` with no subcmd)
        if has_delegate and step.exit_code == 2:
            wasted.append(
                WastedStep(
                    category="exit_2_unknown_arg",
                    step_index=step.index,
                    command=step.command,
                    exit_code=step.exit_code,
                    reason="delegate command exited with code 2 (unknown argument / usage error)",
                )
            )

        # 2. exit 76 duplicate wait
        if "wait" in delegate_subcmds and step.exit_code == 76:
            wasted.append(
                WastedStep(
                    category="exit_76_duplicate_wait",
                    step_index=step.index,
                    command=step.command,
                    exit_code=step.exit_code,
                    reason="duplicate delegate wait rejected with exit 76",
                )
            )

        # 3. manual polling (sleep / while / until)
        poll_word = _has_manual_polling(clauses)
        if poll_word is not None:
            wasted.append(
                WastedStep(
                    category="manual_polling",
                    step_index=step.index,
                    command=step.command,
                    exit_code=step.exit_code,
                    reason=f"manual polling primitive `{poll_word}` used instead of delegate wait",
                )
            )

        # 4. running tests on an unapplied tree after apply/land failed (both across steps and in `;` chains)
        apply_indices = [
            i for i, (is_del, sub) in enumerate(delegate_invocations) if is_del and sub in {"apply", "land"}
        ]
        if apply_indices:
            single_apply = len(clauses) == 1
            if _has_unapplied_failure_marker(step, single_apply):
                unapplied_failed = True
                # Check if a later clause in the same command ran tests via non-short-circuiting `;` / `||`.
                first_apply_idx = apply_indices[0]
                reached_without_and_shortcircuit = False
                for idx in range(first_apply_idx, len(clauses) - 1):
                    sep = clauses[idx][1]
                    if sep in {";", "||", "&"}:
                        reached_without_and_shortcircuit = True
                    next_seg = clauses[idx + 1][0]
                    if reached_without_and_shortcircuit and _is_test_clause(next_seg, test_cmd):
                        wasted.append(
                            WastedStep(
                                category="test_on_unapplied_tree",
                                step_index=step.index,
                                command=step.command,
                                exit_code=step.exit_code,
                                reason="ran tests after failed apply/land in the same `;` command chain",
                            )
                        )
                        break
            elif step.exit_code == 0:
                unapplied_failed = False
                read_files_seen.clear()
        elif unapplied_failed and any(_is_test_clause(seg, test_cmd) for seg, _ in clauses):
            wasted.append(
                WastedStep(
                    category="test_on_unapplied_tree",
                    step_index=step.index,
                    command=step.command,
                    exit_code=step.exit_code,
                    reason="ran tests on unapplied workspace after apply/land failed",
                )
            )

        # 5. manual git commit missing Delegate-Run trailer (heuristic fallback only when no git repo ground truth)
        if not has_git_ground_truth and step.exit_code == 0:
            for seg, _ in clauses:
                is_commit, uses_file = _git_commit_clause_info(seg)
                if is_commit and not uses_file and "Delegate-Run:" not in step.command:
                    wasted.append(
                        WastedStep(
                            category="commit_missing_trailer",
                            step_index=step.index,
                            command=step.command,
                            exit_code=step.exit_code,
                            reason="manual git commit without Delegate-Run trailer",
                        )
                    )
                    break

        # 6. redundant read of the same unchanged file
        read_target = _read_file_target(clauses)
        if read_target is not None:
            if read_target in read_files_seen:
                wasted.append(
                    WastedStep(
                        category="redundant_file_read",
                        step_index=step.index,
                        command=step.command,
                        exit_code=step.exit_code,
                        reason=f"repeated read of unchanged file `{read_target}`",
                    )
                )
            else:
                read_files_seen.add(read_target)
        elif not has_delegate and not any(_is_test_clause(seg, test_cmd) for seg, _ in clauses):
            # Non-read shell command may mutate files; clear read cache.
            read_files_seen.clear()

    # Ground-truth check against actual git commits since base_head (excluding fixture hook commits).
    if has_git_ground_truth and repo_path is not None and base_head is not None:
        res = subprocess.run(
            ["git", "-C", str(repo_path), "rev-list", f"{base_head}..HEAD"],
            env=env,
            capture_output=True,
            text=True,
        )
        if res.returncode == 0:
            hooks = hook_commits or set()
            for sha in [line.strip() for line in res.stdout.splitlines() if line.strip()]:
                if sha in hooks:
                    continue
                body = subprocess.run(
                    ["git", "-C", str(repo_path), "log", "-1", "--format=%B", sha],
                    env=env,
                    capture_output=True,
                    text=True,
                ).stdout
                if "Delegate-Run:" not in body:
                    wasted.append(
                        WastedStep(
                            category="commit_missing_trailer",
                            step_index=len(steps),
                            command=f"git commit ({sha[:8]})",
                            exit_code=0,
                            reason=f"commit {sha[:8]} is missing `Delegate-Run:` trailer",
                        )
                    )

    return wasted


def verify_final_state(ctx: ScenarioContext) -> tuple[dict[str, bool], list[str]]:
    spec = ctx.scenario.get("verify", {})
    checks: dict[str, bool] = {}
    failures: list[str] = []

    # 1. Expected file contents
    files_ok = True
    for rel, needles in (spec.get("expected_files") or {}).items():
        path = ctx.repo / rel
        if not path.is_file():
            files_ok = False
            failures.append(f"expected file missing: {rel}")
            continue
        text = path.read_text(encoding="utf-8")
        for needle in needles:
            if needle not in text:
                files_ok = False
                failures.append(f"file {rel} missing expected snippet: {needle!r}")
    checks["expected_files"] = files_ok

    # 2. Delegate-Run trailer commits
    min_trailers = int(spec.get("min_trailer_commits", 1))
    revs = ctx._git("rev-list", f"{ctx.base_head}..HEAD").splitlines()
    trailer_count = 0
    for sha in revs:
        if sha in ctx.hook_commits:
            continue
        body = ctx._git("log", "-1", "--format=%B", sha)
        if "Delegate-Run:" in body:
            trailer_count += 1
    trailer_ok = trailer_count >= min_trailers
    checks["delegate_run_trailer"] = trailer_ok
    if not trailer_ok:
        failures.append(f"expected >= {min_trailers} commit(s) with Delegate-Run trailer, got {trailer_count}")

    # 3. Repository tests pass
    if spec.get("test_must_pass", True):
        test_cmd = ctx.scenario["test_cmd"]
        test_res = subprocess.run(
            ["bash", "-lc", test_cmd],
            cwd=ctx.repo,
            env=ctx.env,
            capture_output=True,
            text=True,
            timeout=60,
        )
        checks["tests_pass"] = test_res.returncode == 0
        if test_res.returncode != 0:
            failures.append(f"final test `{test_cmd}` failed (exit {test_res.returncode}): {test_res.stdout}{test_res.stderr}")

    # 4. Worktrees cleaned (every write/landed worktree removed; read-only scout worktrees are GC'd lazily)
    if spec.get("worktrees_cleaned", True):
        read_only_worktrees: set[Path] = set()
        if ctx.runs_dir.is_dir():
            for meta_file in ctx.runs_dir.glob("*/meta.json"):
                try:
                    meta = json.loads(meta_file.read_text(encoding="utf-8"))
                except (OSError, json.JSONDecodeError):
                    continue
                if meta.get("mode") == "read-only":
                    wt_val = meta.get("worktree")
                    wt_path = wt_val.get("path") if isinstance(wt_val, dict) else wt_val
                    if isinstance(wt_path, str) and wt_path:
                        read_only_worktrees.add(Path(wt_path).resolve())
                    wd_val = meta.get("workdir")
                    if isinstance(wd_val, str) and wd_val:
                        read_only_worktrees.add(Path(wd_val).resolve())
        wt_out = ctx._git("worktree", "list", "--porcelain")
        wt_paths = [
            Path(ln.split(" ", 1)[1]).resolve()
            for ln in wt_out.splitlines()
            if ln.startswith("worktree ")
        ]
        leftover = [
            str(p)
            for p in wt_paths
            if p != ctx.repo.resolve() and p not in read_only_worktrees
        ]
        wt_ok = len(leftover) == 0
        checks["worktrees_cleaned"] = wt_ok
        if not wt_ok:
            failures.append(f"leftover write git worktrees: {leftover}")

    # 5. Work item closed (S4)
    if spec.get("work_item_done") and ctx.work_id:
        shown = subprocess.run(
            [str(ctx.delegate_bin), "work", "show", ctx.work_id, "--json"],
            cwd=ctx.repo,
            env=ctx.env,
            capture_output=True,
            text=True,
        )
        done_ok = False
        if shown.returncode == 0:
            doc = json.loads(shown.stdout)
            done_ok = doc.get("state") == "done"
        checks["work_item_done"] = done_ok
        if not done_ok:
            failures.append(f"work item {ctx.work_id} not closed as done: {shown.stdout}{shown.stderr}")

    return checks, failures


def run_scenario(
    scenario: dict[str, Any],
    delegate_bin: Path,
    controller: ControllerRunner,
    skill_md: str,
    *,
    keep_temp: bool = False,
) -> ScenarioResult:
    tmp_dir = Path(tempfile.mkdtemp(prefix=f"novice-{scenario['id']}-"))
    ctx: ScenarioContext | None = None
    try:
        ctx = ScenarioContext(scenario, delegate_bin, tmp_dir, skill_md)
        input_tokens, controller_errors = controller.run(ctx)
        ctx.settle()
        ctx.sync_hook_commits()

        checks, state_failures = verify_final_state(ctx)
        wasted = analyze_trajectory(
            ctx.steps,
            repo_path=ctx.repo,
            base_head=ctx.base_head,
            hook_commits=ctx.hook_commits,
            test_cmd=scenario.get("test_cmd"),
            env=ctx.env,
        )
        failures = [*controller_errors, *state_failures]
        failed_commands = [
            {
                "step": s.index,
                "command": s.command,
                "exit_code": s.exit_code,
                "stderr": s.stderr.strip()[:300],
            }
            for s in ctx.steps
            if s.exit_code != 0
        ]
        passed = not failures and all(checks.values()) and len(wasted) == 0
        return ScenarioResult(
            id=scenario["id"],
            title=scenario["title"],
            passed=passed,
            checks=checks,
            failures=failures,
            total_steps=len(ctx.steps),
            input_tokens=input_tokens,
            wasted_steps=wasted,
            failed_commands=failed_commands,
        )
    finally:
        if ctx is not None:
            ctx.settle()
        if not keep_temp:
            shutil.rmtree(tmp_dir, ignore_errors=True)


def format_report(results: list[ScenarioResult], mode: str) -> str:
    lines: list[str] = [f"Novice replay ({mode}): {sum(r.passed for r in results)}/{len(results)} passed"]
    total_steps = sum(r.total_steps for r in results)
    total_tokens = sum(r.input_tokens for r in results)
    total_wasted = sum(len(r.wasted_steps) for r in results)

    for r in results:
        status = "PASS" if r.passed else "FAIL"
        checks_str = ", ".join(f"{k}={'ok' if v else 'FAIL'}" for k, v in r.checks.items())
        lines.append(
            f"  [{status}] {r.id} ({r.title}) | steps={r.total_steps} tokens={r.input_tokens} "
            f"wasted={len(r.wasted_steps)} | {checks_str}"
        )
        for f in r.failures:
            lines.append(f"      failure: {f}")
        for w in r.wasted_steps:
            lines.append(
                f"      wasted[{w.category}] step #{w.step_index} (exit {w.exit_code}): "
                f"{w.command} -- {w.reason}"
            )
        for fc in r.failed_commands:
            lines.append(
                f"      nonzero step #{fc['step']} (exit {fc['exit_code']}): {fc['command']}"
                + (f" | {fc['stderr']}" if fc["stderr"] else "")
            )

    lines.append(
        f"Summary: total_steps={total_steps}, input_tokens={total_tokens}, wasted_steps={total_wasted}"
    )
    return "\n".join(lines)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Memoryless novice replay benchmark for delegate skill")
    parser.add_argument("--model", help="Run with a real controller model via `pi` (excluded from verify.sh)")
    parser.add_argument("--scenario", action="append", help="Run only the given scenario id (repeatable)")
    parser.add_argument("--fixtures", type=Path, default=DEFAULT_FIXTURES_DIR, help="Path to novice-fixtures dir")
    parser.add_argument("--json", action="store_true", help="Emit structured JSON report")
    parser.add_argument("--keep-temp", action="store_true", help="Keep temporary workdirs for debugging")
    args = parser.parse_args(argv)

    missing_skill = validate_skill_md(SKILL_MD_PATH)
    if missing_skill:
        print(f"SKILL.md missing required top-screen commands: {', '.join(missing_skill)}", file=sys.stderr)
        return 1

    skill_md = SKILL_MD_PATH.read_text(encoding="utf-8")
    delegate_bin = resolve_delegate_binary(ROOT)
    selected = set(args.scenario) if args.scenario else None
    scenarios = load_scenarios(args.fixtures, selected=selected)
    if not scenarios:
        print(f"No scenarios found in {args.fixtures}", file=sys.stderr)
        return 1

    if args.model:
        controller: ControllerRunner = ModelController(args.model)
        mode = f"model:{args.model}"
    else:
        controller = ScriptedSkillController()
        mode = "scripted-zero-token"

    results = [
        run_scenario(s, delegate_bin, controller, skill_md, keep_temp=args.keep_temp)
        for s in scenarios
    ]

    if args.json:
        payload = {
            "mode": mode,
            "passed": all(r.passed for r in results),
            "total_steps": sum(r.total_steps for r in results),
            "input_tokens": sum(r.input_tokens for r in results),
            "wasted_step_count": sum(len(r.wasted_steps) for r in results),
            "scenarios": [r.to_dict() for r in results],
        }
        print(json.dumps(payload, ensure_ascii=False, indent=2))
    else:
        print(format_report(results, mode))

    return 0 if all(r.passed for r in results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
