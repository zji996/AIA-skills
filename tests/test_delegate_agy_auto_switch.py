"""End-to-end integration tests for agy dual-account automatic selection,
`delegate agy account pick`, stickiness vs fresh-session entry points,
writer mid-run exhaustion handoff (with appends/verdict/segmented tokens),
read-only rerun, and reviewer full-template retry before fallback.

All tests inject quota cache & scripted fake `agy` (never invokes real `agy`).
"""
import json
import os
import subprocess
import time
import unittest
from pathlib import Path

import test_delegate as fixtures
import test_delegate_638 as acct_fixtures


class DelegateAgyAutoSwitchTests(unittest.TestCase):
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
        self.usage_log = self.work / "agy-usage-calls.txt"

    def write_user_config(self, doc: dict) -> None:
        cfg_dir = Path(self.env["XDG_CONFIG_HOME"]) / "delegate"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "config.json").write_text(json.dumps(doc), encoding="utf-8")
        for k in ("DELEGATE_SCOUT_MODEL", "DELEGATE_REVIEW_MODEL", "DELEGATE_WRITE_MODEL"):
            self.env.pop(k, None)

    def _seed_two_accounts(self) -> None:
        vault_dir = self.fake_home / ".gemini/agy-swap"
        vault_dir.mkdir(parents=True, exist_ok=True)
        vault = {
            "account:alpha@example.test": acct_fixtures._b64_vault_entry(
                "alpha@example.test", "AT-ALPHA", "RT-ALPHA"
            ),
            "account:beta@example.test": acct_fixtures._b64_vault_entry(
                "beta@example.test", "AT-BETA", "RT-BETA"
            ),
        }
        (vault_dir / "vault.json").write_text(json.dumps(vault), encoding="utf-8")
        (vault_dir / "accounts.json").write_text(
            json.dumps(
                {
                    "alpha@example.test": {"name": "Alpha"},
                    "beta@example.test": {"name": "Beta"},
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

    def _enable_auto_mode(self) -> dict:
        res = self.cli("agy", "account", "use", "auto", "--json")
        self.assertEqual(res.returncode, 0, res.stderr + res.stdout)
        doc = json.loads(res.stdout)
        self.assertEqual(doc.get("mode"), "auto")
        self.assertEqual(doc.get("selected"), "auto")
        return doc

    def _write_quota_cache(
        self,
        alpha_gem=(80.0, 80.0),
        beta_gem=(80.0, 80.0),
        alpha_claude=(80.0, 80.0),
        beta_claude=(80.0, 80.0),
        fetched_epoch: int | None = None,
    ) -> Path:
        now = int(time.time()) if fetched_epoch is None else fetched_epoch
        now_iso = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now))
        reset_5h = "2099-01-01T00:00:00Z"
        reset_7d = "2099-01-07T00:00:00Z"
        quota_path = Path(self.env["XDG_STATE_HOME"]) / "delegate/agy-quota.json"
        quota_path.parent.mkdir(parents=True, exist_ok=True)
        doc = {
            "schemaVersion": 1,
            "updatedAt": now_iso,
            "updatedEpoch": now,
            "accounts": {
                "alpha": {
                    "ok": True,
                    "fetchedAt": now_iso,
                    "fetchedEpoch": now,
                    "entries": [
                        {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": alpha_gem[0], "resetAt": reset_5h},
                        {"pool": "Gemini Models", "window": "weekly", "remainingPct": alpha_gem[1], "resetAt": reset_7d},
                        {"pool": "Claude and GPT models", "window": "fiveHour", "remainingPct": alpha_claude[0], "resetAt": reset_5h},
                        {"pool": "Claude and GPT models", "window": "weekly", "remainingPct": alpha_claude[1], "resetAt": reset_7d},
                    ],
                },
                "beta": {
                    "ok": True,
                    "fetchedAt": now_iso,
                    "fetchedEpoch": now,
                    "entries": [
                        {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": beta_gem[0], "resetAt": reset_5h},
                        {"pool": "Gemini Models", "window": "weekly", "remainingPct": beta_gem[1], "resetAt": reset_7d},
                        {"pool": "Claude and GPT models", "window": "fiveHour", "remainingPct": beta_claude[0], "resetAt": reset_5h},
                        {"pool": "Claude and GPT models", "window": "weekly", "remainingPct": beta_claude[1], "resetAt": reset_7d},
                    ],
                },
            },
        }
        quota_path.write_text(json.dumps(doc), encoding="utf-8")
        return quota_path

    def _install_scripted_agy(self, script_body: str, extra_config: dict | None = None) -> Path:
        bin_dir = self.work / "agy-bin"
        bin_dir.mkdir(exist_ok=True)
        agy = bin_dir / "agy"
        counter_file = self.work / "agy-counter.txt"
        for p in (counter_file, self.calls_log, self.usage_log):
            if p.exists():
                p.unlink()
        agy.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "argv = sys.argv[1:]\n"
            "if '--version' in argv or '--help' in argv:\n"
            "    print('1.6.0')\n"
            "    sys.exit(0)\n"
            "if '/usage' in argv:\n"
            f"    with open({str(self.usage_log)!r}, 'a', encoding='utf-8') as f:\n"
            "        f.write(os.environ.get('DELEGATE_AGY_ACCOUNT', 'usage') + '\\n')\n"
            "    print('Gemini Models\\tFive Hour Limit Remaining\\t90%\\t2099-01-01T00:00:00Z')\n"
            "    print('Gemini Models\\tWeekly Limit Remaining\\t90%\\t2099-01-07T00:00:00Z')\n"
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
            "model_arg = ''\n"
            "for i, a in enumerate(argv):\n"
            "    if a == '--conversation' and i + 1 < len(argv):\n"
            "        conv_id = argv[i + 1]\n"
            "    if a == '--model' and i + 1 < len(argv):\n"
            "        model_arg = argv[i + 1]\n"
            "tok_path = Path(os.environ['HOME']) / '.gemini/antigravity-cli/antigravity-oauth-token'\n"
            "tok_doc = json.loads(tok_path.read_text()) if tok_path.is_file() else {}\n"
            "rec = {\n"
            "    'call': call_idx,\n"
            "    'account': os.environ.get('DELEGATE_AGY_ACCOUNT', ''),\n"
            "    'token': tok_doc.get('token', {}).get('access_token', ''),\n"
            "    'conv': conv_id,\n"
            "    'model': model_arg,\n"
            "    'run_dir': os.environ.get('DELEGATE_RUN_DIR', ''),\n"
            "    'content': content,\n"
            "}\n"
            f"with open({str(self.calls_log)!r}, 'a', encoding='utf-8') as f:\n"
            "    f.write(json.dumps(rec) + '\\n')\n"
            "def emit_agy(text, cid=None, inp=100, out=20, cr=500):\n"
            "    c = conv_id or cid or f'conv-{call_idx}'\n"
            "    print(json.dumps({'event':'init','conversation_id':c}), flush=True)\n"
            "    print(json.dumps({'event':'step_update','step_update':{'step_index':1,'step_type':'agent_response','state':'DONE','text_delta':text}}), flush=True)\n"
            "    print(json.dumps({'event':'result','result':{'conversation_id':c,'status':'SUCCESS','response':text,'usage':{'input_tokens':inp,'output_tokens':out,'cache_read_tokens':cr}}}), flush=True)\n"
            "def emit_error(err_msg, cid=None, inp=80, out=10, cr=300):\n"
            "    c = conv_id or cid or f'conv-{call_idx}'\n"
            "    print(json.dumps({'event':'init','conversation_id':c}), flush=True)\n"
            "    print(json.dumps({'event':'result','result':{'conversation_id':c,'status':'ERROR','error':err_msg,'response':'','usage':{'input_tokens':inp,'output_tokens':out,'cache_read_tokens':cr}}}), flush=True)\n"
            "    sys.exit(1)\n"
            + script_body
            + "\n",
            encoding="utf-8",
        )
        agy.chmod(0o755)
        self.env["PATH"] = f"{bin_dir}:{self.env.get('PATH', '')}"
        cfg = {
            "models": {
                "neon": "agy/gemini-4-neon",
                "opus": "agy/claude-opus-4-6-thinking",
            },
            "tiers": {"scout": "neon", "review": "neon", "write": "neon"},
            "agy": {
                "autoSwitch": {
                    "fiveHourMinPct": 30,
                    "weeklyMinPct": 30,
                    "refreshMinutes": 15,
                    "perRunReservePct": 5,
                }
            },
        }
        if extra_config:
            cfg.update(extra_config)
        self.write_user_config(cfg)
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

    def test_pick_cli_readonly_and_repo_config_rejects_auto_switch(self):
        self._seed_two_accounts()
        self._install_scripted_agy("emit_agy('unused')")
        self._write_quota_cache(
            alpha_gem=(20.0, 80.0),
            beta_gem=(90.0, 85.0),
            alpha_claude=(85.0, 90.0),
            beta_claude=(15.0, 80.0),
        )
        runs_dir = Path(self.env["DELEGATE_RUNS"])
        before_runs = set(runs_dir.iterdir()) if runs_dir.exists() else set()

        # 1. Fixed mode (default / legacy accounts.json without `mode`): `pick` works and reports fixed slot!
        r_fixed = self._run_raw("agy", "account", "pick", "--model", "neon", "--json")
        self.assertEqual(r_fixed.returncode, 0, r_fixed.stderr)
        doc_fixed = json.loads(r_fixed.stdout)
        self.assertEqual(doc_fixed["mode"], "fixed")
        self.assertEqual(doc_fixed["selected"], "alpha")
        self.assertFalse(doc_fixed["wouldSwitch"])
        self.assertIn("固定号", doc_fixed["reason"])

        # `list` and `status` both show the `auto` row (unselected in fixed mode; `*` on alpha)
        list_fixed = self._run_raw("agy", "account", "list").stdout
        self.assertIn("* alpha", list_fixed)
        self.assertIn("  auto", list_fixed)
        st_fixed = self._run_raw("agy", "account", "status").stdout
        self.assertTrue(st_fixed.splitlines()[0].startswith("委派选号：固定 alpha"), st_fixed)
        self.assertIn("本机实盘：alpha", st_fixed)

        # 2. Switch to `use auto` -> keeps live ~/.gemini on alpha, marks `* auto` in list/status!
        live_tok_path = self.fake_home / ".gemini/antigravity-cli/antigravity-oauth-token"
        live_before = live_tok_path.read_text(encoding="utf-8")
        self._enable_auto_mode()
        self.assertEqual(live_tok_path.read_text(encoding="utf-8"), live_before)

        list_auto = self._run_raw("agy", "account", "list").stdout
        self.assertTrue(
            list_auto.splitlines()[0].startswith("委派选号：auto · Gemini → beta · Claude/GPT → alpha"),
            list_auto,
        )
        self.assertIn("本机实盘：alpha（只给手动运行的 agy 用，auto 不改它）", list_auto)
        self.assertIn("* auto", list_auto)
        self.assertIn("Gemini→beta(绿)", list_auto)
        st_auto = self._run_raw("agy", "account", "status").stdout
        self.assertTrue(
            st_auto.splitlines()[0].startswith("委派选号：auto · Gemini → beta · Claude/GPT → alpha"),
            st_auto,
        )
        self.assertIn("本机实盘：alpha（只给手动运行的 agy 用，auto 不改它）", st_auto)

        # JSON pick for Gemini alias `neon` in auto mode -> picks beta (without calling /usage or creating runs)
        r_json = self._run_raw("agy", "account", "pick", "--model", "neon", "--json")
        self.assertEqual(r_json.returncode, 0, r_json.stderr)
        doc = json.loads(r_json.stdout)
        self.assertEqual(doc["mode"], "auto")
        self.assertEqual(doc["selected"], "beta")
        self.assertTrue(doc["wouldSwitch"])
        self.assertEqual(doc["pool"], "gemini")
        self.assertEqual(doc["picks"]["claude_gpt"]["selected"], "alpha")
        self.assertNotIn("example.test", r_json.stdout)

        # Human pick without --model prints both pools, tiers, cache time, and reasons
        r_hum = self._run_raw("agy", "account", "pick")
        self.assertEqual(r_hum.returncode, 0, r_hum.stderr)
        self.assertIn("模式：auto", r_hum.stdout)
        self.assertIn("新会话选号 [Gemini]：beta", r_hum.stdout)
        self.assertIn("新会话选号 [Claude/GPT]：alpha", r_hum.stdout)
        self.assertIn("缓存:", r_hum.stdout)

        # Strictly read-only: 0 `/usage` calls, 0 runs created, no agy-auto-switch.json written
        self.assertFalse(self.usage_log.exists(), "pick must NOT query agy -p /usage")
        after_runs = set(runs_dir.iterdir()) if runs_dir.exists() else set()
        self.assertEqual(before_runs, after_runs)
        state_file = Path(self.env["XDG_STATE_HOME"]) / "delegate/agy-auto-switch.json"
        self.assertFalse(state_file.exists(), "pick must NOT mutate poolCurrent")

        # Repository `.delegate.json` containing `agy.autoSwitch` MUST error immediately!
        bad_repo = self.repo(
            {
                "src/a.txt": "x\n",
                ".delegate.json": json.dumps({"agy": {"autoSwitch": {"fiveHourMinPct": 30}}}),
            }
        )
        bad_res = self.cli("start", "--workdir", str(bad_repo), "should reject repo autoSwitch")
        self.assertNotEqual(bad_res.returncode, 0)
        self.assertIn("autoSwitch", bad_res.stderr + bad_res.stdout)

        # Removed `agy.autoSwitch.enabled` in user config MUST also error with `use auto` hint!
        (bad_repo / ".delegate.json").unlink()
        self.write_user_config({"agy": {"autoSwitch": {"enabled": True}}})
        bad_en = self.cli("start", "--workdir", str(bad_repo), "should reject enabled")
        self.assertNotEqual(bad_en.returncode, 0)
        self.assertIn("use auto", bad_en.stderr + bad_en.stdout)

    def test_stickiness_and_new_session_entry_points_and_use_auto_vs_fixed_roundtrip(self):
        self._seed_two_accounts()
        repo = self.repo({"src/app.txt": "v0\n"})
        self._install_scripted_agy(
            "Path('src/app.txt').write_text(f'v{call_idx}\\n', encoding='utf-8')\n"
            "emit_agy(f'done {call_idx}')\n"
        )
        self._enable_auto_mode()
        live_tok_path = self.fake_home / ".gemini/antigravity-cli/antigravity-oauth-token"
        # alpha Gemini is Yellow (20%), beta Gemini is Green (90%) -> new session picks beta
        # without changing global live ~/.gemini (still alpha)!
        self._write_quota_cache(alpha_gem=(20.0, 80.0), beta_gem=(90.0, 85.0))
        run1, dir1 = self._start_and_wait(repo, "start auto switch", "--worktree", "--no-scout")
        meta1 = json.loads((dir1 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta1.get("agyAccount"), "beta")
        self.assertEqual(meta1.get("agyAccountSwitch", {}).get("kind"), "auto")
        self.assertEqual(meta1.get("agyAccountSwitch", {}).get("from"), "alpha")
        self.assertEqual(meta1.get("agyAccountSwitch", {}).get("to"), "beta")

        # Global live ~/.gemini is UNCHANGED (still alpha) and mode is still auto
        acct_st = json.loads(self.cli("agy", "account", "status", "--json").stdout)
        self.assertEqual(acct_st["active"], "alpha")
        self.assertEqual(acct_st["mode"], "auto")
        self.assertIn("AT-ALPHA", live_tok_path.read_text(encoding="utf-8"))

        # `status` and `card` render one-line auto-switch notice; `agy_account_auto` logged
        self.assertIn("自动选号：alpha → beta", self._run_raw("status", run1).stdout)
        self.assertIn("自动选号：alpha → beta", self._run_raw("card", run1, "--no-mark").stdout)
        run1_evs = [
            json.loads(l)
            for l in (dir1 / "events.jsonl").read_text(encoding="utf-8").splitlines()
            if l.strip()
        ]
        self.assertTrue(any(e.get("e") == "agy_account_auto" for e in run1_evs))

        # Now flip quotas: beta drops to 0% Red, alpha recovers to 95% Green.
        # Normal `reply` MUST stay strictly sticky on beta!
        self._write_quota_cache(alpha_gem=(95.0, 95.0), beta_gem=(0.0, 50.0))
        run2, dir2 = self._reply_and_wait(run1, "sticky reply on beta", "--minor")
        meta2 = json.loads((dir2 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta2.get("agyAccount"), "beta")
        self.assertNotIn("agyAccountSwitch", meta2)

        # `reply --fresh` is a NEW session in auto mode -> upgrades from Red beta to Green alpha!
        run3, dir3 = self._reply_and_wait(run2, "fresh reply switches to alpha", "--fresh", "--minor")
        meta3 = json.loads((dir3 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta3.get("agyAccount"), "alpha")
        self.assertEqual(meta3.get("agyAccountSwitch", {}).get("kind"), "auto")
        self.assertEqual(meta3.get("agyAccountSwitch", {}).get("from"), "beta")
        self.assertEqual(meta3.get("agyAccountSwitch", {}).get("to"), "alpha")

        # Switch to fixed `use beta` -> turns OFF auto-switch! Even when beta is 10% Yellow & alpha is 95% Green,
        # a new session MUST stay fixed on beta!
        self._write_quota_cache(alpha_gem=(95.0, 95.0), beta_gem=(10.0, 80.0))
        self.assertEqual(self.cli("agy", "account", "use", "beta").returncode, 0)
        self.assertIn("AT-BETA", live_tok_path.read_text(encoding="utf-8"))
        run4, dir4 = self._start_and_wait(repo, "start fixed on beta", "--no-scout")
        meta4 = json.loads((dir4 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta4.get("agyAccount"), "beta")
        self.assertNotIn("agyAccountSwitch", meta4)

        # Switch back to `use auto` -> live ~/.gemini stays on beta, and new session upgrades from Yellow beta to Green alpha!
        self._enable_auto_mode()
        self.assertIn("AT-BETA", live_tok_path.read_text(encoding="utf-8"))
        run5, dir5 = self._start_and_wait(repo, "start after re-entering auto", "--no-scout")
        meta5 = json.loads((dir5 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta5.get("agyAccount"), "alpha")
        self.assertEqual(meta5.get("agyAccountSwitch", {}).get("kind"), "auto")
        self.assertIn("AT-BETA", live_tok_path.read_text(encoding="utf-8"))

        calls = self._read_calls()
        self.assertEqual([c["account"] for c in calls], ["beta", "beta", "alpha", "beta", "alpha"])

    def test_writer_mid_run_exhaustion_handoff_and_segmented_tokens(self):
        self._seed_two_accounts()
        self._enable_auto_mode()
        repo = self.repo({"src/app.txt": "v0\n"})
        # Call 1 (alpha): modifies src/app.txt, consumes tokens (120, 30, 1000), then hits insufficient_quota.
        # Call 2 (handoff on beta): receives handoff prompt (original prompt + modified file index + git status),
        # finishes edit, emits tokens (60, 15, 400).
        self._install_scripted_agy(
            "if call_idx == 1:\n"
            "    Path('src/app.txt').write_text('partial-from-alpha\\n', encoding='utf-8')\n"
            "    emit_error('insufficient_quota: five hour limit reached', cid='conv-a', inp=120, out=30, cr=1000)\n"
            "else:\n"
            "    Path('src/app.txt').write_text('finished-by-beta\\n', encoding='utf-8')\n"
            "    emit_agy('handoff completed cleanly', cid='conv-b', inp=60, out=15, cr=400)\n"
        )
        self._write_quota_cache(alpha_gem=(80.0, 80.0), beta_gem=(85.0, 85.0))

        run1, dir1 = self._start_and_wait(
            repo, "write feature handoff", "--worktree", "--no-scout", "--accept", "true"
        )
        sum1 = json.loads((dir1 / "summary.json").read_text(encoding="utf-8"))
        meta1 = json.loads((dir1 / "meta.json").read_text(encoding="utf-8"))

        self.assertEqual(sum1["state"], "delivered", f"summary={sum1}")
        self.assertEqual(meta1.get("agyAccount"), "beta")
        self.assertEqual(meta1.get("agyAccountSwitch", {}).get("kind"), "handoff")
        self.assertEqual(meta1.get("agyAccountSwitch", {}).get("from"), "alpha")
        self.assertEqual(meta1.get("agyAccountSwitch", {}).get("to"), "beta")

        # Segmented token sum across conv-a (120,30,1000) + conv-b (60,15,400) = (180,45,1400)!
        self.assertEqual(sum1["tokens"], {"input": 180, "output": 45, "cacheRead": 1400})

        # Verify handoff prompt content (original prompt + modified file index + git status/diff, NO inline diff)
        calls = self._read_calls()
        self.assertEqual([c["account"] for c in calls], ["alpha", "beta"])
        self.assertEqual(calls[1]["conv"], "", "handoff must open a brand-new conversation")
        self.assertIn("write feature handoff", calls[1]["content"])
        self.assertIn("`src/app.txt`", calls[1]["content"])
        self.assertIn("git status", calls[1]["content"])
        self.assertNotIn("partial-from-alpha", calls[1]["content"])

        # Verify passive exhaustion was marked on alpha in quota cache
        qcache = json.loads(
            (Path(self.env["XDG_STATE_HOME"]) / "delegate/agy-quota.json").read_text(encoding="utf-8")
        )
        alpha_fh = [
            e
            for e in qcache["accounts"]["alpha"]["entries"]
            if "gemini" in e["pool"].lower() and e["window"] == "fiveHour"
        ][0]
        self.assertEqual(alpha_fh["source"], "passive_error")
        self.assertEqual(alpha_fh["remainingPct"], 0.0)

        # Verify status/card show one-line handoff notice & event logged
        self.assertIn("中途换号接力：alpha → beta", self._run_raw("status", run1).stdout)
        self.assertIn("中途换号接力：alpha → beta", self._run_raw("card", run1, "--no-mark").stdout)
        run_evs = [
            json.loads(l)
            for l in (dir1 / "events.jsonl").read_text(encoding="utf-8").splitlines()
            if l.strip()
        ]
        self.assertTrue(any(e.get("e") == "agy_account_handoff" for e in run_evs))

    def test_read_only_mid_run_exhaustion_reruns_original_prompt_once(self):
        self._seed_two_accounts()
        self._enable_auto_mode()
        repo = self.repo({"src/app.txt": "v0\n"})
        self._install_scripted_agy(
            "if call_idx == 1:\n"
            "    emit_error('quota exceeded for this account', cid='conv-ro-a')\n"
            "else:\n"
            "    emit_agy('read-only answer from beta', cid='conv-ro-b')\n"
        )
        self._write_quota_cache(alpha_gem=(80.0, 80.0), beta_gem=(85.0, 85.0))

        run1, dir1 = self._start_and_wait(repo, "read only inspect code", "--read-only")
        sum1 = json.loads((dir1 / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(sum1["state"], "answered", f"summary={sum1}")
        calls = self._read_calls()
        self.assertEqual([c["account"] for c in calls], ["alpha", "beta"])
        self.assertIn("read only inspect code", calls[1]["content"])
        self.assertNotIn("当前工作区已改动文件索引", calls[1]["content"])

    def test_reviewer_exhaustion_uses_full_template_retry_not_handoff_before_fallback(self):
        self._seed_two_accounts()
        self._enable_auto_mode()
        repo = self.repo(
            {
                "src/app.txt": "v0\n",
                ".delegate.json": json.dumps({"review": True}),
            }
        )
        # Call 1: author on alpha succeeds.
        # Call 2: primary reviewer on alpha hits insufficient_quota.
        # Call 3: primary reviewer retries ONCE on beta (same model `gemini-4-neon`, NOT fallback `opus`!)
        #         with full review template (containing rubric + task brief + diff, NO writer handoff text) -> PASS!
        self._install_scripted_agy(
            "run_dir = os.environ.get('DELEGATE_RUN_DIR', '')\n"
            "is_rev = '/review/' in run_dir\n"
            "if not is_rev:\n"
            "    Path('src/app.txt').write_text('v1\\n', encoding='utf-8')\n"
            "    emit_agy('author done', cid='conv-author')\n"
            "elif call_idx == 2:\n"
            "    emit_error('insufficient_quota: five hour limit reached', cid='conv-rev-a')\n"
            "else:\n"
            "    emit_agy('PASS\\nreviewed on beta\\n\\n## 主控重点看\\n无', cid='conv-rev-b')\n",
            extra_config={
                "tiers": {"scout": "neon", "review": "opus", "write": "neon"},
                "review": {"enabled": True, "model": "neon"},
            },
        )
        self._write_quota_cache(alpha_gem=(80.0, 80.0), beta_gem=(85.0, 85.0))

        run1, dir1 = self._start_and_wait(
            repo, "review exhaustion full template test", "--worktree", "--no-scout", "--accept", "true"
        )
        sum1 = json.loads((dir1 / "summary.json").read_text(encoding="utf-8"))
        meta1 = json.loads((dir1 / "meta.json").read_text(encoding="utf-8"))
        rev1 = json.loads((dir1 / "review.json").read_text(encoding="utf-8"))

        self.assertEqual(sum1["state"], "delivered", f"summary={sum1}")
        self.assertEqual(rev1.get("verdict"), "PASS", f"review={rev1}")
        self.assertNotIn("fallback", rev1, "same-model retry on beta succeeded, should NOT fall back to opus")
        self.assertEqual(meta1.get("reviewAgyAccount"), "beta")
        self.assertEqual(meta1.get("reviewAgyAccountSwitch", {}).get("kind"), "handoff")
        self.assertEqual(meta1.get("reviewAgyAccountSwitch", {}).get("from"), "alpha")
        self.assertEqual(meta1.get("reviewAgyAccountSwitch", {}).get("to"), "beta")

        calls = self._read_calls()
        self.assertEqual([c["account"] for c in calls], ["alpha", "alpha", "beta"])
        self.assertEqual(calls[2]["conv"], "", "reviewer account retry must be fresh session")
        self.assertIn("审查要点", calls[2]["content"])
        self.assertIn("review exhaustion full template test", calls[2]["content"])
        self.assertNotIn("当前工作区已改动文件索引", calls[2]["content"])
        self.assertIn("审查换号重审：alpha → beta", self._run_raw("status", run1).stdout)
        self.assertIn("审查换号重审：alpha → beta", self._run_raw("card", run1, "--no-mark").stdout)

    def test_reviewer_continuation_exhaustion_rebuilds_full_template_and_falls_back_if_other_red(self):
        self._seed_two_accounts()
        self._enable_auto_mode()
        repo = self.repo(
            {
                "src/app.txt": "v0\n",
                ".delegate.json": json.dumps({"review": True}),
            }
        )
        # Round 1: author(alpha) -> rev1(alpha) asks REWORK.
        # Round 2: author rework(alpha) -> rev2 continuation (fork_target != None, on alpha) hits quota error!
        #          Retry on beta MUST use FULL review template (contains 审查要点 + 任务说明 + 上一轮审查结论原文).
        self._install_scripted_agy(
            "run_dir = os.environ.get('DELEGATE_RUN_DIR', '')\n"
            "is_rev = '/review/' in run_dir\n"
            "if not is_rev:\n"
            "    Path('src/app.txt').write_text(f'v{call_idx}\\n', encoding='utf-8')\n"
            "    emit_agy(f'author done {call_idx}', cid='conv-author')\n"
            "elif call_idx == 2:\n"
            "    emit_agy('REWORK\\nplease add error handling\\n\\n## 主控重点看\\n无', cid='conv-rev')\n"
            "elif call_idx == 4:\n"
            "    emit_error('insufficient_quota: five hour limit reached', cid='conv-rev')\n"
            "else:\n"
            "    emit_agy('PASS\\nall fixed on beta\\n\\n## 主控重点看\\n无', cid='conv-rev-beta')\n",
            extra_config={
                "tiers": {"scout": "neon", "review": "opus", "write": "neon"},
                "review": {"enabled": True, "model": "neon"},
            },
        )
        self._write_quota_cache(alpha_gem=(80.0, 80.0), beta_gem=(85.0, 85.0))

        run1, dir1 = self._start_and_wait(
            repo, "incremental reviewer full template retry", "--worktree", "--no-scout", "--accept", "true"
        )
        rev1 = json.loads((dir1 / "review.json").read_text(encoding="utf-8"))
        self.assertEqual(rev1.get("verdict"), "PASS", f"review={rev1}")
        self.assertNotIn("fallback", rev1)

        calls = self._read_calls()
        # 1: author(alpha), 2: rev1(alpha REWORK), 3: author-rework(alpha),
        # 4: rev2-cont(alpha quota err), 5: rev2-retry(beta full template PASS)
        self.assertEqual([c["account"] for c in calls], ["alpha", "alpha", "alpha", "alpha", "beta"])
        self.assertNotEqual(calls[3]["conv"], "", "rev2 first try should continue conv-rev")
        self.assertEqual(calls[4]["conv"], "", "rev2 retry on beta must be fresh session")
        self.assertIn("审查要点", calls[4]["content"])
        self.assertIn("incremental reviewer full template retry", calls[4]["content"])
        self.assertIn("please add error handling", calls[4]["content"])

    def test_writer_rework_exhaustion_handoff_includes_verdict_and_caps_at_one_handoff(self):
        self._seed_two_accounts()
        self._enable_auto_mode()
        repo = self.repo(
            {
                "src/app.txt": "v0\n",
                ".delegate.json": json.dumps({"review": True}),
            }
        )
        # Call 1: author(alpha) succeeds
        # Call 2: reviewer(alpha) requests REWORK with specific feedback
        # Call 3: author rework(alpha) hits insufficient_quota
        # Call 4: handoff on beta receives rework verdict + original prompt, finishes edit
        # Call 5: reviewer round 2 PASS
        self._install_scripted_agy(
            "run_dir = os.environ.get('DELEGATE_RUN_DIR', '')\n"
            "is_rev = '/review/' in run_dir\n"
            "if not is_rev and call_idx == 1:\n"
            "    Path('src/app.txt').write_text('v1\\n', encoding='utf-8')\n"
            "    emit_agy('author v1', cid='conv-a')\n"
            "elif is_rev and call_idx == 2:\n"
            "    emit_agy('REWORK\\nFIX_UNIQUE_VERDICT_MARKER\\n\\n## 主控重点看\\n无', cid='conv-r')\n"
            "elif not is_rev and call_idx == 3:\n"
            "    emit_error('insufficient_quota: five hour limit reached', cid='conv-a')\n"
            "elif not is_rev and call_idx == 4:\n"
            "    Path('src/app.txt').write_text('v2-fixed\\n', encoding='utf-8')\n"
            "    emit_agy('rework handoff done', cid='conv-b')\n"
            "else:\n"
            "    emit_agy('PASS\\nlooks great\\n\\n## 主控重点看\\n无', cid='conv-r')\n"
        )
        self._write_quota_cache(alpha_gem=(80.0, 80.0), beta_gem=(85.0, 85.0))

        run1, dir1 = self._start_and_wait(
            repo, "rework handoff task", "--worktree", "--no-scout", "--accept", "true"
        )
        sum1 = json.loads((dir1 / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(sum1["state"], "delivered", f"summary={sum1}")
        calls = self._read_calls()
        # Call 4 is the writer handoff on beta during rework
        handoff_call = calls[3]
        self.assertEqual(handoff_call["account"], "beta")
        self.assertIn("rework handoff task", handoff_call["content"])
        self.assertIn("FIX_UNIQUE_VERDICT_MARKER", handoff_call["content"])
        self.assertIn("`src/app.txt`", handoff_call["content"])

    def test_reviewer_falls_back_when_other_account_is_red_and_events_do_not_trigger_stop_hook(self):
        self._seed_two_accounts()
        self._enable_auto_mode()
        repo = self.repo(
            {
                "src/app.txt": "v0\n",
                ".delegate.json": json.dumps({"review": True}),
            }
        )
        # alpha Gemini is 80% Green, beta Gemini is 2% Red; Claude/GPT on both is 85% Green.
        # Author(alpha) succeeds; primary reviewer(alpha, neon) hits insufficient_quota;
        # because beta is Red on Gemini pool, reviewer skips beta retry and falls back directly to opus!
        self._install_scripted_agy(
            "run_dir = os.environ.get('DELEGATE_RUN_DIR', '')\n"
            "is_rev = '/review/' in run_dir\n"
            "if not is_rev:\n"
            "    Path('src/app.txt').write_text('v1\\n', encoding='utf-8')\n"
            "    emit_agy('author done', cid='conv-author')\n"
            "elif 'neon' in model_arg:\n"
            "    emit_error('insufficient_quota: five hour limit reached', cid='conv-rev-a')\n"
            "else:\n"
            "    emit_agy('PASS\\nfallback opus passed\\n\\n## 主控重点看\\n无', cid='conv-rev-fb')\n",
            extra_config={
                "tiers": {"scout": "neon", "review": "opus", "write": "neon"},
                "review": {"enabled": True, "model": "neon"},
            },
        )
        self._write_quota_cache(
            alpha_gem=(80.0, 80.0),
            beta_gem=(2.0, 50.0),
            alpha_claude=(85.0, 85.0),
            beta_claude=(85.0, 85.0),
        )
        run1, dir1 = self._start_and_wait(
            repo, "fallback when other red", "--worktree", "--no-scout", "--accept", "true"
        )
        rev1 = json.loads((dir1 / "review.json").read_text(encoding="utf-8"))
        self.assertEqual(rev1.get("verdict"), "PASS", f"review={rev1}")
        self.assertIn("fallback", rev1, "when other account is Red, reviewer must fall back to opus")
        self.assertEqual(rev1["fallback"]["to"], "opus")
        calls = self._read_calls()
        # 1: author(alpha, neon), 2: rev(alpha, neon -> quota err), 3: fallback rev(alpha, opus -> PASS)
        self.assertEqual(len(calls), 3)
        self.assertIn("opus", calls[2]["model"])

    def test_single_account_is_noop_even_when_auto_switch_enabled(self):
        # Configure ONLY ONE account slot (alpha) with autoSwitch enabled -> does nothing.
        vault_dir = self.fake_home / ".gemini/agy-swap"
        vault_dir.mkdir(parents=True, exist_ok=True)
        vault = {
            "account:alpha@example.test": acct_fixtures._b64_vault_entry(
                "alpha@example.test", "AT-ALPHA", "RT-ALPHA"
            ),
        }
        (vault_dir / "vault.json").write_text(json.dumps(vault), encoding="utf-8")
        (vault_dir / "accounts.json").write_text(
            json.dumps({"alpha@example.test": {"name": "Alpha"}}), encoding="utf-8"
        )
        acct_fixtures._write_live_creds(
            self.fake_home, "alpha@example.test", "AT-ALPHA", "RT-ALPHA"
        )
        self.assertEqual(self.cli("agy", "account", "import-agy-swap", "--json").returncode, 0)
        self._enable_auto_mode()
        self._install_scripted_agy("emit_agy('single account ok')")
        repo = self.repo({"src/app.txt": "v0\n"})
        run1, dir1 = self._start_and_wait(repo, "single account task", "--read-only")
        meta1 = json.loads((dir1 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta1.get("agyAccount"), "alpha")
        self.assertNotIn("agyAccountSwitch", meta1)
        self.assertFalse(self.usage_log.exists(), "single account must not trigger /usage refresh")

    def test_reply_fresh_shows_auto_switch_line_even_when_pool_current_already_switched(self):
        self._seed_two_accounts()
        self._enable_auto_mode()
        repo = self.repo({"src/app.txt": "v0\n"})
        self._install_scripted_agy(
            "Path('src/app.txt').write_text(f'v{call_idx}\\n', encoding='utf-8')\n"
            "emit_agy(f'done {call_idx}')\n"
        )
        # 1. Start chain run1 on alpha (alpha Green 90%, beta Yellow 20%).
        self._write_quota_cache(alpha_gem=(90.0, 90.0), beta_gem=(20.0, 80.0))
        run1, dir1 = self._start_and_wait(repo, "chain on alpha", "--worktree", "--no-scout")
        meta1 = json.loads((dir1 / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta1.get("agyAccount"), "alpha")

        # 2. Flip quotas: alpha drops to Red 0%, beta rises to Green 95%.
        #    Run an unrelated `start` first so `poolCurrent[gemini]` is ALREADY switched to beta!
        self._write_quota_cache(alpha_gem=(0.0, 50.0), beta_gem=(95.0, 95.0))
        run_other, dir_other = self._start_and_wait(repo, "unrelated run moves poolCurrent", "--read-only")
        meta_other = json.loads((dir_other / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta_other.get("agyAccount"), "beta")
        self.assertEqual(meta_other.get("agyAccountSwitch", {}).get("kind"), "auto")

        # 3. Normal `reply` on run1 MUST stay sticky on alpha.
        run_sticky, dir_sticky = self._reply_and_wait(run1, "sticky reply stays on alpha", "--minor")
        meta_sticky = json.loads((dir_sticky / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta_sticky.get("agyAccount"), "alpha")
        self.assertNotIn("agyAccountSwitch", meta_sticky)

        # 4. `reply --fresh` on run1 (chain records alpha, poolCurrent is already beta):
        #    MUST pick beta AND record/render `自动选号：alpha → beta` in meta, status, and card!
        run_fresh, dir_fresh = self._reply_and_wait(
            run1, "fresh reply after poolCurrent already beta", "--fresh", "--minor"
        )
        meta_fresh = json.loads((dir_fresh / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta_fresh.get("agyAccount"), "beta")
        self.assertEqual(meta_fresh.get("agyAccountSwitch", {}).get("kind"), "auto")
        self.assertEqual(meta_fresh.get("agyAccountSwitch", {}).get("from"), "alpha")
        self.assertEqual(meta_fresh.get("agyAccountSwitch", {}).get("to"), "beta")
        self.assertIn("自动选号：alpha → beta", self._run_raw("card", run_fresh, "--no-mark").stdout)
        self.assertIn("自动选号：alpha → beta", self._run_raw("status", run_fresh).stdout)

        # 5. In fixed mode (`use beta`), `reply --fresh` on run1 picks beta WITHOUT `自动选号`.
        self.assertEqual(self.cli("agy", "account", "use", "beta").returncode, 0)
        run_fixed, dir_fixed = self._reply_and_wait(
            run1, "fresh reply in fixed mode", "--fresh", "--minor"
        )
        meta_fixed = json.loads((dir_fixed / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta_fixed.get("agyAccount"), "beta")
        self.assertNotIn("agyAccountSwitch", meta_fixed)
        self.assertNotIn("自动选号", self._run_raw("card", run_fixed, "--no-mark").stdout)

    def _seed_three_accounts(self) -> None:
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

    def _write_three_account_quota_cache(
        self, alpha_gem=(80.0, 80.0), beta_gem=(85.0, 85.0), gamma_gem=(90.0, 90.0)
    ) -> None:
        now = int(time.time())
        now_iso = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(now))
        reset_5h = "2099-01-01T00:00:00Z"
        reset_7d = "2099-01-07T00:00:00Z"
        quota_path = Path(self.env["XDG_STATE_HOME"]) / "delegate/agy-quota.json"
        quota_path.parent.mkdir(parents=True, exist_ok=True)
        def entry(gem):
            return {
                "ok": True,
                "fetchedAt": now_iso,
                "fetchedEpoch": now,
                "entries": [
                    {"pool": "Gemini Models", "window": "fiveHour", "remainingPct": gem[0], "resetAt": reset_5h},
                    {"pool": "Gemini Models", "window": "weekly", "remainingPct": gem[1], "resetAt": reset_7d},
                    {"pool": "Claude and GPT models", "window": "fiveHour", "remainingPct": 80.0, "resetAt": reset_5h},
                    {"pool": "Claude and GPT models", "window": "weekly", "remainingPct": 80.0, "resetAt": reset_7d},
                ],
            }
        doc = {
            "schemaVersion": 1,
            "updatedAt": now_iso,
            "updatedEpoch": now,
            "accounts": {
                "alpha": entry(alpha_gem),
                "beta": entry(beta_gem),
                "gamma": entry(gamma_gem),
            },
        }
        quota_path.write_text(json.dumps(doc), encoding="utf-8")

    def test_pending_reply_after_parent_quota_exhausted_auto_reselects_fresh_session(self):
        self._seed_three_accounts()
        self._enable_auto_mode()
        self.env["DELEGATE_RESUME_BACKOFF"] = "1,1,1"
        repo = self.repo({"src/app.txt": "v0\n"})
        gate = self.work / "gate-auto.flag"
        ready = self.work / "ready-auto.flag"
        quota_err = (
            'AGY_ERROR: {"short_error":"RESOURCE_EXHAUSTED (code 429): Individual quota reached. '
            'Please upgrade your subscription to increase your limits. Resets in 1h6m5s.",'
            '"status":"RESOURCE_EXHAUSTED","error_code":429,"code_kind":"http","retryable":true}'
        )
        # Call 1 (alpha): writes partial edit, waits on gate so we can queue follow-up, then hits 429 quota reached.
        # Call 2 (mid-run handoff on beta): also hits 429 quota reached -> parent ends `failed` on beta (no 3x retry).
        # Call 3 (pending_reply fallback in auto mode): must NOT stick to beta; opens fresh session on gamma
        #         with workspace interruption fact + original follow-up and succeeds.
        self._install_scripted_agy(
            "import time\n"
            "acct = os.environ.get('DELEGATE_AGY_ACCOUNT', '')\n"
            f"if call_idx == 1:\n"
            "    Path('src/app.txt').write_text('partial-from-alpha\\n', encoding='utf-8')\n"
            f"    Path({str(ready)!r}).write_text('1', encoding='utf-8')\n"
            f"    while not Path({str(gate)!r}).exists():\n"
            "        time.sleep(0.05)\n"
            f"    emit_error({quota_err!r}, cid='conv-alpha')\n"
            "elif call_idx == 2:\n"
            f"    emit_error({quota_err!r}, cid='conv-beta')\n"
            "else:\n"
            "    Path('src/app.txt').write_text('finished-by-gamma\\n', encoding='utf-8')\n"
            "    emit_agy('fallback fresh done on gamma', cid='conv-gamma')\n"
        )
        self._write_three_account_quota_cache(
            alpha_gem=(80.0, 80.0), beta_gem=(95.0, 95.0), gamma_gem=(85.0, 85.0)
        )
        started = self.cli(
            "start", "--workdir", str(repo), "--worktree", "--no-scout", "--accept", "true",
            "parent task with queued reply",
        )
        self.assertEqual(started.returncode, 0, started.stderr + started.stdout)
        parent_id = self._parse_run_id(started.stdout)
        parent_dir = Path(self.env["DELEGATE_RUNS"]) / parent_id
        self.until(lambda: ready.exists(), "parent did not start", timeout=20.0)

        # Queue a normal follow-up (NO --fresh) while parent is active.
        qres = self.cli("reply", parent_id, "补充：检查边界条件")
        self.assertEqual(qres.returncode, 0, qres.stderr + qres.stdout)

        # Release parent to fail on alpha -> handoff beta -> fail on beta.
        gate.write_text("go", encoding="utf-8")
        self.until(lambda: (parent_dir / "summary.json").is_file(), "parent did not settle", timeout=30.0)
        parent_sum = json.loads((parent_dir / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(parent_sum["state"], "failed", f"parent_sum={parent_sum}")
        self.assertNotIn("resumes", parent_sum, "quota exhaustion must not trigger 3-round transient retries")

        # Wait for pending_reply to deliver the child run and for the child run to settle.
        delivered_file = parent_dir / "reply-delivered.json"
        self.until(
            lambda: delivered_file.is_file(),
            f"pending reply not delivered; pending={parent_dir / 'pending-reply.json'}",
            timeout=30.0,
        )
        child_dir = Path(json.loads(delivered_file.read_text(encoding="utf-8"))["run"])
        child_id = child_dir.name
        self.until(lambda: (child_dir / "summary.json").is_file(), "child run did not settle", timeout=30.0)
        child_sum = json.loads((child_dir / "summary.json").read_text(encoding="utf-8"))
        child_meta = json.loads((child_dir / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(child_sum["state"], "delivered", f"child_sum={child_sum}")
        self.assertEqual(child_meta.get("agyAccount"), "gamma")
        self.assertEqual(child_meta.get("agyAccountSwitch", {}).get("kind"), "auto")
        self.assertEqual(child_meta.get("agyAccountSwitch", {}).get("from"), "beta")
        self.assertEqual(child_meta.get("agyAccountSwitch", {}).get("to"), "gamma")
        self.assertIn("自动选号：beta → gamma", self._run_raw("card", child_id, "--no-mark").stdout)

        calls = self._read_calls()
        self.assertEqual([c["account"] for c in calls], ["alpha", "beta", "gamma"])
        self.assertEqual(calls[2]["conv"], "", "fallback after quota exhaustion must open a fresh session")
        self.assertIn("上一会话因账号额度用尽中断，这是新会话；已有改动在工作区", calls[2]["content"])
        self.assertIn("补充：检查边界条件", calls[2]["content"])

    def test_pending_reply_after_parent_quota_exhausted_fixed_mode_fails_fast_with_hint(self):
        self._seed_two_accounts()
        # Fixed single-account mode on alpha (auto mode OFF).
        self.assertEqual(self.cli("agy", "account", "use", "alpha").returncode, 0)
        self.env["DELEGATE_RESUME_BACKOFF"] = "1,1,1"
        repo = self.repo({"src/app.txt": "v0\n"})
        gate = self.work / "gate-fixed.flag"
        ready = self.work / "ready-fixed.flag"
        quota_err = (
            'AGY_ERROR: {"short_error":"RESOURCE_EXHAUSTED (code 429): Individual quota reached. '
            'Please upgrade your subscription to increase your limits. Resets in 1h6m5s.",'
            '"status":"RESOURCE_EXHAUSTED","error_code":429,"code_kind":"http","retryable":true}'
        )
        self._install_scripted_agy(
            "import time\n"
            f"if call_idx == 1:\n"
            f"    Path({str(ready)!r}).write_text('1', encoding='utf-8')\n"
            f"    while not Path({str(gate)!r}).exists():\n"
            "        time.sleep(0.05)\n"
            f"emit_error({quota_err!r}, cid='conv-fixed-alpha')\n"
        )
        self._write_quota_cache(alpha_gem=(80.0, 80.0), beta_gem=(95.0, 95.0))
        started = self.cli(
            "start", "--workdir", str(repo), "--worktree", "--no-scout",
            "fixed mode parent task",
        )
        self.assertEqual(started.returncode, 0, started.stderr + started.stdout)
        parent_id = self._parse_run_id(started.stdout)
        parent_dir = Path(self.env["DELEGATE_RUNS"]) / parent_id
        self.until(lambda: ready.exists(), "parent did not start", timeout=20.0)

        qres = self.cli("reply", parent_id, "补充：固定号继续")
        self.assertEqual(qres.returncode, 0, qres.stderr + qres.stdout)

        gate.write_text("go", encoding="utf-8")
        self.until(lambda: (parent_dir / "summary.json").is_file(), "parent did not settle", timeout=30.0)
        parent_sum = json.loads((parent_dir / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(parent_sum["state"], "failed")
        self.assertEqual(parent_sum.get("attempts"), 1)
        self.assertNotIn("resumes", parent_sum)
        self.assertIn("delegate agy account use", parent_sum.get("error", ""))

        delivered_file = parent_dir / "reply-delivered.json"
        self.until(lambda: delivered_file.is_file(), "fixed-mode pending reply not delivered", timeout=30.0)
        child_dir = Path(json.loads(delivered_file.read_text(encoding="utf-8"))["run"])
        self.until(lambda: (child_dir / "summary.json").is_file(), "fixed-mode child did not settle", timeout=30.0)
        child_sum = json.loads((child_dir / "summary.json").read_text(encoding="utf-8"))
        child_meta = json.loads((child_dir / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(child_sum["state"], "failed")
        self.assertEqual(child_meta.get("agyAccount"), "alpha")
        self.assertEqual(child_sum.get("attempts"), 1, f"must not spin 3 retries: {child_sum}")
        self.assertNotIn("resumes", child_sum, f"must not spin 3 retries: {child_sum}")
        self.assertIn("delegate agy account use", child_sum.get("error", ""))

        calls = self._read_calls()
        # Exactly 1 call for parent + 1 call for child, both on alpha (zero transient retries).
        self.assertEqual([c["account"] for c in calls], ["alpha", "alpha"])

    def test_agy_auto_status_separates_delegation_and_live_and_real_quota(self):
        """Auto mode status/list puts delegation selection first, live slot on its own line, and labels real quota."""
        self._seed_two_accounts()
        self._write_quota_cache(
            alpha_gem=(15.0, 20.0),
            beta_gem=(90.0, 88.0),
            alpha_claude=(85.0, 82.0),
            beta_claude=(25.0, 30.0),
        )
        self._enable_auto_mode()

        st = self._run_raw("agy", "account", "status")
        self.assertEqual(st.returncode, 0, st.stderr)
        lines = st.stdout.splitlines()
        self.assertEqual(
            lines[0],
            "委派选号：auto · Gemini → beta · Claude/GPT → alpha（在跑 0 个）",
            st.stdout,
        )
        self.assertEqual(
            lines[1],
            "本机实盘：alpha（只给手动运行的 agy 用，auto 不改它）",
            st.stdout,
        )
        self.assertIn("alpha 的实时额度（agy -p /usage，", st.stdout)

        lst = self._run_raw("agy", "account", "list")
        self.assertEqual(lst.returncode, 0, lst.stderr)
        list_lines = lst.stdout.splitlines()
        self.assertEqual(
            list_lines[0],
            "委派选号：auto · Gemini → beta · Claude/GPT → alpha（在跑 0 个）",
            lst.stdout,
        )
        self.assertEqual(
            list_lines[1],
            "本机实盘：alpha（只给手动运行的 agy 用，auto 不改它）",
            lst.stdout,
        )

    def test_agy_home_cleaned_on_task_end_and_clean_sweeps_orphans(self):
        """Per-task delegate-agy-home-* is deleted on task finish/stop, and `clean` sweeps orphans safely."""
        self._seed_two_accounts()
        repo = self.repo({"README.md": "hi\n"})
        recorded_homes = self.work / "recorded_homes.txt"
        self._write_quota_cache(alpha_gem=(85.0, 85.0), beta_gem=(90.0, 90.0))
        self._enable_auto_mode()

        # 1. Task finishes normally -> overlay HOME created under self.runtime and removed on exit.
        self._install_scripted_agy(
            f"with open({str(recorded_homes)!r}, 'a', encoding='utf-8') as f:\n"
            "    f.write(os.environ.get('HOME', '') + '\\n')\n"
            "emit_agy('done', cid='conv-home-ok')\n"
        )
        started = self.cli("start", "--workdir", str(repo), "--worktree", "--no-scout", "ok task")
        self.assertEqual(started.returncode, 0, started.stderr + started.stdout)
        run_id = self._parse_run_id(started.stdout)
        run_dir = Path(self.env["DELEGATE_RUNS"]) / run_id
        self.until(lambda: (run_dir / "summary.json").is_file(), "ok task did not settle", timeout=30.0)

        homes = [line.strip() for line in recorded_homes.read_text(encoding="utf-8").splitlines() if line.strip()]
        self.assertGreaterEqual(len(homes), 1)
        for h in homes:
            hp = Path(h)
            self.assertEqual(hp.parent, self.runtime, f"overlay must live under isolated XDG_RUNTIME_DIR: {hp}")
            self.assertTrue(hp.name.startswith("delegate-agy-home-"), hp.name)
            self.assertFalse(hp.exists(), f"per-task agy home must be removed after task exit: {hp}")

        # 2. Task fails -> overlay HOME is also removed.
        recorded_homes.unlink()
        self._install_scripted_agy(
            f"with open({str(recorded_homes)!r}, 'a', encoding='utf-8') as f:\n"
            "    f.write(os.environ.get('HOME', '') + '\\n')\n"
            "sys.exit(2)\n"
        )
        started_fail = self.cli("start", "--workdir", str(repo), "--worktree", "--no-scout", "fail task")
        self.assertEqual(started_fail.returncode, 0, started_fail.stderr + started_fail.stdout)
        fail_id = self._parse_run_id(started_fail.stdout)
        fail_dir = Path(self.env["DELEGATE_RUNS"]) / fail_id
        self.until(lambda: (fail_dir / "summary.json").is_file(), "fail task did not settle", timeout=30.0)
        for h in [line.strip() for line in recorded_homes.read_text(encoding="utf-8").splitlines() if line.strip()]:
            self.assertFalse(Path(h).exists(), f"per-task agy home must be removed after failed task: {h}")

        # 3. `delegate clean --finished --sync` removes orphan delegate dirs and leaves non-delegate / live dirs untouched.
        dead_gem = self.runtime / "delegate-agy-home-999999-0-0000000000000001"
        (dead_gem / ".gemini").mkdir(parents=True)
        (dead_gem / ".gemini" / "google_accounts.json").write_text('{"active":"x@test"}', encoding="utf-8")

        dead_empty = self.runtime / "delegate-agy-home-999999-1-0000000000000002"
        dead_empty.mkdir(mode=0o700)

        live_dir = self.runtime / f"delegate-agy-home-{os.getpid()}-2-0000000000000003"
        (live_dir / ".gemini").mkdir(parents=True)

        non_delegate = self.runtime / "user-custom-dir"
        (non_delegate / ".gemini").mkdir(parents=True)

        bad_format = self.runtime / "delegate-agy-home-not-ours"
        (bad_format / ".gemini").mkdir(parents=True)

        victim = self.runtime / "symlink-victim"
        (victim / ".gemini").mkdir(parents=True)
        sym = self.runtime / "delegate-agy-home-999999-3-0000000000000004"
        sym.symlink_to(victim)

        clean_res = self.cli("clean", "--finished", "--force", "--sync", "--json")
        self.assertEqual(clean_res.returncode, 0, clean_res.stderr + clean_res.stdout)
        self.assertFalse(dead_gem.exists(), "dead delegate-agy-home-* with .gemini must be removed")
        self.assertFalse(dead_empty.exists(), "dead empty 0700 delegate-agy-home-* must be removed")
        self.assertTrue(live_dir.is_dir(), "live-pid delegate-agy-home-* must be kept")
        self.assertTrue(non_delegate.is_dir(), "non-delegate dir must be kept")
        self.assertTrue(bad_format.is_dir(), "bad-format dir must be kept")
        self.assertTrue(sym.is_symlink(), "symlink must not be removed")
        self.assertTrue(victim.is_dir(), "symlink target must not be touched")

        # 4. `_gc-worker` also sweeps orphan delegate-agy-home-* directories.
        gc_orphan = self.runtime / "delegate-agy-home-999999-4-0000000000000005"
        (gc_orphan / ".gemini").mkdir(parents=True)
        gc_res = self.cli("_gc-worker")
        self.assertEqual(gc_res.returncode, 0, gc_res.stderr)
        self.assertFalse(gc_orphan.exists(), "_gc-worker must sweep orphan delegate-agy-home-* dirs")


if __name__ == "__main__":
    unittest.main()
