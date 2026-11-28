"""Tests for the single-source controller host table, `delegate hosts`, `delegate install-hooks`,
Cursor/Kilo silent allow (Stop & PreToolUse), Codex & Pi controller support, stranger-session
Stop-hook isolation vs delegate-using 30m stall-chain penetration, and Pi colleague `--no-skills`.
"""
import importlib.util
import json
import os
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures

ROOT = Path(__file__).resolve().parent.parent
STOP_HOOK = ROOT / "skills/delegate/hooks/claude-code-stop.py"
BACKGROUND_HOOK = ROOT / "skills/delegate/hooks/claude-code-background.py"


def iso_ago_mins(mins: int) -> str:
    ts = int(time.time() - mins * 60)
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts))


class DelegateHostsTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def clean_env(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        strip = set(fixtures.CALLER_ENV) | {
            "ANTIGRAVITY_CONVERSATION_ID",
            "ANTIGRAVITY_AGENT",
            "PI_DELEGATE_ACTIVE",
            "DELEGATE_AGENT",
            "DELEGATE_RUN_DIR",
            "CURSOR_VERSION",
            "CURSOR_PROJECT_DIR",
            "CURSOR_TRACE_ID",
            "KILO_SESSION_ID",
            "KILOCODE_VERSION",
            "KILO_VERSION",
            "CODEX_VERSION",
        }
        env = {k: v for k, v in self.env.items() if k not in strip}
        if extra:
            env.update(extra)
        return env

    def run_delegate(self, args: list[str], cwd: Path, env: dict[str, str], stdin: str | None = None):
        return subprocess.run(
            [str(fixtures.DELEGATE), *args],
            cwd=cwd,
            input=stdin,
            capture_output=True,
            text=True,
            env=env,
            timeout=30,
        )

    def test_hosts_command_and_python_shield_conformance(self):
        res_human = self.run_delegate(["hosts"], self.work, self.clean_env())
        self.assertEqual(res_human.returncode, 0, res_human.stderr)
        for host_id in ("claude", "agy", "codex", "pi", "cursor", "kilo"):
            self.assertIn(host_id, res_human.stdout)

        res_json = self.run_delegate(["hosts", "--json"], self.work, self.clean_env())
        self.assertEqual(res_json.returncode, 0, res_json.stderr)
        table = json.loads(res_json.stdout)
        ctrl_ids = [c["id"] for c in table["controllers"]]
        self.assertEqual(ctrl_ids, ["claude", "agy", "codex", "pi"])
        ignored_ids = [i["id"] for i in table["ignored"]]
        self.assertEqual(ignored_ids, ["cursor", "kilo"])

        by_id = {c["id"]: c for c in table["controllers"]}
        self.assertEqual(
            by_id["claude"]["capabilities"],
            {"hasStopHook": True, "hasPreToolHook": True, "bgWakesController": True, "checkIn50m": True},
        )
        self.assertEqual(
            by_id["agy"]["capabilities"],
            {"hasStopHook": True, "hasPreToolHook": True, "bgWakesController": True, "checkIn50m": True},
        )
        self.assertEqual(
            by_id["codex"]["capabilities"],
            {"hasStopHook": True, "hasPreToolHook": True, "bgWakesController": False, "checkIn50m": False},
        )
        self.assertEqual(
            by_id["pi"]["capabilities"],
            {"hasStopHook": False, "hasPreToolHook": False, "bgWakesController": False, "checkIn50m": False},
        )

        # Conformance: Python PreToolUse shield mirrors the single-source host table.
        spec = importlib.util.spec_from_file_location("bg_hook", BACKGROUND_HOOK)
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        expected_ignored_env = {k for h in table["ignored"] for k in h["detect"]["env"]}
        expected_ignored_payload = {k for h in table["ignored"] for k in h["detect"]["payload"]}
        self.assertEqual(set(mod.IGNORED_HOST_ENV), expected_ignored_env)
        self.assertEqual(set(mod.IGNORED_HOST_PAYLOAD), expected_ignored_payload)
        for hid in ("claude", "agy", "codex"):
            self.assertEqual(
                mod.HOST_PROFILES[hid]["bg_wakes_controller"],
                by_id[hid]["capabilities"]["bgWakesController"],
                hid,
            )
            self.assertEqual(
                mod.HOST_PROFILES[hid]["tool_name"],
                by_id[hid]["preToolProtocol"]["toolName"],
                hid,
            )

    def test_install_hooks_command_for_all_hosts_and_cursor_rejection(self):
        # Codex install, idempotency, backup, uninstall
        codex_hooks = self.work / "codex-hooks.json"
        orig = json.dumps({"hooks": {"PostToolUse": [{"hooks": [{"type": "command", "command": "echo keep"}]}]}}, indent=2) + "\n"
        codex_hooks.write_text(orig, encoding="utf-8")

        r1 = self.run_delegate(["install-hooks", "--host", "codex", "--path", str(codex_hooks)], self.work, self.clean_env())
        self.assertEqual(r1.returncode, 0, r1.stderr)
        bak = codex_hooks.with_name(codex_hooks.name + ".bak")
        self.assertTrue(bak.is_file())
        self.assertEqual(bak.read_text(encoding="utf-8"), orig)
        doc = json.loads(codex_hooks.read_text(encoding="utf-8"))
        self.assertIn("--format codex", doc["hooks"]["Stop"][0]["hooks"][0]["command"])
        self.assertEqual(doc["hooks"]["PreToolUse"][0]["matcher"], "Bash")
        self.assertIn("--format codex", doc["hooks"]["PreToolUse"][0]["hooks"][0]["command"])
        self.assertEqual(doc["hooks"]["PostToolUse"][0]["hooks"][0]["command"], "echo keep")

        # Claude install strips any temporary CURSOR_VERSION wrapper and normalizes command
        claude_cfg = self.work / "claude-settings.json"
        claude_cfg.write_text(json.dumps({
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": '[ -n "$CURSOR_VERSION" ] && exit 0; python3 "$HOME/.claude/skills/delegate/hooks/claude-code-stop.py"'}]}],
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": '[ -n "$CURSOR_VERSION" ] && exit 0; python3 "$HOME/.claude/skills/delegate/hooks/claude-code-background.py"'}]}],
            }
        }), encoding="utf-8")
        r_cl = self.run_delegate(["install-hooks", "--host", "claude", "--path", str(claude_cfg)], self.work, self.clean_env())
        self.assertEqual(r_cl.returncode, 0, r_cl.stderr)
        cl_doc = json.loads(claude_cfg.read_text(encoding="utf-8"))
        self.assertEqual(len(cl_doc["hooks"]["Stop"]), 1)
        self.assertNotIn("CURSOR_VERSION", cl_doc["hooks"]["Stop"][0]["hooks"][0]["command"])
        self.assertEqual(len(cl_doc["hooks"]["PreToolUse"]), 1)
        self.assertNotIn("CURSOR_VERSION", cl_doc["hooks"]["PreToolUse"][0]["hooks"][0]["command"])

        # Pi is a clean no-op
        r_pi = self.run_delegate(["install-hooks", "--host", "pi"], self.work, self.clean_env())
        self.assertEqual(r_pi.returncode, 0, r_pi.stderr)
        self.assertIn("skill-only", r_pi.stdout)

        # Cursor is explicitly rejected as skill-only
        r_cur = self.run_delegate(["install-hooks", "--host", "cursor"], self.work, self.clean_env())
        self.assertNotEqual(r_cur.returncode, 0)
        self.assertIn("skill-only", r_cur.stderr)

    def test_stop_hook_rendering_for_every_host_and_cursor_silent_allow(self):
        repo = self.repo({"a.txt": "v0\n"})
        owner_sid = "sess-render-owner"
        owner_env = self.clean_env({"CLAUDE_CODE_SESSION_ID": owner_sid})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        r1 = self.outcome(
            self.run_delegate(
                ["start", "--json", "--read-only", "--workdir", str(repo), "--name", "render-job", "check"],
                repo,
                owner_env,
            )
        )
        self.until(lambda: (Path(r1["dir"]) / "exit_code").exists(), "render-job finished")

        hook_base = {
            **self.clean_env(),
            "DELEGATE_BIN": str(fixtures.DELEGATE),
            "DELEGATE_STOP_HOOK_WAITERS": "",
            "DELEGATE_STOP_HOOK_PROCS": "",
            "DELEGATE_STOP_HOOK_CURRENT_ONLY": "1",
        }

        # 1) Claude block
        res_c = self.run_delegate(
            ["hook", "check", "--format", "claude"],
            repo,
            hook_base,
            stdin=json.dumps({"cwd": str(repo), "session_id": owner_sid, "stop_hook_active": False}),
        )
        self.assertEqual(res_c.returncode, 0)
        out_c = json.loads(res_c.stdout)
        self.assertEqual(out_c["decision"], "block")
        self.assertIn("render-job", out_c["reason"])

        # 2) Agy block
        res_a = self.run_delegate(
            ["hook", "check", "--format", "agy"],
            repo,
            hook_base,
            stdin=json.dumps({"cwd": str(repo), "conversationId": owner_sid, "continuationActive": False, "terminationReason": "NO_TOOL_CALL"}),
        )
        self.assertEqual(res_a.returncode, 0)
        out_a = json.loads(res_a.stdout)
        self.assertEqual(out_a["decision"], "continue")
        self.assertIn("render-job", out_a["reason"])

        # 3) Codex block
        res_x = self.run_delegate(
            ["hook", "check", "--format", "codex"],
            repo,
            hook_base,
            stdin=json.dumps({"cwd": str(repo), "session_id": owner_sid, "turn_id": "turn-1", "stop_hook_active": False}),
        )
        self.assertEqual(res_x.returncode, 0)
        out_x = json.loads(res_x.stdout)
        self.assertEqual(out_x["decision"], "block")
        self.assertIn("render-job", out_x["reason"])

        # 4) Cursor env-only -> EMPTY allow even through claude-code-stop.py!
        for env_extra in ({"CURSOR_VERSION": "0.50.0"}, {"CURSOR_PROJECT_DIR": str(repo)}):
            res_cur_env = subprocess.run(
                [sys.executable, str(STOP_HOOK)],
                input=json.dumps({"cwd": str(repo), "session_id": owner_sid, "stop_hook_active": False}),
                capture_output=True,
                text=True,
                env={**hook_base, **env_extra},
                timeout=15,
            )
            self.assertEqual(res_cur_env.returncode, 0, res_cur_env.stderr)
            self.assertEqual(res_cur_env.stdout.strip(), "", f"Cursor env {env_extra} must allow empty")

        # 5) Cursor payload-only (clean env) -> EMPTY allow!
        for payload in (
            {"cwd": str(repo), "cursor_version": "0.50.0", "session_id": owner_sid},
            {"cwd": str(repo), "workspace_roots": [str(repo)], "session_id": owner_sid},
            {"cwd": str(repo), "conversation_id": owner_sid},
        ):
            res_cur_pl = subprocess.run(
                [sys.executable, str(STOP_HOOK)],
                input=json.dumps(payload),
                capture_output=True,
                text=True,
                env=hook_base,
                timeout=15,
            )
            self.assertEqual(res_cur_pl.returncode, 0, res_cur_pl.stderr)
            self.assertEqual(res_cur_pl.stdout.strip(), "", f"Cursor payload {payload} must allow empty")

        # 6) Allow rendering after reading result: claude/codex empty, agy `{}`
        self.assertEqual(self.run_delegate(["result", r1["run"]], repo, owner_env).returncode, 0)
        self.assertEqual(
            self.run_delegate(
                ["hook", "check", "--format", "claude"],
                repo,
                hook_base,
                stdin=json.dumps({"cwd": str(repo), "session_id": owner_sid, "stop_hook_active": False}),
            ).stdout.strip(),
            "",
        )
        self.assertEqual(
            self.run_delegate(
                ["hook", "check", "--format", "codex"],
                repo,
                hook_base,
                stdin=json.dumps({"cwd": str(repo), "session_id": owner_sid, "turn_id": "t2", "stop_hook_active": False}),
            ).stdout.strip(),
            "",
        )
        self.assertEqual(
            json.loads(
                self.run_delegate(
                    ["hook", "check", "--format", "agy"],
                    repo,
                    hook_base,
                    stdin=json.dumps({"cwd": str(repo), "conversationId": owner_sid, "continuationActive": False}),
                ).stdout
            ),
            {},
        )

    def test_pre_tool_shield_codex_and_cursor_silent_allow(self):
        repo = self.repo({"a.txt": "v0\n"})
        (repo / ".local/run/delegate").mkdir(parents=True, exist_ok=True)
        base_env = self.clean_env()

        def run_shield(payload: dict, cli_args: list[str] | None = None, extra_env: dict[str, str] | None = None):
            env = {**base_env, **(extra_env or {})}
            res = subprocess.run(
                [sys.executable, str(BACKGROUND_HOOK), *(cli_args or [])],
                input=json.dumps(payload),
                cwd=str(repo),
                env=env,
                capture_output=True,
                text=True,
                timeout=15,
            )
            self.assertEqual(res.returncode, 0, res.stderr)
            return res.stdout.strip()

        # Codex allows foreground `delegate wait` (no background auto-wakeup)
        codex_wait = {
            "cwd": str(repo),
            "turn_id": "t-1",
            "tool_name": "Bash",
            "tool_input": {"command": "/skills/delegate/bin/delegate wait --any --max 5m"},
        }
        self.assertEqual(run_shield(codex_wait, ["--format", "codex"]), "")
        self.assertEqual(run_shield(codex_wait), "")  # auto-detected via turn_id

        # Codex STILL blocks dangerous commands (pgrep -f, git prune, timeout build, poll loop)
        codex_prune = {
            "cwd": str(repo),
            "turn_id": "t-1",
            "tool_name": "Bash",
            "tool_input": {"command": "git prune"},
        }
        out_prune = json.loads(run_shield(codex_prune, ["--format", "codex"]))
        self.assertEqual(out_prune["hookSpecificOutput"]["permissionDecision"], "deny")
        self.assertIn("refs/delegate/", out_prune["hookSpecificOutput"]["permissionDecisionReason"])

        codex_poll = {
            "cwd": str(repo),
            "turn_id": "t-1",
            "tool_name": "Bash",
            "tool_input": {"command": "until [ -f .local/run/delegate/r1/exit_code ]; do sleep 5; done"},
        }
        out_poll = json.loads(run_shield(codex_poll, ["--format", "codex"]))
        self.assertEqual(out_poll["hookSpecificOutput"]["permissionDecision"], "deny")
        self.assertIn("--max 5m", out_poll["hookSpecificOutput"]["permissionDecisionReason"])
        self.assertNotIn("run_in_background", out_poll["hookSpecificOutput"]["permissionDecisionReason"])

        # Cursor (env-only or payload-only) ALWAYS silently allowed even for git prune / delegate wait
        claude_wait = {
            "cwd": str(repo),
            "tool_name": "Bash",
            "tool_input": {"command": "git prune && /skills/delegate/bin/delegate wait"},
        }
        self.assertEqual(run_shield(claude_wait, extra_env={"CURSOR_VERSION": "0.50.0"}), "")
        self.assertEqual(run_shield({**claude_wait, "cursor_version": "0.50.0"}), "")
        self.assertEqual(run_shield({**claude_wait, "workspace_roots": [str(repo)]}), "")
        self.assertEqual(run_shield({**claude_wait, "kilo_version": "1.0"}), "")

    def test_stranger_session_not_blocked_while_delegate_user_receives_30m_penetration(self):
        repo = self.repo({"a.txt": "v0\n"})
        other_sid = f"sess-other-stalled-{os.getpid()}"
        stranger_sid = f"sess-stranger-{os.getpid()}"
        active_user_sid = f"sess-used-delegate-{os.getpid()}"

        # 1) Create an active running job + a 35m delivered unmerged write run blocking downstream W001, owned by other_sid.
        other_env = self.clean_env({"CLAUDE_CODE_SESSION_ID": other_sid})
        self.fake_pi([fixtures.answer("edited"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r_wr = self.outcome(
            self.run_delegate(
                ["start", "--json", "--worktree", "--workdir", str(repo), "--name", "stalled-write", "--accept", "true", "edit"],
                repo,
                other_env,
            )
        )
        self.until(lambda: (Path(r_wr["dir"]) / "exit_code").exists(), "stalled-write finished")

        # Backdate r_wr to 35 minutes ago so 30m stall-chain penetration triggers.
        old_iso = iso_ago_mins(35)
        for fname, field in (("meta.json", "startedAt"), ("summary.json", "finishedAt")):
            p = Path(r_wr["dir"]) / fname
            if p.is_file():
                d = json.loads(p.read_text(encoding="utf-8"))
                d[field] = old_iso
                p.write_text(json.dumps(d) + "\n", encoding="utf-8")
        idx_p = self.state_dir() / "index.json"
        if idx_p.is_file():
            idx = json.loads(idx_p.read_text(encoding="utf-8"))
            for entry in (idx.get("runs") or {}).values():
                if isinstance(entry, dict) and isinstance(entry.get("status"), dict):
                    entry["status"]["finishedAt"] = old_iso
            idx_p.write_text(json.dumps(idx) + "\n", encoding="utf-8")

        # Downstream blocked work item waiting on r_wr
        (self.state_dir() / "work.jsonl").write_text(
            json.dumps({
                "id": "W901",
                "title": "下游受阻项",
                "repo": str(repo.resolve()),
                "state": "blocked",
                "waitOn": [f"run_applied:{r_wr['run']}"],
                "stateSince": old_iso,
                "createdAt": old_iso,
                "updatedAt": old_iso,
            }, ensure_ascii=False) + "\n",
            encoding="utf-8",
        )

        # Also spawn a live running job owned by other_sid in the same repo.
        gate = self.work / "stranger-gate"
        ready = self.work / "stranger-ready"
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], gate=gate, ready_file=ready)
        r_live = self.outcome(
            self.run_delegate(
                ["start", "--json", "--read-only", "--workdir", str(repo), "--name", "other-live", "work"],
                repo,
                other_env,
            )
        )
        self.addCleanup(lambda: (gate.touch(), self.run_delegate(["stop", r_live["run"]], repo, other_env)))
        self.until(ready.is_file, "other-live entered gate")

        hook_env = {
            **self.clean_env(),
            "DELEGATE_BIN": str(fixtures.DELEGATE),
            "DELEGATE_STOP_HOOK_WAITERS": "",
            "DELEGATE_STOP_HOOK_PROCS": "",
            "DELEGATE_STOP_HOOK_CURRENT_ONLY": "0",
        }

        # 2) Stranger session (NEVER ran delegate in this repo) stops -> MUST NOT be blocked,
        #    and MUST NOT be added to sessions.json!
        res_stranger = subprocess.run(
            [sys.executable, str(STOP_HOOK)],
            input=json.dumps({"cwd": str(repo), "session_id": stranger_sid, "stop_hook_active": False}),
            capture_output=True,
            text=True,
            env=hook_env,
            timeout=15,
        )
        self.assertEqual(res_stranger.returncode, 0, res_stranger.stderr)
        self.assertEqual(res_stranger.stdout.strip(), "", f"stranger session must never be blocked: {res_stranger.stdout}")
        sessions_after = json.loads((self.state_dir() / "sessions.json").read_text(encoding="utf-8"))
        self.assertNotIn(stranger_sid, sessions_after, "hook check must never insert stranger session into sessions.json")

        # 3) Session that HAS used delegate in this repo (`delegate events` records it in sessions.json for repo)
        #    stops -> STILL receives the 30m stall-chain penetration block!
        active_env = self.clean_env({"CLAUDE_CODE_SESSION_ID": active_user_sid})
        ev_res = self.run_delegate(["events", "--since", "0"], repo, active_env)
        self.assertEqual(ev_res.returncode, 0, ev_res.stderr)
        sessions_used = json.loads((self.state_dir() / "sessions.json").read_text(encoding="utf-8"))
        self.assertIn(active_user_sid, sessions_used)
        self.assertIn(str(repo.resolve()), sessions_used[active_user_sid].get("repos", []))

        res_used = subprocess.run(
            [sys.executable, str(STOP_HOOK)],
            input=json.dumps({"cwd": str(repo), "session_id": active_user_sid, "stop_hook_active": False}),
            capture_output=True,
            text=True,
            env=hook_env,
            timeout=15,
        )
        self.assertEqual(res_used.returncode, 0, res_used.stderr)
        blk = json.loads(res_used.stdout)
        self.assertEqual(blk["decision"], "block")
        self.assertIn("stalled-write", blk["reason"])
        self.assertIn("W901", blk["reason"])

    def test_pi_colleague_launched_with_no_skills_and_codex_rpi_caller_recognition(self):
        repo = self.repo({"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])

        # Codex caller recognition (`CODEX_THREAD_ID`) & pi colleague receives `--no-skills`
        codex_env = self.clean_env({"CODEX_THREAD_ID": "codex-thread-123"})
        r_codex = self.outcome(
            self.run_delegate(["run", "--json", "--read-only", "--workdir", str(repo), "t-codex"], repo, codex_env)
        )
        meta_codex = json.loads((Path(r_codex["dir"]) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta_codex["caller"], "codex-thread-123")
        self.assertEqual(meta_codex["callerSource"], "CODEX_THREAD_ID")

        pi_log_lines = Path(self.env["PI_LOG"]).read_text(encoding="utf-8").splitlines()
        self.assertTrue(pi_log_lines)
        self.assertIn("--no-skills", pi_log_lines[-1])

        # rpi caller recognition (`RPI_SESSION_ID`)
        rpi_env = self.clean_env({"RPI_SESSION_ID": "rpi-sess-456"})
        r_rpi = self.outcome(
            self.run_delegate(["run", "--json", "--read-only", "--workdir", str(repo), "t-rpi"], repo, rpi_env)
        )
        meta_rpi = json.loads((Path(r_rpi["dir"]) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta_rpi["caller"], "rpi-sess-456")
        self.assertEqual(meta_rpi["callerSource"], "RPI_SESSION_ID")

    def _init_git_repo(self, path: Path) -> Path:
        path.mkdir(parents=True, exist_ok=True)
        subprocess.run(["git", "-C", str(path), "init", "-q"], check=True)
        (path / "a.txt").write_text("v0\n", encoding="utf-8")
        subprocess.run(["git", "-C", str(path), "add", "a.txt"], check=True)
        subprocess.run(
            ["git", "-C", str(path), "-c", "user.name=T", "-c", "user.email=t@e", "commit", "-q", "-m", "init"],
            check=True,
        )
        return path

    def _seed_stalled_run_in_repo(self, repo: Path, other_sid: str, wid: str) -> dict:
        other_env = self.clean_env({"CLAUDE_CODE_SESSION_ID": other_sid})
        other_env.pop("DELEGATE_RUNS", None)
        self.fake_pi([fixtures.answer("edited"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r_wr = self.outcome(
            self.run_delegate(
                ["start", "--json", "--worktree", "--workdir", str(repo), "--name", f"stalled-{wid}", "--accept", "true", "edit"],
                repo,
                other_env,
            )
        )
        self.until(lambda: (Path(r_wr["dir"]) / "exit_code").exists(), f"stalled-{wid} finished")
        old_iso = iso_ago_mins(35)
        for fname, field in (("meta.json", "startedAt"), ("summary.json", "finishedAt")):
            p = Path(r_wr["dir"]) / fname
            if p.is_file():
                d = json.loads(p.read_text(encoding="utf-8"))
                d[field] = old_iso
                p.write_text(json.dumps(d) + "\n", encoding="utf-8")
        idx_p = self.state_dir() / "index.json"
        if idx_p.is_file():
            idx = json.loads(idx_p.read_text(encoding="utf-8"))
            for entry in (idx.get("runs") or {}).values():
                if isinstance(entry, dict) and isinstance(entry.get("status"), dict):
                    entry["status"]["finishedAt"] = old_iso
            idx_p.write_text(json.dumps(idx) + "\n", encoding="utf-8")
        (self.state_dir() / "work.jsonl").write_text(
            json.dumps({
                "id": wid,
                "title": "下游受阻项",
                "repo": str(repo.resolve()),
                "state": "blocked",
                "waitOn": [f"run_applied:{r_wr['run']}"],
                "stateSince": old_iso,
                "createdAt": old_iso,
                "updatedAt": old_iso,
            }, ensure_ascii=False) + "\n",
            encoding="utf-8",
        )
        return r_wr

    def test_legacy_sessions_json_cwd_only_does_not_leak_into_new_repo(self):
        repo_a = self._init_git_repo(self.work / "legacy-repo-a")
        repo_b = self._init_git_repo(self.work / "legacy-repo-b")
        other_sid = f"sess-other-legacy-{os.getpid()}"
        legacy_sid = f"sess-legacy-{os.getpid()}"

        self._seed_stalled_run_in_repo(repo_b, other_sid, "W902")

        # Write a pre-upgrade legacy entry with ONLY `cwd` (no `repos` key) pointing at repo_a/sub.
        sub_a = repo_a / "sub"
        sub_a.mkdir(exist_ok=True)
        sessions_path = self.state_dir() / "sessions.json"
        sdoc = json.loads(sessions_path.read_text(encoding="utf-8")) if sessions_path.is_file() else {}
        sdoc[legacy_sid] = {"lastSeenAt": iso_ago_mins(2), "cwd": str(sub_a.resolve())}
        sessions_path.write_text(json.dumps(sdoc, indent=2) + "\n", encoding="utf-8")

        hook_env = {
            **self.clean_env(),
            "DELEGATE_STOP_HOOK_WAITERS": "",
            "DELEGATE_STOP_HOOK_PROCS": "",
            "DELEGATE_STOP_HOOK_CURRENT_ONLY": "0",
        }
        hook_env.pop("DELEGATE_RUNS", None)
        # Stopping in unrelated repo_b MUST NOT block legacy_sid and MUST migrate repo_a (not repo_b) into `repos`.
        res = self.run_delegate(
            ["hook", "check", "--format", "claude"],
            repo_b,
            hook_env,
            stdin=json.dumps({"cwd": str(repo_b), "session_id": legacy_sid, "stop_hook_active": False}),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout.strip(), "", f"legacy session from repo_a must not be blocked in repo_b: {res.stdout}")

        updated = json.loads(sessions_path.read_text(encoding="utf-8"))
        repos_list = updated[legacy_sid].get("repos", [])
        self.assertIn(str(repo_a.resolve()), repos_list)
        self.assertNotIn(str(repo_b.resolve()), repos_list)

    def test_non_git_ancestor_in_sessions_repos_does_not_penetrate_child_git_repo(self):
        parent = self.work / "non-git-home"
        parent.mkdir(parents=True, exist_ok=True)
        repo_a = self._init_git_repo(parent / "repo-a")
        repo_b = self._init_git_repo(parent / "repo-b")
        other_sid = f"sess-other-parent-{os.getpid()}"
        user_sid = f"sess-parent-user-{os.getpid()}"

        self._seed_stalled_run_in_repo(repo_b, other_sid, "W903")

        # 1) Launching `delegate start --workdir repo_a` from non-git `parent` must NOT record `parent` in `repos`.
        user_env = self.clean_env({"CLAUDE_CODE_SESSION_ID": user_sid})
        user_env.pop("DELEGATE_RUNS", None)
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
        r_a = self.outcome(
            self.run_delegate(
                ["run", "--json", "--read-only", "--workdir", str(repo_a), "check-a"],
                parent,
                user_env,
            )
        )
        self.assertEqual(r_a["state"], "answered")
        sessions_path = self.state_dir() / "sessions.json"
        sdoc = json.loads(sessions_path.read_text(encoding="utf-8"))
        self.assertIn(str(repo_a.resolve()), sdoc[user_sid].get("repos", []))
        self.assertNotIn(str(parent.resolve()), sdoc[user_sid].get("repos", []))

        # 2) Even if `repos` explicitly contains non-git ancestor `parent` (e.g. `delegate events` run in $HOME),
        #    stopping in unrelated child git repo `repo_b` MUST NOT penetrate!
        sdoc[user_sid]["repos"] = [str(parent.resolve())]
        sessions_path.write_text(json.dumps(sdoc, indent=2) + "\n", encoding="utf-8")

        hook_env = {
            **self.clean_env(),
            "DELEGATE_STOP_HOOK_WAITERS": "",
            "DELEGATE_STOP_HOOK_PROCS": "",
            "DELEGATE_STOP_HOOK_CURRENT_ONLY": "0",
        }
        hook_env.pop("DELEGATE_RUNS", None)
        res = self.run_delegate(
            ["hook", "check", "--format", "claude"],
            repo_b,
            hook_env,
            stdin=json.dumps({"cwd": str(repo_b), "session_id": user_sid, "stop_hook_active": False}),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(
            res.stdout.strip(),
            "",
            f"non-git ancestor dir in sessions.repos must not penetrate child git repo_b: {res.stdout}",
        )


if __name__ == "__main__":
    unittest.main()
