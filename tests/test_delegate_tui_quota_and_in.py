"""Tests: real agy quota (`agy -p /usage </dev/null`), cache/age, temp HOME writeback,
real-over-note priority + 1h burn rate, analytics in `snapshot --json`, and
`--in <run>` fail-closed archive reconstruction for cleaned runs & released pool slots.
"""
import json
import os
import shutil
import stat
import subprocess
import time
import unittest
from pathlib import Path

import test_delegate as fixtures
import test_delegate_638 as acct_fixtures


class DelegateQuotaAndInFailClosedTests(unittest.TestCase):
    fake_pi = fixtures.DelegateTests.fake_pi
    fake_docker = fixtures.DelegateTests.fake_docker
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def setUp(self):
        fixtures.DelegateTests.setUp(self)
        old_cfg = Path(self.env["XDG_CONFIG_HOME"]) / "delegate/config.json"
        self.fake_home = self.work / "home"
        self.fake_home.mkdir(mode=0o700, exist_ok=True)
        self.env["HOME"] = str(self.fake_home)
        self.env["XDG_CONFIG_HOME"] = str(self.fake_home / ".config")
        if old_cfg.is_file():
            dst_cfg = Path(self.env["XDG_CONFIG_HOME"]) / "delegate/config.json"
            dst_cfg.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(old_cfg, dst_cfg)
        self.env.pop("DELEGATE_AGY_BIN", None)

    def _seed_accounts(self):
        vault_dir = self.fake_home / ".gemini/agy-swap"
        vault_dir.mkdir(parents=True, exist_ok=True)
        vault = {
            "account:alpha@example.test": acct_fixtures._b64_vault_entry(
                "alpha@example.test", "AT-ALPHA-ORIG", "RT-ALPHA-ORIG"
            ),
            "account:beta@example.test": acct_fixtures._b64_vault_entry(
                "beta@example.test", "AT-BETA-ORIG", "RT-BETA-ORIG"
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
            self.fake_home, "alpha@example.test", "AT-ALPHA-ORIG", "RT-ALPHA-ORIG"
        )
        res = self.cli("agy", "account", "import-agy-swap", "--json")
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(json.loads(res.stdout)["active"], "alpha")

    def test_real_quota_fetch_cache_age_temp_home_writeback_and_real_priority_over_note(self):
        self._seed_accounts()
        trace_file = self.work / "quota-calls.jsonl"
        mock_agy = self.work / "mock-agy-usage"
        mock_agy.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "argv = sys.argv[1:]\n"
            "assert argv == ['-p', '/usage'], f'unexpected argv: {argv}'\n"
            "stdin_data = sys.stdin.read()\n"
            "assert stdin_data == '', f'expected </dev/null, got {stdin_data!r}'\n"
            "home = Path(os.environ['HOME'])\n"
            "acct_json = json.loads((home / '.gemini/google_accounts.json').read_text())\n"
            "email = acct_json.get('active', '')\n"
            f"with open({str(trace_file)!r}, 'a', encoding='utf-8') as fh:\n"
            "    fh.write(json.dumps({'home': str(home), 'email': email}) + '\\n')\n"
            "if email == 'alpha@example.test':\n"
            "    print('Gemini Models\\tWeekly Limit Remaining\\t90%\\t2026-11-30T20:09:03Z')\n"
            "    print('Gemini Models\\tFive Hour Limit Remaining\\t80%\\t2026-11-24T15:32:40Z')\n"
            "    print('Claude and GPT models\\tFive Hour Limit Remaining\\t75%')\n"
            "    print('Notice: unknown preview banner line')\n"
            "elif email == 'beta@example.test':\n"
            "    # Simulate OAuth token refresh inside temp HOME\n"
            "    oc = home / '.gemini/oauth_creds.json'\n"
            "    doc = json.loads(oc.read_text())\n"
            "    doc['access_token'] = 'AT-BETA-REFRESHED'\n"
            "    oc.write_text(json.dumps(doc))\n"
            "    print('Gemini Models\\tWeekly Limit Remaining\\t98%\\t2026-12-01T00:00:00Z')\n"
            "else:\n"
            "    sys.stderr.write(f'unexpected email {email}\\n')\n"
            "    sys.exit(2)\n",
            encoding="utf-8",
        )
        mock_agy.chmod(0o755)
        self.env["DELEGATE_AGY_BIN"] = str(mock_agy)

        # Seed 3h of usage for `alpha`: weekly uses `since:3h` average, fiveHour uses `5h` average.
        now = int(time.time())
        ledger = Path(self.env["XDG_STATE_HOME"]) / "delegate/agy-usage.jsonl"
        ledger.parent.mkdir(parents=True, exist_ok=True)
        ledger_rows = [
            {
                "v": 1,
                "at": "2026-11-24T10:30:00Z",
                "epoch": now - 1200,  # 20m ago -> inside 1h, 5h, 24h, 7d
                "run": "r-1h",
                "repo": "/tmp/demo",
                "repoName": "demo",
                "role": "author",
                "mode": "write",
                "round": 1,
                "rework": False,
                "attempt": 1,
                "provider": "agy",
                "model": "agy/gemini-4-neon",
                "isAgy": True,
                "account": "alpha",
                "session": "s1",
                "tokens": {"input": 400_000, "output": 100_000, "cacheRead": 3_500_000},  # 4M
            },
            {
                "v": 1,
                "at": "2026-11-24T08:00:00Z",
                "epoch": now - 3 * 3600,
                "run": "r-3h-rev",
                "repo": "/tmp/demo",
                "repoName": "demo",
                "role": "reviewer",
                "mode": "read-only",
                "round": 1,
                "rework": False,
                "attempt": 1,
                "provider": "agy",
                "model": "agy/gemini-4-neon",
                "isAgy": True,
                "account": "alpha",
                "session": "s2",
                "tokens": {"input": 200_000, "output": 50_000, "cacheRead": 5_750_000},  # 6M
            },
        ]
        ledger.write_text("\n".join(json.dumps(r) for r in ledger_rows) + "\n", encoding="utf-8")

        # Also record a manual `note` with different percentages to verify real quota takes priority.
        note_res = self.cli("agy", "account", "note", "alpha", "--weekly", "50", "--five-hour", "50", "--json")
        self.assertEqual(note_res.returncode, 0, note_res.stderr)

        # 1. Trigger manual quota refresh via `delegate agy account list --quota --json`
        res = self.cli("agy", "account", "list", "--quota", "--json")
        self.assertEqual(res.returncode, 0, res.stderr)
        doc = json.loads(res.stdout)
        self.assertEqual(doc["active"], "alpha")
        self.assertIsNotNone(doc["quotaAgeSeconds"])
        self.assertGreaterEqual(doc["quotaAgeSeconds"], 0)

        # Verify cache file exists in delegate state dir with fetch timestamp
        cache_path = Path(self.env["XDG_STATE_HOME"]) / "delegate/agy-quota.json"
        self.assertTrue(cache_path.is_file())
        cache_doc = json.loads(cache_path.read_text(encoding="utf-8"))
        self.assertTrue(cache_doc["accounts"]["alpha"]["ok"])
        self.assertTrue(cache_doc["accounts"]["beta"]["ok"])

        # Verify BOTH active and standby ran in isolated temp HOMEs (never real HOME) and both were deleted!
        calls = [json.loads(line) for line in trace_file.read_text().splitlines() if line.strip()]
        by_email = {c["email"]: Path(c["home"]) for c in calls}
        self.assertNotEqual(by_email["alpha@example.test"].resolve(), self.fake_home.resolve())
        self.assertFalse(by_email["alpha@example.test"].exists(), "active temp HOME was not deleted")
        self.assertNotEqual(by_email["beta@example.test"].resolve(), self.fake_home.resolve())
        self.assertFalse(by_email["beta@example.test"].exists(), "standby temp HOME was not deleted")
        self.assertNotEqual(by_email["alpha@example.test"].resolve(), by_email["beta@example.test"].resolve())

        # Verify standby refreshed credential was written back to beta slot with 0600 mode,
        # while live HOME (alpha) remains untouched!
        beta_oauth = self.fake_home / ".config/delegate/agy-accounts/beta/oauth_creds.json"
        self.assertEqual(json.loads(beta_oauth.read_text())["access_token"], "AT-BETA-REFRESHED")
        self.assertEqual(stat.S_IMODE(beta_oauth.stat().st_mode), 0o600)
        live_oauth = self.fake_home / ".gemini/oauth_creds.json"
        self.assertEqual(json.loads(live_oauth.read_text())["access_token"], "AT-ALPHA-ORIG")

        # Verify parsed entries, missing resetAt tolerated, unknown line preserved verbatim!
        alpha = next(a for a in doc["accounts"] if a["id"] == "alpha")
        self.assertTrue(alpha["quota"]["ok"])
        self.assertEqual(len(alpha["quota"]["entries"]), 3)
        self.assertEqual(alpha["quota"]["unknownLines"], ["Notice: unknown preview banner line"])
        claude_row = next(e for e in alpha["quota"]["entries"] if "Claude" in e["pool"])
        self.assertEqual(claude_row["remainingPct"], 75.0)
        self.assertEqual(claude_row["resetAt"], "")

        # Real quota takes priority over manual note (90% weekly vs manual 50%),
        # and quotaEstimate uses `/usage` same-cycle state (no local token calibration fields).
        self.assertEqual(alpha["quotaNote"]["weekly"], 50.0)
        self.assertEqual(alpha["quotaEstimate"]["weekly"]["source"], "real")
        self.assertEqual(alpha["quotaEstimate"]["weekly"]["remainingPct"], 90.0)
        self.assertIn(alpha["quotaEstimate"]["weekly"]["status"], ("idle", "safe", "insufficient", "depletes"))
        self.assertNotIn("tokensPerPct", alpha["quotaEstimate"]["weekly"])
        self.assertNotIn("burnRateSource", alpha["quotaEstimate"]["weekly"])
        self.assertEqual(alpha["quotaEstimate"]["fiveHour"]["source"], "real")
        self.assertNotIn("tokensPerPct", alpha["quotaEstimate"]["fiveHour"])
        # Verify history persisted resetAt epochs alongside percentages
        alpha_hist = cache_doc["accounts"]["alpha"]["history"]
        self.assertGreaterEqual(len(alpha_hist), 1)
        self.assertIn("weeklyResetEp", alpha_hist[-1]["pools"]["Gemini Models"])
        self.assertIn("fiveHourResetEp", alpha_hist[-1]["pools"]["Gemini Models"])
        self.assertEqual(alpha["usage"]["oneHour"]["total"], 4_000_000)
        self.assertEqual(len(alpha["usage"]["hourly24h"]), 24)
        self.assertEqual(alpha["usage"]["hourly24h"][-1], 4_000_000)

        # 2. Verify `status --quota` human output includes real quota, unknown line, age, and estimate
        st_res = self.cli("agy", "account", "status", "--quota")
        self.assertEqual(st_res.returncode, 0, st_res.stderr)
        self.assertIn("alpha 的实时额度（agy -p /usage，", st_res.stdout)
        self.assertIn("Gemini Models", st_res.stdout)
        self.assertIn("[raw] Notice: unknown preview banner line", st_res.stdout)
        self.assertIn("fallback; real quota takes priority", st_res.stdout)

        # 3. Verify lock-free cache reads (without --quota) NEVER invoke `agy`
        failing_agy = self.work / "failing-agy"
        failing_agy.write_text("#!/bin/sh\necho SHOULD_NOT_RUN >&2\nexit 99\n", encoding="utf-8")
        failing_agy.chmod(0o755)
        self.env["DELEGATE_AGY_BIN"] = str(failing_agy)
        quota_cache = Path(self.env["XDG_STATE_HOME"]) / "delegate/agy-quota.json"
        qdoc = json.loads(quota_cache.read_text(encoding="utf-8"))
        qdoc["accounts"]["alpha"]["fetchedEpoch"] = int(qdoc["accounts"]["alpha"]["fetchedEpoch"]) - 5
        quota_cache.write_text(json.dumps(qdoc), encoding="utf-8")

        snap_res = self.cli("snapshot", "--json")
        self.assertEqual(snap_res.returncode, 0, snap_res.stderr)
        snap = json.loads(snap_res.stdout)
        snap_alpha = next(a for a in snap["agyAccounts"]["accounts"] if a["id"] == "alpha")
        self.assertGreaterEqual(snap_alpha["quota"]["ageSeconds"], 1)
        self.assertEqual(snap_alpha["quotaEstimate"]["weekly"]["source"], "real")
        # Verify TUI analytics payload in `snapshot --json`
        analytics = snap["analytics"]
        self.assertEqual(len(analytics["hourly24h"]["total"]), 24)
        self.assertEqual(analytics["byPurpose"]["write"], 4_000_000)
        self.assertEqual(analytics["byPurpose"]["review"], 6_000_000)
        self.assertEqual(len(analytics["daily7d"]), 7)
        self.assertTrue(any(m["model"] == "gemini-4-neon" for m in analytics["byModel"]))

    def test_in_cleaned_run_reconstructs_readonly_snapshot_from_archive(self):
        repo = self.repo({"app.txt": "v0\n"})
        obs_file = self.work / "observed-archived.txt"
        self.fake_pi(
            [fixtures.answer("wrote app.txt and added.txt"), fixtures.SETTLED],
            [fixtures.answer("inspected archived snapshot"), fixtures.SETTLED],
            pre=(
                'if [ "$n" -eq 1 ]; then '
                'printf "from-old-run\\n" > app.txt; '
                'printf "run-only-file\\n" > added.txt; '
                'else '
                f'printf "app=%s added=%s\\n" "$(cat app.txt 2>/dev/null || echo MISSING)" "$(cat added.txt 2>/dev/null || echo MISSING)" > {str(obs_file)!r}; '
                'fi'
            ),
        )

        st = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "old-write", "--accept", "true", "WRITE_STEP")
        )
        self.assertEqual(st["state"], "delivered", st)
        run_id = st["run"]
        wt_path = Path(st["worktree"])

        # Apply and clean (removes worktree, archives manifest.json + applied.patch + record)
        ap = self.cli("apply", run_id, cwd=repo)
        self.assertEqual(ap.returncode, 0, ap.stderr)
        cl = self.cli("clean", "--sync", run_id, cwd=repo)
        self.assertEqual(cl.returncode, 0, cl.stderr)
        self.assertFalse(wt_path.exists(), "worktree should be deleted after clean")

        # Advance trunk to a completely different state so reading trunk would be immediately obvious!
        (Path(repo) / "app.txt").write_text("newer-trunk-content\n", encoding="utf-8")
        (Path(repo) / "added.txt").unlink()
        subprocess.run(["git", "-C", repo, "add", "-A"], check=True)
        subprocess.run(["git", "-C", repo, "commit", "-m", "advance trunk"], check=True, stdout=subprocess.DEVNULL)

        # Verify archive has applied.patch + manifest.json, and start --read-only --in <run_id>
        arch_dir = Path(self.env["XDG_CACHE_HOME"]) / "delegate/archive" / run_id
        self.assertTrue((arch_dir / "manifest.json").is_file())
        self.assertTrue((arch_dir / "record/applied.patch").is_file())

        rev_start = self.cli(
            "start", "--workdir", repo, "--read-only", "--in", run_id, "--name", "rev-archived", "INSPECT"
        )
        self.assertEqual(rev_start.returncode, 0, rev_start.stderr)
        self.assertIn("重建只读快照", rev_start.stderr)
        rev_id = self.outcome(rev_start)["run"]
        w = self.cli("wait", rev_id, cwd=repo)
        self.assertEqual(w.returncode, 0, w.stderr)
        observed = obs_file.read_text(encoding="utf-8").strip()
        self.assertEqual(observed, "app=from-old-run added=run-only-file")
        self.assertNotIn("newer-trunk-content", observed)

    def test_in_cleaned_run_fails_closed_when_archive_unreconstructable(self):
        repo = self.repo({"app.txt": "v0\n"})
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre='printf "from-broken-run\\n" > app.txt',
        )
        st = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "broken-run", "--accept", "true", "edit")
        )
        self.assertEqual(st["state"], "delivered", st)
        run_id = st["run"]
        ap = self.cli("apply", run_id, cwd=repo)
        self.assertEqual(ap.returncode, 0, ap.stderr)
        cl = self.cli("clean", "--sync", run_id, cwd=repo)
        self.assertEqual(cl.returncode, 0, cl.stderr)

        arch_rec = Path(self.env["XDG_CACHE_HOME"]) / "delegate/archive" / run_id / "record"
        # Case A: corrupt applied.patch -> must fail closed, never fall back to trunk!
        (arch_rec / "applied.patch").write_text(
            "diff --git a/nonexistent.txt b/nonexistent.txt\n"
            "--- a/nonexistent.txt\n"
            "+++ b/nonexistent.txt\n"
            "@@ -1,1 +1,1 @@\n"
            "-does not exist\n"
            "+broken\n",
            encoding="utf-8",
        )
        bad_patch = self.cli("start", "--workdir", repo, "--read-only", "--in", run_id, "INSPECT")
        self.assertNotEqual(bad_patch.returncode, 0, "must fail closed on corrupt patch")
        self.assertIn("无法从归档重建只读快照", bad_patch.stderr)
        self.assertIn("下一步", bad_patch.stderr)
        self.assertIn(f"delegate show {run_id}", bad_patch.stderr)

        # Case B: missing patches + unreachable after tree -> must fail closed!
        for pname in ("applied.patch", "changes.patch"):
            p = arch_rec / pname
            if p.exists():
                p.unlink()
        cj = arch_rec / "changes.json"
        cdoc = json.loads(cj.read_text(encoding="utf-8"))
        cdoc["after"] = "0" * 40
        cj.write_text(json.dumps(cdoc), encoding="utf-8")
        ap_meta = arch_rec / ".applied"
        if ap_meta.exists():
            adoc = json.loads(ap_meta.read_text(encoding="utf-8"))
            adoc["tree"] = "0" * 40
            ap_meta.write_text(json.dumps(adoc), encoding="utf-8")

        missing_arch = self.cli("start", "--workdir", repo, "--read-only", "--in", run_id, "INSPECT")
        self.assertNotEqual(missing_arch.returncode, 0, "must fail closed when archive cannot reconstruct")
        self.assertIn("拒绝启动", missing_arch.stderr)
        self.assertIn("下一步", missing_arch.stderr)

        # Confirm zero read-only runs were silently launched against trunk
        runs_dir = Path(self.env["DELEGATE_RUNS"])
        live_runs = [p for p in runs_dir.iterdir() if p.is_dir() and (p / "meta.json").is_file()] if runs_dir.exists() else []
        self.assertEqual(live_runs, [], f"unexpected runs spawned after fail-closed: {live_runs}")

    def test_in_released_pool_slot_reconstructs_from_archive_not_trunk(self):
        repo = self.repo(
            {
                "app.txt": "v0\n",
                ".delegate.json": json.dumps({"worktree": {"pool": 1}}),
            }
        )
        obs_file = self.work / "observed-pool.txt"
        self.fake_pi(
            [fixtures.answer("run 1 done"), fixtures.SETTLED],
            [fixtures.answer("run 2 done"), fixtures.SETTLED],
            [fixtures.answer("inspected pool run 1"), fixtures.SETTLED],
            pre=(
                'if [ "$n" -eq 1 ]; then '
                'printf "pool-run-1-value\\n" > app.txt; '
                'elif [ "$n" -eq 2 ]; then '
                'printf "pool-run-2-value\\n" > app.txt; '
                'else '
                f'cat app.txt > {str(obs_file)!r}; '
                'fi'
            ),
        )
        st1 = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "pool-r1", "--accept", "true", "POOL_WRITE_1")
        )
        self.assertEqual(st1["state"], "delivered", st1)
        run1 = st1["run"]
        slot_path = Path(st1["worktree"])
        self.assertTrue(slot_path.is_dir())
        self.assertEqual(self.cli("apply", run1, cwd=repo).returncode, 0)
        self.assertEqual(self.cli("clean", "--sync", run1, cwd=repo).returncode, 0)

        # Reuse the same pool slot for a second run so slot_path STILL EXISTS with DIFFERENT content!
        (Path(repo) / "app.txt").write_text("trunk-after-r1\n", encoding="utf-8")
        subprocess.run(["git", "-C", repo, "add", "-A"], check=True)
        subprocess.run(["git", "-C", repo, "commit", "-m", "trunk after r1"], check=True, stdout=subprocess.DEVNULL)

        st2 = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "pool-r2", "--accept", "true", "POOL_WRITE_2")
        )
        self.assertEqual(st2["state"], "delivered", st2)
        self.assertEqual(Path(st2["worktree"]), slot_path, "expected slot reuse")

        # Now `--read-only --in <run1>` MUST reconstruct run1 from archive (seeing pool-run-1-value),
        # NOT read slot_path (which now holds pool-run-2-value)!
        rev = self.cli("start", "--workdir", repo, "--read-only", "--in", run1, "--name", "rev-pool-r1", "INSPECT")
        self.assertEqual(rev.returncode, 0, rev.stderr)
        self.assertIn("重建只读快照", rev.stderr)
        rev_id = self.outcome(rev)["run"]
        self.assertEqual(self.cli("wait", rev_id, cwd=repo).returncode, 0)
        observed = obs_file.read_text(encoding="utf-8").strip()
        self.assertEqual(observed, "pool-run-1-value")
        self.assertNotIn("pool-run-2-value", observed)
        self.assertNotIn("trunk-after-r1", observed)

    def test_quota_isolation_prevents_live_home_contamination_and_keeps_two_accounts_separate(self):
        self._seed_accounts()
        # Simulate a long-lived interactive `agy` process started under `beta` overwriting
        # live `~/.gemini/antigravity-cli/antigravity-oauth-token` while leaving
        # `~/.gemini/google_accounts.json` pointing at `alpha`.
        live_cli_tok = self.fake_home / ".gemini/antigravity-cli/antigravity-oauth-token"
        live_cli_tok.write_text(
            json.dumps(
                {
                    "auth_method": "consumer",
                    "id_token_email": "beta@example.test",
                    "token": {
                        "access_token": "AT-BETA-LIVE-STALE",
                        "refresh_token": "RT-BETA-ORIG",
                        "expiry": "2026-11-25T19:50:07Z",
                        "token_type": "Bearer",
                    },
                }
            ),
            encoding="utf-8",
        )
        live_cli_tok.chmod(0o600)

        # Drift must be detected immediately even though google_accounts.json still says alpha!
        st_pre = json.loads(self.cli("agy", "account", "status", "--json").stdout)
        self.assertEqual(st_pre["active"], "alpha")
        self.assertEqual(st_pre["liveEmail"], "beta@example.test")
        self.assertTrue(st_pre["liveDrifted"])

        # Mock `agy -p /usage` reads `antigravity-cli/antigravity-oauth-token` (just like real agy)
        # and writes an isolated auth log line under HOME.
        mock_agy = self.work / "mock-agy-cli-token"
        mock_agy.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "home = Path(os.environ['HOME'])\n"
            "tok = json.loads((home / '.gemini/antigravity-cli/antigravity-oauth-token').read_text())\n"
            "email = tok['id_token_email']\n"
            "log_dir = home / '.gemini/antigravity-cli/log'\n"
            "log_dir.mkdir(parents=True, exist_ok=True)\n"
            "(log_dir / 'cli-usage.log').write_text(\n"
            "    f'I1125 19:02:22 server_oauth.go:226] OAuth: authenticated successfully as {email}\\n'\n"
            ")\n"
            "if email == 'alpha@example.test':\n"
            "    print('Gemini Models\\tWeekly Limit Remaining\\t93%\\t2026-11-30T20:03:02Z')\n"
            "    print('Gemini Models\\tFive Hour Limit Remaining\\t80%\\t2026-11-25T14:28:48Z')\n"
            "    print('Claude and GPT models\\tWeekly Limit Remaining\\t100%\\t2026-11-30T19:55:32Z')\n"
            "elif email == 'beta@example.test':\n"
            "    print('Gemini Models\\tWeekly Limit Remaining\\t50%\\t2026-11-30T20:09:03Z')\n"
            "    print('Gemini Models\\tFive Hour Limit Remaining\\t100%\\t2026-11-25T12:55:45Z')\n"
            "    print('Claude and GPT models\\tWeekly Limit Remaining\\t90%\\t2026-11-27T02:47:42Z')\n"
            "else:\n"
            "    sys.exit(2)\n",
            encoding="utf-8",
        )
        mock_agy.chmod(0o755)
        self.env["DELEGATE_AGY_BIN"] = str(mock_agy)

        res = self.cli("agy", "account", "list", "--quota", "--json")
        self.assertEqual(res.returncode, 0, res.stderr)
        doc = json.loads(res.stdout)
        self.assertNotIn("duplicateQuotaGroups", doc)

        alpha = next(a for a in doc["accounts"] if a["id"] == "alpha")
        beta = next(a for a in doc["accounts"] if a["id"] == "beta")
        self.assertTrue(alpha["quota"]["ok"])
        self.assertTrue(beta["quota"]["ok"])
        self.assertEqual(alpha["quota"]["entries"][0]["remainingPct"], 93.0)
        self.assertEqual(alpha["quota"]["entries"][0]["resetAt"], "2026-11-30T20:03:02Z")
        self.assertEqual(beta["quota"]["entries"][0]["remainingPct"], 50.0)
        self.assertEqual(beta["quota"]["entries"][0]["resetAt"], "2026-11-30T20:09:03Z")

    def test_quota_identity_mismatch_refuses_persist_and_duplicate_entries_warn(self):
        self._seed_accounts()
        # 1. Seed a valid baseline quota fetch for both accounts.
        mock_good = self.work / "mock-agy-good"
        mock_good.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os\n"
            "from pathlib import Path\n"
            "home = Path(os.environ['HOME'])\n"
            "email = json.loads((home / '.gemini/antigravity-cli/antigravity-oauth-token').read_text())['id_token_email']\n"
            "if email == 'alpha@example.test':\n"
            "    print('Gemini Models\\tWeekly Limit Remaining\\t93%\\t2026-11-30T20:03:02Z')\n"
            "else:\n"
            "    print('Gemini Models\\tWeekly Limit Remaining\\t50%\\t2026-11-30T20:09:03Z')\n",
            encoding="utf-8",
        )
        mock_good.chmod(0o755)
        self.env["DELEGATE_AGY_BIN"] = str(mock_good)
        r0 = self.cli("agy", "account", "list", "--quota", "--json")
        self.assertEqual(r0.returncode, 0, r0.stderr)

        # 2. Simulate agy authenticating as beta while querying alpha (log + refreshed token mismatch).
        mock_wrong = self.work / "mock-agy-wrong-id"
        mock_wrong.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os\n"
            "from pathlib import Path\n"
            "home = Path(os.environ['HOME'])\n"
            "tok_path = home / '.gemini/antigravity-cli/antigravity-oauth-token'\n"
            "email = json.loads(tok_path.read_text())['id_token_email']\n"
            "log_dir = home / '.gemini/antigravity-cli/log'\n"
            "log_dir.mkdir(parents=True, exist_ok=True)\n"
            "if email == 'alpha@example.test':\n"
            "    # Pretend agy silently authenticated as beta and even rewrote the token!\n"
            "    (log_dir / 'cli-usage.log').write_text(\n"
            "        'I1125 19:02:06 server_oauth.go:226] OAuth: authenticated successfully as beta@example.test\\n'\n"
            "    )\n"
            "    tok_path.write_text(json.dumps({\n"
            "        'auth_method': 'consumer',\n"
            "        'id_token_email': 'beta@example.test',\n"
            "        'token': {'access_token': 'AT-WRONG', 'refresh_token': 'RT-WRONG', 'expiry': '2026-11-26T00:00:00Z'}\n"
            "    }))\n"
            "    print('Gemini Models\\tWeekly Limit Remaining\\t50%\\t2026-11-30T20:09:03Z')\n"
            "else:\n"
            "    print('Gemini Models\\tWeekly Limit Remaining\\t50%\\t2026-11-30T20:09:03Z')\n",
            encoding="utf-8",
        )
        mock_wrong.chmod(0o755)
        self.env["DELEGATE_AGY_BIN"] = str(mock_wrong)

        r1 = self.cli("agy", "account", "list", "--quota", "--json")
        self.assertEqual(r1.returncode, 0, r1.stderr)
        doc1 = json.loads(r1.stdout)
        alpha1 = next(a for a in doc1["accounts"] if a["id"] == "alpha")
        self.assertFalse(alpha1["quota"]["ok"])
        self.assertTrue(alpha1["quota"]["identityMismatch"])
        self.assertEqual(alpha1["quota"]["observedEmail"], "beta@example.test")
        # Prior 93% preserved, NEVER overwritten with beta's 50%!
        self.assertEqual(alpha1["quota"]["entries"][0]["remainingPct"], 93.0)
        # Mismatched quota must not be appended to history in agy-quota.json either!
        cache_path = Path(self.env["XDG_STATE_HOME"]) / "delegate/agy-quota.json"
        cache1 = json.loads(cache_path.read_text(encoding="utf-8"))
        self.assertEqual([pt["weekly"] for pt in cache1["accounts"]["alpha"]["history"]], [93.0])
        # Slot credential must NOT be overwritten with beta's token!
        alpha_slot_tok = json.loads(
            (self.fake_home / ".config/delegate/agy-accounts/alpha/antigravity-cli-oauth-token").read_text()
        )
        self.assertEqual(alpha_slot_tok["id_token_email"], "alpha@example.test")

        # Both `list` and `status` must mark 「身份不符，未更新」.
        list_txt = self.cli("agy", "account", "list").stdout
        self.assertIn("身份不符，未更新", list_txt)
        status_txt = self.cli("agy", "account", "status").stdout
        self.assertIn("身份不符，未更新", status_txt)

        # 3. With no prior cache and a corrupted slot credential email, mismatch must NOT store entries.
        cache_path.unlink()
        beta_slot_tok = self.fake_home / ".config/delegate/agy-accounts/beta/antigravity-cli-oauth-token"
        beta_slot_tok.write_text(
            json.dumps(
                {
                    "auth_method": "consumer",
                    "id_token_email": "alpha@example.test",
                    "token": {
                        "access_token": "AT-CONTAMINATED",
                        "refresh_token": "RT-CONTAMINATED",
                        "expiry": "2026-11-26T00:00:00Z",
                    },
                }
            ),
            encoding="utf-8",
        )
        self.env["DELEGATE_AGY_BIN"] = str(mock_good)
        r2 = self.cli("agy", "account", "list", "--quota", "--json")
        self.assertEqual(r2.returncode, 0, r2.stderr)
        doc2 = json.loads(r2.stdout)
        beta2 = next(a for a in doc2["accounts"] if a["id"] == "beta")
        self.assertFalse(beta2["quota"]["ok"])
        self.assertTrue(beta2["quota"]["identityMismatch"])
        self.assertEqual(beta2["quota"]["entries"], [])
        cache2 = json.loads(cache_path.read_text(encoding="utf-8"))
        self.assertEqual(cache2["accounts"]["beta"]["entries"], [])
        self.assertEqual(cache2["accounts"]["beta"]["history"], [])

        # 4. Duplicate entries across two slots (identical including resetAt) warn in list & status.
        dup_entries = [
            {
                "pool": "Gemini Models",
                "window": "weekly",
                "windowLabel": "Weekly Limit Remaining",
                "remainingPct": 50.0,
                "resetAt": "2026-11-30T20:09:03Z",
            },
            {
                "pool": "Claude and GPT models",
                "window": "weekly",
                "windowLabel": "Weekly Limit Remaining",
                "remainingPct": 90.0,
                "resetAt": "2026-11-27T02:47:42Z",
            },
        ]
        now_ep = int(time.time())
        dup_cache = {
            "schemaVersion": 1,
            "updatedAt": "2026-11-25T10:53:05Z",
            "updatedEpoch": now_ep,
            "accounts": {
                "alpha": {
                    "fetchedAt": "2026-11-25T10:53:05Z",
                    "fetchedEpoch": now_ep,
                    "ok": True,
                    "entries": dup_entries,
                    "unknownLines": [],
                    "history": [],
                },
                "beta": {
                    "fetchedAt": "2026-11-25T10:53:05Z",
                    "fetchedEpoch": now_ep,
                    "ok": True,
                    "entries": dup_entries,
                    "unknownLines": [],
                    "history": [],
                },
            },
        }
        cache_path.write_text(json.dumps(dup_cache), encoding="utf-8")

        list_dup = self.cli("agy", "account", "list").stdout
        status_dup = self.cli("agy", "account", "status").stdout
        self.assertIn("warning:", list_dup)
        self.assertIn("alpha, beta", list_dup)
        self.assertIn("完全相同", list_dup)
        self.assertIn("warning:", status_dup)
        self.assertIn("alpha, beta", status_dup)
        self.assertIn("完全相同", status_dup)

        # Changing only resetAt on beta clears the duplicate warning.
        dup_cache["accounts"]["beta"]["entries"] = [
            {**dup_entries[0], "resetAt": "2026-11-30T20:03:02Z"},
            dup_entries[1],
        ]
        cache_path.write_text(json.dumps(dup_cache), encoding="utf-8")
        self.assertNotIn("完全相同", self.cli("agy", "account", "list").stdout)
        self.assertNotIn("完全相同", self.cli("agy", "account", "status").stdout)


if __name__ == "__main__":
    unittest.main()
