"""6.27.0 regressions:
1. Pooled reply chain keeps chain-net diff口径 (meta.base snapshots worktree, not source;
   status / diff --total / protectViolation / commitsDiffer ignore unrelated main advances)
2. Main branch drift is displayed on its own line (`主干已前进 N 个提交 / M 个文件，与本链改动重叠 K 个`),
   prominent (`!!!`) only when overlap > 0
3. Default `reply` baseline sync merges cleanly onto current source, updates worktree Git HEAD,
   preserves pool slot `target/`, adds factual prompt line; falls back to old baseline with
   overlap warning and `--no-sync`/`--sync=agent` hints on conflict
"""
import json
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate627Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def git(self, repo, *args):
        return subprocess.check_output(["git", "-C", str(repo), *args], text=True).strip()

    def test_pooled_reply_chain_ignores_main_advances_and_protected_paths_with_no_sync(self):
        repo = self.repo({
            ".gitignore": "target/\n",
            ".delegate.json": json.dumps({
                "worktree": {"pool": 1},
                "defaults": {"protect": ["docs/decision/"]},
            }),
            "services/worker/a.py": "a0\n",
            "services/worker/b.py": "b0\n",
            "src/other1.py": "o1\n",
            "src/other2.py": "o2\n",
            "docs/decision/0001.md": "adr 1 v0\n",
        })

        # Round 1 in pool slot: colleague commits change to services/worker/a.py.
        self.fake_pi(
            [fixtures.answer("r1"), fixtures.SETTLED],
            pre="echo a1 > services/worker/a.py && git add services/worker/a.py && git -c user.name=t -c user.email=t@t commit -qm 'worker: update a'",
        )
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "worker-opt", "--accept", "true", "round 1")
        )
        self.assertEqual(r1["state"], "delivered", r1)

        # Source main advances 2 commits, including a protected path under docs/decision/.
        (repo / "src/other1.py").write_text("o1 from main\n")
        (repo / "src/new_from_main.py").write_text("new file\n")
        self.git(repo, "add", "src/other1.py", "src/new_from_main.py")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "main commit 1")

        (repo / "docs/decision/0001.md").write_text("adr 1 v1 from main\n")
        (repo / "docs/decision/0002.md").write_text("adr 2 from main\n")
        self.git(repo, "add", "docs/decision/0001.md", "docs/decision/0002.md")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "main protected adr")

        # Round 2 in the SAME pool slot with --no-sync: must NOT snapshot source as meta.base!
        self.fake_pi(
            [fixtures.answer("r2"), fixtures.SETTLED],
            pre="echo b1 > services/worker/b.py && git add services/worker/b.py && git -c user.name=t -c user.email=t@t commit -qm 'worker: update b'",
        )
        r2 = self.outcome(
            self.cli("reply", "--wait", "--no-sync", r1["run"], "round 2")
        )
        self.assertEqual(r2["state"], "delivered", r2)
        self.assertNotIn("protectViolation", r2)
        self.assertNotIn("commitsDiffer", r2)
        self.assertEqual(r2["changes"]["files"], 1)
        self.assertEqual(r2["pendingChanges"]["files"], 2)

        # Human `status <run>` shows ONLY the chain's 2 files (6.32: zero-side-effect read path omits live sourceDrift).
        st = self.cli("status", r2["run"], human=True)
        self.assertEqual(st.returncode, 0, st.stderr)
        self.assertIn("改 2 个文件", st.stdout)
        self.assertNotIn("触及保护路径", st.stdout)
        self.assertNotIn("主干已前进", st.stdout)

        # `diff --total` shows ONLY the chain's own 2 files.
        total_diff = self.cli("diff", "--total", r2["run"])
        self.assertEqual(total_diff.returncode, 0, total_diff.stderr)
        self.assertIn("services/worker/a.py", total_diff.stdout)
        self.assertIn("services/worker/b.py", total_diff.stdout)
        self.assertNotIn("docs/decision/", total_diff.stdout)
        self.assertNotIn("src/new_from_main.py", total_diff.stdout)

        # `apply` lands only the chain's changes and preserves main's commits intact.
        applied = self.cli("apply", "--json", r2["run"])
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertEqual((repo / "services/worker/a.py").read_text(), "a1\n")
        self.assertEqual((repo / "services/worker/b.py").read_text(), "b1\n")
        self.assertEqual((repo / "docs/decision/0001.md").read_text(), "adr 1 v1 from main\n")
        self.assertEqual((repo / "docs/decision/0002.md").read_text(), "adr 2 from main\n")
        self.assertEqual((repo / "src/new_from_main.py").read_text(), "new file\n")

    def test_source_drift_line_highlights_only_when_overlap_is_nonzero(self):
        repo = self.repo({"a.txt": "a0\n", "b.txt": "b0\n", "c.txt": "c0\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo a1 > a.txt; echo b1 > b.txt")
        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "drift-check", "task"))

        # 6.32.0: read paths (`status`, `wait`) stay zero-side-effect and do not run live `source_drift`.
        (repo / "c.txt").write_text("c1\n")
        self.git(repo, "add", "c.txt")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "disjoint")
        calm = self.cli("status", r1["run"], human=True)
        self.assertEqual(calm.returncode, 0, calm.stderr)
        self.assertIn("改 2 个文件", calm.stdout)
        self.assertNotIn("主干已前进", calm.stdout)

        (repo / "b.txt").write_text("b-main\n")
        self.git(repo, "add", "b.txt")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "overlap b")
        warn_status = self.cli("status", r1["run"], human=True)
        self.assertEqual(warn_status.returncode, 0, warn_status.stderr)
        self.assertIn("改 2 个文件", warn_status.stdout)
        self.assertNotIn("主干已前进", warn_status.stdout)
        self.assertEqual(list(Path(r1["dir"]).glob(".snapshot-index-*")), [])

    def test_reply_default_auto_sync_clean_rebases_preserves_target_and_notifies_prompt(self):
        repo = self.repo({
            ".gitignore": "target/\n",
            ".delegate.json": json.dumps({
                "worktree": {"pool": 1},
                "defaults": {"protect": ["docs/decision/"]},
            }),
            "src/mine.txt": "m0\n",
            "src/other.txt": "o0\n",
            "docs/decision/0001.md": "adr0\n",
        })

        # Round 1 commits src/mine.txt and seeds incremental build cache in pool slot's target/.
        self.fake_pi(
            [fixtures.answer("r1"), fixtures.SETTLED],
            pre=(
                "echo m1 > src/mine.txt && git add src/mine.txt && "
                "git -c user.name=t -c user.email=t@t commit -qm 'feat: mine 1' && "
                "mkdir -p target/incremental && printf 'cached-obj' > target/incremental/cache.bin"
            ),
        )
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "auto-sync", "--accept", "true", "第一步")
        )
        self.assertEqual(r1["state"], "delivered", r1)
        slot = Path(r1["worktree"])
        cache_file = slot / "target/incremental/cache.bin"
        self.assertTrue(cache_file.is_file())
        cache_mtime = cache_file.stat().st_mtime_ns

        # Main advances 2 commits (including a protected ADR).
        (repo / "src/other.txt").write_text("o1-from-main\n")
        self.git(repo, "add", "src/other.txt")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "main other")
        (repo / "docs/decision/0002.md").write_text("adr2-from-main\n")
        self.git(repo, "add", "docs/decision/0002.md")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "main adr 2")
        new_main = self.git(repo, "rev-parse", "HEAD")
        short_main = new_main[:7]

        # Round 2: plain reply (default auto-sync ON) sees main's new files, keeps target/, commits src/mine2.txt.
        self.fake_pi(
            [fixtures.answer("r2"), fixtures.SETTLED],
            pre=(
                "test \"$(cat src/other.txt)\" = 'o1-from-main' && "
                "test -f docs/decision/0002.md && "
                "echo m2 > src/mine2.txt && git add src/mine2.txt && "
                "git -c user.name=t -c user.email=t@t commit -qm 'feat: mine 2'"
            ),
        )
        res2 = self.cli("reply", "--wait", r1["run"], "继续优化")
        self.assertEqual(res2.returncode, 0, res2.stdout + res2.stderr)
        self.assertIn(f"基线已同步到主干 {short_main}", res2.stderr)
        r2 = self.outcome(res2)
        self.assertEqual(r2["state"], "delivered", r2)
        self.assertNotIn("protectViolation", r2)
        self.assertNotIn("commitsDiffer", r2)
        self.assertEqual(r2["netBase"], new_main)
        self.assertEqual(r2["changes"]["files"], 1)
        self.assertEqual(r2["pendingChanges"]["files"], 2)

        # Prompt carries factual baseline-synced notice so the colleague is not surprised.
        prompt_text = (Path(r2["dir"]) / "prompt.md").read_text()
        self.assertIn(f"基线已同步到主干 {short_main}", prompt_text)

        # Incremental target cache in pool slot was NOT wiped or rewritten.
        self.assertTrue(cache_file.is_file())
        self.assertEqual(cache_file.read_text(), "cached-obj")
        self.assertEqual(cache_file.stat().st_mtime_ns, cache_mtime)

        # Worktree commit history was cleanly replayed onto new_main.
        titles = self.git(slot, "log", "--reverse", "--format=%s", f"{new_main}..HEAD").splitlines()
        self.assertEqual(titles, ["feat: mine 1", "feat: mine 2"])

        # Status shows 2 files and no drift (already caught up to main).
        st = self.cli("status", r2["run"], human=True)
        self.assertIn("改 2 个文件", st.stdout)
        self.assertNotIn("主干已前进", st.stdout)

        # diff --total and apply remain exact; acceptStillValid is True because acceptance ran on synced main!
        total_diff = self.cli("diff", "--total", r2["run"])
        self.assertIn("src/mine.txt", total_diff.stdout)
        self.assertIn("src/mine2.txt", total_diff.stdout)
        self.assertNotIn("docs/decision/0002.md", total_diff.stdout)

        applied = self.cli("apply", "--json", r2["run"])
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        ap_json = self.outcome(applied)
        self.assertTrue(ap_json["apply"]["ok"])
        self.assertTrue(ap_json["acceptStillValid"])
        self.assertEqual((repo / "src/mine.txt").read_text(), "m1\n")
        self.assertEqual((repo / "src/mine2.txt").read_text(), "m2\n")
        self.assertEqual((repo / "docs/decision/0002.md").read_text(), "adr2-from-main\n")

    def test_reply_default_auto_sync_conflict_keeps_old_baseline_and_warns(self):
        repo = self.repo({
            ".gitignore": "target/\n",
            ".delegate.json": json.dumps({"worktree": {"pool": 1}}),
            "a.txt": "base\n",
            "b.txt": "b0\n",
        })
        self.fake_pi(
            [fixtures.answer("r1"), fixtures.SETTLED],
            pre="echo colleague-v1 > a.txt && mkdir -p target && echo obj > target/keep.o",
        )
        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "sync-conflict", "r1"))
        slot = Path(r1["worktree"])

        # Main makes a conflicting edit to a.txt plus an unrelated edit to b.txt.
        (repo / "a.txt").write_text("main-v1\n")
        (repo / "b.txt").write_text("b1-main\n")
        self.git(repo, "add", "a.txt", "b.txt")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "main conflict")

        # --sync and --no-sync are mutually exclusive.
        bad = self.cli("reply", r1["run"], "--sync", "--no-sync", "nope")
        self.assertEqual(bad.returncode, 2)

        # Default reply hits conflict: keeps old baseline atomically, warns with overlap & manual options.
        self.fake_pi([fixtures.answer("r2"), fixtures.SETTLED])
        res2 = self.cli("reply", "--wait", r1["run"], "继续处理")
        self.assertEqual(res2.returncode, 0, res2.stdout + res2.stderr)
        self.assertIn("保持旧基线", res2.stderr)
        self.assertIn("a.txt", res2.stderr)
        self.assertIn("--no-sync", res2.stderr)
        self.assertIn("--sync=agent", res2.stderr)
        # Worktree was untouched (atomic fallback: neither a.txt nor b.txt overwritten; target preserved).
        self.assertEqual((slot / "a.txt").read_text(), "colleague-v1\n")
        self.assertEqual((slot / "b.txt").read_text(), "b0\n")
        self.assertEqual((slot / "target/keep.o").read_text(), "obj\n")

        r2 = self.outcome(res2)
        self.assertEqual(r2["state"], "answered")
        self.assertEqual(r2["pendingChanges"]["files"], 1)

        st = self.cli("status", r2["run"], human=True)
        self.assertIn("改 1 个文件", st.stdout)


if __name__ == "__main__":
    unittest.main()
