import json
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateWorkAndTui1126Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    start_gated = fixtures.DelegateTests.start_gated
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    until = fixtures.DelegateTests.until
    wait_until = fixtures.DelegateTests.wait_until
    wait_text = fixtures.DelegateTests.wait_text
    repo = fixtures.DelegateTests.repo

    def make_named_repo(self, name, files):
        path = self.work / name
        path.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "init", "-q", str(path)], check=True)
        subprocess.run(["git", "-C", str(path), "config", "user.email", "t@example.com"], check=True)
        subprocess.run(["git", "-C", str(path), "config", "user.name", "T"], check=True)
        for rel, content in files.items():
            fp = path / rel
            fp.parent.mkdir(parents=True, exist_ok=True)
            fp.write_text(content)
        subprocess.run(["git", "-C", str(path), "add", "-A"], check=True)
        subprocess.run(["git", "-C", str(path), "commit", "-q", "-m", "init"], check=True)
        return path

    def test_item7_delegate_card_marks_delivery_read_and_clears_unread_pending(self):
        repo = self.make_named_repo("repo-card", {"src.txt": "base\n"})
        self.fake_pi([fixtures.answer("all good"), fixtures.SETTLED])
        self.env["DELEGATE_CALLER"] = "caller-card-read"

        # Start background read-only run and wait for it to finish WITHOUT reading it.
        started = self.outcome(
            self.cli("start", "--workdir", repo, "--read-only", "--name", "card-check", "inspect", cwd=repo)
        )
        run_id = started["run"]
        run_dir = Path(started["dir"])
        self.until(
            lambda: (run_dir / "summary.json").is_file() and (run_dir / "exit_code").exists(),
            "card-check run finished",
        )
        meta_path = run_dir / "meta.json"

        # Before reading card: `hook pending` reports `[未读交付]` pointing to `delegate card <run>`.
        pend_before = self.cli("hook", "pending", "--cwd", str(repo), human=True)
        self.assertEqual(pend_before.returncode, 0, pend_before.stderr)
        self.assertIn("未读交付", pend_before.stdout)
        self.assertIn(f"delegate card {run_id}", pend_before.stdout)

        # `delegate card --no-mark` leaves it unread.
        no_mark = self.cli("card", "--no-mark", run_id, cwd=repo, human=True)
        self.assertEqual(no_mark.returncode, 0, no_mark.stderr)
        self.assertFalse(json.loads(meta_path.read_text()).get("resultReadAt"))

        # Following the affordance (`delegate card <run>`) marks it read and clears `[未读交付]`.
        card_out = self.cli("card", run_id, cwd=repo, human=True)
        self.assertEqual(card_out.returncode, 0, card_out.stderr)
        self.assertTrue(
            json.loads(meta_path.read_text()).get("resultReadAt"),
            "delegate card must record resultReadAt",
        )
        pend_after = self.cli("hook", "pending", "--cwd", str(repo), human=True)
        self.assertEqual(pend_after.returncode, 0, pend_after.stderr)
        self.assertNotIn(run_id, pend_after.stdout)

    def test_item6_cli_work_and_snapshot_share_unified_counts_ephemeral_and_eta(self):
        repo = self.repo({"a.txt": "hello\n"})
        self.env["DELEGATE_CALLER"] = "caller-unified"

        # Add 1 ready item + 1 deferred item (far future).
        w_ready = json.loads(
            self.cli("work", "add", "就绪测试项", "--repo", str(repo), "--class", "feat", "--json", cwd=repo).stdout
        )
        w_def = json.loads(
            self.cli("work", "add", "远期暂缓项", "--repo", str(repo), "--class", "feat", "--json", cwd=repo).stdout
        )
        self.assertEqual(
            self.cli(
                "work", "defer", w_def["id"], "--until", "2099-12-01T00:00:00Z", "--reason", "等远期", cwd=repo
            ).returncode,
            0,
        )

        # Start 1 unattached active run (shows up as `R:...` in BOTH CLI and snapshot!).
        state, gate = self.start_gated(
            "--workdir", repo, "--read-only", "--name", "eph-live", "inspect",
            attempts=([fixtures.answer("done"), fixtures.SETTLED],),
        )
        self.addCleanup(lambda: (gate.touch(), self.cli("wait", "--no-result", state["run"], cwd=repo)))

        work_json_res = self.cli("work", "--all", "--json", cwd=repo)
        self.assertEqual(work_json_res.returncode, 0, work_json_res.stderr)
        work_lines = [json.loads(ln) for ln in work_json_res.stdout.splitlines() if ln.strip()]
        work_summary = next(o["workSummary"] for o in work_lines if "workSummary" in o)

        snap_res = self.cli("snapshot", "--json", "--repo", str(repo), cwd=repo)
        self.assertEqual(snap_res.returncode, 0, snap_res.stderr)
        snap = json.loads(snap_res.stdout)

        snap_open = [w for w in snap["work"] if w.get("state") != "done"]
        snap_counts = {}
        for w in snap_open:
            st = w["state"]
            snap_counts[st] = snap_counts.get(st, 0) + 1

        # Exact 1-to-1 match between CLI workSummary and TUI snapshot work items!
        for st in ("attention", "ready", "verify", "active", "blocked", "deferred"):
            self.assertEqual(
                work_summary[st],
                snap_counts.get(st, 0),
                f"mismatch for state={st}: cli={work_summary} snap={snap_counts}",
            )
        self.assertEqual(work_summary["active"], 1)
        self.assertTrue(
            any(w.get("ephemeral") and str(w.get("id", "")).startswith("R:") for w in snap_open),
            f"snapshot missing ephemeral R: row: {snap_open}",
        )
        # Exact match on DAG ETA summary (excluding deferred item!).
        cli_eta = work_summary["eta"]
        snap_eta = snap["eta"]["work"]
        self.assertEqual(cli_eta["writeChains"], snap_eta["writeChains"])
        self.assertEqual(cli_eta["writeChains"], 1)
        self.assertEqual(cli_eta["deferredCount"], 1)
        self.assertEqual(snap_eta["deferredCount"], 1)
        self.assertEqual(cli_eta["devWallRange"], snap_eta["devWallRange"])
        self.assertEqual(cli_eta.get("earliestFinishP50Clock"), snap_eta.get("earliestFinishP50Clock"))

        # Wide terminal COLUMNS=180 keeps long titles untruncated and formats local defer time.
        old_cols = self.env.get("COLUMNS")
        self.env["COLUMNS"] = "180"
        try:
            wide_out = self.cli("work", "--all", cwd=repo, human=True)
        finally:
            if old_cols is None:
                self.env.pop("COLUMNS", None)
            else:
                self.env["COLUMNS"] = old_cols
        self.assertEqual(wide_out.returncode, 0, wide_out.stderr)
        self.assertNotIn("2099-12-01T", wide_out.stdout, "raw UTC ISO leaked in CLI work table")
        self.assertIn("另有 1 项暂缓", wide_out.stdout)
        self.assertIn(w_ready["id"], wide_out.stdout)

    def test_item15_wait_waits_across_repos_by_default_and_supports_current_repo_flag(self):
        self.env.pop("DELEGATE_RUNS", None)  # store runs under each repository separately
        repo_a = self.make_named_repo("repo-a", {"a.txt": "a\n"})
        repo_b = self.make_named_repo("repo-b", {"b.txt": "b\n"})
        self.env["DELEGATE_CALLER"] = "sess-cross-repo"

        # Launch a run in repo_b, then call `delegate wait` from repo_a!
        state_b, gate_b = self.start_gated(
            "--workdir", repo_b, "--read-only", "--name", "cross-repo-job", "check b",
            attempts=([fixtures.answer("cross repo answer"), fixtures.SETTLED],),
            cwd=repo_b,
        )
        self.addCleanup(lambda: (gate_b.touch(), self.cli("stop", state_b["run"], cwd=repo_b)))

        # With `--current-repo` from repo_a: must NOT wait for repo_b's run.
        local_only = self.cli("wait", "--current-repo", "--max", ".2", cwd=repo_a, human=True)
        self.assertEqual(local_only.returncode, 0, local_only.stdout + local_only.stderr)
        self.assertIn("本会话没有在跑的任务", local_only.stdout)

        # Default `delegate wait` from repo_a MUST see repo_b's running job (exit 75 on short --max,
        # and report remaining count 1 instead of falsely claiming no tasks running!).
        cross_running = self.cli("wait", "--max", ".2", cwd=repo_a, human=True)
        self.assertEqual(cross_running.returncode, 75, cross_running.stdout + cross_running.stderr)
        self.assertIn("本会话还剩 1 个任务在跑", cross_running.stdout)

        # Release repo_b's run -> default `delegate wait` from repo_a collects its delivery!
        gate_b.touch()
        collected = self.cli("wait", cwd=repo_a, human=True)
        self.assertEqual(collected.returncode, 0, collected.stdout + collected.stderr)
        self.assertIn("cross repo answer", collected.stdout)
        self.assertIn("本会话没有在跑的任务", collected.stdout)

    def test_item6_zero_change_write_delivery_not_counted_as_wave_controller_merge_blocker(self):
        repo = self.make_named_repo("repo-zero-change", {"src.txt": "base\n"})
        self.env["DELEGATE_CALLER"] = "caller-zero-change"

        # 1a. Real worktree write run with ZERO file changes (finishes as `answered`, changes.files == 0).
        self.fake_pi([fixtures.answer("no edits needed in worktree"), fixtures.SETTLED])
        started_wt_zero = self.outcome(
            self.cli(
                "start", "--workdir", repo, "--worktree", "--name", "zero-wt-write",
                "check and edit if needed", cwd=repo,
            )
        )
        wt_zero_id = started_wt_zero["run"]
        wt_zero_dir = Path(started_wt_zero["dir"])
        self.until(
            lambda: (wt_zero_dir / "summary.json").is_file() and (wt_zero_dir / "exit_code").exists(),
            "zero-wt-write run finished",
        )

        # 1b. Delivered write run (`state == "delivered"`) with ZERO net changes (`pendingChanges.files == 0`),
        #     matching the exact case where a rework round reverts to zero net diff.
        self.fake_pi([fixtures.answer("reverted to zero net diff"), fixtures.SETTLED])
        started_deliv_zero = self.outcome(
            self.cli("start", "--workdir", repo, "--worktree", "--name", "zero-delivered-write", "check", cwd=repo)
        )
        deliv_zero_id = started_deliv_zero["run"]
        deliv_zero_dir = Path(started_deliv_zero["dir"])
        self.until(
            lambda: (deliv_zero_dir / "summary.json").is_file() and (deliv_zero_dir / "exit_code").exists(),
            "zero-delivered-write run finished",
        )
        sum_path = deliv_zero_dir / "summary.json"
        sum_doc = json.loads(sum_path.read_text())
        sum_doc["state"] = "delivered"
        sum_doc["changes"] = {"files": 0, "added": 0, "deleted": 0}
        sum_doc["pendingChanges"] = {"files": 0, "added": 0, "deleted": 0}
        sum_path.write_text(json.dumps(sum_doc))

        # Even while unread, neither zero-change write delivery may count as a ControllerMerge wave blocker.
        snap_unread = json.loads(self.cli("snapshot", "--json", "--repo", str(repo), cwd=repo).stdout)
        wave_unread = snap_unread["eta"]["wave"]
        self.assertEqual(wave_unread["blockedCount"], 0, f"unread zero-change miscounted: {wave_unread}")
        self.assertEqual(wave_unread["blockers"], [], f"unread zero-change blocker leaked: {wave_unread}")

        # After reading both via `delegate card`: `hook pending` and `eta.wave` agree on 0 blockers/pending.
        self.assertEqual(self.cli("card", wt_zero_id, cwd=repo, human=True).returncode, 0)
        self.assertEqual(self.cli("card", deliv_zero_id, cwd=repo, human=True).returncode, 0)
        pend_after_read = json.loads(self.cli("hook", "pending", "--json", "--cwd", str(repo)).stdout)
        self.assertEqual(pend_after_read.get("items", []), [], f"expected 0 pending after card: {pend_after_read}")
        snap_read = json.loads(self.cli("snapshot", "--json", "--repo", str(repo), cwd=repo).stdout)
        wave_read = snap_read["eta"]["wave"]
        self.assertEqual(wave_read["blockedCount"], 0, f"read zero-change miscounted: {wave_read}")
        self.assertEqual(wave_read["blockers"], [], f"read zero-change blocker leaked: {wave_read}")

        # 2. Control check: a real worktree write delivery WITH file changes IS counted in both `hook pending` and `eta.wave`.
        self.fake_pi([fixtures.answer("edited src.txt"), fixtures.SETTLED], pre="printf 'updated\\n' >> src.txt")
        started_real = self.outcome(
            self.cli("start", "--workdir", repo, "--worktree", "--name", "real-write", "edit src.txt", cwd=repo)
        )
        real_dir = Path(started_real["dir"])
        self.until(
            lambda: (real_dir / "summary.json").is_file() and (real_dir / "exit_code").exists(),
            "real-write run finished",
        )

        pend_real = json.loads(self.cli("hook", "pending", "--json", "--cwd", str(repo)).stdout)
        self.assertEqual(len(pend_real.get("items", [])), 1, f"expected 1 pending for real write: {pend_real}")
        snap_real = json.loads(self.cli("snapshot", "--json", "--repo", str(repo), cwd=repo).stdout)
        wave_real = snap_real["eta"]["wave"]
        self.assertEqual(wave_real["blockedCount"], 1, f"expected 1 wave blocker for real write: {wave_real}")
        self.assertEqual(wave_real["blockers"][0]["compact"], "卡主控")


if __name__ == "__main__":
    unittest.main()
