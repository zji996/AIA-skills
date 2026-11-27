import importlib.util
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "eval-novice.py"


def _load_eval_novice():
    spec = importlib.util.spec_from_file_location("eval_novice", SCRIPT)
    assert spec and spec.loader
    mod = importlib.util.module_from_spec(spec)
    import sys
    sys.modules[spec.name] = mod
    spec.loader.exec_module(mod)
    return mod


EVAL_NOVICE = _load_eval_novice()


class EvalNoviceUnitTests(unittest.TestCase):
    def _step(self, index: int, command: str, exit_code: int = 0, stdout: str = "", stderr: str = ""):
        return EVAL_NOVICE.CommandStep(
            index=index,
            command=command,
            argv=command.split(),
            exit_code=exit_code,
            stdout=stdout,
            stderr=stderr,
            cwd="/tmp/repo",
        )

    def test_detects_all_wasted_step_categories(self):
        steps = [
            # 1. exit 2 unknown argument
            self._step(1, "delegate work close W001 --bogus done", exit_code=2, stderr="unrecognized argument"),
            # 2. exit 76 duplicate wait
            self._step(2, "delegate wait --exclusive 20261127-120000-a", exit_code=76),
            # 3. manual polling (sleep / while / until)
            self._step(3, "while ! delegate status 20261127-120000-a; do sleep 1; done", exit_code=0),
            # 4. land fails (unapplied) followed by running test on old tree
            self._step(
                4,
                "delegate land 20261127-120000-a -m 'feat: x'",
                exit_code=3,
                stdout="LAND FAILED（未合入：文件冲突） 20261127-120000-a",
            ),
            self._step(5, "python3 -m unittest -q", exit_code=1),
            # 5. manual git commit missing Delegate-Run trailer
            self._step(6, "git commit -m 'feat: manual commit without trailer'", exit_code=0),
            # 6. redundant unchanged file read
            self._step(7, "cat app.py", exit_code=0),
            self._step(8, "cat app.py", exit_code=0),
        ]

        wasted = EVAL_NOVICE.analyze_trajectory(steps, test_cmd="python3 -m unittest -q")
        categories = [w.category for w in wasted]

        self.assertIn("exit_2_unknown_arg", categories)
        self.assertIn("exit_76_duplicate_wait", categories)
        self.assertIn("manual_polling", categories)
        self.assertIn("test_on_unapplied_tree", categories)
        self.assertIn("commit_missing_trailer", categories)
        self.assertIn("redundant_file_read", categories)

    def test_compound_semicolon_vs_and_and_tight_separators(self):
        # 1. Single `;` command where old-tree test exits 1 -> MUST detect test_on_unapplied_tree.
        semicolon_fail = [
            self._step(
                1,
                "delegate land 20261127-120000-a -m 'x'; python3 -m unittest -q",
                exit_code=1,
                stdout="LAND FAILED（未合入：文件冲突） 20261127-120000-a\nFAILED",
            ),
        ]
        self.assertEqual(
            [w.category for w in EVAL_NOVICE.analyze_trajectory(semicolon_fail, test_cmd="python3 -m unittest -q")],
            ["test_on_unapplied_tree"],
        )

        # 2. Single `;` command where old-tree test happens to exit 0 -> STILL MUST detect test_on_unapplied_tree.
        semicolon_zero = [
            self._step(
                1,
                "delegate land 20261127-120000-a -m 'x'; python3 -m unittest -q",
                exit_code=0,
                stdout="LAND FAILED（未合入：文件冲突） 20261127-120000-a\nOK",
            ),
        ]
        self.assertEqual(
            [w.category for w in EVAL_NOVICE.analyze_trajectory(semicolon_zero, test_cmd="python3 -m unittest -q")],
            ["test_on_unapplied_tree"],
        )

        # 3. Single `&&` command short-circuits on land failure -> MUST NOT false-positive.
        and_shortcircuit = [
            self._step(
                1,
                "delegate land 20261127-120000-a -m 'x' && python3 -m unittest -q",
                exit_code=3,
                stdout="LAND FAILED（未合入：文件冲突） 20261127-120000-a",
            ),
        ]
        self.assertEqual(
            EVAL_NOVICE.analyze_trajectory(and_shortcircuit, test_cmd="python3 -m unittest -q"),
            [],
        )

        # 4. Tight `;` without spaces: `;sleep` and `;pytest`.
        tight_steps = [
            self._step(1, "delegate status 20261127-120000-a;sleep 1", exit_code=0),
            self._step(
                2,
                "delegate apply 20261127-120000-a;pytest",
                exit_code=1,
                stderr="delegate: nothing applied",
            ),
        ]
        self.assertEqual(
            [w.category for w in EVAL_NOVICE.analyze_trajectory(tight_steps)],
            ["manual_polling", "test_on_unapplied_tree"],
        )

    def test_global_unknown_arg_and_env_prefix(self):
        steps = [
            self._step(1, "delegate --bogus", exit_code=2, stderr="unrecognized argument"),
            self._step(2, "FOO=1 delegate start --bogus", exit_code=2, stderr="unrecognized argument"),
            self._step(3, "env BAR=2 delegate wait --exclusive 20261127-120000-a", exit_code=76),
        ]
        wasted = EVAL_NOVICE.analyze_trajectory(steps)
        self.assertEqual(
            [w.category for w in wasted],
            ["exit_2_unknown_arg", "exit_2_unknown_arg", "exit_76_duplicate_wait"],
        )

    def test_git_commit_file_flag_and_dash_c_option(self):
        # `git commit -F <file>` in offline trajectory mode must NOT false-positive.
        resume_step = [self._step(1, "git commit -F /tmp/delegate-commit-msg.txt", exit_code=0)]
        self.assertEqual(EVAL_NOVICE.analyze_trajectory(resume_step), [])

        # `git -C <dir> commit -m ...` without trailer MUST be detected in offline mode.
        dash_c_step = [self._step(1, "git -C /tmp/repo commit -m 'manual without trailer'", exit_code=0)]
        self.assertEqual(
            [w.category for w in EVAL_NOVICE.analyze_trajectory(dash_c_step)],
            ["commit_missing_trailer"],
        )

    def test_clean_trajectory_has_zero_wasted_steps(self):
        steps = [
            # Prompt mentions sleep/while as guidance text; must NOT trigger manual_polling.
            self._step(1, "delegate start --worktree --accept 'python3 -m unittest -q' '勿用 while/sleep 轮询'", exit_code=0),
            self._step(2, "delegate wait", exit_code=0),
            # Conflict failure followed by reply --sync=agent -> wait -> land (no test on unapplied tree).
            self._step(
                3,
                "delegate land 20261127-120000-a -m 'feat: x'",
                exit_code=3,
                stdout="LAND FAILED（未合入：文件冲突） 20261127-120000-a",
            ),
            self._step(4, "delegate reply --sync=agent 20261127-120000-a 'resolve conflict'", exit_code=0),
            self._step(5, "delegate wait", exit_code=0),
            self._step(6, "delegate land 20261127-120001-b -m 'feat: x'", exit_code=0, stdout="LAND OK 20261127-120001-b"),
            # Running tests AFTER land succeeded is valid.
            self._step(7, "python3 -m unittest -q", exit_code=0),
        ]

        wasted = EVAL_NOVICE.analyze_trajectory(steps, test_cmd="python3 -m unittest -q")
        self.assertEqual(wasted, [])

    def test_git_ground_truth_for_commit_trailers_not_shortcircuited(self):
        with tempfile.TemporaryDirectory(prefix="novice-git-") as tmp:
            repo = Path(tmp) / "repo"
            repo.mkdir()
            env = {
                "PATH": "/usr/bin:/bin",
                "GIT_AUTHOR_NAME": "t",
                "GIT_AUTHOR_EMAIL": "t@t",
                "GIT_COMMITTER_NAME": "t",
                "GIT_COMMITTER_EMAIL": "t@t",
            }
            subprocess.run(["git", "-C", str(repo), "init", "-q"], env=env, check=True)
            (repo / "a.txt").write_text("v0\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(repo), "add", "a.txt"], env=env, check=True)
            subprocess.run(["git", "-C", str(repo), "commit", "-q", "-m", "init"], env=env, check=True)
            base_head = subprocess.check_output(
                ["git", "-C", str(repo), "rev-parse", "HEAD"], env=env, text=True
            ).strip()

            # Commit 1: via -F file WITH Delegate-Run trailer -> must NOT be flagged.
            msg_ok = Path(tmp) / "msg-ok.txt"
            msg_ok.write_text("feat: resumed commit\n\nDelegate-Run: 20261127-120000-a\n", encoding="utf-8")
            (repo / "a.txt").write_text("v1\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(repo), "add", "a.txt"], env=env, check=True)
            subprocess.run(["git", "-C", str(repo), "commit", "-q", "-F", str(msg_ok)], env=env, check=True)

            steps_ok = [self._step(1, f"git commit -F {msg_ok}", exit_code=0)]
            self.assertEqual(
                EVAL_NOVICE.analyze_trajectory(steps_ok, repo_path=repo, base_head=base_head, env=env),
                [],
            )

            # Commit 2: missing trailer -> MUST be flagged by git ground truth even when step list has git commit.
            (repo / "a.txt").write_text("v2\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(repo), "add", "a.txt"], env=env, check=True)
            subprocess.run(["git", "-C", str(repo), "commit", "-q", "-m", "no trailer"], env=env, check=True)

            steps_mixed = [
                self._step(1, f"git commit -F {msg_ok}", exit_code=0),
                self._step(2, "git commit -m 'no trailer'", exit_code=0),
            ]
            wasted = EVAL_NOVICE.analyze_trajectory(steps_mixed, repo_path=repo, base_head=base_head, env=env)
            self.assertEqual([w.category for w in wasted], ["commit_missing_trailer"])

    def test_model_controller_isolation_env_and_flags(self):
        host_env = {
            "HOME": "/home/real-user",
            "PATH": "/tmp/agent-shims:/usr/bin:/bin",
            "DELEGATE_CALLER": "host-caller",
            "DELEGATE_EXTRA": "leak",
            "PI_DELEGATE_ACTIVE": "1",
            "ANTIGRAVITY_CONVERSATION_ID": "conv-leak",
            "CLAUDE_CODE_SESSION_ID": "claude-leak",
            "OPENAI_API_KEY": "keep-auth",
        }
        ctx_env = {
            "DELEGATE_POLL": "0.1",
            "DELEGATE_RUNS": "/tmp/iso/runs",
            "XDG_STATE_HOME": "/tmp/iso/state",
            "XDG_CACHE_HOME": "/tmp/iso/cache",
            "XDG_CONFIG_HOME": "/tmp/iso/config",
        }
        env = EVAL_NOVICE.build_model_controller_env(ctx_env, Path("/tmp/iso/bin"), base_env=host_env)
        self.assertEqual(env["HOME"], "/home/real-user")
        self.assertEqual(env["OPENAI_API_KEY"], "keep-auth")
        self.assertEqual(env["DELEGATE_RUNS"], "/tmp/iso/runs")
        self.assertNotIn("DELEGATE_CALLER", env)
        self.assertNotIn("DELEGATE_EXTRA", env)
        self.assertNotIn("PI_DELEGATE_ACTIVE", env)
        self.assertNotIn("ANTIGRAVITY_CONVERSATION_ID", env)
        self.assertNotIn("CLAUDE_CODE_SESSION_ID", env)
        self.assertNotIn("agent-shims", env["PATH"])
        self.assertTrue(env["PATH"].startswith("/tmp/iso/bin:"))

        argv = EVAL_NOVICE.build_model_controller_argv("/usr/bin/pi", "test/model", Path("/tmp/sess"), "hello")
        self.assertIn("--no-skills", argv)
        self.assertIn("--no-context-files", argv)
        self.assertIn("--no-extensions", argv)

    def test_fixtures_and_skill_md_are_valid(self):
        self.assertEqual(EVAL_NOVICE.validate_skill_md(ROOT / "skills" / "delegate" / "SKILL.md"), [])
        scenarios = EVAL_NOVICE.load_scenarios(ROOT / "evals" / "novice-fixtures")
        ids = [s["id"] for s in scenarios]
        self.assertEqual(
            ids,
            [
                "s1-golden-path",
                "s2-conflict-recovery",
                "s3-parallel-wait",
                "s4-question-and-work",
            ],
        )


if __name__ == "__main__":
    unittest.main()
