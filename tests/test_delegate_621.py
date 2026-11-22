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

    def test_controller_source_commit_during_shell_readonly_and_write_runs_is_not_violation(self):
        import subprocess
        repo = self.repo({"a.txt": "v0\n", "partial.txt": "p0\n", "other.txt": "o0\n"})
        # Dirty a.txt and partially staged partial.txt before runs start.
        (repo / "a.txt").write_text("v1 by controller\n")
        (repo / "partial.txt").write_text("p1 staged\n")
        subprocess.run(["git", "-C", str(repo), "add", "partial.txt"], check=True)
        (repo / "partial.txt").write_text("p2 unstaged\n")

        gate = self.work / "release-shell"
        ready = self.work / "ready-shell"
        self.fake_pi(
            [
                # Even if colleague runs a non-readonly-classified bash referencing repo root (no writes),
                # controller's HEAD commit must not trigger a violation.
                {"type": "tool_execution_start", "toolName": "bash", "args": {"command": f"env | grep -q . && ls {shlex.quote(str(repo))}"}},
                fixtures.answer("verified ok"),
                fixtures.SETTLED,
            ],
            pre=f"touch {shlex.quote(str(ready))}; while [ ! -f {shlex.quote(str(gate))} ]; do sleep .05; done",
        )
        started = self.outcome(
            self.cli("start", "--read-only", "--shell", "--workdir", repo, "verify actions")
        )
        self.addCleanup(lambda: self.cli("stop", started["run"]))
        self.until(ready.exists, "shell agent started")

        # Controller commits a.txt and the staged portion of partial.txt, advancing HEAD,
        # AND leaves an unrelated uncommitted edit in other.txt.
        subprocess.run(["git", "-C", str(repo), "add", "a.txt"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "-qm", "controller commit during run"], check=True)
        (repo / "other.txt").write_text("o1 controller dirty\n")

        gate.touch()
        finished = self.cli("wait", "--all", started["run"])
        self.assertEqual(finished.returncode, 0, finished.stdout + finished.stderr)
        state = self.outcome(finished)
        self.assertEqual(state["state"], "answered")
        self.assertNotIn("readOnlyViolation", state)
        self.assertNotIn("sourceWriteViolation", state)
        # Committed a.txt and unchanged-worktree partial.txt are excluded; only uncommitted other.txt shows.
        self.assertEqual(state.get("sourceWorkspaceChanges"), ["other.txt"])

        # Guardrail: if colleague actually edits a committed path in working tree after controller's commit,
        # it IS still caught and rejected.
        gate2 = self.work / "release-rogue"
        ready2 = self.work / "ready-rogue"
        rogue_cmd = f"echo rogue > {shlex.quote(str(repo / 'a.txt'))}"
        self.fake_pi(
            [
                {"type": "tool_execution_start", "toolName": "bash", "args": {"command": rogue_cmd}},
                fixtures.answer("sneaky"),
                fixtures.SETTLED,
            ],
            pre=f"touch {shlex.quote(str(ready2))}; while [ ! -f {shlex.quote(str(gate2))} ]; do sleep .05; done; {rogue_cmd}",
        )
        started2 = self.outcome(
            self.cli("start", "--read-only", "--shell", "--workdir", repo, "verify again")
        )
        self.addCleanup(lambda: self.cli("stop", started2["run"]))
        self.until(ready2.exists, "rogue agent started")
        (repo / "a.txt").write_text("v2 committed\n")
        subprocess.run(["git", "-C", str(repo), "add", "a.txt"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "-qm", "commit a v2"], check=True)
        gate2.touch()
        finished2 = self.cli("wait", "--all", started2["run"])
        self.assertEqual(finished2.returncode, 1, finished2.stdout + finished2.stderr)
        state2 = self.outcome(finished2)
        self.assertEqual(state2["state"], "rejected")
        self.assertEqual(state2["readOnlyViolation"], ["source:a.txt"])
        self.assertEqual(state2["sourceWriteViolation"], ["a.txt"])


if __name__ == "__main__":
    unittest.main()
