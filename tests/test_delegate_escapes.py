"""Tests: `delegate escapes` post-merge escaped defect accounting via `Fixes-Run:` trailers,
active & archived run meta resolution, unknown run counting, multi-line trailers,
`--since`/`--repo`/`--json` options, and `apply --commit` trailer behavior.
"""
import json
import os
import subprocess
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


def _git(repo: Path, *args: str, commit_ts: int | None = None) -> str:
    env = dict(os.environ)
    env.setdefault("GIT_AUTHOR_NAME", "tester")
    env.setdefault("GIT_AUTHOR_EMAIL", "tester@example.test")
    env.setdefault("GIT_COMMITTER_NAME", "tester")
    env.setdefault("GIT_COMMITTER_EMAIL", "tester@example.test")
    if commit_ts is not None:
        stamp = f"{commit_ts} +0000"
        env["GIT_AUTHOR_DATE"] = stamp
        env["GIT_COMMITTER_DATE"] = stamp
    proc = subprocess.run(
        ["git", "-C", str(repo), *args],
        env=env,
        text=True,
        capture_output=True,
        check=True,
    )
    return proc.stdout.strip()


def _init_repo(path: Path, base_ts: int) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-q", "-b", "main")
    _git(path, "config", "user.name", "tester")
    _git(path, "config", "user.email", "tester@example.test")
    (path / "seed.txt").write_text("seed\n", encoding="utf-8")
    _git(path, "add", "-A")
    _git(path, "commit", "-qm", "init", commit_ts=base_ts)
    return path


