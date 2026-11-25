"""6.38.0 tests: local agy multi-account management (replaces agy-swap).

All tests run in isolated temporary HOME directories with synthetic test emails
(`alpha@example.test`, `beta@example.test`, `gamma@example.test`) and fake
credential files; real user HOME and real ~/.local/bin/agy are never touched.
"""
import base64
import concurrent.futures
import fcntl
import json
import os
import signal
import stat
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


def _b64_vault_entry(email: str, access: str, refresh: str, expiry: str = "2026-11-24T05:00:00Z") -> str:
    payload = {
        "auth_method": "consumer",
        "id_token_email": email,
        "token": {
            "access_token": access,
            "expires_in": 3599,
            "expiry": expiry,
            "expiry_date": 1795463433526,
            "id_token": f"IDTOK-{access}",
            "refresh_token": refresh,
            "scope": "openid email",
            "token_type": "Bearer",
        },
    }
    raw = json.dumps(payload).encode("utf-8")
    return "go-keyring-base64:" + base64.b64encode(raw).decode("ascii")


def _write_live_creds(home: Path, email: str, access: str, refresh: str, scope: str | None = None) -> None:
    gemini = home / ".gemini"
    cli_dir = gemini / "antigravity-cli"
    ag_dir = gemini / "antigravity"
    cli_dir.mkdir(parents=True, exist_ok=True)
    ag_dir.mkdir(parents=True, exist_ok=True)

    tok_doc = json.dumps(
        {
            "auth_method": "consumer",
            "id_token_email": email,
            "token": {
                "access_token": access,
                "expires_in": 3599,
                "expiry": "2026-11-24T05:00:00Z",
                "refresh_token": refresh,
                "token_type": "Bearer",
            },
        },
        indent=2,
    )
    oauth_doc = json.dumps(
        {
            "access_token": access,
            "expiry_date": 0,
            "id_token": "",
            "refresh_token": refresh,
            "scope": "openid email",
            "token_type": "Bearer",
        },
        indent=2,
    )
    acct_doc = json.dumps({"active": email, "old": []}, indent=2)

    for path, content in [
        (gemini / "oauth_creds.json", oauth_doc),
        (gemini / "google_accounts.json", acct_doc),
        (gemini / "jetski-standalone-oauth-token", tok_doc),
        (cli_dir / "antigravity-oauth-token", tok_doc),
        (ag_dir / "antigravity-oauth-token", tok_doc),
    ]:
        path.write_text(content + "\n", encoding="utf-8")
        path.chmod(0o600)

    scope_path = home / ".config/antigravity/credential-user-scope"
    if scope is not None:
        scope_path.parent.mkdir(parents=True, exist_ok=True)
        scope_path.write_text(scope, encoding="utf-8")
        scope_path.chmod(0o600)
    elif scope_path.exists():
        scope_path.unlink()


