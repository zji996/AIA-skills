"""Stop hook read-state, caller-scoped ownership, orphan takeover, and per-session work-remind cooldown tests:
(a) Deliveries/questions already read (`resultReadAt`), applied, cleaned, or replied by another session
    are treated as handled and never block any session (even if original caller is dead & timed out).
(b) Non-initiator sessions are not blocked by another living caller's runs (events, question, waiter,
    unmerged) — only counted when blocking on own items; also not blocked before orphan timeout.
(c) Once the original caller is gone AND unclaimed duration exceeds `DELEGATE_ORPHAN_TAKEOVER_HOURS`
    (default 6.0h, configurable), non-initiator sessions receive an orphan takeover reminder.
(d) A session's own unread deliveries still block immediately, and per-caller cursors remain isolated
    so another session reading `events` cannot swallow this session's cursor.
(e) Stale work-item reminders block only the responsible caller (last `work add` / `work wait` /
    `start --work`), never consuming the responsible caller's 6h cooldown when another session stops;
    ownerless items or items whose caller expired past orphan timeout allow takeover with cooldown
    keyed per `(work_id, caller)`.
"""
import json
import os
import shlex
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures

ROOT = Path(__file__).resolve().parent.parent
STOP_HOOK = ROOT / "skills/delegate/hooks/claude-code-stop.py"


def iso_ago(hours: float) -> str:
    ts = int(time.time() - hours * 3600)
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts))