def _commit_file(repo: Path, rel: str, content: str, message: str, commit_ts: int) -> str:
    fp = repo / rel
    fp.parent.mkdir(parents=True, exist_ok=True)
    fp.write_text(content, encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", message, commit_ts=commit_ts)
    return _git(repo, "rev-parse", "HEAD")


def _write_run_record(
    base_dir: Path,
    run_id: str,
    *,
    author_model: str,
    class_name: str | None = None,
    summary_review_model: str | None = None,
    review_json_model: str | None = None,
    config_review_model: str | None = None,
    review_enabled: bool = True,
) -> Path:
    base_dir.mkdir(parents=True, exist_ok=True)
    meta: dict = {
        "run": run_id,
        "model": author_model,
        "reviewConfig": {
            "enabled": review_enabled,
            "model": config_review_model,
        },
    }
    if class_name is not None:
        meta["class"] = class_name
    (base_dir / "meta.json").write_text(json.dumps(meta), encoding="utf-8")
    if summary_review_model is not None:
        (base_dir / "summary.json").write_text(
            json.dumps({"review": {"status": "done", "model": summary_review_model}}),
            encoding="utf-8",
        )
    if review_json_model is not None:
        (base_dir / "review.json").write_text(
            json.dumps({"status": "done", "model": review_json_model}),
            encoding="utf-8",
        )
    return base_dir


class DelegateEscapesTests(unittest.TestCase):
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def setUp(self):
        fixtures.DelegateTests.setUp(self)
        self.cache_dir = Path(self.env["XDG_CACHE_HOME"]) / "delegate"
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        self.env.pop("DELEGATE_RUNS", None)

    def test_escapes_counts_active_archived_and_unknown_runs(self):
        now = int(time.time())
        repo = _init_repo(self.work / "repo-escapes", now - 20 * 86400)

        # 1. Active run (in <repo>/.local/run/delegate/<run-id>), review model in summary.json
        _write_run_record(
            repo / ".local/run/delegate/run-a",
            "run-a",
            author_model="gpt-5.4",
            class_name="build",
            summary_review_model="claude-opus-4-6",
        )
        # 2. Archived run only (cleaned from repo, only in <cache_dir>/archive/run-b/record)
        _write_run_record(
            self.cache_dir / "archive/run-b/record",
            "run-b",
            author_model="gpt-5.4",
            class_name="hard",
            review_json_model="claude-opus-4-6",
        )
        # 3. Applied run (in .applied/), review model fallback from meta.reviewConfig
        _write_run_record(
            repo / ".local/run/delegate/.applied/run-c",
            "run-c",
            author_model="gemini-3-pro",
            class_name="build",
            config_review_model="gpt-5.4",
        )

        # 3 Delegate-Run commits
        _commit_file(repo, "a.txt", "a\n", "feat: a\n\nDelegate-Run: run-a\n", now - 5 * 86400)
        _commit_file(repo, "b.txt", "b\n", "feat: b\n\nDelegate-Run: run-b\n", now - 4 * 86400)
        _commit_file(repo, "c.txt", "c\n", "feat: c\n\nDelegate-Run: run-c\n", now - 3 * 86400)

        # 1 fix commit pointing to archived run-b
        fix_b_sha = _commit_file(
            repo,
            "b.txt",
            "b-fixed\n",
            "fix: handle edge case in b\n\nFixes-Run: run-b\n",
            now - 2 * 86400,
        )
        # 1 fix commit pointing to a non-existent run
        _commit_file(
            repo,
            "missing.txt",
            "m\n",
            "fix: unknown origin\n\nFixes-Run: run-does-not-exist\n",
            now - 86400,
        )

        # Run from outside the repo using --repo PATH
        proc = self.cli("escapes", "--repo", str(repo), "--json")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        data = json.loads(proc.stdout)

        self.assertEqual(data["mergedRuns"], 3)
        self.assertEqual(data["escapedRuns"], 2)
        self.assertEqual(data["fixCommits"], 2)
        self.assertEqual(data["unknownRuns"], 1)
        self.assertEqual(data["unknownRunIds"], ["run-does-not-exist"])

        by_rev = {row["model"]: row for row in data["byReviewer"]}
        self.assertEqual(by_rev["claude-opus-4-6"]["merged"], 2)
        self.assertEqual(by_rev["claude-opus-4-6"]["escaped"], 1)
        self.assertAlmostEqual(by_rev["claude-opus-4-6"]["rate"], 0.5)
        self.assertEqual(by_rev["gpt-5.4"]["merged"], 1)
        self.assertEqual(by_rev["gpt-5.4"]["escaped"], 0)
        self.assertAlmostEqual(by_rev["gpt-5.4"]["rate"], 0.0)

        by_author = {row["model"]: row for row in data["byAuthor"]}
        self.assertEqual(by_author["gpt-5.4"]["merged"], 2)
        self.assertEqual(by_author["gpt-5.4"]["escaped"], 1)
        self.assertAlmostEqual(by_author["gpt-5.4"]["rate"], 0.5)
        self.assertEqual(by_author["gemini-3-pro"]["merged"], 1)
        self.assertEqual(by_author["gemini-3-pro"]["escaped"], 0)

        by_class = {row["class"]: row for row in data["byClass"]}
        self.assertEqual(by_class["hard"]["merged"], 1)
        self.assertEqual(by_class["hard"]["escaped"], 1)
        self.assertAlmostEqual(by_class["hard"]["rate"], 1.0)
        self.assertEqual(by_class["build"]["merged"], 2)
        self.assertEqual(by_class["build"]["escaped"], 0)

        escapes_map = {item["run"]: item for item in data["escapes"]}
        self.assertTrue(escapes_map["run-b"]["found"])
        self.assertEqual(escapes_map["run-b"]["source"], "archive")
        self.assertEqual(escapes_map["run-b"]["class"], "hard")
        self.assertEqual(escapes_map["run-b"]["fixCommits"], [fix_b_sha])
        self.assertFalse(escapes_map["run-does-not-exist"]["found"])
        self.assertEqual(escapes_map["run-does-not-exist"]["source"], "unknown")

        # Human-readable output
        text_proc = self.cli("escapes", "--repo", str(repo))
        self.assertEqual(text_proc.returncode, 0, text_proc.stderr)
        out = text_proc.stdout
        self.assertIn("合入委派：3", out)
        self.assertIn("被 Fixes-Run 指到：2", out)
        self.assertIn("claude-opus-4-6", out)
        self.assertIn("50.0%", out)
        self.assertIn("未知 run（1）：run-does-not-exist", out)

    def test_multiline_fixes_run_and_since_window_with_zero_merged_rate(self):
        now = int(time.time())
        repo = _init_repo(self.work / "repo-window", now - 60 * 86400)

        _write_run_record(
            self.cache_dir / "archive/run-old/record",
            "run-old",
            author_model="legacy-writer",
            class_name="hard",
            summary_review_model="legacy-reviewer",
        )
        _write_run_record(
            repo / ".local/run/delegate/run-new",
            "run-new",
            author_model="new-writer",
            class_name="build",
            review_enabled=False,
        )

        # Merged 45 days ago (outside default 30d window)
        _commit_file(
            repo,
            "old.txt",
            "v1\n",
            "feat: old\n\nDelegate-Run: run-old\n",
            now - 45 * 86400,
        )
        # Merged 2 days ago (inside 30d window)
        _commit_file(
            repo,
            "new.txt",
            "v1\n",
            "feat: new\n\nDelegate-Run: run-new\n",
            now - 2 * 86400,
        )
        # Single fix commit referencing both runs via multi-line Fixes-Run
        _commit_file(
            repo,
            "fix.txt",
            "fixed\n",
            "fix: both\n\nFixes-Run: run-old\nfixes-run: run-new\nFixes-Run: run-old\n",
            now - 3600,
        )

        proc_30d = self.cli("escapes", "--since", "30d", "--repo", str(repo), "--json")
        self.assertEqual(proc_30d.returncode, 0, proc_30d.stderr)
        d30 = json.loads(proc_30d.stdout)
        self.assertEqual(d30["mergedRuns"], 1)
        self.assertEqual(d30["escapedRuns"], 2)
        self.assertEqual(d30["fixCommits"], 1)
        by_rev = {r["model"]: r for r in d30["byReviewer"]}
        self.assertEqual(by_rev["legacy-reviewer"]["merged"], 0)
        self.assertEqual(by_rev["legacy-reviewer"]["escaped"], 1)
        self.assertIsNone(by_rev["legacy-reviewer"]["rate"])
        self.assertEqual(by_rev["(无审查)"]["merged"], 1)
        self.assertEqual(by_rev["(无审查)"]["escaped"], 1)
        self.assertAlmostEqual(by_rev["(无审查)"]["rate"], 1.0)

        # With --since all, run-old is also in mergedRuns
        proc_all = self.cli("escapes", "--since", "all", "--repo", str(repo), "--json")
        self.assertEqual(proc_all.returncode, 0, proc_all.stderr)
        dall = json.loads(proc_all.stdout)
        self.assertEqual(dall["mergedRuns"], 2)
        by_rev_all = {r["model"]: r for r in dall["byReviewer"]}
        self.assertEqual(by_rev_all["legacy-reviewer"]["merged"], 1)
        self.assertAlmostEqual(by_rev_all["legacy-reviewer"]["rate"], 1.0)

    def test_apply_commit_does_not_auto_add_and_preserves_fixes_run(self):
        repo = self.repo({"seed.txt": "seed\n"})

        # 1. Normal apply --commit should add Delegate-Run but NOT Fixes-Run
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre='printf "one\\n" > one.txt',
        )
        run1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", str(repo), "--name", "one", "task")
        )["run"]
        ap1 = self.cli("apply", run1, "--commit", "-m", "feat: add one")
        self.assertEqual(ap1.returncode, 0, ap1.stdout + ap1.stderr)
        body1 = _git(repo, "log", "-1", "--format=%B")
        self.assertIn(f"Delegate-Run: {run1}", body1)
        self.assertNotIn("Fixes-Run:", body1)

        # 2. apply --commit with user-supplied Fixes-Run trailer preserves it untouched
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre='printf "fixed\\n" > one.txt',
        )
        run2 = self.outcome(
            self.cli("run", "--worktree", "--workdir", str(repo), "--name", "fix-one", "fix")
        )["run"]
        ap2 = self.cli(
            "apply",
            run2,
            "--commit",
            "-m",
            f"fix: one\n\nFixes-Run: {run1}",
        )
        self.assertEqual(ap2.returncode, 0, ap2.stdout + ap2.stderr)
        body2 = _git(repo, "log", "-1", "--format=%B")
        self.assertIn(f"Fixes-Run: {run1}", body2)
        self.assertIn(f"Delegate-Run: {run2}", body2)

        # Verify `delegate escapes` picks up the fix against run1
        esc = self.cli("escapes", "--repo", str(repo), "--json")
        self.assertEqual(esc.returncode, 0, esc.stderr)
        data = json.loads(esc.stdout)
        self.assertEqual(data["mergedRuns"], 2)
        self.assertEqual(data["escapedRuns"], 1)
        self.assertEqual(data["unknownRuns"], 0)
        self.assertEqual(data["escapes"][0]["run"], run1)

    def test_help_escapes(self):
        proc = self.cli("escapes", "--help")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("Fixes-Run:", proc.stdout)


if __name__ == "__main__":
    unittest.main()
