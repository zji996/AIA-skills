"""Auto-GC worktree reclamation & `.truncated` unblocking tests (ergonomics P0-3):
1. `.truncated` no longer blocks `apply` auto-clean or `_gc-worker`; full `result.md` and
   `result-original.md` are preserved in archive and readable via `delegate result`.
2. `_gc-worker` immediately archives & reclaims worktrees of applied runs (including `--keep`
   and manually landed changes) and `work drop` runs, while `--in` can still reconstruct from
   archive.
3. Unmerged finished runs have NO fixed TTL or static GiB cap (P9): under normal disk space
   even very old unmerged finished runs stay intact; only when whole-disk watermark triggers
   does `_gc-worker` archive & evict least-recently-touched non-busy worktrees until target is
   met, never touching active runs or worktrees referenced by a live process (`/proc` cwd).
"""
import fcntl
import json
import os
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateAutoGcTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    start_gated = fixtures.DelegateTests.start_gated
    until = fixtures.DelegateTests.until
    wait_until = fixtures.DelegateTests.wait_until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def cache_dir(self) -> Path:
        return Path(self.env["XDG_CACHE_HOME"]) / "delegate"

    def run_gc_worker_sync(
        self, repo: Path, extra_env: dict[str, str] | None = None, drain_bg: bool = False
    ) -> None:
        lock_path = self.state_dir() / ".gc-worker.lock"
        lock_path.parent.mkdir(parents=True, exist_ok=True)
        with open(lock_path, "a+") as f:
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
            fcntl.flock(f.fileno(), fcntl.LOCK_UN)
        saved: dict[str, str | None] = {}
        if extra_env:
            for k, v in extra_env.items():
                saved[k] = self.env.get(k)
                self.env[k] = v
        try:
            res = self.cli("_gc-worker", cwd=repo)
            self.assertEqual(res.returncode, 0, res.stderr)
            with open(lock_path, "a+") as f:
                fcntl.flock(f.fileno(), fcntl.LOCK_EX)
                fcntl.flock(f.fileno(), fcntl.LOCK_UN)
            if drain_bg:
                res2 = self.cli("_gc-worker", cwd=repo)
                self.assertEqual(res2.returncode, 0, res2.stderr)
        finally:
            for k, v in saved.items():
                if v is None:
                    self.env.pop(k, None)
                else:
                    self.env[k] = v

    def write_user_disk_config(self, disk_cfg: dict) -> None:
        cfg_path = Path(self.env["XDG_CONFIG_HOME"]) / "delegate" / "config.json"
        doc = json.loads(cfg_path.read_text()) if cfg_path.is_file() else {}
        doc["disk"] = disk_cfg
        cfg_path.parent.mkdir(parents=True, exist_ok=True)
        cfg_path.write_text(json.dumps(doc))

    def test_truncated_does_not_block_apply_auto_clean_and_preserves_original_result(self):
        repo = self.repo({"README.md": "init\n"})
        long_answer = "LONG-ANSWER-" + ("z" * 120)
        self.fake_pi([fixtures.answer(long_answer), fixtures.SETTLED], pre="echo ok > trunc.txt")
        self.env["DELEGATE_RESULT_CHARS"] = "20"

        started = json.loads(
            self.cli("start", "--worktree", "--no-scout", "--name", "trunc-write", "edit trunc.txt", cwd=repo).stdout
        )
        waited = self.cli("wait", started["run"], cwd=repo)
        self.assertEqual(waited.returncode, 0, waited.stderr)
        self.assertIn("省略", waited.stdout)

        run_dir = Path(started["dir"])
        wt_dir = Path(started["worktree"])
        self.assertTrue((run_dir / ".truncated").exists(), "wait should mark .truncated")
        # Also create result-original.md (simulating compressed long answer) to verify archive retention.
        (run_dir / "result-original.md").write_text(long_answer + "\n")

        applied = self.cli("apply", "--commit", started["run"], cwd=repo)
        self.assertEqual(applied.returncode, 0, applied.stderr)
        self.run_gc_worker_sync(repo)

        self.assertFalse(wt_dir.exists(), ".truncated must not block worktree cleanup after apply")
        self.assertFalse(run_dir.exists(), ".truncated must not block full auto-clean after apply --commit")

        arch_record = self.cache_dir() / "archive" / started["run"] / "record"
        self.assertTrue((arch_record / "result.md").is_file())
        self.assertTrue((arch_record / "result-original.md").is_file())
        self.assertEqual((arch_record / "result-original.md").read_text().strip(), long_answer)
        self.assertEqual(self.cli("result", started["run"], cwd=repo).stdout.strip(), long_answer)

    def test_gc_worker_immediately_reclaims_applied_and_dropped_worktrees(self):
        repo = self.repo({"README.md": "init\n"})
        # 1. Applied run with --keep -> worktree stays until _gc-worker runs, then reclaimed immediately.
        self.fake_pi([fixtures.answer("done keep"), fixtures.SETTLED], pre="echo a > kept.txt")
        st_applied = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--name", "kept-apply", "write kept.txt", cwd=repo)
        )
        wt_applied = Path(st_applied["worktree"])
        self.assertTrue(wt_applied.is_dir())
        ap = self.cli("apply", "--keep", st_applied["run"], cwd=repo)
        self.assertEqual(ap.returncode, 0, ap.stderr)
        self.assertTrue(wt_applied.is_dir(), "apply --keep leaves worktree initially")

        # 2. Dropped work item's run -> _gc-worker immediately archives & removes run + worktree,
        #    while --in can still reconstruct from archive.
        add_res = self.cli("work", "add", "drop me", "--class", "feat", "--json", cwd=repo)
        self.assertEqual(add_res.returncode, 0, add_res.stderr)
        work_id = json.loads(add_res.stdout)["id"]

        self.fake_pi([fixtures.answer("draft for drop"), fixtures.SETTLED], pre="echo drop > drop.txt")
        st_drop = self.outcome(
            self.cli(
                "run",
                "--worktree",
                "--no-scout",
                "--work",
                work_id,
                "--name",
                "dropped-run",
                "write drop.txt",
                cwd=repo,
            )
        )
        run_drop = Path(st_drop["dir"])
        wt_drop = Path(st_drop["worktree"])
        self.assertTrue(wt_drop.is_dir())

        drop_res = self.cli("work", "drop", work_id, "--reason", "direction changed", cwd=repo)
        self.assertEqual(drop_res.returncode, 0, drop_res.stderr)

        self.run_gc_worker_sync(repo, drain_bg=True)

        self.assertFalse(wt_applied.exists(), "_gc-worker must immediately reclaim applied run worktree")
        self.assertFalse(wt_drop.exists(), "_gc-worker must immediately reclaim dropped run worktree")
        self.assertFalse(run_drop.exists(), "_gc-worker must clean dropped run after archiving")

        arch_drop = self.cache_dir() / "archive" / st_drop["run"] / "record"
        self.assertTrue((arch_drop / "result.md").is_file())
        self.assertTrue((arch_drop / "changes.patch").is_file())

        # Verify `--in` can reconstruct the dropped & archived run's snapshot.
        self.fake_pi([fixtures.answer("inspected"), fixtures.SETTLED], pre="test -f drop.txt")
        insp = self.outcome(
            self.cli("run", "--read-only", "--in", st_drop["run"], "inspect drop.txt", cwd=repo)
        )
        self.assertEqual(insp["state"], "answered")

    def test_unmerged_finished_runs_reclaimed_only_on_disk_watermark_lru_respecting_busy_and_active(self):
        repo = self.repo({"README.md": "init\n"})
        # Disable minFreeGib so primary_bytes_to_free is governed cleanly by highPct/targetPct.
        self.write_user_disk_config(
            {"highPct": 85, "targetPct": 75, "minFreeGib": 0}
        )

        # Create 3 unmerged finished write runs: busy_old (oldest), evict_old (2nd oldest), newer (newest).
        created = {}
        for name, fname in [("busy-old", "b.txt"), ("evict-old", "e.txt"), ("newer-one", "n.txt")]:
            self.fake_pi([fixtures.answer(f"wrote {fname}"), fixtures.SETTLED], pre=f"echo {name} > {fname}")
            st = self.outcome(
                self.cli("run", "--worktree", "--no-scout", "--name", name, f"write {fname}", cwd=repo)
            )
            wt = Path(st["worktree"])
            # Put ~200 KiB payload in each worktree so evicting 1 worktree satisfies a 150 KiB target.
            (wt / "payload.bin").write_bytes(b"x" * 204_800)
            created[name] = (st, Path(st["dir"]), wt)

        # Set deterministic exit_code mtimes far in the past (year 2001) to prove NO fixed TTL exists.
        os.utime(created["busy-old"][1] / "exit_code", (1_000_000_000, 1_000_000_000))
        os.utime(created["evict-old"][1] / "exit_code", (1_000_000_100, 1_000_000_100))
        os.utime(created["newer-one"][1] / "exit_code", (1_000_000_200, 1_000_000_200))

        # 1. Below watermark (20% used): NONE of the unmerged finished runs are evicted despite old mtime.
        self.run_gc_worker_sync(repo, {"DELEGATE_DISK_PRIMARY_FAKE_SPACE": "1000000:800000"})
        for name, (_, run_dir, wt_dir) in created.items():
            self.assertTrue(run_dir.is_dir(), f"{name} must stay when disk is below watermark")
            self.assertTrue(wt_dir.is_dir(), f"{name} worktree must stay when disk is below watermark")

        # 2. Start an active gated run AND hold a live process with cwd inside `busy-old`'s worktree.
        active_st, gate = self.start_gated(
            "--worktree", "--no-scout", "--name", "active-keep", "active task", cwd=repo
        )
        active_run_dir = Path(active_st["dir"])
        active_wt_dir = Path(active_st["worktree"])

        holder = subprocess.Popen(
            ["python3", "-c", "import signal; signal.pause()"],
            cwd=str(created["busy-old"][2]),
        )
        try:
            # High watermark: total=1_000_000, avail=100_000 -> used=900_000 (90% >= 85%),
            # target=750_000 -> need=150_000 bytes. Evicting `evict-old` (~200 KiB) satisfies need.
            self.run_gc_worker_sync(repo, {"DELEGATE_DISK_PRIMARY_FAKE_SPACE": "1000000:100000"})

            # Active run untouched.
            self.assertTrue(active_run_dir.is_dir(), "active run must never be evicted")
            self.assertTrue(active_wt_dir.is_dir(), "active worktree must never be evicted")

            # Oldest run `busy-old` is in use by `holder` (/proc cwd) -> must NOT be evicted.
            self.assertTrue(created["busy-old"][1].is_dir(), "busy worktree must not be evicted")
            self.assertTrue(created["busy-old"][2].is_dir(), "busy worktree dir must remain")

            # Second-oldest non-busy `evict-old` -> archived and evicted!
            self.assertFalse(created["evict-old"][1].exists(), "LRU non-busy unmerged run should be evicted")
            self.assertFalse(created["evict-old"][2].exists(), "LRU non-busy unmerged worktree should be removed")
            arch_evicted = self.cache_dir() / "archive" / created["evict-old"][0]["run"] / "record"
            self.assertTrue((arch_evicted / "result.md").is_file(), "evicted run must be archived first")
            self.assertTrue((arch_evicted / "changes.patch").is_file(), "evicted run patch must be archived")

            # Newest run `newer-one` -> untouched because freeing `evict-old` already met watermark target.
            self.assertTrue(created["newer-one"][1].is_dir(), "newer unmerged run should remain once target met")
            self.assertTrue(created["newer-one"][2].is_dir(), "newer worktree should remain once target met")
        finally:
            holder.terminate()
            try:
                holder.wait(timeout=5)
            except subprocess.TimeoutExpired:
                holder.kill()
                holder.wait(timeout=5)
            gate.write_text("go\n")
            self.cli("wait", active_st["run"], cwd=repo)

    def test_watermark_lru_aggregates_shared_reply_chain_by_latest_round(self):
        repo = self.repo({"README.md": "init\n"})
        self.write_user_disk_config({"highPct": 85, "targetPct": 75, "minFreeGib": 0})

        # Reply chain r1 -> r2 sharing one worktree: r1 is oldest, r2 is newest.
        self.fake_pi([fixtures.answer("round 1"), fixtures.SETTLED], pre="echo r1 > chain.txt")
        r1 = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--name", "chain-r1", "write r1", cwd=repo)
        )
        self.fake_pi([fixtures.answer("round 2"), fixtures.SETTLED], pre="echo r2 > chain.txt")
        r2 = self.outcome(
            self.cli("reply", r1["run"], "--wait", "update to r2", cwd=repo)
        )
        self.assertEqual(r1["worktree"], r2["worktree"], "reply must share worktree")
        chain_wt = Path(r2["worktree"])
        (chain_wt / "payload.bin").write_bytes(b"c" * 204_800)

        # Standalone unmerged run `mid` whose mtime is newer than r1 but older than r2.
        self.fake_pi([fixtures.answer("mid"), fixtures.SETTLED], pre="echo mid > mid.txt")
        mid = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--name", "mid-run", "write mid", cwd=repo)
        )
        mid_wt = Path(mid["worktree"])
        (mid_wt / "payload.bin").write_bytes(b"m" * 204_800)

        os.utime(Path(r1["dir"]) / "exit_code", (1_000_000_000, 1_000_000_000))
        os.utime(Path(mid["dir"]) / "exit_code", (1_000_000_200, 1_000_000_200))
        os.utime(Path(r2["dir"]) / "exit_code", (1_000_000_500, 1_000_000_500))

        # High watermark needs 150 KiB: `mid` (effective mtime 200) must be evicted BEFORE
        # the shared chain `r1->r2` (effective mtime 500), leaving r1, r2 and chain_wt intact.
        self.run_gc_worker_sync(repo, {"DELEGATE_DISK_PRIMARY_FAKE_SPACE": "1000000:100000"})

        self.assertFalse(Path(mid["dir"]).exists(), "older standalone run should be evicted first")
        self.assertFalse(mid_wt.exists(), "older standalone worktree should be deleted")
        self.assertTrue(Path(r1["dir"]).is_dir(), "r1 must not be evicted ahead of older standalone run")
        self.assertTrue(Path(r2["dir"]).is_dir(), "newer reply r2 must stay intact")
        self.assertTrue(chain_wt.is_dir(), "shared worktree must not be deleted while newer r2 survives")

    def test_watermark_pool_slot_does_not_overcredit_freed_bytes(self):
        repo = self.repo({"README.md": "init\n"})
        self.write_user_disk_config({"highPct": 85, "targetPct": 75, "minFreeGib": 0})

        # 1. Create an older unmerged run inside a pooled slot with 250 KiB payload.
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"pool": 1}}))
        self.fake_pi([fixtures.answer("pooled"), fixtures.SETTLED], pre="echo p > p.txt")
        pooled = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--name", "pooled-old", "write p", cwd=repo)
        )
        pooled_wt = Path(pooled["worktree"])
        (pooled_wt / "retained.bin").write_bytes(b"p" * 256_000)

        # 2. Create a newer non-pooled unmerged run with 200 KiB payload.
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"pool": 0}}))
        self.fake_pi([fixtures.answer("nonpool"), fixtures.SETTLED], pre="echo np > np.txt")
        nonpool = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--name", "nonpool-new", "write np", cwd=repo)
        )
        nonpool_wt = Path(nonpool["worktree"])
        (nonpool_wt / "payload.bin").write_bytes(b"n" * 204_800)

        os.utime(Path(pooled["dir"]) / "exit_code", (1_000_000_000, 1_000_000_000))
        os.utime(Path(nonpool["dir"]) / "exit_code", (1_000_000_300, 1_000_000_300))

        # High watermark needs 150 KiB. Releasing the pool slot keeps the slot directory &
        # retained.bin on disk (~0 bytes freed), so _gc-worker must NOT stop early and MUST
        # continue to evict `nonpool-new` to actually free 200 KiB.
        self.run_gc_worker_sync(repo, {"DELEGATE_DISK_PRIMARY_FAKE_SPACE": "1000000:100000"})

        self.assertTrue(pooled_wt.is_dir(), "pool slot directory itself is retained for reuse")
        self.assertFalse(Path(nonpool["dir"]).exists(), "non-pooled run must still be evicted to meet real freed bytes")
        self.assertFalse(nonpool_wt.exists(), "non-pooled worktree must be deleted when pool release freed ~0 bytes")

    def test_manual_head_commit_of_reply_chain_reclaims_worktree_cross_repo(self):
        repo = self.repo({"a.txt": "init\n"})
        other_cwd = self.work / "other-cwd"
        other_cwd.mkdir(parents=True, exist_ok=True)

        # Reply chain r1 -> r2 sharing one worktree, where r2 overwrites r1's edit.
        self.fake_pi([fixtures.answer("v1"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r1 = self.outcome(
            self.cli("run", "--worktree", "--no-scout", "--name", "manual-r1", "set v1", cwd=repo)
        )
        self.fake_pi([fixtures.answer("v2"), fixtures.SETTLED], pre="echo v2 > a.txt")
        r2 = self.outcome(
            self.cli("reply", r1["run"], "--wait", "set v2", cwd=repo)
        )
        wt = Path(r2["worktree"])
        self.assertTrue(wt.is_dir())

        # Controller manually lands r2's final content into source HEAD.
        (repo / "a.txt").write_text("v2\n")
        subprocess.run(["git", "-C", str(repo), "add", "a.txt"], check=True)
        subprocess.run(["git", "-C", str(repo), "commit", "-q", "-m", "manual land v2"], check=True)

        # Run `_gc-worker` from a completely different working directory (cross-repo).
        self.run_gc_worker_sync(other_cwd)

        self.assertFalse(wt.exists(), "reply chain worktree must be reclaimed after manual HEAD commit")
        self.assertFalse(Path(r1["dir"]).exists(), "superseded predecessor r1 must be cleaned")
        self.assertFalse(Path(r2["dir"]).exists(), "committed tail r2 must be cleaned")
        self.assertTrue((self.cache_dir() / "archive" / r2["run"] / "record" / "result.md").is_file())


if __name__ == "__main__":
    unittest.main()
