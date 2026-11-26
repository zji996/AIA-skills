"""End-to-end tests for delegate disk health, overflow, slot-leak fixes, and visibility:
1. Reply chain `apply` writing `.applied` releases pool slot immediately (even with
   `cleanOnApply: false` or `--keep`), and keeps slot held when an unapplied sharer remains.
2. `record_worktree_bytes` follows `target` symlink when `disk.primaryRoot` is configured.
3. `delegate disk prune [--dry-run] [--json]` + `delegate disk status` with simulated
   two-disk watermarks via user `config.json` `disk` section.
4. `status`, `snapshot --json`, and `hook check` surface primary/overflow watermarks,
   overflow slot count, and >24h & >10 GiB stale-slot reminders without deleting runs.
"""
import json
import os
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateDiskHealthTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def run_write(self, repo, pre, name):
        self.fake_pi([fixtures.answer(f"done {name}"), fixtures.SETTLED], pre=pre)
        return self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", name, name))

    def user_config_path(self) -> Path:
        return Path(self.env["XDG_CONFIG_HOME"]) / "delegate" / "config.json"

    def write_user_config(self, cfg: dict) -> None:
        p = self.user_config_path()
        p.parent.mkdir(parents=True, exist_ok=True)
        existing = json.loads(p.read_text()) if p.is_file() else {}
        existing.update(cfg)
        p.write_text(json.dumps(existing))

    def test_apply_releases_pool_slot_on_reply_chain_even_with_clean_on_apply_false(self):
        repo = self.repo({
            "a.txt": "v0\n",
            ".delegate.json": json.dumps({"worktree": {"pool": 1, "cleanOnApply": False}}),
        })
        r1 = self.run_write(repo, "echo v1 > a.txt", "round1")
        slot = Path(r1["worktree"])
        self.assertEqual(slot.parent.name, "pool")
        owner_file = slot.with_name(f"{slot.name}.owner")
        self.assertTrue(owner_file.is_file(), "root run should own pool slot")

        self.fake_pi([fixtures.answer("done r2"), fixtures.SETTLED], pre="echo v2 > a.txt")
        r2 = self.outcome(self.cli("reply", "--wait", "--no-sync", r1["run"], "round2"))
        self.assertEqual(Path(r2["worktree"]), slot)
        # Owner file still points to ancestor r1 before apply.
        self.assertEqual(Path(owner_file.read_text().strip()), Path(r1["dir"]))

        # Apply r2 (cleanOnApply=False keeps run records, yet slot MUST be released immediately).
        res = self.cli("apply", r2["run"])
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertTrue((Path(r2["dir"]) / ".applied").is_file())
        self.assertFalse(owner_file.exists(), "slot owner must be released on apply for reply chain")

        # Next write task must immediately reuse the released slot!
        r3 = self.run_write(repo, "echo v3 > a.txt", "round3")
        self.assertEqual(Path(r3["worktree"]), slot, "new write run must reuse released pool slot")

    def test_record_worktree_bytes_dereferences_target_symlink(self):
        primary_root = self.work / "primary-disk"
        self.write_user_config({
            "disk": {
                "primaryRoot": str(primary_root),
                "minFreeGib": 0,
            }
        })
        repo = self.repo({
            ".gitignore": "target/\n",
            "a.txt": "v0\n",
            ".delegate.json": json.dumps({"worktree": {"pool": 1}}),
        })
        # Write 512 KiB into target/ during the task so record_worktree_bytes measures it.
        cmd = (
            "echo v1 > a.txt && mkdir -p target/debug/deps && "
            "dd if=/dev/zero of=target/debug/deps/libbig-1111111111111111.rlib bs=1024 count=512 2>/dev/null"
        )
        r1 = self.run_write(repo, cmd, "symlink-bytes")
        slot = Path(r1["worktree"])
        self.assertTrue((slot / "target").is_symlink())
        self.assertEqual(os.readlink(slot / "target"), str(primary_root / slot.name / "target"))

        meta = json.loads((Path(r1["dir"]) / "meta.json").read_text())
        self.assertGreaterEqual(meta.get("targetBytes", 0), 512 * 1024)
        self.assertGreaterEqual(meta.get("worktreeBytes", 0), 512 * 1024)

    def test_disk_prune_cli_and_visibility_in_status_snapshot_and_stop_hook(self):
        primary_root = self.work / "primary-disk"
        overflow_root = self.work / "overflow-disk"
        extra_target = self.work / "main-repo" / "target"
        primary_root.mkdir(parents=True, exist_ok=True)
        overflow_root.mkdir(parents=True, exist_ok=True)
        (extra_target / "debug" / "deps").mkdir(parents=True, exist_ok=True)

        stale_lib = extra_target / "debug" / "deps" / "libstale-1111111111111111.rlib"
        stale_lib.write_bytes(b"x" * (256 * 1024))
        old_ts = time.time() - 7200
        os.utime(stale_lib, (old_ts, old_ts))

        self.write_user_config({
            "disk": {
                "primaryRoot": str(primary_root),
                "overflowRoot": str(overflow_root),
                "extraTargets": [str(extra_target)],
                "highPct": 85,
                "targetPct": 70,
                "minFreeGib": 0,
                "overflowHighPct": 90,
                "overflowTargetPct": 80,
                "overflowMinFreeGib": 0,
                "overflowTargetFreeGib": 0,
                "staleRunHours": 24,
                "staleRunGib": 10,
            }
        })

        # Create an overflowed slot and a stale >24h >10GiB run occupying a slot.
        pool_dir = Path(self.env["XDG_CACHE_HOME"]) / "delegate" / "pool"
        ov_slot = pool_dir / "demo-2"
        ov_slot.mkdir(parents=True, exist_ok=True)
        ov_target = overflow_root / "demo-2" / "target"
        ov_target.mkdir(parents=True, exist_ok=True)
        os.symlink(ov_target, ov_slot / "target")

        stale_slot = pool_dir / "demo-1"
        stale_slot.mkdir(parents=True, exist_ok=True)
        state_dir = Path(self.env["XDG_STATE_HOME"]) / "delegate"
        stale_run = state_dir / "runs" / "20261124-010000-stale-big"
        stale_run.mkdir(parents=True, exist_ok=True)
        old_epoch = int(time.time()) - 28 * 3600
        (stale_run / "meta.json").write_text(json.dumps({
            "run": "20261124-010000-stale-big",
            "name": "stale-big",
            "mode": "write",
            "startedEpoch": old_epoch,
            "startedNs": old_epoch * 1_000_000_000,
            "worktree": {"path": str(stale_slot), "pooled": True},
            "worktreeBytes": 12 * (1024 ** 3),
        }))
        sum_p = stale_run / "summary.json"
        sum_p.write_text(json.dumps({
            "finishedAt": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(old_epoch))
        }))
        os.utime(sum_p, (old_epoch, old_epoch))

        # Fake watermarks: primary 90% used (1 MiB total, 100 KiB avail), overflow 40% used.
        self.env["DELEGATE_DISK_PRIMARY_FAKE_SPACE"] = f"{1024 * 1024}:{100 * 1024}"
        self.env["DELEGATE_DISK_OVERFLOW_FAKE_SPACE"] = f"{1024 * 1024}:{600 * 1024}"

        # 1. Dry-run prune leaves file untouched and does not write disk-health.json.
        dry = self.cli("disk", "prune", "--dry-run", "--json")
        self.assertEqual(dry.returncode, 0, dry.stderr)
        dry_doc = json.loads(dry.stdout)
        self.assertTrue(dry_doc["dryRun"])
        self.assertGreaterEqual(dry_doc["deleted"]["lruBytes"], 200 * 1024)
        self.assertTrue(stale_lib.exists())
        self.assertFalse((state_dir / "disk-health.json").exists())

        # 2. Real prune deletes stale_lib and writes disk-health.json.
        real = self.cli("disk", "prune", "--json")
        self.assertEqual(real.returncode, 0, real.stderr)
        real_doc = json.loads(real.stdout)
        self.assertFalse(real_doc["dryRun"])
        self.assertFalse(stale_lib.exists())
        self.assertTrue((state_dir / "disk-health.json").is_file())
        self.assertEqual(real_doc["overflowSlotCount"], 1)
        self.assertEqual(len(real_doc["staleRuns"]), 1)
        self.assertTrue(stale_run.is_dir(), "stale run must NEVER be auto-deleted")

        # 3. `delegate status` (human & --json) displays watermarks, overflow slots, and stale run reminder.
        repo = self.repo({"a.txt": "x\n"})
        st_human = self.cli("status", cwd=repo, human=True)
        self.assertEqual(st_human.returncode, 0, st_human.stderr)
        self.assertIn("主盘", st_human.stdout)
        self.assertIn("溢出盘", st_human.stdout)
        self.assertIn("溢出槽 1", st_human.stdout)
        self.assertIn("stale-big", st_human.stdout)

        # 4. `delegate snapshot --json` exposes diskHealth + alerts for TUI overview.
        snap = self.cli("snapshot", "--json", cwd=repo)
        self.assertEqual(snap.returncode, 0, snap.stderr)
        snap_doc = json.loads(snap.stdout)
        self.assertEqual(snap_doc["diskHealth"]["overflowSlotCount"], 1)
        self.assertEqual(len(snap_doc["diskHealth"]["staleRuns"]), 1)
        alert_kinds = {a["kind"] for a in snap_doc["alerts"]}
        self.assertIn("disk_high", alert_kinds)
        self.assertIn("disk_stale_slot", alert_kinds)

        # 5. Stop hook (`hook check --format json` & `claude`) includes diskHealth & reminder without deleting run.
        for k in list(fixtures.CALLER_ENV) + [
            "ANTIGRAVITY_CONVERSATION_ID",
            "ANTIGRAVITY_AGENT",
            "PI_DELEGATE_ACTIVE",
            "DELEGATE_AGENT",
        ]:
            self.env.pop(k, None)
        hk_json = self.cli("hook", "check", "--format", "json", "--cwd", str(repo))
        self.assertEqual(hk_json.returncode, 0, hk_json.stderr)
        hk_doc = json.loads(hk_json.stdout)
        self.assertEqual(hk_doc["diskHealth"]["overflowSlotCount"], 1)
        self.assertIn("stale-big", hk_doc.get("diskReminder", ""))
        self.assertTrue(stale_run.is_dir(), "Stop hook must NEVER auto-delete stale run")

    def test_disk_subcommands_fail_closed_on_invalid_config(self):
        repo = self.repo({"a.txt": "x\n"})

        # 1. Unknown field in user config `disk` fails both `disk status` and `disk prune` with exit 2.
        self.write_user_config({"disk": {"bogusKey": 123}})
        for sub in ("status", "prune"):
            res = self.cli("disk", sub, cwd=repo)
            self.assertEqual(res.returncode, 2, (sub, res.stdout, res.stderr))
            self.assertIn("bogusKey", res.stderr)

        # 2. Inverted watermark (`targetPct >= highPct`) fails closed and names the field.
        self.write_user_config({"disk": {"highPct": 80, "targetPct": 85}})
        res = self.cli("disk", "prune", "--dry-run", cwd=repo)
        self.assertEqual(res.returncode, 2, (res.stdout, res.stderr))
        self.assertIn("disk.targetPct", res.stderr)

        # 3. Malformed JSON in user config fails closed (never falls back to defaults).
        self.user_config_path().write_text("{not valid json")
        res = self.cli("disk", "status", cwd=repo)
        self.assertEqual(res.returncode, 2, (res.stdout, res.stderr))
        self.assertIn("config JSON", res.stderr)

        # 4. Repository `.delegate.json` setting `disk` fails closed identically to `config::load`.
        self.user_config_path().write_text(json.dumps({}))
        (repo / ".delegate.json").write_text(json.dumps({"disk": {"highPct": 85}}))
        res = self.cli("disk", "prune", "--dry-run", cwd=repo)
        self.assertEqual(res.returncode, 2, (res.stdout, res.stderr))
        self.assertIn("disk", res.stderr)
        self.assertIn(".delegate.json", res.stderr)


if __name__ == "__main__":
    unittest.main()
