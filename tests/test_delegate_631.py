"""6.31.0 regressions:
- Unified work table `<state>/work.jsonl`, 7-state machine, pre-reserved `artifacts[]`/`followups[]`
- Layer-0 summary + Layer-1 6-column table (merging ephemeral unattached runs as `R:<short>`) + Layer-2 `work show` card with Host pointers
- `wait_on`: `run_applied` (`--after-applied`), `run_delivered`, `work_closed` (cycle detection), read-only `owner:Qxx` (`docs/owner/README.md`), `after_time`, `manual`
- Timely tail hints on `wait`, `wait --since`, `apply`, `merge`, `clean` (zero prefix change, Host pointers)
- `start --work` and `reply` inheriting `workId`; `apply --closes`; `work close` / `drop` / `defer`
- `work wait --after-applied --auto` and `queue add --after-applied` zero-token auto-dispatch
- `work import-md --dry-run` with `history.jsonl` cross-checking and `<file>:<line>` pointers
- Automatic idempotent migration of `notes.json` & `deferred.tsv` + thin-shell `delegate note`
- Graded Stop hook: blocks immediate blockers and one-shot `work_*` alert events via cursor; never blocks `blocked`/`deferred`/non-stale `verify`/backlog `ready`
- Backward compatibility for pre-6.31 runs without `workId`
"""
import json
import os
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures

ROOT = Path(__file__).resolve().parent.parent
STOP_HOOK = ROOT / "skills/delegate/hooks/claude-code-stop.py"


