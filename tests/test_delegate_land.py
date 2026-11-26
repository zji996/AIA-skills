"""Tests for `delegate land`, `apply --commit -m/-F`, full-chain staging, and APPLY/LAND headline/footer contracts."""

import json
import os
import re
import stat
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateLandTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def git(self, repo: Path, *args: str) -> str:
        res = subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            text=True,
            check=True,
        )
        return res.stdout.strip()

    def test_1_land_reply_chain_full_files_custom_message_trailer_and_clean(self):
        repo = self.repo(
            {
                "a.txt": "a0\n",
                "remove_me.txt": "bye\n",
                "old_name.txt": "same line 1\nsame line 2\n",
                "other.txt": "other0\n",
            }
        )
        # Round 1: modify a.txt, delete remove_me.txt, rename old_name.txt -> new_name.txt.
        self.fake_pi(
            [fixtures.answer("round 1"), fixtures.SETTLED],
            pre=(
                "echo a1 > a.txt && rm remove_me.txt && mv old_name.txt new_name.txt && "
                "git add -A && git -c user.name=t -c user.email=t@t commit -qm 'r1: restructure'"
            ),
        )
        r1 = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--workdir", repo, "--name", "land-chain", "--accept", "true", "r1")
        )
        self.assertEqual(r1["state"], "delivered")

        # Round 2: add new.txt only (round 2 itself doesn't touch round 1's files).
        self.fake_pi(
            [fixtures.answer("round 2"), fixtures.SETTLED],
            pre="echo brand-new > new.txt && git add new.txt && git -c user.name=t -c user.email=t@t commit -qm 'r2: add new'",
        )
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "r2", cwd=repo))
        self.assertEqual(r2["state"], "delivered")

        # Leave an unrelated dirty file in source; `land` must NOT stage or commit it.
        (repo / "other.txt").write_text("caller dirty\n", encoding="utf-8")

        res = self.cli("land", r2["run"], "-m", "feat: full chain land", cwd=repo, human=True)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        lines = res.stdout.splitlines()
        self.assertEqual(lines[0], f"LAND OK {r2['run']}")
        self.assertEqual(lines[-1], f"LAND OK {r2['run']}")
        self.assertIn("已提交 ", res.stdout)

        # Verify commit message subject + Delegate-Run trailer.
        subject = self.git(repo, "log", "-1", "--format=%s")
        body = self.git(repo, "log", "-1", "--format=%B")
        self.assertEqual(subject, "feat: full chain land")
        self.assertIn(f"Delegate-Run: {r2['run']}", body)

        # Verify committed file set matches the full chain (including deletion & rename), excluding other.txt.
        committed = sorted(
            self.git(repo, "diff-tree", "--no-commit-id", "--name-only", "--no-renames", "-r", "HEAD").splitlines()
        )
        self.assertEqual(committed, ["a.txt", "new.txt", "new_name.txt", "old_name.txt", "remove_me.txt"])
        self.assertFalse((repo / "remove_me.txt").exists())
        self.assertFalse((repo / "old_name.txt").exists())
        self.assertEqual((repo / "new_name.txt").read_text(encoding="utf-8"), "same line 1\nsame line 2\n")
        self.assertEqual((repo / "other.txt").read_text(encoding="utf-8"), "caller dirty\n")
        porcelain = subprocess.check_output(["git", "-C", str(repo), "status", "--porcelain"], text=True)
        self.assertIn(" M other.txt", porcelain)

        # Worktree & run auto-cleaned after successful land.
        self.assertFalse(Path(r2["worktree"]).exists())
        self.assertFalse(Path(r2["dir"]).exists())

        # Re-running `land` on already-applied run is idempotent.
        again = self.cli("land", r2["run"], cwd=repo, human=True)
        self.assertEqual(again.returncode, 0, again.stdout + again.stderr)
        again_lines = again.stdout.splitlines()
        self.assertTrue(again_lines[0].startswith(f"LAND OK {r2['run']}（已于 "), again.stdout)
        self.assertEqual(again_lines[-1], again_lines[0])

    def test_2_land_default_title_and_file_msg_and_apply_commit_options(self):
        repo = self.repo({"a.txt": "a0\n", "b.txt": "b0\n", "del.txt": "d0\n"})

        # 2a) `land` without -m reuses colleague's original commit title + trailer.
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre="echo a1 > a.txt && git add a.txt && git -c user.name=t -c user.email=t@t commit -qm 'fix: colleague title'",
        )
        r1 = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--workdir", repo, "--name", "orig-title", "--accept", "true", "t1")
        )
        res1 = self.cli("land", r1["run"], cwd=repo, human=True)
        self.assertEqual(res1.returncode, 0, res1.stdout + res1.stderr)
        self.assertEqual(res1.stdout.splitlines()[0], f"LAND OK {r1['run']}")
        self.assertEqual(self.git(repo, "log", "-1", "--format=%s"), "fix: colleague title")
        self.assertIn(f"Delegate-Run: {r1['run']}", self.git(repo, "log", "-1", "--format=%B"))

        # 2b) `land -F <file>` reads message from file + attaches trailer.
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre="echo b1 > b.txt && git add b.txt && git -c user.name=t -c user.email=t@t commit -qm 'ignored title'",
        )
        r2 = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--workdir", repo, "--name", "file-msg", "--accept", "true", "t2")
        )
        msg_file = self.work / "commit-msg.txt"
        msg_file.write_text("chore: from file\n\ndetailed body line\n", encoding="utf-8")
        res2 = self.cli("land", r2["run"], "-F", str(msg_file), cwd=repo, human=True)
        self.assertEqual(res2.returncode, 0, res2.stdout + res2.stderr)
        body2 = self.git(repo, "log", "-1", "--format=%B")
        self.assertTrue(body2.startswith("chore: from file\n\ndetailed body line"), body2)
        self.assertIn(f"Delegate-Run: {r2['run']}", body2)

        # 2c) `apply --commit --message` & full-chain deletion staging.
        self.fake_pi(
            [fixtures.answer("r1"), fixtures.SETTLED],
            pre="rm del.txt && git add -A && git -c user.name=t -c user.email=t@t commit -qm 'remove del'",
        )
        r3a = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--workdir", repo, "--name", "apply-msg", "--accept", "true", "t3a")
        )
        self.fake_pi(
            [fixtures.answer("r2"), fixtures.SETTLED],
            pre="echo a2 > a.txt && git add a.txt && git -c user.name=t -c user.email=t@t commit -qm 'edit a'",
        )
        r3b = self.outcome(self.cli("reply", "--wait", r3a["run"], "t3b", cwd=repo))

        # Usage validation checks (exit 2).
        self.assertEqual(self.cli("apply", "-m", "no-commit", r3b["run"], cwd=repo).returncode, 2)
        self.assertEqual(self.cli("land", "-m", "both", "-F", str(msg_file), r3b["run"], cwd=repo).returncode, 2)
        self.assertEqual(self.cli("land", "-mattached", r3b["run"], cwd=repo).returncode, 2)
        self.assertEqual(self.cli("land", "--message=equals", r3b["run"], cwd=repo).returncode, 2)
        self.assertEqual(self.cli("land", "-m", "   ", r3b["run"], cwd=repo).returncode, 2)

        res3 = self.cli("apply", "--commit", "--message", "feat: apply custom msg", r3b["run"], cwd=repo, human=True)
        self.assertEqual(res3.returncode, 0, res3.stdout + res3.stderr)
        lines3 = res3.stdout.splitlines()
        self.assertEqual(lines3[0], f"APPLY OK {r3b['run']}")
        self.assertEqual(lines3[-1], f"APPLY OK {r3b['run']}")
        self.assertEqual(self.git(repo, "log", "-1", "--format=%s"), "feat: apply custom msg")
        self.assertIn(f"Delegate-Run: {r3b['run']}", self.git(repo, "log", "-1", "--format=%B"))
        committed3 = sorted(
            self.git(repo, "diff-tree", "--no-commit-id", "--name-only", "--no-renames", "-r", "HEAD").splitlines()
        )
        self.assertEqual(committed3, ["a.txt", "del.txt"])
        self.assertFalse((repo / "del.txt").exists())

    def test_3_land_verify_merge_accept_accept_still_valid_and_no_verify(self):
        # 3a) mergeAccept runs on source workspace even when applyVerify: false.
        marker = self.work / "merge-accept-ran"
        cfg = {"applyVerify": False, "mergeAccept": f"echo ran >> {marker}"}
        repo = self.repo({".delegate.json": json.dumps(cfg), "a.txt": "a0\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo a1 > a.txt")
        r1 = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--workdir", repo, "--name", "ma-pass", "--accept", "true", "t1")
        )
        res1 = self.cli("land", r1["run"], "-m", "feat: ma pass", cwd=repo, human=True)
        self.assertEqual(res1.returncode, 0, res1.stdout + res1.stderr)
        self.assertTrue(marker.is_file(), "expected mergeAccept to run on source workspace")
        self.assertEqual(res1.stdout.splitlines()[0], f"LAND OK {r1['run']}")
        self.assertIn("合并后验收通过", res1.stdout)

        # 3b) Failing mergeAccept -> LAND FAILED（验收失败）, files kept, no commit, worktree kept, resume hint printed.
        (repo / ".delegate.json").write_text(json.dumps({"applyVerify": False, "mergeAccept": "false"}), encoding="utf-8")
        self.git(repo, "add", ".delegate.json")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "set failing mergeAccept")
        head_before = self.git(repo, "rev-parse", "HEAD")

        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo a2 > a.txt")
        r2 = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--workdir", repo, "--name", "ma-fail", "--accept", "true", "t2")
        )
        res2 = self.cli("land", r2["run"], "-m", "feat: should not commit", cwd=repo, human=True)
        self.assertEqual(res2.returncode, 1, res2.stdout + res2.stderr)
        lines2 = res2.stdout.splitlines()
        self.assertEqual(lines2[0], f"LAND FAILED（验收失败） {r2['run']}")
        self.assertEqual(lines2[-1], f"LAND FAILED（验收失败） {r2['run']}")
        self.assertIn("下一步：", res2.stdout)
        self.assertEqual((repo / "a.txt").read_text(encoding="utf-8"), "a2\n")
        self.assertEqual(self.git(repo, "rev-parse", "HEAD"), head_before)
        self.assertTrue(Path(r2["worktree"]).exists())

        # 3c) `--no-verify` skips failing mergeAccept and explicitly reports skip in output.
        self.git(repo, "checkout", "--", "a.txt")
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo a3 > a.txt")
        r3 = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--workdir", repo, "--name", "no-verify", "--accept", "true", "t3")
        )
        res3 = self.cli("land", "--no-verify", r3["run"], "-m", "feat: skip verify", cwd=repo, human=True)
        self.assertEqual(res3.returncode, 0, res3.stdout + res3.stderr)
        self.assertEqual(res3.stdout.splitlines()[0], f"LAND OK {r3['run']}")
        self.assertIn("已跳过验收（--no-verify）", res3.stdout)
        self.assertEqual(self.git(repo, "log", "-1", "--format=%s"), "feat: skip verify")

    def test_3b_land_verify_accept_still_valid_and_ignores_apply_verify_false(self):
        # 3d) Without mergeAccept: clean mainline -> acceptStillValid skips re-running accept and prints 沿用交付时验收.
        repo2 = self.repo({"x.txt": "x0\n", "other.txt": "o0\n", ".delegate.json": json.dumps({"applyVerify": False})})
        accept_counter = self.work / "accept-count"
        accept_cmd = f"echo tick >> {accept_counter}"
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo x1 > x.txt")
        r4 = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--workdir", repo2, "--name", "reuse-accept", "--accept", accept_cmd, "t4")
        )
        self.assertEqual(accept_counter.read_text(encoding="utf-8").splitlines(), ["tick"])
        res4 = self.cli("land", r4["run"], "-m", "feat: reuse accept", cwd=repo2, human=True)
        self.assertEqual(res4.returncode, 0, res4.stdout + res4.stderr)
        self.assertEqual(res4.stdout.splitlines()[0], f"LAND OK {r4['run']}")
        self.assertIn("沿用交付时验收", res4.stdout)
        # Accept did NOT re-run because acceptStillValid was true.
        self.assertEqual(accept_counter.read_text(encoding="utf-8").splitlines(), ["tick"])

        # 3e) Without mergeAccept, mainline advanced + applyVerify: false -> land MUST ignore applyVerify: false and run accept!
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo x2 > x.txt")
        r5 = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--workdir", repo2, "--name", "rerun-accept", "--accept", accept_cmd, "t5")
        )
        before_ticks = len(accept_counter.read_text(encoding="utf-8").splitlines())
        # Advance mainline on an unrelated file so acceptStillValid becomes false.
        (repo2 / "other.txt").write_text("o1\n", encoding="utf-8")
        self.git(repo2, "add", "other.txt")
        self.git(repo2, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "advance mainline")

        res5 = self.cli("land", r5["run"], "-m", "feat: rerun accept", cwd=repo2, human=True)
        self.assertEqual(res5.returncode, 0, res5.stdout + res5.stderr)
        self.assertEqual(res5.stdout.splitlines()[0], f"LAND OK {r5['run']}")
        self.assertIn("合并后验收通过", res5.stdout)
        after_ticks = len(accept_counter.read_text(encoding="utf-8").splitlines())
        self.assertEqual(after_ticks, before_ticks + 1)

    def test_4_land_conflict_exits_3_and_prints_land_failed_first_and_last(self):
        repo = self.repo({"shared.txt": "base\n"})
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre="echo colleague > shared.txt && git add shared.txt && git -c user.name=t -c user.email=t@t commit -qm 'colleague'",
        )
        run = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--workdir", repo, "--name", "land-conf", "--accept", "true", "edit")
        )
        (repo / "shared.txt").write_text("main\n", encoding="utf-8")
        self.git(repo, "add", "shared.txt")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "main edit")
        head_before = self.git(repo, "rev-parse", "HEAD")

        res = self.cli("land", run["run"], "-m", "feat: conflict", cwd=repo, human=True)
        self.assertEqual(res.returncode, 3, res.stdout + res.stderr)
        lines = res.stdout.splitlines()
        self.assertEqual(lines[0], f"LAND FAILED（未合入：文件冲突） {run['run']}")
        self.assertEqual(lines[-1], lines[0])
        self.assertEqual((repo / "shared.txt").read_text(encoding="utf-8"), "main\n")
        self.assertEqual(self.git(repo, "rev-parse", "HEAD"), head_before)

    def test_5_land_commit_hook_failure_keeps_staged_while_apply_commit_unstages(self):
        repo = self.repo({"a.txt": "a0\n", "b.txt": "b0\n", "other.txt": "o0\n"})
        hook_path = repo / ".git" / "hooks" / "pre-commit"
        hook_gate = self.work / "allow-commit"
        hook_path.write_text(
            f"#!/bin/sh\nif [ ! -f {hook_gate} ]; then echo 'hook blocked' >&2; exit 1; fi\n",
            encoding="utf-8",
        )
        hook_path.chmod(hook_path.stat().st_mode | stat.S_IXUSR)

        # Unrelated dirty file in source MUST stay untouched.
        (repo / "other.txt").write_text("caller dirty\n", encoding="utf-8")

        # 5a) `land` fails pre-commit hook -> keeps written file AND staged index + prints `git commit -F` resume cmd.
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo a1 > a.txt")
        r_land = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--workdir", repo, "--name", "hook-land", "--accept", "true", "t1")
        )
        res_land = self.cli("land", r_land["run"], "-m", "feat: hook test", cwd=repo, human=True)
        self.assertEqual(res_land.returncode, 1, res_land.stdout + res_land.stderr)
        land_lines = res_land.stdout.splitlines()
        self.assertEqual(land_lines[0], f"LAND FAILED（自动提交） {r_land['run']}")
        self.assertEqual(land_lines[-1], land_lines[0])
        # `a.txt` is written AND still staged in index; `other.txt` remains unstaged dirty.
        status_land = subprocess.check_output(["git", "-C", str(repo), "status", "--porcelain"], text=True)
        self.assertIn("M  a.txt", status_land, status_land)
        self.assertIn(" M other.txt", status_land, status_land)
        # Resume command uses saved commit message file with Delegate-Run trailer.
        next_lines = [ln for ln in res_land.stdout.splitlines() if ln.strip().startswith("下一步：")]
        self.assertTrue(next_lines, f"expected '下一步：' line in:\n{res_land.stdout}")
        m = re.search(r"commit -F (\S+)", next_lines[0])
        self.assertIsNotNone(m, f"expected 'commit -F <path>' resume command in:\n{next_lines[0]}")
        saved_msg_path = Path(m.group(1).strip("'\""))
        self.assertTrue(saved_msg_path.is_file(), saved_msg_path)
        saved_msg = saved_msg_path.read_text(encoding="utf-8")
        self.assertIn("feat: hook test", saved_msg)
        self.assertIn(f"Delegate-Run: {r_land['run']}", saved_msg)

        # Allow hook and run the exact resume commit -> succeeds cleanly!
        hook_gate.write_text("ok\n", encoding="utf-8")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-F", str(saved_msg_path))
        self.assertEqual(self.git(repo, "log", "-1", "--format=%s"), "feat: hook test")
        self.assertIn(f"Delegate-Run: {r_land['run']}", self.git(repo, "log", "-1", "--format=%B"))

        # 5b) `apply --commit` fails pre-commit hook -> keeps written file on disk, UNSTAGES index (original behavior).
        hook_gate.unlink()
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo b1 > b.txt")
        r_apply = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--workdir", repo, "--name", "hook-apply", "--accept", "true", "t2")
        )
        res_apply = self.cli("apply", "--commit", "-m", "feat: apply hook", r_apply["run"], cwd=repo, human=True)
        self.assertEqual(res_apply.returncode, 1, res_apply.stdout + res_apply.stderr)
        apply_lines = res_apply.stdout.splitlines()
        self.assertEqual(apply_lines[0], f"APPLY FAILED（自动提交） {r_apply['run']}")
        self.assertEqual(apply_lines[-1], apply_lines[0])
        self.assertEqual((repo / "b.txt").read_text(encoding="utf-8"), "b1\n")
        # Staged diff MUST be empty (apply --commit resets staged paths on commit failure).
        staged_after_apply = self.git(repo, "diff", "--cached", "--name-only")
        self.assertEqual(staged_after_apply, "", f"expected apply --commit to unstage on failure, got: {staged_after_apply!r}")

    def test_6_land_refuses_dirty_index_and_preserves_existing_staged_changes(self):
        repo = self.repo({"a.txt": "a0\n", "other.txt": "o0\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo a1 > a.txt")
        run = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--workdir", repo, "--name", "dirty-idx", "--accept", "true", "t1")
        )
        (repo / "other.txt").write_text("pre-staged\n", encoding="utf-8")
        self.git(repo, "add", "other.txt")
        head_before = self.git(repo, "rev-parse", "HEAD")

        res = self.cli("land", run["run"], "-m", "feat: dirty idx", cwd=repo, human=True)
        self.assertEqual(res.returncode, 1, res.stdout + res.stderr)
        lines = res.stdout.splitlines()
        self.assertEqual(lines[0], f"LAND FAILED（自动提交） {run['run']}")
        self.assertEqual(lines[-1], lines[0])
        self.assertIn("下一步：", res.stdout)
        self.assertEqual(self.git(repo, "rev-parse", "HEAD"), head_before)
        # Pre-existing staged change stays staged, written file stays on disk.
        self.assertEqual(self.git(repo, "diff", "--cached", "--name-only"), "other.txt")
        self.assertEqual((repo / "a.txt").read_text(encoding="utf-8"), "a1\n")


if __name__ == "__main__":
    unittest.main()
