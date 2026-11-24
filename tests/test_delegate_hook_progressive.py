"""Progressive Stop hook, shared pending classification, stall-chain escalation,
and multi-format (`claude` / `agy` / `json`) hook kernel tests.

Covers:
A. Shared actionable-pending rule (`tests/fixtures/pending_classification.json`)
   matching between `delegate-tui` and `delegate hook pending --json`.
B. `delegate hook pending` prioritized Layer-2 backlog snapshot (human + JSON).
C. Single-item progressive Stop hook with all 7 verbatim templates, >40-char title
   truncation, multi-category priority selection + `另有 N 件` collapse, and zero
   `--wait-manual` references.
D. Missing-brief ready work items: 1h grace period (silent within 1h, alerts past 1h).
E. Work-item cooldown: only the single displayed work item is recorded in
   `work-remind.json`; collapsed items are never prematurely marked reminded.
F. Stall-chain escalation (30m cross-repo/cross-session penetration along `parent`
   chain, `verify` 30m threshold without `has_idle_slot` gating, `blocked` items
   never alerting, and `delegate work defer` clearing the block).
G. `delegate hook check --format claude|agy|json` entry guards (colleague env vars,
   anti-loop flags, `agy` `terminationReason`), output shapes, and `agy` waiter hint.
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
SHARED_FIXTURE = ROOT / "tests/fixtures/pending_classification.json"


def iso_ago(hours: float) -> str:
    ts = int(time.time() - hours * 3600)
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts))


def iso_ago_mins(mins: float) -> str:
    return iso_ago(mins / 60.0)


class DelegateHookProgressiveTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def clean_caller_env(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        strip = set(fixtures.CALLER_ENV) | {
            "ANTIGRAVITY_CONVERSATION_ID",
            "ANTIGRAVITY_AGENT",
            "PI_DELEGATE_ACTIVE",
            "DELEGATE_AGENT",
            "DELEGATE_RUN_DIR",
        }
        env = {k: v for k, v in self.env.items() if k not in strip}
        if extra:
            env.update(extra)
        return env

    def run_hook_check(
        self,
        fmt: str,
        payload: dict,
        env: dict[str, str] | None = None,
        extra_env: dict[str, str] | None = None,
        cwd: Path | None = None,
    ) -> subprocess.CompletedProcess:
        henv = {
            **(env if env is not None else self.clean_caller_env()),
            "DELEGATE_BIN": str(fixtures.DELEGATE),
            "DELEGATE_STOP_HOOK_WAITERS": "",
            "DELEGATE_STOP_HOOK_PROCS": "",
            "DELEGATE_STOP_HOOK_CURRENT_ONLY": "1",
        }
        if extra_env:
            henv.update(extra_env)
        return subprocess.run(
            [str(fixtures.DELEGATE), "hook", "check", "--format", fmt],
            input=json.dumps(payload),
            cwd=cwd,
            capture_output=True,
            text=True,
            env=henv,
            timeout=15,
        )

    def run_stop_wrapper(
        self,
        payload: dict,
        env: dict[str, str] | None = None,
        extra_env: dict[str, str] | None = None,
    ) -> dict | None:
        henv = {
            **(env if env is not None else self.clean_caller_env()),
            "DELEGATE_BIN": str(fixtures.DELEGATE),
            "DELEGATE_STOP_HOOK_WAITERS": "",
            "DELEGATE_STOP_HOOK_PROCS": "",
            "DELEGATE_STOP_HOOK_CURRENT_ONLY": "1",
        }
        if extra_env:
            henv.update(extra_env)
        res = subprocess.run(
            [sys.executable, str(STOP_HOOK)],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            env=henv,
            timeout=15,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        out = res.stdout.strip()
        return json.loads(out) if out else None

    def write_work_jsonl(self, items: list[dict]) -> None:
        sdir = self.state_dir()
        sdir.mkdir(parents=True, exist_ok=True)
        (sdir / "work.jsonl").write_text(
            "".join(json.dumps(it, ensure_ascii=False) + "\n" for it in items),
            encoding="utf-8",
        )

    def materialize_run(
        self,
        runs_dir: Path,
        repo: Path,
        item: dict,
        live_pid: int | None = None,
    ) -> Path:
        rid = item["run"]
        rdir = runs_dir / rid
        rdir.mkdir(parents=True, exist_ok=True)
        meta: dict = {
            "run": rid,
            "name": item.get("name", rid),
            "mode": item.get("mode", "write"),
            "workdir": str(repo),
            "top": str(repo),
            "startedAt": item.get("startedAt") or iso_ago(2.0),
            "startedEpoch": int(time.time() - 7200),
        }
        if item.get("parent"):
            meta["parent"] = item["parent"]
        if item.get("caller"):
            meta["caller"] = item["caller"]
        if item.get("resultReadAt"):
            meta["resultReadAt"] = item["resultReadAt"]
        if item.get("workId"):
            meta["workId"] = item["workId"]
        if item.get("applied"):
            meta["appliedAt"] = item.get("appliedAt") or iso_ago(1.0)
            (rdir / ".applied").write_text(json.dumps({"appliedBy": "delegate"}) + "\n")
        (rdir / "meta.json").write_text(json.dumps(meta), encoding="utf-8")

        st = item.get("state", "delivered")
        if st in ("running", "starting", "waiting", "queued"):
            if st in ("waiting", "queued"):
                (rdir / ".waiting").write_text("")
            else:
                (rdir / "pid").write_text(f"{live_pid or os.getpid()}\n")
        else:
            summary: dict = {
                "state": st,
                "finishedAt": item.get("finishedAt") or iso_ago(1.0),
            }
            if item.get("question"):
                summary["question"] = True
            if "changes" in item:
                summary["changes"] = item["changes"]
            if "pendingChanges" in item:
                summary["pendingChanges"] = item["pendingChanges"]
            (rdir / "summary.json").write_text(json.dumps(summary), encoding="utf-8")
            (rdir / "exit_code").write_text("0\n" if st in ("delivered", "answered") else "1\n")

        if item.get("premerge"):
            (rdir / "premerge.json").write_text(json.dumps(item["premerge"]), encoding="utf-8")

        return rdir

    def write_index_for_runs(self, repo: Path, runs_dir: Path, items: list[dict]) -> None:
        sdir = self.state_dir()
        sdir.mkdir(parents=True, exist_ok=True)
        runs_map = {}
        for item in items:
            rdir = (runs_dir / item["run"]).resolve()
            st_obj = dict(item)
            st_obj["dir"] = str(rdir)
            meta_obj = {
                "resultReadAt": item.get("resultReadAt", ""),
                "appliedAt": item.get("appliedAt", ""),
                "repliedAt": item.get("repliedAt", ""),
                "appliedCleaned": False,
            }
            runs_map[str(rdir)] = {
                "repo": str(repo.resolve()),
                "runsRoot": str(runs_dir.resolve()),
                "startedNs": 1,
                "sig": "fixture",
                "status": st_obj,
                "meta": meta_obj,
            }
        (sdir / "index.json").write_text(
            json.dumps({"schemaVersion": 1, "updatedAt": iso_ago(0.01), "seq": 0, "runs": runs_map}),
            encoding="utf-8",
        )

    # ------------------------------------------------------------------
    # A. Shared actionable-pending fixture consistency
    # ------------------------------------------------------------------
    def test_a_shared_pending_fixture_matches_hook_and_snapshot(self):
        doc = json.loads(SHARED_FIXTURE.read_text(encoding="utf-8"))
        repo = self.repo({"a.txt": "v0\n"})
        runs_dir = Path(self.env["DELEGATE_RUNS"])

        for item in doc["runs"]:
            self.materialize_run(runs_dir, repo, item)
        self.write_index_for_runs(repo, runs_dir, doc["runs"])

        res = subprocess.run(
            [str(fixtures.DELEGATE), "hook", "pending", "--json", "--cwd", str(repo)],
            capture_output=True,
            text=True,
            env={
                **self.clean_caller_env(),
                "DELEGATE_STOP_HOOK_CURRENT_ONLY": "1",
                "DELEGATE_STOP_HOOK_WAITERS": "",
                "DELEGATE_STOP_HOOK_PROCS": "",
            },
            timeout=15,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        pending_doc = json.loads(res.stdout)
        self.assertEqual(pending_doc["pendingRunCount"], doc["expectedPendingCount"])
        self.assertEqual(sorted(pending_doc["pendingRuns"]), sorted(doc["expectedPendingRuns"]))

        # Verify `delegate snapshot --json` also exposes `resultReadAt` for TUI classification.
        snap_res = self.cli("snapshot", "--json", "--repo", str(repo), cwd=repo, human=True)
        self.assertEqual(snap_res.returncode, 0, snap_res.stderr)
        snap = json.loads(snap_res.stdout)
        snap_by_id = {
            r["run"]: r for rg in snap["repos"] for r in rg.get("runs", [])
        }
        self.assertEqual(
            snap_by_id["20261124-100014-readonly-answered-read"].get("resultReadAt"),
            "2026-11-24T10:10:00Z",
        )
        self.assertFalse(
            snap_by_id["20261124-100013-readonly-answered-unread"].get("resultReadAt")
        )

    # ------------------------------------------------------------------
    # B. `delegate hook pending` Layer-2 snapshot (human + JSON)
    # ------------------------------------------------------------------
    def test_b_hook_pending_snapshot_prioritized_output(self):
        repo = self.repo({"a.txt": "v0\n"})
        runs_dir = Path(self.env["DELEGATE_RUNS"])
        brief = repo / "docs/w010.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# W010\n", encoding="utf-8")

        runs = [
            {
                "run": "20261124-100001-ask-design",
                "name": "ask-design",
                "mode": "read-only",
                "state": "answered",
                "question": True,
                "finishedAt": iso_ago(0.5),
            },
            {
                "run": "20261124-100002-live-build",
                "name": "live-build",
                "mode": "write",
                "state": "running",
            },
            {
                "run": "20261124-100003-feat-unmerged",
                "name": "feat-unmerged",
                "mode": "write",
                "state": "delivered",
                "changes": {"files": 2, "added": 12, "deleted": 3},
                "premerge": {"status": "failed", "failedLayer": "generate"},
                "finishedAt": iso_ago(0.4),
            },
        ]
        for r in runs:
            self.materialize_run(runs_dir, repo, r)
        self.write_work_jsonl([
            {
                "id": "W010",
                "title": "就绪十小时项",
                "repo": str(repo),
                "state": "ready",
                "brief": str(brief),
                "stateSince": iso_ago(10.5),
                "createdAt": iso_ago(10.5),
                "updatedAt": iso_ago(10.5),
            },
            {
                "id": "W011",
                "title": "缺说明两小时项",
                "repo": str(repo),
                "state": "ready",
                "stateSince": iso_ago(2.2),
                "createdAt": iso_ago(2.2),
                "updatedAt": iso_ago(2.2),
            },
            {
                "id": "W012",
                "title": "待闭环五小时项",
                "repo": str(repo),
                "state": "verify",
                "stateSince": iso_ago(5.2),
                "createdAt": iso_ago(5.2),
                "updatedAt": iso_ago(5.2),
            },
        ])

        env = {
            **self.clean_caller_env(),
            "DELEGATE_STOP_HOOK_CURRENT_ONLY": "1",
            "DELEGATE_STOP_HOOK_WAITERS": "",
            "DELEGATE_STOP_HOOK_PROCS": "",
        }
        res = subprocess.run(
            [str(fixtures.DELEGATE), "hook", "pending", "--cwd", str(repo)],
            capture_output=True,
            text=True,
            env=env,
            timeout=15,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        expected_lines = [
            "待处理 6 件（按优先级）：",
            '[提问] ask-design · 等你决定 · 下一步：`delegate result 20261124-100001-ask-design`，再 `delegate reply 20261124-100001-ask-design "<决定>"`',
            "[在跑无人等] live-build · 等 1 个委派在跑，没有挂等待 · 下一步：后台跑 `delegate wait --any`",
            '[未合入] feat-unmerged · 已交付未合入（预合并失败：generate） · 下一步：`delegate card 20261124-100003-feat-unmerged`；合不了用 `delegate work defer 20261124-100003-feat-unmerged --until 4h --reason "…"`',
            f"[就绪未派] [W010] 就绪十小时项 · 就绪 10 小时没派 · 下一步：`delegate start --work W010 --prompt-file {brief}`",
            '[待闭环] [W012] 待闭环五小时项 · 待闭环 5 小时 · 下一步：`delegate work close W012 --evidence "…"`',
            "[缺说明] [W011] 缺说明两小时项 · 没有任务说明 · 下一步：`delegate work wait W011 --brief <文件>`",
        ]
        self.assertEqual(res.stdout.strip().splitlines(), expected_lines)
        # Strictly read-only: must not touch work-remind.json or stop-hook.jsonl.
        self.assertFalse((self.state_dir() / "work-remind.json").exists())
        self.assertFalse((self.state_dir() / "stop-hook.jsonl").exists())

    # ------------------------------------------------------------------
    # C. All 7 verbatim templates + >40-char title clip + multi-category
    # ------------------------------------------------------------------
    def test_c_all_seven_verbatim_templates_and_multi_category_single_item(self):
        repo = self.repo({"a.txt": "v0\n"})
        runs_dir = Path(self.env["DELEGATE_RUNS"])
        sdir = self.state_dir()
        sdir.mkdir(parents=True, exist_ok=True)
        brief = repo / "docs/spec.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# Spec\n", encoding="utf-8")

        def reset_state():
            for child in list(runs_dir.iterdir()) if runs_dir.is_dir() else []:
                if child.is_dir():
                    import shutil
                    shutil.rmtree(child)
            for fname in ("events.jsonl", "cursors.json", "work.jsonl", "work-remind.json", "index.json"):
                (sdir / fname).unlink(missing_ok=True)

        # 1) 未读交付
        reset_state()
        self.materialize_run(runs_dir, repo, {
            "run": "20261124-110001-ro-deliv",
            "name": "ro-deliv",
            "mode": "read-only",
            "state": "answered",
        })
        (sdir / "events.jsonl").write_text(
            json.dumps({
                "seq": 1,
                "at": iso_ago(0.1),
                "run": "20261124-110001-ro-deliv",
                "repo": str(repo.resolve()),
                "kind": "delivered",
                "payload": {"name": "ro-deliv"},
            }) + "\n",
            encoding="utf-8",
        )
        b1 = self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False})
        self.assertEqual(
            b1,
            {"decision": "block", "reason": "ro-deliv 已交付待看。下一步：`delegate card 20261124-110001-ro-deliv`"},
        )

        # 2) 提问
        reset_state()
        self.materialize_run(runs_dir, repo, {
            "run": "20261124-110002-q-item",
            "name": "q-item",
            "mode": "read-only",
            "state": "answered",
            "question": True,
        })
        b2 = self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False})
        self.assertEqual(
            b2,
            {
                "decision": "block",
                "reason": 'q-item 在等你决定。下一步：`delegate result 20261124-110002-q-item`，再 `delegate reply 20261124-110002-q-item "<决定>"`',
            },
        )

        # 3) 在跑无人等 (k=2)
        reset_state()
        self.materialize_run(runs_dir, repo, {
            "run": "20261124-110003-live-a",
            "name": "live-a",
            "mode": "write",
            "state": "running",
        })
        self.materialize_run(runs_dir, repo, {
            "run": "20261124-110004-live-b",
            "name": "live-b",
            "mode": "write",
            "state": "running",
        })
        b3 = self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False})
        self.assertIsNotNone(b3)
        self.assertIn("等 2 个委派在跑，没有挂等待。下一步：后台跑 `delegate wait --any`", b3["reason"])
        self.assertNotIn("另有", b3["reason"])

        # 4a) 已交付未合入（无原因 -> 省略括号）
        reset_state()
        self.materialize_run(runs_dir, repo, {
            "run": "20261124-110005-plain-unmerged",
            "name": "plain-unmerged",
            "mode": "write",
            "state": "delivered",
            "changes": {"files": 1, "added": 3, "deleted": 0},
        })
        b4a = self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False})
        self.assertEqual(
            b4a,
            {
                "decision": "block",
                "reason": 'plain-unmerged 已交付未合入。下一步：`delegate card 20261124-110005-plain-unmerged`；合不了用 `delegate work defer 20261124-110005-plain-unmerged --until 4h --reason "…"`',
            },
        )

        # 4b) 已交付未合入（带预合并失败原因）
        reset_state()
        self.materialize_run(runs_dir, repo, {
            "run": "20261124-110006-pm-fail",
            "name": "pm-fail",
            "mode": "write",
            "state": "delivered",
            "changes": {"files": 1, "added": 3, "deleted": 0},
            "premerge": {"status": "failed", "failedLayer": "generate"},
        })
        b4b = self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False})
        self.assertEqual(
            b4b,
            {
                "decision": "block",
                "reason": 'pm-fail 已交付未合入（预合并失败：generate）。下一步：`delegate card 20261124-110006-pm-fail`；合不了用 `delegate work defer 20261124-110006-pm-fail --until 4h --reason "…"`',
            },
        )

        # 5) 工作项就绪未派 + 超过 40 字截断加「…」
        reset_state()
        long_title = "测" * 45
        self.write_work_jsonl([{
            "id": "W020",
            "title": long_title,
            "repo": str(repo),
            "state": "ready",
            "brief": str(brief),
            "stateSince": iso_ago(9.2),
            "createdAt": iso_ago(9.2),
            "updatedAt": iso_ago(9.2),
        }])
        b5 = self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False})
        self.assertEqual(
            b5,
            {
                "decision": "block",
                "reason": f"[W020] {'测' * 40}… 就绪 9 小时没派。下一步：`delegate start --work W020 --prompt-file {brief}`",
            },
        )

        # 6) 工作项缺说明 (>1h)
        reset_state()
        self.write_work_jsonl([{
            "id": "W021",
            "title": "补任务说明",
            "repo": str(repo),
            "state": "ready",
            "stateSince": iso_ago(1.5),
            "createdAt": iso_ago(1.5),
            "updatedAt": iso_ago(1.5),
        }])
        b6 = self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False})
        self.assertEqual(
            b6,
            {
                "decision": "block",
                "reason": "[W021] 补任务说明 没有任务说明。下一步：`delegate work wait W021 --brief <文件>`",
            },
        )

        # 7) 待闭环
        reset_state()
        self.write_work_jsonl([{
            "id": "W022",
            "title": "真机复测闭环",
            "repo": str(repo),
            "state": "verify",
            "stateSince": iso_ago(6.2),
            "createdAt": iso_ago(6.2),
            "updatedAt": iso_ago(6.2),
        }])
        b7 = self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False})
        self.assertEqual(
            b7,
            {
                "decision": "block",
                "reason": '[W022] 真机复测闭环 待闭环 6 小时。下一步：`delegate work close W022 --evidence "…"`',
            },
        )

        # Multi-category coexistence: question + unmerged + ready work -> ONLY question shown, remaining=2!
        reset_state()
        self.materialize_run(runs_dir, repo, {
            "run": "20261124-110010-top-q",
            "name": "top-q",
            "mode": "read-only",
            "state": "answered",
            "question": True,
        })
        self.materialize_run(runs_dir, repo, {
            "run": "20261124-110011-other-unmerged",
            "name": "other-unmerged",
            "mode": "write",
            "state": "delivered",
            "changes": {"files": 1, "added": 2, "deleted": 0},
        })
        self.write_work_jsonl([{
            "id": "W030",
            "title": "低优工作项",
            "repo": str(repo),
            "state": "ready",
            "brief": str(brief),
            "stateSince": iso_ago(10.2),
            "createdAt": iso_ago(10.2),
            "updatedAt": iso_ago(10.2),
        }])
        b_multi = self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False})
        self.assertEqual(
            b_multi,
            {
                "decision": "block",
                "reason": 'top-q 在等你决定。下一步：`delegate result 20261124-110010-top-q`，再 `delegate reply 20261124-110010-top-q "<决定>"`（另有 2 件：`delegate hook pending`）',
            },
        )
        self.assertNotIn("\n", b_multi["reason"])
        self.assertNotIn("--wait-manual", b_multi["reason"])
        self.assertFalse((sdir / "work-remind.json").exists())

    # ------------------------------------------------------------------
    # D. Missing-brief 1h grace period
    # ------------------------------------------------------------------
    def test_d_missing_brief_1h_grace_period(self):
        repo = self.repo({"a.txt": "v0\n"})
        # 30 minutes old (< 1h grace) -> MUST NOT remind
        self.write_work_jsonl([{
            "id": "W059",
            "title": "刚建好的缺说明项",
            "repo": str(repo),
            "state": "ready",
            "stateSince": iso_ago_mins(30),
            "createdAt": iso_ago_mins(30),
            "updatedAt": iso_ago_mins(30),
        }])
        self.assertIsNone(self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False}))

        # 65 minutes old (>= 1h grace) -> MUST remind
        self.write_work_jsonl([{
            "id": "W059",
            "title": "超过一小时的缺说明项",
            "repo": str(repo),
            "state": "ready",
            "stateSince": iso_ago_mins(65),
            "createdAt": iso_ago_mins(65),
            "updatedAt": iso_ago_mins(65),
        }])
        blk = self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False})
        self.assertIsNotNone(blk)
        self.assertEqual(
            blk["reason"],
            "[W059] 超过一小时的缺说明项 没有任务说明。下一步：`delegate work wait W059 --brief <文件>`",
        )

    # ------------------------------------------------------------------
    # E. Cooldown records ONLY the single displayed work item
    # ------------------------------------------------------------------
    def test_e_cooldown_records_only_displayed_item_not_collapsed(self):
        repo = self.repo({"a.txt": "v0\n"})
        brief = repo / "docs/spec.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# Spec\n", encoding="utf-8")
        self.write_work_jsonl([
            {
                "id": "W001",
                "title": "项一最老",
                "repo": str(repo),
                "state": "ready",
                "brief": str(brief),
                "stateSince": iso_ago(12.2),
                "createdAt": iso_ago(12.2),
                "updatedAt": iso_ago(12.2),
            },
            {
                "id": "W002",
                "title": "项二次老",
                "repo": str(repo),
                "state": "ready",
                "brief": str(brief),
                "stateSince": iso_ago(10.2),
                "createdAt": iso_ago(10.2),
                "updatedAt": iso_ago(10.2),
            },
            {
                "id": "W003",
                "title": "项三较新",
                "repo": str(repo),
                "state": "ready",
                "brief": str(brief),
                "stateSince": iso_ago(9.2),
                "createdAt": iso_ago(9.2),
                "updatedAt": iso_ago(9.2),
            },
        ])
        remind_path = self.state_dir() / "work-remind.json"

        # Call 1 -> shows ONLY W001 (另有 2 件), records ONLY W001
        b1 = self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False})
        self.assertIn("[W001] 项一最老", b1["reason"])
        self.assertIn("（另有 2 件：`delegate hook pending`）", b1["reason"])
        self.assertEqual(set(json.loads(remind_path.read_text()).keys()), {"W001"})

        # Call 2 -> shows ONLY W002 (另有 1 件), records W001+W002 (not W003)
        b2 = self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False})
        self.assertIn("[W002] 项二次老", b2["reason"])
        self.assertIn("（另有 1 件：`delegate hook pending`）", b2["reason"])
        self.assertEqual(set(json.loads(remind_path.read_text()).keys()), {"W001", "W002"})

        # Call 3 -> shows ONLY W003 (no 另有), records all 3
        b3 = self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False})
        self.assertIn("[W003] 项三较新", b3["reason"])
        self.assertNotIn("另有", b3["reason"])
        self.assertEqual(set(json.loads(remind_path.read_text()).keys()), {"W001", "W002", "W003"})

        # Call 4 -> all 3 in cooldown -> passes
        self.assertIsNone(self.run_stop_wrapper({"cwd": str(repo), "stop_hook_active": False}))

    # ------------------------------------------------------------------
    # F. Stall-chain escalation: 11-24 shape (other-session continuation
    #    run delivered 35m ago blocking 3 downstream items -> penetrates
    #    current session across repos; defer clears; verify 30m w/o slot)
    # ------------------------------------------------------------------
    def test_f_stall_chain_35m_cross_repo_penetration_defer_and_verify_without_slot(self):
        repo_a = self.work / "aia-skills-sim"
        repo_b = self.work / "norvane-sim"
        for rp, fname in ((repo_a, "a.txt"), (repo_b, "b.txt")):
            rp.mkdir(parents=True, exist_ok=True)
            subprocess.run(["git", "-C", str(rp), "init", "-q"], check=True)
            (rp / fname).write_text("v0\n", encoding="utf-8")
            subprocess.run(["git", "-C", str(rp), "add", fname], check=True)
            subprocess.run(
                ["git", "-C", str(rp), "-c", "user.name=T", "-c", "user.email=t@e", "commit", "-q", "-m", "init"],
                check=True,
            )

        runs_a = repo_a / ".local/run/delegate"
        runs_b = repo_b / ".local/run/delegate"
        runs_a.mkdir(parents=True, exist_ok=True)
        runs_b.mkdir(parents=True, exist_ok=True)

        # Register both repos in `<state>/repos.json`
        sdir = self.state_dir()
        sdir.mkdir(parents=True, exist_ok=True)
        (sdir / "repos.json").write_text(
            json.dumps({
                "repos": {
                    str(repo_a.resolve()): {"repo": str(repo_a.resolve()), "runs": str(runs_a.resolve())},
                    str(repo_b.resolve()): {"repo": str(repo_b.resolve()), "runs": str(runs_b.resolve())},
                }
            }),
            encoding="utf-8",
        )

        # Spawn a live process for `sess-other-alive` so W058 considers original caller ALIVE!
        other_sid = f"sess-other-alive-{os.getpid()}"
        cur_sid = f"sess-cur-active-{os.getpid()}"
        alive_proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            env={**self.clean_caller_env(), "CLAUDE_CODE_SESSION_ID": other_sid},
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.addCleanup(lambda: (alive_proc.kill(), alive_proc.wait(timeout=5)) if alive_proc.poll() is None else None)

        # Root + continuation leaf in repo_a, owned by `other_sid`, delivered 35 minutes ago!
        root_rid = "20261124-140427-delegate-fixes"
        leaf_rid = "20261124-145101-bb52"
        self.materialize_run(runs_a, repo_a, {
            "run": root_rid,
            "name": "delegate-fixes",
            "mode": "write",
            "state": "delivered",
            "caller": other_sid,
            "changes": {"files": 5, "added": 100, "deleted": 10},
            "finishedAt": iso_ago_mins(50),
        })
        self.materialize_run(runs_a, repo_a, {
            "run": leaf_rid,
            "name": "reply to delegate-fixes",
            "parent": root_rid,
            "mode": "write",
            "state": "delivered",
            "caller": other_sid,
            "changes": {"files": 5, "added": 120, "deleted": 12},
            "finishedAt": iso_ago_mins(35),
        })

        # Also add a lower-priority non-blocking unmerged run in current repo_b owned by `cur_sid`
        self.materialize_run(runs_b, repo_b, {
            "run": "20261124-150000-local-leaf",
            "name": "local-leaf",
            "mode": "write",
            "state": "delivered",
            "caller": cur_sid,
            "changes": {"files": 1, "added": 2, "deleted": 0},
            "finishedAt": iso_ago_mins(10),
        })

        # 3 downstream work items transitively blocked on `run_applied:<root_rid>`
        self.write_work_jsonl([
            {
                "id": "W057",
                "title": "下游一",
                "repo": str(repo_a),
                "state": "blocked",
                "waitOn": [f"run_applied:{root_rid}"],
                "stateSince": iso_ago_mins(60),
                "createdAt": iso_ago_mins(60),
                "updatedAt": iso_ago_mins(60),
            },
            {
                "id": "W058",
                "title": "下游二",
                "repo": str(repo_a),
                "state": "blocked",
                "waitOn": ["work_closed:W057"],
                "stateSince": iso_ago_mins(60),
                "createdAt": iso_ago_mins(60),
                "updatedAt": iso_ago_mins(60),
            },
            {
                "id": "W069",
                "title": "下游三",
                "repo": str(repo_a),
                "state": "blocked",
                "waitOn": ["work_closed:W058"],
                "stateSince": iso_ago_mins(60),
                "createdAt": iso_ago_mins(60),
                "updatedAt": iso_ago_mins(60),
            },
        ])

        cur_env = {
            **self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": cur_sid}),
            "DELEGATE_STOP_HOOK_CURRENT_ONLY": "0",
        }
        cur_env.pop("DELEGATE_RUNS", None)

        # Current session in repo_b MUST be blocked by repo_a's stall-chain leaf (Tier 2 > local Tier 5)!
        blk = self.run_stop_wrapper(
            {"cwd": str(repo_b), "session_id": cur_sid, "stop_hook_active": False},
            env=cur_env,
            extra_env={"DELEGATE_STOP_HOOK_CURRENT_ONLY": "0"},
        )
        self.assertIsNotNone(blk)
        self.assertEqual(blk["decision"], "block")
        self.assertIn("【aia-skills-sim】delegate-fixes", blk["reason"])
        self.assertIn("挡 3 个下游（W057、W058、W069）", blk["reason"])
        self.assertIn(f"cd {repo_a}", blk["reason"])
        self.assertIn(f"delegate card {leaf_rid}", blk["reason"])
        self.assertIn(f"delegate work defer {leaf_rid}", blk["reason"])
        # Only 1 item shown; local-leaf is collapsed into `另有 1 件`
        self.assertIn("（另有 1 件：`delegate hook pending`）", blk["reason"])
        self.assertNotIn("local-leaf", blk["reason"])
        self.assertNotIn("\n", blk["reason"])

        # Defer the leaf run -> stall-chain penetration clears (falls through to local-leaf)!
        defer_res = subprocess.run(
            [str(fixtures.DELEGATE), "work", "defer", leaf_rid, "--until", "4h", "--reason", "先收当前仓"],
            cwd=repo_a,
            capture_output=True,
            text=True,
            env=cur_env,
            timeout=15,
        )
        self.assertEqual(defer_res.returncode, 0, defer_res.stdout + defer_res.stderr)
        blk_after = self.run_stop_wrapper(
            {"cwd": str(repo_b), "session_id": cur_sid, "stop_hook_active": False},
            env=cur_env,
            extra_env={"DELEGATE_STOP_HOOK_CURRENT_ONLY": "0"},
        )
        self.assertIsNotNone(blk_after)
        self.assertIn("local-leaf", blk_after["reason"])
        self.assertNotIn("delegate-fixes", blk_after["reason"])

        # Verify: blocking `verify` item at 35m alerts WITHOUT idle slot; `blocked` item itself never alerts!
        (runs_b / "20261124-150000-local-leaf" / ".applied").write_text("{}\n")
        self.materialize_run(runs_b, repo_b, {
            "run": "20261124-150100-busy-slot",
            "name": "busy-slot",
            "mode": "write",
            "state": "running",
            "caller": cur_sid,
        })
        (repo_b / ".delegate.json").write_text(json.dumps({"remote": False, "repoMaxActive": 1}))
        self.write_work_jsonl([
            {
                "id": "W071",
                "title": "已合入待真机验",
                "repo": str(repo_b),
                "state": "verify",
                "applied": True,
                "caller": other_sid,
                "stateSince": iso_ago_mins(35),
                "createdAt": iso_ago_mins(90),
                "updatedAt": iso_ago_mins(35),
            },
            {
                "id": "W064",
                "title": "被W071挡住的下游",
                "repo": str(repo_b),
                "state": "blocked",
                "waitOn": ["work_closed:W071"],
                "stateSince": iso_ago_mins(35),
                "createdAt": iso_ago_mins(90),
                "updatedAt": iso_ago_mins(35),
            },
            {
                "id": "W072",
                "title": "无下游的verify三十五分钟不报警",
                "repo": str(repo_b),
                "state": "verify",
                "applied": True,
                "caller": cur_sid,
                "stateSince": iso_ago_mins(35),
                "createdAt": iso_ago_mins(90),
                "updatedAt": iso_ago_mins(35),
            },
        ])
        blk_verify = self.run_stop_wrapper(
            {"cwd": str(repo_b), "session_id": cur_sid, "stop_hook_active": False},
            env=cur_env,
            extra_env={
                "DELEGATE_STOP_HOOK_CURRENT_ONLY": "0",
                "DELEGATE_STOP_HOOK_WAITERS": "delegate wait",
            },
        )
        self.assertIsNotNone(blk_verify)
        self.assertIn("[W071] 已合入待真机验 待闭环 35 分钟（挡 1 个下游：W064）", blk_verify["reason"])
        self.assertIn("delegate work close W071", blk_verify["reason"])
        self.assertNotIn("W072", blk_verify["reason"])
        self.assertNotIn("另有", blk_verify["reason"])

    # ------------------------------------------------------------------
    # G. `delegate hook check --format claude|agy|json` & guards
    # ------------------------------------------------------------------
    def test_g_hook_check_claude_and_agy_formats_guards_and_zero_git_zero_lock(self):
        repo = self.repo({"a.txt": "v0\n"})
        runs_dir = Path(self.env["DELEGATE_RUNS"])

        # 1) Allow outputs when clean
        allow_claude = self.run_hook_check("claude", {"cwd": str(repo), "stop_hook_active": False})
        self.assertEqual(allow_claude.returncode, 0, allow_claude.stderr)
        self.assertEqual(allow_claude.stdout.strip(), "")

        allow_agy = self.run_hook_check("agy", {"cwd": str(repo), "continuationActive": False, "terminationReason": "NO_TOOL_CALL"})
        self.assertEqual(allow_agy.returncode, 0, allow_agy.stderr)
        self.assertEqual(json.loads(allow_agy.stdout), {})

        # Create a live run without waiter to test block outputs & agy waiter wording
        self.materialize_run(runs_dir, repo, {
            "run": "20261124-160001-live-agy",
            "name": "live-agy",
            "mode": "write",
            "state": "running",
        })

        # Shadow `git` with a failing stub + enable state-lock probe to prove zero-git & zero-lock!
        nogit_bin = self.work / "nogit-hook"
        nogit_bin.mkdir(exist_ok=True)
        bad_git = nogit_bin / "git"
        bad_git.write_text("#!/bin/sh\necho 'git forbidden' >&2\nexit 99\n")
        bad_git.chmod(0o755)
        probe_env = {
            "PATH": f"{nogit_bin}:{self.env['PATH']}",
            "DELEGATE_MEASURE_STATE_LOCK": "1",
        }

        # 2) Block output: claude -> decision=block; agy -> decision=continue + agy waiter wording
        blk_claude = self.run_hook_check("claude", {"cwd": str(repo), "stop_hook_active": False}, extra_env=probe_env)
        self.assertEqual(blk_claude.returncode, 0, blk_claude.stderr)
        self.assertNotIn("STATE_LOCK_ATTEMPT", blk_claude.stderr)
        doc_c = json.loads(blk_claude.stdout)
        self.assertEqual(doc_c["decision"], "block")
        self.assertIn("后台跑 `delegate wait --any`", doc_c["reason"])

        blk_agy = self.run_hook_check(
            "agy",
            {"cwd": str(repo), "conversationId": "agy-c1", "continuationActive": False, "terminationReason": "NO_TOOL_CALL"},
            extra_env=probe_env,
        )
        self.assertEqual(blk_agy.returncode, 0, blk_agy.stderr)
        self.assertNotIn("STATE_LOCK_ATTEMPT", blk_agy.stderr)
        doc_a = json.loads(blk_agy.stdout)
        self.assertEqual(doc_a["decision"], "continue")
        self.assertIn("run_command 设 WaitMsBeforeAsync=500 后台跑 `delegate wait --any`", doc_a["reason"])

        # 3) Anti-loop guards
        loop_c = self.run_hook_check("claude", {"cwd": str(repo), "stop_hook_active": True})
        self.assertEqual(loop_c.stdout.strip(), "")
        loop_a = self.run_hook_check("agy", {"cwd": str(repo), "continuationActive": True, "terminationReason": "NO_TOOL_CALL"})
        self.assertEqual(json.loads(loop_a.stdout), {})

        # 4) agy terminationReason != NO_TOOL_CALL -> allow `{}`
        for reason in ("USER_CANCELED", "ERROR", "MAX_INVOCATIONS"):
            term_a = self.run_hook_check(
                "agy",
                {"cwd": str(repo), "continuationActive": False, "terminationReason": reason},
            )
            self.assertEqual(json.loads(term_a.stdout), {}, reason)

        # 5) Colleague env guards bypass BOTH formats immediately
        for var, val in (
            ("PI_DELEGATE_ACTIVE", "1"),
            ("DELEGATE_AGENT", "aia"),
            ("DELEGATE_RUN_DIR", str(runs_dir / "20261124-160001-live-agy")),
        ):
            col_c = self.run_hook_check("claude", {"cwd": str(repo)}, extra_env={var: val})
            self.assertEqual(col_c.stdout.strip(), "", f"claude failed colleague guard for {var}")
            col_a = self.run_hook_check(
                "agy",
                {"cwd": str(repo), "terminationReason": "NO_TOOL_CALL"},
                extra_env={var: val},
            )
            self.assertEqual(json.loads(col_a.stdout), {}, f"agy failed colleague guard for {var}")


if __name__ == "__main__":
    unittest.main()
