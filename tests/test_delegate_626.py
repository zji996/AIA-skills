"""6.26.0 regressions:
1. apply ok=false diagnostics (failedStep + writtenFiles) and Markdown conflict-marker false positives
2. applyVerify=false factual wording vs prominent real failures
3. apply uncommitted reminder + colleague commit titles + opt-in --commit (fail-closed)
4. start not blocked by apply's slow non-write phases (e.g. regeneration) and progress line when waiting for write lock
"""
import fcntl
import hashlib
import json
import shlex
import subprocess
import threading
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate626Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def run_write(self, repo, pre, name="task", *options):
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre=pre)
        return self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", name, *options, "task")
        )

    def test_apply_regeneration_failure_reports_step_and_written_files(self):
        cfg = {
            "applyVerify": False,
            "generated": {
                "paths": ["gen/"],
                "inputs": ["src/"],
                "command": "echo generator-failed-here >&2; exit 2",
            },
        }
        repo = self.repo({
            ".delegate.json": json.dumps(cfg),
            "docs/guide.md": "v0\n",
            "src/a.txt": "a0\n",
            "gen/out.txt": "g0\n",
        })
        pre = (
            "echo v1 > docs/guide.md && git add docs/guide.md && "
            "git -c user.name=t -c user.email=t@t commit -qm 'docs: update guide' && "
            "echo a1 > src/a.txt && git add src/a.txt && "
            "git -c user.name=t -c user.email=t@t commit -qm 'feat: update src'"
        )
        state = self.run_write(repo, pre, "regen-fail", "--accept", "true")

        res = self.cli("apply", state["run"], human=True)
        self.assertEqual(res.returncode, 1, res.stdout + res.stderr)
        # Both commits' files landed in the source working tree even though regeneration failed.
        self.assertEqual((repo / "docs/guide.md").read_text(), "v1\n")
        self.assertEqual((repo / "src/a.txt").read_text(), "a1\n")
        # JSON payload (printed on stdout before human lines) and human summary explain step + written files.
        payload = self.outcome(res)
        self.assertFalse(payload["apply"]["ok"])
        self.assertEqual(payload["apply"]["failedStep"], "regeneration")
        self.assertEqual(payload["apply"]["generateExitCode"], 2)
        self.assertIn("generateLog", payload["apply"])
        self.assertEqual(sorted(payload["apply"]["writtenFiles"]), ["docs/guide.md", "src/a.txt"])
        self.assertIn("!!! apply 失败（阶段：重新生成生成物 / regeneration", res.stdout)
        self.assertIn("以下 2 个文件已写入源工作区", res.stdout)
        self.assertIn("docs/guide.md", res.stdout)
        self.assertIn("src/a.txt", res.stdout)
        self.assertIn("apply failed at regeneration", res.stderr)

    def test_apply_conflict_without_merge_reports_step_and_nothing_written(self):
        repo = self.repo({"a.txt": "base\n"})
        state = self.run_write(repo, "echo colleague > a.txt", "conflict-abort")
        (repo / "a.txt").write_text("caller\n")

        res = self.cli("apply", state["run"], human=True)
        self.assertEqual(res.returncode, 1, res.stdout + res.stderr)
        payload = self.outcome(res)
        self.assertFalse(payload["apply"]["ok"])
        self.assertEqual(payload["apply"]["failedStep"], "conflicts")
        self.assertEqual(payload["apply"]["writtenFiles"], [])
        self.assertEqual(payload["apply"]["conflicts"], ["a.txt"])
        self.assertIn("!!! apply 失败（阶段：文件冲突 / conflicts", res.stdout)
        self.assertIn("未写入源工作区", res.stdout)
        self.assertEqual((repo / "a.txt").read_text(), "caller\n")

    def test_markdown_code_fence_and_setext_heading_do_not_false_positive(self):
        repo = self.repo({"docs/notes.md": "old\n"})
        md_content = (
            "Section Title\n"
            "=======\n\n"
            "Example marker syntax:\n\n"
            "```text\n"
            ">>>>>>> example-label\n"
            "=======\n"
            "```\n"
        )
        pre = f"python3 -c {shlex.quote(f'open(\"docs/notes.md\", \"w\").write({md_content!r})')}"
        state = self.run_write(repo, pre, "md-clean")
        res = self.cli("apply", "--json", state["run"])
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        payload = self.outcome(res)
        self.assertTrue(payload["apply"]["ok"])
        self.assertNotIn("conflictMarkers", payload["apply"])
        self.assertEqual((repo / "docs/notes.md").read_text(), md_content)

        # A real complete conflict block inside a Markdown code fence is still caught.
        real_conflict = "```text\n<<<<<<< current\nleft\n=======\nright\n>>>>>>> run-2\n```\n"
        pre2 = f"python3 -c {shlex.quote(f'open(\"docs/notes.md\", \"w\").write({real_conflict!r})')}"
        state2 = self.run_write(repo, pre2, "md-real-conflict")
        res2 = self.cli("apply", "--json", state2["run"])
        self.assertEqual(res2.returncode, 1, res2.stdout + res2.stderr)
        payload2 = self.outcome(res2)
        self.assertFalse(payload2["apply"]["ok"])
        self.assertEqual(payload2["apply"]["conflictMarkers"], ["docs/notes.md"])
        self.assertEqual(payload2["apply"]["failedStep"], "conflict-markers")
        self.assertEqual(payload2["apply"]["writtenFiles"], ["docs/notes.md"])

    def test_apply_verify_false_wording_is_factual_while_failures_stay_prominent(self):
        repo = self.repo({
            ".delegate.json": json.dumps({"applyVerify": False}),
            "a.txt": "a0\n",
            "other.txt": "o0\n",
        })
        state = self.run_write(repo, "echo a1 > a.txt", "verify-off", "--accept", "echo check-all")
        (repo / "other.txt").write_text("caller moved main\n")

        res = self.cli("apply", state["run"], human=True)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        self.assertIn(
            "主干已前进，验收快照失效；本仓库配置合入后不自动验收，批量门禁时跑：echo check-all",
            res.stdout,
        )
        self.assertNotIn("验收无效", res.stdout)
        # JSON must still omit `verify` when applyVerify=false.
        payload = self.outcome(res)
        self.assertNotIn("verify", payload)
        self.assertTrue(payload.get("applyVerifyDisabled"))

        # When apply itself fails (conflict), prominent failure warning still appears.
        state2 = self.run_write(repo, "echo a2 > a.txt", "verify-off-conflict", "--accept", "echo check-all")
        (repo / "a.txt").write_text("caller-conflict\n")
        failed = self.cli("apply", state2["run"], human=True)
        self.assertEqual(failed.returncode, 1, failed.stdout + failed.stderr)
        self.assertIn("!!! apply 失败", failed.stdout)
        self.assertNotIn("本仓库配置合入后不自动验收", failed.stdout)

    def test_apply_reminds_uncommitted_files_and_lists_colleague_commit_titles(self):
        repo = self.repo({"a.txt": "a0\n", "b.txt": "b0\n"})
        pre = (
            "echo a1 > a.txt && git add a.txt && "
            "git -c user.name=t -c user.email=t@t commit -qm 'feat: first change' && "
            "echo b1 > b.txt && git add b.txt && "
            "git -c user.name=t -c user.email=t@t commit -qm 'docs: second change'"
        )
        state = self.run_write(repo, pre, "uncommitted-hint", "--accept", "true")
        res = self.cli("apply", state["run"], human=True)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        self.assertIn("改动未提交：2 个文件", res.stdout)
        self.assertIn("feat: first change", res.stdout)
        self.assertIn("docs: second change", res.stdout)
        self.assertIn("改动未提交：2 个文件", res.stderr)
        self.assertIn("feat: first change", res.stderr)
        # Last line remains acceptance validity for downstream parsers/tests.
        self.assertEqual(res.stdout.splitlines()[-1], "验收仍然有效（仓库快照范围）")

    def test_apply_commit_flag_squashes_with_titles_and_fails_closed(self):
        repo = self.repo({"a.txt": "a0\n", "b.txt": "b0\n", "other.txt": "o0\n"})
        pre = (
            "echo a1 > a.txt && git add a.txt && "
            "git -c user.name=t -c user.email=t@t commit -qm 'feat: add a1' && "
            "echo b1 > b.txt && git add b.txt && "
            "git -c user.name=t -c user.email=t@t commit -qm 'test: add b1'"
        )
        state = self.run_write(repo, pre, "squash-commit", "--accept", "true")
        # Leave an unrelated dirty file in source; --commit must NOT sweep it in.
        (repo / "other.txt").write_text("caller dirty\n")

        # --commit and --keep-commits are mutually exclusive.
        bad = self.cli("apply", "--commit", "--keep-commits", state["run"])
        self.assertEqual(bad.returncode, 2)

        res = self.cli("apply", "--commit", state["run"], human=True)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        self.assertIn("已提交 ", res.stdout)
        self.assertIn("feat: add a1", res.stdout)
        self.assertNotIn("改动未提交", res.stdout)
        payload = self.outcome(res)
        self.assertTrue(payload["apply"]["ok"])
        self.assertTrue(payload["apply"]["committed"])
        self.assertTrue(payload["apply"].get("commit"))

        msg = subprocess.check_output(["git", "-C", str(repo), "log", "-1", "--format=%B"], text=True)
        self.assertIn("feat: add a1", msg)
        self.assertIn("- test: add b1", msg)
        committed_files = subprocess.check_output(
            ["git", "-C", str(repo), "diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
            text=True,
        ).splitlines()
        self.assertEqual(sorted(committed_files), ["a.txt", "b.txt"])
        # Unrelated dirty file stays uncommitted in source.
        self.assertEqual((repo / "other.txt").read_text(), "caller dirty\n")
        status = subprocess.check_output(["git", "-C", str(repo), "status", "--porcelain"], text=True)
        self.assertIn(" M other.txt", status)

        # Fail closed when source index already has staged changes.
        state2 = self.run_write(repo, "echo a2 > a.txt", "dirty-index-commit")
        subprocess.run(["git", "-C", str(repo), "add", "other.txt"], check=True)
        refused = self.cli("apply", "--commit", state2["run"], human=True)
        self.assertEqual(refused.returncode, 1, refused.stdout + refused.stderr)
        p2 = self.outcome(refused)
        self.assertFalse(p2["apply"]["ok"])
        self.assertEqual(p2["apply"]["failedStep"], "commit")
        self.assertEqual(p2["apply"]["writtenFiles"], ["a.txt"])
        self.assertIn("自动提交 / commit", refused.stdout)
        self.assertIn("以下 1 个文件已写入源工作区：a.txt", refused.stdout)

    def test_start_not_blocked_while_apply_runs_slow_generator(self):
        gen_started = self.work / "gen-started"
        gen_gate = self.work / "gen-gate"
        gen_cmd = (
            f"touch {shlex.quote(str(gen_started))}; "
            f"while [ ! -f {shlex.quote(str(gen_gate))} ]; do sleep 0.05; done; "
            f"cp a.txt gen/out.txt"
        )
        cfg = {
            "applyVerify": False,
            "generated": {"paths": ["gen/"], "inputs": ["a.txt"], "command": gen_cmd},
        }
        repo = self.repo({
            ".delegate.json": json.dumps(cfg),
            "a.txt": "a0\n",
            "gen/out.txt": "g0\n",
        })
        first = self.run_write(repo, "echo a1 > a.txt", "slow-gen-apply")

        proc = subprocess.Popen(
            [str(fixtures.DELEGATE), "apply", first["run"]],
            env=self.env,
            cwd=repo,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(lambda: proc.poll() is None and proc.kill())
        self.until(gen_started.exists, "apply generator started")

        # While apply is blocked inside generated.command, `start` must complete promptly.
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
        t0 = time.monotonic()
        started = self.cli("start", "--worktree", "--workdir", repo, "--name", "fast-start", "task", timeout=10)
        elapsed = time.monotonic() - t0
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
        self.assertLess(elapsed, 5.0, f"start waited too long during apply regeneration: {elapsed:.2f}s")
        # Generator is still running while start already succeeded.
        self.assertIsNone(proc.poll(), "apply generator should still be waiting on gate")

        gen_gate.touch()
        out, err = proc.communicate(timeout=15)
        self.assertEqual(proc.returncode, 0, out + err)
        self.assertEqual((repo / "gen/out.txt").read_text(), "a1\n")
        start_info = self.outcome(started)
        self.cli("wait", start_info["run"])

    def test_start_prints_progress_when_waiting_for_apply_write_lock(self):
        repo = self.repo({"a.txt": "a0\n"})
        lock_dir = (
            self.work
            / "state/delegate/source-watch"
            / hashlib.sha1(str(repo.resolve()).encode()).hexdigest()
        )
        lock_dir.mkdir(parents=True, exist_ok=True)
        lock_path = lock_dir / "lock"
        lock_file = lock_path.open("a+")
        fcntl.flock(lock_file, fcntl.LOCK_EX)

        def release_soon():
            time.sleep(0.35)
            fcntl.flock(lock_file, fcntl.LOCK_UN)
            lock_file.close()

        releaser = threading.Thread(target=release_soon, daemon=True)
        releaser.start()
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
        started = self.cli("start", "--worktree", "--workdir", repo, "--name", "wait-write-lock", "task", timeout=10)
        releaser.join(timeout=5)
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
        self.assertIn("等待 apply 完成写盘", started.stderr)
        self.cli("wait", self.outcome(started)["run"])


if __name__ == "__main__":
    unittest.main()