class Delegate631Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def run_stop_hook(self, payload, env=None):
        run_env = {
            **self.env,
            "DELEGATE_BIN": str(fixtures.DELEGATE),
            "DELEGATE_STOP_HOOK_WAITERS": "",
        }
        if env:
            run_env.update(env)
        res = subprocess.run(
            [sys.executable, str(STOP_HOOK)],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            env=run_env,
            timeout=15,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        out = res.stdout.strip()
        return json.loads(out) if out else None

    def test_33_motion_library_end_to_end_walkthrough(self):
        repo = self.repo({
            "src/body.ts": "export const idle = false;\n",
            "src/tab.ts": "export const tab = 'old';\n",
        })
        brief = repo / ".local/plan/brief-body-idle-scene.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# 3D 默认待机动作与书房场景\n将资源移入 assets/body/ 并接上前端 idle 循环。\n")

        # 1. Create work item with done_when
        add = self.cli(
            "work", "add", "3D默认待机动作与默认书房场景上线",
            "--repo", repo,
            "--done-when", "8个公共动作与书房场景入库、前端默认播idle、明暗截图已验",
            "--artifact", ".local/body-motion/public-library/",
            "--artifact", ".local/scene/default/",
            human=True,
        )
        self.assertEqual(add.returncode, 0, add.stdout + add.stderr)
        self.assertIn("[W001]", add.stdout)
        self.assertIn("delegate work show W001", add.stdout)

        # 2. Dispatch first baton R1 with --work W001 -> delivers & read -> stays in verify
        self.fake_pi([fixtures.answer("已生成 8 个动作到 .local/body-motion/public-library/"), fixtures.SETTLED])
        r1 = self.outcome(self.cli(
            "run", "--read-only", "--workdir", repo, "--work", "W001", "--name", "motion-gen", "生成动作"
        ))
        self.assertEqual(r1["workId"], "W001")

        # Clean R1 -> 1-line retention confirmation with pointer, never blocks clean
        clean1 = self.cli("clean", "--no-archive", r1["run"], human=True)
        self.assertEqual(clean1.returncode, 0, clean1.stdout + clean1.stderr)
        self.assertIn("工作项保留：[W001]", clean1.stdout)
        self.assertIn("delegate work show W001", clean1.stdout)

        # Card shows verify state, artifacts[], done_when, and runs[]
        card1 = json.loads(self.cli("work", "show", "W001", "--json", human=True).stdout)
        self.assertEqual(card1["state"], "verify")
        self.assertEqual(len(card1["artifacts"]), 2)
        self.assertIn(r1["run"], card1["runs"])
        self.assertIn("followups", card1)

        # 3. Start upstream run 007d (3D tab) and put W001 blocked waiting on run_applied:007d
        self.fake_pi([fixtures.answer("3D 独立标签页已完成"), fixtures.SETTLED], pre="echo 'export const tab = \"3d\";' > src/tab.ts")
        r007d = self.outcome(self.cli(
            "start", "--worktree", "--workdir", repo, "--name", "007d-tab", "--accept", "true", "3D 独立标签页"
        ))

        w_wait = self.cli(
            "work", "wait", "W001",
            "--after-applied", r007d["run"],
            "--brief", str(brief),
            human=True,
        )
        self.assertEqual(w_wait.returncode, 0, w_wait.stdout + w_wait.stderr)
        self.assertIn("blocked", w_wait.stdout)

        # Default `work` folds blocked into summary count; `--all` reveals W001
        tbl_default = self.cli("work", human=True)
        self.assertEqual(tbl_default.returncode, 0)
        self.assertIn("工作表：", tbl_default.stdout)
        self.assertIn("已折叠：等待 1", tbl_default.stdout)
        self.assertIn("delegate work show <ID>", tbl_default.stdout)

        # 4. Wait for 007d -> tail hint mentions downstream W001 waiting on apply
        wait_out = self.cli("wait", r007d["run"], human=True)
        self.assertEqual(wait_out.returncode, 0, wait_out.stdout + wait_out.stderr)
        self.assertIn("关联下游：[W001]", wait_out.stdout)
        self.assertIn("合入后解锁", wait_out.stdout)
        self.assertIn("delegate work show W001", wait_out.stdout)

        # 5. Apply 007d -> unlocks W001 immediately & prints executable start --work command
        apply_007d = self.cli("apply", r007d["run"], human=True)
        self.assertEqual(apply_007d.returncode, 0, apply_007d.stdout + apply_007d.stderr)
        self.assertIn("已解锁 [W001]", apply_007d.stdout)
        self.assertIn("delegate start --work W001 --prompt-file", apply_007d.stdout)
        self.assertIn("delegate work show W001", apply_007d.stdout)

        # 6. Dispatch final baton R3 -> apply R3 -> stays in verify because of done_when/artifacts
        self.fake_pi([fixtures.answer("已入库并接上 idle 循环"), fixtures.SETTLED], pre="echo 'export const idle = true;' > src/body.ts")
        r3 = self.outcome(self.cli(
            "run", "--worktree", "--workdir", repo, "--work", "W001", "--name", "body-wire",
            "--accept", "true", "--prompt-file", str(brief)
        ))
        apply_r3 = self.cli("apply", r3["run"], human=True)
        self.assertEqual(apply_r3.returncode, 0, apply_r3.stdout + apply_r3.stderr)
        self.assertIn("工作项 [W001] 留在 verify", apply_r3.stdout)
        self.assertIn("delegate work close W001 --evidence", apply_r3.stdout)

        # 7. Close requires --evidence (fail closed)
        bad_close = self.cli("work", "close", "W001", human=True)
        self.assertNotEqual(bad_close.returncode, 0)
        self.assertIn("--evidence", bad_close.stderr)

        # Explicit close with evidence transitions to done
        close_ok = self.cli("work", "close", "W001", "--evidence", "abc1234, 1440/390 明暗截图已验", human=True)
        self.assertEqual(close_ok.returncode, 0, close_ok.stdout + close_ok.stderr)
        self.assertIn("已关闭 [W001]", close_ok.stdout)

        final_card = self.cli("work", "show", "W001", human=True)
        self.assertEqual(final_card.returncode, 0)
        self.assertIn("状态：done", final_card.stdout)
        self.assertIn("abc1234, 1440/390 明暗截图已验", final_card.stdout)
        self.assertIn("原文：", final_card.stdout)

    def test_start_work_validation_and_reply_inherits_work_id(self):
        repo = self.repo({"a.txt": "v0\n"})
        # Unknown work ID fails closed
        bad = self.cli("start", "--read-only", "--workdir", repo, "--work", "W999", "hi", human=True)
        self.assertNotEqual(bad.returncode, 0)
        self.assertIn("W999", bad.stderr)

        self.assertEqual(self.cli("work", "add", "续做继承测试", "--repo", repo, human=True).returncode, 0)

        self.fake_pi([fixtures.answer("round 1"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--work", "W001", "--accept", "true", "step 1"))
        self.assertEqual(r1["workId"], "W001")

        self.fake_pi([fixtures.answer("round 2"), fixtures.SETTLED], pre="echo v2 > a.txt")
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "step 2"))
        self.assertEqual(r2["workId"], "W001")

        card = json.loads(self.cli("work", "show", "W001", "--json", human=True).stdout)
        self.assertIn(r1["run"], card["runs"])
        self.assertIn(r2["run"], card["runs"])

        # Simple item without done_when/artifacts/followups auto-closes on apply!
        self.assertEqual(self.cli("apply", r2["run"], human=True).returncode, 0)
        card_after = json.loads(self.cli("work", "show", "W001", "--json", human=True).stdout)
        self.assertEqual(card_after["state"], "done")
        self.assertEqual(card_after["closedKind"], "auto")

    def test_auto_dispatch_after_applied_for_work_and_queue(self):
        repo = self.repo({"a.txt": "v0\n", "b.txt": "v0\n"})
        brief = repo / ".local/plan/next-step.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("基于已合入的 a.txt 继续修改 b.txt\n")

        self.fake_pi([fixtures.answer("upstream done"), fixtures.SETTLED], pre="echo v1 > a.txt")
        up = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "up-a", "--accept", "true", "modify a"))

        # Add work item waiting on run_applied with --auto
        add = self.cli(
            "work", "add", "下游自动派发",
            "--repo", repo,
            "--after-applied", up["run"],
            "--brief", str(brief),
            "--auto", "--", "--read-only", "--workdir", repo,
            human=True,
        )
        self.assertEqual(add.returncode, 0, add.stdout + add.stderr)
        self.assertIn("blocked", add.stdout)

        # Also add a queue item with --after-applied
        qadd = self.cli(
            "queue", "add", "q-after-apply",
            "--after-applied", up["run"],
            "--prompt-file", str(brief),
            "--read-only", "--workdir", repo,
            human=True,
        )
        self.assertEqual(qadd.returncode, 0, qadd.stdout + qadd.stderr)
        self.assertIn("after-applied", qadd.stdout)
        self.assertIn("等待上游", qadd.stdout)

        # Prepare fake_pi for downstream auto-dispatched runs
        self.fake_pi([fixtures.answer("downstream auto done"), fixtures.SETTLED])

        # Apply upstream -> triggers BOTH work --auto AND queue --after-applied!
        applied = self.cli("apply", up["run"], human=True)
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertIn("已自动派发 [W001]", applied.stdout)

        card = json.loads(self.cli("work", "show", "W001", "--json", human=True).stdout)
        self.assertTrue(card["autoDispatch"]["dispatchedRun"], card)
        self.assertEqual(len(card["runs"]), 1)

        qlist = [json.loads(ln) for ln in self.cli("queue", "list", "--json", human=True).stdout.splitlines() if ln.strip()]
        self.assertEqual(qlist[0]["status"], "dispatched")
        self.assertTrue(qlist[0].get("runId"))

        # Wait for auto-dispatched runs to finish cleanly
        self.cli("wait", "--all", "--max", "15", human=True)

    def test_owner_readme_read_only_and_work_closed_and_drop_and_ephemeral_merge(self):
        repo = self.repo({"a.txt": "v0\n"})
        owner_readme = repo / "docs/owner/README.md"
        owner_readme.parent.mkdir(parents=True, exist_ok=True)
        initial_owner = (
            "# 所有者案头\n\n"
            "## 要你动手\n"
            "| 编号 | 做什么 |\n"
            "| --- | --- |\n"
            "| Q18 | 删掉旧星痕 |\n\n"
            "## 已否\n"
            "| 日期 | 编号 | 事项 |\n"
            "| --- | --- |\n"
        )
        owner_readme.write_text(initial_owner)
        orig_mtime_ns = owner_readme.stat().st_mtime_ns

        # W001 waits on owner:Q18
        self.assertEqual(
            self.cli("work", "add", "跟进 Q18", "--repo", repo, "--wait-owner", "Q18", human=True).returncode,
            0,
        )
        c1 = json.loads(self.cli("work", "show", "W001", "--json", human=True).stdout)
        self.assertEqual(c1["state"], "blocked")
        self.assertFalse(c1["waitOnStatus"][0]["satisfied"])

        # Verify docs/owner/README.md was NEVER written to!
        self.assertEqual(owner_readme.read_text(), initial_owner)
        self.assertEqual(owner_readme.stat().st_mtime_ns, orig_mtime_ns)

        # Simulate owner deciding & removing Q18 row -> W001 flips to ready automatically
        owner_readme.write_text(initial_owner.replace("| Q18 | 删掉旧星痕 |\n", ""))
        c1_after = json.loads(self.cli("work", "show", "W001", "--json", human=True).stdout)
        self.assertEqual(c1_after["state"], "ready")
        self.assertTrue(c1_after["waitOnStatus"][0]["satisfied"])

        # W002 waits on work_closed:W001; dropping W001 does NOT auto-unlock W002
        self.assertEqual(
            self.cli("work", "add", "W001之后", "--repo", repo, "--after-work", "W001", human=True).returncode,
            0,
        )
        # Cycle check rejects W001 waiting on W002
        cyc = self.cli("work", "wait", "W001", "--after-work", "W002", human=True)
        self.assertNotEqual(cyc.returncode, 0)
        self.assertIn("cycle", cyc.stderr.lower())

        drop_out = self.cli("work", "drop", "W001", "--reason", "方案取消", human=True)
        self.assertEqual(drop_out.returncode, 0, drop_out.stdout + drop_out.stderr)
        self.assertIn("不会自动解锁", drop_out.stdout)
        self.assertEqual(json.loads(self.cli("work", "show", "W002", "--json", human=True).stdout)["state"], "blocked")

        # Ephemeral unattached run appears in `delegate work` as `R:<short>` without creating a work.jsonl row
        self.fake_pi([fixtures.answer("adhoc write"), fixtures.SETTLED], pre="echo v1 > a.txt")
        adhoc = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "adhoc-fix", "--accept", "true", "fix"))
        tbl = self.cli("work", human=True)
        self.assertEqual(tbl.returncode, 0)
        self.assertIn("[R:", tbl.stdout)
        self.assertIn("adhoc-fix", tbl.stdout)
        for ln in tbl.stdout.splitlines():
            if ln.startswith("["):
                self.assertLessEqual(len(ln), 100, f"Row exceeds 100 chars: {ln}")

    def test_merge_tail_hints_and_apply_merge_unlocks_downstream(self):
        repo = self.repo({"a.txt": "v0\n", "b.txt": "v0\n"})
        self.fake_pi([fixtures.answer("a done"), fixtures.SETTLED], pre="echo a1 > a.txt")
        ra = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "feat-a", "--accept", "true", "a"))

        self.assertEqual(
            self.cli("work", "add", "合入后收尾", "--repo", repo, "--after-applied", ra["run"], human=True).returncode,
            0,
        )

        # Merge ra in isolated integration worktree -> tail hint mentions W001 will unlock after apply
        m = self.cli("merge", ra["run"], cwd=repo, human=True)
        self.assertEqual(m.returncode, 0, m.stdout + m.stderr)
        self.assertIn("合入后将解锁 [W001]", m.stdout)
        self.assertIn("delegate work show W001", m.stdout)

        # Apply the merge run -> unlocks W001!
        merges = [json.loads(ln) for ln in self.cli("merge", "list", "--json", cwd=repo, human=True).stdout.splitlines() if ln.strip()]
        merge_run = merges[-1]["mergeRun"]
        ap = self.cli("apply", "--keep-commits", merge_run, cwd=repo, human=True)
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)
        self.assertIn("已解锁 [W001]", ap.stdout)
        self.assertEqual(json.loads(self.cli("work", "show", "W001", "--json", human=True).stdout)["state"], "ready")

    def test_auto_migration_of_legacy_notes_and_deferred_tsv(self):
        repo = self.repo({"a.txt": "v0\n"})
        runs_dir = Path(self.env["DELEGATE_RUNS"])
        runs_dir.mkdir(parents=True, exist_ok=True)

        # Plant legacy notes.json and deferred.tsv BEFORE work.jsonl exists
        (runs_dir / "notes.json").write_text(json.dumps({
            "chains": [{
                "root": "20261122-100000-legacy",
                "runs": ["20261122-100000-legacy"],
                "name": "旧版遗留任务",
                "latestRun": "20261122-100000-legacy",
                "cleaned": True,
                "applied": True,
                "items": [{
                    "text": "检查旧日志",
                    "createdAt": "2026-11-22T02:00:00Z",
                    "done": False,
                }],
            }]
        }))
        def_path = repo / ".local/delegate/deferred.tsv"
        def_path.parent.mkdir(parents=True, exist_ok=True)
        def_path.write_text(f"20261123-090000-def1\t等维护窗口\t{int(time.time())}\n")

        # First `delegate work --all` auto-migrates both!
        res = self.cli("work", "--all", cwd=repo, human=True)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        self.assertIn("旧版遗留任务", res.stdout)
        self.assertIn("verify", res.stdout)
        self.assertIn("deferred", res.stdout)
        self.assertIn("等维护窗口", self.cli("work", "show", "W002", cwd=repo, human=True).stdout)

        # Thin-shell `delegate note` sees migrated note and completes it in work.jsonl
        note_list = self.cli("note", cwd=repo, human=True)
        self.assertIn("检查旧日志", note_list.stdout)
        self.assertEqual(self.cli("note", "20261122-100000-legacy", "--done", cwd=repo, human=True).returncode, 0)
        self.assertIn("无未完成待办", self.cli("note", cwd=repo, human=True).stdout)

    def test_import_md_dry_run_and_history_cross_check(self):
        repo = self.repo({"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("完成 007d 顶层空间"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r007d = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "007d", "--accept", "true", "3D 顶层空间"))
        self.assertEqual(self.cli("apply", r007d["run"], human=True).returncode, 0)

        md = repo / ".local/plan/sample-backlog.md"
        md.parent.mkdir(parents=True, exist_ok=True)
        md.write_text(
            "# 待办\n\n"
            "## 等前置条件\n"
            "- 3D 顶层空间（007d 链）合入后：派 .local/plan/brief-body-idle-scene.md。\n\n"
            "## 可派（排队）\n"
            "- 清理旧兼容列。\n\n"
            "## 要所有者定\n"
            "- D002：身体第二步是否改流式驱动。\n"
        )
        before_work = self.state_dir() / "work.jsonl"
        existed_before = before_work.exists()

        dry = self.cli("work", "import-md", str(md), "--dry-run", cwd=repo, human=True)
        self.assertEqual(dry.returncode, 0, dry.stdout + dry.stderr)
        self.assertIn("dry-run，未写入", dry.stdout)
        self.assertIn("run_applied:007d", dry.stdout)
        self.assertIn("owner:D002", dry.stdout)
        self.assertIn("疑似过期", dry.stdout)
        self.assertIn(f"{md.resolve()}:4", dry.stdout)
        # --dry-run must NOT create/modify work.jsonl
        if not existed_before:
            self.assertFalse(before_work.exists())

    def test_graded_stop_hook_and_old_run_compatibility(self):
        repo = self.repo({"a.txt": "v0\n"})
        # Create a synthetic pre-6.31 old run (no workId field in meta.json)
        self.fake_pi([fixtures.answer("old run ok"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r_old = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "legacy-feat", "--accept", "true", "old task"))
        meta_path = Path(r_old["dir"]) / "meta.json"
        meta = json.loads(meta_path.read_text())
        meta.pop("workId", None)
        meta_path.write_text(json.dumps(meta))

        # Consume delivery event cursor first
        self.cli("events", "--since", "0", human=True)
        latest_evs = [json.loads(ln) for ln in self.cli("events", "--json", human=True).stdout.splitlines() if ln.strip()]
        max_seq = max((e["seq"] for e in latest_evs), default=0)
        self.cli("wait", "--since", str(max_seq), "--max", "0.05", human=True)

        # Unmerged write run blocks Stop hook
        blk = self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False})
        self.assertIsNotNone(blk)
        self.assertEqual(blk["decision"], "block")
        self.assertIn("legacy-feat", blk["reason"])

        # Defer via `delegate work defer` -> Stop hook passes!
        defer_res = self.cli("work", "defer", r_old["run"], "--reason", "等集成测试窗口", "--until", "4h", human=True)
        self.assertEqual(defer_res.returncode, 0, defer_res.stdout + defer_res.stderr)
        self.assertIsNone(self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False}))

        # Add a blocked item, a ready item (with brief), and a fresh verify item (note) -> NONE of them block Stop hook!
        brief = repo / "docs/ready-brief.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# Ready Brief\n")
        self.assertEqual(self.cli("work", "add", "远期阻塞", "--repo", repo, "--after-time", "2099-01-01", human=True).returncode, 0)
        self.assertEqual(self.cli("work", "add", "积压就绪", "--repo", repo, "--brief", str(brief), human=True).returncode, 0)
        self.assertEqual(self.cli("apply", r_old["run"], human=True).returncode, 0)
        self.assertEqual(self.cli("note", r_old["run"], "合入后看一眼监控", human=True).returncode, 0)
        self.assertIsNone(self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False}))

        # Now emit a one-shot `work_ready` event -> Stop hook blocks ONCE until cursor advances!
        self.assertEqual(self.cli("work", "add", "等旧任务关闭", "--repo", repo, "--after-work", "W001", "--brief", str(brief), human=True).returncode, 0)
        # Closing W001 unlocks W004 and emits `work_ready`
        self.assertEqual(self.cli("work", "close", "W001", "--evidence", "verified", human=True).returncode, 0)
        ev_blk = self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False})
        self.assertIsNotNone(ev_blk)
        self.assertIn("工作项就绪:W004", ev_blk["reason"])

        # Reading via `wait --since` consumes cursor -> Stop hook passes!
        self.cli("wait", "--since", str(max_seq), "--max", "0.05", human=True)
        self.assertIsNone(self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False}))

    def test_reviewer_ro_guard_excludes_dep_build_and_gitignore_but_flags_tracked_and_unignored(self):
        repo = self.repo({
            ".gitignore": "*.log\n.review-cache/\n",
            ".delegate.json": json.dumps({"review": {"tier": "review"}}),
            "src/app.ts": "export const v = 0;\n",
        })

        # Custom scripted pi: call 1 = author edits src/app.ts;
        # call 2 = reviewer creates node_modules symlink, target, .venv, __pycache__, dist, test-results,
        #          and .gitignore-ignored files, then outputs PASS -> must stay PASS!
        # call 3 = author (second run) edits src/app.ts;
        # call 4 = reviewer creates node_modules + ALSO creates unignored new file & edits tracked file ->
        #          must fail closed to NEEDS_CONTROLLER listing ONLY tracked/unignored files!
        log_file = self.work / "ro-guard-calls.jsonl"
        self.env["RO_GUARD_LOG"] = str(log_file)
        pi = self.bin / "pi"
        pi.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "args = sys.argv[1:]\n"
            "if args == ['--version']:\n"
            "    print('fake-pi 1.0'); sys.exit(0)\n"
            "if len(args) >= 2 and args[1] == '--list-models':\n"
            "    print('provider model context max-out thinking images')\n"
            "    print('p m 1M 1K yes yes'); sys.exit(0)\n"
            "sys.stdin.read()\n"
            "lp = Path(os.environ['RO_GUARD_LOG'])\n"
            "idx = len(lp.read_text().splitlines()) + 1 if lp.is_file() else 1\n"
            "with lp.open('a') as fh: fh.write(json.dumps({'call': idx}) + '\\n')\n"
            "def answer(t):\n"
            "    print(json.dumps({'type':'message_end','message':{'role':'assistant','stopReason':'stop','model':'fake','usage':{'input':1,'output':1,'cacheRead':0},'content':[{'type':'text','text':t}]}}))\n"
            "    print(json.dumps({'type':'agent_settled'}))\n"
            "if idx in (1, 3):\n"
            "    Path('src/app.ts').write_text(f'export const v = {idx};\\n')\n"
            "    answer('implemented')\n"
            "elif idx == 2:\n"
            "    Path('apps/web/node_modules/pkg').mkdir(parents=True, exist_ok=True)\n"
            "    Path('apps/web/node_modules/pkg/index.js').write_text('ok')\n"
            "    os.symlink('pkg', 'apps/web/node_modules/pkg-link')\n"
            "    Path('target/debug').mkdir(parents=True, exist_ok=True)\n"
            "    Path('target/debug/out').write_text('bin')\n"
            "    Path('.venv/lib').mkdir(parents=True, exist_ok=True)\n"
            "    Path('.venv/lib/x.py').write_text('')\n"
            "    Path('pkg/__pycache__').mkdir(parents=True, exist_ok=True)\n"
            "    Path('pkg/__pycache__/a.pyc').write_text('pyc')\n"
            "    Path('dist').mkdir(parents=True, exist_ok=True)\n"
            "    Path('dist/bundle.js').write_text('js')\n"
            "    Path('test-results').mkdir(parents=True, exist_ok=True)\n"
            "    Path('test-results/shot.png').write_text('png')\n"
            "    Path('pnpm-debug.log').write_text('log')\n"
            "    Path('.review-cache').mkdir(parents=True, exist_ok=True)\n"
            "    Path('.review-cache/tmp').write_text('cache')\n"
            "    answer('PASS\\nClean implementation.')\n"
            "elif idx == 4:\n"
            "    Path('apps/web/node_modules/pkg').mkdir(parents=True, exist_ok=True)\n"
            "    Path('apps/web/node_modules/pkg/index.js').write_text('ok')\n"
            "    Path('src/app.ts').write_text('export const v = 999;\\n')\n"
            "    Path('src/sneaky.ts').write_text('export const bad = true;\\n')\n"
            "    answer('PASS\\nLooks good.')\n"
        )
        pi.chmod(0o755)

        # Run 1: reviewer only touches dep/build/.gitignore dirs -> PASS preserved!
        r_ok = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "rev-clean-deps", "--accept", "true", "edit app")
        )
        self.assertEqual(r_ok["review"]["verdict"], "PASS", r_ok["review"])
        self.assertNotIn("readOnlyViolation", r_ok["review"])
        self.assertNotIn("roViolationCount", r_ok["review"])

        # Run 2: reviewer touches tracked src/app.ts + unignored src/sneaky.ts + node_modules ->
        # upgraded to NEEDS_CONTROLLER, listing ONLY src/app.ts and src/sneaky.ts!
        r_bad = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "rev-real-write", "--accept", "true", "edit app 2")
        )
        self.assertEqual(r_bad["review"]["verdict"], "NEEDS_CONTROLLER", r_bad["review"])
        self.assertEqual(r_bad["review"]["upgradedBy"], "readOnlyViolation", r_bad["review"])
        self.assertEqual(r_bad["review"]["roViolationCount"], 2, r_bad["review"])
        self.assertEqual(
            sorted(r_bad["review"]["roViolationSample"]),
            ["src/app.ts", "src/sneaky.ts"],
            r_bad["review"],
        )
        self.assertEqual(
            sorted(Path(r_bad["review"]["roViolationFile"]).read_text().splitlines()),
            ["src/app.ts", "src/sneaky.ts"],
        )


if __name__ == "__main__":
    unittest.main()
