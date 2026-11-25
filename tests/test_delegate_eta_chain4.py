"""delegate ETA · Chain 4 integration tests:
1. `delegate snapshot --json` exposes per-run `eta`, per-work `eta`, and top-level `eta`
   (`wave`, `work`, `queue`) with zero locks, zero disk writes, and zero git calls.
2. Stop hook (`delegate hook check --format claude|agy|json` & `claude-code-stop.py`)
   attaches fastest delivery/answer ETA clock to existing `waiter` reminders with zero locks,
   zero git, zero new blocking conditions, and omits countdown when all uncovered runs are
   stopped-clock (`stallWarning`).
3. Online milestone calibration writes `<run>/eta-pred.json` at `started` & first `accept/review`,
   appends prediction-vs-actual records to `<state>/eta-calibration.jsonl` at `delivered` &
   `applied`, and surfaces `ETA p75覆盖` + `etaCalibration` in `delegate speed`.
4. Optimistic online calibration (<60% p75 coverage) emits advisory `⚠` alert without stopping
   tasks, and deleting derived `eta-calibration.jsonl` / `eta-model.json` degrades cleanly.
"""
import datetime
import json
import os
import subprocess
import sys
import unittest
from pathlib import Path

import test_delegate as fixtures

STOP_HOOK = Path(__file__).resolve().parent.parent / "skills" / "delegate" / "hooks" / "claude-code-stop.py"


