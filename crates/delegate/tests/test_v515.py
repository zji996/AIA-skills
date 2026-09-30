"""5.15 black-box regressions; reuse the repository's fake-agent fixtures."""
import importlib.util
import json
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
fixtures = sys.modules.get("tests.test_delegate") or sys.modules.get("test_delegate")
if fixtures is None:
    spec = importlib.util.spec_from_file_location("delegate_fixtures", ROOT / "tests/test_delegate.py")
    fixtures = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixtures)


class V515Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until
    outcome = fixtures.DelegateTests.outcome

    def cli(self, *args, human=False, **kwargs):
        return fixtures.DelegateTests.cli(self, *args, human=human, **kwargs)

    def test_managed_gitlink_and_nonmanaged_skip_worktree(self):
        repo = self.repo({"a.txt": "old\n", "other.txt": "other\n"})
        sub = self.work / "library"
        subprocess.run(["git", "init", "-q", str(sub)], check=True)
        subprocess.run(["git", "-C", str(sub), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "--allow-empty", "-qm", "lib"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "protocol.file.allow=always", "submodule",
                        "add", "-q", str(sub), "third_party/pi"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "-qm", "sub"], check=True)
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"link": ["third_party/pi"]}}))
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo new > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        self.assertIs(state["accept"]["snapshotComplete"], True)
        self.assertEqual(state["accept"]["excluded"], ["third_party/pi"])
        applied = self.outcome(self.cli("apply", state["run"]))
        self.assertIs(applied["acceptStillValid"], True)
        self.assertEqual(applied["excluded"], ["third_party/pi"])
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre="git update-index --skip-worktree other.txt; echo next > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        self.assertIs(state["accept"]["snapshotComplete"], False)
        self.assertIn("index", state["accept"]["snapshotReason"])

    def test_generated_protection_allows_generator_rejects_manual_drift(self):
        repo = self.repo({"a.txt": "old\n", "gen/out": "old\n"})
        (repo / ".delegate.json").write_text(json.dumps({"generated": {
            "paths": ["gen/"], "command": "cp a.txt gen/out"}}))
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo new > a.txt; cp a.txt gen/out")
        result = self.cli("run", "--worktree", "--workdir", repo, "--protect-reason", "gen/", "generated",
                          "--accept", "cmp a.txt gen/out", "task")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("may only be changed", result.stderr)
        state = self.outcome(result)
        self.assertNotIn("protectViolation", state)
        self.assertIn("may only be changed by the generator", (Path(state["dir"]) / "prompt.md").read_text())
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo new > a.txt; echo manual > gen/out")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--protect", "gen/out",
                                     "--accept", "true", "task"))
        self.assertEqual(state["state"], "rejected")
        self.assertEqual(state["protectViolation"], ["gen/out"])
        self.assertIn("生成物被手改", state["error"])
        self.assertNotIn("accept", state)
        (repo / ".delegate.json").write_text("{}")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--protect", "gen/", "task"))
        self.assertEqual(state["protectViolation"], ["gen/out"])

    def test_duplicate_wait_and_dead_waiter_lock(self):
        gate = self.work / "gate"
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre=f"while [ ! -f {gate} ]; do sleep .02; done")
        state = self.outcome(self.cli("start", "--read-only", "--name", "one", "task"))
        self.addCleanup(self.cli, "stop", state["run"])
        lock = Path(state["dir"]) / "waiter.lock"
        first = subprocess.Popen([str(fixtures.DELEGATE), "wait", state["run"]],
                                 env=self.env, cwd=self.work, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: first.poll() is None and first.kill())
        self.until(lambda: lock.exists() and lock.read_text().strip() == str(first.pid), "waiter lock")
        for args in ((state["run"],), ("--any", state["run"]), ("--stream", state["run"]), ("--machine",)):
            out = self.cli("wait", *args)
            self.assertEqual(out.returncode, 76, out.stderr)
            self.assertEqual(out.stdout, "")
            self.assertIn(f"pid {first.pid}", out.stderr)
        first.kill()
        first.communicate(timeout=10)
        gate.touch()
        out = self.cli("wait", state["run"], human=True)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("done", out.stdout)

    def test_wait_any_skips_covered_task_and_delivers_free_task(self):
        gate = self.work / "gate"
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre=f'case "$*" in *one*) while [ ! -f {gate} ]; do sleep .02; done;; esac')
        one = self.outcome(self.cli("start", "--read-only", "--name", "one", "task"))
        self.addCleanup(self.cli, "stop", one["run"])
        two = self.outcome(self.cli("start", "--read-only", "--name", "two", "task"))
        lock = Path(one["dir"]) / "waiter.lock"
        first = subprocess.Popen([str(fixtures.DELEGATE), "wait", one["run"]],
                                 env=self.env, cwd=self.work, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: first.poll() is None and first.kill())
        self.until(lambda: lock.exists() and lock.read_text().strip() == str(first.pid), "waiter lock")
        out = self.cli("wait", "--any", one["run"], two["run"])
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(self.outcome(out)["name"], "two")
        self.assertNotIn('"name":"one"', out.stdout)
        gate.touch()
        first.communicate(timeout=10)

if __name__ == "__main__":
    unittest.main()
