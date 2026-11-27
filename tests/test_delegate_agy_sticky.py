"""Tests: agy session stickiness (author & reviewer), fallback to active account only
when pinned slot is unavailable (deleted / invalid / quota exhausted), --fresh re-pinning,
and status/card/snapshot switch notices.
"""
import json
import os
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures
import test_delegate_638 as acct_fixtures


class DelegateAgyStickyTests(unittest.TestCase):
    cli = fixtures.DelegateTests.cli
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def setUp(self):
        fixtures.DelegateTests.setUp(self)
        self.fake_home = self.work / "home"
        self.fake_home.mkdir(mode=0o700, exist_ok=True)
        self.env["HOME"] = str(self.fake_home)
        self.env["XDG_CONFIG_HOME"] = str(self.fake_home / ".config")
        self.env.pop("DELEGATE_AGY_BIN", None)
        self.calls_log = self.work / "agy-calls.jsonl"

    def write_user_config(self, doc: dict) -> None:
        cfg_dir = Path(self.env["XDG_CONFIG_HOME"]) / "delegate"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "config.json").write_text(json.dumps(doc), encoding="utf-8")
        for k in ("DELEGATE_SCOUT_MODEL", "DELEGATE_REVIEW_MODEL", "DELEGATE_WRITE_MODEL"):
            self.env.pop(k, None)

    def _seed_three_accounts(self) -> None:
        """Seed alpha (active), beta, gamma slots via import-agy-swap."""
        vault_dir = self.fake_home / ".gemini/agy-swap"
        vault_dir.mkdir(parents=True, exist_ok=True)
        vault = {
            "account:alpha@example.test": acct_fixtures._b64_vault_entry(
                "alpha@example.test", "AT-ALPHA", "RT-ALPHA"
            ),
            "account:beta@example.test": acct_fixtures._b64_vault_entry(
                "beta@example.test", "AT-BETA", "RT-BETA"
            ),
            "account:gamma@example.test": acct_fixtures._b64_vault_entry(
                "gamma@example.test", "AT-GAMMA", "RT-GAMMA"
            ),
        }
        (vault_dir / "vault.json").write_text(json.dumps(vault), encoding="utf-8")
        (vault_dir / "accounts.json").write_text(
            json.dumps(
                {
                    "alpha@example.test": {"name": "Alpha"},
                    "beta@example.test": {"name": "Beta"},
                    "gamma@example.test": {"name": "Gamma"},
                }
            ),
            encoding="utf-8",
        )
        acct_fixtures._write_live_creds(
            self.fake_home, "alpha@example.test", "AT-ALPHA", "RT-ALPHA"
        )
        res = self.cli("agy", "account", "import-agy-swap", "--json")
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(json.loads(res.stdout)["active"], "alpha")

    def _install_scripted_agy(self, script_body: str) -> Path:
        bin_dir = self.work / "agy-bin"
        bin_dir.mkdir(exist_ok=True)
        agy = bin_dir / "agy"
        counter_file = self.work / "agy-counter.txt"
        if counter_file.exists():
            counter_file.unlink()
        if self.calls_log.exists():
            self.calls_log.unlink()
        agy.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, subprocess, sys\n"
            "from pathlib import Path\n"
            "argv = sys.argv[1:]\n"
            "if '--version' in argv or '--help' in argv:\n"
            "    print('1.6.0')\n"
            "    sys.exit(0)\n"
            "if not os.environ.get('DELEGATE_RUN_DIR'):\n"
            "    sys.exit(0)\n"
            f"counter = Path({str(counter_file)!r})\n"
            "call_idx = int(counter.read_text()) + 1 if counter.exists() else 1\n"
            "counter.write_text(str(call_idx))\n"
            "raw_in = sys.stdin.readline()\n"
            "msg = json.loads(raw_in.strip().splitlines()[-1]) if raw_in.strip() else {}\n"
            "content = msg.get('message', {}).get('content', '')\n"
            "conv_id = ''\n"
            "for i, a in enumerate(argv):\n"
            "    if a == '--conversation' and i + 1 < len(argv):\n"
            "        conv_id = argv[i + 1]\n"
            "tok_path = Path(os.environ['HOME']) / '.gemini/antigravity-cli/antigravity-oauth-token'\n"
            "tok_doc = json.loads(tok_path.read_text()) if tok_path.is_file() else {}\n"
            "rec = {\n"
            "    'call': call_idx,\n"
            "    'account': os.environ.get('DELEGATE_AGY_ACCOUNT', ''),\n"
            "    'email': tok_doc.get('id_token_email', ''),\n"
            "    'token': tok_doc.get('token', {}).get('access_token', ''),\n"
            "    'conv': conv_id,\n"
            "    'argv': argv,\n"
            "    'content': content,\n"
            "    'run_dir': os.environ.get('DELEGATE_RUN_DIR', ''),\n"
            "}\n"
            f"with open({str(self.calls_log)!r}, 'a', encoding='utf-8') as f:\n"
            "    f.write(json.dumps(rec) + '\\n')\n"
            "def emit_agy(text, cid=None):\n"
            "    c = conv_id or cid or f'conv-{call_idx}'\n"
            "    print(json.dumps({'event':'init','conversation_id':c}), flush=True)\n"
            "    print(json.dumps({'event':'step_update','step_update':{'step_index':1,'step_type':'agent_response','state':'DONE','text_delta':text}}), flush=True)\n"
            "    print(json.dumps({'event':'result','result':{'conversation_id':c,'status':'SUCCESS','response':text,'usage':{'input_tokens':100,'output_tokens':20,'cache_read_tokens':500}}}), flush=True)\n"
            + script_body
            + "\n",
            encoding="utf-8",
        )
        agy.chmod(0o755)
        self.env["PATH"] = f"{bin_dir}:{self.env.get('PATH', '')}"
        self.write_user_config(
            {
                "models": {"neon": "agy/gemini-4-neon"},
                "tiers": {"scout": "neon", "review": "neon", "write": "neon"},
            }
        )
        return agy

    def _read_calls(self) -> list[dict]:
        if not self.calls_log.exists():
            return []
        return [
            json.loads(line)
            for line in self.calls_log.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]

    def _run_raw(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(fixtures.DELEGATE), *args],
            cwd=self.work,
            env=self.env,
            capture_output=True,
            text=True,
            timeout=30,
        )

    def _parse_run_id(self, stdout: str) -> str:
        s = stdout.strip()
        if s.startswith("{"):
            return json.loads(s)["run"]
        return s.splitlines()[-1].strip()

    def _start_and_wait(self, repo: Path, prompt: str, *extra: str) -> tuple[str, Path]:
        res = self.cli("start", "--workdir", str(repo), *extra, prompt)
        self.assertEqual(res.returncode, 0, f"start failed:\n{res.stdout}\n{res.stderr}")
        run_id = self._parse_run_id(res.stdout)
        run_dir = Path(self.env["DELEGATE_RUNS"]) / run_id
        self.until(lambda: (run_dir / "summary.json").exists(), f"{run_id} finishes", timeout=20)
        return run_id, run_dir

    def _reply_and_wait(self, parent_id: str, prompt: str, *extra: str) -> tuple[str, Path]:
        res = self.cli("reply", parent_id, *extra, prompt)
        self.assertEqual(res.returncode, 0, f"reply failed:\n{res.stdout}\n{res.stderr}")
        run_id = self._parse_run_id(res.stdout)
        run_dir = Path(self.env["DELEGATE_RUNS"]) / run_id
        self.until(lambda: (run_dir / "summary.json").exists(), f"{run_id} finishes", timeout=20)
        return run_id, run_dir

    def test_reply_and_sync_stay_pinned_after_switch_and_fresh_uses_active(self):
        self._seed_three_accounts()
        repo = self.repo({"src/app.txt": "v0\n"})
        self._install_scripted_agy(
            "p = Path('src/app.txt')\n"
            "p.write_text(f'v{call_idx}\\n', encoding='utf-8')\n"
            "emit_agy(f'done round {call_idx}')\n"
        )

        # 1. Start on alpha (worktree mode so --sync can be tested on reply)
        run1, dir1 = self._start_and_wait(repo, "round 1", "--worktree")
        meta1 = json.loads((dir1 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta1.get("agyAccount"), "alpha")
        self.assertNotIn("alpha@example.test", json.dumps(meta1))

        # 2. Switch active account to beta; check human CLI note mentions existing sessions
        sw = self._run_raw("agy", "account", "use", "beta")
        self.assertEqual(sw.returncode, 0, sw.stderr)
        self.assertIn("existing sessions", sw.stdout)
        self.assertIn("new runs will use beta", sw.stdout)

        # 3. Normal reply stays pinned to alpha
        run2, dir2 = self._reply_and_wait(run1, "round 2 follow up")
        meta2 = json.loads((dir2 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta2.get("agyAccount"), "alpha")
        self.assertNotIn("agyAccountSwitch", meta2)

        # 4. --sync reply also stays pinned to alpha
        (repo / "README.md").write_text("mainline update\n", encoding="utf-8")
        subprocess.run(["git", "add", "README.md"], cwd=repo, check=True)
        subprocess.run(
            ["git", "commit", "-m", "mainline"],
            cwd=repo,
            check=True,
            capture_output=True,
        )
        run3, dir3 = self._reply_and_wait(run2, "round 3 sync", "--sync", "--minor")
        meta3 = json.loads((dir3 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta3.get("agyAccount"), "alpha")
        self.assertNotIn("agyAccountSwitch", meta3)

        # 5. --fresh reply starts a new session on active account beta
        run4, dir4 = self._reply_and_wait(run3, "round 4 fresh", "--fresh", "--minor")
        meta4 = json.loads((dir4 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta4.get("agyAccount"), "beta")
        self.assertNotIn("agyAccountSwitch", meta4)

        calls = self._read_calls()
        self.assertEqual([c["account"] for c in calls], ["alpha", "alpha", "alpha", "beta"])
        self.assertEqual([c["token"] for c in calls], ["AT-ALPHA", "AT-ALPHA", "AT-ALPHA", "AT-BETA"])
        self.assertNotIn("gamma", [c["account"] for c in calls])

    def test_fallback_on_quota_exhausted_and_deleted_slot_updates_meta_status_card_snapshot(self):
        self._seed_three_accounts()
        repo = self.repo({"src/app.txt": "v0\n"})
        self._install_scripted_agy(
            "Path('src/app.txt').write_text(f'v{call_idx}\\n', encoding='utf-8')\n"
            "emit_agy(f'done {call_idx}')\n"
        )

        # Start run1 on alpha, then switch active to beta
        run1, _ = self._start_and_wait(repo, "start on alpha")
        self.assertEqual(self.cli("agy", "account", "use", "beta").returncode, 0)

        # Mark alpha Gemini quota exhausted (future resetAt) in <state>/agy-quota.json:
        # strict stickiness MUST keep pinned reply on alpha (never silently switch accounts
        # while resuming the same conversation).
        quota_path = Path(self.env["XDG_STATE_HOME"]) / "delegate/agy-quota.json"
        quota_path.parent.mkdir(parents=True, exist_ok=True)
        quota_path.write_text(
            json.dumps(
                {
                    "schemaVersion": 1,
                    "updatedAt": "2026-11-26T16:00:00Z",
                    "updatedEpoch": 1795622400,
                    "accounts": {
                        "alpha": {
                            "ok": True,
                            "fetchedAt": "2026-11-26T16:00:00Z",
                            "fetchedEpoch": 1795622400,
                            "entries": [
                                {
                                    "pool": "Gemini Models",
                                    "window": "fiveHour",
                                    "windowLabel": "Five Hour Limit Remaining",
                                    "remainingPct": 0.0,
                                    "resetAt": "2099-01-01T00:00:00Z",
                                }
                            ],
                        }
                    },
                }
            ),
            encoding="utf-8",
        )

        run2, dir2 = self._reply_and_wait(run1, "reply after alpha exhausted stays sticky")
        meta2 = json.loads((dir2 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta2.get("agyAccount"), "alpha")
        self.assertNotIn("agyAccountSwitch", meta2)

        # Now test deleted pinned slot: clear quota, start on alpha, switch to beta, remove alpha
        quota_path.unlink()
        self.assertEqual(self.cli("agy", "account", "use", "alpha").returncode, 0)
        run3, _ = self._start_and_wait(repo, "start before delete")
        self.assertEqual(self.cli("agy", "account", "use", "beta").returncode, 0)
        self.assertEqual(self.cli("agy", "account", "remove", "alpha").returncode, 0)

        run4, dir4 = self._reply_and_wait(run3, "reply after alpha deleted")
        meta4 = json.loads((dir4 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta4.get("agyAccount"), "beta")
        self.assertEqual(meta4.get("agyAccountSwitch", {}).get("from"), "alpha")
        self.assertEqual(meta4.get("agyAccountSwitch", {}).get("to"), "beta")
        self.assertEqual(meta4.get("agyAccountSwitch", {}).get("reason"), "原槽位已删除")
        expected_del = "本 run 已从 alpha 换到 beta（原槽位已删除）"
        st = self._run_raw("status", run4)
        self.assertEqual(st.returncode, 0, st.stderr)
        self.assertIn(expected_del, st.stdout)
        card = self._run_raw("card", run4, "--no-mark")
        self.assertEqual(card.returncode, 0, card.stderr)
        self.assertIn(expected_del, card.stdout)

        snap = self.cli("snapshot", "--json", "--repo", str(repo))
        self.assertEqual(snap.returncode, 0, snap.stderr)
        snap_doc = json.loads(snap.stdout)
        runs_in_snap = [
            r for rp in snap_doc.get("repos", []) for r in rp.get("runs", []) if r.get("run") == run4
        ]
        self.assertEqual(len(runs_in_snap), 1)
        self.assertEqual(runs_in_snap[0].get("agyAccountSwitch", {}).get("from"), "alpha")
        self.assertEqual(runs_in_snap[0].get("agyAccountSwitch", {}).get("to"), "beta")

        calls = self._read_calls()
        self.assertEqual([c["account"] for c in calls], ["alpha", "alpha", "alpha", "beta"])
        self.assertNotIn("gamma", [c["account"] for c in calls])

    def test_reviewer_session_stays_pinned_across_rework_after_switch(self):
        self._seed_three_accounts()
        repo = self.repo(
            {
                "src/app.txt": "v0\n",
                ".delegate.json": json.dumps({"review": True}),
            }
        )
        delegate_bin = str(fixtures.DELEGATE)
        real_home = str(self.fake_home)
        real_xdg = str(self.fake_home / ".config")
        self._install_scripted_agy(
            f"delegate_bin = {delegate_bin!r}\n"
            f"real_home = {real_home!r}\n"
            f"real_xdg = {real_xdg!r}\n"
            "run_dir = os.environ.get('DELEGATE_RUN_DIR', '')\n"
            "is_rev = '/review/' in run_dir\n"
            "if not is_rev:\n"
            "    Path('src/app.txt').write_text(f'author-v{call_idx}\\n', encoding='utf-8')\n"
            "    emit_agy(f'author done {call_idx}', cid='conv-author')\n"
            "elif call_idx == 2:\n"
            "    # Reviewer round 1: switch active account to beta mid-run, then request REWORK\n"
            "    sw_env = dict(os.environ)\n"
            "    sw_env['HOME'] = real_home\n"
            "    sw_env['XDG_CONFIG_HOME'] = real_xdg\n"
            "    subprocess.run([delegate_bin, 'agy', 'account', 'use', 'beta'], env=sw_env, check=True)\n"
            "    emit_agy('REWORK\\nplease polish\\n\\n## 主控重点看\\n无', cid='conv-rev')\n"
            "else:\n"
            "    # Reviewer round 2 (after author rework): PASS\n"
            "    emit_agy('PASS\\nall good\\n\\n## 主控重点看\\n无', cid='conv-rev')\n"
        )

        run_id, run_dir = self._start_and_wait(
            repo, "implement with review", "--worktree", "--accept", "true"
        )
        meta = json.loads((run_dir / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta.get("agyAccount"), "alpha")
        self.assertEqual(meta.get("reviewAgyAccount"), "alpha")
        self.assertNotIn("agyAccountSwitch", meta)
        self.assertNotIn("reviewAgyAccountSwitch", meta)

        # Verify active account really is beta now
        st_acct = json.loads(self.cli("agy", "account", "status", "--json").stdout)
        self.assertEqual(st_acct.get("active"), "beta")

        # All 4 calls (author1, rev1, author-rework, rev2) stayed on alpha!
        calls = self._read_calls()
        self.assertEqual(len(calls), 4, f"unexpected calls: {calls}")
        self.assertEqual([c["account"] for c in calls], ["alpha", "alpha", "alpha", "alpha"])
        self.assertNotIn("gamma", [c["account"] for c in calls])

    def test_fallback_then_rework_after_second_switch_stays_on_fallback_slot(self):
        """Regression: when reply falls back alpha->beta on attempt 1 and active switches to
        gamma before rework, author rework must read disk meta (beta) and NOT jump to gamma.
        """
        self._seed_three_accounts()
        repo = self.repo(
            {
                "src/app.txt": "v0\n",
                ".delegate.json": json.dumps({"review": True}),
            }
        )
        delegate_bin = str(fixtures.DELEGATE)
        real_home = str(self.fake_home)
        real_xdg = str(self.fake_home / ".config")
        self._install_scripted_agy(
            f"delegate_bin = {delegate_bin!r}\n"
            f"real_home = {real_home!r}\n"
            f"real_xdg = {real_xdg!r}\n"
            "run_dir = os.environ.get('DELEGATE_RUN_DIR', '')\n"
            "is_rev = '/review/' in run_dir\n"
            "if not is_rev:\n"
            "    Path('src/app.txt').write_text(f'v{call_idx}\\n', encoding='utf-8')\n"
            "    emit_agy(f'author done {call_idx}', cid='conv-author')\n"
            "elif call_idx == 2:\n"
            "    emit_agy('PASS\\nround1 ok\\n\\n## 主控重点看\\n无', cid='conv-rev-1')\n"
            "elif call_idx == 4:\n"
            "    # Reply reviewer round 1: switch active to gamma, then request REWORK\n"
            "    sw_env = dict(os.environ)\n"
            "    sw_env['HOME'] = real_home\n"
            "    sw_env['XDG_CONFIG_HOME'] = real_xdg\n"
            "    subprocess.run([delegate_bin, 'agy', 'account', 'use', 'gamma'], env=sw_env, check=True)\n"
            "    emit_agy('REWORK\\nneeds rework\\n\\n## 主控重点看\\n无', cid='conv-rev-2')\n"
            "else:\n"
            "    emit_agy('PASS\\nrework ok\\n\\n## 主控重点看\\n无', cid='conv-rev-2')\n"
        )

        # 1. Start run1 on alpha (author + reviewer PASS)
        run1, _ = self._start_and_wait(repo, "initial work", "--worktree", "--accept", "true")

        # 2. Switch active to beta and delete alpha slot so reply falls back alpha->beta
        self.assertEqual(self.cli("agy", "account", "use", "beta").returncode, 0)
        self.assertEqual(self.cli("agy", "account", "remove", "alpha").returncode, 0)

        # 3. Reply: author1 falls back alpha->beta; rev1 switches active to gamma & asks REWORK;
        #    author rework & rev2 must BOTH stay on beta (never jump to gamma!).
        run2, dir2 = self._reply_and_wait(run1, "follow up with rework")
        meta2 = json.loads((dir2 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta2.get("agyAccount"), "beta")
        self.assertEqual(meta2.get("reviewAgyAccount"), "beta")
        self.assertEqual(meta2.get("agyAccountSwitch", {}).get("from"), "alpha")
        self.assertEqual(meta2.get("agyAccountSwitch", {}).get("to"), "beta")
        self.assertEqual(meta2.get("agyAccountSwitch", {}).get("reason"), "原槽位已删除")

        # Verify only ONE agy_account_switch event was logged on run2 (no duplicate switch on rework)
        events = [
            json.loads(line)
            for line in (dir2 / "events.jsonl").read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        switch_events = [ev for ev in events if ev.get("e") == "agy_account_switch"]
        self.assertEqual(len(switch_events), 1, f"expected 1 switch event, got: {switch_events}")
        self.assertEqual(switch_events[0]["from"], "alpha")
        self.assertEqual(switch_events[0]["to"], "beta")

        calls = self._read_calls()
        # run1: alpha(author), alpha(rev)
        # run2: beta(author1 fallback), beta(rev1), beta(author rework), beta(rev2)
        self.assertEqual(
            [c["account"] for c in calls],
            ["alpha", "alpha", "beta", "beta", "beta", "beta"],
            f"unexpected calls: {calls}",
        )
        self.assertNotIn("gamma", [c["account"] for c in calls])

    def test_reply_does_not_inherit_old_agy_agent_from_parent_meta(self):
        """Regression: non-fresh reply always takes agy.agent from current config,
        never inheriting an old agy.agent from parent meta, while keeping other agy keys."""
        self._seed_three_accounts()
        repo = self.repo({"a.txt": "base\n"})
        self._install_scripted_agy(
            "Path('a.txt').write_text(f'v{call_idx}\\n', encoding='utf-8')\n"
            "emit_agy(f'done {call_idx}', cid='conv-agent-1')\n"
        )
        # Start run1 with agy.agent="aia" and agy.autoSwitch configured.
        self.write_user_config(
            {
                "models": {"neon": "agy/gemini-4-neon"},
                "tiers": {"scout": "neon", "review": "neon", "write": "neon"},
                "agy": {"agent": "aia", "autoSwitch": {"weeklyMinPct": 25}},
            }
        )
        expected_warn = (
            "agy.agent=aia：agy 1.5.17 起自定义 agent 的对话按更高费率扣额度"
            "（上游实测约 4 倍），不需要时删掉这一项，规则写进 ~/.gemini/GEMINI.md。"
        )
        start_res = self.cli("start", "--workdir", str(repo), "--worktree", "--no-scout", "--accept", "true", "initial task")
        self.assertEqual(start_res.returncode, 0, start_res.stderr)
        self.assertIn(expected_warn, start_res.stderr)
        run1 = self._parse_run_id(start_res.stdout)
        dir1 = Path(self.env["DELEGATE_RUNS"]) / run1
        self.until(lambda: (dir1 / "summary.json").exists(), f"{run1} finishes", timeout=20)
        meta1 = json.loads((dir1 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta1.get("agy"), {"agent": "aia", "autoSwitch": {"weeklyMinPct": 25}})

        # Reply while agy.agent is still set: stderr prints warning.
        reply_warn = self.cli("reply", "--over-limit", "test", run1, "still has agent")
        self.assertEqual(reply_warn.returncode, 0, reply_warn.stderr)
        self.assertIn(expected_warn, reply_warn.stderr)
        run_mid = self._parse_run_id(reply_warn.stdout)
        dir_mid = Path(self.env["DELEGATE_RUNS"]) / run_mid
        self.until(lambda: (dir_mid / "summary.json").exists(), f"{run_mid} finishes", timeout=20)

        # Now remove agy.agent from current config (parent meta still has agent="aia").
        self.write_user_config(
            {
                "models": {"neon": "agy/gemini-4-neon"},
                "tiers": {"scout": "neon", "review": "neon", "write": "neon"},
            }
        )
        reply_clean = self.cli("reply", "--over-limit", "test", run_mid, "continue without agent")
        self.assertEqual(reply_clean.returncode, 0, reply_clean.stderr)
        self.assertNotIn("agy.agent=", reply_clean.stderr)
        run2 = self._parse_run_id(reply_clean.stdout)
        dir2 = Path(self.env["DELEGATE_RUNS"]) / run2
        self.until(lambda: (dir2 / "summary.json").exists(), f"{run2} finishes", timeout=20)

        meta2 = json.loads((dir2 / "meta.json").read_text(encoding="utf-8"))
        self.assertNotIn("agent", meta2.get("agy") or {})
        self.assertEqual(meta2.get("agy"), {"autoSwitch": {"weeklyMinPct": 25}})

        calls = self._read_calls()
        self.assertEqual(len(calls), 3)
        self.assertIn("--agent", calls[0]["argv"])
        self.assertIn("aia", calls[0]["argv"])
        self.assertNotIn("--agent", calls[2]["argv"], f"resumed call must not pass --agent: {calls[2]['argv']}")
        self.assertEqual(calls[2]["conv"], "conv-agent-1")

    def test_fresh_reply_attaches_header_root_raw_brief_and_scout_map(self):
        """Regression (W235): reply --fresh includes header, root raw-brief.md,
        root scout-map.md (if present), and then this reply's text; non-fresh reply does not."""
        self._seed_three_accounts()
        repo = self.repo({"a.txt": "base\n"})
        self._install_scripted_agy(
            "Path('a.txt').write_text(f'v{call_idx}\\n', encoding='utf-8')\n"
            "emit_agy(f'done {call_idx}', cid=f'conv-fresh-{call_idx}')\n"
        )
        run1, dir1 = self._start_and_wait(
            repo, "ORIGINAL-RAW-BRIEF-W235", "--worktree", "--no-scout", "--accept", "true"
        )
        # Seed root raw-brief.md and scout-map.md as scout-first does on the chain root.
        (dir1 / "raw-brief.md").write_text("ORIGINAL-RAW-BRIEF-W235\n详细要求：阶段 1 + 阶段 2\n", encoding="utf-8")
        (dir1 / "scout-map.md").write_text("SCOUT-MAP-W235：关键文件 a.txt\n", encoding="utf-8")

        # Ordinary non-fresh reply must NOT prepend the fresh header or raw-brief.md.
        run2, dir2 = self._reply_and_wait(run1, "NORMAL-REPLY-ROUND2", "--minor")
        prompt2 = (dir2 / "prompt.md").read_text(encoding="utf-8")
        header = "（这是同一任务的新会话：原任务说明附在下面，worktree 里保留着前几轮的改动，先看清现状再接着做。）"
        self.assertNotIn(header, prompt2)
        self.assertNotIn("ORIGINAL-RAW-BRIEF-W235", prompt2)
        self.assertIn("NORMAL-REPLY-ROUND2", prompt2)

        # Fresh reply on child run2 (also with --minor to verify rework length checks raw reply).
        run3, dir3 = self._reply_and_wait(run2, "FRESH-FOLLOWUP-ROUND3", "--fresh", "--minor")
        prompt3 = (dir3 / "prompt.md").read_text(encoding="utf-8")
        p_hdr = prompt3.find(header)
        p_brief = prompt3.find("ORIGINAL-RAW-BRIEF-W235\n详细要求：阶段 1 + 阶段 2")
        p_map = prompt3.find("SCOUT-MAP-W235：关键文件 a.txt")
        p_reply = prompt3.find("FRESH-FOLLOWUP-ROUND3")
        self.assertTrue(
            0 <= p_hdr < p_brief < p_map < p_reply,
            f"expected header < raw-brief < scout-map < reply in prompt3:\n{prompt3}",
        )
        calls = self._read_calls()
        self.assertEqual(calls[-1]["conv"], "")
        self.assertIn(header, calls[-1]["content"])
        self.assertIn("ORIGINAL-RAW-BRIEF-W235", calls[-1]["content"])


if __name__ == "__main__":
    unittest.main()
