"""6.36.0 regressions:
1. Unified zero-lock, zero-git Stop hook (`skills/delegate/hooks/claude-code-stop.py`):
   - Matches both bare `delegate wait` in PATH and `/.../delegate wait`.
   - Skips unmerged when `apply`, serial `merge`, `_merge-worker` premerge, or `lane` gate is running.
   - Skips zero-change deliveries.
   - Drops `deferred.tsv` in Stop hook; deferrals live in `work.jsonl` via `delegate work defer --reason --until`
     (with automatic migration of legacy `deferred.tsv`).
   - Appends every block to `<state>/stop-hook.jsonl`; `delegate hook stats` reports weekly trigger counts
     and post-block outcomes (`挂了 wait` / `apply 了` / `defer 了` / `被忽略了`).
2. Work table false positives (W044):
   - Cleaned/archived runs no longer affect work item state (e.g. manual `wait_on` stays `blocked`).
   - Zero-change deliveries display as `已收取` instead of `attention · 待合 +0f`.
3. Review card false positives (6.34 risk rules):
   - Lockfile self-version-only bump is not flagged as dependency risk.
   - Language- and syntax-aware skip markers (`sys.exit(0)` in Python does not hit `xit(`).
   - `[&str; 10]` -> `[&str; 12]`, `7.min(len)`, and plain field `unwrap_or(0)` do not hit `config_constant`.
   - Real 6.35 diff (`27058e5`) replay leaves only the 1 `premerge` lock risk item.
"""
import json
import os
import shlex
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures

ROOT = Path(__file__).resolve().parent.parent
STOP_HOOK = ROOT / "skills/delegate/hooks/claude-code-stop.py"