def iso_from_epoch(ts: int) -> str:
    return datetime.datetime.fromtimestamp(ts, tz=datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def make_nogit_dir(base: Path) -> Path:
    nogit = base / "nogit-bin"
    nogit.mkdir(parents=True, exist_ok=True)
    bad_git = nogit / "git"
    bad_git.write_text("#!/bin/sh\necho 'git forbidden' >&2\nexit 99\n", encoding="utf-8")
    bad_git.chmod(0o755)
    return nogit


class DelegateEtaChain4Tests(unittest.TestCase):
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

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def make_repo(self) -> Path:
        return self.repo({"notes.txt": "initial\n"})

    def seed_events(
        self,
        repo: Path,
        count: int = 12,
        write_work_sec: int = 900,
        ro_sec: int = 240,
        cls: str = "build",
    ) -> int:
        now = int(datetime.datetime.now(tz=datetime.timezone.utc).timestamp())
        t0 = now - 3 * 86_400
        state = self.state_dir()
        state.mkdir(parents=True, exist_ok=True)
        lines = []
        seq = 0
        for i in range(count):
            rid = f"seed-w-{i}"
            st = t0 + i * 3600
            work_dur = write_work_sec + (i % 4) * 60
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
            ro_id = f"seed-ro-{i}"
            ro_st = st + 1800
            for ts, kind, payload in [
                (ro_st, "started", {"mode": "read-only", "class": "scout"}),
                (ro_st + ro_sec + (i % 3) * 30, "delivered", {"mode": "read-only"}),
            ]:
                seq += 1
                lines.append(
                    json.dumps(
                        {
                            "seq": seq,
                            "at": iso_from_epoch(ts),
                            "run": ro_id,
                            "repo": str(repo),
                            "kind": kind,
                            "payload": payload,
                        }
                    )
                )
        (state / "events.jsonl").write_text("\n".join(lines) + "\n", encoding="utf-8")
        return now, seq

    def hold_state_lock(self):
        state = self.state_dir()
        state.mkdir(parents=True, exist_ok=True)
        lock_file = state / ".state.lock"
        ready = self.work / f"lock-ready-{id(self)}"
        gate = self.work / f"lock-gate-{id(self)}"
        for p in (ready, gate):
            if p.exists():
                p.unlink()
        holder_code = (
            "import fcntl, pathlib, time\n"
            f"f = open({str(lock_file)!r}, 'a+')\n"
            "fcntl.flock(f.fileno(), fcntl.LOCK_EX)\n"
            f"pathlib.Path({str(ready)!r}).write_text('held')\n"
            f"while not pathlib.Path({str(gate)!r}).exists():\n"
            "    time.sleep(0.01)\n"
        )
        proc = subprocess.Popen([sys.executable, "-c", holder_code])

        def cleanup():
            gate.write_text("go")
            if proc.poll() is None:
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()

        self.addCleanup(cleanup)
        self.wait_until(lambda: ready.is_file(), "state.lock holder acquired lock")
        return proc, gate

    def test_snapshot_json_exposes_run_work_and_wave_eta_lock_free_and_zero_git(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, write_work_sec=900, cls="build")

        # Add an open work item to verify work DAG & per-work-item ETA in snapshot --json.
        add = self.cli("work", "add", "implement eta tui", "--repo", str(repo), "--class", "feat", cwd=repo)
        self.assertEqual(add.returncode, 0, add.stdout + add.stderr)

        state_doc, gate = self.start_gated(
            "--workdir",
            repo,
            "--class",
            "build",
            "--name",
            "snap-live",
            "build feature",
            cwd=repo,
        )
        run_id = state_doc["run"]

        # Remove derived eta-model.json to prove snapshot --json stays strictly read-only
        # (computes in memory without writing eta-model.json or touching .state.lock or git).
        model_path = self.state_dir() / "eta-model.json"
        if model_path.exists():
            model_path.unlink()

        nogit = make_nogit_dir(self.work)
        holder, lock_gate = self.hold_state_lock()
        probe_env = dict(self.env)
        probe_env["PATH"] = f"{nogit}:{self.env['PATH']}"
        probe_env["DELEGATE_MEASURE_STATE_LOCK"] = "1"

        res = subprocess.run(
            [str(fixtures.DELEGATE), "snapshot", "--json", "--repo", str(repo)],
            cwd=repo,
            env=probe_env,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        self.assertNotIn("STATE_LOCK_ATTEMPT", res.stderr, "snapshot --json must never touch .state.lock")
        self.assertIsNone(holder.poll(), "state.lock holder must remain alive during lock-free snapshot")
        self.assertFalse(model_path.exists(), "snapshot --json must perform zero disk writes")
        lock_gate.write_text("go")

        doc = json.loads(res.stdout)
        self.assertIn("eta", doc)
        wave = doc["eta"]["wave"]
        self.assertEqual(wave["activeCount"], 1)
        self.assertEqual(wave["runningCount"], 1)
        self.assertTrue(wave.get("earliestP50Clock"))
        self.assertTrue(wave.get("waveP50Clock"))
        self.assertTrue(wave.get("waveP75Clock"))

        # Per-run ETA attached with <= 6-col compactBadge
        runs = doc["repos"][0]["runs"]
        live_entry = next(r for r in runs if r["run"] == run_id)
        self.assertIn("eta", live_entry)
        self.assertEqual(live_entry["eta"]["stage"], "agent_work")
        self.assertFalse(live_entry["eta"]["stoppedClock"])
        self.assertTrue(live_entry["eta"]["compactBadge"])
        self.assertLessEqual(len(live_entry["eta"]["compactBadge"]), 6)

        # Per-work-item ETA & work DAG summary exposed
        self.assertGreaterEqual(doc["eta"]["work"]["writeChains"], 1)
        self.assertEqual(len(doc["work"]), 1)
        self.assertIn("eta", doc["work"][0])
        self.assertTrue(doc["work"][0]["eta"]["compactBadge"])
        self.assertTrue(doc["work"][0]["eta"]["focusBadge"])

        gate.write_text("go")

    def test_stop_hook_waiter_appends_fastest_eta_without_new_block_condition_or_locks(self):
        repo = self.make_repo()
        now, max_seq = self.seed_events(repo, count=12, write_work_sec=1800, ro_sec=360, cls="build")
        # Persist eta-model.json once so hook check uses the cached model read-only.
        sp = self.cli("speed", cwd=repo)
        self.assertEqual(sp.returncode, 0, sp.stdout + sp.stderr)

        caller = "eta-hook-sess-1"
        self.env["DELEGATE_CALLER"] = caller
        # Advance this caller's event cursor past historical seeded events so Tier 4 waiter fires cleanly.
        (self.state_dir() / "cursors.json").write_text(
            json.dumps(
                {
                    "default": {"seq": max_seq, "readRuns": {}, "updatedAt": iso_from_epoch(now)},
                    "callers": {
                        caller: {"seq": max_seq, "readRuns": {}, "updatedAt": iso_from_epoch(now)}
                    },
                }
            ),
            encoding="utf-8",
        )

        # Start one longer write run and one faster read-only run without any waiter.
        w_doc, w_gate = self.start_gated(
            "--workdir",
            repo,
            "--class",
            "build",
            "--name",
            "slow-write",
            "long task",
            cwd=repo,
        )
        ro_doc, ro_gate = self.start_gated(
            "--workdir",
            repo,
            "--read-only",
            "--tier",
            "scout",
            "--name",
            "fast-scout",
            "quick check",
            cwd=repo,
        )

        nogit = make_nogit_dir(self.work)
        holder, lock_gate = self.hold_state_lock()
        hook_env = dict(self.env)
        hook_env["PATH"] = f"{nogit}:{self.env['PATH']}"
        hook_env["DELEGATE_MEASURE_STATE_LOCK"] = "1"
        hook_env["DELEGATE_CALLER"] = caller
        hook_env["DELEGATE_BIN"] = str(fixtures.DELEGATE)
        hook_env["DELEGATE_STOP_HOOK_WAITERS"] = ""

        # 1) Claude Stop hook wrapper appends fastest ETA clock (from the faster scout run: 答复).
        payload_claude = json.dumps({"cwd": str(repo), "session_id": caller, "stop_hook_active": False})
        res_py = subprocess.run(
            [sys.executable, str(STOP_HOOK)],
            input=payload_claude,
            cwd=repo,
            env=hook_env,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(res_py.returncode, 0, res_py.stdout + res_py.stderr)
        self.assertNotIn("STATE_LOCK_ATTEMPT", res_py.stderr)
        self.assertIsNone(holder.poll())
        out_c = json.loads(res_py.stdout)
        self.assertEqual(out_c["decision"], "block")
        self.assertIn("最快预计", out_c["reason"])
        self.assertIn("答复", out_c["reason"])

        # 2) JSON format exposes structured `fastestEta`.
        res_json = subprocess.run(
            [str(fixtures.DELEGATE), "hook", "check", "--format", "json"],
            input=payload_claude,
            cwd=repo,
            env=hook_env,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(res_json.returncode, 0, res_json.stdout + res_json.stderr)
        jdoc = json.loads(res_json.stdout)
        self.assertTrue(jdoc["item"].get("fastestEta"), jdoc)

        # 3) Agy format also carries fastest ETA with agy waiter wording.
        payload_agy = json.dumps(
            {
                "cwd": str(repo),
                "conversationId": caller,
                "continuationActive": False,
                "terminationReason": "NO_TOOL_CALL",
            }
        )
        res_agy = subprocess.run(
            [str(fixtures.DELEGATE), "hook", "check", "--format", "agy"],
            input=payload_agy,
            cwd=repo,
            env=hook_env,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(res_agy.returncode, 0, res_agy.stdout + res_agy.stderr)
        out_a = json.loads(res_agy.stdout)
        self.assertEqual(out_a["decision"], "continue")
        self.assertIn("最快预计", out_a["reason"])
        self.assertIn("WaitMsBeforeAsync=500", out_a["reason"])

        # 4) Attaching a waiter allows immediately — ETA adds ZERO new blocking conditions.
        covered_env = dict(hook_env)
        covered_env["DELEGATE_STOP_HOOK_WAITERS"] = f"{fixtures.DELEGATE} wait --any"
        res_ok = subprocess.run(
            [sys.executable, str(STOP_HOOK)],
            input=payload_claude,
            cwd=repo,
            env=covered_env,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(res_ok.returncode, 0, res_ok.stdout + res_ok.stderr)
        self.assertEqual(res_ok.stdout.strip(), "", f"covered runs must not be blocked: {res_ok.stdout}")

        # 5) If all uncovered live runs are stopped-clock (stallWarning), omit countdown!
        for rdir in (Path(w_doc["dir"]), Path(ro_doc["dir"])):
            (rdir / "stall-warning.json").write_text(
                json.dumps({"kind": "agent", "elapsedSeconds": 900, "detail": "stalled"}),
                encoding="utf-8",
            )
        res_stalled = subprocess.run(
            [str(fixtures.DELEGATE), "hook", "check", "--format", "claude"],
            input=payload_claude,
            cwd=repo,
            env=hook_env,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(res_stalled.returncode, 0, res_stalled.stdout + res_stalled.stderr)
        out_s = json.loads(res_stalled.stdout)
        self.assertEqual(out_s["decision"], "block")
        self.assertNotIn("最快预计", out_s["reason"], "stopped-clock runs must not invent countdown")

        lock_gate.write_text("go")
        w_gate.write_text("go")
        ro_gate.write_text("go")

    def test_online_calibration_end_to_end_started_accept_delivered_applied_and_speed_coverage(self):
        repo = self.make_repo()
        self.seed_events(repo, count=12, write_work_sec=600, cls="build")
        sp0 = self.cli("speed", cwd=repo)
        self.assertEqual(sp0.returncode, 0, sp0.stdout + sp0.stderr)

        state_doc, gate = self.start_gated(
            "--worktree",
            "--workdir",
            repo,
            "--class",
            "build",
            "--accept",
            "true",
            "--name",
            "cal-e2e",
            "add calibration test file",
            cwd=repo,
            pre="printf 'ok\\n' > cal_added.txt",
        )
        run_id = state_doc["run"]
        run_dir = Path(state_doc["dir"])
        pred_path = run_dir / "eta-pred.json"

        # Started milestone prediction written immediately on start.
        pred_start = self.wait_json(pred_path, lambda d: isinstance(d.get("started"), dict), "eta-pred.json started")
        self.assertGreater(pred_start["started"]["toDeliveredP50Sec"], 0)
        self.assertGreater(pred_start["started"]["toAppliedP50Sec"], 0)

        # Release gate -> run enters accept, delivers, and finishes recording delivered calibration!
        gate.write_text("go")
        self.wait_json(run_dir / "summary.json", lambda d: d.get("state") == "delivered", "run delivered")
        pred_done = self.wait_json(
            pred_path,
            lambda d: bool(d.get("recordedDelivered")),
            "eta-pred.json recordedDelivered",
        )
        self.assertIn("acceptReview", pred_done)
        self.assertGreater(pred_done["acceptReview"]["toDeliveredP50Sec"], 0)

        # Apply the run to trigger `applied` milestone calibration record.
        ap = self.cli("apply", run_id, cwd=repo)
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)

        cal_path = self.state_dir() / "eta-calibration.jsonl"
        self.assertTrue(cal_path.is_file(), "eta-calibration.jsonl must be written")
        rows = [json.loads(line) for line in cal_path.read_text(encoding="utf-8").splitlines() if line.strip()]
        pairs = {(r["checkpoint"], r["milestone"]) for r in rows if r["run"] == run_id}
        self.assertEqual(
            pairs,
            {("started", "delivered"), ("accept_review", "delivered"), ("started", "applied")},
            f"missing online calibration milestones: {rows}",
        )
        for r in rows:
            self.assertIn("actualSec", r)
            self.assertIn("withinP75", r)

        # `delegate speed` displays `ETA p75覆盖` column + online calibration summary & JSON.
        sp_h = self.cli("speed", "--since", "14d", "--repo", str(repo), cwd=repo)
        self.assertEqual(sp_h.returncode, 0, sp_h.stdout + sp_h.stderr)
        self.assertIn("ETA p75覆盖", sp_h.stdout)
        self.assertIn("ETA 在线校准", sp_h.stdout)

        sp_j = self.cli("speed", "--since", "14d", "--repo", str(repo), "--json", cwd=repo)
        self.assertEqual(sp_j.returncode, 0, sp_j.stdout + sp_j.stderr)
        jdoc = json.loads(sp_j.stdout)
        self.assertIn("etaCalibration", jdoc)
        self.assertGreaterEqual(jdoc["etaCalibration"]["totalRecords"], 3)
        self.assertEqual(jdoc["etaCalibration"]["overall"]["startedDelivered"]["count"], 1)
        self.assertEqual(jdoc["etaCalibration"]["overall"]["acceptDelivered"]["count"], 1)
        self.assertEqual(jdoc["etaCalibration"]["overall"]["startedApplied"]["count"], 1)
        self.assertIn("etaCalibration", jdoc["repos"][0])

    def test_online_calibration_optimistic_alert_and_deletable_derived_file(self):
        repo = self.make_repo()
        now, _ = self.seed_events(repo, count=12, write_work_sec=600, cls="build")
        cal_path = self.state_dir() / "eta-calibration.jsonl"
        # 2/10 within p75 -> 20% coverage (<60% -> optimistic alert!)
        lines = []
        for i in range(10):
            actual = 500.0 if i < 2 else 1400.0
            lines.append(
                json.dumps(
                    {
                        "at": iso_from_epoch(now - 300 + i * 10),
                        "run": f"opt-{i}",
                        "repo": str(repo),
                        "repoName": repo.name,
                        "mode": "write",
                        "class": "build",
                        "checkpoint": "started",
                        "milestone": "delivered",
                        "predP50Sec": 600.0,
                        "predP75Sec": 800.0,
                        "actualSec": actual,
                        "withinP75": actual <= 800.0,
                        "fallbackLevel": "L1",
                        "isPrior": False,
                    }
                )
            )
        cal_path.write_text("\n".join(lines) + "\n", encoding="utf-8")

        sp = self.cli("speed", "--since", "14d", "--repo", str(repo), cwd=repo)
        self.assertEqual(sp.returncode, 0, sp.stdout + sp.stderr)
        self.assertIn("20%⚠(10)", sp.stdout)
        self.assertIn("偏乐观", sp.stdout)

        # Deleting derived files (eta-calibration.jsonl & eta-model.json) degrades cleanly.
        cal_path.unlink()
        model_path = self.state_dir() / "eta-model.json"
        if model_path.exists():
            model_path.unlink()

        sp2 = self.cli("speed", "--since", "14d", "--repo", str(repo), "--json", cwd=repo)
        self.assertEqual(sp2.returncode, 0, sp2.stdout + sp2.stderr)
        doc2 = json.loads(sp2.stdout)
        self.assertEqual(doc2["etaCalibration"]["totalRecords"], 0)
        self.assertTrue(model_path.is_file(), "eta-model.json rebuilt after deletion")


if __name__ == "__main__":
    unittest.main()
