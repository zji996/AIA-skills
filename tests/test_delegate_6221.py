"""6.22.1 regressions: pin run Git objects under refs/delegate/<run-id>/*,
clean removes refs (cleanOnApply keeps them), upgrade backfills legacy runs,
and missing objects show clear status/diff/apply recovery commands."""
import json
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate6221Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def git(self, repo, *args):
        return subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True, text=True, check=True,
        ).stdout.strip()

    def run_refs(self, repo, run_id):
        out = self.git(repo, "for-each-ref", "--format=%(refname)", f"refs/delegate/{run_id}/")
        return sorted(line for line in out.splitlines() if line)

    def test_refs_survive_git_prune_and_clean_on_apply_until_clean(self):
        repo = self.repo({"a.txt": "committed\n"})
        # Dirty uncommitted change in source so base snapshot tree differs from HEAD.
        (repo / "a.txt").write_text("dirty-base\n")
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre="echo delivered > a.txt; echo extra > added.txt",
        )
        run = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "pin-test", "edit"))
        run_id = run["run"]
        wt = Path(run["worktree"])
        self.assertTrue(wt.is_dir())

        refs = self.run_refs(repo, run_id)
        self.assertIn(f"refs/delegate/{run_id}/base", refs)
        self.assertIn(f"refs/delegate/{run_id}/worktree-base", refs)
        self.assertIn(f"refs/delegate/{run_id}/after", refs)

        # Aggressive prune + gc must not remove any pinned objects.
        self.git(repo, "prune", "--expire=now")
        self.git(repo, "gc", "--prune=now")

        diff_res = self.cli("diff", run_id)
        self.assertEqual(diff_res.returncode, 0, diff_res.stderr)
        self.assertIn("+delivered", diff_res.stdout)
        self.assertIn("+extra", diff_res.stdout)

        total_diff = self.cli("diff", run_id, "--total")
        self.assertEqual(total_diff.returncode, 0, total_diff.stderr)
        self.assertIn("+delivered", total_diff.stdout)

        apply_res = self.cli("apply", "--json", run_id)
        self.assertEqual(apply_res.returncode, 0, apply_res.stdout + apply_res.stderr)
        self.assertEqual((repo / "a.txt").read_text(), "delivered\n")
        self.assertEqual((repo / "added.txt").read_text(), "extra\n")

        # Default cleanOnApply removes the worktree, but keeps the run and its refs until clean.
        self.assertFalse(wt.exists())
        refs_after_apply = self.run_refs(repo, run_id)
        self.assertIn(f"refs/delegate/{run_id}/base", refs_after_apply)
        self.assertIn(f"refs/delegate/{run_id}/after", refs_after_apply)
        self.assertIn(f"refs/delegate/{run_id}/applied", refs_after_apply)

        # Explicit clean deletes the run and all of its refs.
        cleaned = self.cli("clean", run_id)
        self.assertEqual(cleaned.returncode, 0, cleaned.stdout + cleaned.stderr)
        self.assertEqual(self.run_refs(repo, run_id), [])

    def test_reply_sync_pins_chain_and_sync_base_across_parent_clean(self):
        repo = self.repo({"a.txt": "v0\n", "b.txt": "b0\n"})
        self.fake_pi([fixtures.answer("r1"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "chain", "first"))

        # Caller edits another file in source before reply --sync.
        (repo / "b.txt").write_text("b1\n")
        self.fake_pi([fixtures.answer("r2"), fixtures.SETTLED], pre="echo v2 > a.txt")
        r2 = self.outcome(self.cli("reply", "--wait", "--sync", r1["run"], "second"))

        self.assertIn(f"refs/delegate/{r1['run']}/sync-base", self.run_refs(repo, r1["run"]))
        r2_refs = self.run_refs(repo, r2["run"])
        self.assertIn(f"refs/delegate/{r2['run']}/base", r2_refs)
        self.assertIn(f"refs/delegate/{r2['run']}/chain-base", r2_refs)
        self.assertIn(f"refs/delegate/{r2['run']}/applied-base", r2_refs)
        self.assertIn(f"refs/delegate/{r2['run']}/after", r2_refs)

        # Cleaning r1 drops r1's refs; r2 must keep its own chain-base and applied-base through prune.
        self.assertEqual(self.cli("clean", "--force", r1["run"]).returncode, 0)
        self.assertEqual(self.run_refs(repo, r1["run"]), [])
        self.git(repo, "prune", "--expire=now")

        total_diff = self.cli("diff", r2["run"], "--total")
        self.assertEqual(total_diff.returncode, 0, total_diff.stderr)
        self.assertIn("+v2", total_diff.stdout)
        applied = self.cli("apply", "--json", r2["run"])
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertEqual((repo / "a.txt").read_text(), "v2\n")
        self.assertEqual((repo / "b.txt").read_text(), "b1\n")

        self.assertEqual(self.cli("clean", r2["run"]).returncode, 0)
        self.assertEqual(self.run_refs(repo, r2["run"]), [])

    def test_upgrade_backfills_legacy_refs_and_reports_lost_objects(self):
        repo = self.repo({"a.txt": "init\n"})
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo alive > a.txt")
        ok_run = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "legacy-ok", "ok"))

        self.fake_pi(
            [fixtures.answer("lost"), fixtures.SETTLED],
            pre="echo recovered > a.txt; echo newfile > untracked.txt",
        )
        lost_run = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "legacy-lost", "lost"))

        # Simulate pre-6.22.1 runs: remove refs and .refs-pinned marker for both runs.
        for run in (ok_run, lost_run):
            for ref in self.run_refs(repo, run["run"]):
                self.git(repo, "update-ref", "-d", ref)
            (Path(run["dir"]) / ".refs-pinned").unlink(missing_ok=True)
            self.assertEqual(self.run_refs(repo, run["run"]), [])

        # Prune ONLY lost_run's after object by pinning ok_run temporarily? No — easier:
        # delete lost_run's after loose object, or prune before creating ok_run!
        # Let's prune lost_run's unreachable `after` tree genuinely with git prune --expire=now,
        # while holding ok_run's after tree with a temporary ref that we remove right after.
        ok_after = json.loads((Path(ok_run["dir"]) / "changes.json").read_text())["after"]
        self.git(repo, "update-ref", "refs/temp/hold-ok", ok_after)
        self.git(repo, "prune", "--expire=now")
        self.git(repo, "update-ref", "-d", "refs/temp/hold-ok")

        # First 6.22.1 command (`status`) backfills refs for existing objects on legacy runs.
        status_human = self.cli("status", human=True)
        self.assertEqual(status_human.returncode, 0, status_human.stderr)

        # legacy-ok was backfilled and now survives git prune --expire=now.
        ok_refs = self.run_refs(repo, ok_run["run"])
        self.assertIn(f"refs/delegate/{ok_run['run']}/base", ok_refs)
        self.assertIn(f"refs/delegate/{ok_run['run']}/worktree-base", ok_refs)
        self.assertIn(f"refs/delegate/{ok_run['run']}/after", ok_refs)
        self.git(repo, "prune", "--expire=now")
        self.assertEqual(self.cli("diff", ok_run["run"]).returncode, 0)
        self.assertEqual(self.cli("apply", "--json", ok_run["run"]).returncode, 0)
        self.assertEqual((repo / "a.txt").read_text(), "alive\n")

        # legacy-lost has lost its `after` tree: status & diff & apply report clearly with recovery command.
        wt = lost_run["worktree"]
        self.assertIn("快照对象已丢失", status_human.stdout)
        self.assertIn(f"git -C {wt} diff HEAD", status_human.stdout)
        self.assertIn("add -N", status_human.stdout)

        status_json = self.outcome(self.cli("status", lost_run["run"]))
        self.assertTrue(any(item["ref"] == "after" for item in status_json["missingObjects"]))
        self.assertIn(f"git -C {wt} diff HEAD", status_json["recovery"])
        self.assertIn("add -N", status_json["recovery"])

        diff_lost = self.cli("diff", lost_run["run"])
        self.assertNotEqual(diff_lost.returncode, 0)
        self.assertIn("快照对象已丢失", diff_lost.stderr)
        self.assertIn(f"git -C {wt} diff HEAD", diff_lost.stderr)
        self.assertIn("add -N", diff_lost.stderr)

        # Following the printed recovery command actually recovers both tracked and untracked changes.
        self.git(wt, "add", "-N", ".")
        recovered = self.git(wt, "diff", "HEAD")
        self.assertIn("+recovered", recovered)
        self.assertIn("+newfile", recovered)

        # When base snapshot itself is lost (e.g. dirty base pruned after HEAD moved), apply also gives recovery hint.
        (repo / "dirty.txt").write_text("dirty-base\n")
        self.fake_pi([fixtures.answer("lost-base"), fixtures.SETTLED], pre="echo changed > dirty.txt")
        lost_base = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "lost-base", "edit"))
        lb_wt = lost_base["worktree"]
        for ref in self.run_refs(repo, lost_base["run"]):
            self.git(repo, "update-ref", "-d", ref)
        (Path(lost_base["dir"]) / ".refs-pinned").unlink(missing_ok=True)
        # Detach worktree HEAD to source HEAD and expire its reflog so the dirty base commit/tree become unreachable.
        src_head = self.git(repo, "rev-parse", "HEAD")
        self.git(lb_wt, "reset", "--soft", src_head)
        self.git(lb_wt, "reflog", "expire", "--expire-unreachable=now", "HEAD")
        self.git(repo, "prune", "--expire=now")

        apply_lost = self.cli("apply", "--json", lost_base["run"])
        self.assertNotEqual(apply_lost.returncode, 0)
        self.assertIn("快照对象已丢失", apply_lost.stderr)
        self.assertIn(f"git -C {lb_wt} diff HEAD", apply_lost.stderr)
        self.assertIn("add -N", apply_lost.stderr)


if __name__ == "__main__":
    unittest.main()
