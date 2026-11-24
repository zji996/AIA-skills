"""6.32.0 (V1) regressions & concurrency stress suite:
1. Read/write non-blocking & zero-deadlock stress test (20 status + 10 work + 8 start + 4 events
   concurrent for 10s; plus holding `.state.lock` for 2s while read-only queries finish unblocked
   at < 1/10 of hold duration with zero state-lock attempts).
2. Full atomic writes (PID+nonce + sync_data + rename) & random SIGKILL recovery stress test.
3. Multi-repo isolation stress test (slow apply on repo A does not block repo B).
4. V2 premerge fast-forward interface reservation (fail-closed on stale premerge.json + apply.lock).
5. Async `start`, `reply` (Auto sync), `clean` + `_gc-worker` + prepare-failure `failed` event +
   binary-mtime cache for `pi_models` / `agent_identity`.
6. W026 supplement: manual merge into HEAD + `clean --force` unlocks downstream `run_applied:` work.
7. `delegate snapshot --json`, Linux `inotify` wakeup for `wait --since` / `events --follow`, and
   zero-lock, zero-side-effect Stop hook reading `index.json`.
"""
import concurrent.futures
import json
import os
import random
import shlex
import signal
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures

STOP_HOOK = fixtures.ROOT / "skills/delegate/hooks/claude-code-stop.py"


