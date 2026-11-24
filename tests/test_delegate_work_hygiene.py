"""Work-hygiene mechanism tests:
1. Default stale thresholds (verify 4h, ready 8h) and 6h cooldown.
2. Stop hook low-priority work reminder: only when no higher-priority block AND repo has idle slot;
   picks up to 3 oldest stale ready (excluding owner items) / verify / missing-brief ready items;
   verbatim block text; `<state>/work-remind.json` 6h per-item cooldown; `stop_hook_active=true` passes.
3. `work add --needs-owner <nod|act|card>` creates Qxx + `wait_on: ["owner:Qxx"]` -> blocked;
   owner/wait keyword warning on title/done_when without `--needs-owner`/`--wait-*`/`--after-*`.
4. Missing-brief ready items (`add` / `import-md` without `--brief`) listed as `缺说明` in Stop hook,
   cleared after `work wait Wxx --brief FILE`.
5. `verify_on` (`run_applied:`, `work_closed:`, `owner:`, `commit_exists:<sha>`; rejects `cmd:`)
   auto-closes `verify` with per-condition evidence; `verify` without `verify_on` does NOT auto-close on apply.
6. `hook stats` attribution for `close` / `wait_on` / `defer` (`work` hook);
   3 consecutive defers mark `反复暂缓` on the work table (display only, never blocks).
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

EXPECTED_HINT_LINE = (
    '可选：派发 `delegate start --work Wxx …` · '
    '等人或等运行 `delegate work wait Wxx --wait-manual "…"|--after-applied RUN` · '
    '暂缓 `delegate work defer Wxx --until … --reason "…"` · '
    '关闭 `delegate work close Wxx --evidence "…"`'
)
EXPECTED_KEYWORD_WARN = (
    '这项像要所有者点头或等前置，仍放进就绪；若是，加 --needs-owner nod 或 --wait-manual "…"'
)


def iso_ago(hours: float) -> str:
    ts = int(time.time() - hours * 3600)
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts))


class DelegateWorkHygieneTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def write_work_jsonl(self, items: list[dict]) -> None:
        sdir = self.state_dir()
        sdir.mkdir(parents=True, exist_ok=True)
        (sdir / "work.jsonl").write_text(
            "".join(json.dumps(it, ensure_ascii=False) + "\n" for it in items),
            encoding="utf-8",
        )

    def run_stop_hook(self, payload: dict, env: dict | None = None):
        run_env = {
            **self.env,
            "DELEGATE_BIN": str(fixtures.DELEGATE),
            "DELEGATE_STOP_HOOK_WAITERS": "",
            "DELEGATE_STOP_HOOK_PROCS": "",
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

    def test_stop_hook_thresholds_max3_cooldown_and_missing_brief(self):
        repo = self.repo({"a.txt": "v0\n"})
        brief = repo / "docs/spec.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# Spec\n")

        self.write_work_jsonl([
            # Owner item (must be excluded even if very old & no brief)
            {
                "id": "Q01",
                "title": "所有者审批项",
                "repo": str(repo),
                "state": "ready",
                "owner": {"kind": "nod", "ask": "请点头"},
                "stateSince": iso_ago(30),
                "createdAt": iso_ago(30),
                "updatedAt": iso_ago(30),
            },
            # Fresh verify (3h < 4h) -> not stale
            {
                "id": "W001",
                "title": "新鲜待闭环",
                "repo": str(repo),
                "state": "verify",
                "doneWhen": "检查",
                "stateSince": iso_ago(3),
                "createdAt": iso_ago(3),
                "updatedAt": iso_ago(3),
            },
            # Stale verify (5h >= 4h) -> eligible (待闭环)
            {
                "id": "W002",
                "title": "老化待闭环",
                "repo": str(repo),
                "state": "verify",
                "doneWhen": "检查",
                "stateSince": iso_ago(5.2),
                "createdAt": iso_ago(5.2),
                "updatedAt": iso_ago(5.2),
            },
            # Fresh ready with brief (7h < 8h) -> not stale
            {
                "id": "W003",
                "title": "新鲜带说明就绪",
                "repo": str(repo),
                "state": "ready",
                "brief": str(brief),
                "stateSince": iso_ago(7),
                "createdAt": iso_ago(7),
                "updatedAt": iso_ago(7),
            },
            # Stale ready with brief (10h >= 8h) -> eligible (就绪)
            {
                "id": "W004",
                "title": "老化就绪十小时",
                "repo": str(repo),
                "state": "ready",
                "brief": str(brief),
                "stateSince": iso_ago(10.2),
                "createdAt": iso_ago(10.2),
                "updatedAt": iso_ago(10.2),
            },
            # Oldest stale ready with brief (12h >= 8h) -> eligible (就绪)
            {
                "id": "W005",
                "title": "最老就绪十二小时",
                "repo": str(repo),
                "state": "ready",
                "brief": str(brief),
                "stateSince": iso_ago(12.2),
                "createdAt": iso_ago(12.2),
                "updatedAt": iso_ago(12.2),
            },
            # Missing-brief ready (1h < 8h) -> eligible as 缺说明
            {
                "id": "W006",
                "title": "缺说明就绪项",
                "repo": str(repo),
                "state": "ready",
                "stateSince": iso_ago(1.2),
                "createdAt": iso_ago(1.2),
                "updatedAt": iso_ago(1.2),
            },
        ])

        # stop_hook_active=True must pass immediately without touching work-remind.json
        self.assertIsNone(self.run_stop_hook({"cwd": str(repo), "stop_hook_active": True}))
        self.assertFalse((self.state_dir() / "work-remind.json").exists())

        # First run: picks 3 oldest (W005 12h, W004 10h, W002 5h), exact verbatim format
        blk1 = self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False})
        self.assertIsNotNone(blk1)
        self.assertEqual(blk1["decision"], "block")
        lines = blk1["reason"].splitlines()
        self.assertEqual(lines[0], "工作表有 3 项停着没动，本回合给每项一个去向：", lines)
        self.assertEqual(lines[1], "[W005] 最老就绪十二小时 · 停 12 小时 · 就绪", lines)
        self.assertEqual(lines[2], "[W004] 老化就绪十小时 · 停 10 小时 · 就绪", lines)
        self.assertEqual(lines[3], "[W002] 老化待闭环 · 停 5 小时 · 待闭环", lines)
        self.assertEqual(lines[4], EXPECTED_HINT_LINE, lines)
        self.assertEqual(len(lines), 5, lines)
        self.assertNotIn("Q01", blk1["reason"])
        self.assertNotIn("W001", blk1["reason"])
        self.assertNotIn("W003", blk1["reason"])

        remind_doc = json.loads((self.state_dir() / "work-remind.json").read_text(encoding="utf-8"))
        self.assertEqual(set(remind_doc.keys()), {"W005", "W004", "W002"})

        # Second run immediately: W005/W004/W002 are in 6h cooldown -> now lists remaining W006 (缺说明)
        blk2 = self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False})
        self.assertIsNotNone(blk2)
        lines2 = blk2["reason"].splitlines()
        self.assertEqual(lines2[0], "工作表有 1 项停着没动，本回合给每项一个去向：", lines2)
        self.assertEqual(lines2[1], "[W006] 缺说明就绪项 · 停 1 小时 · 缺说明", lines2)
        self.assertEqual(lines2[2], EXPECTED_HINT_LINE, lines2)

        # Third run immediately: all 4 eligible items are within 6h cooldown -> passes!
        self.assertIsNone(self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False}))

        # Backdate W006's remind time past 6h -> re-triggers; then fill brief via `work wait W006 --brief` -> clears!
        remind_doc = json.loads((self.state_dir() / "work-remind.json").read_text(encoding="utf-8"))
        remind_doc["W006"] = iso_ago(6.1)
        (self.state_dir() / "work-remind.json").write_text(json.dumps(remind_doc), encoding="utf-8")
        blk3 = self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False})
        self.assertIsNotNone(blk3)
        self.assertIn("[W006]", blk3["reason"])

        # Add brief to W006 and expire cooldown again -> no longer flagged as 缺说明 (and 1h < 8h)
        upd = self.cli("work", "wait", "W006", "--brief", str(brief), human=True)
        self.assertEqual(upd.returncode, 0, upd.stdout + upd.stderr)
        # Consume any `work_stale` events emitted by `work wait`'s reconcile pass
        ev_out = self.cli("events", "--since", "0", human=True)
        self.assertIn("W002", ev_out.stdout)
        self.assertIn("W004", ev_out.stdout)
        self.assertIn("W005", ev_out.stdout)
        remind_doc["W006"] = iso_ago(7.0)
        (self.state_dir() / "work-remind.json").write_text(json.dumps(remind_doc), encoding="utf-8")
        self.assertIsNone(self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False}))

    def test_stop_hook_only_triggers_when_idle_and_no_higher_priority_block(self):
        repo = self.repo({"a.txt": "v0\n"})
        brief = repo / "docs/spec.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# Spec\n")

        self.write_work_jsonl([
            {
                "id": "W001",
                "title": "久置就绪项",
                "repo": str(repo),
                "state": "ready",
                "brief": str(brief),
                "stateSince": iso_ago(10),
                "createdAt": iso_ago(10),
                "updatedAt": iso_ago(10),
            }
        ])

        # 1) Higher-priority block (unmerged write run) suppresses work-hygiene reminder
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r_unmerged = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "unmerged-job", "--accept", "true", "edit")
        )
        # Consume events cursor so unmerged is the active blocker
        self.cli("events", "--since", "0", human=True)
        blk_hi = self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False})
        self.assertIsNotNone(blk_hi)
        self.assertIn("unmerged-job", blk_hi["reason"])
        self.assertNotIn("工作表有", blk_hi["reason"])
        self.assertFalse((self.state_dir() / "work-remind.json").exists())

        # Apply the unmerged run so no higher-priority block remains
        self.assertEqual(self.cli("apply", r_unmerged["run"], human=True).returncode, 0)
        self.cli("events", "--since", "0", human=True)

        # 2) Repo at capacity (1 running run with waiter covered + repoMaxActive=1) -> NO idle slot -> passes!
        runs_dir = Path(self.env["DELEGATE_RUNS"])
        fake_run = runs_dir / "20261124-100000-busy"
        fake_run.mkdir(parents=True, exist_ok=True)
        (fake_run / "meta.json").write_text(
            json.dumps({
                "run": "20261124-100000-busy",
                "name": "busy",
                "mode": "write",
                "startedEpoch": time.time(),
            })
        )
        # Use current python PID so pid_alive() is True
        (fake_run / "pid").write_text(f"{os.getpid()}\n")

        (repo / ".delegate.json").write_text(json.dumps({"remote": False, "repoMaxActive": 1}))
        blk_busy = self.run_stop_hook(
            {"cwd": str(repo), "stop_hook_active": False},
            env={"DELEGATE_STOP_HOOK_WAITERS": "delegate wait"},
        )
        self.assertIsNone(blk_busy, f"must not trigger when repo has no idle slot: {blk_busy}")
        self.assertFalse((self.state_dir() / "work-remind.json").exists())

        # 3) Queued/waiting run in repo also means no idle slot even if repoMaxActive=0
        (repo / ".delegate.json").write_text(json.dumps({"remote": False}))
        (fake_run / ".waiting").write_text("")
        blk_q = self.run_stop_hook(
            {"cwd": str(repo), "stop_hook_active": False},
            env={"DELEGATE_STOP_HOOK_WAITERS": "delegate wait"},
        )
        self.assertIsNone(blk_q, f"must not trigger when repo has queued run: {blk_q}")

        # 4) Remove waiting marker & allow capacity (repoMaxActive=2) -> idle slot available -> triggers!
        (fake_run / ".waiting").unlink()
        (repo / ".delegate.json").write_text(json.dumps({"remote": False, "repoMaxActive": 2}))
        blk_idle = self.run_stop_hook(
            {"cwd": str(repo), "stop_hook_active": False},
            env={"DELEGATE_STOP_HOOK_WAITERS": "delegate wait"},
        )
        self.assertIsNotNone(blk_idle)
        self.assertIn("[W001] 久置就绪项", blk_idle["reason"])

    def test_needs_owner_and_keyword_warning(self):
        repo = self.repo({"a.txt": "v0\n"})

        # 1) --needs-owner nod creates Q01 + W001 blocked on owner:Q01 without keyword warning
        add_need = self.cli("work", "add", "对外发布审批", "--repo", repo, "--needs-owner", "nod", "--json", human=True)
        self.assertEqual(add_need.returncode, 0, add_need.stdout + add_need.stderr)
        self.assertNotIn(EXPECTED_KEYWORD_WARN, add_need.stderr)
        w1 = json.loads(add_need.stdout)
        self.assertEqual(w1["id"], "W001")
        self.assertEqual(w1["state"], "blocked")
        self.assertEqual(w1["waitOn"], ["owner:Q01"])

        q1 = json.loads(self.cli("work", "show", "Q01", "--json", human=True).stdout)
        self.assertEqual(q1["owner"]["kind"], "nod")
        self.assertEqual(q1["state"], "attention")

        # Closing Q01 unlocks W001 to ready
        self.assertEqual(self.cli("work", "close", "Q01", "--evidence", "所有者点头", human=True).returncode, 0)
        w1_after = json.loads(self.cli("work", "show", "W001", "--json", human=True).stdout)
        self.assertEqual(w1_after["state"], "ready")

        # Invalid --needs-owner value fails closed
        bad_need = self.cli("work", "add", "无效类型", "--repo", repo, "--needs-owner", "direction", human=True)
        self.assertNotEqual(bad_need.returncode, 0)

        # 2) Keyword in title without --needs-owner / --wait-* / --after-* prints warning, does NOT reject
        warn_title = self.cli("work", "add", "删除旧数据表", "--repo", repo, human=True)
        self.assertEqual(warn_title.returncode, 0, warn_title.stdout + warn_title.stderr)
        self.assertIn(EXPECTED_KEYWORD_WARN, warn_title.stderr)
        w2 = json.loads(self.cli("work", "show", "W002", "--json", human=True).stdout)
        self.assertEqual(w2["state"], "ready")

        # 3) Keyword in --done-when also warns, still enters ready
        warn_dw = self.cli("work", "add", "更新客户端", "--repo", repo, "--done-when", "真机测试通过", human=True)
        self.assertEqual(warn_dw.returncode, 0, warn_dw.stdout + warn_dw.stderr)
        self.assertIn(EXPECTED_KEYWORD_WARN, warn_dw.stderr)
        w3 = json.loads(self.cli("work", "show", "W003", "--json", human=True).stdout)
        self.assertEqual(w3["state"], "ready")

        # 4) Keyword with --wait-manual does NOT warn
        no_warn = self.cli(
            "work", "add", "不可逆迁移", "--repo", repo, "--wait-manual", "等维护窗口", human=True
        )
        self.assertEqual(no_warn.returncode, 0, no_warn.stdout + no_warn.stderr)
        self.assertNotIn(EXPECTED_KEYWORD_WARN, no_warn.stderr)
        w4 = json.loads(self.cli("work", "show", "W004", "--json", human=True).stdout)
        self.assertEqual(w4["state"], "blocked")

    def test_verify_on_auto_close_and_verify_without_verify_on_stays_open(self):
        repo = self.repo({"a.txt": "v0\n", "b.txt": "v0\n"})
        head_sha = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()[:7]

        # Reject cmd: in --verify-on
        bad_cmd = self.cli("work", "add", "非法条件", "--repo", repo, "--verify-on", "cmd:true", human=True)
        self.assertNotEqual(bad_cmd.returncode, 0)
        self.assertIn("cmd:", bad_cmd.stderr)

        # W001: has done_when, NO verify_on -> write run applied stays in verify (not auto-closed)
        self.assertEqual(
            self.cli("work", "add", "无自动闭环条件项", "--repo", repo, "--done-when", "人工验收", human=True).returncode,
            0,
        )
        self.fake_pi([fixtures.answer("edit a"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--work", "W001", "--accept", "true", "edit a")
        )
        ap1 = self.cli("apply", r1["run"], human=True)
        self.assertEqual(ap1.returncode, 0, ap1.stdout + ap1.stderr)
        w1 = json.loads(self.cli("work", "show", "W001", "--json", human=True).stdout)
        self.assertEqual(w1["state"], "verify")

        # Create owner item Q01 for W002's verify_on
        self.assertEqual(
            self.cli("work", "add", "--owner-kind", "nod", "--ask", "上线点头", "--repo", repo, human=True).returncode,
            0,
        )
        # W002: has verify_on = [run_applied:r1, commit_exists:<head>, owner:Q01]
        self.assertEqual(
            self.cli(
                "work", "add", "自动闭环验证项",
                "--repo", repo,
                "--verify-on", f"run_applied:{r1['run']}",
                "--verify-on", f"commit_exists:{head_sha}",
                "--verify-on", "owner:Q01",
                human=True,
            ).returncode,
            0,
        )
        # Dispatch write run for W002 and apply -> enters verify (Q01 not closed yet)
        self.fake_pi([fixtures.answer("edit b"), fixtures.SETTLED], pre="echo v1 > b.txt")
        r2 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--work", "W002", "--accept", "true", "edit b")
        )
        ap2 = self.cli("apply", r2["run"], human=True)
        self.assertEqual(ap2.returncode, 0, ap2.stdout + ap2.stderr)
        w2_mid = json.loads(self.cli("work", "show", "W002", "--json", human=True).stdout)
        self.assertEqual(w2_mid["state"], "verify")

        # Close Q01 -> reconcile satisfies all verify_on -> W002 auto-closes with evidence!
        self.assertEqual(self.cli("work", "close", "Q01", "--evidence", "已点头", human=True).returncode, 0)
        w2_done = json.loads(self.cli("work", "show", "W002", "--json", human=True).stdout)
        self.assertEqual(w2_done["state"], "done")
        self.assertEqual(w2_done["closedKind"], "auto")
        self.assertIn(f"run_applied:{r1['run']}", w2_done["closeEvidence"])
        self.assertIn(f"commit_exists:{head_sha}", w2_done["closeEvidence"])
        self.assertIn("owner:Q01", w2_done["closeEvidence"])

    def test_repeated_defer_3x_and_hook_stats_attribution(self):
        repo = self.repo({"a.txt": "v0\n"})
        brief = repo / "docs/spec.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# Spec\n")

        self.write_work_jsonl([
            {
                "id": "W001",
                "title": "项一将关闭",
                "repo": str(repo),
                "state": "verify",
                "doneWhen": "检查",
                "stateSince": iso_ago(6),
                "createdAt": iso_ago(6),
                "updatedAt": iso_ago(6),
            },
            {
                "id": "W002",
                "title": "项二将挂前置",
                "repo": str(repo),
                "state": "ready",
                "brief": str(brief),
                "stateSince": iso_ago(10),
                "createdAt": iso_ago(10),
                "updatedAt": iso_ago(10),
            },
            {
                "id": "W003",
                "title": "项三将反复暂缓",
                "repo": str(repo),
                "state": "ready",
                "brief": str(brief),
                "stateSince": iso_ago(11),
                "createdAt": iso_ago(11),
                "updatedAt": iso_ago(11),
            },
        ])

        # Trigger Stop hook -> records `hook: "work"` in stop-hook.jsonl
        blk = self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False})
        self.assertIsNotNone(blk)
        self.assertIn("W001", blk["reason"])
        self.assertIn("W002", blk["reason"])
        self.assertIn("W003", blk["reason"])

        # Follow up with close (W001), wait_on (W002), and 3x defer (W003)
        self.assertEqual(self.cli("work", "close", "W001", "--evidence", "已验", human=True).returncode, 0)
        self.assertEqual(self.cli("work", "wait", "W002", "--wait-manual", "等外部接口", human=True).returncode, 0)
        for n in range(1, 4):
            dres = self.cli("work", "defer", "W003", "--reason", f"第{n}次延后", "--until", "4h", human=True)
            self.assertEqual(dres.returncode, 0, dres.stdout + dres.stderr)

        # W003 deferred 3 times -> marked 反复暂缓 in table and card, never blocked
        tbl = self.cli("work", "--all", human=True)
        self.assertIn("反复暂缓", tbl.stdout)
        card = self.cli("work", "show", "W003", human=True)
        self.assertIn("反复暂缓（连续 3 次）", card.stdout)

        # Verify `hook stats` attributes close / wait_on / defer for the work hook!
        # Since one block listed W001/W002/W003, add two separate per-item work blocks to verify all 3 outcomes independently
        now_iso = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() - 30))
        with (self.state_dir() / "stop-hook.jsonl").open("w", encoding="utf-8") as fh:
            for wid in ("W001", "W002", "W003"):
                fh.write(json.dumps({"ts": now_iso, "hook": "work", "runs": wid, "workIds": [wid]}) + "\n")

        stats_json = json.loads(self.cli("hook", "stats", "--json", human=True).stdout)
        self.assertEqual(stats_json["byHook"].get("work"), 3, stats_json)
        self.assertEqual(stats_json["outcomes"]["closed"], 1, stats_json)
        self.assertEqual(stats_json["outcomes"]["waitedOn"], 1, stats_json)
        self.assertEqual(stats_json["outcomes"]["deferred"], 1, stats_json)

    def test_import_md_missing_brief_marked_and_cleared(self):
        repo = self.repo({"a.txt": "v0\n"})
        md = repo / "backlog.md"
        md.write_text("# 待办\n\n## 可派\n- 清理无说明旧接口。\n", encoding="utf-8")
        imp = self.cli("work", "import-md", str(md), cwd=repo, human=True)
        self.assertEqual(imp.returncode, 0, imp.stdout + imp.stderr)

        # Imported ready item without brief is listed as 缺说明 in Stop hook
        blk = self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False})
        self.assertIsNotNone(blk)
        self.assertIn("缺说明", blk["reason"])
        self.assertIn("清理无说明旧接口", blk["reason"])

        # Add brief via `work wait W001 --brief` -> clears missing-brief mark
        brief = repo / "docs/brief-cleanup.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# 清理计划\n", encoding="utf-8")
        self.assertEqual(self.cli("work", "wait", "W001", "--brief", str(brief), human=True).returncode, 0)
        # Remove remind file to test fresh evaluation
        (self.state_dir() / "work-remind.json").unlink(missing_ok=True)
        self.assertIsNone(self.run_stop_hook({"cwd": str(repo), "stop_hook_active": False}))


if __name__ == "__main__":
    unittest.main()
