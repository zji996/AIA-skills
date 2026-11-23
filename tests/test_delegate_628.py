"""End-to-end tests for delegate 6.28.0:
1. Machine-wide append-only event stream (`events.jsonl`), monotonic seq, rebuild, `events --since/--follow/--json`
2. Cursor-based `delegate wait --since SEQ` returning only events > SEQ + new cursor
3. Claude Code Stop hook race lock (`有新事件，先看 delegate events --since N`) driven by `wait`/`result` cursors
4. Liveness/activity classification from transcript/events/stderr tail (`status` + `--json`)
5. Stall warn-only (never auto-kills; only explicit `stop` kills) & mechanical repetition self-check (ADR 0120)
6. Deterministic DAG continuation queue (`delegate queue add|list|remove`): auto-dispatch on PASS, pause on fail/question/NEEDS_CONTROLLER
"""

import json
import shlex
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures

ROOT = Path(__file__).resolve().parent.parent
STOP_HOOK = ROOT / "skills/delegate/hooks/claude-code-stop.py"


class Delegate628Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    until = fixtures.DelegateTests.until
    repo = fixtures.DelegateTests.repo

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def read_events(self) -> list[dict]:
        p = self.state_dir() / "events.jsonl"
        if not p.is_file():
            return []
        return [json.loads(line) for line in p.read_text().splitlines() if line.strip()]

    def test_event_stream_lifecycle_seq_monotonic_rebuild_and_write_failure_resilience(self):
        repo = self.repo({"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo v1 > a.txt")
        run_state = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "ev-demo", "--accept", "true", "task")
        )
        self.assertEqual(run_state["state"], "delivered")

        # Apply & clean to cover applied and cleaned events too.
        self.assertEqual(self.cli("apply", "--json", run_state["run"], cwd=repo).returncode, 0)
        self.assertEqual(self.cli("clean", "--no-archive", run_state["run"], cwd=repo).returncode, 0)

        # Question run + stop run
        self.fake_pi([fixtures.answer("需要主控决定\n选 A 还是 B？"), fixtures.SETTLED])
        q_state = self.outcome(self.cli("run", "--read-only", "--name", "ev-q", "ask"))
        self.assertTrue(q_state.get("question"))

        gate = self.work / "stop-gate"
        self.fake_pi(
            [fixtures.answer("never"), fixtures.SETTLED],
            pre=f"while [ ! -f {shlex.quote(str(gate))} ]; do sleep 0.05; done",
        )
        s_state = self.outcome(self.cli("start", "--read-only", "--name", "ev-stop", "hang"))
        self.assertEqual(self.cli("stop", s_state["run"]).returncode, 0)

        evs = self.read_events()
        seqs = [e["seq"] for e in evs]
        self.assertEqual(seqs, list(range(1, len(seqs) + 1)), f"non-monotonic seqs: {seqs}")
        kinds = {e["kind"] for e in evs}
        for required in ("started", "lane_acquired", "lane_released", "accept", "delivered", "applied", "cleaned", "question", "stopped"):
            self.assertIn(required, kinds, f"missing kind {required} in {kinds}")

        # `delegate events --since N --json` filters strictly > N
        mid = len(seqs) // 2
        res = self.cli("events", "--since", str(mid), "--json")
        self.assertEqual(res.returncode, 0, res.stderr)
        tail = [json.loads(ln) for ln in res.stdout.splitlines() if ln.strip()]
        self.assertTrue(all(e["seq"] > mid for e in tail))
        self.assertEqual(len(tail), len(seqs) - mid)

        # `delegate events --rebuild` reconstructs events from remaining run metas even after deleting events.jsonl
        (self.state_dir() / "events.jsonl").unlink()
        (self.state_dir() / "events.seq").unlink(missing_ok=True)
        rebuilt = self.cli("events", "--rebuild", "--json")
        self.assertEqual(rebuilt.returncode, 0, rebuilt.stderr)
        info = json.loads(rebuilt.stdout.strip())
        self.assertTrue(info["rebuilt"])
        self.assertGreaterEqual(info["events"], 2)
        rebuilt_evs = self.read_events()
        self.assertEqual([e["seq"] for e in rebuilt_evs], list(range(1, len(rebuilt_evs) + 1)))

        # Write failure resilience: replace events.jsonl with a directory -> run still succeeds!
        ev_file = self.state_dir() / "events.jsonl"
        ev_file.unlink()
        ev_file.mkdir()
        self.fake_pi([fixtures.answer("resilient ok"), fixtures.SETTLED])
        ok = self.outcome(self.cli("run", "--read-only", "--name", "resilient", "still works"))
        self.assertEqual(ok["state"], "answered")

    def test_wait_since_cursor_and_stop_hook_race_lock(self):
        repo = self.repo({"f.txt": "init\n"})
        no_runs_env = {k: v for k, v in self.env.items() if k != "DELEGATE_RUNS"}

        def run_in(*args, human=False):
            cmd = [str(fixtures.DELEGATE), *args]
            if not human and args[0] in {"start", "run", "reply", "wait", "status", "stop", "clean", "events"} and "--json" not in args:
                cmd.insert(2, "--json")
            return subprocess.run(cmd, cwd=repo, capture_output=True, text=True, env=no_runs_env, timeout=30)

        def hook(active=False):
            env = {
                **no_runs_env,
                "DELEGATE_BIN": str(fixtures.DELEGATE),
                "DELEGATE_STOP_HOOK_WAITERS": "",
            }
            payload = {"cwd": str(repo), "stop_hook_active": active}
            res = subprocess.run(
                [sys.executable, str(STOP_HOOK)],
                input=json.dumps(payload),
                capture_output=True,
                text=True,
                env=env,
                timeout=15,
            )
            self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
            out = res.stdout.strip()
            return json.loads(out) if out else None

        # Start a read-only run in the background (no wait yet!) and let it finish.
        self.fake_pi([fixtures.answer("first report"), fixtures.SETTLED])
        r1 = self.outcome(run_in("start", "--read-only", "--name", "bg-one", "task 1"))
        self.until(lambda: (Path(r1["dir"]) / "exit_code").exists(), "r1 finished")

        # Stop hook MUST block because delivered event seq > cursor (0)!
        blocked = hook()
        self.assertIsNotNone(blocked)
        self.assertEqual(blocked["decision"], "block")
        self.assertIn("有新事件，先看 delegate events --since 0", blocked["reason"])

        # Reading with `result --no-mark` (used by TUI) MUST NOT advance the cursor!
        self.assertEqual(run_in("result", "--no-mark", r1["run"], human=True).returncode, 0)
        still_blocked = hook()
        self.assertIsNotNone(still_blocked)
        self.assertIn("有新事件，先看 delegate events --since 0", still_blocked["reason"])

        # Reading with `result` (normal) advances cursor -> Stop hook clears!
        self.assertEqual(run_in("result", r1["run"], human=True).returncode, 0)
        self.assertIsNone(hook())

        # Start a second run, wait via `delegate wait --since <cur>` -> returns only new events + new cursor!
        cursor_before = self.read_events()[-1]["seq"]
        self.fake_pi([fixtures.answer("second report"), fixtures.SETTLED])
        r2 = self.outcome(run_in("start", "--read-only", "--name", "bg-two", "task 2"))
        self.until(lambda: (Path(r2["dir"]) / "exit_code").exists(), "r2 finished")

        # Race lock triggers again with exact previous cursor N!
        blocked2 = hook()
        self.assertIsNotNone(blocked2)
        self.assertIn(f"有新事件，先看 delegate events --since {cursor_before}", blocked2["reason"])

        w = run_in("wait", "--since", str(cursor_before), "--json")
        self.assertEqual(w.returncode, 0, w.stderr)
        lines = [json.loads(ln) for ln in w.stdout.splitlines() if ln.strip()]
        summary = lines[-1]
        returned_events = lines[:-1]
        self.assertTrue(all(e["seq"] > cursor_before for e in returned_events))
        self.assertGreater(summary["cursor"], cursor_before)
        self.assertTrue(any(e["kind"] == "delivered" and e["run"] == r2["run"] for e in returned_events))

        # After `wait --since`, cursor is updated -> Stop hook passes!
        self.assertIsNone(hook())

        # stop_hook_active=True always passes through even with unread events.
        self.fake_pi([fixtures.answer("需要主控决定\n确认吗？"), fixtures.SETTLED])
        r3 = self.outcome(run_in("start", "--read-only", "--name", "bg-q", "task 3"))
        self.until(lambda: (Path(r3["dir"]) / "exit_code").exists(), "r3 finished")
        self.assertIsNotNone(hook(active=False))
        self.assertIsNone(hook(active=True))

    def test_liveness_activity_from_transcript_command_and_stderr_tail(self):
        gate = self.work / "act-gate"
        bash_start = {"type": "tool_execution_start", "toolName": "bash", "args": {"command": "cargo test -p demo"}}
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre=f"printf '%s\\n' {shlex.quote(json.dumps(bash_start))}; while [ ! -f {shlex.quote(str(gate))} ]; do sleep 0.05; done",
        )
        run = self.outcome(self.cli("start", "--read-only", "--name", "act-cmd", "run tests"))
        self.addCleanup(lambda: (gate.touch(), self.cli("stop", run["run"])))

        def has_cmd_activity():
            st = self.outcome(self.cli("status", run["run"]))
            return st.get("activity", {}).get("kind") == "running_command"

        self.until(has_cmd_activity, "running_command activity")
        st_json = self.outcome(self.cli("status", run["run"]))
        self.assertEqual(st_json["activity"]["kind"], "running_command")
        self.assertIn("cargo test -p demo", st_json["activity"]["cmd"])
        self.assertIn("elapsedSeconds", st_json["activity"])

        st_human = self.cli("status", run["run"], human=True)
        self.assertIn("在跑命令：cargo test -p demo", st_human.stdout)
        gate.touch()
        self.until(lambda: (Path(run["dir"]) / "exit_code").exists(), "act-cmd done")

        # Quota detection from stderr tail on a live run
        gate2 = self.work / "quota-gate"
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre=f"echo 'HTTP 429 Too Many Requests: quota exceeded' >&2; while [ ! -f {shlex.quote(str(gate2))} ]; do sleep 0.05; done",
        )
        run2 = self.outcome(self.cli("start", "--read-only", "--name", "act-quota", "quota test"))
        self.addCleanup(lambda: (gate2.touch(), self.cli("stop", run2["run"])))

        def has_quota_activity():
            st = self.outcome(self.cli("status", run2["run"]))
            return st.get("activity", {}).get("kind") == "quota"

        self.until(has_quota_activity, "quota activity")
        st2_human = self.cli("status", run2["run"], human=True)
        self.assertIn("配额受限", st2_human.stdout)
        gate2.touch()

    def test_mechanical_repetition_triggers_colleague_invisible_warning_without_killing(self):
        self.env["DELEGATE_LOOP_THRESHOLD"] = "3"
        self.env["DELEGATE_STALL_IDLE"] = "0"
        self.env["DELEGATE_STALL_COMMAND"] = "0"

        events_script = []
        for _ in range(3):
            events_script.append({"type": "tool_execution_start", "toolName": "bash", "args": {"command": "echo stuck"}})
            events_script.append({
                "type": "tool_execution_end",
                "toolName": "bash",
                "isError": True,
                "result": {"content": [{"type": "text", "text": "same failure"}]},
            })
        events_script.append(fixtures.answer("recovered eventually"))
        events_script.append(fixtures.SETTLED)
        self.fake_pi(events_script)

        state = self.outcome(self.cli("run", "--read-only", "--name", "loop-test", "repeat"))
        # Never killed — colleague completes normally!
        self.assertEqual(state["state"], "answered")

        machine_evs = self.read_events()
        loop_warns = [
            e for e in machine_evs
            if e["kind"] == "stall_warning" and e["payload"].get("stallKind") == "loop"
        ]
        self.assertEqual(len(loop_warns), 1, machine_evs)
        self.assertEqual(loop_warns[0]["payload"]["count"], 3)
        self.assertIn("echo stuck", loop_warns[0]["payload"]["cmd"])
        self.assertIn("ADR 0120", loop_warns[0]["payload"].get("selfCheck", ""))

    def test_deterministic_dag_queue_auto_dispatches_on_pass_and_pauses_on_question_or_failure(self):
        repo = self.repo({"code.txt": "v0\n"})
        prompt_b = self.work / "step-b.md"
        prompt_b.write_text("step B prompt\n")
        prompt_c = self.work / "step-c.md"
        prompt_c.write_text("step C prompt\n")

        gate_a = self.work / "gate-a"
        self.fake_pi(
            [fixtures.answer("step done"), fixtures.SETTLED],
            pre=f'case "$*" in *step-a*) while [ ! -f {shlex.quote(str(gate_a))} ]; do sleep 0.05; done;; esac; echo x >> code.txt',
        )
        run_a = self.outcome(
            self.cli("start", "--workdir", repo, "--worktree", "--name", "step-a", "--accept", "true", "task A")
        )
        self.addCleanup(lambda: gate_a.touch())

        # Build DAG: run_a -> step-b -> step-c
        add_b = self.cli(
            "queue", "add", "step-b",
            "--after", run_a["run"],
            "--prompt-file", str(prompt_b),
            "--worktree", "--accept", "true",
            "--json",
            cwd=repo,
        )
        self.assertEqual(add_b.returncode, 0, add_b.stderr)
        item_b = json.loads(add_b.stdout.strip())
        self.assertEqual(item_b["status"], "pending")

        add_c = self.cli(
            "queue", "add", "step-c",
            "--after", "step-b",
            "--prompt-file", str(prompt_c),
            "--read-only",
            "--json",
            cwd=repo,
        )
        self.assertEqual(add_c.returncode, 0, add_c.stderr)

        # Cycle detection: adding step-b again or cyclic dep fails with exit 2.
        dup = self.cli("queue", "add", "step-b", "--after", "step-c", "--prompt-file", str(prompt_b), cwd=repo)
        self.assertEqual(dup.returncode, 2)

        # `delegate queue list` shows both pending items.
        listed = self.cli("queue", "list", cwd=repo, human=True)
        self.assertEqual(listed.returncode, 0, listed.stderr)
        self.assertIn("step-b", listed.stdout)
        self.assertIn("step-c", listed.stdout)

        # Release step-a -> step-b auto-dispatches -> step-c auto-dispatches (zero LLM orchestration)!
        gate_a.touch()

        def both_dispatched():
            out = self.cli("queue", "list", "--json", cwd=repo)
            items = {json.loads(ln)["name"]: json.loads(ln) for ln in out.stdout.splitlines() if ln.strip()}
            return items.get("step-b", {}).get("status") == "dispatched" and items.get("step-c", {}).get("status") == "dispatched"

        self.until(both_dispatched, "DAG step-b and step-c auto-dispatched", timeout=20)
        evs = self.read_events()
        dispatched_names = [e["payload"].get("queueName") for e in evs if e["kind"] == "queue_dispatched"]
        self.assertIn("step-b", dispatched_names)
        self.assertIn("step-c", dispatched_names)

        # Now test pause on question: upstream asks a question -> downstream pauses + emits queue_paused!
        self.fake_pi([fixtures.answer("需要主控决定\n是否继续？"), fixtures.SETTLED])
        q_up = self.outcome(self.cli("run", "--workdir", repo, "--read-only", "--name", "ask-up", "ask"))
        self.assertTrue(q_up.get("question"))

        prompt_d = self.work / "step-d.md"
        prompt_d.write_text("step D prompt\n")
        add_d = self.cli(
            "queue", "add", "step-d",
            "--after", q_up["run"],
            "--prompt-file", str(prompt_d),
            "--read-only",
            "--json",
            cwd=repo,
        )
        self.assertEqual(add_d.returncode, 0, add_d.stderr)
        item_d = json.loads(add_d.stdout.strip())
        self.assertEqual(item_d["status"], "paused")
        self.assertIn("提问", item_d.get("pauseReason", ""))

        evs = self.read_events()
        paused_evs = [e for e in evs if e["kind"] == "queue_paused" and e["payload"].get("queueName") == "step-d"]
        self.assertEqual(len(paused_evs), 1, evs)

        # Test pause on 6.29 review.json NEEDS_CONTROLLER and dispatch on PASS
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo y >> code.txt")
        rev_up = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "rev-up", "--accept", "true", "task rev")
        )
        self.assertEqual(rev_up["state"], "delivered")
        (Path(rev_up["dir"]) / "review.json").write_text(json.dumps({"verdict": "NEEDS_CONTROLLER", "reason": "架构变更需确认"}))
        prompt_e = self.work / "step-e.md"
        prompt_e.write_text("step E prompt\n")
        add_e = self.cli(
            "queue", "add", "step-e",
            "--after", rev_up["run"],
            "--prompt-file", str(prompt_e),
            "--read-only",
            "--json",
            cwd=repo,
        )
        self.assertEqual(add_e.returncode, 0, add_e.stderr)
        item_e = json.loads(add_e.stdout.strip())
        self.assertEqual(item_e["status"], "paused")
        self.assertIn("需要主控", item_e.get("pauseReason", ""))

        # Switching review.json to PASS and queuing another step dispatches automatically
        (Path(rev_up["dir"]) / "review.json").write_text(json.dumps({"verdict": "PASS"}))
        prompt_f = self.work / "step-f.md"
        prompt_f.write_text("step F prompt\n")
        add_f = self.cli(
            "queue", "add", "step-f",
            "--after", rev_up["run"],
            "--prompt-file", str(prompt_f),
            "--read-only",
            "--json",
            cwd=repo,
        )
        self.assertEqual(add_f.returncode, 0, add_f.stderr)
        item_f = json.loads(add_f.stdout.strip())
        self.assertEqual(item_f["status"], "dispatched")
        self.assertTrue(item_f.get("runId"))
        self.cli("wait", "--no-result", item_f["runId"])

    def test_events_follow_streams_live_events(self):
        self.fake_pi([fixtures.answer("first"), fixtures.SETTLED])
        self.outcome(self.cli("run", "--read-only", "--name", "ev-base", "base"))
        base_seq = self.read_events()[-1]["seq"]

        proc = subprocess.Popen(
            [str(fixtures.DELEGATE), "events", "--since", str(base_seq), "--follow", "--json"],
            cwd=self.work,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=self.env,
        )
        try:
            time.sleep(0.15)
            self.fake_pi([fixtures.answer("streamed"), fixtures.SETTLED])
            r2 = self.outcome(self.cli("run", "--read-only", "--name", "ev-live", "live"))

            seen = []
            deadline = time.monotonic() + 10.0
            while time.monotonic() < deadline:
                line = proc.stdout.readline()
                if not line:
                    break
                ev = json.loads(line)
                seen.append(ev)
                if ev.get("kind") == "delivered" and ev.get("run") == r2["run"]:
                    break
            self.assertTrue(
                any(e["kind"] == "delivered" and e["run"] == r2["run"] and e["seq"] > base_seq for e in seen),
                seen,
            )
        finally:
            proc.terminate()
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=5)
            if proc.stdout:
                proc.stdout.close()
            if proc.stderr:
                proc.stderr.close()

    def test_accept_default_unlimited_emits_stall_warning_and_passes(self):
        # Help text must no longer claim a default 10m acceptance timeout.
        for sub in ("start", "reply"):
            h = self.cli(sub, "--help", human=True)
            self.assertEqual(h.returncode, 0, h.stderr)
            self.assertNotIn("else 10m", h.stdout)

        # Shorten silent-output stall threshold to 1s while keeping acceptance timeout unset (unlimited).
        self.env["DELEGATE_STALL_ACCEPT"] = "1"
        repo = self.repo({"lib.rs": "pub fn x() {}\n"})
        gate = self.work / "accept-gate"
        accept_cmd = (
            f"echo 'Compiling cold-crate v0.1.0'; "
            f"while [ ! -f {shlex.quote(str(gate))} ]; do sleep 0.05; done; "
            f"echo 'Finished dev profile'"
        )
        self.fake_pi([fixtures.answer("implemented"), fixtures.SETTLED], pre="echo '// ok' >> lib.rs")
        started = self.outcome(
            self.cli("start", "--worktree", "--workdir", repo, "--name", "cold-accept", "--accept", accept_cmd, "work")
        )
        self.addCleanup(lambda: (gate.touch(), self.cli("stop", started["run"])))

        meta = json.loads((Path(started["dir"]) / "meta.json").read_text())
        self.assertIsNone(meta["acceptTimeoutSeconds"])

        # Wait until silent acceptance emits stall_warning while process stays alive.
        def has_accept_stall_warning():
            st = self.outcome(self.cli("status", started["run"]))
            sw = st.get("stallWarning")
            return isinstance(sw, dict) and sw.get("kind") == "accept"

        self.until(has_accept_stall_warning, "accept stall_warning in live status", timeout=15)
        live = self.outcome(self.cli("status", started["run"]))
        self.assertEqual(live["stallWarning"]["kind"], "accept")
        self.assertGreaterEqual(live["stallWarning"]["elapsedSeconds"], 1)
        self.assertIn("Compiling cold-crate v0.1.0", live["stallWarning"].get("lastLine", ""))
        self.assertIn("卡住提醒", live.get("activity", {}).get("label", ""))

        live_human = self.cli("status", started["run"], human=True)
        self.assertIn("卡住提醒", live_human.stdout)

        # Release acceptance -> must pass (never killed by stall_warning or a 10m default).
        gate.touch()
        settled = self.outcome(self.cli("wait", started["run"]))
        self.assertEqual(settled["state"], "delivered")
        self.assertTrue(settled["accept"]["ok"])
        self.assertFalse(settled["accept"].get("timedOut", False))

        evs = self.read_events()
        accept_warns = [
            e for e in evs
            if e["kind"] == "stall_warning"
            and e["run"] == started["run"]
            and e["payload"].get("stallKind") == "accept"
        ]
        self.assertGreaterEqual(len(accept_warns), 1, evs)
        self.assertGreaterEqual(accept_warns[0]["payload"]["elapsedSeconds"], 1)
        self.assertIn("Compiling cold-crate v0.1.0", accept_warns[0]["payload"].get("lastLine", ""))

        # Explicit `.delegate.json` acceptTimeout still enforces timeout when configured.
        (repo / ".delegate.json").write_text(json.dumps({"accept": "sleep 5", "acceptTimeout": "1s"}))
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo '// timed' >> lib.rs")
        timed_state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "timed task"))
        self.assertEqual(timed_state["state"], "rejected")
        self.assertTrue(timed_state["accept"]["timedOut"])


if __name__ == "__main__":
    unittest.main()
