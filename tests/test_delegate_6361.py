"""6.36.1 regressions:
1. Human `delegate events` (non-`--follow`, non-`--json`) atomically advances the caller's cursor
   in `<state>/cursors.json` (`DELEGATE_CALLER` / `CLAUDE_CODE_SESSION_ID` / ... or `default`)
   to the last printed seq so running the Stop hook's suggested `delegate events --since N`
   clears the block; `--json` and `--follow` leave cursors untouched.
2. Stop hook suppresses stale/resolved unread alert events:
   - `work_ready` once the work item is closed (`done`), dropped, or no longer `ready` (e.g. `verify`);
   - `work_defer_expired` / `work_stale` once closed, freshly re-deferred, or state-changed;
   - `delivered` / `question` / `rejected` / `failed` once the run is applied, cleaned, or replied to;
   - `stall_warning` once the run is stopped/finished;
   - `premerge_conflict` / `premerge_failed` once `premerge_ready` supersedes it;
   - `needs_attention` / `review_verdict(NEEDS_CONTROLLER)` once `review_verdict(PASS)` or valid `sign.json` exists;
   - `queue_paused` once the queue item is cancelled/dispatched.
"""
import json
import re
import select
import signal
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures

ROOT = Path(__file__).resolve().parent.parent
STOP_HOOK = ROOT / "skills/delegate/hooks/claude-code-stop.py"
SUGGESTED_EVENTS_RE = re.compile(r"delegate events --since (\d+)")


