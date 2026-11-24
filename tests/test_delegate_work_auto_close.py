"""Tests for work auto-close on apply, work_applied:<Wxx> unlock,
parent-chain downstream hints, and premerge blocked-downstream prioritization.
"""
import json
import os
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateWorkAutoCloseTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def work_show_json(self, wid: str) -> dict:
        res = self.cli("work", "show", wid, "--json", human=True)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        return json.loads(res.stdout)

    def git_head(self, repo: Path) -> str:
        return subprocess.check_output(
            ["git", "-C", str(repo), "rev-parse", "HEAD"], text=True
        ).strip()

    def test_apply_auto_closes_with_text_done_when_working_tree_and_commit_evidence(self):
        repo = self.repo({"a.txt": "v0\n", "b.txt": "v0\n"})
        base_head = self.git_head(repo)[:7]

        # W001 has text doneWhen only -> apply (without --commit) auto-closes with working-tree evidence
        self.assertEqual(
            self.cli(
                "work", "add", "仅写完成条件应自动关",
                "--repo", repo,
                "--done-when", "改完 a.txt 且单测通过",
                human=True,
            ).returncode,
            0,
        )
        self.fake_pi([fixtures.answer("updated a"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--work", "W001", "--accept", "true", "edit a")
        )
        ap1 = self.cli("apply", r1["run"], human=True)
        self.assertEqual(ap1.returncode, 0, ap1.stdout + ap1.stderr)
        self.assertIn("已自动关闭工作项 [W001]", ap1.stdout)
        w1 = self.work_show_json("W001")
        self.assertEqual(w1["state"], "done")
        self.assertEqual(w1["closedKind"], "auto")
        self.assertIn(r1["run"], w1["closeEvidence"])
        self.assertIn(f"working-tree @ {base_head}", w1["closeEvidence"])

        # Commit a.txt cleanly so --commit on W002 has a clean index
        subprocess.run(["git", "-C", str(repo), "add", "a.txt"], check=True)
        subprocess.run(["git", "-C", str(repo), "commit", "-q", "-m", "land a"], check=True)

        # W002 has text doneWhen -> apply --commit auto-closes with commit short hash evidence
        self.assertEqual(
            self.cli(
                "work", "add", "带提交自动关",
                "--repo", repo,
                "--done-when", "改完 b.txt 并提交",
                human=True,
            ).returncode,
            0,
        )
        self.fake_pi([fixtures.answer("updated b"), fixtures.SETTLED], pre="echo v1 > b.txt")
        r2 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--work", "W002", "--accept", "true", "edit b")
        )
        ap2 = self.cli("apply", "--commit", r2["run"], human=True)
        self.assertEqual(ap2.returncode, 0, ap2.stdout + ap2.stderr)
        new_head = self.git_head(repo)[:7]
        w2 = self.work_show_json("W002")
        self.assertEqual(w2["state"], "done")
        self.assertEqual(w2["closedKind"], "auto")
        self.assertEqual(w2["closeEvidence"], f"applied {r2['run']} ({new_head})")

    def test_all_non_auto_close_exceptions_stay_open_with_reasons(self):
        repo = self.repo({
            "f1.txt": "v0\n",
            "f2.txt": "v0\n",
            "f3.txt": "v0\n",
            "f4.txt": "v0\n",
            "f5.txt": "v0\n",
            "f6.txt": "v0\n",
            "f7.txt": "v0\n",
            "f8.txt": "v0\n",
            "f9.txt": "v0\n",
        })

        # 1a) Explicit --needs-verify on work add
        self.assertEqual(
            self.cli("work", "add", "显式人工验证", "--repo", repo, "--needs-verify", human=True).returncode,
            0,
        )
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo v1 > f1.txt")
        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--work", "W001", "--accept", "true", "f1"))
        ap1 = self.cli("apply", r1["run"], human=True)
        self.assertEqual(ap1.returncode, 0, ap1.stdout + ap1.stderr)
        self.assertIn("留在 verify", ap1.stdout)
        self.assertIn("--needs-verify", ap1.stdout)
        w1 = self.work_show_json("W001")
        self.assertEqual(w1["state"], "verify")
        self.assertTrue(w1.get("needsVerify"))

        # 1b) `work wait --needs-verify` sets flag and `--no-needs-verify` clears it
        self.assertEqual(self.cli("work", "add", "后补验证标记", "--repo", repo, human=True).returncode, 0)
        self.assertEqual(self.cli("work", "wait", "W002", "--needs-verify", human=True).returncode, 0)
        self.assertTrue(self.work_show_json("W002").get("needsVerify"))
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo v1 > f2.txt")
        r2 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--work", "W002", "--accept", "true", "f2"))
        ap2 = self.cli("apply", r2["run"], human=True)
        self.assertEqual(ap2.returncode, 0, ap2.stdout + ap2.stderr)
        self.assertEqual(self.work_show_json("W002")["state"], "verify")
        self.assertEqual(self.cli("work", "wait", "W002", "--no-needs-verify", human=True).returncode, 0)
        self.assertFalse(self.work_show_json("W002").get("needsVerify", False))

        # 2a) Unsatisfied verify_on
        self.assertEqual(
            self.cli(
                "work", "add", "未满足verify_on", "--repo", repo,
                "--verify-on", "commit_exists:deadbeef", human=True,
            ).returncode,
            0,
        )
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo v1 > f3.txt")
        r3 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--work", "W003", "--accept", "true", "f3"))
        ap3 = self.cli("apply", r3["run"], human=True)
        self.assertEqual(ap3.returncode, 0, ap3.stdout + ap3.stderr)
        self.assertIn("闭环条件待满足", ap3.stdout)
        self.assertEqual(self.work_show_json("W003")["state"], "verify")

        # 2b) Artifacts present
        self.assertEqual(
            self.cli("work", "add", "带产物", "--repo", repo, "--artifact", ".local/out.bin", human=True).returncode,
            0,
        )
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo v1 > f4.txt")
        r4 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--work", "W004", "--accept", "true", "f4"))
        ap4 = self.cli("apply", r4["run"], human=True)
        self.assertEqual(ap4.returncode, 0, ap4.stdout + ap4.stderr)
        self.assertIn("未接线产物", ap4.stdout)
        self.assertEqual(self.work_show_json("W004")["state"], "verify")

        # 2c) Unchecked followups present
        self.assertEqual(
            self.cli("work", "add", "带未勾待办", "--repo", repo, "--followup", "补一份文档", human=True).returncode,
            0,
        )
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo v1 > f5.txt")
        r5 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--work", "W005", "--accept", "true", "f5"))
        ap5 = self.cli("apply", r5["run"], human=True)
        self.assertEqual(ap5.returncode, 0, ap5.stdout + ap5.stderr)
        self.assertIn("未完成待办", ap5.stdout)
        self.assertEqual(self.work_show_json("W005")["state"], "verify")

        # 3a) Same work item has another run in `question` state
        self.assertEqual(self.cli("work", "add", "同项有提问中run", "--repo", repo, human=True).returncode, 0)
        self.fake_pi([fixtures.answer("需要主控决定\n\n是否保留旧字段？"), fixtures.SETTLED])
        r6_q = self.outcome(self.cli("run", "--read-only", "--workdir", repo, "--work", "W006", "ask question"))
        self.assertTrue(r6_q.get("question"))
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo v1 > f6.txt")
        r6_w = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--work", "W006", "--accept", "true", "f6"))
        ap6 = self.cli("apply", r6_w["run"], human=True)
        self.assertEqual(ap6.returncode, 0, ap6.stdout + ap6.stderr)
        self.assertIn("提问中", ap6.stdout)
        self.assertNotEqual(self.work_show_json("W006")["state"], "done")

        # 3b) Same work item has another run still `running`
        self.assertEqual(self.cli("work", "add", "同项有在跑run", "--repo", repo, human=True).returncode, 0)
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo v1 > f7.txt")
        r7_w = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--work", "W007", "--accept", "true", "f7"))
        self.fake_pi([fixtures.answer("slow"), fixtures.SETTLED], pre="sleep 30")
        r7_running = self.outcome(self.cli("start", "--read-only", "--workdir", repo, "--work", "W007", "slow task"))
        try:
            self.until(
                lambda: self.outcome(self.cli("status", r7_running["run"])).get("state") == "running",
                10,
            )
            ap7 = self.cli("apply", r7_w["run"], human=True)
            self.assertEqual(ap7.returncode, 0, ap7.stdout + ap7.stderr)
            self.assertIn("在跑中", ap7.stdout)
            self.assertNotEqual(self.work_show_json("W007")["state"], "done")
        finally:
            self.cli("stop", r7_running["run"], human=True)

        # 4) --force applied a rejected run (accept failed) -> stays in verify with reason
        self.assertEqual(self.cli("work", "add", "被拒run强合", "--repo", repo, human=True).returncode, 0)
        self.fake_pi([fixtures.answer("attempted"), fixtures.SETTLED], pre="echo v1 > f8.txt")
        r8 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--work", "W008", "--accept", "false", "f8"))
        self.assertEqual(r8["state"], "rejected")
        ap8 = self.cli("apply", "--force", "--no-verify", r8["run"], human=True)
        self.assertEqual(ap8.returncode, 0, ap8.stdout + ap8.stderr)
        self.assertIn("留在 verify", ap8.stdout)
        self.assertIn("被拒或验收未通过", ap8.stdout)
        self.assertEqual(self.work_show_json("W008")["state"], "verify")

        # 5) Explicit `apply --keep-open` -> stays in verify with reason; exclusive with --closes
        self.assertEqual(self.cli("work", "add", "显式保留verify", "--repo", repo, human=True).returncode, 0)
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo v1 > f9.txt")
        r9 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--work", "W009", "--accept", "true", "f9"))
        bad_combo = self.cli("apply", "--keep-open", "--closes", "W009", r9["run"], human=True)
        self.assertNotEqual(bad_combo.returncode, 0)
        self.assertIn("exclusive", bad_combo.stderr)
        ap9 = self.cli("apply", "--keep-open", r9["run"], human=True)
        self.assertEqual(ap9.returncode, 0, ap9.stdout + ap9.stderr)
        self.assertIn("留在 verify", ap9.stdout)
        self.assertIn("--keep-open", ap9.stdout)
        self.assertEqual(self.work_show_json("W009")["state"], "verify")

    def test_after_applied_work_unlocks_on_apply_before_done(self):
        repo = self.repo({"core.txt": "v0\n"})

        # W001 requires manual verification after apply (--needs-verify)
        self.assertEqual(
            self.cli("work", "add", "核心改动需真机验", "--repo", repo, "--needs-verify", human=True).returncode,
            0,
        )
        # W002 waits via `work add --after-applied W001` (work_applied:W001)
        self.assertEqual(
            self.cli("work", "add", "等代码落即开始", "--repo", repo, "--after-applied", "W001", human=True).returncode,
            0,
        )
        # W003 attaches via `work wait --after-applied W001`
        self.assertEqual(self.cli("work", "add", "后挂合入前置", "--repo", repo, human=True).returncode, 0)
        self.assertEqual(self.cli("work", "wait", "W003", "--after-applied", "w1", human=True).returncode, 0)
        # W004 waits via `--after-work W001` (work_closed:W001) for contrast
        self.assertEqual(
            self.cli("work", "add", "等闭环才开始", "--repo", repo, "--after-work", "W001", human=True).returncode,
            0,
        )

        self.assertEqual(self.work_show_json("W002")["waitOn"], ["work_applied:W001"])
        self.assertEqual(self.work_show_json("W003")["waitOn"], ["work_applied:W001"])
        self.assertEqual(self.work_show_json("W002")["state"], "blocked")
        self.assertEqual(self.work_show_json("W003")["state"], "blocked")
        self.assertEqual(self.work_show_json("W004")["state"], "blocked")

        # Cycle check rejects W001 waiting on W002
        cyc = self.cli("work", "wait", "W001", "--after-applied", "W002", human=True)
        self.assertNotEqual(cyc.returncode, 0)
        self.assertIn("cycle", cyc.stderr)

        # Apply W001's run -> W001 stays `verify`, while W002 & W003 unlock to `ready`; W004 stays `blocked`!
        self.fake_pi([fixtures.answer("core done"), fixtures.SETTLED], pre="echo v1 > core.txt")
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--work", "W001", "--accept", "true", "core change")
        )
        ap = self.cli("apply", r1["run"], human=True)
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)
        self.assertIn("已解锁 [W002]", ap.stdout)
        self.assertIn("已解锁 [W003]", ap.stdout)
        self.assertEqual(self.work_show_json("W001")["state"], "verify")
        self.assertTrue(self.work_show_json("W001").get("applied"))
        self.assertEqual(self.work_show_json("W002")["state"], "ready")
        self.assertEqual(self.work_show_json("W003")["state"], "ready")
        self.assertEqual(self.work_show_json("W004")["state"], "blocked")

        # Closing W001 unlocks W004
        self.assertEqual(
            self.cli("work", "close", "W001", "--evidence", "真机验证通过", human=True).returncode,
            0,
        )
        self.assertEqual(self.work_show_json("W004")["state"], "ready")

    def test_parent_chain_leaf_matches_root_run_in_wait_and_events_hints(self):
        repo = self.repo({"a.txt": "v0\n"})

        # Start root run
        self.fake_pi([fixtures.answer("round 1"), fixtures.SETTLED], pre="echo v1 > a.txt")
        root = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "delegate-fixes", "--accept", "true", "round 1")
        )

        # Downstream work item waits on the ROOT run ID
        self.assertEqual(
            self.cli(
                "work", "add", "下游等根run合入", "--repo", repo, "--after-applied", root["run"], human=True
            ).returncode,
            0,
        )

        # Record event cursor before reply leaf
        ev_before = json.loads(
            self.cli("events", "--json", human=True).stdout.strip().splitlines()[-1]
        )["seq"]

        # Reply creates leaf run with parent == root
        self.fake_pi([fixtures.answer("round 2"), fixtures.SETTLED], pre="echo v2 > a.txt")
        leaf = self.outcome(self.cli("reply", "--wait", root["run"], "round 2"))
        self.assertEqual(leaf["parent"], root["run"])

        # `delegate wait <leaf>` tail hints recognize downstream waiting on root run!
        wait_leaf = self.cli("wait", leaf["run"], human=True)
        self.assertEqual(wait_leaf.returncode, 0, wait_leaf.stdout + wait_leaf.stderr)
        self.assertIn("关联下游：[W001]", wait_leaf.stdout)
        self.assertIn("合入后解锁", wait_leaf.stdout)

        # `delegate wait --since <ev_before>` (hints_for_events) also recognizes downstream via parent chain!
        wait_since = self.cli("wait", "--since", str(ev_before), human=True)
        self.assertEqual(wait_since.returncode, 0, wait_since.stdout + wait_since.stderr)
        self.assertIn("关联下游：[W001]", wait_since.stdout)

    def test_premerge_worker_prioritizes_higher_transitive_blocked_downstream(self):
        order_log = Path(self.temp.name) / "premerge-order.log"
        # Accept command appends which run's file changed so we observe exact worker order
        accept_cmd = (
            f"if grep -q early_v1 early.txt; then echo early >> {order_log}; fi; "
            f"if grep -q chain_v2 chain.txt; then echo chain >> {order_log}; fi"
        )
        repo = self.repo({
            "early.txt": "v0\n",
            "chain.txt": "v0\n",
            "main.txt": "v0\n",
        })

        # Disable automatic background premerge while setting up both runs
        self.env["DELEGATE_PREMERGE"] = "0"

        # 1) Earlier run with 0 blocked downstream
        self.fake_pi([fixtures.answer("early done"), fixtures.SETTLED], pre="echo early_v1 > early.txt")
        run_early = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "early-no-down", "--accept", accept_cmd, "early")
        )

        # 2) Later run with a reply leaf whose root run transitively blocks 3 work items (W001 -> W002 -> W003)
        self.fake_pi([fixtures.answer("chain r1"), fixtures.SETTLED], pre="echo chain_v1 > chain.txt")
        run_root = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "chain-root", "--accept", accept_cmd, "chain r1")
        )
        self.fake_pi([fixtures.answer("chain r2"), fixtures.SETTLED], pre="echo chain_v2 > chain.txt")
        run_leaf = self.outcome(self.cli("reply", "--wait", run_root["run"], "chain r2"))

        self.assertEqual(
            self.cli("work", "add", "下游1", "--repo", repo, "--after-applied", run_root["run"], human=True).returncode,
            0,
        )
        self.assertEqual(
            self.cli("work", "add", "下游2", "--repo", repo, "--after-applied", "W001", human=True).returncode,
            0,
        )
        self.assertEqual(
            self.cli("work", "add", "下游3", "--repo", repo, "--after-work", "W002", human=True).returncode,
            0,
        )

        # Advance source HEAD on an unrelated file so `_merge-worker` must execute `accept_cmd`
        # (cannot reuse original accept.tree) and records exact execution order in `order_log`.
        (repo / "main.txt").write_text("v1\n")
        subprocess.run(["git", "-C", str(repo), "add", "main.txt"], check=True)
        subprocess.run(["git", "-C", str(repo), "commit", "-q", "-m", "advance head"], check=True)

        # Clear any log entries written during the initial run acceptances
        if order_log.exists():
            order_log.unlink()

        # Run `_merge-worker` synchronously with premerge enabled
        self.env["DELEGATE_PREMERGE"] = "1"
        worker = self.cli("_merge-worker", str(repo), human=True)
        self.assertEqual(worker.returncode, 0, worker.stdout + worker.stderr)

        lines = [ln.strip() for ln in order_log.read_text().splitlines() if ln.strip()]
        self.assertEqual(lines, ["chain", "early"], f"expected chain leaf before early run, got {lines}")

        # Verify both runs reached premerge `ready`
        pm_leaf = json.loads((Path(run_leaf["dir"]) / "premerge.json").read_text())
        pm_early = json.loads((Path(run_early["dir"]) / "premerge.json").read_text())
        self.assertEqual(pm_leaf["status"], "ready")
        self.assertEqual(pm_early["status"], "ready")


if __name__ == "__main__":
    unittest.main()
