import json
import os
import shlex
import subprocess
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateLaneStallTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    start_gated = fixtures.DelegateTests.start_gated
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    until = fixtures.DelegateTests.until
    wait_text = fixtures.DelegateTests.wait_text
    repo = fixtures.DelegateTests.repo

    def _seed_lane_history(self, repo_path: Path, label: str, samples: list[float]) -> None:
        state_dir = Path(self.env["XDG_STATE_HOME"]) / "delegate"
        state_dir.mkdir(parents=True, exist_ok=True)
        events_path = state_dir / "events.jsonl"
        repo_str = str(repo_path.resolve())
        with events_path.open("a", encoding="utf-8") as f:
            for idx, held in enumerate(samples, 1):
                rec = {
                    "seq": idx,
                    "at": f"2026-11-28T00:00:0{idx}Z",
                    "run": f"hist-{idx}",
                    "repo": repo_str,
                    "kind": "lane_released",
                    "payload": {
                        "label": label,
                        "heldSeconds": held,
                        "remote": False,
                        "lane": "local",
                    },
                }
                f.write(json.dumps(rec) + "\n")

    def test_lane_blocked_on_pipe_read_warns_writer_status_and_wait_machine(self):
        repo = self.repo({"seed.txt": "seed\n"})
        # Seed 3 historical lane_released events (10, 20, 30 -> p75 = 25s).
        # With multiplier 0.015 => threshold = 0.375s; window = 0.25s.
        self._seed_lane_history(repo, f"{repo.name}: python3 blocked.py", [10.0, 20.0, 30.0])
        self.env["DELEGATE_STALL_IDLE"] = "0"
        self.env["DELEGATE_STALL_COMMAND"] = "0"
        self.env["DELEGATE_LANE_STALL_MULTIPLIER"] = "0.015"
        self.env["DELEGATE_LANE_STALL_WINDOW"] = "0.35"
        self.env["DELEGATE_POLL"] = "0.05"

        fifo = self.work / "block.fifo"
        os.mkfifo(fifo)
        # Keep write end open in test process so child blocks in pipe_read with 0 CPU.
        fifo_w = os.open(fifo, os.O_RDWR)
        self.addCleanup(lambda: os.close(fifo_w))

        lane_stderr = self.work / "lane.stderr"
        lane_done = self.work / "lane.done"
        blocked_script = repo / "blocked.py"
        blocked_script.write_text(
            "import sys\n"
            "with open(sys.argv[1], 'rb', buffering=0) as f:\n"
            "    f.read(1)\n",
            encoding="utf-8",
        )
        lane_cmd = (
            f"({shlex.quote(str(fixtures.DELEGATE))} lane "
            f"{shlex.quote(f'python3 {blocked_script} {fifo}')} "
            f"2>{shlex.quote(str(lane_stderr))}; "
            f"echo $? > {shlex.quote(str(lane_done))}) &"
        )
        bash_ev = {
            "type": "tool_execution_start",
            "toolName": "bash",
            "args": {"command": f"delegate lane 'python3 {blocked_script}'"},
        }
        pre_script = (
            f"printf '%s\\n' {shlex.quote(json.dumps(bash_ev))}; "
            f"{lane_cmd}"
        )
        state, gate = self.start_gated(
            "--workdir", repo, "--name", "stuck-lane", "task",
            attempts=([fixtures.answer("done after unblock"), fixtures.SETTLED],),
            pre=pre_script,
        )
        run_dir = Path(state["dir"])
        run_id = state["run"]

        # Start `delegate wait --machine` while the lane command is blocked;
        # it must wake immediately on the lane stall warning instead of timing out at --max 20.
        t0 = time.monotonic()
        wait_res = self.cli("wait", "--machine", "--max", "20", human=True)
        elapsed = time.monotonic() - t0
        self.assertEqual(wait_res.returncode, 0, wait_res.stdout + wait_res.stderr)
        self.assertLess(elapsed, 10.0, f"wait --machine took too long: {elapsed:.2f}s")
        self.assertIn("疑似卡住", wait_res.stdout)
        self.assertIn("pid=", wait_res.stdout)
        self.assertIn("wchan=", wait_res.stdout)
        self.assertIn("reply", wait_res.stdout)

        # Calling `wait --machine` again must NOT re-emit the same warning (deduplicated).
        wait_again = self.cli("wait", "--machine", "--max", "0.25", human=True)
        self.assertEqual(wait_again.returncode, 75, wait_again.stdout + wait_again.stderr)
        self.assertNotIn("⚠ 疑似卡住", wait_again.stdout)

        # Writer-side stderr received the Chinese hint.
        err_text = lane_stderr.read_text(encoding="utf-8")
        self.assertIn("疑似卡住", err_text)
        self.assertIn("pid=", err_text)
        self.assertIn("wchan=", err_text)

        # Status (JSON and human) shows 疑似卡住 + PID + comm + wchan.
        st_json = self.outcome(self.cli("status", run_id))
        sw = st_json.get("stallWarning")
        self.assertIsInstance(sw, dict, st_json)
        self.assertEqual(sw["kind"], "lane")
        self.assertIsInstance(sw["pid"], int)
        self.assertGreater(sw["pid"], 0)
        self.assertTrue(sw.get("comm"))
        self.assertTrue(sw.get("wchan"))
        self.assertIn(f"pid={sw['pid']}", wait_res.stdout)

        st_human = self.cli("status", run_id, human=True)
        self.assertIn("疑似卡住", st_human.stdout)
        self.assertIn(f"pid={sw['pid']}", st_human.stdout)
        self.assertIn(f"wchan={sw['wchan']}", st_human.stdout)

        # The blocked process must NOT be killed; unblocking the FIFO lets it finish cleanly.
        self.assertFalse(lane_done.exists(), "lane command must not be auto-killed")
        os.write(fifo_w, b"x")
        self.until(lambda: lane_done.exists(), "lane command exits after FIFO unblock")
        self.assertEqual(lane_done.read_text().strip(), "0")
        # stall-warning.json must be cleared automatically once lane exits.
        self.until(
            lambda: not (run_dir / "stall-warning.json").exists(),
            "lane stall-warning.json cleared after command exit",
        )

        gate.touch()
        settled = self.outcome(self.cli("wait", run_id))
        self.assertEqual(settled["state"], "answered")

    def test_lane_active_cpu_and_cmd_out_output_do_not_false_positive(self):
        repo = self.repo({"seed.txt": "seed\n"})
        self._seed_lane_history(repo, f"{repo.name}: python3 busy.py", [10.0, 20.0, 30.0])
        self._seed_lane_history(repo, f"{repo.name}: _cmd-out", [10.0, 20.0, 30.0])
        self.env["DELEGATE_STALL_IDLE"] = "0"
        self.env["DELEGATE_STALL_COMMAND"] = "0"
        self.env["DELEGATE_LANE_STALL_MULTIPLIER"] = "0.015"
        self.env["DELEGATE_LANE_STALL_WINDOW"] = "0.35"

        run_dir = self.work / "fake-run"
        run_dir.mkdir(parents=True, exist_ok=True)
        (run_dir / "meta.json").write_text(
            json.dumps({"run": "fake-run", "name": "busy", "workdir": str(repo)}),
            encoding="utf-8",
        )
        env = dict(self.env)
        env["DELEGATE_RUN_DIR"] = str(run_dir)

        # 1. Active CPU burn for ~1.1s (well past 0.375s threshold + 0.25s window): no false positive.
        busy_script = repo / "busy.py"
        busy_script.write_text(
            "import time\n"
            "end = time.monotonic() + 1.1\n"
            "x = 0\n"
            "while time.monotonic() < end:\n"
            "    x = (x + 1) & 0xffff\n",
            encoding="utf-8",
        )
        r_cpu = subprocess.run(
            [str(fixtures.DELEGATE), "lane", f"python3 {busy_script}"],
            cwd=repo,
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(r_cpu.returncode, 0, r_cpu.stdout + r_cpu.stderr)
        self.assertNotIn("疑似卡住", r_cpu.stderr)
        self.assertFalse((run_dir / "stall-warning.json").exists())

        # 2. Low-CPU command wrapped by `_cmd-out` that continuously writes output:
        # `.active` bytes keep growing so it must NOT trigger a false positive.
        stream_script = repo / "stream.py"
        stream_script.write_text(
            "import sys, time\n"
            "for i in range(12):\n"
            "    print(f'Compiling crate-{i} v0.1.0', flush=True)\n"
            "    time.sleep(0.1)\n",
            encoding="utf-8",
        )
        cmd_out_cmd = (
            f"{shlex.quote(str(fixtures.DELEGATE))} _cmd-out "
            f"{shlex.quote(str(run_dir))} python3 {shlex.quote(str(stream_script))}"
        )
        r_out = subprocess.run(
            [str(fixtures.DELEGATE), "lane", cmd_out_cmd],
            cwd=repo,
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(r_out.returncode, 0, r_out.stdout + r_out.stderr)
        self.assertNotIn("疑似卡住", r_out.stderr)
        self.assertFalse((run_dir / "stall-warning.json").exists())


if __name__ == "__main__":
    unittest.main()