class Delegate6361Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def clean_caller_env(self, extra: dict | None = None) -> dict[str, str]:
        env = {k: v for k, v in self.env.items() if k not in fixtures.CALLER_ENV}
        if extra:
            env.update(extra)
        return env

    def run_delegate(self, args: list[str], cwd: Path, env: dict[str, str]) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(fixtures.DELEGATE), *args],
            cwd=cwd,
            capture_output=True,
            text=True,
            env=env,
            timeout=30,
        )

    def run_stop_hook(
        self,
        cwd: Path,
        env: dict[str, str],
        session_id: str | None = None,
        active: bool = False,
    ) -> dict | None:
        hook_env = {
            **env,
            "DELEGATE_BIN": str(fixtures.DELEGATE),
            "DELEGATE_STOP_HOOK_WAITERS": "",
            "DELEGATE_STOP_HOOK_PROCS": "",
            "DELEGATE_STOP_HOOK_CURRENT_ONLY": "1",
        }
        payload: dict = {"cwd": str(cwd), "stop_hook_active": active}
        if session_id is not None:
            payload["session_id"] = session_id
        res = subprocess.run(
            [sys.executable, str(STOP_HOOK)],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            env=hook_env,
            timeout=10,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        out = res.stdout.strip()
        return json.loads(out) if out else None

    def read_cursors(self) -> dict:
        p = self.state_dir() / "cursors.json"
        return json.loads(p.read_text(encoding="utf-8")) if p.is_file() else {}

    def append_event(self, run: str, repo: Path, kind: str, payload: dict) -> int:
        sdir = self.state_dir()
        sdir.mkdir(parents=True, exist_ok=True)
        ev_file = sdir / "events.jsonl"
        seq_file = sdir / "events.seq"
        cur = 0
        if ev_file.is_file():
            for ln in ev_file.read_text(encoding="utf-8").splitlines():
                ln = ln.strip()
                if ln:
                    cur = max(cur, int(json.loads(ln).get("seq") or 0))
        nxt = cur + 1
        rec = {
            "seq": nxt,
            "at": "2026-11-23T12:00:00Z",
            "run": run,
            "repo": str(repo.resolve()),
            "kind": kind,
            "payload": payload,
        }
        with ev_file.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(rec, ensure_ascii=False) + "\n")
        seq_file.write_text(f"{nxt}\n", encoding="utf-8")
        return nxt

    def test_stop_hook_suggested_events_command_clears_block_with_session_id(self):
        repo = self.repo({"a.txt": "v0\n"})
        sid = "session-claude-6361"
        other_sid = "session-other-6361"
        sess_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": sid})
        other_env = self.clean_caller_env({"DELEGATE_CALLER": other_sid})

        # Create W001 + W002 waiting on W001, then close W001 to emit a real `work_ready` event.
        brief = repo / "docs/downstream-brief.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# Downstream\n")
        self.assertEqual(
            self.run_delegate(["work", "add", "前置任务", "--repo", str(repo), "--brief", str(brief)], repo, sess_env).returncode,
            0,
        )
        self.assertEqual(
            self.run_delegate(
                ["work", "add", "待派下游", "--repo", str(repo), "--after-work", "W001", "--brief", str(brief)],
                repo,
                sess_env,
            ).returncode,
            0,
        )
        # Seed an earlier cursor for this session (simulating the 6.36.0 stuck-at-309 scenario).
        sdir = self.state_dir()
        sdir.mkdir(parents=True, exist_ok=True)
        (sdir / "cursors.json").write_text(
            json.dumps(
                {
                    "default": {"seq": 0, "readRuns": {}},
                    "callers": {
                        sid: {"seq": 0, "readRuns": {}},
                        other_sid: {"seq": 0, "readRuns": {}},
                    },
                }
            )
            + "\n",
            encoding="utf-8",
        )

        # Closing W001 unlocks W002 (`ready`) and emits `work_ready`.
        self.assertEqual(
            self.run_delegate(["work", "close", "W001", "--evidence", "done"], repo, sess_env).returncode,
            0,
        )

        # 1) Stop hook blocks and suggests `delegate events --since 0`.
        blk = self.run_stop_hook(repo, sess_env, session_id=sid)
        self.assertIsNotNone(blk)
        self.assertEqual(blk["decision"], "block")
        self.assertIn("工作项就绪:W002", blk["reason"])
        m = SUGGESTED_EVENTS_RE.search(blk["reason"])
        self.assertIsNotNone(m, f"missing suggested command in: {blk['reason']}")
        since_arg = m.group(1)
        self.assertEqual(since_arg, "0")

        # 2) `--json` must NOT advance cursor; hook must STILL block.
        jres = self.run_delegate(["events", "--since", since_arg, "--json"], repo, sess_env)
        self.assertEqual(jres.returncode, 0, jres.stderr)
        self.assertEqual(self.read_cursors()["callers"][sid]["seq"], 0)
        self.assertIsNotNone(self.run_stop_hook(repo, sess_env, session_id=sid))

        # 3) `--follow` must NOT advance cursor; terminate cleanly after initial print.
        proc = subprocess.Popen(
            [str(fixtures.DELEGATE), "events", "--since", since_arg, "--follow"],
            cwd=repo,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=sess_env,
        )
        try:
            # Wait until `--follow` prints its initial event line before sending SIGINT.
            ready, _, _ = select.select([proc.stdout], [], [], 10.0)
            self.assertTrue(ready, "events --follow did not print initial event")
            first_line = proc.stdout.readline()
            self.assertTrue(first_line.strip(), "expected non-empty initial event line")
            proc.send_signal(signal.SIGINT)
            proc.communicate(timeout=5)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait(timeout=5)
        self.assertEqual(self.read_cursors()["callers"][sid]["seq"], 0)

        # 4) Execute the EXACT suggested command `delegate events --since <N>` -> cursor advances!
        ev_res = self.run_delegate(["events", "--since", since_arg], repo, sess_env)
        self.assertEqual(ev_res.returncode, 0, ev_res.stderr)
        self.assertIn("work_ready", ev_res.stdout)
        self.assertIn("游标：", ev_res.stdout)
        cursors_after = self.read_cursors()
        advanced_seq = cursors_after["callers"][sid]["seq"]
        self.assertGreater(advanced_seq, 0)
        # Other caller's cursor remains untouched (session isolation).
        self.assertEqual(cursors_after["callers"][other_sid]["seq"], 0)

        # 5) Stop hook now PASSES for `sid`, while `other_sid` still sees the unread event!
        self.assertIsNone(self.run_stop_hook(repo, sess_env, session_id=sid))
        other_blk = self.run_stop_hook(repo, other_env, session_id=other_sid)
        self.assertIsNotNone(other_blk)
        self.assertIn("工作项就绪:W002", other_blk["reason"])

    def test_stop_hook_suggested_events_command_clears_block_without_session_id_default(self):
        repo = self.repo({"a.txt": "v0\n"})
        default_env = self.clean_caller_env()

        # Start a read-only run and let it deliver with no session id set.
        self.fake_pi([fixtures.answer("read-only summary"), fixtures.SETTLED])
        started = self.outcome(
            self.run_delegate(
                ["start", "--json", "--read-only", "--workdir", str(repo), "--name", "ro-default", "check"],
                repo,
                default_env,
            )
        )
        self.until(lambda: (Path(started["dir"]) / "exit_code").exists(), "ro-default finished")

        # 1) Stop hook blocks on `default` cursor (0) with single-item unread delivery.
        blk = self.run_stop_hook(repo, default_env, session_id=None)
        self.assertIsNotNone(blk)
        self.assertEqual(blk["decision"], "block")
        self.assertIn(f"ro-default 已交付待看。下一步：`delegate card {started['run']}`", blk["reason"])
        since_arg = "0"

        # 2) `--json` leaves `default` cursor untouched -> still blocks.
        jres = self.run_delegate(["events", "--since", since_arg, "--json"], repo, default_env)
        self.assertEqual(jres.returncode, 0, jres.stderr)
        self.assertEqual(self.read_cursors().get("default", {}).get("seq", 0), 0)
        self.assertIsNotNone(self.run_stop_hook(repo, default_env, session_id=None))

        # 3) Run the exact suggested command `delegate events --since <N>` -> advances `default.seq`!
        ev_res = self.run_delegate(["events", "--since", since_arg], repo, default_env)
        self.assertEqual(ev_res.returncode, 0, ev_res.stderr)
        self.assertIn("游标：", ev_res.stdout)
        self.assertGreater(self.read_cursors()["default"]["seq"], 0)

        # 4) Stop hook now PASSES!
        self.assertIsNone(self.run_stop_hook(repo, default_env, session_id=None))

    def test_stale_work_events_do_not_block_stop_hook(self):
        repo = self.repo({"a.txt": "v0\n"})
        env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": "sess-work-stale"})

        # Create W001 (blocker) and four dependents:
        # W002 -> will be closed (`done`)
        # W003 -> will be dropped (`done` + `closedKind=dropped`)
        # W004 -> will enter `verify` (zero-change delivery bound via --work)
        # W005 -> remains `ready`
        self.assertEqual(self.run_delegate(["work", "add", "上游", "--repo", str(repo)], repo, env).returncode, 0)
        for title in ("将关闭项", "将放弃项", "将进验证项", "仍就绪项"):
            res = self.run_delegate(
                ["work", "add", title, "--repo", str(repo), "--after-work", "W001"],
                repo,
                env,
            )
            self.assertEqual(res.returncode, 0, res.stderr)

        # Closing W001 unlocks W002..W005 and emits 4 `work_ready` events.
        self.assertEqual(
            self.run_delegate(["work", "close", "W001", "--evidence", "merged"], repo, env).returncode,
            0,
        )
        # Close W002, drop W003, move W004 to `verify` via zero-change write run.
        self.assertEqual(
            self.run_delegate(["work", "close", "W002", "--evidence", "done"], repo, env).returncode,
            0,
        )
        self.assertEqual(
            self.run_delegate(["work", "drop", "W003", "--reason", "obsolete"], repo, env).returncode,
            0,
        )
        self.fake_pi([fixtures.answer("checked ok, no edits needed"), fixtures.SETTLED])
        r_v = self.outcome(
            self.run_delegate(
                [
                    "run",
                    "--json",
                    "--worktree",
                    "--workdir",
                    str(repo),
                    "--work",
                    "W004",
                    "--name",
                    "w004-verify",
                    "--accept",
                    "true",
                    "check",
                ],
                repo,
                env,
            )
        )
        # Mark r_v read so only work_* events are evaluated.
        self.assertEqual(self.run_delegate(["result", r_v["run"]], repo, env).returncode, 0)
        w4 = json.loads(self.run_delegate(["work", "show", "W004", "--json"], repo, env).stdout)
        self.assertEqual(w4["state"], "verify", w4)

        # Also append stale `work_defer_expired` and `work_stale` events for already-closed W002.
        self.append_event("", repo, "work_defer_expired", {"workId": "W002", "name": "将关闭项", "reason": "旧暂缓"})
        self.append_event(
            "",
            repo,
            "work_stale",
            {"workId": "W002", "name": "将关闭项", "state": "ready", "reason": "就绪未派 48h"},
        )
        # Also append `work_stale` (state=ready) for W004 which is now in `verify`.
        self.append_event(
            "",
            repo,
            "work_stale",
            {"workId": "W004", "name": "将进验证项", "state": "ready", "reason": "就绪未派 24h"},
        )

        # Stop hook MUST block ONLY for W005 (still `ready`), NOT for W002/W003/W004!
        blk = self.run_stop_hook(repo, env, session_id="sess-work-stale")
        self.assertIsNotNone(blk)
        self.assertIn("W005", blk["reason"])
        for suppressed in ("W002", "W003", "W004"):
            self.assertNotIn(suppressed, blk["reason"], f"stale {suppressed} must not alert: {blk['reason']}")

        # Close W005 as well -> now ALL work_ready events are stale -> Stop hook PASSES without even touching cursor!
        self.assertEqual(
            self.run_delegate(["work", "close", "W005", "--evidence", "done"], repo, env).returncode,
            0,
        )
        self.assertIsNone(self.run_stop_hook(repo, env, session_id="sess-work-stale"))

    def test_stale_run_and_queue_alert_events_do_not_block_stop_hook(self):
        repo = self.repo({"a.txt": "v0\n"})
        observer_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": "sess-observer"})
        actor_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": "sess-actor"})

        # 1) Delivered run applied by ANOTHER session -> observer's cursor did not advance,
        #    but `applied` event + `.applied` marker suppress the stale `delivered` alert.
        self.fake_pi([fixtures.answer("edit v1"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r_applied = self.outcome(
            self.run_delegate(
                ["run", "--json", "--worktree", "--workdir", str(repo), "--name", "applied-run", "--accept", "true", "e1"],
                repo,
                actor_env,
            )
        )
        self.assertEqual(self.run_delegate(["apply", r_applied["run"]], repo, actor_env).returncode, 0)
        self.assertIsNone(self.run_stop_hook(repo, observer_env, session_id="sess-observer"))

        # 2) Delivered run cleaned by another session -> `cleaned` suppresses stale `delivered` alert.
        self.fake_pi([fixtures.answer("edit v2"), fixtures.SETTLED], pre="echo v2 > a.txt")
        r_cleaned = self.outcome(
            self.run_delegate(
                ["run", "--json", "--worktree", "--workdir", str(repo), "--name", "cleaned-run", "--accept", "true", "e2"],
                repo,
                actor_env,
            )
        )
        self.assertEqual(
            self.run_delegate(["clean", "--force", "--no-archive", r_cleaned["run"]], repo, actor_env).returncode,
            0,
        )
        self.assertIsNone(self.run_stop_hook(repo, observer_env, session_id="sess-observer"))

        # 3) Question run replied to -> parent's `question` event is superseded by child run;
        #    applying the child resolves both parent's `question` and child's `delivered`.
        self.fake_pi([fixtures.answer("需要主控决定\n选方案 A 还是 B？"), fixtures.SETTLED])
        r_q = self.outcome(
            self.run_delegate(
                ["run", "--json", "--worktree", "--workdir", str(repo), "--name", "q-parent", "--accept", "true", "ask"],
                repo,
                actor_env,
            )
        )
        self.fake_pi([fixtures.answer("按方案 A 已完成"), fixtures.SETTLED], pre="echo v3 > a.txt")
        r_reply = self.outcome(
            self.run_delegate(["reply", "--json", r_q["run"], "选 A"], repo, actor_env)
        )
        self.until(lambda: (Path(r_reply["dir"]) / "exit_code").exists(), "reply finished")
        self.assertEqual(self.run_delegate(["apply", "--allow-dirty-overlap", r_reply["run"]], repo, actor_env).returncode, 0)
        self.assertIsNone(self.run_stop_hook(repo, observer_env, session_id="sess-observer"))

        # 4) Valid `sign.json` suppresses `needs_attention` / `review_verdict(NEEDS_CONTROLLER)` on that run.
        self.fake_pi([fixtures.answer("zero edit"), fixtures.SETTLED])
        r_signed = self.outcome(
            self.run_delegate(
                ["run", "--json", "--worktree", "--workdir", str(repo), "--name", "signed-run", "--accept", "true", "chk"],
                repo,
                actor_env,
            )
        )
        r_signed_dir = Path(r_signed["dir"])
        (r_signed_dir / "review.json").write_text(
            json.dumps({"verdict": "NEEDS_CONTROLLER", "controllerReview": {"hash": "sha256:abc123", "paths": ["a.txt"]}}),
            encoding="utf-8",
        )
        (r_signed_dir / "sign.json").write_text(
            json.dumps({"hash": "sha256:abc123", "signedAt": "2026-11-23T12:00:00Z"}),
            encoding="utf-8",
        )
        self.append_event(r_signed["run"], repo, "needs_attention", {"name": "signed-run", "verdict": "NEEDS_CONTROLLER"})
        self.append_event(r_signed["run"], repo, "review_verdict", {"name": "signed-run", "verdict": "NEEDS_CONTROLLER"})
        # Mark r_signed's `delivered` read for observer? Wait: `delivered` was emitted BEFORE `needs_attention`;
        # test with a separate observer cursor past `delivered` or just clean `r_signed` after testing `sign.json`!
        evs_so_far = [
            json.loads(ln)
            for ln in (self.state_dir() / "events.jsonl").read_text(encoding="utf-8").splitlines()
            if ln.strip()
        ]
        delivered_seq = max(e["seq"] for e in evs_so_far if e["run"] == r_signed["run"] and e["kind"] == "delivered")
        cpath = self.state_dir() / "cursors.json"
        cdoc = self.read_cursors() or {"default": {"seq": 0, "readRuns": {}}, "callers": {}}
        cdoc.setdefault("callers", {})["sess-observer"] = {"seq": delivered_seq, "readRuns": {}}
        cpath.write_text(json.dumps(cdoc) + "\n", encoding="utf-8")
        # Both `needs_attention` and `review_verdict(NEEDS_CONTROLLER)` are > delivered_seq, yet suppressed by valid sign.json!
        self.assertIsNone(self.run_stop_hook(repo, observer_env, session_id="sess-observer"))

        # 5) Synthetic resolved alerts:
        #    - `stall_warning` followed by `stopped`
        #    - `premerge_conflict` & `premerge_failed` followed by `premerge_ready`
        #    - `needs_attention` & `review_verdict(NEEDS_CONTROLLER)` followed by `review_verdict(PASS)`
        #    - `queue_paused` for a cancelled queue item in `dag-queue.json`
        self.append_event("run-stall-1", repo, "stall_warning", {"name": "stalled-job"})
        self.append_event("run-stall-1", repo, "stopped", {"name": "stalled-job"})

        self.append_event("run-pm-1", repo, "premerge_conflict", {"name": "pm-job", "conflicts": ["a.txt"]})
        self.append_event("run-pm-1", repo, "premerge_failed", {"name": "pm-job", "error": "boom"})
        self.append_event("run-pm-1", repo, "premerge_ready", {"name": "pm-job", "baseHead": "abc1234"})

        self.append_event("run-rev-1", repo, "needs_attention", {"name": "rev-job", "verdict": "NEEDS_CONTROLLER"})
        self.append_event("run-rev-1", repo, "review_verdict", {"name": "rev-job", "verdict": "NEEDS_CONTROLLER"})
        self.append_event("run-rev-1", repo, "review_verdict", {"name": "rev-job", "verdict": "PASS"})

        self.append_event("run-q-1", repo, "queue_paused", {"queueName": "step-2", "reason": "upstream failed"})
        (self.state_dir() / "dag-queue.json").write_text(
            json.dumps({"items": [{"name": "step-2", "status": "cancelled"}]}),
            encoding="utf-8",
        )

        # None of those resolved alerts may block the Stop hook!
        self.assertIsNone(self.run_stop_hook(repo, observer_env, session_id="sess-observer"))

        # Contrast: an active `queue_paused` (status == "paused") MUST still block!
        self.append_event("run-q-2", repo, "queue_paused", {"queueName": "step-3", "reason": "still paused"})
        (self.state_dir() / "dag-queue.json").write_text(
            json.dumps(
                {
                    "items": [
                        {"name": "step-2", "status": "cancelled"},
                        {"name": "step-3", "status": "paused"},
                    ]
                }
            ),
            encoding="utf-8",
        )
        blk_q = self.run_stop_hook(repo, observer_env, session_id="sess-observer")
        self.assertIsNotNone(blk_q)
        self.assertIn("step-3", blk_q["reason"])
        self.assertNotIn("step-2", blk_q["reason"])


if __name__ == "__main__":
    unittest.main()
