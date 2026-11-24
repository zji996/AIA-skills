"""End-to-end tests for work value classes, `work import-dag`, value-quota auto-fill
with hotspots, and `delegate fill stats [--since 24h]`.
"""
import json
import os
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


def iso_ago(seconds_ago: int) -> str:
    ts = int(time.time()) - seconds_ago
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts))


class DelegateFillAndDagTests(unittest.TestCase):
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

    def read_events(self) -> list[dict]:
        p = self.state_dir() / "events.jsonl"
        if not p.is_file():
            return []
        return [
            json.loads(line)
            for line in p.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]

    def test_work_class_and_missing_class_badge(self):
        repo = self.repo({"README.md": "hello\n"})

        # Invalid class is rejected
        bad = self.cli("work", "add", "无效类别", "--class", "bug", cwd=repo, human=True)
        self.assertNotEqual(bad.returncode, 0)
        self.assertIn("--class", bad.stderr)

        # Add one without class and one with class=feat
        a1 = self.cli("work", "add", "没有类别的任务", "--repo", repo, "--json", cwd=repo, human=True)
        self.assertEqual(a1.returncode, 0, a1.stdout + a1.stderr)
        w1 = json.loads(a1.stdout)
        self.assertNotIn("class", w1)

        a2 = self.cli(
            "work", "add", "用户可见新功能", "--repo", repo, "--class", "feat", "--json", cwd=repo, human=True
        )
        self.assertEqual(a2.returncode, 0, a2.stdout + a2.stderr)
        w2 = json.loads(a2.stdout)
        self.assertEqual(w2["class"], "feat")

        # `delegate work` appends 「缺类别」 only on W001
        tbl = self.cli("work", cwd=repo, human=True)
        self.assertEqual(tbl.returncode, 0, tbl.stdout + tbl.stderr)
        lines = tbl.stdout.splitlines()
        w1_line = next(l for l in lines if f"[{w1['id']}]" in l)
        w2_line = next(l for l in lines if f"[{w2['id']}]" in l)
        self.assertTrue(w1_line.endswith(" · 缺类别"), w1_line)
        self.assertNotIn("缺类别", w2_line)

        # `work wait W001 --class unblock` updates class, preserves state_since, removes 缺类别
        orig_since = w1["stateSince"]
        upd = self.cli("work", "wait", w1["id"], "--class", "unblock", "--json", cwd=repo, human=True)
        self.assertEqual(upd.returncode, 0, upd.stdout + upd.stderr)
        w1_after = json.loads(upd.stdout)
        self.assertEqual(w1_after["class"], "unblock")
        self.assertEqual(w1_after["stateSince"], orig_since)

        tbl2 = self.cli("work", cwd=repo, human=True)
        w1_line2 = next(l for l in tbl2.stdout.splitlines() if f"[{w1['id']}]" in l)
        self.assertNotIn("缺类别", w1_line2)

    def test_import_dag_rejections_dry_run_and_edges(self):
        repo = self.repo({"README.md": "hello\n"})
        brief_a = repo / "brief-a.md"
        brief_b = repo / "brief-b.md"
        brief_c = repo / "brief-c.md"
        brief_a.write_text("Task A brief\n", encoding="utf-8")
        brief_b.write_text("Task B brief\n", encoding="utf-8")
        brief_c.write_text("Task C brief\n", encoding="utf-8")

        # 1) Duplicate key -> whole batch rejected
        dup_file = repo / "dup.json"
        dup_file.write_text(
            json.dumps({
                "items": [
                    {"key": "k1", "title": "A", "class": "feat", "brief": str(brief_a)},
                    {"key": "k1", "title": "B", "class": "docs", "brief": str(brief_b)},
                ]
            }),
            encoding="utf-8",
        )
        r_dup = self.cli("work", "import-dag", str(dup_file), "--repo", repo, cwd=repo, human=True)
        self.assertNotEqual(r_dup.returncode, 0)
        self.assertIn("duplicate key", r_dup.stderr)
        self.assertFalse((self.state_dir() / "work.jsonl").exists())

        # 2) Dependency cycle -> whole batch rejected
        cyc_file = repo / "cycle.json"
        cyc_file.write_text(
            json.dumps({
                "items": [
                    {"key": "a", "title": "A", "class": "feat", "brief": str(brief_a), "after": ["c"]},
                    {"key": "b", "title": "B", "class": "unblock", "brief": str(brief_b), "after": ["a"]},
                    {"key": "c", "title": "C", "class": "infra", "brief": str(brief_c), "after": ["b"]},
                ]
            }),
            encoding="utf-8",
        )
        r_cyc = self.cli("work", "import-dag", str(cyc_file), "--repo", repo, cwd=repo, human=True)
        self.assertNotEqual(r_cyc.returncode, 0)
        self.assertIn("cycle", r_cyc.stderr)
        self.assertFalse((self.state_dir() / "work.jsonl").exists())

        # 3) Missing brief file -> whole batch rejected
        miss_file = repo / "missing.json"
        miss_file.write_text(
            json.dumps({
                "items": [
                    {"key": "a", "title": "A", "class": "feat", "brief": str(brief_a)},
                    {"key": "b", "title": "B", "class": "docs", "brief": str(repo / "nope.md")},
                ]
            }),
            encoding="utf-8",
        )
        r_miss = self.cli("work", "import-dag", str(miss_file), "--repo", repo, cwd=repo, human=True)
        self.assertNotEqual(r_miss.returncode, 0)
        self.assertIn("brief file does not exist", r_miss.stderr)
        self.assertFalse((self.state_dir() / "work.jsonl").exists())

        # 4) Valid DAG: --dry-run prints items & edges without writing work.jsonl
        dag_file = repo / "dag.json"
        dag_file.write_text(
            json.dumps({
                "items": [
                    {
                        "key": "core",
                        "title": "Core refactor",
                        "class": "unblock",
                        "brief": "brief-a.md",
                        "doneWhen": "unit tests pass",
                        "files": ["crates/core/"],
                        "tier": "write",
                        "model": "test",
                        "thinking": "medium",
                    },
                    {
                        "key": "ui",
                        "title": "UI panel",
                        "class": "feat",
                        "brief": "brief-b.md",
                        "after": ["core"],
                        "files": ["web/ui.tsx", "README.md"],
                        "tier": "write",
                    },
                    {
                        "key": "doc",
                        "title": "Update guide",
                        "class": "docs",
                        "brief": "brief-c.md",
                        "after": ["core", "ui"],
                        "files": ["README.md"],
                    },
                ]
            }),
            encoding="utf-8",
        )
        dry = self.cli("work", "import-dag", str(dag_file), "--dry-run", "--repo", repo, cwd=repo, human=True)
        self.assertEqual(dry.returncode, 0, dry.stdout + dry.stderr)
        self.assertIn("core -> ui", dry.stdout)
        self.assertIn("ui -> doc", dry.stdout)
        self.assertFalse((self.state_dir() / "work.jsonl").exists())

        # 5) Real import creates items with correct after-work edges, files, and autoDispatch
        imp = self.cli("work", "import-dag", str(dag_file), "--repo", repo, "--json", cwd=repo, human=True)
        self.assertEqual(imp.returncode, 0, imp.stdout + imp.stderr)
        doc = json.loads(imp.stdout)
        by_key = {item["key"]: item for item in doc["imported"]}
        self.assertEqual(len(by_key), 3)
        core_id = by_key["core"]["id"]
        ui_id = by_key["ui"]["id"]
        doc_id = by_key["doc"]["id"]

        card_core = json.loads(self.cli("work", "show", core_id, "--json", cwd=repo, human=True).stdout)
        card_ui = json.loads(self.cli("work", "show", ui_id, "--json", cwd=repo, human=True).stdout)
        card_doc = json.loads(self.cli("work", "show", doc_id, "--json", cwd=repo, human=True).stdout)

        self.assertEqual(card_core["state"], "ready")
        self.assertEqual(card_core["class"], "unblock")
        self.assertEqual(card_core["files"], ["crates/core/"])
        self.assertEqual(
            card_core["autoDispatch"]["startArgs"],
            ["--worktree", "--tier", "write", "--model", "test", "--thinking", "medium"],
        )
        self.assertEqual(Path(card_core["autoDispatch"]["promptFile"]), brief_a.resolve())

        self.assertEqual(card_ui["state"], "blocked")
        self.assertEqual(card_ui["waitOn"], [f"work_closed:{core_id}"])
        self.assertEqual(card_doc["state"], "blocked")
        self.assertEqual(card_doc["waitOn"], [f"work_closed:{core_id}", f"work_closed:{ui_id}"])

    def test_fill_dispatch_priority_quota_hotspots_and_target(self):
        repo = self.repo({"README.md": "hello\n"})
        (repo / ".delegate.json").write_text(
            json.dumps({
                "remote": False,
                "accept": "true",
                "fill": {"target": 6, "maxInfraDocsShare": 0.3},
                "hotspots": ["README.md"],
            }),
            encoding="utf-8",
        )
        brief = repo / "brief.md"
        brief.write_text("do fill work\n", encoding="utf-8")
        brief_s = str(brief.resolve())
        repo_s = str(repo.resolve())
        runs_root = Path(self.env["DELEGATE_RUNS"]).resolve()
        runs_dir = str(runs_root)

        def make_live_run(run_id: str, mode: str = "write", work_id: str = "") -> str:
            rdir = runs_root / run_id
            rdir.mkdir(parents=True, exist_ok=True)
            meta = {
                "run": run_id,
                "name": run_id,
                "top": repo_s,
                "workdir": repo_s,
                "mode": mode,
                "startedAt": iso_ago(120),
            }
            if work_id:
                meta["workId"] = work_id
            (rdir / "meta.json").write_text(json.dumps(meta), encoding="utf-8")
            (rdir / "supervisor.pid").write_text(f"{os.getpid()}\n", encoding="utf-8")
            return str(rdir)

        def make_item(wid, state, cls, since, files=None, wait_on=None, live_run_id=None, ro=False):
            start_args = ["--read-only"] if ro else ["--worktree", "--accept", "true"]
            runs_list = []
            run_dirs_map = {}
            if live_run_id:
                rdir_s = make_live_run(live_run_id, mode="read-only" if ro else "write", work_id=wid)
                runs_list.append(live_run_id)
                run_dirs_map[live_run_id] = rdir_s
            return {
                "id": wid,
                "title": f"task-{wid}",
                "repo": repo_s,
                "state": state,
                "class": cls,
                "files": files or [],
                "waitOn": wait_on or [],
                "runs": runs_list,
                "runDirs": run_dirs_map,
                "artifacts": [],
                "followups": [],
                "brief": brief_s,
                "autoDispatch": {
                    "promptFile": brief_s,
                    "startArgs": start_args,
                    "cwd": repo_s,
                    "repo": repo_s,
                    "runsDir": runs_dir,
                },
                "createdAt": since,
                "updatedAt": since,
                "stateSince": since,
            }

        # 1) Active write run W001 holds hotspot README.md + 2 active infra/docs (W002, W003)
        #    fill the 30% quota (target=6 -> max 2).
        # Ready candidates:
        #   W010: feat touching README.md (must be SKIPPED due to hotspot!)
        #   W011: infra touching src/z.rs (must be SKIPPED due to infra/docs quota 2/6!)
        #   W012: unblock touching src/a.rs (overlaps non-hotspot src/a.rs with active W001 -> ALLOWED!)
        #   W013: missing class (must be IGNORED!)
        self.write_work_jsonl([
            make_item("W001", "active", "feat", iso_ago(300), files=["README.md", "src/a.rs"], live_run_id="20261124-200001-active-w1"),
            make_item("W002", "active", "infra", iso_ago(290), files=["scripts/a.sh"], live_run_id="20261124-200002-active-w2"),
            make_item("W003", "active", "docs", iso_ago(280), files=["docs/x.md"], live_run_id="20261124-200003-active-w3"),
            make_item("W010", "ready", "feat", iso_ago(200), files=["README.md"]),
            make_item("W011", "ready", "infra", iso_ago(190), files=["src/z.rs"]),
            make_item("W012", "ready", "unblock", iso_ago(180), files=["src/a.rs"]),
            make_item("W013", "ready", "", iso_ago(170), files=["src/b.rs"]),
        ])
        self.fake_pi([fixtures.answer("done W012"), fixtures.SETTLED], pre="echo ok > src_a_done.txt")

        # Trigger reconcile via `work --rebuild`
        res = self.cli("work", "--rebuild", cwd=repo, human=True)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)

        # W012 must be dispatched; W010 (hotspot), W011 (quota), W013 (missing class) remain ready
        c10 = json.loads(self.cli("work", "show", "W010", "--json", cwd=repo, human=True).stdout)
        c11 = json.loads(self.cli("work", "show", "W011", "--json", cwd=repo, human=True).stdout)
        c12 = json.loads(self.cli("work", "show", "W012", "--json", cwd=repo, human=True).stdout)
        c13 = json.loads(self.cli("work", "show", "W013", "--json", cwd=repo, human=True).stdout)
        self.assertEqual(c10["state"], "ready")
        self.assertEqual(c11["state"], "ready")
        self.assertEqual(c13["state"], "ready")
        dispatched_w12 = c12["autoDispatch"].get("dispatchedRun")
        self.assertTrue(dispatched_w12, c12)

        # Wait for W012's dispatched run to settle cleanly before inspecting events & moving on
        self.cli("wait", dispatched_w12, cwd=repo)

        # Verify `work_dispatched` event has reason="fill", class="unblock", activeWriteRuns=3
        evs = self.read_events()
        fill_evs = [
            e for e in evs
            if e.get("kind") == "work_dispatched"
            and e.get("payload", {}).get("workId") == "W012"
        ]
        self.assertEqual(len(fill_evs), 1, evs)
        p = fill_evs[0]["payload"]
        self.assertEqual(p.get("reason"), "fill")
        self.assertEqual(p.get("class"), "unblock")
        self.assertEqual(p.get("activeWriteRuns"), 3)

        # 2) Target full (6 active write runs on disk) -> does NOT dispatch ready feat W099
        full_items = [
            make_item(
                f"W05{i}", "active", "feat", iso_ago(300 - i), live_run_id=f"20261124-20010{i}-full-w{i}"
            )
            for i in range(1, 7)
        ]
        full_items.append(make_item("W099", "ready", "feat", iso_ago(100)))
        self.write_work_jsonl(full_items)
        res_full = self.cli("work", "--rebuild", cwd=repo, human=True)
        self.assertEqual(res_full.returncode, 0, res_full.stdout + res_full.stderr)
        c99 = json.loads(self.cli("work", "show", "W099", "--json", cwd=repo, human=True).stdout)
        self.assertEqual(c99["state"], "ready")
        self.assertFalse(c99["autoDispatch"].get("dispatchedRun"))

        # 3) 5 active write runs + 2 active read-only runs -> read-only does NOT count, so W099 dispatches!
        # Remove the 6th write run and add 2 ephemeral active read-only runs
        (runs_root / "20261124-200106-full-w6" / "exit_code").write_text("0\n", encoding="utf-8")
        (runs_root / "20261124-200106-full-w6" / "summary.json").write_text(
            json.dumps({"state": "delivered"}), encoding="utf-8"
        )
        # Also mark earlier W001..W003 active runs as finished so only W051..W055 (5 writes) are active
        for old_r in ("20261124-200001-active-w1", "20261124-200002-active-w2", "20261124-200003-active-w3"):
            (runs_root / old_r / "exit_code").write_text("0\n", encoding="utf-8")
            (runs_root / old_r / "summary.json").write_text(json.dumps({"state": "delivered"}), encoding="utf-8")
        make_live_run("20261124-200201-ro-1", mode="read-only")
        make_live_run("20261124-200202-ro-2", mode="read-only")
        self.write_work_jsonl(full_items[:5] + [make_item("W099", "ready", "feat", iso_ago(100))])
        self.fake_pi([fixtures.answer("done W099"), fixtures.SETTLED], pre="echo ok > w99_done.txt")
        res_ro = self.cli("work", "--rebuild", cwd=repo, human=True)
        self.assertEqual(res_ro.returncode, 0, res_ro.stdout + res_ro.stderr)
        c99_after = json.loads(self.cli("work", "show", "W099", "--json", cwd=repo, human=True).stdout)
        dispatched_w99 = c99_after["autoDispatch"].get("dispatchedRun")
        self.assertTrue(dispatched_w99, c99_after)
        self.cli("wait", dispatched_w99, cwd=repo)

    def test_fill_stats_fixture_stream_and_stop_lines(self):
        repo = self.repo({"README.md": "hello\n"})
        cfg_path = repo / ".delegate.json"
        orig_cfg = json.dumps({"remote": False, "fill": {"target": 6, "maxInfraDocsShare": 0.3}})
        cfg_path.write_text(orig_cfg, encoding="utf-8")
        repo_s = str(repo.resolve())

        sdir = self.state_dir()
        sdir.mkdir(parents=True, exist_ok=True)
        now_ep = int(time.time())

        # Build fixture event stream:
        # - 10 write deliveries, 2 premerge_conflict (20% > 10%), 3 rework_sent (30% > 28.3%)
        # - 6 delivered simultaneously before apply -> peakUnmerged = 6 (> 5)
        # - lane queued 30s / held 120s -> 20% (> 15%)
        # - 2 read-only runs taking 5% of agy tokens
        # - weekly quota dropped 5% across 10 deliveries -> 5.0% per 10 deliveries
        evs = []
        seq = 1

        def add_ev(secs_ago, run, kind, payload):
            nonlocal seq
            evs.append({
                "seq": seq,
                "at": iso_ago(secs_ago),
                "run": run,
                "repo": repo_s,
                "kind": kind,
                "payload": payload,
            })
            seq += 1

        for i in range(1, 7):
            rid = f"run-w{i}"
            add_ev(700 - i * 10, rid, "work_dispatched", {"reason": "fill", "class": "feat", "activeWriteRuns": i - 1})
            add_ev(600 - i * 10, rid, "delivered", {"mode": "write", "state": "delivered", "files": 2})
        for i in range(1, 7):
            add_ev(500 - i * 5, f"run-w{i}", "applied", {})
        for i in range(7, 11):
            rid = f"run-w{i}"
            add_ev(400 - i * 10, rid, "delivered", {"mode": "write", "state": "delivered", "files": 1})
            add_ev(395 - i * 10, rid, "applied", {})

        add_ev(450, "run-w2", "premerge_conflict", {"conflicts": ["README.md"]})
        add_ev(440, "run-w3", "premerge_conflict", {"conflicts": ["README.md"]})
        add_ev(430, "run-w1", "rework_sent", {"round": 1})
        add_ev(420, "run-w4", "rework_sent", {"round": 1})
        add_ev(410, "run-w5", "rework_sent", {"round": 1})
        add_ev(300, "run-w1", "lane_acquired", {"queuedSeconds": 30.0})
        add_ev(290, "run-w1", "lane_released", {"heldSeconds": 120.0})
        add_ev(250, "run-ro1", "delivered", {"mode": "read-only", "state": "answered"})
        add_ev(240, "run-ro2", "started", {"mode": "read-only"})

        (sdir / "events.jsonl").write_text(
            "".join(json.dumps(e) + "\n" for e in evs), encoding="utf-8"
        )
        (sdir / "agy-usage.jsonl").write_text(
            "".join(
                json.dumps(u) + "\n"
                for u in [
                    {
                        "epoch": now_ep - 500,
                        "run": "run-w1",
                        "repo": repo_s,
                        "mode": "write",
                        "isAgy": True,
                        "tokens": {"input": 950000, "output": 0, "cacheRead": 0},
                    },
                    {
                        "epoch": now_ep - 245,
                        "run": "run-ro1",
                        "repo": repo_s,
                        "mode": "read-only",
                        "isAgy": True,
                        "tokens": {"input": 50000, "output": 0, "cacheRead": 0},
                    },
                ]
            ),
            encoding="utf-8",
        )
        (sdir / "agy-quota.json").write_text(
            json.dumps({
                "accounts": {
                    "acct1": {
                        "history": [
                            {"epoch": now_ep - 800, "weekly": 95.0},
                            {"epoch": now_ep - 100, "weekly": 90.0},
                        ]
                    }
                }
            }),
            encoding="utf-8",
        )

        # JSON output check
        j_res = self.cli("fill", "stats", "--since", "24h", "--json", cwd=repo, human=True)
        self.assertEqual(j_res.returncode, 0, j_res.stdout + j_res.stderr)
        st = json.loads(j_res.stdout)
        self.assertEqual(st["target"], 6)
        self.assertEqual(st["writeDeliveries"], 10)
        self.assertEqual(st["fillDispatched"], 6)
        self.assertAlmostEqual(st["premergeConflictRate"], 0.2, places=4)
        self.assertAlmostEqual(st["reworkRate"], 0.3, places=4)
        self.assertEqual(st["peakUnmerged"], 6)
        self.assertAlmostEqual(st["laneQueueShare"], 0.2, places=4)
        self.assertAlmostEqual(st["agyWeeklyPctPer10Deliveries"], 5.0, places=2)
        self.assertEqual(st["readonlyRuns"], 2)
        self.assertAlmostEqual(st["readonlyAgyShare"], 0.05, places=4)
        self.assertTrue(st["stopTriggered"])
        self.assertTrue(st["stopLines"]["conflict"]["triggered"])
        self.assertTrue(st["stopLines"]["rework"]["triggered"])
        self.assertTrue(st["stopLines"]["peakUnmerged"]["triggered"])
        self.assertTrue(st["stopLines"]["laneQueue"]["triggered"])

        # Human output check & ADR 0120: never auto-downgrades `.delegate.json`
        h_res = self.cli("fill", "stats", "--since", "24h", cwd=repo, human=True)
        self.assertEqual(h_res.returncode, 0, h_res.stdout + h_res.stderr)
        self.assertIn("target=6", h_res.stdout)
        self.assertIn("停止线：已触发", h_res.stdout)
        self.assertIn("不自动降档", h_res.stdout)
        self.assertIn("只读 run：2 次（占 agy 用量 5.0%）", h_res.stdout)
        self.assertEqual(cfg_path.read_text(encoding="utf-8"), orig_cfg)


if __name__ == "__main__":
    unittest.main()