class DelegateStopHookReadStateTests(unittest.TestCase):
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
        extra_env: dict | None = None,
    ) -> dict | None:
        hook_env = {
            **env,
            "DELEGATE_BIN": str(fixtures.DELEGATE),
            "DELEGATE_STOP_HOOK_WAITERS": "",
            "DELEGATE_STOP_HOOK_PROCS": "",
            "DELEGATE_STOP_HOOK_CURRENT_ONLY": "1",
        }
        if extra_env:
            hook_env.update(extra_env)
        payload: dict = {"cwd": str(cwd), "stop_hook_active": False}
        if session_id is not None:
            payload["session_id"] = session_id
        res = subprocess.run(
            [sys.executable, str(STOP_HOOK)],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            env=hook_env,
            timeout=15,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        out = res.stdout.strip()
        return json.loads(out) if out else None

    def spawn_alive_caller(self, caller_id: str) -> subprocess.Popen:
        proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            env={**self.clean_caller_env(), "CLAUDE_CODE_SESSION_ID": caller_id},
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.addCleanup(lambda: (proc.kill(), proc.wait(timeout=5)) if proc.poll() is None else None)
        return proc

    def get_dead_pid(self) -> int:
        proc = subprocess.Popen(
            [sys.executable, "-c", "pass"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        proc.wait(timeout=5)
        return proc.pid

    def write_sessions_json(self, doc: dict) -> None:
        sdir = self.state_dir()
        sdir.mkdir(parents=True, exist_ok=True)
        (sdir / "sessions.json").write_text(json.dumps(doc, indent=2) + "\n", encoding="utf-8")

    def backdate_events_and_runs(self, hours: float, run_dirs: list[Path] | None = None) -> None:
        old_iso = iso_ago(hours)
        old_epoch = int(time.time() - hours * 3600)
        ev_file = self.state_dir() / "events.jsonl"
        if ev_file.is_file():
            rows = []
            for ln in ev_file.read_text(encoding="utf-8").splitlines():
                ln = ln.strip()
                if not ln:
                    continue
                obj = json.loads(ln)
                obj["at"] = old_iso
                rows.append(json.dumps(obj, ensure_ascii=False))
            ev_file.write_text("\n".join(rows) + "\n", encoding="utf-8")

        for rdir in run_dirs or []:
            meta_p = rdir / "meta.json"
            if meta_p.is_file():
                meta = json.loads(meta_p.read_text(encoding="utf-8"))
                meta["startedAt"] = old_iso
                meta["startedEpoch"] = old_epoch
                meta_p.write_text(json.dumps(meta) + "\n", encoding="utf-8")
            sum_p = rdir / "summary.json"
            if sum_p.is_file():
                summ = json.loads(sum_p.read_text(encoding="utf-8"))
                summ["finishedAt"] = old_iso
                sum_p.write_text(json.dumps(summ) + "\n", encoding="utf-8")

        # Also drop index.json cached status so backdated finishedAt is read cleanly if needed.
        idx_p = self.state_dir() / "index.json"
        if idx_p.is_file():
            idx = json.loads(idx_p.read_text(encoding="utf-8"))
            for entry in (idx.get("runs") or {}).values():
                if isinstance(entry, dict) and isinstance(entry.get("status"), dict):
                    entry["status"]["finishedAt"] = old_iso
                    entry["status"]["startedAt"] = old_iso
            idx_p.write_text(json.dumps(idx) + "\n", encoding="utf-8")

    # ------------------------------------------------------------------
    # (a) Deliveries/questions read (`resultReadAt`), applied, cleaned,
    #     or replied by another session must never block this session.
    # ------------------------------------------------------------------
    def test_a_other_session_read_applied_cleaned_replied_deliveries_do_not_block(self):
        repo = self.repo({"a.txt": "v0\n"})
        main_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": "sess-main-a"})
        chat_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": "sess-chat-a"})

        # 1) Read-only `eyes-design` run via `delegate run` (automatically sets resultReadAt).
        self.fake_pi([fixtures.answer("eyes design conclusion"), fixtures.SETTLED])
        r_eyes = self.outcome(
            self.run_delegate(
                ["run", "--json", "--read-only", "--workdir", str(repo), "--name", "eyes-design", "design"],
                repo,
                main_env,
            )
        )
        eyes_meta = json.loads((Path(r_eyes["dir"]) / "meta.json").read_text(encoding="utf-8"))
        self.assertTrue(eyes_meta.get("resultReadAt"), eyes_meta)

        # 2) Read-only `adr0181-review` started async, then read via `delegate result`.
        self.fake_pi([fixtures.answer("adr0181 review notes"), fixtures.SETTLED])
        r_adr = self.outcome(
            self.run_delegate(
                ["start", "--json", "--read-only", "--workdir", str(repo), "--name", "adr0181-review", "review"],
                repo,
                main_env,
            )
        )
        self.until(lambda: (Path(r_adr["dir"]) / "exit_code").exists(), "adr0181-review finished")
        self.assertEqual(self.run_delegate(["result", r_adr["run"]], repo, main_env).returncode, 0)

        # 3) Write run applied by main session.
        self.fake_pi([fixtures.answer("edited a"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r_app = self.outcome(
            self.run_delegate(
                ["run", "--json", "--worktree", "--workdir", str(repo), "--name", "feat-applied", "--accept", "true", "edit"],
                repo,
                main_env,
            )
        )
        self.assertEqual(self.run_delegate(["apply", r_app["run"]], repo, main_env).returncode, 0)

        # 4) Write run cleaned by main session.
        self.fake_pi([fixtures.answer("scratch"), fixtures.SETTLED], pre="echo scratch > a.txt")
        r_cln = self.outcome(
            self.run_delegate(
                ["start", "--json", "--worktree", "--workdir", str(repo), "--name", "feat-cleaned", "--accept", "true", "tmp"],
                repo,
                main_env,
            )
        )
        self.until(lambda: (Path(r_cln["dir"]) / "exit_code").exists(), "feat-cleaned finished")
        self.assertEqual(
            self.run_delegate(["clean", "--force", "--no-archive", r_cln["run"]], repo, main_env).returncode,
            0,
        )

        # 5) Question run replied to, and its read-only child read via `result`.
        self.fake_pi([fixtures.answer("需要主控决定\n选 A 还是 B？"), fixtures.SETTLED])
        r_q = self.outcome(
            self.run_delegate(
                ["start", "--json", "--read-only", "--workdir", str(repo), "--name", "q-replied", "ask"],
                repo,
                main_env,
            )
        )
        self.until(lambda: (Path(r_q["dir"]) / "exit_code").exists(), "q-replied finished")
        self.fake_pi([fixtures.answer("已按 A 给出方案"), fixtures.SETTLED])
        r_reply = self.outcome(self.run_delegate(["reply", "--json", r_q["run"], "选 A"], repo, main_env))
        self.until(lambda: (Path(r_reply["dir"]) / "exit_code").exists(), "q-replied child finished")
        self.assertEqual(self.run_delegate(["result", r_reply["run"]], repo, main_env).returncode, 0)

        # Simulate main session dead & 12h old so orphan takeover WOULD fire if staleness check failed!
        dead_pid = self.get_dead_pid()
        self.write_sessions_json({"sess-main-a": {"pid": dead_pid, "lastSeenAt": iso_ago(12)}})
        self.backdate_events_and_runs(
            12.0,
            [Path(r_eyes["dir"]), Path(r_adr["dir"]), Path(r_app["dir"]), Path(r_q["dir"]), Path(r_reply["dir"])],
        )

        # Discussion session has cursor at 0 -> MUST NOT be blocked by any of the 5 handled deliveries!
        self.assertIsNone(self.run_stop_hook(repo, chat_env, session_id="sess-chat-a"))

        # Even if index.json is stripped of `meta.resultReadAt`, disk `meta.json` fallback still suppresses!
        idx_p = self.state_dir() / "index.json"
        if idx_p.is_file():
            idx = json.loads(idx_p.read_text(encoding="utf-8"))
            for entry in (idx.get("runs") or {}).values():
                if isinstance(entry, dict) and isinstance(entry.get("meta"), dict):
                    entry["meta"]["resultReadAt"] = ""
            idx_p.write_text(json.dumps(idx) + "\n", encoding="utf-8")
        self.assertIsNone(self.run_stop_hook(repo, chat_env, session_id="sess-chat-a"))

    # ------------------------------------------------------------------
    # (b) Living original caller -> non-initiator not blocked (count only);
    #     dead caller before orphan timeout -> still not blocked.
    # ------------------------------------------------------------------
    def test_b_living_caller_non_initiator_not_blocked_and_only_counted(self):
        repo = self.repo({"a.txt": "v0\n"})
        main_sid = "sess-main-b"
        chat_sid = "sess-chat-b"
        main_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": main_sid})
        chat_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": chat_sid})

        # Keep a real live process for `sess-main-b`!
        alive_proc = self.spawn_alive_caller(main_sid)

        # 1) Unread read-only delivery owned by main.
        self.fake_pi([fixtures.answer("unread ro"), fixtures.SETTLED])
        r_ro = self.outcome(
            self.run_delegate(
                ["start", "--json", "--read-only", "--workdir", str(repo), "--name", "main-ro", "check"],
                repo,
                main_env,
            )
        )
        self.until(lambda: (Path(r_ro["dir"]) / "exit_code").exists(), "main-ro finished")

        # 2) Unread + unmerged write delivery owned by main.
        self.fake_pi([fixtures.answer("edited a"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r_wr = self.outcome(
            self.run_delegate(
                ["start", "--json", "--worktree", "--workdir", str(repo), "--name", "main-wr", "--accept", "true", "edit"],
                repo,
                main_env,
            )
        )
        self.until(lambda: (Path(r_wr["dir"]) / "exit_code").exists(), "main-wr finished")

        # 3) Open questioning run owned by main.
        self.fake_pi([fixtures.answer("需要主控决定\n选方案 1 还是 2？"), fixtures.SETTLED])
        r_q = self.outcome(
            self.run_delegate(
                ["start", "--json", "--read-only", "--workdir", str(repo), "--name", "main-q", "ask"],
                repo,
                main_env,
            )
        )
        self.until(lambda: (Path(r_q["dir"]) / "exit_code").exists(), "main-q finished")

        # 4) Live running job owned by main (no waiter).
        gate = self.work / "main-live-gate-b"
        ready = self.work / "main-live-ready-b"
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            gate=gate,
            ready_file=ready,
        )
        r_live = self.outcome(
            self.run_delegate(
                ["start", "--json", "--read-only", "--workdir", str(repo), "--name", "main-live", "wait"],
                repo,
                main_env,
            )
        )
        self.addCleanup(lambda: (gate.touch(), self.run_delegate(["stop", r_live["run"]], repo, main_env)))
        self.until(ready.is_file, "main-live agent entered gate")

        # Even if main's runs are 10h old (> 6h!), main is ALIVE -> chat session MUST NOT be blocked!
        self.backdate_events_and_runs(10.0, [Path(r_ro["dir"]), Path(r_wr["dir"]), Path(r_q["dir"])])
        self.assertIsNone(self.run_stop_hook(repo, chat_env, session_id=chat_sid))

        # When chat session has its OWN unread delivery, it blocks ONLY for its own item and counts main's!
        self.fake_pi([fixtures.answer("chat own answer"), fixtures.SETTLED])
        r_chat = self.outcome(
            self.run_delegate(
                ["start", "--json", "--read-only", "--workdir", str(repo), "--name", "chat-own", "check"],
                repo,
                chat_env,
            )
        )
        self.until(lambda: (Path(r_chat["dir"]) / "exit_code").exists(), "chat-own finished")
        blk_own = self.run_stop_hook(repo, chat_env, session_id=chat_sid)
        self.assertIsNotNone(blk_own)
        self.assertIn(f"chat-own 已交付待看。下一步：`delegate card {r_chat['run']}`", blk_own["reason"])
        for foreign in ("main-ro", "main-wr", "main-q", "main-live"):
            self.assertNotIn(foreign, blk_own["reason"])

        # Mark chat-own read -> chat passes again!
        self.assertEqual(self.run_delegate(["result", r_chat["run"]], repo, chat_env).returncode, 0)
        self.assertIsNone(self.run_stop_hook(repo, chat_env, session_id=chat_sid))

        # Stop live job, kill main's process, and test fresh (<6h) dead caller -> STILL not blocked!
        gate.touch()
        self.run_delegate(["stop", r_live["run"]], repo, main_env)
        alive_proc.kill()
        alive_proc.wait(timeout=5)
        self.write_sessions_json({main_sid: {"pid": alive_proc.pid, "lastSeenAt": iso_ago(0.1)}})
        self.backdate_events_and_runs(0.5, [Path(r_ro["dir"]), Path(r_wr["dir"]), Path(r_q["dir"])])
        self.assertIsNone(self.run_stop_hook(repo, chat_env, session_id=chat_sid))

    # ------------------------------------------------------------------
    # (c) Dead original caller + unclaimed past timeout -> takeover block!
    #     Verifies both configurable DELEGATE_ORPHAN_TAKEOVER_HOURS and 6h default.
    # ------------------------------------------------------------------
    def test_c_expired_caller_orphan_takeover_and_configurable_timeout(self):
        repo = self.repo({"a.txt": "v0\n"})
        dead_sid = f"sess-dead-c-{os.getpid()}"
        chat_sid = f"sess-chat-c-{os.getpid()}"
        dead_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": dead_sid})
        chat_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": chat_sid})

        self.fake_pi([fixtures.answer("orphan ro"), fixtures.SETTLED])
        r_ro = self.outcome(
            self.run_delegate(
                ["start", "--json", "--read-only", "--workdir", str(repo), "--name", "orphan-ro", "check"],
                repo,
                dead_env,
            )
        )
        self.until(lambda: (Path(r_ro["dir"]) / "exit_code").exists(), "orphan-ro finished")

        self.fake_pi([fixtures.answer("orphan edit"), fixtures.SETTLED], pre="echo v2 > a.txt")
        r_wr = self.outcome(
            self.run_delegate(
                ["start", "--json", "--worktree", "--workdir", str(repo), "--name", "orphan-wr", "--accept", "true", "edit"],
                repo,
                dead_env,
            )
        )
        self.until(lambda: (Path(r_wr["dir"]) / "exit_code").exists(), "orphan-wr finished")

        # Wait until finish_run has also flushed events.jsonl and index.json for both runs before backdating.
        events_file = self.state_dir() / "events.jsonl"
        index_file = self.state_dir() / "index.json"
        def _both_flushed():
            if not (events_file.is_file() and index_file.is_file()):
                return False
            try:
                runs_map = json.loads(index_file.read_text(encoding="utf-8")).get("runs", {})
            except ValueError:
                return False
            by_run = {
                (e.get("status") or {}).get("run"): (e.get("status") or {}).get("state")
                for e in runs_map.values()
                if isinstance(e, dict)
            }
            return by_run.get(r_ro["run"]) == "answered" and by_run.get(r_wr["run"]) == "delivered"
        self.until(_both_flushed, "both runs flushed to events.jsonl and index.json")

        self.write_sessions_json({dead_sid: {"pid": self.get_dead_pid(), "lastSeenAt": iso_ago(3.5)}})
        self.backdate_events_and_runs(3.0, [Path(r_ro["dir"]), Path(r_wr["dir"])])

        # 3h < default 6h -> NOT yet eligible for takeover.
        self.assertIsNone(self.run_stop_hook(repo, chat_env, session_id=chat_sid))

        # Configurable timeout = 2h (< 3h) -> takeover reminder APPEARS (single item + 另有 1 件)!
        blk_cfg = self.run_stop_hook(
            repo,
            chat_env,
            session_id=chat_sid,
            extra_env={"DELEGATE_ORPHAN_TAKEOVER_HOURS": "2"},
        )
        self.assertIsNotNone(blk_cfg)
        self.assertEqual(blk_cfg["decision"], "block")
        self.assertIn("接盘", blk_cfg["reason"])
        self.assertIn("delegate card", blk_cfg["reason"])
        self.assertIn("orphan-ro", blk_cfg["reason"])
        self.assertIn("另有 1 件：`delegate hook pending`", blk_cfg["reason"])

        # Backdate to 7h (> default 6h) with NO env override -> default 6h triggers takeover!
        self.backdate_events_and_runs(7.0, [Path(r_ro["dir"]), Path(r_wr["dir"])])
        blk_def = self.run_stop_hook(repo, chat_env, session_id=chat_sid)
        self.assertIsNotNone(blk_def)
        self.assertIn("接盘", blk_def["reason"])
        self.assertIn("orphan-ro", blk_def["reason"])
        self.assertIn("另有 1 件：`delegate hook pending`", blk_def["reason"])

        # Reading orphan-ro clears it and surfaces orphan-wr; applying orphan-wr clears everything!
        self.assertEqual(self.run_delegate(["result", r_ro["run"]], repo, chat_env).returncode, 0)
        blk_wr = self.run_stop_hook(repo, chat_env, session_id=chat_sid)
        self.assertIsNotNone(blk_wr)
        self.assertIn("orphan-wr", blk_wr["reason"])
        self.assertEqual(self.run_delegate(["apply", r_wr["run"]], repo, chat_env).returncode, 0)
        self.assertIsNone(self.run_stop_hook(repo, chat_env, session_id=chat_sid))

    # ------------------------------------------------------------------
    # (d) Own unread delivery blocks immediately; per-caller cursors stay isolated.
    # ------------------------------------------------------------------
    def test_d_own_unread_delivery_blocks_immediately_and_cursors_stay_isolated(self):
        repo = self.repo({"a.txt": "v0\n"})
        own_sid = "sess-own-d"
        bystander_sid = "sess-bystander-d"
        own_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": own_sid})
        bystander_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": bystander_sid})

        self.fake_pi([fixtures.answer("fresh own delivery"), fixtures.SETTLED])
        r_own = self.outcome(
            self.run_delegate(
                ["start", "--json", "--read-only", "--workdir", str(repo), "--name", "own-fresh", "check"],
                repo,
                own_env,
            )
        )
        self.until(lambda: (Path(r_own["dir"]) / "exit_code").exists(), "own-fresh finished")

        # Bystander reading `delegate events --since 0` advances ONLY bystander's cursor, NOT own_sid's!
        ev_by = self.run_delegate(["events", "--since", "0"], repo, bystander_env)
        self.assertEqual(ev_by.returncode, 0, ev_by.stderr)
        cursors = json.loads((self.state_dir() / "cursors.json").read_text(encoding="utf-8"))
        self.assertGreater(cursors["callers"][bystander_sid]["seq"], 0)
        self.assertNotIn(own_sid, cursors["callers"])

        # Own session MUST still be blocked immediately!
        blk = self.run_stop_hook(repo, own_env, session_id=own_sid)
        self.assertIsNotNone(blk)
        self.assertEqual(blk["decision"], "block")
        self.assertIn(f"own-fresh 已交付待看。下一步：`delegate card {r_own['run']}`", blk["reason"])
        self.assertNotIn("接盘", blk["reason"])

        # Running `delegate events --since 0` as own_sid advances own_sid's cursor and clears block.
        self.assertEqual(self.run_delegate(["events", "--since", "0"], repo, own_env).returncode, 0)
        self.assertIsNone(self.run_stop_hook(repo, own_env, session_id=own_sid))

    # ------------------------------------------------------------------
    # (e) Stale work-item reminder scoped to responsible caller (`add`/`wait`/`start --work`),
    #     non-responsible session neither blocked nor consuming cooldown; cooldown per (item x session).
    # ------------------------------------------------------------------
    def test_e_work_item_stale_reminder_caller_scope_and_per_session_cooldown(self):
        repo = self.repo({"a.txt": "v0\n"})
        brief = repo / "docs/spec.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# Spec\n")

        owner_sid = "sess-owner-e"
        handover_sid = "sess-handover-e"
        chat_sid = "sess-chat-e"
        third_sid = "sess-third-e"
        gone_sid = f"sess-gone-e-{os.getpid()}"

        owner_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": owner_sid})
        handover_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": handover_sid})
        chat_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": chat_sid})
        third_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": third_sid})

        # 1) Verify `work add`, `work wait`, and `start --work` record/update `caller`!
        res_add = self.run_delegate(
            ["work", "add", "主控负责项", "--repo", str(repo), "--brief", str(brief), "--json"],
            repo,
            owner_env,
        )
        self.assertEqual(res_add.returncode, 0, res_add.stderr)
        w1 = json.loads(res_add.stdout)
        self.assertEqual(w1["id"], "W001")
        self.assertEqual(w1.get("caller"), owner_sid)

        # `work wait` updates responsible caller to handover_sid.
        res_wait = self.run_delegate(
            ["work", "wait", "W001", "--brief", str(brief), "--json"],
            repo,
            handover_env,
        )
        self.assertEqual(res_wait.returncode, 0, res_wait.stderr)
        self.assertEqual(json.loads(res_wait.stdout).get("caller"), handover_sid)

        # `start --work W001` updates responsible caller back to owner_sid!
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        r_w = self.outcome(
            self.run_delegate(
                ["run", "--json", "--read-only", "--workdir", str(repo), "--work", "W001", "--name", "w1-run", "check"],
                repo,
                owner_env,
            )
        )
        self.assertEqual(self.run_delegate(["result", r_w["run"]], repo, owner_env).returncode, 0)
        w1_after_start = json.loads(
            self.run_delegate(["work", "show", "W001", "--json"], repo, owner_env).stdout
        )
        self.assertEqual(w1_after_start.get("caller"), owner_sid)

        # Consume events so only low-priority work-hygiene reminder is evaluated.
        self.run_delegate(["events", "--since", "0"], repo, owner_env)
        self.run_delegate(["events", "--since", "0"], repo, chat_env)
        self.run_delegate(["events", "--since", "0"], repo, third_env)

        # Reset work.jsonl with W001 (owned by living owner_sid, stale 10h),
        # W002 (owned by dead gone_sid, stale 10h), W003 (no caller recorded, stale 9h).
        self.spawn_alive_caller(owner_sid)
        self.write_sessions_json({gone_sid: {"pid": self.get_dead_pid(), "lastSeenAt": iso_ago(10)}})
        (self.state_dir() / "work.jsonl").write_text(
            "".join(
                json.dumps(it, ensure_ascii=False) + "\n"
                for it in [
                    {
                        "id": "W001",
                        "title": "主控负责项",
                        "repo": str(repo),
                        "state": "ready",
                        "brief": str(brief),
                        "caller": owner_sid,
                        "stateSince": iso_ago(10.2),
                        "createdAt": iso_ago(10.2),
                        "updatedAt": iso_ago(10.2),
                    }
                ]
            ),
            encoding="utf-8",
        )

        # 2) Non-responsible `chat_sid` MUST NOT be blocked by W001, and MUST NOT touch work-remind.json!
        self.assertIsNone(self.run_stop_hook(repo, chat_env, session_id=chat_sid))
        self.assertFalse((self.state_dir() / "work-remind.json").exists())

        # 3) Immediately afterwards, responsible `owner_sid` IS blocked (cooldown not consumed by chat_sid!).
        blk_owner = self.run_stop_hook(repo, owner_env, session_id=owner_sid)
        self.assertIsNotNone(blk_owner)
        self.assertIn("[W001] 主控负责项", blk_owner["reason"])
        self.assertNotIn("待接手", blk_owner["reason"])
        remind_doc = json.loads((self.state_dir() / "work-remind.json").read_text(encoding="utf-8"))
        self.assertIn(owner_sid, remind_doc["W001"])
        self.assertNotIn(chat_sid, remind_doc["W001"])
        # owner_sid is now in 6h cooldown.
        self.assertIsNone(self.run_stop_hook(repo, owner_env, session_id=owner_sid))

        # 4) Now add W002 (dead caller, expired 10h) and W003 (no caller, stale 9h).
        #    chat_sid takes over W002 & W003; cooldown is keyed per (item x session) so third_sid still sees them!
        (self.state_dir() / "work.jsonl").write_text(
            "".join(
                json.dumps(it, ensure_ascii=False) + "\n"
                for it in [
                    {
                        "id": "W001",
                        "title": "主控负责项",
                        "repo": str(repo),
                        "state": "ready",
                        "brief": str(brief),
                        "caller": owner_sid,
                        "stateSince": iso_ago(10.2),
                        "createdAt": iso_ago(10.2),
                        "updatedAt": iso_ago(10.2),
                    },
                    {
                        "id": "W002",
                        "title": "失联会话项",
                        "repo": str(repo),
                        "state": "ready",
                        "brief": str(brief),
                        "caller": gone_sid,
                        "stateSince": iso_ago(10.2),
                        "createdAt": iso_ago(10.2),
                        "updatedAt": iso_ago(10.2),
                    },
                    {
                        "id": "W003",
                        "title": "无负责会话项",
                        "repo": str(repo),
                        "state": "ready",
                        "brief": str(brief),
                        "stateSince": iso_ago(9.2),
                        "createdAt": iso_ago(9.2),
                        "updatedAt": iso_ago(9.2),
                    },
                ]
            ),
            encoding="utf-8",
        )

        # Progressive single-item: chat_sid sees W003 first (ownerless), then W002 (orphan);
        # only the displayed item is recorded in work-remind.json per call!
        blk_chat1 = self.run_stop_hook(repo, chat_env, session_id=chat_sid)
        self.assertIsNotNone(blk_chat1)
        self.assertNotIn("W001", blk_chat1["reason"])
        self.assertIn("[W003] 无负责会话项", blk_chat1["reason"])
        self.assertIn("另有 1 件：`delegate hook pending`", blk_chat1["reason"])

        blk_chat2 = self.run_stop_hook(repo, chat_env, session_id=chat_sid)
        self.assertIsNotNone(blk_chat2)
        self.assertIn("[W002] 失联会话项", blk_chat2["reason"])
        self.assertNotIn("另有", blk_chat2["reason"])

        # chat_sid is now in cooldown for W002 & W003...
        self.assertIsNone(self.run_stop_hook(repo, chat_env, session_id=chat_sid))
        # ...but third_sid has its OWN independent cooldown per (item x session) and STILL gets reminded!
        blk_third1 = self.run_stop_hook(repo, third_env, session_id=third_sid)
        self.assertIsNotNone(blk_third1)
        self.assertIn("W003", blk_third1["reason"])
        self.assertNotIn("W001", blk_third1["reason"])
        blk_third2 = self.run_stop_hook(repo, third_env, session_id=third_sid)
        self.assertIsNotNone(blk_third2)
        self.assertIn("W002", blk_third2["reason"])
        self.assertNotIn("W001", blk_third2["reason"])

        remind_doc2 = json.loads((self.state_dir() / "work-remind.json").read_text(encoding="utf-8"))
        self.assertEqual(set(remind_doc2["W002"].keys()), {chat_sid, third_sid})
        self.assertEqual(set(remind_doc2["W003"].keys()), {chat_sid, third_sid})


if __name__ == "__main__":
    unittest.main()
