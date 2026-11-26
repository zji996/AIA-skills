import json
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateCliErgonomicsTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    start_gated = fixtures.DelegateTests.start_gated
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    until = fixtures.DelegateTests.until
    repo = fixtures.DelegateTests.repo

    def test_work_close_accepts_reason_alias_and_writes_evidence(self):
        repo = self.repo({"a.txt": "v0\n"})
        added = self.cli("work", "add", "测试别名关闭", "--repo", repo, "--json")
        self.assertEqual(added.returncode, 0, added.stderr)
        wid = json.loads(added.stdout)["id"]

        closed = self.cli("work", "close", wid, "--reason", "已验证通过", "--json")
        self.assertEqual(closed.returncode, 0, closed.stderr)

        shown = self.cli("work", "show", wid, "--json")
        self.assertEqual(shown.returncode, 0, shown.stderr)
        doc = json.loads(shown.stdout)
        self.assertEqual(doc["state"], "done")
        self.assertEqual(doc.get("close_evidence") or doc.get("closeEvidence"), "已验证通过")

    def test_unknown_arguments_suggest_nearest_flag_synonym_and_usage(self):
        cases = [
            (
                ("apply", "--comit"),
                "unrecognized argument '--comit'; did you mean '--commit'?",
                "usage: delegate apply ",
            ),
            (
                ("start", "--readonl", "hello"),
                "unrecognized argument '--readonl'; did you mean '--read-only'?",
                "usage: delegate start ",
            ),
            (
                ("work", "drop", "W001", "--evidence", "obsolete"),
                "unrecognized argument '--evidence'; did you mean '--reason'?",
                "usage: delegate work drop ",
            ),
            (
                ("queue", "list", "--al"),
                "unrecognized argument '--al'; did you mean '--all'?",
                "usage: delegate queue list ",
            ),
            (
                ("agy", "account", "use", "slot1", "--forc"),
                "unrecognized argument '--forc'; did you mean '--force'?",
                "usage: delegate agy account use ",
            ),
            (
                ("card", "--jso"),
                "unrecognized argument '--jso'; did you mean '--json'?",
                "usage: delegate card ",
            ),
        ]
        for argv, expected_msg, expected_usage in cases:
            with self.subTest(argv=argv):
                res = self.cli(*argv, human=True)
                self.assertEqual(res.returncode, 2, res.stdout + res.stderr)
                self.assertIn(expected_msg, res.stderr)
                self.assertIn(expected_usage, res.stderr)

    def test_concurrent_wait_attaches_by_default_and_exclusive_returns_76(self):
        self.repo({"a.txt": "v0\n"})
        run, gate = self.start_gated(
            "--read-only",
            "--name",
            "shared-wait",
            "task",
            attempts=([fixtures.answer("shared wait result"), fixtures.SETTLED],),
        )
        run_id = run["run"]
        run_dir = Path(run["dir"])

        w1 = subprocess.Popen(
            [str(fixtures.DELEGATE), "wait", run_id],
            cwd=self.work,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=self.env,
        )
        self.addCleanup(lambda: (w1.terminate(), w1.wait(timeout=5)) if w1.poll() is None else None)
        self.until(lambda: (run_dir / "waiter.lock").is_file(), "waiter #1 claimed waiter.lock")

        # --exclusive preserves legacy exit 76 behavior.
        excl = self.cli("wait", "--exclusive", run_id, human=True)
        self.assertEqual(excl.returncode, 76, excl.stdout + excl.stderr)

        # Default second wait attaches to the same run instead of exiting 76.
        w2 = subprocess.Popen(
            [str(fixtures.DELEGATE), "wait", run_id],
            cwd=self.work,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=self.env,
        )
        self.addCleanup(lambda: (w2.terminate(), w2.wait(timeout=5)) if w2.poll() is None else None)
        self.until(
            lambda: (run_dir / "waiter-attach.lock").is_file(),
            "waiter #2 acquired waiter-attach.lock",
        )

        gate.touch()
        out1, err1 = w1.communicate(timeout=15)
        out2, err2 = w2.communicate(timeout=15)
        self.assertEqual(w1.returncode, 0, out1 + err1)
        self.assertEqual(w2.returncode, 0, out2 + err2)
        self.assertIn("shared wait result", out1)
        self.assertIn("shared wait result", out2)

    def test_attached_waiter_matches_nonzero_exit_code(self):
        repo = self.repo({"a.txt": "v0\n"})
        run, gate = self.start_gated(
            "--worktree",
            "--no-scout",
            "--accept",
            "false",
            "--name",
            "shared-fail",
            "task",
            attempts=([fixtures.answer("failing run"), fixtures.SETTLED],),
            cwd=repo,
        )
        run_id = run["run"]
        run_dir = Path(run["dir"])

        w1 = subprocess.Popen(
            [str(fixtures.DELEGATE), "wait", run_id],
            cwd=repo,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=self.env,
        )
        self.addCleanup(lambda: (w1.terminate(), w1.communicate(timeout=5)) if w1.poll() is None else None)
        self.until(lambda: (run_dir / "waiter.lock").is_file(), "waiter #1 claimed waiter.lock")

        w2 = subprocess.Popen(
            [str(fixtures.DELEGATE), "wait", run_id],
            cwd=repo,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=self.env,
        )
        self.addCleanup(lambda: (w2.terminate(), w2.communicate(timeout=5)) if w2.poll() is None else None)
        self.until(
            lambda: (run_dir / "waiter-attach.lock").is_file(),
            "waiter #2 acquired waiter-attach.lock",
        )

        gate.touch()
        out1, err1 = w1.communicate(timeout=15)
        out2, err2 = w2.communicate(timeout=15)
        self.assertEqual(w1.returncode, 1, out1 + err1)
        self.assertEqual(w2.returncode, w1.returncode, out2 + err2)

    def test_stop_hook_counts_attached_waiter_as_covered(self):
        repo = self.repo({"a.txt": "v0\n"})
        hook_env = dict(self.env)
        hook_env["DELEGATE_CALLER"] = "sess-attach"
        hook_env["DELEGATE_STOP_HOOK_WAITERS"] = ""
        hook_env["DELEGATE_STOP_HOOK_PROCS"] = ""
        self.env["DELEGATE_CALLER"] = "sess-attach"

        run, gate = self.start_gated(
            "--read-only",
            "--name",
            "hook-attach",
            "task",
            attempts=([fixtures.answer("ok"), fixtures.SETTLED],),
        )
        run_id = run["run"]
        run_dir = Path(run["dir"])

        def run_hook():
            proc = subprocess.run(
                [str(fixtures.DELEGATE), "hook", "check", "--format", "claude", "--cwd", str(repo)],
                input=json.dumps({"cwd": str(repo), "session_id": "sess-attach", "stop_hook_active": False}),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=hook_env,
                timeout=10,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            return json.loads(proc.stdout) if proc.stdout.strip() else None

        w1 = subprocess.Popen(
            [str(fixtures.DELEGATE), "wait", run_id],
            cwd=repo,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=hook_env,
        )
        self.addCleanup(lambda: (w1.terminate(), w1.wait(timeout=5)) if w1.poll() is None else None)
        self.until(lambda: (run_dir / "waiter.lock").is_file(), "waiter #1 claimed waiter.lock")

        w2 = subprocess.Popen(
            [str(fixtures.DELEGATE), "wait", run_id],
            cwd=repo,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=hook_env,
        )
        self.addCleanup(lambda: (w2.terminate(), w2.wait(timeout=5)) if w2.poll() is None else None)
        self.until(
            lambda: (run_dir / "waiter-attach.lock").is_file(),
            "waiter #2 acquired waiter-attach.lock",
        )

        # Kill primary waiter so ONLY attached waiter #2 remains.
        w1.terminate()
        w1.communicate(timeout=5)
        self.assertIsNone(run_hook(), "attached waiter must keep run covered in Stop hook")

        # Once attached waiter #2 also exits, Stop hook blocks as uncovered.
        w2.terminate()
        w2.communicate(timeout=5)
        blocked = run_hook()
        self.assertIsNotNone(blocked)
        self.assertEqual(blocked["decision"], "block")
        gate.touch()


if __name__ == "__main__":
    unittest.main()
