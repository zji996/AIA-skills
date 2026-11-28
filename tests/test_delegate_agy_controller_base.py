"""Tests for agy-as-controller foundation:
1. `ANTIGRAVITY_CONVERSATION_ID` caller recognition, priority with `ANTIGRAVITY_AGENT` /
   `DELEGATE_CALLER`, and `work` `AutoDispatch` preserving initiator caller.
2. `clean_env` stripping parent controller `ANTIGRAVITY_*` & session env vars on colleague launch.
3. `wait` 50m check-in default active for `ANTIGRAVITY_CONVERSATION_ID` sessions.
4. `wait --machine` caller filtering by default and `--all` cross-session opt-in.
5. `apply` idempotency inside `apply.lock` (reports `已于 <at> 合入` with zero changes).
6. PreToolUse hook (`claude-code-background.py`) supporting `agy` `run_command` format and
   colleague bypass (bypasses only wait rule; keeps `git prune` guard active).
7. Caller liveness/expiry using delegate's own `sessions.json` timestamps (no `CLAUDE_PID`/`.claude/`).
"""
import json
import os
import shlex
import subprocess
import sys
import threading
import time
import unittest
from pathlib import Path

import test_delegate as fixtures

ROOT = Path(__file__).resolve().parent.parent
BACKGROUND_HOOK = ROOT / "skills/delegate/hooks/claude-code-background.py"
STOP_HOOK = ROOT / "skills/delegate/hooks/claude-code-stop.py"

CONTROLLER_STRIPPED_VARS = (
    "ANTIGRAVITY_LS_ADDRESS",
    "ANTIGRAVITY_CSRF_TOKEN",
    "ANTIGRAVITY_CONVERSATION_ID",
    "ANTIGRAVITY_TRAJECTORY_ID",
    "ANTIGRAVITY_SOURCE_METADATA",
    "ANTIGRAVITY_AGENT",
    "ANTIGRAVITY_APP_DATA_DIR",
    "ANTIGRAVITY_PROJECT_ID",
    "ANTIGRAVITY_AGENTAPI_EXE",
    "ANTIGRAVITY_LS_VERSION",
    "ANTIGRAVITY_FUTURE_CUSTOM",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_THREAD_ID",
    "PI_SESSION_ID",
    "DELEGATE_CALLER",
)


def iso_ago(hours: float) -> str:
    ts = int(time.time() - hours * 3600)
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts))


class DelegateAgyControllerBaseTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def clean_caller_env(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        env = {
            k: v
            for k, v in self.env.items()
            if k not in CONTROLLER_STRIPPED_VARS and k not in fixtures.CALLER_ENV
        }
        if extra:
            env.update(extra)
        return env

    def run_delegate(self, args: list[str], cwd: Path, env: dict[str, str], timeout: int = 30):
        return subprocess.run(
            [str(fixtures.DELEGATE), *args],
            cwd=cwd,
            capture_output=True,
            text=True,
            env=env,
            timeout=timeout,
        )

    # 1. Caller recognition, priority & AutoDispatch caller preservation
    def test_caller_env_antigravity_priority_and_work_auto_dispatch_caller(self):
        repo = self.repo({"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])

        # (a) ANTIGRAVITY_AGENT=1 + ANTIGRAVITY_CONVERSATION_ID beats CLAUDE_CODE_SESSION_ID.
        agy_env = self.clean_caller_env({
            "ANTIGRAVITY_AGENT": "1",
            "ANTIGRAVITY_CONVERSATION_ID": "agy-conv-1",
            "CLAUDE_CODE_SESSION_ID": "claude-outer-1",
            "CODEX_THREAD_ID": "codex-outer-1",
            "PI_SESSION_ID": "pi-outer-1",
        })
        r1 = self.outcome(self.run_delegate(["run", "--json", "--read-only", "--workdir", str(repo), "t1"], repo, agy_env))
        meta1 = json.loads((Path(r1["dir"]) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta1["caller"], "agy-conv-1")
        self.assertEqual(meta1["callerSource"], "ANTIGRAVITY_CONVERSATION_ID")

        # (b) DELEGATE_CALLER beats ANTIGRAVITY_CONVERSATION_ID even with ANTIGRAVITY_AGENT=1.
        explicit_env = {**agy_env, "DELEGATE_CALLER": "explicit-caller"}
        r2 = self.outcome(self.run_delegate(["run", "--json", "--read-only", "--workdir", str(repo), "t2"], repo, explicit_env))
        meta2 = json.loads((Path(r2["dir"]) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta2["caller"], "explicit-caller")
        self.assertEqual(meta2["callerSource"], "DELEGATE_CALLER")

        # (c) Without ANTIGRAVITY_AGENT, CLAUDE_CODE_SESSION_ID beats stale ANTIGRAVITY_CONVERSATION_ID.
        claude_env = self.clean_caller_env({
            "ANTIGRAVITY_CONVERSATION_ID": "agy-stale",
            "CLAUDE_CODE_SESSION_ID": "claude-real",
        })
        r3 = self.outcome(self.run_delegate(["run", "--json", "--read-only", "--workdir", str(repo), "t3"], repo, claude_env))
        meta3 = json.loads((Path(r3["dir"]) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta3["caller"], "claude-real")
        self.assertEqual(meta3["callerSource"], "CLAUDE_CODE_SESSION_ID")

        # (d) AutoDispatch records and preserves the initiator's agy caller when triggered by another session.
        brief = repo / "docs/auto-brief.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# Auto task\nDo work\n", encoding="utf-8")

        w_blocker = json.loads(
            self.run_delegate(["work", "add", "前置项", "--repo", str(repo), "--json"], repo, agy_env).stdout
        )
        w_auto_res = self.run_delegate(
            [
                "work", "add", "自动派发项",
                "--repo", str(repo),
                "--brief", str(brief),
                "--after-work", w_blocker["id"],
                "--auto",
                "--json",
                "--",
                "--read-only",
                "--workdir", str(repo),
            ],
            repo,
            agy_env,
        )
        self.assertEqual(w_auto_res.returncode, 0, w_auto_res.stderr)
        w_auto = json.loads(w_auto_res.stdout)
        self.assertEqual(w_auto["autoDispatch"]["caller"], "agy-conv-1")

        # Close blocker from a DIFFERENT controller session -> auto-dispatched run must still belong to agy-conv-1!
        other_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": "other-closer"})
        close_res = self.run_delegate(
            ["work", "close", w_blocker["id"], "--evidence", "done", "--json"],
            repo,
            other_env,
        )
        self.assertEqual(close_res.returncode, 0, close_res.stderr)

        w_auto_after = json.loads(
            self.run_delegate(["work", "show", w_auto["id"], "--json"], repo, other_env).stdout
        )
        dispatched_run = w_auto_after["autoDispatch"].get("dispatchedRun")
        self.assertTrue(dispatched_run, w_auto_after)
        dispatched_dir = Path(self.env["DELEGATE_RUNS"]) / dispatched_run
        self.until(lambda: (dispatched_dir / "exit_code").exists(), "auto-dispatched run finished")
        dispatched_meta = json.loads((dispatched_dir / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(dispatched_meta["caller"], "agy-conv-1")

    # 2. clean_env strips parent controller ANTIGRAVITY_* and session env vars
    def test_clean_env_strips_parent_controller_and_antigravity_vars(self):
        repo = self.repo({"a.txt": "v0\n"})
        dumped_pi = self.work / "pi-child-env.txt"
        self.fake_pi(
            [fixtures.answer("ok"), fixtures.SETTLED],
            pre=f"env > {shlex.quote(str(dumped_pi))}",
        )
        polluted_env = self.clean_caller_env({
            "ANTIGRAVITY_LS_ADDRESS": "127.0.0.1:41234",
            "ANTIGRAVITY_CSRF_TOKEN": "secret-csrf-token",
            "ANTIGRAVITY_CONVERSATION_ID": "agy-parent-conv",
            "ANTIGRAVITY_TRAJECTORY_ID": "agy-parent-traj",
            "ANTIGRAVITY_SOURCE_METADATA": "parent-meta",
            "ANTIGRAVITY_AGENT": "1",
            "ANTIGRAVITY_APP_DATA_DIR": "/tmp/parent-app-data",
            "ANTIGRAVITY_PROJECT_ID": "parent-project-123",
            "ANTIGRAVITY_AGENTAPI_EXE": "/usr/local/bin/agentapi",
            "ANTIGRAVITY_LS_VERSION": "1.5.19",
            "ANTIGRAVITY_FUTURE_CUSTOM": "future-val",
            "CLAUDE_CODE_SESSION_ID": "claude-parent",
            "CODEX_THREAD_ID": "codex-parent",
            "PI_SESSION_ID": "pi-parent",
            "DELEGATE_CALLER": "explicit-parent",
        })
        r_pi = self.outcome(
            self.run_delegate(["run", "--json", "--read-only", "--workdir", str(repo), "check-env"], repo, polluted_env)
        )
        self.assertEqual(r_pi["state"], "answered")
        pi_meta = json.loads((Path(r_pi["dir"]) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(pi_meta["caller"], "explicit-parent")
        leaked_pi = {}
        for line in dumped_pi.read_text(encoding="utf-8").splitlines():
            if "=" in line:
                k, v = line.split("=", 1)
                if k in CONTROLLER_STRIPPED_VARS or k.startswith("ANTIGRAVITY_"):
                    leaked_pi[k] = v
        self.assertEqual(leaked_pi, {}, f"parent controller env leaked to pi colleague: {leaked_pi}")

        # Also verify when launching an `agy` colleague!
        dumped_agy = self.work / "agy-child-env.txt"
        fake_agy = self.bin / "agy"
        fake_agy.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "if '--version' in sys.argv[1:]:\n"
            "    print('1.6.0')\n"
            "    sys.exit(0)\n"
            f"Path({str(dumped_agy)!r}).write_text('\\n'.join(f'{{k}}={{v}}' for k, v in os.environ.items()) + '\\n')\n"
            "sys.stdin.readline()\n"
            "print(json.dumps({'event': 'init', 'conversation_id': 'child-conv-1'}), flush=True)\n"
            "print(json.dumps({'event': 'step_update', 'step_update': {'step_index': 1, 'step_type': 'agent_response', 'state': 'DONE', 'text_delta': 'agy ok'}}), flush=True)\n"
            "print(json.dumps({'event': 'result', 'result': {'conversation_id': 'child-conv-1', 'status': 'SUCCESS', 'response': 'agy ok'}}), flush=True)\n",
            encoding="utf-8",
        )
        fake_agy.chmod(0o755)
        fixtures.DelegateTests.user_config(
            self,
            {"models": {**fixtures.REGISTERED_MODELS, "agytest": "agy/test-model"}},
        )
        r_agy = self.outcome(
            self.run_delegate(
                ["run", "--json", "--read-only", "--model", "agytest", "--workdir", str(repo), "check-agy-env"],
                repo,
                {**polluted_env, "DELEGATE_AGY_BIN": str(fake_agy)},
            )
        )
        self.assertEqual(r_agy["state"], "answered")
        leaked_agy = {}
        for line in dumped_agy.read_text(encoding="utf-8").splitlines():
            if "=" in line:
                k, v = line.split("=", 1)
                if k in CONTROLLER_STRIPPED_VARS or k.startswith("ANTIGRAVITY_"):
                    leaked_agy[k] = v
        self.assertEqual(leaked_agy, {}, f"parent controller env leaked to agy colleague: {leaked_agy}")

    # 3. wait 50m check-in active for agy sessions
    def test_wait_check_in_enabled_for_antigravity_session(self):
        repo = self.repo({"a.txt": "v0\n"})
        gate = self.work / "checkin-gate"
        agy_env = self.clean_caller_env({
            "ANTIGRAVITY_AGENT": "1",
            "ANTIGRAVITY_CONVERSATION_ID": "agy-checkin-1",
            "DELEGATE_CHECK_IN": ".1s",
            "DELEGATE_STALL_IDLE": "0",
            "DELEGATE_STALL_COMMAND": "0",
        })
        self.fake_pi(
            [fixtures.answer("ok"), fixtures.SETTLED],
            pre=f"while [ ! -f {shlex.quote(str(gate))} ]; do sleep 0.05; done",
        )
        state = self.outcome(self.run_delegate(["start", "--json", "--read-only", "--workdir", str(repo), "slow"], repo, agy_env))
        self.addCleanup(lambda: (gate.touch(), self.run_delegate(["stop", state["run"]], repo, agy_env)))
        self.until(lambda: (Path(state["dir"]) / "agent.pid").exists(), "agent started")
        res = self.run_delegate(["wait"], repo, agy_env)
        self.assertEqual(res.returncode, 75, res.stdout + res.stderr)
        self.assertIn("正常推进", res.stdout)
        self.assertIn("继续等：", res.stdout)
        self.assertTrue((Path(state["dir"]) / "agent.pid").exists())

    # 4. wait --machine filters by current caller; --all lifts filter
    def test_wait_machine_filters_by_caller_and_all_lifts_filter(self):
        base_env = self.clean_caller_env()
        base_env.pop("DELEGATE_RUNS", None)

        repo_a = self.work / "repo-a"
        repo_b = self.work / "repo-b"
        elsewhere = self.work / "elsewhere"
        for d in (repo_a, repo_b):
            d.mkdir()
            subprocess.run(["git", "init", "-q", str(d)], check=True)
        elsewhere.mkdir()

        gate_a = self.work / "gate-a"
        gate_b = self.work / "gate-b"
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre=(
                f'case "$*" in '
                f'*run-a*) while [ ! -f {shlex.quote(str(gate_a))} ]; do sleep 0.05; done;; '
                f'*run-b*) while [ ! -f {shlex.quote(str(gate_b))} ]; do sleep 0.05; done;; '
                f'esac'
            ),
        )

        env_a = {**base_env, "ANTIGRAVITY_AGENT": "1", "ANTIGRAVITY_CONVERSATION_ID": "agy-ctrl-a"}
        env_b = {**base_env, "CLAUDE_CODE_SESSION_ID": "claude-ctrl-b"}

        run_a = self.outcome(self.run_delegate(["start", "--json", "--read-only", "--name", "run-a", "task-a"], repo_a, env_a))
        run_b = self.outcome(self.run_delegate(["start", "--json", "--read-only", "--name", "run-b", "task-b"], repo_b, env_b))
        self.addCleanup(lambda: (gate_a.touch(), gate_b.touch()))
        self.addCleanup(lambda: self.run_delegate(["stop", run_a["run"]], repo_a, env_a))
        self.addCleanup(lambda: self.run_delegate(["stop", run_b["run"]], repo_b, env_b))

        # Controller A starts `wait --machine` in background: must NOT claim run-b's waiter.lock!
        proc_a = subprocess.Popen(
            [str(fixtures.DELEGATE), "wait", "--machine", "--json"],
            cwd=elsewhere,
            env=env_a,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(lambda: (proc_a.kill(), proc_a.wait(timeout=5)) if proc_a.poll() is None else None)
        self.until(lambda: (Path(run_a["dir"]) / "waiter.lock").exists(), "controller A claimed run-a")

        # Controller B runs `wait --machine --max .1` while A is waiting -> must get 75 (still running), NOT 76 (covered)!
        probe_b = self.run_delegate(["wait", "--machine", "--max", ".1", "--json"], elsewhere, env_b)
        self.assertEqual(probe_b.returncode, 75, f"controller A stole controller B's waiter.lock: {probe_b.stdout + probe_b.stderr}")

        # Release run-a -> controller A receives ONLY run-a; run-b is NOT delivered!
        gate_a.touch()
        out_a, err_a = proc_a.communicate(timeout=15)
        self.assertEqual(proc_a.returncode, 0, out_a + err_a)
        got_a = [json.loads(line)["name"] for line in out_a.splitlines() if line.startswith('{"run"')]
        self.assertEqual(got_a, ["run-a"])
        self.assertFalse((Path(run_b["dir"]) / ".delivered").exists())

        # Now Controller A has 0 active runs; `wait --machine` hints about other session's run-b and exits 0.
        idle_a = self.run_delegate(["wait", "--machine"], elsewhere, env_a)
        self.assertEqual(idle_a.returncode, 0, idle_a.stdout + idle_a.stderr)
        self.assertIn("wait --machine --all", idle_a.stderr)
        self.assertFalse((Path(run_b["dir"]) / ".delivered").exists())

        # Explicit `wait --machine --all` collects Controller B's run-b across sessions!
        timer = threading.Timer(0.15, gate_b.touch)
        timer.start()
        self.addCleanup(timer.cancel)
        all_a = self.run_delegate(["wait", "--machine", "--all", "--json"], elsewhere, env_a)
        self.assertEqual(all_a.returncode, 0, all_a.stdout + all_a.stderr)
        got_all = [json.loads(line)["name"] for line in all_a.stdout.splitlines() if line.startswith('{"run"')]
        self.assertEqual(got_all, ["run-b"])

    # 5. apply idempotency inside apply.lock (sequential & concurrent race)
    def test_apply_idempotent_inside_apply_lock_sequential_and_concurrent(self):
        repo = self.repo({"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo v1 > a.txt")
        state = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "--name", "idem-run", "edit")
        )
        self.assertEqual(state["state"], "delivered")

        # First apply with --commit --keep lands the change and creates 1 commit.
        first = self.cli("apply", "--commit", "--keep", state["run"], cwd=repo, human=True)
        self.assertEqual(first.returncode, 0, first.stdout + first.stderr)
        commits_after_first = int(
            subprocess.check_output(["git", "-C", str(repo), "rev-list", "--count", "HEAD"], text=True).strip()
        )
        events_path = self.state_dir() / "events.jsonl"
        applied_events_1 = sum(
            1 for ln in events_path.read_text(encoding="utf-8").splitlines()
            if '"applied"' in ln and state["run"] in ln
        )
        self.assertEqual(applied_events_1, 1)

        # Modify source file after first apply; second apply must NOT touch it, must NOT commit, must exit 0.
        (repo / "a.txt").write_text("controller-edit\n")
        second_human = self.cli("apply", "--commit", "--keep", state["run"], cwd=repo, human=True)
        self.assertEqual(second_human.returncode, 0, second_human.stdout + second_human.stderr)
        self.assertIn("已于 ", second_human.stdout)
        self.assertIn("合入", second_human.stdout)
        self.assertEqual((repo / "a.txt").read_text(), "controller-edit\n")

        second_json = self.cli("apply", "--json", "--commit", "--keep", state["run"], cwd=repo)
        self.assertEqual(second_json.returncode, 0, second_json.stdout + second_json.stderr)
        out_j = self.outcome(second_json)
        self.assertTrue(out_j["apply"]["alreadyApplied"])
        self.assertIn("已于 ", out_j["apply"]["message"])
        self.assertIn("已于 ", second_json.stderr)

        commits_after_second = int(
            subprocess.check_output(["git", "-C", str(repo), "rev-list", "--count", "HEAD"], text=True).strip()
        )
        self.assertEqual(commits_after_second, commits_after_first)
        applied_events_2 = sum(
            1 for ln in events_path.read_text(encoding="utf-8").splitlines()
            if '"applied"' in ln and state["run"] in ln
        )
        self.assertEqual(applied_events_2, 1)

        # Concurrent race: two controllers call `apply --keep` on the same run simultaneously.
        (repo / "a.txt").write_text("v0\n")
        subprocess.run(["git", "-C", str(repo), "checkout", "--", "a.txt"], check=True)
        self.fake_pi([fixtures.answer("done2"), fixtures.SETTLED], pre="echo v2 > b.txt")
        state2 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "--name", "race-run", "edit b")
        )
        p1 = subprocess.Popen(
            [str(fixtures.DELEGATE), "apply", "--json", "--keep", state2["run"]],
            cwd=repo, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        p2 = subprocess.Popen(
            [str(fixtures.DELEGATE), "apply", "--json", "--keep", state2["run"]],
            cwd=repo, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        o1, e1 = p1.communicate(timeout=30)
        o2, e2 = p2.communicate(timeout=30)
        self.assertEqual(p1.returncode, 0, o1 + e1)
        self.assertEqual(p2.returncode, 0, o2 + e2)
        j1 = self.outcome(subprocess.CompletedProcess([], p1.returncode, o1, e1))
        j2 = self.outcome(subprocess.CompletedProcess([], p2.returncode, o2, e2))
        flags = sorted([bool(j1["apply"].get("alreadyApplied")), bool(j2["apply"].get("alreadyApplied"))])
        self.assertEqual(flags, [False, True], f"expected exactly one winner and one alreadyApplied: {j1} vs {j2}")
        self.assertEqual((repo / "b.txt").read_text(), "v2\n")

    # 6. PreToolUse shield for agy run_command + colleague bypass (keeps git prune guard)
    def test_pre_tool_hook_agy_format_and_colleague_guard(self):
        repo = self.repo({"a.txt": "v0\n"})
        (repo / ".local/run/delegate").mkdir(parents=True, exist_ok=True)
        ctrl_env = {
            k: v
            for k, v in os.environ.items()
            if k not in ("PI_DELEGATE_ACTIVE", "DELEGATE_AGENT")
        }

        def run_agy(cmd: str, wait_ms=5000, is_daemon=False, tool_name="run_command", env=None, cli_args=None):
            args = {"CommandLine": cmd}
            if wait_ms is not None:
                args["WaitMsBeforeAsync"] = wait_ms
            if is_daemon:
                args["IsDaemon"] = True
            payload = json.dumps({
                "cwd": str(repo),
                "toolCall": {"id": "tc1", "name": tool_name, "args": args},
            })
            res = subprocess.run(
                [sys.executable, str(BACKGROUND_HOOK), *(cli_args or [])],
                input=payload,
                cwd=str(repo),
                env=ctrl_env if env is None else env,
                capture_output=True,
                text=True,
            )
            self.assertEqual(res.returncode, 0, res.stderr)
            return json.loads(res.stdout)

        # Non-run_command allowed
        self.assertEqual(run_agy("echo hi", tool_name="read_file")["decision"], "allow")

        # Standalone wait with WaitMsBeforeAsync=500, IsDaemon=False -> allowed (both auto-detect & --format agy)
        self.assertEqual(run_agy("/skills/delegate/bin/delegate wait --any", wait_ms=500)["decision"], "allow")
        self.assertEqual(
            run_agy("D=/skills/delegate/bin/delegate; $D wait --any", wait_ms=500)["decision"],
            "allow",
        )
        self.assertEqual(
            run_agy("/skills/delegate/bin/delegate wait --any", wait_ms=500, cli_args=["--format", "agy"])["decision"],
            "allow",
        )

        # Wait with WaitMsBeforeAsync > 2000, missing, or IsDaemon=True -> denied with WaitMsBeforeAsync: 500 hint
        for bad_kwargs in ({"wait_ms": 5000}, {"wait_ms": None}, {"wait_ms": 500, "is_daemon": True}):
            out = run_agy("/skills/delegate/bin/delegate wait --any", **bad_kwargs)
            self.assertEqual(out["decision"], "deny", bad_kwargs)
            self.assertIn("WaitMsBeforeAsync: 500", out["reason"])

        # Mixed wait command denied even at WaitMsBeforeAsync=500
        mixed = run_agy("echo hi && /skills/delegate/bin/delegate wait --any", wait_ms=500)
        self.assertEqual(mixed["decision"], "deny")
        self.assertIn("把 wait 单独作为一条后台命令发出", mixed["reason"])

        # Hard guards: foreground capped build, foreground delegate poll, pgrep -f, git prune / gc --prune=now
        t = "time" + "out"
        for bad_kwargs in ({"wait_ms": 5000}, {"wait_ms": None}, {"wait_ms": 500, "is_daemon": True}):
            cap = run_agy(f"{t} 30 cargo test", **bad_kwargs)
            self.assertEqual(cap["decision"], "deny", bad_kwargs)
            self.assertIn("WaitMsBeforeAsync: 500, IsDaemon: false", cap["reason"])
            self.assertNotIn("run_in_background", cap["reason"])
        self.assertEqual(run_agy(f"{t} 30 cargo test", wait_ms=500)["decision"], "allow")

        poll = run_agy("until [ -f .local/run/delegate/r1/exit_code ]; do sleep 5; done", wait_ms=5000)
        self.assertEqual(poll["decision"], "deny")
        self.assertIn("WaitMsBeforeAsync: 500", poll["reason"])

        pg = run_agy("pg" + "rep -f worker", wait_ms=500)
        self.assertEqual(pg["decision"], "deny")

        pr = run_agy("git prune", wait_ms=500)
        self.assertEqual(pr["decision"], "deny")
        self.assertIn("refs/delegate/", pr["reason"])

        # Colleague process (PI_DELEGATE_ACTIVE=1 or DELEGATE_AGENT=agy):
        # bypasses background wait rule, but STILL denies git prune!
        for marker in ({"PI_DELEGATE_ACTIVE": "1"}, {"DELEGATE_AGENT": "agy"}):
            col_env = {**ctrl_env, **marker}
            self.assertEqual(
                run_agy("/skills/delegate/bin/delegate wait --any", wait_ms=5000, env=col_env)["decision"],
                "allow",
                marker,
            )
            self.assertEqual(
                run_agy("git prune", wait_ms=5000, env=col_env)["decision"],
                "deny",
                marker,
            )
            self.assertEqual(
                run_agy("git gc --prune=now", wait_ms=5000, env=col_env)["decision"],
                "deny",
                marker,
            )

    # 7. Caller liveness/expiry uses delegate's own sessions.json timestamps (no CLAUDE_PID / .claude/)
    def test_caller_liveness_and_expiry_uses_delegate_timestamps_for_agy(self):
        repo = self.repo({"a.txt": "v0\n"})
        agy1_env = self.clean_caller_env({
            "ANTIGRAVITY_AGENT": "1",
            "ANTIGRAVITY_CONVERSATION_ID": "agy-owner-ts",
            "TMUX_PANE": "%42",
        })
        agy2_env = self.clean_caller_env({
            "ANTIGRAVITY_AGENT": "1",
            "ANTIGRAVITY_CONVERSATION_ID": "agy-bystander-ts",
        })

        self.fake_pi([fixtures.answer("unread delivery"), fixtures.SETTLED])
        r1 = self.outcome(
            self.run_delegate(["start", "--json", "--read-only", "--workdir", str(repo), "--name", "agy-job", "check"], repo, agy1_env)
        )
        self.until(lambda: (Path(r1["dir"]) / "exit_code").exists(), "agy-job finished")

        # Delegate itself recorded `lastSeenAt`, `tty`, and `cwd` in `<state>/sessions.json` with no CLAUDE_PID / .claude/ dependency.
        sessions_path = self.state_dir() / "sessions.json"
        self.assertTrue(sessions_path.is_file(), "sessions.json should be populated by touch_caller_session")
        sessions = json.loads(sessions_path.read_text(encoding="utf-8"))
        self.assertIn("agy-owner-ts", sessions)
        self.assertTrue(sessions["agy-owner-ts"].get("lastSeenAt"))
        self.assertEqual(sessions["agy-owner-ts"].get("tty"), "tmux:%42")
        self.assertEqual(sessions["agy-owner-ts"].get("cwd"), str(repo.resolve()))

        def run_stop(env: dict[str, str]):
            hook_env = {
                **env,
                "DELEGATE_BIN": str(fixtures.DELEGATE),
                "DELEGATE_STOP_HOOK_WAITERS": "",
                "DELEGATE_STOP_HOOK_PROCS": "",
                "DELEGATE_STOP_HOOK_CURRENT_ONLY": "1",
            }
            res = subprocess.run(
                [sys.executable, str(STOP_HOOK)],
                input=json.dumps({"cwd": str(repo), "stop_hook_active": False}),
                capture_output=True,
                text=True,
                env=hook_env,
                timeout=15,
            )
            self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
            out = res.stdout.strip()
            return json.loads(out) if out else None

        # While agy-owner-ts's delegate timestamp is fresh (<6h), agy-bystander-ts is NOT blocked.
        self.assertIsNone(run_stop(agy2_env))

        # Backdate agy-owner-ts's sessions.json lastSeenAt (no pid needed) and run timestamps past 6h -> takeover fires!
        old_iso = iso_ago(7.0)
        sessions_path.write_text(json.dumps({"agy-owner-ts": {"lastSeenAt": old_iso}}, indent=2) + "\n", encoding="utf-8")
        ev_file = self.state_dir() / "events.jsonl"
        rows = []
        for ln in ev_file.read_text(encoding="utf-8").splitlines():
            if ln.strip():
                obj = json.loads(ln)
                obj["at"] = old_iso
                rows.append(json.dumps(obj, ensure_ascii=False))
        ev_file.write_text("\n".join(rows) + "\n", encoding="utf-8")
        for fname, field in (("meta.json", "startedAt"), ("summary.json", "finishedAt")):
            p = Path(r1["dir"]) / fname
            if p.is_file():
                d = json.loads(p.read_text(encoding="utf-8"))
                d[field] = old_iso
                p.write_text(json.dumps(d) + "\n", encoding="utf-8")
        idx_p = self.state_dir() / "index.json"
        if idx_p.is_file():
            idx = json.loads(idx_p.read_text(encoding="utf-8"))
            for entry in (idx.get("runs") or {}).values():
                if isinstance(entry, dict) and isinstance(entry.get("status"), dict):
                    entry["status"]["finishedAt"] = old_iso
                    entry["status"]["startedAt"] = old_iso
            idx_p.write_text(json.dumps(idx) + "\n", encoding="utf-8")

        blk = run_stop(agy2_env)
        self.assertIsNotNone(blk)
        self.assertIn("接盘", blk["reason"])
        self.assertIn("agy-job", blk["reason"])


if __name__ == "__main__":
    unittest.main()
