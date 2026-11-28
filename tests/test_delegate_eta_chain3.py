"""delegate ETA · Chain 3 integration tests (CLI display & docs):
1. `status` L0 appends one tail ETA segment, `status <run>` expands stage breakdown, `--json` includes structured `eta`.
2. `status` L0 & `--json` reflect live heavy-lane queue wait (`等 lane(前N)`).
3. `status` stop-clock attribution shows "卡在谁" (question & stallWarning) and never stops an active run.
4. `work` L0 headline appends DAG dev wall-clock + external wait / owner counts; L1 keeps 6 columns <= 100 chars with embedded focus badges (`~HH:MM`, `等Wxx(~HH:MM)`, `卡所有者Qxx`).
5. `work show <Wxx>` renders upstream critical path breakdown (`上游关键路径：W001 → W002 → W003`) and JSON `eta.upstreamBreakdown`.
6. `queue list` prints top `DAG 关键路径` headline (and stop-clock when paused) while `queue list --json` keeps `"name"` per NDJSON line and attaches `"eta"`.
7. `wait` and `wait --since` append a single tail ETA line (remaining wave + work chain) before `本会话还剩` / `继续等：`.
8. All Chain 3 read commands (`status`, `work`, `work show`, `queue list`) remain strictly zero-lock under held `.state.lock`.
"""
import datetime
import json
import subprocess
import sys
import unittest
from pathlib import Path

import test_delegate as fixtures