class Delegate638AgyAccountsTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory(prefix="delegate-agy-acct-")
        self.home = Path(self._tmp.name) / "home"
        self.home.mkdir(mode=0o700)
        self.env = {
            **os.environ,
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / ".config"),
            "XDG_STATE_HOME": str(self.home / ".local/state"),
            "XDG_CACHE_HOME": str(self.home / ".cache"),
        }
        for k in ("DELEGATE_AGY_BIN", "DELEGATE_RUNS", "PI_DELEGATE_RUNS"):
            self.env.pop(k, None)

    def tearDown(self):
        self._tmp.cleanup()

    def run_cli(self, *args: str, env: dict | None = None) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(fixtures.DELEGATE), *args],
            cwd=self.home,
            env=env or self.env,
            capture_output=True,
            text=True,
            timeout=30,
        )

    def seed_agy_swap(self) -> tuple[str, str, str, str]:
        swap_dir = self.home / ".gemini/agy-swap"
        swap_dir.mkdir(parents=True, exist_ok=True)
        alpha_acc, alpha_ref = "SECRET-ACCESS-ALPHA-111", "SECRET-REFRESH-ALPHA-111"
        beta_acc, beta_ref = "SECRET-ACCESS-BETA-222", "SECRET-REFRESH-BETA-222"
        vault = {
            "account:alpha@example.test": _b64_vault_entry("alpha@example.test", alpha_acc, alpha_ref),
            "account:beta@example.test": _b64_vault_entry("beta@example.test", beta_acc, beta_ref),
        }
        accounts = {
            "alpha@example.test": {"email": "alpha@example.test", "name": "Alpha Primary"},
            "beta@example.test": {"email": "beta@example.test", "name": "Beta Secondary"},
        }
        (swap_dir / "vault.json").write_text(json.dumps(vault, indent=2), encoding="utf-8")
        (swap_dir / "accounts.json").write_text(json.dumps(accounts, indent=2), encoding="utf-8")
        _write_live_creds(
            self.home,
            "alpha@example.test",
            alpha_acc,
            alpha_ref,
            scope="scope-alpha-12345",
        )
        return alpha_acc, alpha_ref, beta_acc, beta_ref

    def assert_no_secrets(self, text: str, secrets: list[str]) -> None:
        for secret in secrets:
            self.assertNotIn(secret, text, f"secret leaked into output: {secret}")

    def test_import_agy_swap_dry_run_and_execute_permissions_and_no_token_leak(self):
        secrets = list(self.seed_agy_swap())
        store_root = self.home / ".config/delegate/agy-accounts"

        # 1. dry-run makes zero writes and prints no tokens
        dry = self.run_cli("agy", "account", "import-agy-swap", "--dry-run")
        self.assertEqual(dry.returncode, 0, dry.stderr)
        self.assertIn("alpha", dry.stdout)
        self.assertIn("beta", dry.stdout)
        self.assertFalse(store_root.exists(), "dry-run must not create store directory")
        self.assert_no_secrets(dry.stdout + dry.stderr, secrets)

        dry_json = self.run_cli("agy", "account", "import-agy-swap", "--dry-run", "--json")
        self.assertEqual(dry_json.returncode, 0, dry_json.stderr)
        self.assert_no_secrets(dry_json.stdout + dry_json.stderr, secrets)
        dj = json.loads(dry_json.stdout)
        self.assertTrue(dj["dryRun"])
        self.assertEqual(len(dj["slots"]), 2)

        # 2. real import creates 0700 dirs and 0600 files, marks live match active, never prints tokens
        imp = self.run_cli("agy", "account", "import-agy-swap", "--json")
        self.assertEqual(imp.returncode, 0, imp.stderr)
        self.assert_no_secrets(imp.stdout + imp.stderr, secrets)
        data = json.loads(imp.stdout)
        self.assertEqual(data["imported"], 2)
        self.assertEqual(data["active"], "alpha")

        self.assertEqual(stat.S_IMODE(store_root.stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE((store_root / "accounts.json").stat().st_mode), 0o600)

        for slot_name, expect_scope in [("alpha", True), ("beta", False)]:
            sdir = store_root / slot_name
            self.assertEqual(stat.S_IMODE(sdir.stat().st_mode), 0o700)
            for fname in (
                "oauth_creds.json",
                "google_accounts.json",
                "jetski-standalone-oauth-token",
                "antigravity-cli-oauth-token",
                "antigravity-oauth-token",
            ):
                fp = sdir / fname
                self.assertTrue(fp.is_file(), f"missing {fp}")
                self.assertEqual(stat.S_IMODE(fp.stat().st_mode), 0o600)
            self.assertEqual((sdir / "credential-user-scope").is_file(), expect_scope)
            if expect_scope:
                self.assertEqual(stat.S_IMODE((sdir / "credential-user-scope").stat().st_mode), 0o600)

        # Source agy-swap files remain completely untouched for rollback
        self.assertTrue((self.home / ".gemini/agy-swap/vault.json").is_file())

        # Re-importing without --overwrite skips both
        imp2 = self.run_cli("agy", "account", "import-agy-swap", "--json")
        self.assertEqual(imp2.returncode, 0, imp2.stderr)
        d2 = json.loads(imp2.stdout)
        self.assertEqual(d2["imported"], 0)
        self.assertEqual(d2["skipped"], 2)

        # list / status / snapshot --json never leak tokens
        for cmd in [
            ("agy", "account", "list"),
            ("agy", "account", "list", "--json"),
            ("agy", "account", "status"),
            ("agy", "account", "status", "--json"),
            ("snapshot", "--json"),
        ]:
            res = self.run_cli(*cmd)
            self.assertEqual(res.returncode, 0, f"{cmd}: {res.stderr}")
            self.assert_no_secrets(res.stdout + res.stderr, secrets)

        snap = json.loads(self.run_cli("snapshot", "--json").stdout)
        self.assertEqual(snap["agyAccounts"]["active"], "alpha")
        self.assertEqual(len(snap["agyAccounts"]["accounts"]), 2)

    def test_add_runs_isolated_temp_home_without_disturbing_active_account(self):
        secrets = list(self.seed_agy_swap())
        self.assertEqual(self.run_cli("agy", "account", "import-agy-swap").returncode, 0)

        marker = self.home / ".gemini/antigravity-cli/cache/antigravity-keyring-unavailable"
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text("unavailable\n", encoding="utf-8")

        fake_agy = Path(self._tmp.name) / "fake-agy-login"
        home_log = Path(self._tmp.name) / "temp-home-seen.txt"
        gamma_acc, gamma_ref = "SECRET-ACCESS-GAMMA-333", "SECRET-REFRESH-GAMMA-333"
        fake_agy.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, pathlib, sys\n"
            "h = pathlib.Path(os.environ['HOME'])\n"
            f"pathlib.Path({str(home_log)!r}).write_text(str(h))\n"
            "assert not (h / '.gemini/oauth_creds.json').exists(), 'temp HOME leaked live creds'\n"
            "assert (h / '.gemini/antigravity-cli/cache/antigravity-keyring-unavailable').is_file()\n"
            "(h / '.gemini/antigravity-cli').mkdir(parents=True, exist_ok=True)\n"
            "(h / '.gemini/antigravity').mkdir(parents=True, exist_ok=True)\n"
            "(h / '.config/antigravity').mkdir(parents=True, exist_ok=True)\n"
            f"tok = {{'auth_method':'consumer','id_token_email':'gamma@example.test','token':{{'access_token':{gamma_acc!r},'refresh_token':{gamma_ref!r},'expiry':'2026-11-24T09:00:00Z','token_type':'Bearer'}}}}\n"
            "(h / '.gemini/jetski-standalone-oauth-token').write_text(json.dumps(tok))\n"
            "(h / '.gemini/antigravity-cli/antigravity-oauth-token').write_text(json.dumps(tok))\n"
            "(h / '.gemini/antigravity/antigravity-oauth-token').write_text(json.dumps(tok))\n"
            f"(h / '.gemini/oauth_creds.json').write_text(json.dumps({{'access_token':{gamma_acc!r},'refresh_token':{gamma_ref!r},'token_type':'Bearer'}}))\n"
            "(h / '.gemini/google_accounts.json').write_text(json.dumps({'active':'gamma@example.test','old':[]}))\n"
            "(h / '.config/antigravity/credential-user-scope').write_text('scope-gamma-99999')\n",
            encoding="utf-8",
        )
        fake_agy.chmod(0o755)

        add = self.run_cli(
            "agy", "account", "add", "gamma", "--label", "third", "--agy-bin", str(fake_agy), "--json"
        )
        self.assertEqual(add.returncode, 0, add.stderr)
        self.assert_no_secrets(add.stdout + add.stderr, secrets + [gamma_acc, gamma_ref])
        out = json.loads(add.stdout)
        self.assertEqual(out["id"], "gamma")
        self.assertEqual(out["email"], "gamma@example.test")
        self.assertEqual(out["active"], "alpha")
        self.assertEqual(out["fileCount"], 6)

        # Temp HOME was distinct from real HOME and cleaned up afterwards
        seen_temp = Path(home_log.read_text().strip())
        self.assertNotEqual(seen_temp, self.home)
        self.assertFalse(seen_temp.exists(), "temporary login HOME must be removed on exit")

        # Live active credentials remained untouched (still alpha)
        live_acct = json.loads((self.home / ".gemini/google_accounts.json").read_text())
        self.assertEqual(live_acct["active"], "alpha@example.test")
        live_oauth = json.loads((self.home / ".gemini/oauth_creds.json").read_text())
        self.assertEqual(live_oauth["access_token"], secrets[0])

    def test_use_saves_back_refreshed_tokens_and_clears_stale_scope(self):
        self.seed_agy_swap()
        self.assertEqual(self.run_cli("agy", "account", "import-agy-swap").returncode, 0)

        # Simulate agy refreshing alpha's tokens while active
        refreshed_alpha_acc = "SECRET-ACCESS-ALPHA-REFRESHED-V2"
        refreshed_alpha_ref = "SECRET-REFRESH-ALPHA-REFRESHED-V2"
        _write_live_creds(
            self.home,
            "alpha@example.test",
            refreshed_alpha_acc,
            refreshed_alpha_ref,
            scope="scope-alpha-refreshed",
        )

        # Switch alpha -> beta
        sw1 = self.run_cli("agy", "account", "use", "beta", "--json")
        self.assertEqual(sw1.returncode, 0, sw1.stderr)
        self.assert_no_secrets(sw1.stdout + sw1.stderr, [refreshed_alpha_acc, refreshed_alpha_ref])
        d1 = json.loads(sw1.stdout)
        self.assertTrue(d1["ok"])
        self.assertEqual(d1["active"], "beta")
        self.assertEqual(d1["savedBackSlot"], "alpha")

        # Live HOME now has beta's creds (0600) and NO leftover alpha scope file
        self.assertEqual(
            json.loads((self.home / ".gemini/google_accounts.json").read_text())["active"],
            "beta@example.test",
        )
        self.assertEqual(
            json.loads((self.home / ".gemini/oauth_creds.json").read_text())["access_token"],
            "SECRET-ACCESS-BETA-222",
        )
        self.assertEqual(
            stat.S_IMODE((self.home / ".gemini/oauth_creds.json").stat().st_mode), 0o600
        )
        self.assertFalse(
            (self.home / ".config/antigravity/credential-user-scope").exists(),
            "previous account's credential-user-scope must not bleed into beta",
        )

        # Alpha's slot captured the refreshed tokens!
        slot_alpha = self.home / ".config/delegate/agy-accounts/alpha"
        self.assertEqual(
            json.loads((slot_alpha / "oauth_creds.json").read_text())["access_token"],
            refreshed_alpha_acc,
        )
        self.assertEqual(
            json.loads((slot_alpha / "jetski-standalone-oauth-token").read_text())["token"][
                "refresh_token"
            ],
            refreshed_alpha_ref,
        )

        # Simulate agy refreshing beta's tokens, then switch back beta -> alpha
        refreshed_beta_acc = "SECRET-ACCESS-BETA-REFRESHED-V2"
        _write_live_creds(self.home, "beta@example.test", refreshed_beta_acc, "SECRET-REFRESH-BETA-V2")
        sw2 = self.run_cli("agy", "account", "use", "alpha", "--json")
        self.assertEqual(sw2.returncode, 0, sw2.stderr)

        # Alpha's refreshed tokens + scope are restored live, and beta's refreshed token saved back!
        self.assertEqual(
            json.loads((self.home / ".gemini/oauth_creds.json").read_text())["access_token"],
            refreshed_alpha_acc,
        )
        self.assertEqual(
            (self.home / ".config/antigravity/credential-user-scope").read_text(),
            "scope-alpha-refreshed",
        )
        slot_beta = self.home / ".config/delegate/agy-accounts/beta"
        self.assertEqual(
            json.loads((slot_beta / "oauth_creds.json").read_text())["access_token"],
            refreshed_beta_acc,
        )

    def test_use_succeeds_while_agy_running_and_ignores_other_home(self):
        self.seed_agy_swap()
        self.assertEqual(self.run_cli("agy", "account", "import-agy-swap").returncode, 0)

        fake_bin_dir = Path(self._tmp.name) / "bin"
        fake_bin_dir.mkdir()
        fake_agy = fake_bin_dir / "agy"
        # Do NOT `exec sleep`, otherwise cmdline/exe becomes `sleep`; keep `sh .../agy` alive
        # just like a real agy wrapper or binary.
        fake_agy.write_text('#!/bin/sh\n[ -n "${READY_FILE:-}" ] && : > "$READY_FILE"\nsleep 30\n', encoding="utf-8")
        fake_agy.chmod(0o755)

        other_home = Path(self._tmp.name) / "other-home"
        other_home.mkdir()
        other_ready = Path(self._tmp.name) / "other-agy.ready"
        same_ready = Path(self._tmp.name) / "same-agy.ready"

        other_proc = subprocess.Popen(
            [str(fake_agy), "--model", "gemini-4-neon"],
            env={**self.env, "HOME": str(other_home), "READY_FILE": str(other_ready)},
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        same_proc = None
        try:
            fixtures.wait_until(other_ready.is_file, "other-home fake agy started")
            # Process in another HOME is ignored (runningAgyCount == 0)
            ok_sw = self.run_cli("agy", "account", "use", "beta", "--json")
            self.assertEqual(ok_sw.returncode, 0, ok_sw.stderr)
            d0 = json.loads(ok_sw.stdout)
            self.assertTrue(d0["ok"])
            self.assertEqual(d0["active"], "beta")
            self.assertEqual(d0["runningAgyCount"], 0)

            # Process in self.home no longer blocks switching: `use` succeeds and reports the
            # running process(es) that will finish on their original account.
            same_proc = subprocess.Popen(
                [str(fake_agy), "--model", "gemini-4-neon"],
                env={
                    **self.env,
                    "DELEGATE_RUN_DIR": "/tmp/fake/20261124-030000-testrun",
                    "READY_FILE": str(same_ready),
                },
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                start_new_session=True,
            )
            fixtures.wait_until(same_ready.is_file, "same-home fake agy started")

            sw_human = self.run_cli("agy", "account", "use", "alpha")
            self.assertEqual(sw_human.returncode, 0, sw_human.stderr)
            self.assertIn("switched agy account to alpha", sw_human.stdout)
            self.assertIn("original account", sw_human.stdout)
            self.assertIn(str(same_proc.pid), sw_human.stdout)
            self.assertIn("20261124-030000-testrun", sw_human.stdout)

            sw_json = self.run_cli("agy", "account", "use", "beta", "--json")
            self.assertEqual(sw_json.returncode, 0, sw_json.stderr)
            sj = json.loads(sw_json.stdout)
            self.assertTrue(sj["ok"])
            self.assertEqual(sj["active"], "beta")
            self.assertEqual(sj["runningAgyCount"], 1)
            pids = [p["pid"] for p in sj["runningAgy"]]
            self.assertIn(same_proc.pid, pids)

            # Live credentials switched to beta immediately for subsequent runs
            self.assertEqual(
                json.loads((self.home / ".gemini/google_accounts.json").read_text())["active"],
                "beta@example.test",
            )
        finally:
            for p in (same_proc, other_proc):
                if p is not None and p.poll() is None:
                    try:
                        os.killpg(p.pid, signal.SIGTERM)
                    except ProcessLookupError:
                        pass
                    try:
                        p.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        try:
                            os.killpg(p.pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                        p.wait(timeout=5)

    def test_live_switch_during_delegated_agy_run_preserves_new_live_creds_and_saves_back_old_slot(self):
        self.seed_agy_swap()
        self.assertEqual(self.run_cli("agy", "account", "import-agy-swap").returncode, 0)

        # Create a minimal git repo for delegate runs
        repo_dir = Path(self._tmp.name) / "repo"
        repo_dir.mkdir()
        subprocess.run(["git", "init", "-q"], cwd=repo_dir, check=True)
        subprocess.run(["git", "config", "user.email", "t@example.test"], cwd=repo_dir, check=True)
        subprocess.run(["git", "config", "user.name", "Test"], cwd=repo_dir, check=True)
        (repo_dir / "README.md").write_text("hello\n", encoding="utf-8")
        subprocess.run(["git", "add", "README.md"], cwd=repo_dir, check=True)
        subprocess.run(["git", "commit", "-qm", "init"], cwd=repo_dir, check=True)

        # Configure agy model in isolated user config
        cfg_dir = self.home / ".config/delegate"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "config.json").write_text(
            json.dumps(
                {
                    "models": {"neon": "agy/gemini-4-neon"},
                    "tiers": {"scout": "neon", "review": "neon", "write": "neon"},
                }
            ),
            encoding="utf-8",
        )

        # Install a fake `agy` binary on PATH that:
        # - Call 1: records seen overlay HOME/account/creds, writes READY, blocks on FIFO,
        #   then writes a refreshed alpha token into its overlay HOME and exits.
        # - Call 2: records seen overlay HOME/account/creds and exits immediately.
        fake_bin_dir = Path(self._tmp.name) / "agy-bin"
        fake_bin_dir.mkdir()
        call_counter = Path(self._tmp.name) / "agy-call-count.txt"
        run1_ready = Path(self._tmp.name) / "run1-ready.json"
        run2_seen = Path(self._tmp.name) / "run2-seen.json"
        gate_fifo = Path(self._tmp.name) / "run1-gate.fifo"
        os.mkfifo(gate_fifo)
        refreshed_alpha_token = "SECRET-ACCESS-ALPHA-OVERLAY-V3"

        fake_agy_helper = Path(self._tmp.name) / "agy-helper.py"
        fake_agy_helper.write_text(
            "import json, os, pathlib, sys\n"
            "argv = sys.argv[1:]\n"
            "if '--version' in argv:\n"
            "    print('1.6.0')\n"
            "    sys.exit(0)\n"
            f"counter = pathlib.Path({str(call_counter)!r})\n"
            "idx = int(counter.read_text()) + 1 if counter.exists() else 1\n"
            "counter.write_text(str(idx))\n"
            "_ = sys.stdin.read()\n"
            "h = pathlib.Path(os.environ['HOME'])\n"
            "tok_path = h / '.gemini/antigravity-cli/antigravity-oauth-token'\n"
            "tok_doc = json.loads(tok_path.read_text())\n"
            "info = {\n"
            "    'home': str(h),\n"
            "    'real_home': os.environ.get('DELEGATE_AGY_REAL_HOME', ''),\n"
            "    'account_env': os.environ.get('DELEGATE_AGY_ACCOUNT', ''),\n"
            "    'email': tok_doc.get('id_token_email', ''),\n"
            "    'access_token': tok_doc.get('token', {}).get('access_token', ''),\n"
            "    'tok_is_symlink': tok_path.is_symlink(),\n"
            "}\n"
            "if idx == 1:\n"
            f"    ready_tmp = pathlib.Path({str(run1_ready)!r} + '.tmp')\n"
            "    ready_tmp.write_text(json.dumps(info))\n"
            f"    os.replace(ready_tmp, {str(run1_ready)!r})\n"
            f"    with open({str(gate_fifo)!r}, 'r') as gf:\n"
            "        gf.read()\n"
            "    # Simulate agy refreshing its OAuth token mid-run and writing to its HOME\n"
            "    tok_doc['token']['access_token'] = " + repr(refreshed_alpha_token) + "\n"
            "    tok_doc['token']['expiry'] = '2026-11-25T15:00:00Z'\n"
            "    tok_path.write_text(json.dumps(tok_doc))\n"
            "    cid = 'conv-run-1'\n"
            "else:\n"
            f"    s_tmp = pathlib.Path({str(run2_seen)!r} + '.tmp')\n"
            "    s_tmp.write_text(json.dumps(info))\n"
            f"    os.replace(s_tmp, {str(run2_seen)!r})\n"
            "    cid = 'conv-run-2'\n"
            "print(json.dumps({'event': 'init', 'conversation_id': cid}), flush=True)\n"
            "print(json.dumps({'event': 'step_update', 'step_update': {'step_index': 1, 'step_type': 'agent_response', 'state': 'DONE', 'text_delta': 'ok'}}), flush=True)\n"
            "print(json.dumps({'event': 'result', 'result': {'conversation_id': cid, 'status': 'SUCCESS', 'response': 'ok', 'usage': {'input_tokens': 150, 'output_tokens': 30, 'cache_read_tokens': 2000}}}), flush=True)\n",
            encoding="utf-8",
        )
        fake_agy = fake_bin_dir / "agy"
        # Keep `/bin/sh .../agy` alive while python runs (matching real wrapper/binary detection).
        fake_agy.write_text(
            f'#!/bin/sh\npython3 "{fake_agy_helper}" "$@"\n',
            encoding="utf-8",
        )
        fake_agy.chmod(0o755)

        run_env = {
            **self.env,
            "PATH": f"{fake_bin_dir}:{self.env.get('PATH', '')}",
        }
        for k in (
            "DELEGATE_SCOUT_MODEL",
            "DELEGATE_REVIEW_MODEL",
            "DELEGATE_WRITE_MODEL",
            "DELEGATE_AGENT",
            "DELEGATE_RUN_DIR",
            "PI_DELEGATE_ACTIVE",
            "DELEGATE_CALLER",
            "ANTIGRAVITY_CONVERSATION_ID",
            "ANTIGRAVITY_AGENT",
        ):
            run_env.pop(k, None)

        # 1. Start delegated run 1 while alpha is active
        st1 = subprocess.run(
            [str(fixtures.DELEGATE), "start", "--read-only", "--tier", "scout", "--json", "probe alpha"],
            cwd=repo_dir,
            env=run_env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(st1.returncode, 0, st1.stderr)
        run1_id = json.loads(st1.stdout)["run"]

        released = False
        try:
            fixtures.wait_until(run1_ready.is_file, "run 1 fake agy ready", timeout=15)
            info1 = json.loads(run1_ready.read_text(encoding="utf-8"))
            self.assertEqual(info1["email"], "alpha@example.test")
            self.assertEqual(info1["account_env"], "alpha")
            self.assertFalse(info1["tok_is_symlink"], "overlay token file must be a private copy")
            self.assertNotEqual(Path(info1["home"]).resolve(), self.home.resolve())
            self.assertEqual(Path(info1["real_home"]).resolve(), self.home.resolve())

            # 2. `status --json` detects the overlay-wrapped delegated agy process
            status_res = self.run_cli("agy", "account", "status", "--json", env=run_env)
            self.assertEqual(status_res.returncode, 0, status_res.stderr)
            st_doc = json.loads(status_res.stdout)
            running_runs = [p.get("run") for p in st_doc["runningAgy"]]
            self.assertIn(run1_id, running_runs, f"expected {run1_id} in {st_doc['runningAgy']}")

            # 3. Switch account to beta WHILE run 1 is still running!
            sw = self.run_cli("agy", "account", "use", "beta", "--json", env=run_env)
            self.assertEqual(sw.returncode, 0, sw.stderr)
            sw_doc = json.loads(sw.stdout)
            self.assertTrue(sw_doc["ok"])
            self.assertEqual(sw_doc["active"], "beta")
            self.assertGreaterEqual(sw_doc["runningAgyCount"], 1)

            # Live ~/.gemini is now beta
            self.assertEqual(
                json.loads((self.home / ".gemini/antigravity-cli/antigravity-oauth-token").read_text())["id_token_email"],
                "beta@example.test",
            )

            # 4. Release run 1 so it refreshes alpha's token inside its overlay and exits
            with open(gate_fifo, "w", encoding="utf-8") as gf:
                gf.write("go\n")
            released = True
        finally:
            if not released:
                try:
                    fd = os.open(gate_fifo, os.O_WRONLY | os.O_NONBLOCK)
                    os.close(fd)
                except OSError:
                    pass

        w1 = subprocess.run(
            [str(fixtures.DELEGATE), "wait", run1_id],
            cwd=repo_dir,
            env=run_env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(w1.returncode, 0, w1.stderr)

        # 5. Run 1 exited: live ~/.gemini must STILL be beta (never clobbered by alpha!)
        live_cli_tok = json.loads(
            (self.home / ".gemini/antigravity-cli/antigravity-oauth-token").read_text(encoding="utf-8")
        )
        self.assertEqual(live_cli_tok["id_token_email"], "beta@example.test")
        self.assertEqual(live_cli_tok["token"]["access_token"], "SECRET-ACCESS-BETA-222")

        # Alpha's slot received the refreshed token written by run 1 (mode 0600)!
        alpha_slot_tok_path = self.home / ".config/delegate/agy-accounts/alpha/antigravity-cli-oauth-token"
        alpha_slot_tok = json.loads(alpha_slot_tok_path.read_text(encoding="utf-8"))
        self.assertEqual(alpha_slot_tok["token"]["access_token"], refreshed_alpha_token)
        self.assertEqual(stat.S_IMODE(alpha_slot_tok_path.stat().st_mode), 0o600)

        # Beta's slot is untouched
        beta_slot_tok = json.loads(
            (self.home / ".config/delegate/agy-accounts/beta/antigravity-cli-oauth-token").read_text(encoding="utf-8")
        )
        self.assertEqual(beta_slot_tok["token"]["access_token"], "SECRET-ACCESS-BETA-222")

        # Run 1's overlay temp dir was cleaned up
        self.assertFalse(Path(info1["home"]).exists(), "overlay temp HOME must be removed on exit")

        # 6. Start delegated run 2 -> picks up newly active beta account
        st2 = subprocess.run(
            [str(fixtures.DELEGATE), "start", "--read-only", "--tier", "scout", "--json", "probe beta"],
            cwd=repo_dir,
            env=run_env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(st2.returncode, 0, st2.stderr)
        run2_id = json.loads(st2.stdout)["run"]
        w2 = subprocess.run(
            [str(fixtures.DELEGATE), "wait", run2_id],
            cwd=repo_dir,
            env=run_env,
            capture_output=True,
            text=True,
            timeout=30,
        )
        self.assertEqual(w2.returncode, 0, w2.stderr)

        info2 = json.loads(run2_seen.read_text(encoding="utf-8"))
        self.assertEqual(info2["email"], "beta@example.test")
        self.assertEqual(info2["account_env"], "beta")
        self.assertEqual(info2["access_token"], "SECRET-ACCESS-BETA-222")

        # 7. agy-usage.jsonl recorded launch-time accounts (run 1 -> alpha, run 2 -> beta)
        ledger = self.home / ".local/state/delegate/agy-usage.jsonl"
        self.assertTrue(ledger.is_file(), "expected agy-usage.jsonl")
        by_run = {
            rec["run"]: rec
            for rec in (json.loads(line) for line in ledger.read_text(encoding="utf-8").splitlines() if line.strip())
        }
        self.assertEqual(by_run[run1_id]["account"], "alpha")
        self.assertEqual(by_run[run2_id]["account"], "beta")

    def test_use_concurrent_lock_serializes_switches(self):
        self.seed_agy_swap()
        self.assertEqual(self.run_cli("agy", "account", "import-agy-swap").returncode, 0)

        lock_path = self.home / ".config/delegate/agy-accounts/.lock"
        lock_target = str(lock_path.resolve())
        lock_file = open(lock_path, "r+b")
        fcntl.flock(lock_file.fileno(), fcntl.LOCK_EX)
        proc = subprocess.Popen(
            [str(fixtures.DELEGATE), "agy", "account", "use", "beta", "--json"],
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            def blocked_on_lock():
                if proc.poll() is not None:
                    raise AssertionError(
                        f"use exited (rc={proc.returncode}) without blocking on .lock"
                    )
                wchan = Path(f"/proc/{proc.pid}/wchan").read_text().strip()
                if "lock" not in wchan:
                    return False
                for fd in Path(f"/proc/{proc.pid}/fd").iterdir():
                    try:
                        if os.readlink(fd) == lock_target:
                            return True
                    except OSError:
                        pass
                return False

            fixtures.wait_until(blocked_on_lock, "use blocked on .lock", timeout=10, ignore=(OSError,))
            self.assertIsNone(proc.poll(), "use must stay blocked while .lock is held")
            self.assertEqual(
                json.loads((self.home / ".config/delegate/agy-accounts/accounts.json").read_text())["active"],
                "alpha",
            )
        finally:
            fcntl.flock(lock_file.fileno(), fcntl.LOCK_UN)
            lock_file.close()

        stdout, stderr = proc.communicate(timeout=10)
        self.assertEqual(proc.returncode, 0, stderr)
        self.assertEqual(json.loads(stdout)["active"], "beta")

        # Concurrent switches stay consistent (no torn state between accounts.json and live files)
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as ex:
            futs = [
                ex.submit(self.run_cli, "agy", "account", "use", target, "--json")
                for target in ("alpha", "beta", "alpha", "beta")
            ]
            for fut in futs:
                r = fut.result()
                self.assertEqual(r.returncode, 0, r.stderr)

        state = json.loads((self.home / ".config/delegate/agy-accounts/accounts.json").read_text())
        active_id = state["active"]
        expected_email = state["accounts"][active_id]["email"]
        live_email = json.loads((self.home / ".gemini/google_accounts.json").read_text())["active"]
        live_tok_email = json.loads(
            (self.home / ".gemini/jetski-standalone-oauth-token").read_text()
        )["id_token_email"]
        self.assertEqual(live_email, expected_email)
        self.assertEqual(live_tok_email, expected_email)

    def test_use_fail_closed_on_untracked_live_and_remove_active_guard(self):
        self.seed_agy_swap()
        self.assertEqual(self.run_cli("agy", "account", "import-agy-swap").returncode, 0)

        # Replace live HOME with an untracked third account -> refuse without --force
        _write_live_creds(self.home, "stranger@example.test", "STRANGER-ACC", "STRANGER-REF")
        refused = self.run_cli("agy", "account", "use", "beta", "--json")
        self.assertEqual(refused.returncode, 1)
        self.assertEqual(json.loads(refused.stdout)["error"], "untracked_live_credentials")

        forced = self.run_cli("agy", "account", "use", "beta", "--force", "--json")
        self.assertEqual(forced.returncode, 0, forced.stderr)
        self.assertEqual(json.loads(forced.stdout)["active"], "beta")

        # Removing active account refuses without --force
        rm_bad = self.run_cli("agy", "account", "remove", "beta")
        self.assertEqual(rm_bad.returncode, 2)
        self.assertIn("active", rm_bad.stderr)

        rm_ok = self.run_cli("agy", "account", "remove", "beta", "--force", "--json")
        self.assertEqual(rm_ok.returncode, 0, rm_ok.stderr)
        self.assertFalse((self.home / ".config/delegate/agy-accounts/beta").exists())
        # Live credentials untouched by remove
        self.assertTrue((self.home / ".gemini/oauth_creds.json").is_file())

    def test_restore_entry_dry_run_wrapper_and_symlink(self):
        libexec = self.home / ".local/libexec"
        bindir = self.home / ".local/bin"
        libexec.mkdir(parents=True)
        bindir.mkdir(parents=True)

        real_bin = libexec / "agy"
        real_bin.write_text('#!/bin/sh\necho "REAL-AGY:$*"\n', encoding="utf-8")
        real_bin.chmod(0o755)

        entry = bindir / "agy"
        orig_wrapper = "#!/bin/sh\n# Managed by agy-swap session guard\nexec agy-swap\n"
        entry.write_text(orig_wrapper, encoding="utf-8")
        entry.chmod(0o755)

        # 1. dry-run leaves entry untouched
        dry = self.run_cli("agy", "account", "restore-entry", "--dry-run", "--json")
        self.assertEqual(dry.returncode, 0, dry.stderr)
        dj = json.loads(dry.stdout)
        self.assertTrue(dj["dryRun"])
        self.assertEqual(dj["currentState"], "agy-swap wrapper")
        self.assertEqual(entry.read_text(), orig_wrapper)

        # 2. default wrapper mode replaces entry and backs up original
        res = self.run_cli("agy", "account", "restore-entry", "--json")
        self.assertEqual(res.returncode, 0, res.stderr)
        rj = json.loads(res.stdout)
        self.assertTrue(rj["ok"])
        self.assertTrue(rj["backedUp"])
        backup = bindir / "agy.bak-agy-swap"
        self.assertEqual(backup.read_text(), orig_wrapper)
        self.assertNotIn("agy-swap --sync-session", entry.read_text())
        self.assertIn('exec "$HOME/.local/libexec/agy" "$@"', entry.read_text())

        run_out = subprocess.run(
            [str(entry), "hello"], env=self.env, capture_output=True, text=True, timeout=10
        )
        self.assertEqual(run_out.returncode, 0, run_out.stderr)
        self.assertEqual(run_out.stdout.strip(), "REAL-AGY:hello")

        # 3. symlink mode replaces entry with direct symlink and preserves original backup
        res2 = self.run_cli("agy", "account", "restore-entry", "--mode", "symlink", "--json")
        self.assertEqual(res2.returncode, 0, res2.stderr)
        self.assertTrue(entry.is_symlink())
        self.assertEqual(entry.resolve(), real_bin.resolve())
        self.assertEqual(backup.read_text(), orig_wrapper)


if __name__ == "__main__":
    unittest.main()
