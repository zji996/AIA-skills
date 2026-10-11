import shlex
import threading
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateWaitDefaultsTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    until = fixtures.DelegateTests.until
    repo = fixtures.DelegateTests.repo

    def start_pair(self):
        gate = self.work / "release"
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED],
                     pre=f'case "$*" in *slow*) while [ ! -f {shlex.quote(str(gate))} ]; do sleep .05; done;; esac')
        states = {}
        for name in ("slow", "fast"):
            states[name] = self.outcome(self.cli("start", "--read-only", "--name", name, "task"))
        self.addCleanup(lambda: self.cli("stop", "slow"))
        self.until(lambda: (Path(states["fast"]["dir"]) / "exit_code").exists(), "fast")
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
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], sleep=.5)
        state = self.outcome(self.cli("start", "--read-only", "task"))
        self.addCleanup(lambda: self.cli("stop", state["run"]))
        result = self.cli("wait", human=True)
        self.assertEqual(result.returncode, 75)
        self.assertIn("正常推进", result.stdout)
        self.assertIn("继续等：", result.stdout)
        self.assertTrue((Path(state["dir"]) / "agent.pid").exists())

    def test_check_in_reports_an_in_flight_command_as_progress(self):
        self.env["CLAUDE_CODE_SESSION_ID"] = "claude-check-in"
        self.env["DELEGATE_CHECK_IN"] = ".3s"
        self.env["DELEGATE_STALL_IDLE"] = "0"
        self.env["DELEGATE_STALL_COMMAND"] = "0"
        event = {"type": "tool_execution_start", "toolName": "bash", "args": {"command": "cargo test --long"}}
        self.fake_pi([], pre=f"printf '%s\\n' {shlex.quote(__import__('json').dumps(event))}; sleep 5")
        state = self.outcome(self.cli("start", "--read-only", "task"))
        self.addCleanup(lambda: self.cli("stop", state["run"]))
        result = self.cli("wait", human=True)
        self.assertEqual(result.returncode, 75)
        self.assertIn("正在执行 cargo test --long", result.stdout)
        self.assertEqual(result.stdout.count("继续等："), 1)

    def test_omitted_timeout_has_no_agent_cap(self):
        self.env["DELEGATE_STALL_IDLE"] = "0"
        self.env["DELEGATE_STALL_COMMAND"] = "0"
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], sleep=.3)
        result = self.cli("run", "task")
        self.assertEqual((result.returncode, self.outcome(result)["state"]), (0, "answered"))

    def test_idle_stall_is_terminal_but_keeps_worktree_changes(self):
        self.env["DELEGATE_STALL_IDLE"] = ".1s"
        self.env["DELEGATE_STALL_COMMAND"] = "0"
        repo = self.repo({"seed.txt": "seed\n"})
        self.fake_pi([], pre="echo kept > kept.txt", sleep=.5)
        state = self.outcome(self.cli("run", "--workdir", repo, "--worktree", "task"))
        self.assertEqual(state["state"], "stalled")
        self.assertTrue((Path(state["dir"]) / "stall.json").exists())

    def test_command_stall_records_the_in_flight_command(self):
        self.env["DELEGATE_STALL_IDLE"] = "0"
        self.env["DELEGATE_STALL_COMMAND"] = ".1s"
        event = {"type": "tool_execution_start", "toolName": "bash", "args": {"command": "sleep 2"}}
        self.fake_pi([], pre=f"printf '%s\\n' {shlex.quote(__import__('json').dumps(event))}; sleep 2")
        state = self.outcome(self.cli("run", "task"))
        self.assertEqual(state["state"], "stalled")
        self.assertEqual(state["stall"]["kind"], "command")
        self.assertIn("sleep 2", state["stall"]["cmd"])


if __name__ == "__main__":
    unittest.main()
