"""Tests: agy per-account & time-window usage tracking, reviewer round usage,
live / uncounted running runs, `delegate usage`, and manual quota snapshot estimation.
"""
import json
import os
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

import test_delegate as fixtures
import test_delegate_638 as acct_fixtures


class DelegateAgyUsageTests(unittest.TestCase):
    fake_pi = fixtures.DelegateTests.fake_pi
    fake_docker = fixtures.DelegateTests.fake_docker
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def setUp(self):
        fixtures.DelegateTests.setUp(self)
        # Use isolated HOME inside tempdir so agy-accounts never touches real ~/.config or ~/.gemini.
        self.fake_home = self.work / "home"
        self.fake_home.mkdir(mode=0o700, exist_ok=True)
        self.env["HOME"] = str(self.fake_home)
        self.env["XDG_CONFIG_HOME"] = str(self.fake_home / ".config")
        self.env.pop("DELEGATE_AGY_BIN", None)

    def write_user_config(self, doc: dict) -> None:
        cfg_dir = Path(self.env["XDG_CONFIG_HOME"]) / "delegate"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "config.json").write_text(json.dumps(doc), encoding="utf-8")
        for k in ("DELEGATE_SCOUT_MODEL", "DELEGATE_REVIEW_MODEL", "DELEGATE_WRITE_MODEL"):
            self.env.pop(k, None)

    def _seed_accounts(self):
        """Create two local agy account slots (`alpha` active, `beta` standby) via import-agy-swap."""
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
        doc = json.loads(res.stdout)
        self.assertEqual(doc["active"], "alpha")

    def _install_scripted_agy(self, script_body: str) -> Path:
        bin_dir = self.work / "agy-bin"
        bin_dir.mkdir(exist_ok=True)
        agy = bin_dir / "agy"
        counter_file = self.work / "agy-calls.txt"
        if counter_file.exists():
            counter_file.unlink()
        agy.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys, time\n"
            "from pathlib import Path\n"
            "argv = sys.argv[1:]\n"
            "if '--version' in argv:\n"
            "    print('1.6.0')\n"
            "    sys.exit(0)\n"
            f"counter = Path({str(counter_file)!r})\n"
            "call_idx = int(counter.read_text()) + 1 if counter.exists() else 1\n"
            "counter.write_text(str(call_idx))\n"
            "raw_in = sys.stdin.read()\n"
            "msg = json.loads(raw_in.strip().splitlines()[-1]) if raw_in.strip() else {}\n"
            "content = msg.get('message', {}).get('content', '')\n"
            "conv_id = ''\n"
            "for i, a in enumerate(argv):\n"
            "    if a == '--conversation' and i + 1 < len(argv):\n"
            "        conv_id = argv[i + 1]\n"
            "def emit_agy(text, inp=100, out=20, cache=1000, cid='conv-1'):\n"
            "    c = conv_id or cid\n"
            "    print(json.dumps({'event':'init','conversation_id':c}), flush=True)\n"
            "    print(json.dumps({'event':'step_update','step_update':{'step_index':1,'step_type':'agent_response','state':'DONE','text_delta':text}}), flush=True)\n"
            "    print(json.dumps({'event':'result','result':{'conversation_id':c,'status':'SUCCESS','response':text,'usage':{'input_tokens':inp,'output_tokens':out,'cache_read_tokens':cache}}}), flush=True)\n"
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

    def test_author_rework_and_multi_round_reviewer_usage_by_account(self):
        self._seed_accounts()
        repo = self.repo(
            {
                "src/app.txt": "v0\n",
                ".delegate.json": json.dumps({"review": True}),
            }
        )
        # Call 1: author round 1 (conv-author-1, cum 1000/200/10000)
        # Call 2: reviewer round 1 -> REWORK (conv-rev-1, cum 400/100/4000)
        # Call 3: author rework round 2 (same conv-author-1, cum 1600/350/15000 -> delta 600/150/5000)
        # Call 4: reviewer round 2 -> PASS (same conv-rev-1, cum 700/180/6500 -> delta 300/80/2500)
        # Call 5: read-only scout under account `beta` (conv-beta-1, 500/100/2000)
        self._install_scripted_agy(
            "if call_idx == 1:\n"
            "    Path('src/app.txt').write_text('v1\\n')\n"
            "    emit_agy('initial implementation', inp=1000, out=200, cache=10000, cid='conv-author-1')\n"
            "elif call_idx == 2:\n"
            "    emit_agy('REWORK\\nplease update to v2\\n\\n## 主控重点看\\n- [中] src/app.txt:1 — need v2', inp=400, out=100, cache=4000, cid='conv-rev-1')\n"
            "elif call_idx == 3:\n"
            "    Path('src/app.txt').write_text('v2\\n')\n"
            "    emit_agy('updated to v2', inp=1600, out=350, cache=15000, cid='conv-author-1')\n"
            "elif call_idx == 4:\n"
            "    emit_agy('PASS\\nlooks good\\n\\n## 主控重点看\\n无', inp=700, out=180, cache=6500, cid='conv-rev-1')\n"
            "else:\n"
            "    emit_agy('scout summary', inp=500, out=100, cache=2000, cid='conv-beta-1')\n"
        )

        st1 = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "rework-usage", "--accept", "true", "update app")
        )
        self.assertEqual(st1["state"], "delivered")
        run1_dir = Path(st1["dir"])

        # Check review/round-1/usage.json and review/round-2/usage.json (delta, slot id only, no email!)
        r1_raw = (run1_dir / "review/round-1/usage.json").read_text(encoding="utf-8")
        r2_raw = (run1_dir / "review/round-2/usage.json").read_text(encoding="utf-8")
        self.assertNotIn("alpha@example.test", r1_raw)
        self.assertNotIn("alpha@example.test", r2_raw)
        r1 = json.loads(r1_raw)
        r2 = json.loads(r2_raw)
        self.assertEqual(r1["agyAccount"], "alpha")
        self.assertEqual(r1["tokens"], {"input": 400, "output": 100, "cacheRead": 4000})
        self.assertEqual(r2["agyAccount"], "alpha")
        self.assertEqual(r2["tokens"], {"input": 300, "output": 80, "cacheRead": 2500})

        # Check summary.json has agyAccount, reviewTokens, and structured usage without email.
        sum_raw = (run1_dir / "summary.json").read_text(encoding="utf-8")
        self.assertNotIn("alpha@example.test", sum_raw)
        summary = json.loads(sum_raw)
        self.assertEqual(summary["agyAccount"], "alpha")
        self.assertEqual(summary["reviewTokens"], {"input": 700, "output": 180, "cacheRead": 6500})
        self.assertEqual(summary["usage"]["author"], {"input": 1600, "output": 350, "cacheRead": 15000})
        self.assertEqual(summary["usage"]["reviewer"], {"input": 700, "output": 180, "cacheRead": 6500})
        self.assertEqual(summary["usage"]["total"], {"input": 2300, "output": 530, "cacheRead": 21500})
        self.assertEqual(len(summary["usage"]["rounds"]), 4)
        self.assertEqual(summary["review"]["tokens"], {"input": 700, "output": 180, "cacheRead": 6500})

        # Switch to beta and run a second task.
        sw = self.cli("agy", "account", "use", "beta", "--json")
        self.assertEqual(sw.returncode, 0, sw.stderr)
        st2 = self.outcome(
            self.cli("run", "--workdir", repo, "--read-only", "--name", "beta-scout", "inspect app")
        )
        self.assertEqual(st2["state"], "answered")

        # Verify `delegate usage --since 24h --by account --json`
        u_acct = self.cli("usage", "--since", "24h", "--by", "account", "--json")
        self.assertEqual(u_acct.returncode, 0, u_acct.stderr)
        self.assertNotIn("alpha@example.test", u_acct.stdout)
        self.assertNotIn("beta@example.test", u_acct.stdout)
        doc = json.loads(u_acct.stdout)
        by_key = {g["key"]: g for g in doc["groups"]}
        self.assertIn("alpha", by_key)
        self.assertIn("beta", by_key)
        self.assertEqual(by_key["alpha"]["author"]["total"], 1600 + 350 + 15000)
        self.assertEqual(by_key["alpha"]["reviewer"]["total"], 700 + 180 + 6500)
        self.assertEqual(by_key["beta"]["author"]["total"], 500 + 100 + 2000)
        self.assertEqual(by_key["beta"]["reviewer"]["total"], 0)
        self.assertEqual(doc["uncountedRunning"], [])

        # Verify --by day, --by run, --by repo and human table output
        for by_mode in ("day", "run", "repo"):
            res = self.cli("usage", "--since", "7d", "--by", by_mode, "--json")
            self.assertEqual(res.returncode, 0, res.stderr)
            d = json.loads(res.stdout)
            self.assertEqual(d["by"], by_mode)
            self.assertGreaterEqual(len(d["groups"]), 1)
            self.assertEqual(d["totals"]["total"]["total"], 2300 + 530 + 21500 + 500 + 100 + 2000)

        human = self.cli("usage", "--since", "24h", "--by", "account")
        self.assertEqual(human.returncode, 0, human.stderr)
        self.assertIn("alpha", human.stdout)
        self.assertIn("beta", human.stdout)
        self.assertIn("作者", human.stdout)
        self.assertIn("审查", human.stdout)
        self.assertIn("合计", human.stdout)

        # Verify ledger contains zero email addresses
        ledger = Path(self.env["XDG_STATE_HOME"]) / "delegate/agy-usage.jsonl"
        self.assertTrue(ledger.is_file())
        ledger_text = ledger.read_text(encoding="utf-8")
        self.assertNotIn("alpha@example.test", ledger_text)
        self.assertNotIn("beta@example.test", ledger_text)

    def test_running_agy_live_usage_and_uncounted_running_listed_separately(self):
        self._seed_accounts()
        repo = self.repo({"a.txt": "hello\n"})
        gate_live = self.work / "gate-live"
        gate_unc = self.work / "gate-unc"
        ready_live = self.work / "ready-live"
        ready_unc = self.work / "ready-unc"

        self._install_scripted_agy(
            f"gate_live = Path({str(gate_live)!r})\n"
            f"gate_unc = Path({str(gate_unc)!r})\n"
            f"ready_live = Path({str(ready_live)!r})\n"
            f"ready_unc = Path({str(ready_unc)!r})\n"
            "if 'LIVE_TASK' in content:\n"
            "    print(json.dumps({'event':'init','conversation_id':'conv-live'}), flush=True)\n"
            "    print(json.dumps({'event':'step_update','step_update':{'step_index':1,'step_type':'tool','state':'ACTIVE','tool_name':'view_file','tool_info':{'parameters':{'AbsolutePath':'a.txt'}},'usage':{'input_tokens':300,'output_tokens':50,'cache_read_tokens':3000}}}), flush=True)\n"
            "    ready_live.write_text('ok')\n"
            "    while not gate_live.exists():\n"
            "        time.sleep(0.05)\n"
            "    print(json.dumps({'event':'step_update','step_update':{'step_index':2,'step_type':'agent_response','state':'DONE','text_delta':'live done'}}), flush=True)\n"
            "    print(json.dumps({'event':'result','result':{'conversation_id':'conv-live','status':'SUCCESS','response':'live done','usage':{'input_tokens':500,'output_tokens':100,'cache_read_tokens':5000}}}), flush=True)\n"
            "else:\n"
            "    print(json.dumps({'event':'init','conversation_id':'conv-unc'}), flush=True)\n"
            "    print(json.dumps({'event':'step_update','step_update':{'step_index':1,'step_type':'tool','state':'ACTIVE','tool_name':'view_file','tool_info':{'parameters':{'AbsolutePath':'a.txt'}}}}), flush=True)\n"
            "    ready_unc.write_text('ok')\n"
            "    while not gate_unc.exists():\n"
            "        time.sleep(0.05)\n"
            "    print(json.dumps({'event':'step_update','step_update':{'step_index':2,'step_type':'agent_response','state':'DONE','text_delta':'unc done'}}), flush=True)\n"
            "    print(json.dumps({'event':'result','result':{'conversation_id':'conv-unc','status':'SUCCESS','response':'unc done','usage':{'input_tokens':200,'output_tokens':40,'cache_read_tokens':1000}}}), flush=True)\n"
        )

        r_live = self.cli("start", "--workdir", repo, "--read-only", "--name", "live-task", "LIVE_TASK inspect")
        self.assertEqual(r_live.returncode, 0, r_live.stderr)
        r_unc = self.cli("start", "--workdir", repo, "--read-only", "--name", "unc-task", "UNC_TASK inspect")
        self.assertEqual(r_unc.returncode, 0, r_unc.stderr)

        try:
            fixtures.wait_until(
                lambda: ready_live.exists() and ready_unc.exists(),
                "live and uncounted tasks to reach mid-run state",
                timeout=15.0,
            )

            def _mid_usage():
                res = self.cli("usage", "--since", "24h", "--by", "run", "--json")
                if res.returncode != 0:
                    return False
                doc = json.loads(res.stdout)
                return doc if len(doc.get("liveRunning", [])) == 1 else False

            mid = fixtures.wait_until(_mid_usage, "supervisor to flush live-usage.json", timeout=10.0)

            # Live task is counted in real time!
            self.assertEqual(len(mid["liveRunning"]), 1, mid)
            self.assertIn("live-task", mid["liveRunning"][0]["run"])
            self.assertEqual(mid["liveRunning"][0]["account"], "alpha")
            self.assertEqual(mid["liveRunning"][0]["tokens"]["total"], 3350)
            self.assertEqual(mid["totals"]["author"]["total"], 3350)

            # Uncounted task is listed separately as 运行中未计!
            self.assertEqual(len(mid["uncountedRunning"]), 1, mid)
            self.assertIn("unc-task", mid["uncountedRunning"][0]["run"])
            self.assertEqual(mid["uncountedRunning"][0]["account"], "alpha")
            self.assertEqual(mid["uncountedRunning"][0]["reason"], "运行中未计")

            mid_human = self.cli("usage", "--since", "24h", "--by", "account")
            self.assertEqual(mid_human.returncode, 0, mid_human.stderr)
            self.assertIn("运行中未计 (1)", mid_human.stdout)
            self.assertIn("unc-task", mid_human.stdout)
            self.assertIn("实时计入", mid_human.stdout)
        finally:
            gate_live.write_text("go")
            gate_unc.write_text("go")

        w = self.cli("wait", "--all")
        self.assertEqual(w.returncode, 0, w.stderr)

        after_res = self.cli("usage", "--since", "24h", "--by", "account", "--json")
        self.assertEqual(after_res.returncode, 0, after_res.stderr)
        after = json.loads(after_res.stdout)
        self.assertEqual(after["liveRunning"], [])
        self.assertEqual(after["uncountedRunning"], [])
        # Exact final total = (500+100+5000) + (200+40+1000) = 6840 (no double-count with mid-run 3350!)
        self.assertEqual(after["totals"]["total"]["total"], 6840)

    def test_account_note_quota_estimates_and_5h_7d_windows_in_snapshot(self):
        self._seed_accounts()
        now = int(time.time())
        ledger = Path(self.env["XDG_STATE_HOME"]) / "delegate/agy-usage.jsonl"
        ledger.parent.mkdir(parents=True, exist_ok=True)
        # Seed 3 records for `alpha`:
        # - 2h ago (in 5h & 7d): 10M tokens
        # - 3d ago (in 7d only): 40M tokens
        # - 10d ago (outside 7d): 99M tokens
        records = [
            {
                "v": 1,
                "at": "2026-11-24T02:00:00Z",
                "epoch": now - 2 * 3600,
                "run": "r-recent",
                "repo": "/tmp/demo",
                "repoName": "demo",
                "role": "author",
                "round": 1,
                "rework": False,
                "attempt": 1,
                "provider": "agy",
                "model": "agy/gemini-4-neon",
                "isAgy": True,
                "account": "alpha",
                "session": "c1",
                "tokens": {"input": 500_000, "output": 100_000, "cacheRead": 9_400_000},
            },
            {
                "v": 1,
                "at": "2026-11-21T02:00:00Z",
                "epoch": now - 3 * 86400,
                "run": "r-3d",
                "repo": "/tmp/demo",
                "repoName": "demo",
                "role": "reviewer",
                "round": 1,
                "rework": False,
                "attempt": 1,
                "provider": "agy",
                "model": "agy/gemini-4-neon",
                "isAgy": True,
                "account": "alpha",
                "session": "c2",
                "tokens": {"input": 2_000_000, "output": 500_000, "cacheRead": 37_500_000},
            },
            {
                "v": 1,
                "at": "2026-11-14T02:00:00Z",
                "epoch": now - 10 * 86400,
                "run": "r-old",
                "repo": "/tmp/demo",
                "repoName": "demo",
                "role": "author",
                "round": 1,
                "rework": False,
                "attempt": 1,
                "provider": "agy",
                "model": "agy/gemini-4-neon",
                "isAgy": True,
                "account": "alpha",
                "session": "c3",
                "tokens": {"input": 1_000_000, "output": 0, "cacheRead": 98_000_000},
            },
        ]
        ledger.write_text("\n".join(json.dumps(r) for r in records) + "\n", encoding="utf-8")

        # 1. Verify 5h (10M) and 7d (50M) windows in `agy account list --json` and `snapshot --json`
        lst = json.loads(self.cli("agy", "account", "list", "--json").stdout)
        alpha = next(a for a in lst["accounts"] if a["id"] == "alpha")
        self.assertEqual(alpha["usage"]["fiveHour"]["total"], 10_000_000)
        self.assertEqual(alpha["usage"]["sevenDay"]["total"], 50_000_000)
        self.assertEqual(alpha["usage"]["sevenDay"]["author"]["total"], 10_000_000)
        self.assertEqual(alpha["usage"]["sevenDay"]["reviewer"]["total"], 40_000_000)

        # 2. Record first manual quota snapshot (single-snapshot window calibration)
        n1_res = self.cli("agy", "account", "note", "alpha", "--weekly", "95.00", "--five-hour", "80.00", "--json")
        self.assertEqual(n1_res.returncode, 0, n1_res.stderr)
        n1 = json.loads(n1_res.stdout)
        self.assertTrue(n1["ok"])
        self.assertEqual(n1["quotaNote"]["weekly"], 95.0)
        self.assertEqual(n1["quotaNote"]["fiveHour"], 80.0)
        # 50M / 5% = 10M per 1% weekly; 10M / 20% = 500K per 1% five-hour
        self.assertEqual(n1["quotaEstimate"]["weekly"]["method"], "window")
        self.assertEqual(n1["quotaEstimate"]["weekly"]["tokensPerPct"], 10_000_000)
        self.assertEqual(n1["quotaEstimate"]["fiveHour"]["method"], "window")
        self.assertEqual(n1["quotaEstimate"]["fiveHour"]["tokensPerPct"], 500_000)
        self.assertGreater(n1["quotaEstimate"]["fiveHour"]["estimatedRemainingHours"], 0)

        # 3. Verify snapshot --json carries usage, quotaNote, quotaEstimate for TUI (active unchanged!)
        snap = json.loads(self.cli("snapshot", "--json").stdout)
        self.assertEqual(snap["agyAccounts"]["active"], "alpha")
        snap_alpha = next(a for a in snap["agyAccounts"]["accounts"] if a["id"] == "alpha")
        self.assertEqual(snap_alpha["usage"]["fiveHour"]["total"], 10_000_000)
        self.assertEqual(snap_alpha["usage"]["sevenDay"]["total"], 50_000_000)
        self.assertEqual(snap_alpha["quotaNote"]["weekly"], 95.0)
        self.assertEqual(snap_alpha["quotaEstimate"]["weekly"]["tokensPerPct"], 10_000_000)

        # 4. Append 6M tokens after note 1, then record note 2 (93.00% -> 2% drop => delta 3M / 1%)
        time.sleep(1.1)
        t_between = int(time.time())
        extra = {
            "v": 1,
            "at": "2026-11-24T04:01:00Z",
            "epoch": t_between,
            "run": "r-after-n1",
            "repo": "/tmp/demo",
            "repoName": "demo",
            "role": "author",
            "round": 1,
            "rework": False,
            "attempt": 1,
            "provider": "agy",
            "model": "agy/gemini-4-neon",
            "isAgy": True,
            "account": "alpha",
            "session": "c4",
            "tokens": {"input": 1_000_000, "output": 0, "cacheRead": 5_000_000},
        }
        with ledger.open("a", encoding="utf-8") as fh:
            fh.write(json.dumps(extra) + "\n")
        time.sleep(1.1)

        n2_res = self.cli("agy", "account", "note", "alpha", "--weekly", "93.00", "--json")
        self.assertEqual(n2_res.returncode, 0, n2_res.stderr)
        n2 = json.loads(n2_res.stdout)
        self.assertEqual(n2["quotaEstimate"]["weekly"]["method"], "delta")
        self.assertEqual(n2["quotaEstimate"]["weekly"]["tokensPerPct"], 3_000_000)

        # 5. Reject out-of-range percentage & verify --clear removes notes
        bad = self.cli("agy", "account", "note", "alpha", "--weekly", "105")
        self.assertNotEqual(bad.returncode, 0)
        clr = self.cli("agy", "account", "note", "alpha", "--clear", "--json")
        self.assertEqual(clr.returncode, 0, clr.stderr)
        self.assertIsNone(json.loads(clr.stdout)["quotaNote"])

    def test_reply_same_session_delta_no_double_count_and_archive_preservation(self):
        self._seed_accounts()
        repo = self.repo({"a.txt": "v0\n"})
        self._install_scripted_agy(
            "if call_idx == 1:\n"
            "    Path('a.txt').write_text('v1\\n')\n"
            "    emit_agy('round 1 done', inp=1000, out=200, cache=8000, cid='conv-chain-1')\n"
            "else:\n"
            "    Path('a.txt').write_text('v2\\n')\n"
            "    emit_agy('reply done', inp=1400, out=300, cache=11000, cid='conv-chain-1')\n"
        )

        st1 = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "chain-a", "--accept", "true", "step 1")
        )
        st2 = self.outcome(
            self.cli("reply", "--wait", st1["run"], "step 2")
        )
        self.assertEqual(st2["state"], "delivered")

        # Total across parent + reply MUST be the session cumulative (1400 + 300 + 11000 = 12700), not double-counted!
        u = json.loads(self.cli("usage", "--since", "24h", "--by", "account", "--json").stdout)
        self.assertEqual(u["totals"]["author"]["input"], 1400)
        self.assertEqual(u["totals"]["author"]["output"], 300)
        self.assertEqual(u["totals"]["author"]["cacheRead"], 11000)
        self.assertEqual(u["totals"]["total"]["total"], 12700)

        # Clean --sync archives usage-rounds.jsonl and usage stats remain intact!
        cln = self.cli("clean", "--finished", "--force", "--sync", cwd=repo)
        self.assertEqual(cln.returncode, 0, cln.stderr)
        arch_rounds = Path(self.env["XDG_CACHE_HOME"]) / "delegate/archive" / st1["run"] / "record/usage-rounds.jsonl"
        self.assertTrue(arch_rounds.is_file(), f"missing archived usage-rounds.jsonl at {arch_rounds}")
        u_after = json.loads(self.cli("usage", "--since", "24h", "--by", "account", "--json").stdout)
        self.assertEqual(u_after["totals"]["total"]["total"], 12700)


if __name__ == "__main__":
    unittest.main()
