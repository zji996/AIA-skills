import json
import shlex
import threading
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateWaitDefaultsTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    start_gated = fixtures.DelegateTests.start_gated
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    until = fixtures.DelegateTests.until
    wait_until = fixtures.DelegateTests.wait_until
    wait_text = fixtures.DelegateTests.wait_text
    repo = fixtures.DelegateTests.repo

    def start_pair(self):
        gate = self.work / "release"
        self.fake_pi(
            [fixtures.answer("ok"), fixtures.SETTLED],
            pre=(
                'case "$*" in *slow*) '
                '[ -n "${DELEGATE_RUN_DIR:-}" ] && : > "$DELEGATE_RUN_DIR/.fake-ready"; '
                f'while [ ! -f {shlex.quote(str(gate))} ]; do sleep .02; done;; esac'
            ),
        )
        states = {}
        for name in ("slow", "fast"):
            states[name] = self.outcome(self.cli("start", "--read-only", "--name", name, "task"))
        self.addCleanup(lambda: (gate.touch(), self.cli("stop", "slow")))
        self.until(
            lambda: (Path(states["slow"]["dir"]) / ".fake-ready").is_file()
            and (Path(states["fast"]["dir"]) / "exit_code").exists(),
            "slow ready in gate and fast finished",
        )
        return gate, states

    def test_default_wait_delivers_finished_and_names_exact_command(self):
        gate, states = self.start_pair()
        first = self.cli("wait", "--max", "2", human=True)
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertIn("fast", first.stdout)
        self.assertFalse((Path(states["slow"]["dir"]) / ".delivered").exists())
        last = first.stdout.splitlines()[-1]
        self.assertIn("本会话还剩 1 个任务在跑", last)
        command = last.split("原样再跑：", 1)[1]
        self.assertEqual(shlex.split(command), [str(fixtures.DELEGATE), "wait", "--max", "2"])
        gate.touch()
        second = self.cli("wait", human=True)
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertIn("slow", second.stdout)
        self.assertEqual(second.stdout.splitlines()[-1], "本会话没有在跑的任务")
        self.assertEqual(self.cli("wait", human=True).stdout.splitlines()[-1], "本会话没有在跑的任务")

    def test_named_wait_returns_on_first_selected_completion(self):
        gate, states = self.start_pair()
        result = self.cli("wait", states["slow"]["run"], states["fast"]["run"], human=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("fast", result.stdout)
        self.assertNotIn("slow ·", result.stdout)
        self.assertFalse((Path(states["slow"]["dir"]) / ".delivered").exists())
        gate.touch()

    def test_until_all_waits_for_every_run_and_conflicting_modes_fail(self):
        gate, states = self.start_pair()
        limited = self.cli("wait", "--until-all", "--max", ".1")
        self.assertEqual(limited.returncode, 75, limited.stderr)
        self.assertFalse((Path(states["slow"]["dir"]) / ".delivered").exists())
        timer = threading.Timer(.2, gate.touch)
        timer.start()
        self.addCleanup(timer.cancel)
        result = self.cli("wait", "--until-all", human=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("slow", result.stdout)
        self.assertEqual(result.stdout.splitlines()[-1], "本会话没有在跑的任务")
        for mode in ("--any", "--stream"):
            self.assertEqual(self.cli("wait", "--until-all", mode).returncode, 2)

    def test_default_max_and_json_contract_stay_intact(self):
        gate, _ = self.start_pair()
        self.assertEqual(self.cli("wait").returncode, 0)
        pending = self.cli("wait", "--max", ".1", human=True)
        self.assertEqual(pending.returncode, 75)
        self.assertIn("本会话还剩 1 个任务在跑", pending.stdout.splitlines()[-1])
        json_pending = self.cli("wait", "--max", ".1")
        self.assertEqual(json_pending.returncode, 75)
        self.assertNotIn("本会话", json_pending.stdout)
        gate.touch()

    def test_final_count_is_for_the_session_even_with_named_wait(self):
        self.env["DELEGATE_CALLER"] = "mine"
        gate, states = self.start_pair()
        self.env["DELEGATE_CALLER"] = "other"
        other = self.outcome(self.cli("start", "--read-only", "--name", "other-slow", "task"))
        self.addCleanup(lambda: self.cli("stop", other["run"]))
        self.env["DELEGATE_CALLER"] = "mine"
        result = self.cli("wait", states["fast"]["run"], human=True)
        self.assertIn("本会话还剩 1 个任务在跑", result.stdout.splitlines()[-1])
        gate.touch()

    def test_claude_check_in_prints_glance_and_leaves_agent_running(self):
        self.env["CLAUDE_CODE_SESSION_ID"] = "claude-check-in"
        self.env["DELEGATE_CHECK_IN"] = ".1s"
        self.env["DELEGATE_STALL_IDLE"] = "0"
        self.env["DELEGATE_STALL_COMMAND"] = "0"
        state, gate = self.start_gated(
            "--read-only", "task",
            attempts=([fixtures.answer("ok"), fixtures.SETTLED],),
        )
        result = self.cli("wait", human=True)
        self.assertEqual(result.returncode, 75, result.stdout + result.stderr)
        self.assertIn("正常推进", result.stdout)
        self.assertIn("继续等：", result.stdout)
        self.assertTrue((Path(state["dir"]) / "agent.pid").exists())
        gate.touch()

    def test_check_in_reports_an_in_flight_command_as_progress(self):
        self.env["CLAUDE_CODE_SESSION_ID"] = "claude-check-in"
        self.env["DELEGATE_CHECK_IN"] = ".1s"
        self.env["DELEGATE_STALL_IDLE"] = "0"
        self.env["DELEGATE_STALL_COMMAND"] = "0"
        event = {"type": "tool_execution_start", "toolName": "bash", "args": {"command": "cargo test --long"}}
        state, gate = self.start_gated(
            "--read-only", "task",
            attempts=([],),
            pre=f"printf '%s\\n' {shlex.quote(json.dumps(event))}",
        )
        self.wait_text(Path(state["dir"]) / "events.jsonl", "cargo test --long", "in-flight bash event recorded")
        result = self.cli("wait", human=True)
        self.assertEqual(result.returncode, 75, result.stdout + result.stderr)
        self.assertIn("正在执行 cargo test --long", result.stdout)
        self.assertEqual(result.stdout.count("继续等："), 1)
        gate.touch()

    def test_omitted_timeout_has_no_agent_cap(self):
        self.env["DELEGATE_STALL_IDLE"] = "0"
        self.env["DELEGATE_STALL_COMMAND"] = "0"
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], sleep=.3)
        result = self.cli("run", "task")
        self.assertEqual((result.returncode, self.outcome(result)["state"]), (0, "answered"))

    def test_idle_stall_warns_without_killing_and_keeps_worktree_changes(self):
        self.env["DELEGATE_STALL_IDLE"] = ".1s"
        self.env["DELEGATE_STALL_COMMAND"] = "0"
        repo = self.repo({"seed.txt": "seed\n"})
        state, gate = self.start_gated(
            "--workdir", repo, "--worktree", "task",
            attempts=([fixtures.answer("finished after idle"), fixtures.SETTLED],),
            pre="echo kept > kept.txt",
        )
        self.wait_text(Path(state["dir"]) / "events.jsonl", '"stall_warning"', "idle stall_warning emitted")
        gate.touch()
        waited = self.outcome(self.cli("wait", state["run"]))
        # 6.28: stall only emits a reminder event and never kills the colleague.
        self.assertEqual(waited["state"], "answered")
        run_events = (Path(state["dir"]) / "events.jsonl").read_text()
        self.assertIn('"stall_warning"', run_events)
        self.assertIn('"idle"', run_events)
        machine_events = (Path(self.env["XDG_STATE_HOME"]) / "delegate" / "events.jsonl").read_text()
        self.assertIn('"stall_warning"', machine_events)

    def test_command_stall_warns_naming_command_without_killing(self):
        self.env["DELEGATE_STALL_IDLE"] = "0"
        self.env["DELEGATE_STALL_COMMAND"] = ".1s"
        event = {"type": "tool_execution_start", "toolName": "bash", "args": {"command": "sleep 0.4"}}
        done_ev = {"type": "tool_execution_end", "toolName": "bash", "isError": False}
        state, gate = self.start_gated(
            "task",
            attempts=([done_ev, fixtures.answer("command finished"), fixtures.SETTLED],),
            pre=f"printf '%s\\n' {shlex.quote(json.dumps(event))}",
        )
        self.wait_text(Path(state["dir"]) / "events.jsonl", '"stall_warning"', "command stall_warning emitted")
        gate.touch()
        waited = self.outcome(self.cli("wait", state["run"]))
        self.assertEqual(waited["state"], "answered")
        run_events = (Path(state["dir"]) / "events.jsonl").read_text()
        self.assertIn('"stall_warning"', run_events)
        self.assertIn("sleep 0.4", run_events)
        self.assertFalse((Path(state["dir"]) / "stall-warning.json").exists())

    def test_legacy_stalled_run_remains_readable(self):
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
        state = self.outcome(self.cli("run", "task"))
        run_dir = Path(state["dir"])
        # Simulate a pre-6.28 run whose summary.json recorded state=stalled.
        summary = __import__("json").loads((run_dir / "summary.json").read_text())
        summary["state"] = "stalled"
        summary["stall"] = {"kind": "command", "cmd": "cargo test --hangs", "seconds": 5400}
        (run_dir / "summary.json").write_text(__import__("json").dumps(summary))
        st = self.outcome(self.cli("status", state["run"]))
        self.assertEqual(st["state"], "stalled")
        self.assertEqual(st["stall"]["cmd"], "cargo test --hangs")


if __name__ == "__main__":
    unittest.main()
