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

    def test_success_cleans_run_and_worktree_and_keeps_json_fields(self):
        repo, state = self.task("--accept", "test -f a.txt")
        result = self.cli("apply", "--json", state["run"])
        self.assertEqual(result.returncode, 0, result.stderr)
        outcome = self.outcome(result)
        self.assertNotIn("验收仍然有效", result.stdout)
        self.assertEqual(outcome["apply"], {"ok": True, "dryRun": False})
        self.assertTrue(outcome["acceptStillValid"])
        self.assertNotIn("cleanup", outcome)
        self.assertFalse(Path(state["dir"]).exists())
        self.assertFalse(Path(state["worktree"]).exists())
        self.assertEqual((repo / "a.txt").read_text(), "new\n")

    def test_keep_and_dry_run_preserve_task(self):
        _, state = self.task("--accept", "true")
        for options in (("--dry-run",), ("--keep",)):
            result = self.cli("apply", *options, state["run"], human=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertTrue(Path(state["dir"]).exists())
            self.assertTrue(Path(state["worktree"]).exists())
        self.assertEqual(result.stdout.splitlines()[-1], "验收仍然有效（仓库快照范围）")

    def test_invalid_acceptance_lists_primary_and_additional_checks(self):
        repo, state = self.task("--accept", "test -f a.txt", "--accept-also", "test -f other.txt")
        (repo / "other.txt").write_text("caller changed\n")
        result = self.cli("apply", state["run"], human=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        last = result.stdout.splitlines()[-1]
        self.assertIn("验收无效", last)
        self.assertIn("需要在主干重跑", last)
        self.assertIn("test -f a.txt && test -f other.txt", last)

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
        self.assertIn("apply cleanup:", result.stderr)
        self.assertFalse(Path(state["dir"]).exists())
        self.assertFalse(Path(state["worktree"]).exists())
        self.assertIn("任务没有验收命令", result.stdout.splitlines()[-1])


if __name__ == "__main__":
    unittest.main()