def iso_from_epoch(ts: int) -> str:
    return datetime.datetime.fromtimestamp(ts, tz=datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


class DelegateEtaChain3Tests(unittest.TestCase):
    def setUp(self):
        fixtures.DelegateTests.setUp(self)

    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until
    wait_until = fixtures.DelegateTests.wait_until
    wait_json = fixtures.DelegateTests.wait_json
    start_gated = fixtures.DelegateTests.start_gated
    hold_lane_gated = fixtures.DelegateTests.hold_lane_gated

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def make_repo(self) -> Path:
        return self.repo({"notes.txt": "initial\n"})

    def seed_events(self, repo: Path, count: int = 12, base_dur: int = 1200, cls: str = "build") -> int:
        now = int(datetime.datetime.now(tz=datetime.timezone.utc).timestamp())
        t0 = now - 3 * 86_400
        state = self.state_dir()
        state.mkdir(parents=True, exist_ok=True)
        lines = []
        seq = 0
        for i in range(count):
            rid = f"seed3-{i}"
            st = t0 + i * 3600
            work_dur = base_dur + (i % 4) * 120
            del_ts = st + work_dur + 180
            app_ts = del_ts + 300
            for ts, kind, payload in [
                (st, "started", {"mode": "write", "class": cls}),
                (del_ts - 180, "lane_acquired", {"queuedSeconds": 0.0}),
                (del_ts, "lane_released", {"heldSeconds": 180.0}),
                (del_ts, "accept", {"ok": True, "tiered": False, "elapsedSeconds": 180.0, "queuedSeconds": 0.0}),
                (del_ts, "delivered", {"mode": "write"}),
                (del_ts + 5, "premerge_ready", {"elapsedSeconds": 5.0, "reusedAccept": True}),
                (app_ts, "applied", {}),
            ]:
                seq += 1
                lines.append(
                    json.dumps(
                        {
                            "seq": seq,
                            "at": iso_from_epoch(ts),
                            "run": rid,
                            "repo": str(repo),
                            "kind": kind,
                            "payload": payload,
                        }
                    )
                )
        (state / "events.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")
        return now

    def test_status_l0_and_detail_and_json_show_live_eta(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, base_dur=1200, cls="build")
        model_path = self.state_dir() / "eta-model.json"
        if model_path.exists():
            model_path.unlink()

        state_doc, gate = self.start_gated(
            "--workdir",
            repo,
            "--class",
            "build",
            "--name",
            "status-eta-live",
            "implement feature",
            cwd=repo,
        )
        run_id = state_doc["run"]

        # L0 status appends a single tail ETA segment and lazily rebuilds deleted eta-model.json
        l0 = self.cli("status", cwd=repo, human=True)
        self.assertEqual(l0.returncode, 0, l0.stdout + l0.stderr)
        run_lines = [ln for ln in l0.stdout.splitlines() if "status-eta-live" in ln]
        self.assertEqual(len(run_lines), 1, l0.stdout)
        self.assertIn("预计 ", run_lines[0])
        self.assertIn("交付", run_lines[0])
        self.assertTrue(model_path.is_file(), "status should lazily rebuild deleted eta-model.json")

        # `status <run>` expands an indented `预估：` breakdown line
        detail = self.cli("status", run_id, cwd=repo, human=True)
        self.assertEqual(detail.returncode, 0, detail.stdout + detail.stderr)
        self.assertIn("  预估：", detail.stdout)
        self.assertIn("返工率", detail.stdout)

        # `status <run> --json` attaches structured `eta` object
        detail_json = self.cli("status", run_id, "--json", cwd=repo)
        self.assertEqual(detail_json.returncode, 0, detail_json.stdout + detail_json.stderr)
        doc = json.loads(detail_json.stdout)
        self.assertIn("eta", doc)
        self.assertEqual(doc["eta"]["stage"], "agent_work")
        self.assertFalse(doc["eta"]["stoppedClock"])
        self.assertGreater(doc["eta"]["toDelivered"]["p50Sec"], 0)
        self.assertIn("toApplied", doc["eta"])

        # `status --json` also attaches structured `eta` on the run line
        list_json = self.cli("status", "--json", cwd=repo)
        self.assertEqual(list_json.returncode, 0, list_json.stdout + list_json.stderr)
        objs = [json.loads(ln) for ln in list_json.stdout.splitlines() if ln.strip()]
        run_obj = next(o for o in objs if o.get("run") == run_id)
        self.assertEqual(run_obj["eta"]["stage"], "agent_work")

        gate.touch()
        self.cli("wait", "--no-result", run_id, cwd=repo)

    def test_status_l0_shows_lane_wait_adjustment(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, base_dur=900, cls="build")
        self.env["DELEGATE_MAX_HEAVY"] = "1"

        _lane_proc, lane_gate = self.hold_lane_gated(label="heavy-holder")
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre="echo lane-edit >> notes.txt",
        )

        started = self.outcome(
            self.cli(
                "start",
                "--workdir",
                repo,
                "--worktree",
                "--class",
                "build",
                "--name",
                "lane-wait-status",
                "--accept",
                "true",
                "edit notes",
                cwd=repo,
            )
        )
        run_id = started["run"]

        def queued_on_lane():
            res = self.cli("status", run_id, "--json", cwd=repo)
            if res.returncode != 0 or not res.stdout.strip():
                return False
            doc = json.loads(res.stdout)
            return doc.get("eta", {}).get("stage") == "lane_queue"

        self.wait_until(queued_on_lane, "run enters lane_queue in status --json", timeout=15)

        l0 = self.cli("status", cwd=repo, human=True)
        self.assertEqual(l0.returncode, 0, l0.stdout + l0.stderr)
        self.assertIn("等 lane(前", l0.stdout)

        det_json = json.loads(self.cli("status", run_id, "--json", cwd=repo).stdout)
        self.assertEqual(det_json["eta"]["stage"], "lane_queue")
        self.assertGreaterEqual(det_json["eta"]["laneWait"]["ahead"], 1)

        lane_gate.touch()
        self.cli("wait", "--no-result", run_id, cwd=repo)

    def test_status_stop_clock_shows_who_is_blocking_without_stopping_run(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, base_dur=900, cls="build")

        # 1) Question run -> stop-clock attributed to controller
        self.fake_pi([fixtures.answer("需要主控决定\n请问继续吗？"), fixtures.SETTLED])
        q_run = self.outcome(
            self.cli("run", "--workdir", repo, "--read-only", "--name", "ask-ctrl", "ask", cwd=repo)
        )
        self.assertTrue(q_run.get("question"))

        q_l0 = self.cli("status", cwd=repo, human=True)
        self.assertEqual(q_l0.returncode, 0, q_l0.stdout + q_l0.stderr)
        self.assertIn("卡在主控·提问", q_l0.stdout)

        q_det = self.cli("status", q_run["run"], cwd=repo, human=True)
        self.assertIn("  停表：卡在主控·提问", q_det.stdout)

        q_json = json.loads(self.cli("status", q_run["run"], "--json", cwd=repo).stdout)
        self.assertTrue(q_json["eta"]["stoppedClock"])
        self.assertEqual(q_json["eta"]["blocker"]["kind"], "controller_question")

        # 2) Active run with stallWarning -> stop-clock, run stays alive (never stopped by ETA)
        live_doc, gate = self.start_gated(
            "--workdir",
            repo,
            "--class",
            "build",
            "--name",
            "stalled-live",
            "long task",
            cwd=repo,
        )
        live_id = live_doc["run"]
        run_dir = Path(live_doc["dir"])
        (run_dir / "stall-warning.json").write_text(
            json.dumps({"kind": "idle", "detail": "无新输出 15 分钟", "elapsedSeconds": 900}),
            encoding="utf-8",
        )

        s_l0 = self.cli("status", cwd=repo, human=True)
        self.assertIn("⚠ 疑似卡住·停表", s_l0.stdout)
        s_json = json.loads(self.cli("status", live_id, "--json", cwd=repo).stdout)
        self.assertEqual(s_json["state"], "running", "ETA/stall must never stop the run")
        self.assertTrue(s_json["eta"]["stoppedClock"])
        self.assertEqual(s_json["eta"]["blocker"]["kind"], "stall_warning")

        gate.touch()
        self.cli("wait", "--no-result", live_id, cwd=repo)

    def test_work_list_l0_headline_and_l1_focus_badges_under_100_chars(self):
        repo = self.make_repo()
        now = self.seed_events(repo, count=12, base_dur=1200, cls="build")

        live_doc, gate = self.start_gated(
            "--workdir",
            repo,
            "--class",
            "build",
            "--name",
            "w1-live",
            "work item 1",
            cwd=repo,
        )
        live_id = live_doc["run"]

        # Create W001 (active), W002 (blocked on W001), W003 (after_time future), W004 (owner Q001)
        w1 = json.loads(
            self.cli("work", "add", "基础改造链一", "--repo", str(repo), "--class", "feat", "--run", live_id, "--json", cwd=repo).stdout
        )
        w2 = json.loads(
            self.cli(
                "work",
                "add",
                "这是一条标题很长的下游工作项用来验证六列和一百字预算",
                "--repo",
                str(repo),
                "--class",
                "feat",
                "--brief",
                "docs/w2.md",
                "--after-applied",
                w1["id"],
                "--json",
                cwd=repo,
            ).stdout
        )
        future_iso = iso_from_epoch(now + 4 * 3600)
        w3 = json.loads(
            self.cli(
                "work",
                "add",
                "定时发版窗口",
                "--repo",
                str(repo),
                "--class",
                "infra",
                "--brief",
                "docs/w3.md",
                "--after-time",
                future_iso,
                "--json",
                cwd=repo,
            ).stdout
        )
        w4_res = self.cli(
            "work",
            "add",
            "需要所有者拍板方案",
            "--repo",
            str(repo),
            "--class",
            "feat",
            "--needs-owner",
            "nod",
            "--ask",
            "是否采用方案 A？",
            "--recommendation",
            "采用方案 A",
            "--if-undecided",
            "延后上线",
            "--cost",
            "1 条写链",
            "--json",
            cwd=repo,
        )
        self.assertEqual(w4_res.returncode, 0, w4_res.stdout + w4_res.stderr)
        w4 = json.loads(w4_res.stdout.splitlines()[0])
        owner_cond = next(c for c in w4["waitOn"] if c.startswith("owner:"))
        q_id = owner_cond.split(":", 1)[1]

        human = self.cli("work", "--all", cwd=repo, human=True)
        self.assertEqual(human.returncode, 0, human.stdout + human.stderr)
        lines = human.stdout.splitlines()
        self.assertGreaterEqual(len(lines), 5, human.stdout)

        # L0 headline has dev wall-clock + external wait + owner blocked
        l0 = lines[0]
        self.assertIn("工作表：需处理 ", l0)
        self.assertIn("推进 ", l0)
        self.assertIn("写链/深2", l0)
        self.assertIn("外部等待 1", l0)
        self.assertIn("卡所有者 1", l0)

        # Every L1 table row stays <= 100 chars and W rows keep exact 6 columns (no extra ETA column)
        row_lines = [ln for ln in lines[1:] if ln.startswith("[W") or ln.startswith("[Q") or ln.startswith("[R:")]
        for rl in row_lines:
            self.assertLessEqual(len(rl), 100, f"row exceeded 100 chars ({len(rl)}): {rl}")
            if rl.startswith("[W"):
                self.assertEqual(len(rl.split(" · ")), 4, f"expected 6-column format (4 dot-separated segments): {rl}")

        w1_line = next(ln for ln in row_lines if ln.startswith(f"[{w1['id']}]"))
        self.assertTrue("~" in w1_line or "<5m" in w1_line, w1_line)

        w2_line = next(ln for ln in row_lines if ln.startswith(f"[{w2['id']}]"))
        self.assertIn(f"等{w1['id']}(~", w2_line)

        w4_line = next(ln for ln in row_lines if ln.startswith(f"[{w4['id']}]"))
        self.assertIn(f"卡所有者{q_id}", w4_line)

        # JSON output includes workSummary.eta and per-item eta
        jres = self.cli("work", "--all", "--json", cwd=repo)
        self.assertEqual(jres.returncode, 0, jres.stdout + jres.stderr)
        jlines = [json.loads(ln) for ln in jres.stdout.splitlines() if ln.strip()]
        summary = next(o["workSummary"] for o in jlines if "workSummary" in o)
        self.assertIn("eta", summary)
        self.assertEqual(summary["eta"]["externalWaitCount"], 1)
        self.assertEqual(len(summary["eta"]["ownerBlocked"]), 1)
        w2_obj = next(o for o in jlines if o.get("id") == w2["id"])
        self.assertEqual(w2_obj["eta"]["upstreamPath"], [w1["id"], w2["id"]])
        self.assertIsNotNone(w3)

        gate.touch()
        self.cli("wait", "--no-result", live_id, cwd=repo)

    def test_work_show_lists_upstream_critical_path_breakdown(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, base_dur=1200, cls="build")

        w1 = json.loads(self.cli("work", "add", "步骤一底座", "--repo", str(repo), "--class", "feat", "--brief", "docs/s1.md", "--json", cwd=repo).stdout)
        w2 = json.loads(
            self.cli("work", "add", "步骤二引擎", "--repo", str(repo), "--class", "feat", "--brief", "docs/s2.md", "--after-applied", w1["id"], "--json", cwd=repo).stdout
        )
        w3 = json.loads(
            self.cli("work", "add", "步骤三展示", "--repo", str(repo), "--class", "feat", "--brief", "docs/s3.md", "--after-applied", w2["id"], "--json", cwd=repo).stdout
        )

        card = self.cli("work", "show", w3["id"], cwd=repo, human=True)
        self.assertEqual(card.returncode, 0, card.stdout + card.stderr)
        self.assertIn("预估：", card.stdout)
        self.assertIn(f"上游关键路径：{w1['id']} → {w2['id']} → {w3['id']}", card.stdout)
        self.assertIn(f"- [{w1['id']}] 步骤一底座", card.stdout)
        self.assertIn(f"- [{w2['id']}] 步骤二引擎", card.stdout)
        self.assertIn(f"- [{w3['id']}] 步骤三展示", card.stdout)

        card_json = json.loads(self.cli("work", "show", w3["id"], "--json", cwd=repo).stdout)
        self.assertEqual(card_json["eta"]["upstreamPath"], [w1["id"], w2["id"], w3["id"]])
        self.assertEqual(len(card_json["eta"]["upstreamBreakdown"]), 3)

    def test_queue_list_shows_top_dag_headline_and_paused_stop_clock(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, base_dur=1200, cls="build")

        up_doc, gate = self.start_gated(
            "--workdir",
            repo,
            "--class",
            "build",
            "--name",
            "q-step-1",
            "step 1",
            cwd=repo,
        )
        up_id = up_doc["run"]

        p2 = self.work / "q2.md"
        p3 = self.work / "q3.md"
        p2.write_text("step 2 prompt\n", encoding="utf-8")
        p3.write_text("step 3 prompt\n", encoding="utf-8")

        self.assertEqual(
            self.cli("queue", "add", "q-step-2", "--after", up_id, "--prompt-file", str(p2), cwd=repo).returncode,
            0,
        )
        self.assertEqual(
            self.cli("queue", "add", "q-step-3", "--after", "q-step-2", "--prompt-file", str(p3), cwd=repo).returncode,
            0,
        )

        # Human `queue list` puts DAG critical path headline on line 1
        listed = self.cli("queue", "list", cwd=repo, human=True)
        self.assertEqual(listed.returncode, 0, listed.stdout + listed.stderr)
        lines = listed.stdout.splitlines()
        self.assertTrue(lines[0].startswith("DAG 关键路径：剩 "), lines[0])
        self.assertIn("全部合入（p75 ", lines[0])
        self.assertIn("q-step-2", listed.stdout)
        self.assertIn("q-step-3", listed.stdout)

        # JSON `queue list --json` preserves `"name"` on every NDJSON line and attaches `"eta"`
        jlisted = self.cli("queue", "list", "--json", cwd=repo)
        self.assertEqual(jlisted.returncode, 0, jlisted.stdout + jlisted.stderr)
        by_name = {json.loads(ln)["name"]: json.loads(ln) for ln in jlisted.stdout.splitlines() if ln.strip()}
        self.assertIn("q-step-2", by_name)
        self.assertIn("q-step-3", by_name)
        self.assertIn("eta", by_name["q-step-2"])
        self.assertIn("eta", by_name["q-step-3"])

        # Release gate cleanly, then test paused queue item triggers stop-clock headline
        gate.touch()
        self.cli("wait", "--no-result", up_id, cwd=repo)

        # Pause a new queue item via upstream question
        self.fake_pi([fixtures.answer("需要主控决定\n是否放行？"), fixtures.SETTLED])
        ask_run = self.outcome(
            self.cli("run", "--workdir", repo, "--read-only", "--name", "q-ask", "ask", cwd=repo)
        )
        p4 = self.work / "q4.md"
        p4.write_text("step 4 prompt\n", encoding="utf-8")
        self.cli("queue", "add", "q-step-4", "--after", ask_run["run"], "--prompt-file", str(p4), cwd=repo)
        self.wait_until(
            lambda: any(
                json.loads(ln).get("name") == "q-step-4" and json.loads(ln).get("status") == "paused"
                for ln in self.cli("queue", "list", "--json", cwd=repo).stdout.splitlines()
                if ln.strip()
            ),
            "q-step-4 becomes paused",
            timeout=15,
        )
        paused_out = self.cli("queue", "list", cwd=repo, human=True)
        self.assertIn("停表", paused_out.stdout.splitlines()[0])

    def test_wait_and_wait_since_append_tail_eta_line(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, base_dur=1200, cls="build")

        # Launch two gated runs: `fast-done` will settle first while `slow-wave` keeps running
        fast_doc, fast_gate = self.start_gated(
            "--workdir",
            repo,
            "--class",
            "build",
            "--name",
            "fast-done",
            "finish first",
            cwd=repo,
        )
        slow_doc, slow_gate = self.start_gated(
            "--workdir",
            repo,
            "--class",
            "build",
            "--name",
            "slow-wave",
            "keep running",
            cwd=repo,
        )
        fast_id = fast_doc["run"]
        slow_id = slow_doc["run"]

        # Create a 2-step work chain bound to `slow-wave`
        w1 = json.loads(
            self.cli("work", "add", "在跑链头", "--repo", str(repo), "--class", "feat", "--run", slow_id, "--json", cwd=repo).stdout
        )
        w2 = json.loads(
            self.cli("work", "add", "下游链尾", "--repo", str(repo), "--class", "feat", "--brief", "docs/tail.md", "--after-applied", w1["id"], "--json", cwd=repo).stdout
        )

        # Capture current event cursor before releasing fast-done
        ev_res = self.cli("events", "--json", cwd=repo)
        cur_seq = max((json.loads(ln)["seq"] for ln in ev_res.stdout.splitlines() if ln.strip()), default=0)

        fast_gate.touch()

        # 1) `delegate wait` appends tail ETA line (remaining wave + work chain) before `本会话还剩`
        w_out = self.cli("wait", cwd=repo, human=True)
        self.assertEqual(w_out.returncode, 0, w_out.stdout + w_out.stderr)
        self.assertIn("其余 1 条在跑：slow-wave 预计 ", w_out.stdout)
        self.assertIn(f"链 [{w1['id']}→{w2['id']}] 剩 2 步，约 ", w_out.stdout)
        self.assertIn("本会话还剩 1 个任务在跑", w_out.stdout.splitlines()[-1])

        # 2) `delegate wait --since <cur_seq>` also appends tail ETA line before `继续等：`
        ws_out = self.cli("wait", "--since", str(cur_seq), "--max", "5s", cwd=repo, human=True)
        self.assertEqual(ws_out.returncode, 0, ws_out.stdout + ws_out.stderr)
        self.assertIn("其余 1 条在跑：slow-wave 预计 ", ws_out.stdout)
        self.assertIn(f"链 [{w1['id']}→{w2['id']}] 剩 2 步，约 ", ws_out.stdout)
        self.assertIn("继续等：", ws_out.stdout.splitlines()[-1])

        slow_gate.touch()
        self.cli("wait", "--no-result", slow_id, cwd=repo)
        self.assertIsNotNone(fast_id)

    def test_chain3_cli_reads_are_strictly_lock_free_under_held_state_lock(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, base_dur=900, cls="build")

        live_doc, gate = self.start_gated(
            "--workdir",
            repo,
            "--class",
            "build",
            "--name",
            "lockfree-run",
            "work",
            cwd=repo,
        )
        live_id = live_doc["run"]
        w1 = json.loads(
            self.cli("work", "add", "无锁项", "--repo", str(repo), "--class", "feat", "--run", live_id, "--json", cwd=repo).stdout
        )
        pq = self.work / "q-lf.md"
        pq.write_text("next\n", encoding="utf-8")
        self.assertEqual(
            self.cli("queue", "add", "lf-next", "--after", live_id, "--prompt-file", str(pq), cwd=repo).returncode,
            0,
        )

        state = self.state_dir()
        lock_file = state / ".state.lock"
        ready_file = state / ".c3-hold-ready"
        release_file = state / ".c3-hold-release"
        probe_env = {**self.env, "DELEGATE_MEASURE_STATE_LOCK": "1"}

        holder_code = (
            "import fcntl, os, sys, time\n"
            "p, ready, rel = sys.argv[1], sys.argv[2], sys.argv[3]\n"
            "fd = os.open(p, os.O_CREAT | os.O_RDWR, 0o644)\n"
            "fcntl.flock(fd, fcntl.LOCK_EX)\n"
            "open(ready, 'w').write('held')\n"
            "deadline = time.monotonic() + 30.0\n"
            "while not os.path.exists(rel) and time.monotonic() < deadline:\n"
            "    time.sleep(0.02)\n"
            "os.close(fd)\n"
        )
        holder = subprocess.Popen(
            [sys.executable, "-c", holder_code, str(lock_file), str(ready_file), str(release_file)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            self.wait_until(ready_file.is_file, "state lock holder ready", timeout=5.0)
            for cmd in (
                ["status"],
                ["status", "--json"],
                ["status", live_id],
                ["work", "--all"],
                ["work", "--all", "--json"],
                ["work", "show", w1["id"]],
                ["queue", "list"],
                ["queue", "list", "--json"],
            ):
                res = subprocess.run(
                    [str(fixtures.DELEGATE), *cmd],
                    env=probe_env,
                    cwd=repo,
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                self.assertEqual(res.returncode, 0, f"{cmd} failed under held lock: {res.stderr}")
                self.assertNotIn("STATE_LOCK_ATTEMPT", res.stderr, f"{cmd} touched .state.lock: {res.stderr}")
            self.assertIsNone(holder.poll(), "holder must still hold .state.lock")
        finally:
            release_file.touch()
            holder.wait(timeout=5)
            for stream in (holder.stdout, holder.stderr):
                if stream:
                    stream.close()
            self.cli("queue", "remove", "lf-next", cwd=repo)
            gate.touch()
            self.cli("wait", "--no-result", live_id, cwd=repo)


if __name__ == "__main__":
    unittest.main()
