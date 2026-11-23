"""6.31.1 hotfix regressions:
- Global lock ordering + non-blocking read-only paths (`status`, `work`, TUI collectors):
  concurrent `start --work` and `status --json --all --files` finish cleanly without deadlock
- Lock wait > threshold diagnostic reports holder PID and cmdline without killing (ADR 0120)
- `start` moves slow work outside `.start.lock` (lock hold stays in milliseconds)
- `status` summary `待合入` includes continuation chain tails (including zero-edit tails and
  non-continuing failed children)
- Mechanical repeat detection excludes pure wait/poll commands (`sleep`, `wait`, `tail` polling)
"""
import concurrent.futures
import json
import os
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate6311Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def test_concurrent_start_work_and_status_no_deadlock(self):
        repo = self.repo({"a.txt": "base\n", "b.txt": "base\n"})
        self.fake_pi(
            [fixtures.answer("ok"), fixtures.SETTLED],
            pre="sleep 0.15",
        )
        work_ids = []
        n_starts = 6
        n_statuses = 8
        for i in range(n_starts):
            res = self.cli("work", "add", f"concurrent-task-{i}", "--repo", repo, "--json")
            self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
            work_ids.append(json.loads(res.stdout.strip())["id"])

        def run_start(wid):
            env = {**self.env, "DELEGATE_MEASURE_START_LOCK": "1"}
            return subprocess.run(
                [
                    str(fixtures.DELEGATE),
                    "start",
                    "--json",
                    "--work",
                    wid,
                    "--worktree",
                    "--workdir",
                    str(repo),
                    "--name",
                    f"run-{wid}",
                    f"do {wid}",
                ],
                env=env,
                cwd=repo,
                capture_output=True,
                text=True,
                timeout=30,
            )

        def run_status(_idx):
            return subprocess.run(
                [str(fixtures.DELEGATE), "status", "--json", "--all", "--files"],
                env=self.env,
                cwd=repo,
                capture_output=True,
                text=True,
                timeout=30,
            )

        def run_work(_idx):
            return subprocess.run(
                [str(fixtures.DELEGATE), "work", "--json"],
                env=self.env,
                cwd=repo,
                capture_output=True,
                text=True,
                timeout=30,
            )

        t0 = time.monotonic()
        with concurrent.futures.ThreadPoolExecutor(max_workers=n_starts + n_statuses + 2) as pool:
            start_futs = [pool.submit(run_start, wid) for wid in work_ids]
            status_futs = [pool.submit(run_status, i) for i in range(n_statuses)]
            work_futs = [pool.submit(run_work, i) for i in range(2)]
            start_results = [f.result(timeout=30) for f in start_futs]
            status_results = [f.result(timeout=30) for f in status_futs]
            work_results = [f.result(timeout=30) for f in work_futs]
        elapsed = time.monotonic() - t0
        self.assertLess(elapsed, 25.0, f"concurrent start+status took too long: {elapsed:.2f}s")

        run_ids = []
        for wid, res in zip(work_ids, start_results):
            self.assertEqual(res.returncode, 0, f"start {wid} failed: {res.stdout}\n{res.stderr}")
            self.assertNotIn("lock order violation", res.stderr)
            payload = next(
                json.loads(line)
                for line in res.stdout.splitlines()
                if line.strip().startswith("{")
            )
            run_ids.append(payload["run"])

        for i, res in enumerate(status_results):
            self.assertEqual(res.returncode, 0, f"status {i} failed: {res.stderr}")
            self.assertNotIn("lock order violation", res.stderr)

        for i, res in enumerate(work_results):
            self.assertEqual(res.returncode, 0, f"work {i} failed: {res.stderr}")
            self.assertNotIn("lock order violation", res.stderr)

        # Wait for all launched runs to finish cleanly and verify work table recorded every run.
        wait_res = self.cli("wait", "--no-result", *run_ids)
        self.assertEqual(wait_res.returncode, 0, wait_res.stderr)

        work_all = self.cli("work", "--all", "--json")
        self.assertEqual(work_all.returncode, 0, work_all.stderr)
        rows = {}
        for line in work_all.stdout.splitlines():
            t = line.strip()
            if not t.startswith("{"):
                continue
            obj = json.loads(t)
            if "id" in obj:
                rows[obj["id"]] = obj
        for wid, rid in zip(work_ids, run_ids):
            self.assertIn(wid, rows)
            self.assertIn(rid, rows[wid]["runs"])

    def test_lock_wait_diagnostic_reports_holder_pid_and_cmdline_without_killing(self):
        state = self.state_dir()
        state.mkdir(parents=True, exist_ok=True)
        lock_file = state / ".work.lock"
        ready_file = state / ".holder-ready"

        holder_code = (
            "import fcntl, json, os, sys, time\n"
            "p, ready = sys.argv[1], sys.argv[2]\n"
            "fd = os.open(p, os.O_CREAT | os.O_RDWR, 0o644)\n"
            "fcntl.flock(fd, fcntl.LOCK_EX)\n"
            "os.ftruncate(fd, 0)\n"
            "os.lseek(fd, 0, os.SEEK_SET)\n"
            "os.write(fd, (json.dumps({'pid': os.getpid(), 'cmd': 'sentinel-holder-cmd'}) + '\\n').encode())\n"
            "open(ready, 'w').write('ok')\n"
            "time.sleep(0.55)\n"
            "os.close(fd)\n"
        )
        holder = subprocess.Popen(
            [sys.executable, "-c", holder_code, str(lock_file), str(ready_file), "lock-holder-marker"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            self.until(lambda: ready_file.is_file(), "holder ready", timeout=5.0)
            env = {**self.env, "DELEGATE_LOCK_WARN_SECONDS": "0.15"}
            res = subprocess.run(
                [str(fixtures.DELEGATE), "work", "add", "after-lock-wait", "--json"],
                env=env,
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
            self.assertIn("lock wait >0.1", res.stderr)
            self.assertIn(f"pid={holder.pid}", res.stderr)
            self.assertIn("lock-holder-marker", res.stderr)
            self.assertIn("ADR 0120", res.stderr)
            # ADR 0120: holder must NOT be killed, it exits 0 cleanly on its own.
            holder_rc = holder.wait(timeout=5)
            self.assertEqual(holder_rc, 0)
        finally:
            if holder.poll() is None:
                holder.kill()
                holder.wait(timeout=5)
            for stream in (holder.stdout, holder.stderr):
                if stream:
                    stream.close()

    def test_status_summary_counts_continuation_chain_tail_pending_apply(self):
        repo = self.repo({"a.txt": "v0\n", "b.txt": "v0\n"})

        # Round 1: edits a.txt
        self.fake_pi([fixtures.answer("edited a"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "--name", "chain-head", "edit a")
        )
        # Round 2 (reply): edits b.txt
        self.fake_pi([fixtures.answer("edited b"), fixtures.SETTLED], pre="echo v1 > b.txt")
        r2 = self.outcome(
            self.cli("reply", "--wait", r1["run"], "also edit b")
        )
        # Round 3 (reply tail): zero new edits (e.g. verification-only reply)
        self.fake_pi([fixtures.answer("verified ok"), fixtures.SETTLED])
        r3 = self.outcome(
            self.cli("reply", "--wait", "--minor", r2["run"], "verify only")
        )

        # Even if summary.json on the zero-edit tail has pendingChanges missing/null,
        # status summary must still count the chain tail in 待合入.
        r3_sum_path = Path(r3["dir"]) / "summary.json"
        r3_sum = json.loads(r3_sum_path.read_text())
        r3_sum["pendingChanges"] = None
        r3_sum_path.write_text(json.dumps(r3_sum) + "\n")

        st_human = self.cli("status", human=True)
        self.assertEqual(st_human.returncode, 0, st_human.stderr)
        self.assertIn("待合入 1", st_human.stdout.splitlines()[0], st_human.stdout)

        st_json = self.cli("status", "--json")
        self.assertEqual(st_json.returncode, 0, st_json.stderr)
        summary = next(
            json.loads(line)["statusSummary"]
            for line in st_json.stdout.splitlines()
            if '"statusSummary"' in line
        )
        self.assertEqual(summary["pendingApply"], 1)

        # A malformed/non-continuing child reply must NOT suppress the delivered chain tail.
        self.fake_pi([fixtures.SETTLED])  # empty answer -> malformed, no saved session
        bad_child = self.cli("reply", "--wait", "--minor", "--retries", "0", r3["run"], "bad reply")
        self.assertNotEqual(bad_child.returncode, 0)
        st_after_bad = self.cli("status", human=True)
        self.assertEqual(st_after_bad.returncode, 0, st_after_bad.stderr)
        self.assertIn("待合入 1", st_after_bad.stdout.splitlines()[0], st_after_bad.stdout)

        # Applying the tail clears 待合入 back to 0.
        app = self.cli("apply", r3["run"])
        self.assertEqual(app.returncode, 0, app.stdout + app.stderr)
        st_done = self.cli("status", human=True)
        self.assertIn("待合入 0", st_done.stdout.splitlines()[0], st_done.stdout)

    def test_mechanical_repeat_excludes_sleep_wait_tail_polling(self):
        repo = self.repo({"a.txt": "v0\n"})
        self.env["DELEGATE_LOOP_THRESHOLD"] = "3"
        self.env["DELEGATE_STALL_IDLE"] = "0"
        self.env["DELEGATE_STALL_COMMAND"] = "0"

        def read_loop_warns(run_id: str):
            ev_path = self.state_dir() / "events.jsonl"
            if not ev_path.is_file():
                return []
            out = []
            for line in ev_path.read_text(encoding="utf-8").splitlines():
                if not line.strip():
                    continue
                ev = json.loads(line)
                if (
                    ev.get("run") == run_id
                    and ev.get("kind") == "stall_warning"
                    and ev.get("payload", {}).get("stallKind") == "loop"
                ):
                    out.append(ev)
            return out

        # 5 identical sleep + tail polling commands must NOT trigger loop warning.
        poll_events = []
        for _ in range(5):
            poll_events.extend([
                {
                    "type": "tool_execution_start",
                    "toolName": "bash",
                    "args": {"command": "sleep 0.01 && tail -n 5 /dev/null"},
                },
                {
                    "type": "tool_execution_end",
                    "toolName": "bash",
                    "isError": False,
                    "result": {"content": [{"type": "text", "text": ""}]},
                },
            ])
        poll_events.extend([fixtures.answer("polling finished"), fixtures.SETTLED])
        self.fake_pi(poll_events)
        r_poll = self.outcome(
            self.cli("run", "--read-only", "--workdir", repo, "--name", "poll-ok", "wait for bg")
        )
        self.assertEqual(
            read_loop_warns(r_poll["run"]),
            [],
            "pure sleep/tail polling must not emit loop stall_warning",
        )

        # 3 identical real failing commands MUST still trigger loop stall_warning.
        loop_events = []
        for _ in range(3):
            loop_events.extend([
                {
                    "type": "tool_execution_start",
                    "toolName": "bash",
                    "args": {"command": "cargo test --no-fail-fast"},
                },
                {
                    "type": "tool_execution_end",
                    "toolName": "bash",
                    "isError": True,
                    "result": {"content": [{"type": "text", "text": "test failed"}]},
                },
            ])
        loop_events.extend([fixtures.answer("done"), fixtures.SETTLED])
        self.fake_pi(loop_events)
        r_loop = self.outcome(
            self.cli("run", "--read-only", "--workdir", repo, "--name", "real-loop", "run tests")
        )
        warns = read_loop_warns(r_loop["run"])
        self.assertEqual(len(warns), 1, f"expected 1 loop warning, got {warns}")
        self.assertEqual(warns[0]["payload"]["count"], 3)
        self.assertIn("cargo test", warns[0]["payload"]["cmd"])


if __name__ == "__main__":
    unittest.main()