class Delegate636Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def run_stop_hook(self, cwd: Path, extra_env: dict | None = None, active: bool = False):
        env = {
            **self.env,
            "DELEGATE_BIN": str(fixtures.DELEGATE),
            "DELEGATE_STOP_HOOK_WAITERS": "",
            "DELEGATE_STOP_HOOK_PROCS": "",
            "DELEGATE_STOP_HOOK_CURRENT_ONLY": "1",
        }
        if extra_env:
            env.update(extra_env)
        res = subprocess.run(
            [sys.executable, str(STOP_HOOK)],
            input=json.dumps({"cwd": str(cwd), "stop_hook_active": active}),
            capture_output=True,
            text=True,
            env=env,
            timeout=10,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        out = res.stdout.strip()
        return json.loads(out) if out else None

    def consume_events(self, repo: Path):
        evs = [
            json.loads(ln)
            for ln in self.cli("events", "--json", cwd=repo, human=True).stdout.splitlines()
            if ln.strip()
        ]
        max_seq = max((e["seq"] for e in evs), default=0)
        self.cli("wait", "--since", str(max_seq), "--max", "0.05", cwd=repo, human=True)

    def test_unified_stop_hook_four_scenarios_zero_git_and_defer_migration(self):
        repo = self.repo({"a.txt": "v0\n"})

        # Shadow `git` with a failing stub to prove the Stop hook runs strictly with zero git calls.
        nogit_bin = self.work / "nogit-bin"
        nogit_bin.mkdir(exist_ok=True)
        bad_git = nogit_bin / "git"
        bad_git.write_text("#!/bin/sh\necho 'git forbidden in stop hook' >&2\nexit 99\n")
        bad_git.chmod(0o755)
        nogit_path = f"{nogit_bin}:{self.env['PATH']}"

        # Scenario 1: Live run + bare `delegate wait` vs no waiter.
        gate = self.work / "stop-live-gate"
        self.fake_pi(
            [fixtures.answer("live done"), fixtures.SETTLED],
            pre=f"while [ ! -f {shlex.quote(str(gate))} ]; do sleep 0.05; done",
        )
        live = self.outcome(self.cli("start", "--read-only", "--workdir", repo, "--name", "live-job", "wait"))
        self.addCleanup(lambda: (gate.touch(), self.cli("stop", live["run"], cwd=repo)))

        # Without waiter -> blocks & logs `waiter` to stop-hook.jsonl.
        blk_waiter = self.run_stop_hook(repo, extra_env={"PATH": nogit_path})
        self.assertIsNotNone(blk_waiter)
        self.assertEqual(blk_waiter["decision"], "block")
        self.assertIn("live-job", blk_waiter["reason"])

        # Bare `delegate wait` in PATH -> passes!
        self.assertIsNone(
            self.run_stop_hook(
                repo,
                extra_env={"PATH": nogit_path, "DELEGATE_STOP_HOOK_WAITERS": "delegate wait"},
            )
        )
        # Path-qualified `/.../delegate wait` -> passes!
        self.assertIsNone(
            self.run_stop_hook(
                repo,
                extra_env={"PATH": nogit_path, "DELEGATE_STOP_HOOK_WAITERS": f"{fixtures.DELEGATE} wait --all"},
            )
        )
        gate.touch()
        self.assertEqual(self.cli("wait", live["run"], cwd=repo).returncode, 0)
        self.assertEqual(self.cli("clean", "--no-archive", live["run"], cwd=repo, human=True).returncode, 0)

        # Scenario 3: Zero-change delivery -> does NOT count as unmerged!
        self.fake_pi([fixtures.answer("nothing needed"), fixtures.SETTLED])
        r_zero = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "zero-deliv", "--accept", "true", "check only")
        )
        self.consume_events(repo)
        self.assertIsNone(self.run_stop_hook(repo, extra_env={"PATH": nogit_path}))

        # Scenario 4: Real unmerged delivery -> blocks!
        self.fake_pi([fixtures.answer("edited a.txt"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r_real = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "real-feat", "--accept", "true", "edit a")
        )
        self.consume_events(repo)
        blk_unmerged = self.run_stop_hook(repo, extra_env={"PATH": nogit_path})
        self.assertIsNotNone(blk_unmerged)
        self.assertEqual(blk_unmerged["decision"], "block")
        self.assertIn("real-feat", blk_unmerged["reason"])
        self.assertIn("delegate work defer", blk_unmerged["reason"])

        # Scenario 2: In-flight `apply`, `merge`, `_merge-worker` premerge, and `lane` gate -> NOT unmerged!
        self.assertIsNone(
            self.run_stop_hook(
                repo,
                extra_env={"PATH": nogit_path, "DELEGATE_STOP_HOOK_PROCS": f"delegate apply {r_real['run']}"},
            )
        )
        self.assertIsNone(
            self.run_stop_hook(
                repo,
                extra_env={"PATH": nogit_path, "DELEGATE_STOP_HOOK_PROCS": "/usr/local/bin/delegate merge --ready"},
            )
        )
        self.assertIsNone(
            self.run_stop_hook(
                repo,
                extra_env={"PATH": nogit_path, "DELEGATE_STOP_HOOK_PROCS": "delegate lane scripts/verify.sh"},
            )
        )
        # Running premerge.json with live workerPid -> NOT unmerged!
        pm_path = Path(r_real["dir"]) / "premerge.json"
        pm_path.write_text(json.dumps({"status": "running", "workerPid": os.getpid()}))
        self.assertIsNone(self.run_stop_hook(repo, extra_env={"PATH": nogit_path}))
        pm_path.unlink()

        # Legacy `deferred.tsv` is ignored by the Stop hook directly, but auto-imported by `work --rebuild`!
        def_tsv = repo / ".local/delegate/deferred.tsv"
        def_tsv.parent.mkdir(parents=True, exist_ok=True)
        def_tsv.write_text(f"{r_real['run']}\t历史延后原因\t{int(time.time())}\n")
        still_blk = self.run_stop_hook(repo, extra_env={"PATH": nogit_path})
        self.assertIsNotNone(still_blk, "Stop hook must not read deferred.tsv directly")

        # `work --rebuild` imports existing deferred.tsv into work.jsonl -> Stop hook now honors work.jsonl!
        self.assertEqual(self.cli("work", "--rebuild", cwd=repo, human=True).returncode, 0)
        self.assertIsNone(self.run_stop_hook(repo, extra_env={"PATH": nogit_path}))

        # `work defer` requires BOTH --reason and --until.
        missing_until = self.cli("work", "defer", r_real["run"], "--reason", "等窗口", cwd=repo, human=True)
        self.assertNotEqual(missing_until.returncode, 0)
        self.assertIn("--until", missing_until.stderr)

        # Verify stop-hook.jsonl logged both waiter and unmerged blocks with hook type and run names.
        log_path = self.state_dir() / "stop-hook.jsonl"
        self.assertTrue(log_path.is_file())
        rows = [json.loads(ln) for ln in log_path.read_text().splitlines() if ln.strip()]
        hooks_seen = {r["hook"] for r in rows}
        self.assertIn("waiter", hooks_seen)
        self.assertIn("unmerged", hooks_seen)
        self.assertTrue(any("real-feat" in r.get("runs", "") for r in rows if r["hook"] == "unmerged"))

    def test_hook_stats_end_to_end_weekly_and_outcomes(self):
        repo = self.repo({"a.txt": "v0\n"})

        # 1) Block on waiter -> then attach `delegate wait` -> attributed as `waited`
        gate = self.work / "stats-gate"
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre=f"while [ ! -f {shlex.quote(str(gate))} ]; do sleep 0.05; done",
        )
        r_live = self.outcome(self.cli("start", "--read-only", "--workdir", repo, "--name", "stat-live", "go"))
        self.addCleanup(lambda: (gate.touch(), self.cli("stop", r_live["run"], cwd=repo)))
        self.assertIsNotNone(self.run_stop_hook(repo))
        gate.touch()
        self.assertEqual(self.cli("wait", r_live["run"], cwd=repo).returncode, 0)
        self.assertEqual(self.cli("clean", "--no-archive", r_live["run"], cwd=repo, human=True).returncode, 0)

        # 2) Block on unmerged -> then `delegate apply` -> attributed as `applied`
        self.fake_pi([fixtures.answer("edit 1"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r_apply = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "stat-apply", "--accept", "true", "e1"))
        self.consume_events(repo)
        self.assertIsNotNone(self.run_stop_hook(repo))
        self.assertEqual(self.cli("apply", r_apply["run"], cwd=repo, human=True).returncode, 0)

        # 3) Block on unmerged -> then `delegate work defer` -> attributed as `deferred`
        self.fake_pi([fixtures.answer("edit 2"), fixtures.SETTLED], pre="echo v2 > a.txt")
        r_def = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "stat-defer", "--accept", "true", "e2"))
        self.consume_events(repo)
        self.assertIsNotNone(self.run_stop_hook(repo))
        self.assertEqual(
            self.cli("work", "defer", r_def["run"], "--reason", "等窗口", "--until", "4h", cwd=repo, human=True).returncode,
            0,
        )

        # 4) Block on unmerged -> do nothing -> attributed as `ignored`
        self.fake_pi([fixtures.answer("edit 3"), fixtures.SETTLED], pre="echo v3 > a.txt")
        self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "stat-ign", "--accept", "true", "e3"))
        self.consume_events(repo)
        self.assertIsNotNone(self.run_stop_hook(repo))

        st_json = json.loads(self.cli("hook", "stats", "--json", cwd=repo, human=True).stdout)
        self.assertEqual(st_json["byHook"].get("waiter"), 1, st_json)
        self.assertEqual(st_json["byHook"].get("unmerged"), 3, st_json)
        self.assertGreaterEqual(st_json["outcomes"]["waited"], 1, st_json)
        self.assertEqual(st_json["outcomes"]["applied"], 1, st_json)
        self.assertEqual(st_json["outcomes"]["deferred"], 1, st_json)
        self.assertEqual(st_json["outcomes"]["ignored"], 1, st_json)
        self.assertEqual(len(st_json["weeks"]), 1)

        st_human = self.cli("hook", "stats", cwd=repo, human=True)
        self.assertEqual(st_human.returncode, 0, st_human.stderr)
        for token in ("挂了 wait", "apply 了", "defer 了", "被忽略了", "合计"):
            self.assertIn(token, st_human.stdout)

    def test_w044_worktable_cleaned_and_zero_change_deliveries(self):
        repo = self.repo({"a.txt": "v0\n"})

        # W038 scenario: work item with structured wait_on precondition + archived zero-change write run
        add = self.cli(
            "work",
            "add",
            "Phase 0 A/B 观察",
            "--repo",
            repo,
            "--after-time",
            "2099-01-01",
            "--json",
            cwd=repo,
            human=True,
        )
        self.assertEqual(add.returncode, 0, add.stderr)
        wid = json.loads(add.stdout)["id"]

        self.fake_pi([fixtures.answer("checked, no code changes needed"), fixtures.SETTLED])
        r0 = self.outcome(
            self.cli(
                "run",
                "--worktree",
                "--workdir",
                repo,
                "--work",
                wid,
                "--name",
                "w038-check",
                "--accept",
                "true",
                "inspect phase 0",
            )
        )
        # Even BEFORE clean: zero-change delivery must NOT force `attention · 待合 +0f`!
        w_before = json.loads(self.cli("work", "show", wid, "--json", cwd=repo, human=True).stdout)
        self.assertEqual(w_before["state"], "blocked", w_before)
        self.assertNotIn("待合 +0f", w_before["focus"])

        # After clean (archived to `<cache>/archive/<run>/record`): STILL `blocked`, never `attention`!
        self.assertEqual(self.cli("clean", "--force", r0["run"], cwd=repo, human=True).returncode, 0)
        w_after = json.loads(self.cli("work", "show", wid, "--json", cwd=repo, human=True).stdout)
        self.assertEqual(w_after["state"], "blocked", w_after)
        self.assertTrue(w_after["focus"].startswith("等到 ") and "01-01" in w_after["focus"], w_after["focus"])

        # Live zero-change delivery on a work item without wait_on -> verify + focus `已收取`
        add2 = self.cli("work", "add", "零改动核对", "--repo", repo, "--json", cwd=repo, human=True)
        wid2 = json.loads(add2.stdout)["id"]
        self.fake_pi([fixtures.answer("verified ok"), fixtures.SETTLED])
        self.outcome(
            self.cli(
                "run",
                "--worktree",
                "--workdir",
                repo,
                "--work",
                wid2,
                "--name",
                "zero-bound",
                "--accept",
                "true",
                "verify only",
            )
        )
        w2 = json.loads(self.cli("work", "show", wid2, "--json", cwd=repo, human=True).stdout)
        self.assertEqual(w2["state"], "verify", w2)
        self.assertEqual(w2["focus"], "已收取", w2)

        # Unattached ephemeral zero-change delivery -> work table shows `已收取`, NOT `待合 +0f`!
        self.fake_pi([fixtures.answer("ephemeral ok"), fixtures.SETTLED])
        self.outcome(
            self.cli(
                "run",
                "--worktree",
                "--workdir",
                repo,
                "--name",
                "zero-eph",
                "--accept",
                "true",
                "ephemeral check",
            )
        )
        table = self.cli("work", cwd=repo, human=True).stdout
        self.assertIn("已收取", table)
        self.assertNotIn("待合 +0f", table)

    def test_review_card_replay_of_635_diff_leaves_only_premerge_lock(self):
        # Verify commit 27058e5 exists in current repo (skip only in shallow clones lacking parent).
        chk = subprocess.run(
            ["git", "-C", str(ROOT), "rev-parse", "--verify", "27058e5^"],
            capture_output=True,
            text=True,
        )
        if chk.returncode != 0:
            self.skipTest("6.35 commit 27058e5 not in git history")

        diff_635 = subprocess.check_output(
            ["git", "-C", str(ROOT), "show", "--format=", "27058e5"],
            text=True,
        )
        repo = self.repo({"README.md": "init\n"})
        self.fake_pi([fixtures.answer("6.35.0 后台预合并与快进 apply"), fixtures.SETTLED], pre="echo x > README.md")
        r = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "replay-635", "--accept", "true", "replay")
        )
        run_dir = Path(r["dir"])
        # Replace changes.patch and remove changes.json/card.json so `delegate card` reads the 6.35 diff directly.
        for stale in ("card.json", "changes.json"):
            p = run_dir / stale
            if p.exists():
                p.unlink()
        (run_dir / "changes.patch").write_text(diff_635, encoding="utf-8")
        meta = json.loads((run_dir / "meta.json").read_text())
        meta["worktree"] = None
        meta.pop("base", None)
        meta.pop("chainBase", None)
        meta.pop("netBase", None)
        (run_dir / "meta.json").write_text(json.dumps(meta))
        sum_doc = json.loads((run_dir / "summary.json").read_text())
        sum_doc.pop("after", None)
        (run_dir / "summary.json").write_text(json.dumps(sum_doc))

        card_json = json.loads(self.cli("card", r["run"], "--json", cwd=repo, human=True).stdout)
        risks = card_json["risks"]
        self.assertEqual(len(risks), 1, f"expected only 1 premerge lock risk, got: {json.dumps(risks, ensure_ascii=False, indent=2)}")
        self.assertEqual(risks[0]["rule"], "concurrency_unsafe")
        self.assertEqual(risks[0]["file"], "crates/delegate/src/premerge.rs")
        self.assertIn("acquire_shell", risks[0]["reason"])


if __name__ == "__main__":
    unittest.main()
