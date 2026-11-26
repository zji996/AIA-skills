"""Regression tests for `delegate files <run>` on reply chains.

Verifies:
1. Reply chains list net changes across the entire chain relative to effective netBase/chainBase
   (same口径 as `delegate diff <run> --total`), including additions, modifications, deletions,
   and both ends of renames.
2. `delegate apply` last-line count `本 run 改动文件（N 个）：delegate files <run>` uses the same set.
3. Applied (`.applied`) runs remain queryable (even without `applied-files.txt`/`.applied.files`),
   and archived runs reconstruct the exact chain file set after auto-clean or `delegate clean`.
"""
import json
import re
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


def _parse_diff_filenames(diff_text: str) -> set[str]:
    names: set[str] = set()
    for line in diff_text.splitlines():
        m = re.match(r"^diff --git a/(.+?) b/(.+)$", line)
        if m:
            names.add(m.group(1))
            names.add(m.group(2))
    return names


class DelegateFilesChainTests(unittest.TestCase):
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def setUp(self):
        fixtures.DelegateTests.setUp(self)

    def _git(self, repo: Path, *args: str) -> str:
        res = subprocess.run(
            ["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t", *args],
            check=True,
            capture_output=True,
            text=True,
        )
        return res.stdout.strip()

    def test_reply_chain_files_matches_diff_total_and_apply_across_lifecycle(self):
        repo = self.repo({"a.txt": "orig_a\n", "c.txt": "orig_c\n"})

        # Round 1: modify a.txt, create b.txt.
        self.fake_pi(
            [fixtures.answer("round 1 done"), fixtures.SETTLED],
            pre="echo mod_a > a.txt; echo new_b > b.txt",
        )
        r1 = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--name", "chain-abc", "--accept", "true", "round 1", cwd=repo)
        )
        self.assertEqual(r1["state"], "delivered")

        # Round 2 (reply): only modify c.txt.
        self.fake_pi(
            [fixtures.answer("round 2 done"), fixtures.SETTLED],
            pre="echo mod_c > c.txt",
        )
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "round 2", cwd=repo))
        self.assertEqual(r2["state"], "delivered")
        expected = ["a.txt", "b.txt", "c.txt"]

        # 1. Before apply: `files` lists A, B, C and matches `diff --total`.
        files_pre = self.cli("files", r2["run"], cwd=repo, human=True)
        self.assertEqual(files_pre.returncode, 0, files_pre.stderr)
        self.assertEqual(files_pre.stdout.splitlines(), expected)

        files_json = self.cli("files", "--json", r2["run"], cwd=repo)
        self.assertEqual(files_json.returncode, 0, files_json.stderr)
        doc = json.loads(files_json.stdout)
        self.assertEqual(doc["count"], 3)
        self.assertEqual(doc["files"], expected)

        diff_total = self.cli("diff", r2["run"], "--total", cwd=repo, human=True)
        self.assertEqual(diff_total.returncode, 0, diff_total.stderr)
        self.assertEqual(_parse_diff_filenames(diff_total.stdout), set(expected))

        # 2. Apply with --keep: last line count matches the exact chain set (3).
        applied = self.cli("apply", "--keep", r2["run"], cwd=repo, human=True)
        self.assertEqual(applied.returncode, 0, applied.stderr + applied.stdout)
        lines = [ln for ln in applied.stdout.splitlines() if ln.strip()]
        self.assertEqual(lines[-1], f"本 run 改动文件（3 个）：delegate files {r2['run']}")
        self.assertEqual((repo / "a.txt").read_text().strip(), "mod_a")
        self.assertEqual((repo / "b.txt").read_text().strip(), "new_b")
        self.assertEqual((repo / "c.txt").read_text().strip(), "mod_c")

        # 3. After apply (`.applied` present): `files` still lists A, B, C.
        files_post = self.cli("files", r2["run"], cwd=repo, human=True)
        self.assertEqual(files_post.returncode, 0, files_post.stderr)
        self.assertEqual(files_post.stdout.splitlines(), expected)

        # 4. Simulate pre-fix `.applied` without `applied-files.txt` or `.applied["files"]`:
        #    `files` must still reconstruct A, B, C from netBase/chainBase -> after.
        r2_dir = Path(r2["dir"])
        applied_txt = r2_dir / "applied-files.txt"
        if applied_txt.exists():
            applied_txt.unlink()
        applied_marker = r2_dir / ".applied"
        st = json.loads(applied_marker.read_text(encoding="utf-8"))
        st.pop("files", None)
        applied_marker.write_text(json.dumps(st), encoding="utf-8")

        files_legacy_applied = self.cli("files", r2["run"], cwd=repo, human=True)
        self.assertEqual(files_legacy_applied.returncode, 0, files_legacy_applied.stderr)
        self.assertEqual(files_legacy_applied.stdout.splitlines(), expected)

        # 5. Clean/archive the applied run: `files` still reconstructs A, B, C from archive.
        cleaned = self.cli("clean", r2["run"], cwd=repo, human=True)
        self.assertEqual(cleaned.returncode, 0, cleaned.stderr + cleaned.stdout)
        files_archived = self.cli("files", r2["run"], cwd=repo, human=True)
        self.assertEqual(files_archived.returncode, 0, files_archived.stderr)
        self.assertEqual(files_archived.stdout.splitlines(), expected)

    def test_reply_chain_files_includes_deletions_and_both_ends_of_rename(self):
        repo = self.repo(
            {
                "remove_me.txt": "bye\n",
                "old_name.txt": "same content line 1\nsame content line 2\n",
                "c.txt": "orig_c\n",
            }
        )

        # Round 1: delete remove_me.txt and rename old_name.txt -> new_name.txt.
        self.fake_pi(
            [fixtures.answer("round 1 del+rename"), fixtures.SETTLED],
            pre="rm remove_me.txt; mv old_name.txt new_name.txt",
        )
        r1 = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--name", "chain-del-ren", "--accept", "true", "round 1", cwd=repo)
        )
        self.assertEqual(r1["state"], "delivered")

        # Round 2: modify c.txt only.
        self.fake_pi(
            [fixtures.answer("round 2 edit c"), fixtures.SETTLED],
            pre="echo edited_c > c.txt",
        )
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "round 2", cwd=repo))
        self.assertEqual(r2["state"], "delivered")

        expected = ["c.txt", "new_name.txt", "old_name.txt", "remove_me.txt"]

        files_pre = self.cli("files", r2["run"], cwd=repo, human=True)
        self.assertEqual(files_pre.returncode, 0, files_pre.stderr)
        self.assertEqual(files_pre.stdout.splitlines(), expected)

        diff_total = self.cli("diff", r2["run"], "--total", cwd=repo, human=True)
        self.assertEqual(diff_total.returncode, 0, diff_total.stderr)
        self.assertEqual(_parse_diff_filenames(diff_total.stdout), set(expected))

        # Apply without --keep (auto-cleans & archives): count is 4, and archived `files` still lists all 4.
        applied = self.cli("apply", r2["run"], cwd=repo, human=True)
        self.assertEqual(applied.returncode, 0, applied.stderr + applied.stdout)
        lines = [ln for ln in applied.stdout.splitlines() if ln.strip()]
        self.assertEqual(lines[-1], f"本 run 改动文件（4 个）：delegate files {r2['run']}")
        self.assertFalse((repo / "remove_me.txt").exists())
        self.assertFalse((repo / "old_name.txt").exists())
        self.assertTrue((repo / "new_name.txt").is_file())
        self.assertEqual((repo / "c.txt").read_text().strip(), "edited_c")

        files_archived = self.cli("files", r2["run"], cwd=repo, human=True)
        self.assertEqual(files_archived.returncode, 0, files_archived.stderr)
        self.assertEqual(files_archived.stdout.splitlines(), expected)

    def test_rebased_reply_chain_files_uses_net_base_and_survives_unapplied_archive(self):
        repo = self.repo({"base.txt": "v0\n"})

        # Round 1: create r1.txt and commit in worktree.
        self.fake_pi(
            [fixtures.answer("r1"), fixtures.SETTLED],
            pre="echo from_r1 > r1.txt; git add r1.txt; git -c user.name=t -c user.email=t@t commit -q -m r1",
        )
        r1 = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--name", "chain-rebased", "--accept", "true", "r1", cwd=repo)
        )
        self.assertEqual(r1["state"], "delivered")

        # Main advances with an unrelated file.
        (repo / "main_only.txt").write_text("unrelated\n", encoding="utf-8")
        self._git(repo, "add", "main_only.txt")
        self._git(repo, "commit", "-q", "-m", "main advance")
        new_main = self._git(repo, "rev-parse", "HEAD")

        # Round 2: rebase onto new main and create r2.txt.
        self.fake_pi(
            [fixtures.answer("r2"), fixtures.SETTLED],
            pre=(
                f"git -c user.name=t -c user.email=t@t rebase {new_main}; "
                "echo from_r2 > r2.txt; git add r2.txt; "
                "git -c user.name=t -c user.email=t@t commit -q -m r2"
            ),
        )
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "r2", cwd=repo))
        self.assertEqual(r2["state"], "delivered")
        self.assertEqual(r2["netBase"], new_main)

        expected = ["r1.txt", "r2.txt"]
        files_pre = self.cli("files", r2["run"], cwd=repo, human=True)
        self.assertEqual(files_pre.returncode, 0, files_pre.stderr)
        self.assertEqual(files_pre.stdout.splitlines(), expected)

        diff_total = self.cli("diff", r2["run"], "--total", cwd=repo, human=True)
        self.assertEqual(diff_total.returncode, 0, diff_total.stderr)
        self.assertEqual(_parse_diff_filenames(diff_total.stdout), set(expected))

        # Archive unapplied run via clean: `files` still reconstructs from archive.
        cleaned = self.cli("clean", r2["run"], cwd=repo, human=True)
        self.assertEqual(cleaned.returncode, 0, cleaned.stderr + cleaned.stdout)
        files_archived = self.cli("files", r2["run"], cwd=repo, human=True)
        self.assertEqual(files_archived.returncode, 0, files_archived.stderr)
        self.assertEqual(files_archived.stdout.splitlines(), expected)


if __name__ == "__main__":
    unittest.main()
