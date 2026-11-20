"""6.21.8 regressions: repo acceptTimeout precedence and attributed worktree outside-write checks."""
import json
import shlex
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate621Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def meta(self, state):
        return json.loads((Path(state["dir"]) / "meta.json").read_text())

    def test_accept_timeout_uses_builtin_default(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        result = self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task")
        self.assertEqual(result.returncode, 0, result.stderr)
        meta = self.meta(self.outcome(result))
        self.assertEqual(meta["acceptTimeoutSeconds"], 600)
        self.assertNotIn("acceptTimeout", meta.get("defaults", {}))

    def test_accept_timeout_uses_repo_config_and_echoes_source(self):
        repo = self.repo({
            "a.txt": "old\n",
            ".delegate.json": json.dumps({"accept": "true", "acceptTimeout": "40m"}),
        })
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        result = self.cli("run", "--worktree", "--workdir", repo, "task")
        self.assertEqual(result.returncode, 0, result.stderr)
        meta = self.meta(self.outcome(result))
        self.assertEqual(meta["acceptTimeoutSeconds"], 2400)
        self.assertEqual(meta["defaults"]["acceptTimeout"], {"value": "40m", "source": "repo"})
        self.assertIn("生效默认值及来源", result.stderr)
        self.assertIn("acceptTimeout", result.stderr)
        self.assertIn("repo", result.stderr)

    def test_accept_timeout_cli_overrides_repo_config(self):
        repo = self.repo({
            "a.txt": "old\n",
            ".delegate.json": json.dumps({"accept": "true", "acceptTimeout": "40m"}),
        })
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        result = self.cli("run", "--worktree", "--workdir", repo, "--accept-timeout", "2m", "task")
        self.assertEqual(result.returncode, 0, result.stderr)
        meta = self.meta(self.outcome(result))
        self.assertEqual(meta["acceptTimeoutSeconds"], 120)
        self.assertNotIn("acceptTimeout", meta.get("defaults", {}))

    def test_accept_timeout_reply_inherits_and_cli_overrides(self):
        repo = self.repo({
            "a.txt": "old\n",
            ".delegate.json": json.dumps({"accept": "true", "acceptTimeout": "40m"}),
        })
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        first = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "first"))
        self.assertEqual(self.meta(first)["acceptTimeoutSeconds"], 2400)

        self.fake_pi([fixtures.answer("done2"), fixtures.SETTLED])
        inherited = self.outcome(self.cli("reply", "--wait", first["run"], "inherit"))
        self.assertEqual(self.meta(inherited)["acceptTimeoutSeconds"], 2400)
        self.assertEqual(self.meta(inherited)["defaults"]["acceptTimeout"], {"value": "40m", "source": "repo"})

        self.fake_pi([fixtures.answer("done3"), fixtures.SETTLED])
        overridden = self.outcome(
            self.cli("reply", "--wait", inherited["run"], "--accept-timeout", "5m", "override")
        )
        self.assertEqual(self.meta(overridden)["acceptTimeoutSeconds"], 300)
        self.assertNotIn("acceptTimeout", self.meta(overridden).get("defaults", {}))

    def test_accept_timeout_invalid_repo_config_fails_usage(self):
        repo = self.repo({
            "a.txt": "old\n",
            ".delegate.json": json.dumps({"acceptTimeout": "0"}),
        })
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        result = self.cli("start", "--worktree", "--workdir", repo, "task")
        self.assertEqual(result.returncode, 2)
        self.assertIn("acceptTimeout", result.stderr)

    def test_worktree_accept_only_with_controller_source_edit_is_not_rejected(self):
        repo = self.repo({"a.txt": "old\n", "adr.md": "v1\n"})
        gate = self.work / "release"
        ready = self.work / "ready"
        self.fake_pi(
            [
                {"type": "tool_execution_start", "toolName": "bash", "args": {"command": "true"}},
                fixtures.answer("acceptance passed"),
                fixtures.SETTLED,
            ],
            pre=f"touch {shlex.quote(str(ready))}; while [ ! -f {shlex.quote(str(gate))} ]; do sleep .05; done",
        )
        started = self.outcome(
            self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", "verify only")
        )
        self.addCleanup(lambda: self.cli("stop", started["run"]))
        self.until(ready.exists, "agent started")
        # Controller edits the source repo while the colleague only runs verification.
        (repo / "adr.md").write_text("v2 by controller\n")
        gate.touch()
        finished = self.cli("wait", "--all", started["run"])
        self.assertEqual(finished.returncode, 0, finished.stdout + finished.stderr)
        state = self.outcome(finished)
        self.assertEqual(state["state"], "delivered")
        self.assertTrue(state["accept"]["ok"])
        self.assertEqual(state["sourceWorkspaceChanges"], ["adr.md"])
        self.assertNotIn("sourceWriteViolation", state)

    def test_worktree_colleague_write_abs_path_to_source_is_rejected(self):
        repo = self.repo({"a.txt": "old\n", "adr.md": "v1\n"})
        target = repo / "a.txt"
        self.fake_pi(
            [
                {"type": "tool_execution_start", "toolName": "write", "args": {"path": str(target)}},
                fixtures.answer("done"),
                fixtures.SETTLED,
            ],
            pre=f"echo colleague > {shlex.quote(str(target))}",
        )
        result = self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        state = self.outcome(result)
        self.assertEqual(state["state"], "rejected")
        self.assertEqual(state["sourceWriteViolation"], ["a.txt"])
        self.assertEqual(state["sourceWorkspaceChanges"], ["a.txt"])

    def test_worktree_colleague_bash_abs_path_to_source_rejects_only_attributed_file(self):
        repo = self.repo({"a.txt": "old\n", "adr.md": "v1\n"})
        target = repo / "a.txt"
        cmd = f"echo colleague > {shlex.quote(str(target))}"
        gate = self.work / "release-bash"
        ready = self.work / "ready-bash"
        self.fake_pi(
            [
                {"type": "tool_execution_start", "toolName": "bash", "args": {"command": cmd}},
                fixtures.answer("done"),
                fixtures.SETTLED,
            ],
            pre=(
                f"touch {shlex.quote(str(ready))}; "
                f"while [ ! -f {shlex.quote(str(gate))} ]; do sleep .05; done; "
                f"{cmd}"
            ),
        )
        started = self.outcome(
            self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", "task")
        )
        self.addCleanup(lambda: self.cli("stop", started["run"]))
        self.until(ready.exists, "agent started")
        (repo / "adr.md").write_text("controller edit\n")
        gate.touch()
        finished = self.cli("wait", "--all", started["run"])
        self.assertEqual(finished.returncode, 1, finished.stdout + finished.stderr)
        state = self.outcome(finished)
        self.assertEqual(state["state"], "rejected")
        self.assertEqual(state["sourceWriteViolation"], ["a.txt"])
        self.assertEqual(state["sourceWorkspaceChanges"], ["a.txt", "adr.md"])

    def test_in_place_write_behavior_unchanged(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi(
            [
                {"type": "tool_execution_start", "toolName": "write", "args": {"path": str(repo / "a.txt")}},
                fixtures.answer("done"),
                fixtures.SETTLED,
            ],
            pre="echo updated > a.txt",
        )
        result = self.cli("run", "--in-place", "--workdir", repo, "--accept", "true", "in-place edit")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        state = self.outcome(result)
        self.assertEqual(state["state"], "delivered")
        self.assertEqual(state["changes"]["files"], 1)
        self.assertNotIn("sourceWriteViolation", state)
        self.assertNotIn("sourceWorkspaceChanges", state)


if __name__ == "__main__":
    unittest.main()
