"""delegate ETA · Chain 2 integration tests:
1. Single active run ETA (conditional residual life, L1 bucket, lazy rebuild after deleting eta-model.json).
2. Real-time heavy lane queue adjustment while another holder occupies the lane (`hold_lane_gated`).
3. Real-time capacity queue adjustment (`start_gated` + `DELEGATE_MAX_ACTIVE=1`) and memory-wait stop-clock.
4. Stop-clock blocker attribution ("卡在谁"): question, premerge conflict, stall warning (never stops run),
   and repo-wide mainline block (>=2 runs failing on the same outside file).
5. Work-item DAG topological critical path: `after_time:` EST separated from `devWall`, `owner:Qxx`
   stripped into `ownerBlocked`, and `class == "unblock"` upstream dependency marked `卡在主干修复`.
6. Continuation queue (`dag-queue.json`) topological critical path and paused stop-clock.
7. Strict zero-lock reads (`delegate eta`, `--work`, `--queue`, `speed --backtest`) under held `.state.lock`.
8. `delegate speed --backtest` rolling offline backtest (strictly causal, p75 coverage & p50 bias, no speed history mutation).
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


class DelegateEtaChain2Tests(unittest.TestCase):
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

    def seed_events(self, repo: Path, count: int = 12, base_dur: int = 900, cls: str = "build") -> int:
        now = int(datetime.datetime.now(tz=datetime.timezone.utc).timestamp())
        t0 = now - 3 * 86_400
        state = self.state_dir()
        state.mkdir(parents=True, exist_ok=True)
        lines = []
        seq = 0
        for i in range(count):
            rid = f"seed-{i}"
            st = t0 + i * 3600
            work_dur = base_dur + (i % 4) * 60
            del_ts = st + work_dur + 120
            app_ts = del_ts + 300
            for ts, kind, payload in [
                (st, "started", {"mode": "write", "class": cls}),
                (del_ts - 120, "lane_acquired", {"queuedSeconds": 0.0}),
                (del_ts, "lane_released", {"heldSeconds": 120.0}),
                (del_ts, "accept", {"ok": True, "tiered": False, "elapsedSeconds": 120.0, "queuedSeconds": 0.0}),
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

    def test_eta_single_running_run_conditional_residual_and_rebuilds_deleted_model(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, base_dur=900, cls="build")
        model_path = self.state_dir() / "eta-model.json"
        if model_path.exists():
            model_path.unlink()

        state_doc, gate = self.start_gated(
            "--workdir",
            repo,
            "--class",
            "build",
            "--name",
            "eta-live-work",
            "implement feature",
            cwd=repo,
        )
        run_id = state_doc["run"]

        res = self.cli("eta", run_id, "--json", cwd=repo)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        doc = json.loads(res.stdout)
        self.assertEqual(doc["kind"], "run")
        r = doc["run"]
        self.assertEqual(r["stage"], "agent_work")
        self.assertFalse(r["stoppedClock"])
        self.assertEqual(r["fallbackLevel"], "L1")
        self.assertIn("toDelivered", r)
        self.assertIn("toApplied", r)
        self.assertGreater(r["toDelivered"]["p50Sec"], 0)
        self.assertGreaterEqual(r["toDelivered"]["p75Sec"], r["toDelivered"]["p50Sec"])
        self.assertTrue(model_path.is_file(), "eta should lazily rebuild deleted eta-model.json")

        human = self.cli("eta", run_id, cwd=repo)
        self.assertEqual(human.returncode, 0, human.stdout + human.stderr)
        self.assertIn("交付", human.stdout)

        gate.touch()
        self.wait_json(
            Path(state_doc["dir"]) / "summary.json",
            lambda d: d.get("state") in ("delivered", "answered", "rejected", "failed"),
            "gated run finishes",
            timeout=15,
        )

    def test_eta_lane_queue_real_time_holder_adjustment(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, base_dur=600, cls="build")
        # Hold the heavy lane so the run's acceptance command blocks in lane-waiting-*
        _holder, lane_gate = self.hold_lane_gated(label="eta-heavy-holder")

        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre="echo updated >> notes.txt",
        )
        res = self.cli(
            "start",
            "--worktree",
            "--workdir",
            repo,
            "--class",
            "build",
            "--accept",
            "true",
            "--name",
            "eta-lane-waiter",
            "change notes",
            cwd=repo,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        run_state = self.outcome(res)
        run_id = run_state["run"]
        run_dir = Path(run_state["dir"])
        self.addCleanup(lambda: self.cli("stop", str(run_dir)) if run_dir.exists() else None)

        self.wait_until(
            lambda: f"accept {run_dir.name}" in self.cli("lane").stdout,
            "run enters heavy lane queue",
            timeout=15,
        )

        eta_res = self.cli("eta", run_id, "--json", cwd=repo)
        self.assertEqual(eta_res.returncode, 0, eta_res.stdout + eta_res.stderr)
        r = json.loads(eta_res.stdout)["run"]
        self.assertEqual(r["stage"], "lane_queue", r)
        self.assertFalse(r["stoppedClock"])
        self.assertIn("laneWait", r)
        self.assertGreaterEqual(r["laneWait"]["ahead"], 1)
        self.assertGreaterEqual(r["laneWait"]["holders"], 1)
        self.assertGreater(r["laneWait"]["waitP50Sec"], 0)
        self.assertIn("等 lane", r["shortText"])

        lane_gate.touch()
        self.wait_json(
            run_dir / "summary.json",
            lambda d: d.get("state") == "delivered",
            "lane waiter delivers after holder releases",
            timeout=15,
        )

    def test_eta_capacity_queue_live_adjustment_and_memory_wait_stop_clock(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, base_dur=900, cls="build")
        self.env["DELEGATE_MIN_AVAILABLE_MB"] = "0"
        self.env["DELEGATE_MAX_ACTIVE"] = "1"

        first_state, first_gate = self.start_gated(
            "--workdir",
            repo,
            "--class",
            "build",
            "--name",
            "cap-holder",
            "hold capacity slot",
            cwd=repo,
        )

        q_res = self.cli(
            "start",
            "--workdir",
            repo,
            "--class",
            "build",
            "--name",
            "cap-waiter",
            "wait for capacity",
            cwd=repo,
        )
        self.assertEqual(q_res.returncode, 0, q_res.stdout + q_res.stderr)
        q_state = self.outcome(q_res)
        q_id = q_state["run"]
        q_dir = Path(q_state["dir"])
        self.addCleanup(
            lambda: (
                (q_dir / ".fake-gate").touch() if q_dir.exists() else None,
                self.cli("stop", str(q_dir)) if q_dir.exists() else None,
            )
        )

        self.wait_until(
            lambda: (q_dir / ".waiting").exists() and (q_dir / "queue.json").is_file(),
            "second run enters capacity queue",
            timeout=15,
        )

        eta_res = self.cli("eta", q_id, "--json", cwd=repo)
        self.assertEqual(eta_res.returncode, 0, eta_res.stdout + eta_res.stderr)
        r = json.loads(eta_res.stdout)["run"]
        self.assertEqual(r["stage"], "capacity_queue", r)
        self.assertFalse(r["stoppedClock"])
        self.assertIn("capacityWait", r)
        self.assertGreater(r["capacityWait"]["waitP50Sec"], 0)

        # Release capacity-queued runs cleanly first.
        first_gate.touch()
        self.wait_json(
            Path(first_state["dir"]) / "summary.json",
            lambda d: bool(d.get("state")),
            "first run exits",
            timeout=15,
        )
        (q_dir / ".fake-gate").touch()
        self.wait_json(
            q_dir / "summary.json",
            lambda d: bool(d.get("state")),
            "queued run exits",
            timeout=15,
        )

        # Real memory-wait queue via DELEGATE_MIN_AVAILABLE_MB: must stop clock and NOT guess minutes!
        self.env["DELEGATE_MAX_ACTIVE"] = "4"
        self.env["DELEGATE_MIN_AVAILABLE_MB"] = "999999999"
        mem_start = self.cli(
            "start",
            "--workdir",
            repo,
            "--class",
            "build",
            "--name",
            "mem-waiter",
            "wait for memory",
            cwd=repo,
        )
        self.assertEqual(mem_start.returncode, 0, mem_start.stdout + mem_start.stderr)
        mem_state = self.outcome(mem_start)
        mem_id = mem_state["run"]
        mem_dir = Path(mem_state["dir"])
        self.addCleanup(lambda: self.cli("stop", str(mem_dir)) if mem_dir.exists() else None)

        self.wait_json(
            mem_dir / "queue.json",
            lambda d: "内存" in d.get("reason", "") or "memory" in d.get("reason", "").lower(),
            "memory queue reason recorded",
            timeout=15,
        )
        mem_res = self.cli("eta", mem_id, "--json", cwd=repo)
        self.assertEqual(mem_res.returncode, 0, mem_res.stdout + mem_res.stderr)
        mr = json.loads(mem_res.stdout)["run"]
        self.assertTrue(mr["stoppedClock"], mr)
        self.assertEqual(mr["blocker"]["kind"], "memory_wait")
        self.assertEqual(mr["blocker"]["label"], "等内存释放")
        self.assertNotIn("toDelivered", mr)
        self.cli("stop", str(mem_dir))

    def test_eta_stop_clock_blockers_question_conflict_stall_and_mainline(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, base_dur=900, cls="build")

        # 1. Question run -> 卡在主控·提问
        self.fake_pi([fixtures.answer("需要主控决定\n选 A 还是 B？"), fixtures.SETTLED])
        qres = self.cli("run", "--workdir", repo, "--name", "eta-q", "ask", cwd=repo)
        self.assertEqual(qres.returncode, 0, qres.stdout + qres.stderr)
        qid = self.outcome(qres)["run"]
        q_eta = json.loads(self.cli("eta", qid, "--json", cwd=repo).stdout)["run"]
        self.assertTrue(q_eta["stoppedClock"])
        self.assertEqual(q_eta["blocker"]["kind"], "controller_question")
        self.assertIn("卡在主控·提问", q_eta["blocker"]["label"])

        # 2. Delivered write run with premerge conflict -> 卡在预合并冲突
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre="echo branch >> notes.txt",
        )
        dres = self.cli("run", "--worktree", "--workdir", repo, "--name", "eta-pm-conf", "edit", cwd=repo)
        self.assertEqual(dres.returncode, 0, dres.stdout + dres.stderr)
        ddoc = self.outcome(dres)
        ddir = Path(ddoc["dir"])
        (ddir / "premerge.json").write_text(
            json.dumps({"status": "conflict", "conflicts": ["notes.txt"]}),
            encoding="utf-8",
        )
        c_eta = json.loads(self.cli("eta", ddoc["run"], "--json", cwd=repo).stdout)["run"]
        self.assertTrue(c_eta["stoppedClock"])
        self.assertEqual(c_eta["blocker"]["kind"], "premerge_conflict")
        self.assertIn("卡在预合并冲突", c_eta["blocker"]["label"])

        # 3. Active gated run with stall-warning.json -> 疑似卡住·停表, and ETA must NOT stop the run!
        s_state, s_gate = self.start_gated("--workdir", repo, "--name", "eta-stall", "work", cwd=repo)
        s_dir = Path(s_state["dir"])
        (s_dir / "stall-warning.json").write_text(
            json.dumps({"elapsedSeconds": 1200, "idleSeconds": 950}),
            encoding="utf-8",
        )
        s_eta = json.loads(self.cli("eta", s_state["run"], "--json", cwd=repo).stdout)["run"]
        self.assertTrue(s_eta["stoppedClock"])
        self.assertEqual(s_eta["blocker"]["kind"], "stall_warning")
        self.assertIn("疑似卡住·停表", s_eta["blocker"]["label"])
        # Confirm run is still active (ETA never stops tasks!)
        st_after = json.loads(self.cli("status", s_state["run"], "--json", cwd=repo).stdout)
        self.assertEqual(st_after["state"], "running")
        s_gate.touch()
        self.wait_json(s_dir / "summary.json", lambda d: bool(d.get("state")), "stalled run exits", timeout=15)

        # 4. Two rejected runs in same repo with failureAttribution pointing to same outside file
        now_iso = iso_from_epoch(int(datetime.datetime.now(tz=datetime.timezone.utc).timestamp()) - 180)
        runs_root = ddir.parent
        for idx in (1, 2):
            mdir = runs_root / f"20261125-00000{idx}-ml-{idx}"
            mdir.mkdir(parents=True, exist_ok=True)
            (mdir / "meta.json").write_text(
                json.dumps({"run": mdir.name, "name": f"ml-{idx}", "mode": "write", "top": str(repo)}),
                encoding="utf-8",
            )
            (mdir / "summary.json").write_text(
                json.dumps(
                    {
                        "state": "rejected",
                        "finishedAt": now_iso,
                        "accept": {
                            "ok": False,
                            "failureAttribution": {"outsideRunFiles": ["scripts/size-audit.py"]},
                        },
                    }
                ),
                encoding="utf-8",
            )
        m_eta = json.loads(self.cli("eta", "20261125-000001-ml-1", "--json", cwd=repo).stdout)["run"]
        self.assertTrue(m_eta["stoppedClock"])
        self.assertEqual(m_eta["blocker"]["kind"], "mainline_blocked")
        self.assertEqual(m_eta["blocker"]["affectedCount"], 2)
        self.assertIn("卡在主干阻塞（2 条受阻）", m_eta["blocker"]["label"])

    def test_eta_work_dag_separates_after_time_est_owner_blockers_and_unblock_dep(self):
        repo = self.make_repo()
        now = self.seed_events(repo, count=12, base_dur=900, cls="build")

        def work_add(*args) -> str:
            r = self.cli("work", "add", "--json", *args, cwd=repo)
            self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
            d = json.loads(r.stdout)
            return d.get("id") or d["item"]["id"]

        w1 = work_add("--class", "feat", "chain 1")
        future_iso = iso_from_epoch(now + 6 * 3600)
        w2 = work_add("--class", "feat", "--after-applied", w1, "--after-time", future_iso, "chain 2 timed")
        w3 = work_add(
            "--class",
            "feat",
            "--needs-owner",
            "nod",
            "--ask",
            "是否允许改协议？",
            "--recommendation",
            "允许",
            "--if-undecided",
            "保持现状",
            "--cost",
            "低",
            "owner gated feature",
        )
        w_unblock = work_add("--class", "unblock", "fix mainline size-audit")
        w_dep = work_add("--class", "feat", "--after-applied", w_unblock, "blocked by mainline fix")

        dag_res = self.cli("eta", "--work", "--json", cwd=repo)
        self.assertEqual(dag_res.returncode, 0, dag_res.stdout + dag_res.stderr)
        dag = json.loads(dag_res.stdout)["work"]

        # Owner-blocked item W3 is stripped from devWall into ownerBlocked!
        self.assertTrue(any(b["id"] == w3 and "卡在所有者" in b["blocker"]["label"] for b in dag["ownerBlocked"]), dag)
        # External timed wait (W2) counted in externalWaitCount and excluded from active DAG totals,
        # while per-item W2 remaining still reflects its >= 6h EST.
        self.assertGreaterEqual(dag["externalWaitCount"], 1)
        self.assertLess(dag["devWallP50Sec"], 4 * 3600, dag)
        fin50_iso = dag["earliestFinishP50At"]
        fin50_ts = int(datetime.datetime.fromisoformat(fin50_iso.replace("Z", "+00:00")).timestamp())
        self.assertLess(fin50_ts, now + 4 * 3600, dag)
        w2_item = next(it for it in dag["items"] if it["id"] == w2)
        w2_fin50_ts = int(
            datetime.datetime.fromisoformat(w2_item["remaining"]["p50At"].replace("Z", "+00:00")).timestamp()
        )
        self.assertGreaterEqual(w2_fin50_ts, now + 6 * 3600, w2_item)

        # Single work item query for W_dep shows unblock dependency
        dep_res = self.cli("eta", w_dep, "--json", cwd=repo)
        self.assertEqual(dep_res.returncode, 0, dep_res.stdout + dep_res.stderr)
        dep_item = json.loads(dep_res.stdout)["workItem"]
        self.assertTrue(dep_item["stoppedClock"])
        self.assertEqual(dep_item["blocker"]["kind"], "mainline_blocked")
        self.assertIn(f"卡在主干修复 [{w_unblock}]", dep_item["blocker"]["label"])
        self.assertIn("unlockClock", dep_item["blocker"])
        self.assertIn(w2, [it["id"] for it in dag["items"]])

    def test_eta_queue_dag_critical_path_and_paused_stop_clock(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, base_dur=900, cls="build")
        qpath = self.state_dir() / "dag-queue.json"
        qpath.write_text(
            json.dumps(
                {
                    "items": [
                        {"name": "s1", "repo": str(repo), "status": "pending", "after": [], "afterApplied": []},
                        {"name": "s2a", "repo": str(repo), "status": "pending", "after": [], "afterApplied": ["s1"]},
                        {"name": "s2b", "repo": str(repo), "status": "pending", "after": [], "afterApplied": ["s1"]},
                        {"name": "s3", "repo": str(repo), "status": "pending", "after": [], "afterApplied": ["s2a", "s2b"]},
                    ]
                }
            ),
            encoding="utf-8",
        )

        res = self.cli("eta", "--queue", "--json", cwd=repo)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        q = json.loads(res.stdout)["queue"]
        self.assertEqual(q["writeChains"], 4)
        self.assertEqual(q["depth"], 3)
        self.assertIn("DAG 关键路径：剩 3 步", q["headline"])

        # Pause s1 -> stop-clock blocker surfaces
        qpath.write_text(
            json.dumps(
                {
                    "items": [
                        {
                            "name": "s1",
                            "repo": str(repo),
                            "status": "paused",
                            "pauseReason": "上游提问，等待主控决定",
                            "after": [],
                            "afterApplied": [],
                        }
                    ]
                }
            ),
            encoding="utf-8",
        )
        pres = self.cli("eta", "--queue", "--json", cwd=repo)
        self.assertEqual(pres.returncode, 0, pres.stdout + pres.stderr)
        pq = json.loads(pres.stdout)["queue"]
        self.assertEqual(len(pq["blockers"]), 1)
        self.assertIn("停表", pq["headline"])

    def test_eta_and_backtest_are_strictly_lock_free_under_held_state_lock(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, base_dur=900, cls="build")
        state = self.state_dir()
        lock_file = state / ".state.lock"
        ready_file = state / ".eta-lock-ready"
        release_file = state / ".eta-lock-release"
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
            env=probe_env,
        )
        try:
            self.wait_until(lambda: ready_file.is_file(), "state lock holder acquired LOCK_EX", timeout=10)
            for cmd in (
                ["eta", "--json"],
                ["eta", "--work", "--json"],
                ["eta", "--queue", "--json"],
                ["speed", "--backtest", "--json"],
            ):
                res = subprocess.run(
                    [str(fixtures.DELEGATE), *cmd],
                    env=probe_env,
                    cwd=repo,
                    capture_output=True,
                    text=True,
                    timeout=10,
                )
                self.assertEqual(res.returncode, 0, f"{cmd} failed under held state lock: {res.stderr}")
                self.assertNotIn("STATE_LOCK_ATTEMPT", res.stderr, f"{cmd} touched .state.lock: {res.stderr}")
            self.assertIsNone(holder.poll(), "state lock holder should still be holding LOCK_EX")
        finally:
            release_file.touch()
            holder.wait(timeout=5)

    def test_speed_backtest_cli_strictly_causal_and_no_history_mutation(self):
        repo = self.make_repo()
        self.seed_events(repo, count=15, base_dur=900, cls="build")

        res = self.cli("speed", "--backtest", "--json", cwd=repo)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        doc = json.loads(res.stdout)
        self.assertEqual(doc["window"], "14d")
        self.assertEqual(doc["start"]["count"], 15)
        self.assertEqual(doc["mid"]["count"], 15)
        self.assertIsNotNone(doc["start"]["p75CoveragePct"])
        self.assertIsNotNone(doc["start"]["p50RelBiasPct"])
        self.assertIn("L1", doc["start"]["byLevel"])
        self.assertEqual(len(doc["repos"]), 1)

        # Backtest must be pure read-only: must NOT write `<state>/speed/*.jsonl` history.
        speed_dir = self.state_dir() / "speed"
        self.assertFalse(speed_dir.exists() and list(speed_dir.glob("*.jsonl")), "backtest must not mutate speed history")

        human = self.cli("speed", "--backtest", cwd=repo)
        self.assertEqual(human.returncode, 0, human.stdout + human.stderr)
        self.assertIn("delegate speed --backtest", human.stdout)
        self.assertIn("p75 覆盖", human.stdout)
        self.assertIn("p50 偏差", human.stdout)


if __name__ == "__main__":
    unittest.main()
