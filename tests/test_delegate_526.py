"""5.26 delegate regressions driven through the source CLI."""
import importlib.util
import json
import os
import subprocess
import sys
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("delegate_fixtures_526", ROOT / "tests/test_delegate.py")
fixtures = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixtures)


class Delegate526Tests(unittest.TestCase):
    def setUp(self):
        fixtures.DelegateTests.setUp(self)

    fake_pi = fixtures.DelegateTests.fake_pi
    repo = fixtures.DelegateTests.repo
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome

    def config(self, repo, value):
        (repo / ".delegate.json").write_text(json.dumps(value))

    def test_applied_reply_retains_context_and_starts_at_current_head(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi([fixtures.answer("first answer"), fixtures.SETTLED], pre="echo one >> a.txt")
        first = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "original task"))
        self.assertEqual(self.cli("apply", first["run"]).returncode, 0)
        run = Path(first["dir"])
        archive = run.parent / ".applied" / run.name
        self.assertTrue((archive / "prompt.md").exists())
        self.assertTrue((archive / "result.md").exists())
        self.assertFalse(Path(first["worktree"]).exists())
        subprocess.run(["git", "-C", str(repo), "add", "-A"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "-qm", "applied"], check=True)
        self.fake_pi([fixtures.answer("second answer"), fixtures.SETTLED], pre="echo two >> a.txt")
        reply = self.cli("reply", "--wait", first["run"], "extra request")
        second = self.outcome(reply)
        self.assertIn("原会话已合入，已开新会话续做", reply.stderr)
        prompt = (Path(second["dir"]) / "prompt.md").read_text()
        for text in ("original task", "first answer", "extra request"):
            self.assertIn(text, prompt)
        self.assertNotEqual(first["worktree"], second["worktree"])
        self.assertIn("one", (Path(second["worktree"]) / "a.txt").read_text())
        self.assertEqual(self.cli("clean", first["run"]).returncode, 0)
        self.assertFalse(archive.exists())

    def test_keep_commits_and_default_preserve_order(self):
        repo = self.repo({"a.txt": "old\n"})
        self.config(repo, {"defaults": {"keepCommits": True}})
        pre = ("git config user.name t; git config user.email t@t; "
               "echo one > one.txt; git add one.txt; git commit -qm first; "
               "echo two > two.txt; git add two.txt; git commit -qm second")
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre=pre)
        first = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        applied = self.cli("apply", "--json", first["run"])
        self.assertEqual(applied.returncode, 0, applied.stderr)
        data = json.loads(applied.stdout.splitlines()[-1])
        self.assertEqual([c["title"] for c in data["commits"]], ["first", "second"])
        self.assertEqual([c["files"] for c in data["commits"]], [1, 1])
        titles = subprocess.check_output(["git", "-C", str(repo), "log", "-2", "--format=%s"], text=True)
        self.assertEqual(titles.splitlines(), ["second", "first"])

    def test_compression_failure_is_bounded_and_result_is_full(self):
        answer = "A" * 240
        self.fake_pi([fixtures.answer(answer), fixtures.SETTLED])
        result = self.cli("run", "--read-only", "--in-place", "--max-answer", "60", "question", human=True)
        self.assertEqual(result.returncode, 0)
        self.assertIn("压缩失败", result.stdout)
        self.assertIn("原文字数：240", result.stdout)
        self.assertNotIn(answer, result.stdout)
        run = [line.split("result: ", 1)[1].split(" ", 1)[0]
               for line in result.stdout.splitlines() if "===== result:" in line][0]
        self.assertEqual(self.cli("result", run).stdout.strip(), answer)

    def test_share_links_only_repository_paths_and_reply_inherits(self):
        repo = self.repo({".gitignore": ".local/\n", "a.txt": "old\n"})
        local = repo / ".local/input"
        local.parent.mkdir()
        local.write_text("secret")
        external = self.work / "outside"
        external.write_text("outside")
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre='test -L .local/input && cat .local/input > "$DELEGATE_RUN_DIR/shared"')
        first = self.outcome(self.cli("run", "--read-only", "--workdir", repo,
                                      "--share", local, "--share", external, "task"))
        self.assertEqual((Path(first["dir"]) / "shared").read_text(), "secret")
        self.assertIn(str(external), (Path(first["dir"]) / "prompt.md").read_text())
        second = self.outcome(self.cli("reply", "--wait", first["run"], "again"))
        self.assertEqual((Path(second["dir"]) / "shared").read_text(), "secret")

    def test_busy_tracks_lane_and_clears_after_exit(self):
        repo = self.repo({"a.txt": "old\n"})
        self.config(repo, {"resources": {"pg": {"commands": [["sleep", "2"]]}}})
        proc = subprocess.Popen([str(fixtures.DELEGATE), "lane", "sleep 2"], cwd=repo,
                                env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            for _ in range(80):
                busy = self.cli("busy", "pg", cwd=repo)
                if busy.returncode == 0:
                    break
                time.sleep(.05)
            self.assertEqual(busy.returncode, 0, busy.stderr)
            self.assertIn("sleep 2", busy.stdout)
        finally:
            proc.communicate(timeout=10)
        free = self.cli("busy", "pg", cwd=repo)
        self.assertEqual((free.returncode, free.stdout), (1, ""))

    def test_accept_blind_warns_without_rejecting(self):
        repo = self.repo({"a.txt": "old\n"})
        self.config(repo, {"acceptBlind": ["**/Dockerfile*", "manage.sh"]})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo FROM > Dockerfile")
        first = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        self.assertEqual((first["state"], first["acceptBlind"]), ("delivered", ["Dockerfile"]))
        self.assertIn("验收未覆盖：Dockerfile", self.cli("status", first["run"], human=True).stdout)
        self.assertIn("验收未覆盖：Dockerfile", self.cli("apply", first["run"]).stdout)

    def test_hook_mixed_command_and_pure_wait(self):
        hook = ROOT / "skills/delegate/hooks/claude-code-background.py"
        def check(command):
            out = subprocess.check_output([sys.executable, str(hook)], input=json.dumps({
                "tool_name": "Bash", "tool_input": {"command": command}}), text=True)
            return json.loads(out)["hookSpecificOutput"]["permissionDecisionReason"]
        mixed = check("$D wait --any & python3 analyze.py")
        self.assertIn("把 wait 单独作为一条后台命令发出", mixed)
        self.assertIn("$D wait --any", mixed)
        self.assertIn("其余部分另发一条", mixed)
        self.assertNotIn("其余部分另发一条", check("$D wait --any"))

    def test_symlink_invocation_is_used_in_next_command(self):
        alias = self.bin / "delegate"
        alias.symlink_to(fixtures.DELEGATE)
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], sleep=1)
        result = subprocess.run([str(alias), "start", "--json", "--read-only", "--in-place", "task"],
                                cwd=self.work, env=self.env, capture_output=True, text=True, timeout=15)
        state = self.outcome(result)
        self.assertIn(str(alias) + " wait", state["next"])
        self.cli("stop", state["run"])
