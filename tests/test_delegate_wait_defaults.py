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


if __name__ == "__main__":
    unittest.main()
