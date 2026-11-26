import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateApplyFinishTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    fake_docker = fixtures.DelegateTests.fake_docker
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def task(self, *options):
        repo = self.repo({"a.txt": "old\n", "other.txt": "original\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo new > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, *options, "task"))
        return repo, state

    def test_success_retains_run_and_releases_worktree_and_keeps_json_fields(self):
        repo, state = self.task("--accept", "test -f a.txt")
        result = self.cli("apply", "--json", state["run"])
        self.assertEqual(result.returncode, 0, result.stderr)
        outcome = self.outcome(result)
        self.assertNotIn("验收仍然有效", result.stdout)
        self.assertEqual(outcome["apply"], {"ok": True, "dryRun": False})
        self.assertTrue(outcome["acceptStillValid"])
        self.assertNotIn("cleanup", outcome)
        # 6.33 P1-4: acceptStillValid=true auto-cleans and archives the applied run.
        self.assertFalse(Path(state["dir"]).exists())
        arch_record = Path(self.env["XDG_CACHE_HOME"]) / "delegate" / "archive" / state["run"] / "record"
        self.assertTrue((arch_record / "applied.patch").is_file())
        self.assertFalse(Path(state["worktree"]).exists())
        self.assertEqual((repo / "a.txt").read_text(), "new\n")

    def test_keep_and_dry_run_preserve_task(self):
        _, state = self.task("--accept", "true")
        for options in (("--dry-run",), ("--keep",)):
            result = self.cli("apply", *options, state["run"], human=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(Path(state["dir"]).exists())
            self.assertTrue(Path(state["worktree"]).exists())
        lines = result.stdout.splitlines()
        self.assertEqual(lines[0], f"APPLY OK {state['run']}")
        self.assertEqual(lines[-3], "验收仍然有效（仓库快照范围）")
        self.assertIn(f"delegate files {state['run']}", lines[-2])
        self.assertEqual(lines[-1], f"APPLY OK {state['run']}")

    def test_invalid_acceptance_lists_primary_and_additional_checks(self):
        repo, state = self.task("--accept", "test -f a.txt", "--accept-also", "test -f other.txt")
        (repo / "other.txt").write_text("caller changed\n")
        (repo / ".delegate.json").write_text('{"applyVerify": false}')
        result = self.cli("apply", state["run"], human=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = result.stdout.splitlines()
        self.assertEqual(lines[0], f"APPLY OK {state['run']}")
        summary_line = lines[-3]
        self.assertIn("主干已前进，验收快照失效；本仓库配置合入后不自动验收，批量门禁时跑：", summary_line)
        self.assertNotIn("验收无效", summary_line)
        self.assertIn("test -f a.txt && test -f other.txt", summary_line)
        self.assertIn(f"delegate files {state['run']}", lines[-2])
        self.assertEqual(lines[-1], f"APPLY OK {state['run']}")

    def test_failed_apply_preserves_task(self):
        repo, state = self.task()
        (repo / "a.txt").write_text("caller\n")
        result = self.cli("apply", "--json", state["run"])
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(Path(state["dir"]).exists())
        self.assertTrue(Path(state["worktree"]).exists())

    def test_cleanup_failure_is_only_a_warning(self):
        _, state = self.task()
        self.fake_docker({"b1": {"path": state["worktree"], "fail": True}})
        result = self.cli("apply", state["run"], human=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        # 6.33 P1-4: worktree/container cleanup is staged asynchronously via trash + _gc-worker,
        # so docker cleanup errors never block or fail `apply`.
        self.assertTrue(Path(state["dir"]).exists())
        self.assertFalse(Path(state["worktree"]).exists())
        lines = result.stdout.splitlines()
        self.assertEqual(lines[0], f"APPLY OK {state['run']}")
        self.assertIn("任务没有验收命令", lines[-3])
        self.assertIn(f"delegate files {state['run']}", lines[-2])
        self.assertEqual(lines[-1], f"APPLY OK {state['run']}")


if __name__ == "__main__":
    unittest.main()