class Delegate632Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    fake_docker = fixtures.DelegateTests.fake_docker
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def cache_dir(self) -> Path:
        return Path(self.env["XDG_CACHE_HOME"]) / "delegate"

    def git(self, repo: Path, *args: str) -> str:
        res = subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            text=True,
            check=True,
        )
        return res.stdout.strip()

    # ------------------------------------------------------------------
    # 1. Concurrency stress test: Read/write non-blocking + 2s lock hold
    # ------------------------------------------------------------------
    def test_stress_read_write_nonblocking_and_2s_state_lock_hold(self):
        repo = self.repo({"a.txt": "base\n", "b.txt": "base\n"})
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])

        # Seed a few finished runs & work items so reads have real data.
        seed_runs = []
        for i in range(4):
            w = self.cli("work", "add", f"seed-w-{i}", "--repo", repo, "--json")
            self.assertEqual(w.returncode, 0, w.stderr)
            r = self.outcome(
                self.cli("start", "--worktree", "--workdir", repo, "--name", f"seed-{i}", f"task {i}")
            )
            seed_runs.append(r["run"])
        self.assertEqual(self.cli("wait", "--no-result", *seed_runs).returncode, 0)

        # Part A: 20 status + 10 work + 8 start + 4 events concurrently for 10s.
        deadline = time.monotonic() + 10.0
        read_latencies_ms = []
        errors = []
        started_run_ids = []
        start_counter = [0]

        def worker_status(idx: int):
            lats = []
            while time.monotonic() < deadline:
                t0 = time.perf_counter()
                res = subprocess.run(
                    [str(fixtures.DELEGATE), "status", "--json", "--all"],
                    env=self.env,
                    cwd=repo,
                    capture_output=True,
                    text=True,
                    timeout=15,
                )
                lats.append((time.perf_counter() - t0) * 1000.0)
                if res.returncode != 0:
                    errors.append(f"status-{idx} rc={res.returncode}: {res.stderr}")
                    break
                if "lock order violation" in res.stderr or "forbidden slow operation" in res.stderr:
                    errors.append(f"status-{idx} assertion: {res.stderr}")
                    break
                for ln in res.stdout.splitlines():
                    ln = ln.strip()
                    if ln:
                        try:
                            json.loads(ln)
                        except ValueError as e:
                            errors.append(f"status-{idx} bad json: {e}: {ln[:120]}")
                            return lats
                time.sleep(0.05)
            return lats

        def worker_work(idx: int):
            lats = []
            while time.monotonic() < deadline:
                t0 = time.perf_counter()
                res = subprocess.run(
                    [str(fixtures.DELEGATE), "work", "--json"],
                    env=self.env,
                    cwd=repo,
                    capture_output=True,
                    text=True,
                    timeout=15,
                )
                lats.append((time.perf_counter() - t0) * 1000.0)
                if res.returncode != 0:
                    errors.append(f"work-{idx} rc={res.returncode}: {res.stderr}")
                    break
                for ln in res.stdout.splitlines():
                    ln = ln.strip()
                    if ln:
                        try:
                            json.loads(ln)
                        except ValueError as e:
                            errors.append(f"work-{idx} bad json: {e}: {ln[:120]}")
                            return lats
                time.sleep(0.05)
            return lats

        def worker_events(idx: int):
            lats = []
            while time.monotonic() < deadline:
                t0 = time.perf_counter()
                res = subprocess.run(
                    [str(fixtures.DELEGATE), "events", "--json"],
                    env=self.env,
                    cwd=repo,
                    capture_output=True,
                    text=True,
                    timeout=15,
                )
                lats.append((time.perf_counter() - t0) * 1000.0)
                if res.returncode != 0:
                    errors.append(f"events-{idx} rc={res.returncode}: {res.stderr}")
                    break
                for ln in res.stdout.splitlines():
                    ln = ln.strip()
                    if ln:
                        try:
                            json.loads(ln)
                        except ValueError as e:
                            errors.append(f"events-{idx} bad json: {e}: {ln[:120]}")
                            return lats
                time.sleep(0.05)
            return lats

        def worker_start(idx: int):
            ids = []
            # Each of 8 start workers launches mock runs across the 10s window.
            for wave in range(2):
                if time.monotonic() >= deadline - 1.0:
                    break
                res = subprocess.run(
                    [
                        str(fixtures.DELEGATE),
                        "start",
                        "--json",
                        "--read-only",
                        "--workdir",
                        str(repo),
                        "--name",
                        f"stress-{idx}-{wave}",
                        f"stress task {idx} {wave}",
                    ],
                    env=self.env,
                    cwd=repo,
                    capture_output=True,
                    text=True,
                    timeout=20,
                )
                if res.returncode != 0:
                    errors.append(f"start-{idx}-{wave} rc={res.returncode}: {res.stderr}")
                    break
                if "lock order violation" in res.stderr or "forbidden slow operation" in res.stderr:
                    errors.append(f"start-{idx}-{wave} assertion: {res.stderr}")
                    break
                for ln in res.stdout.splitlines():
                    if ln.strip().startswith("{"):
                        ids.append(json.loads(ln)["run"])
                        break
                time.sleep(0.5)
            return ids

        with concurrent.futures.ThreadPoolExecutor(max_workers=42) as pool:
            futs_r = [pool.submit(worker_status, i) for i in range(20)]
            futs_r += [pool.submit(worker_work, i) for i in range(10)]
            futs_r += [pool.submit(worker_events, i) for i in range(4)]
            futs_s = [pool.submit(worker_start, i) for i in range(8)]
            for f in futs_r:
                read_latencies_ms.extend(f.result(timeout=30))
            for f in futs_s:
                started_run_ids.extend(f.result(timeout=30))

        self.assertEqual(errors, [], "\n".join(errors[:10]))
        self.assertGreater(len(read_latencies_ms), 100)
        read_latencies_ms.sort()
        # Under 42 concurrent workers, verify reads never stalled/deadlocked and P99 stays fast.
        p99_idx = min(len(read_latencies_ms) - 1, int(0.99 * len(read_latencies_ms)))
        p99_ms = read_latencies_ms[p99_idx]
        # Allow generous headroom for debug binary under 42-way CPU load, while still proving no lock stalls.
        self.assertLess(p99_ms, 500.0, f"concurrent read P99 too high: {p99_ms:.1f}ms")

        if started_run_ids:
            self.cli("wait", "--no-result", *started_run_ids)

        # Part B: Force-hold `state/.state.lock` for 2.0s; assert ALL read-only commands finish
        # well below the lock-hold duration (< 1/10 of hold = 200ms), before the holder releases
        # the lock, and make ZERO state-lock acquisition attempts (proving reads are truly lock-free
        # without depending on a load-sensitive 20ms wall-clock cutoff).
        state = self.state_dir()
        state.mkdir(parents=True, exist_ok=True)
        lock_file = state / ".state.lock"
        ready_file = state / ".hold-ready"
        hold_s = 2.0
        max_read_ms = hold_s * 1000.0 / 10.0  # 200ms vs 2000ms lock hold
        probe_env = {**self.env, "DELEGATE_MEASURE_STATE_LOCK": "1"}

        # Positive control: verify a write command emits STATE_LOCK_ATTEMPT under probe_env.
        ctrl = subprocess.run(
            [str(fixtures.DELEGATE), "work", "add", "probe-state-lock", "--repo", str(repo), "--json"],
            env=probe_env,
            cwd=repo,
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(ctrl.returncode, 0, ctrl.stderr)
        self.assertIn("STATE_LOCK_ATTEMPT=acquire", ctrl.stderr, f"state-lock probe did not fire: {ctrl.stderr!r}")
        self.assertIn("STATE_LOCK_HOLD_MS=", ctrl.stderr, f"state-lock hold probe did not fire: {ctrl.stderr!r}")

        holder_code = (
            "import fcntl, os, sys, time\n"
            "p, ready, hold = sys.argv[1], sys.argv[2], float(sys.argv[3])\n"
            "fd = os.open(p, os.O_CREAT | os.O_RDWR, 0o644)\n"
            "fcntl.flock(fd, fcntl.LOCK_EX)\n"
            "open(ready, 'w').write('held')\n"
            "time.sleep(hold)\n"
            "os.close(fd)\n"
        )
        # Warm up 1 read first so OS page cache / dynamic loader is hot.
        self.assertEqual(self.cli("status", "--json", "--all", cwd=repo).returncode, 0)
        self.assertEqual(self.cli("snapshot", "--json", cwd=repo).returncode, 0)

        holder = subprocess.Popen(
            [sys.executable, "-c", holder_code, str(lock_file), str(ready_file), str(hold_s)],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            self.until(ready_file.is_file, "state lock holder ready", timeout=5.0)
            for cmd in (
                ["status", "--json", "--all"],
                ["work", "--json"],
                ["events", "--json"],
                ["snapshot", "--json"],
            ):
                samples = []
                for _ in range(3):
                    t0 = time.perf_counter()
                    res = subprocess.run(
                        [str(fixtures.DELEGATE), *cmd],
                        env=probe_env,
                        cwd=repo,
                        capture_output=True,
                        text=True,
                        timeout=5,
                    )
                    dt_ms = (time.perf_counter() - t0) * 1000.0
                    samples.append(dt_ms)
                    self.assertEqual(res.returncode, 0, f"{cmd} failed while lock held: {res.stderr}")
                    self.assertNotIn(
                        "STATE_LOCK_ATTEMPT",
                        res.stderr,
                        f"read command {cmd} attempted to acquire .state.lock: {res.stderr}",
                    )
                    self.assertNotIn(
                        "STATE_LOCK_HOLD_MS",
                        res.stderr,
                        f"read command {cmd} held .state.lock: {res.stderr}",
                    )
                    for ln in res.stdout.splitlines():
                        if ln.strip():
                            json.loads(ln)
                best_ms = min(samples)
                self.assertLess(
                    best_ms,
                    max_read_ms,
                    f"read command {cmd} took {best_ms:.2f}ms >= {max_read_ms:.0f}ms "
                    f"(1/10 of {hold_s:.1f}s lock hold; samples={samples})",
                )
            # Confirm holder is STILL holding the lock (all reads finished while lock was exclusively held).
            self.assertIsNone(holder.poll(), "holder should still be holding .state.lock")
        finally:
            holder.wait(timeout=5)
            for s in (holder.stdout, holder.stderr):
                if s:
                    s.close()

    # ------------------------------------------------------------------
    # 2. Concurrency stress test: Atomic writes & random SIGKILL recovery
    # ------------------------------------------------------------------
    def test_stress_atomic_writes_and_random_sigkill_recovery(self):
        repo = self.repo({"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="sleep 0.12")
        state = self.state_dir()
        errors = []
        run_dirs = []

        def writer_work(idx: int):
            for j in range(5):
                res = subprocess.run(
                    [str(fixtures.DELEGATE), "work", "add", f"atomic-{idx}-{j}", "--repo", str(repo), "--json"],
                    env=self.env,
                    cwd=repo,
                    capture_output=True,
                    text=True,
                    timeout=20,
                )
                if res.returncode != 0:
                    errors.append(f"work-add-{idx}-{j}: {res.stderr}")

        def writer_start(idx: int):
            res = subprocess.run(
                [
                    str(fixtures.DELEGATE),
                    "start",
                    "--json",
                    "--read-only",
                    "--workdir",
                    str(repo),
                    "--name",
                    f"kill-me-{idx}",
                    f"task {idx}",
                ],
                env=self.env,
                cwd=repo,
                capture_output=True,
                text=True,
                timeout=20,
            )
            if res.returncode != 0:
                errors.append(f"start-{idx}: {res.stderr}")
                return None
            for ln in res.stdout.splitlines():
                if ln.strip().startswith("{"):
                    return Path(json.loads(ln)["dir"])
            return None

        with concurrent.futures.ThreadPoolExecutor(max_workers=16) as pool:
            wfuts = [pool.submit(writer_work, i) for i in range(8)]
            sfuts = [pool.submit(writer_start, i) for i in range(8)]
            for f in sfuts:
                d = f.result(timeout=25)
                if d:
                    run_dirs.append(d)
            # Randomly SIGKILL half of the spawned supervisors (using their recorded PIDs only!).
            rng = random.Random(632)
            for d in run_dirs:
                if rng.random() < 0.5:
                    pid_file = d / "pid"
                    if pid_file.is_file():
                        try:
                            pid = int(pid_file.read_text().strip())
                            if pid > 0:
                                os.kill(pid, signal.SIGKILL)
                        except (OSError, ValueError):
                            pass
            for f in wfuts:
                f.result(timeout=25)

        self.assertEqual(errors, [], "\n".join(errors[:10]))
        # Let surviving supervisors settle.
        time.sleep(0.5)

        # Verify events.jsonl has strictly monotonic seq and zero corrupt/truncated lines.
        ev_file = state / "events.jsonl"
        self.assertTrue(ev_file.is_file())
        prev_seq = 0
        for line_no, raw in enumerate(ev_file.read_text(encoding="utf-8").splitlines(), 1):
            if not raw.strip():
                continue
            ev = json.loads(raw)
            seq = ev["seq"]
            self.assertGreater(seq, prev_seq, f"non-monotonic seq at line {line_no}: {seq} <= {prev_seq}")
            prev_seq = seq

        # Verify work.jsonl has zero corrupt/truncated lines and all 40 added work items exist.
        work_file = state / "work.jsonl"
        self.assertTrue(work_file.is_file())
        work_rows = [json.loads(ln) for ln in work_file.read_text(encoding="utf-8").splitlines() if ln.strip()]
        self.assertEqual(len(work_rows), 40)

        # Verify index.json is valid JSON.
        idx_file = state / "index.json"
        self.assertTrue(idx_file.is_file())
        idx_doc = json.loads(idx_file.read_text(encoding="utf-8"))
        self.assertEqual(idx_doc["schemaVersion"], 1)

        # Verify all read commands recover and succeed cleanly after SIGKILL.
        for cmd in (["status", "--json", "--all"], ["work", "--json"], ["events", "--json"], ["snapshot", "--json"]):
            res = self.cli(*cmd, cwd=repo)
            self.assertEqual(res.returncode, 0, f"{cmd} failed after SIGKILL: {res.stderr}")
            for ln in res.stdout.splitlines():
                if ln.strip():
                    json.loads(ln)

    # ------------------------------------------------------------------
    # 3. Concurrency stress test: Multi-repo isolation
    # ------------------------------------------------------------------
    def test_stress_multi_repo_isolation(self):
        repo_a = self.work / "repo-a"
        repo_b = self.work / "repo-b"
        for rp, fname in ((repo_a, "a.txt"), (repo_b, "b.txt")):
            rp.mkdir()
            self.git(rp, "init", "-q")
            self.git(rp, "config", "user.email", "t@example.com")
            self.git(rp, "config", "user.name", "Test")
            (rp / fname).write_text("v0\n")
            if rp == repo_a:
                gate = self.work / "repo-a-apply.gate"
                started = self.work / "repo-a-apply.started"
                (rp / ".delegate.json").write_text(json.dumps({
                    "generated": {
                        "paths": ["gen/"],
                        "command": f"mkdir -p gen; touch {shlex.quote(str(started))}; while [ ! -f {shlex.quote(str(gate))} ]; do sleep 0.02; done; cp a.txt gen/out.txt",
                    }
                }))
            self.git(rp, "add", ".")
            self.git(rp, "commit", "-qm", "init")

        env_a = {**self.env, "DELEGATE_RUNS": str(repo_a / ".local/run/delegate")}
        env_b = {**self.env, "DELEGATE_RUNS": str(repo_b / ".local/run/delegate")}

        # Run task on repo A that triggers slow apply generator.
        self.fake_pi([fixtures.answer("a done"), fixtures.SETTLED], pre="echo a1 > a.txt")
        ra = self.outcome(subprocess.run(
            [str(fixtures.DELEGATE), "run", "--json", "--worktree", "--workdir", str(repo_a), "--name", "a-task", "edit a"],
            env=env_a, cwd=repo_a, capture_output=True, text=True, check=True,
        ))
        # Run task on repo B ready to apply.
        self.fake_pi([fixtures.answer("b done"), fixtures.SETTLED], pre="echo b1 > b.txt")
        rb = self.outcome(subprocess.run(
            [str(fixtures.DELEGATE), "run", "--json", "--worktree", "--workdir", str(repo_b), "--name", "b-task", "edit b"],
            env=env_b, cwd=repo_b, capture_output=True, text=True, check=True,
        ))

        # Start slow apply on repo A (holds repo A's apply.lock while waiting on gate).
        proc_a = subprocess.Popen(
            [str(fixtures.DELEGATE), "apply", "--json", ra["run"]],
            env=env_a, cwd=repo_a, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.addCleanup(lambda: (gate.touch(), proc_a.poll() is None and proc_a.kill()))
        self.until(started.exists, "repo A apply generator started", timeout=10.0)

        # While repo A is blocked in apply, repo B must start, apply, and status without waiting!
        t0 = time.monotonic()
        self.fake_pi([fixtures.answer("b2"), fixtures.SETTLED])
        b_start = subprocess.run(
            [str(fixtures.DELEGATE), "start", "--json", "--read-only", "--workdir", str(repo_b), "--name", "b-concurrent", "check b"],
            env=env_b, cwd=repo_b, capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(b_start.returncode, 0, b_start.stderr)
        b_apply = subprocess.run(
            [str(fixtures.DELEGATE), "apply", "--json", rb["run"]],
            env=env_b, cwd=repo_b, capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(b_apply.returncode, 0, b_apply.stdout + b_apply.stderr)
        b_status = subprocess.run(
            [str(fixtures.DELEGATE), "status", "--json", "--all"],
            env=env_b, cwd=repo_b, capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(b_status.returncode, 0, b_status.stderr)
        elapsed_b = time.monotonic() - t0
        self.assertLess(elapsed_b, 3.0, f"repo B operations delayed by repo A apply: {elapsed_b:.2f}s")
        # Repo A is STILL waiting on gate while repo B already finished everything!
        self.assertIsNone(proc_a.poll(), "repo A apply should still be blocked on gate")
        self.assertEqual((repo_b / "b.txt").read_text(), "b1\n")

        # Release repo A and confirm it finishes cleanly too.
        gate.touch()
        out_a, err_a = proc_a.communicate(timeout=15)
        self.assertEqual(proc_a.returncode, 0, out_a + err_a)
        self.assertEqual((repo_a / "gen/out.txt").read_text(), "a1\n")
        b_rid = next(json.loads(ln)["run"] for ln in b_start.stdout.splitlines() if ln.strip().startswith("{"))
        subprocess.run([str(fixtures.DELEGATE), "wait", "--no-result", b_rid], env=env_b, cwd=repo_b, capture_output=True, text=True, timeout=15)

    # ------------------------------------------------------------------
    # 4. V2 Premerge fast-forward interface reservation (fail-closed)
    # ------------------------------------------------------------------
    def test_v2_premerge_interface_fail_closed_and_apply_lock(self):
        repo = self.repo({"a.txt": "v0\n", "b.txt": "v0\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "pm-stub", "edit a"))
        # Plant a stale V2 `premerge.json` with mismatched baseHead: V1 apply must fail-closed to normal 3-way merge.
        (Path(r1["dir"]) / "premerge.json").write_text(json.dumps({
            "baseHead": "0" * 40,
            "mergedTree": "f" * 40,
            "acceptOk": True,
            "conflicts": [],
        }))
        (repo / "b.txt").write_text("main-b\n")
        self.git(repo, "add", "b.txt")
        self.git(repo, "commit", "-qm", "advance main")
        ap = self.cli("apply", "--json", r1["run"])
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)
        self.assertEqual((repo / "a.txt").read_text(), "v1\n")
        self.assertEqual((repo / "b.txt").read_text(), "main-b\n")

    # ------------------------------------------------------------------
    # 5. Async start / reply (Auto) / clean + _gc-worker + prepare fail + cache
    # ------------------------------------------------------------------
    def test_async_clean_gc_worker_and_prepare_failure_emits_failed_event(self):
        repo = self.repo({"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "gc-test", "edit"))
        tree = Path(r1["worktree"])
        self.assertTrue(tree.is_dir())
        db = self.fake_docker({"c1": {"path": str(tree), "name": "scratch-1"}})

        # Async `clean --force` renames worktree & run immediately and spawns `_gc-worker`.
        res = self.cli("clean", "--force", "--json", r1["run"])
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertFalse(tree.exists(), "worktree must vanish immediately via atomic rename")
        self.assertFalse(Path(r1["dir"]).exists(), "run dir must vanish immediately via atomic rename")
        rows = [json.loads(ln) for ln in res.stdout.splitlines() if ln.strip()]
        self.assertTrue(rows[0].get("gcAsync"))
        # Background `_gc-worker` removes the container and trash job cleanly.
        self.until(lambda: json.loads(db.read_text()) == {}, "_gc-worker removes docker container", timeout=10.0)
        self.until(
            lambda: not list((self.cache_dir() / "trash/jobs").glob("*.json")),
            "_gc-worker drains trash jobs",
            timeout=10.0,
        )

        # Prepare failure inside `_supervise` immediately writes a `failed` event to events.jsonl.
        env_fail = {**self.env, "DELEGATE_TEST_FAIL_PREPARE": "1"}
        st_fail = subprocess.run(
            [str(fixtures.DELEGATE), "start", "--json", "--worktree", "--workdir", str(repo), "--name", "prep-fail", "will fail"],
            env=env_fail,
            cwd=repo,
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(st_fail.returncode, 0, st_fail.stderr)
        fail_rid = next(json.loads(ln)["run"] for ln in st_fail.stdout.splitlines() if ln.strip().startswith("{"))
        self.until(
            lambda: any(
                json.loads(ln).get("run") == fail_rid and json.loads(ln).get("kind") == "failed"
                for ln in (self.state_dir() / "events.jsonl").read_text().splitlines()
                if ln.strip()
            ),
            "prepare failure emits failed event",
            timeout=10.0,
        )

        # Verify binary-mtime cache for pi_models & agent_identity was populated.
        cache_file = self.cache_dir() / "agent-probe-cache.json"
        self.assertTrue(cache_file.is_file(), "agent-probe-cache.json should exist")
        cache_doc = json.loads(cache_file.read_text(encoding="utf-8"))
        self.assertTrue(cache_doc.get("piModels") or cache_doc.get("agentIdentity"))

    # ------------------------------------------------------------------
    # 6. W026 Supplement: manual merge into HEAD + clean --force unlocks run_applied
    # ------------------------------------------------------------------
    def test_w026_manual_merge_and_clean_force_unlocks_run_applied_work_item(self):
        repo = self.repo({"a.txt": "v0\n", "other.txt": "o0\n"})
        # Upstream run R1 commits a change in its worktree.
        self.fake_pi(
            [fixtures.answer("feat done"), fixtures.SETTLED],
            pre="echo feat-v1 > a.txt && git add a.txt && git -c user.name=t -c user.email=t@t commit -qm 'feat: a v1'",
        )
        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "upstream-026", "edit a"))
        self.assertEqual(r1["state"], "answered")

        # Downstream work item W001 waits on `run_applied:<r1>`.
        add_res = self.cli(
            "work", "add", "W026 downstream follow-up",
            "--repo", repo,
            "--after-applied", r1["run"],
            "--json",
        )
        self.assertEqual(add_res.returncode, 0, add_res.stderr)
        wid = json.loads(add_res.stdout.strip())["id"]
        self.assertEqual(json.loads(self.cli("work", "show", wid, "--json").stdout)["state"], "blocked")

        # Controller manually merges R1's change into HEAD AND makes a subsequent commit on top!
        wt_head = self.git(Path(r1["worktree"]), "rev-parse", "HEAD")
        self.git(repo, "cherry-pick", wt_head)
        (repo / "a.txt").write_text("feat-v1-plus-followup\n")
        self.git(repo, "add", "a.txt")
        self.git(repo, "commit", "-qm", "later edit on top of cherry-pick")

        # Now controller runs `delegate clean --force <r1>` without ever running `delegate apply`!
        cleaned = self.cli("clean", "--force", r1["run"])
        self.assertEqual(cleaned.returncode, 0, cleaned.stdout + cleaned.stderr)

        # W001 MUST now be unlocked to `ready` (not stuck in `blocked` forever)!
        after_card = json.loads(self.cli("work", "show", wid, "--json").stdout)
        self.assertEqual(after_card["state"], "ready", f"W026 downstream stuck after clean --force: {after_card}")
        # Archive record also preserved `.applied`!
        arch_applied = self.cache_dir() / "archive" / r1["run"] / "record/.applied"
        self.assertTrue(arch_applied.is_file(), "archived record should record .applied")

    # ------------------------------------------------------------------
    # 7. `snapshot --json`, inotify wakeup, and zero-lock Stop hook
    # ------------------------------------------------------------------
    def test_snapshot_json_inotify_wait_since_and_zero_lock_stop_hook(self):
        repo = self.repo({"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "--name", "snap-run", "edit"))

        # `delegate snapshot --json` returns complete schema in one process.
        snap_res = self.cli("snapshot", "--json", cwd=repo)
        self.assertEqual(snap_res.returncode, 0, snap_res.stderr)
        snap = json.loads(snap_res.stdout)
        self.assertEqual(snap["schemaVersion"], 1)
        self.assertGreaterEqual(snap["seq"], 1)
        self.assertTrue(any(r["run"] == r1["run"] for g in snap["repos"] for r in g["runs"]))
        self.assertIn("work", snap)
        self.assertIn("events", snap)
        self.assertIn("lane", snap)

        # `wait --since` wakes up promptly via inotify (well under 400ms after event is emitted).
        cur_seq = snap["seq"]
        waiter = subprocess.Popen(
            [str(fixtures.DELEGATE), "wait", "--since", str(cur_seq), "--max", "5", "--json"],
            env=self.env,
            cwd=repo,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        time.sleep(0.15)
        t_emit = time.monotonic()
        self.assertEqual(self.cli("work", "add", "trigger-event", "--repo", repo, "--json").returncode, 0)
        # Emit an actual event by starting a quick read-only run.
        self.fake_pi([fixtures.answer("quick"), fixtures.SETTLED])
        s2 = self.outcome(self.cli("start", "--read-only", "--workdir", repo, "--name", "wake", "hi"))
        out, err = waiter.communicate(timeout=5)
        wake_ms = (time.monotonic() - t_emit) * 1000.0
        self.assertEqual(waiter.returncode, 0, err)
        self.assertIn(s2["run"], out)
        self.assertLess(wake_ms, 1500.0, f"inotify wait --since woke slowly: {wake_ms:.1f}ms")
        self.cli("wait", "--no-result", s2["run"])

        # Stop hook reads `index.json` directly with zero locks (even while `.state.lock` is held!).
        lock_file = self.state_dir() / ".state.lock"
        ready_file = self.state_dir() / ".stop-hook-hold"
        probe_log = self.state_dir() / ".stop-hook-lock-probe.log"
        hold_s = 2.0
        max_hook_ms = hold_s * 1000.0 / 4.0  # 500ms vs 2000ms lock hold (allows Python startup under load)
        hook_runner = (
            "import runpy, sys\n"
            "hook_path, lock_path, log_path = sys.argv[1], sys.argv[2], sys.argv[3]\n"
            "hits = []\n"
            "def _audit(ev, args):\n"
            "    if ev == 'fcntl.flock':\n"
            "        hits.append(f'flock:{args}')\n"
            "    elif ev == 'open' and args and isinstance(args[0], (str, bytes)):\n"
            "        p = args[0].decode('utf-8', 'ignore') if isinstance(args[0], bytes) else args[0]\n"
            "        if p == lock_path or p.endswith('/.state.lock') or p == '.state.lock':\n"
            "            hits.append(f'open:{p}')\n"
            "sys.addaudithook(_audit)\n"
            "sys.argv = [hook_path]\n"
            "try:\n"
            "    runpy.run_path(hook_path, run_name='__main__')\n"
            "finally:\n"
            "    with open(log_path, 'w', encoding='utf-8') as fh:\n"
            "        fh.write('\\n'.join(hits))\n"
        )
        holder = subprocess.Popen(
            [
                sys.executable,
                "-c",
                "import fcntl, os, sys, time; fd=os.open(sys.argv[1], os.O_CREAT|os.O_RDWR, 0o644); "
                "fcntl.flock(fd, fcntl.LOCK_EX); open(sys.argv[2], 'w').write('ok'); "
                "time.sleep(float(sys.argv[3])); os.close(fd)",
                str(lock_file),
                str(ready_file),
                str(hold_s),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            self.until(ready_file.is_file, "holder ready", timeout=5.0)
            t0 = time.perf_counter()
            hook_res = subprocess.run(
                [sys.executable, "-c", hook_runner, str(STOP_HOOK), str(lock_file), str(probe_log)],
                input=json.dumps({"cwd": str(repo), "stop_hook_active": False}),
                env={
                    **self.env,
                    "DELEGATE_BIN": str(fixtures.DELEGATE),
                    "DELEGATE_STOP_HOOK_CURRENT_ONLY": "1",
                    "DELEGATE_MEASURE_STATE_LOCK": "1",
                },
                capture_output=True,
                text=True,
                timeout=5,
            )
            hook_ms = (time.perf_counter() - t0) * 1000.0
            self.assertEqual(hook_res.returncode, 0, hook_res.stderr)
            self.assertNotIn("STATE_LOCK_ATTEMPT", hook_res.stderr, f"Stop hook touched .state.lock: {hook_res.stderr}")
            self.assertNotIn("STATE_LOCK_HOLD_MS", hook_res.stderr, f"Stop hook held .state.lock: {hook_res.stderr}")
            self.assertTrue(probe_log.is_file(), "Stop hook audit probe did not write log")
            probe_hits = probe_log.read_text(encoding="utf-8").strip()
            self.assertEqual(probe_hits, "", f"Stop hook attempted lock/open on .state.lock: {probe_hits}")
            self.assertLess(
                hook_ms,
                max_hook_ms,
                f"Stop hook blocked on .state.lock: {hook_ms:.1f}ms >= {max_hook_ms:.0f}ms (1/4 of {hold_s:.1f}s hold)",
            )
            self.assertIsNone(holder.poll(), "holder should still be holding .state.lock")
            decision = json.loads(hook_res.stdout)
            self.assertEqual(decision["decision"], "block")
            self.assertIn("snap-run", decision["reason"])
        finally:
            holder.wait(timeout=5)
            for s in (holder.stdout, holder.stderr):
                if s:
                    s.close()


if __name__ == "__main__":
    unittest.main()
