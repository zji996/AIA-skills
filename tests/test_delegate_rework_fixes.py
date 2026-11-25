"""Regressions for rework-analysis tool-side fixes (items 1, 3, 4, 5, 6):
1. Concurrent write hotspot / version / migration collision hints across running & unapplied runs
   (single stderr line with --after <run> suggestion, non-blocking).
3. Review `original_task_brief()` concatenates the full reply brief chain (including archived
   ancestors) so reviewers judge by the latest decisions (reproducing the `c9cb` incident).
4. `agy` OAuth2 401 (`UNAUTHENTICATED (code 401)` / `Expected OAuth 2 access token`) backs off
   and re-probes once; still failing marks permanent failure with `agy 登录失效，`delegate agy account status``.
5. Usage口径: `tokensDelta` propagated into `history`, `show`, and `card` (and multi-hop ancestor
   lookup across failed intermediate replies).
6. Archive size cap removed (apply/clean preserves all files without skipping); `status` warns
   when total archive size exceeds threshold.
"""
import json
import os
import shutil
import subprocess
import sys
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateReworkFixesTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def write_agy_config(self):
        for k in ("DELEGATE_SCOUT_MODEL", "DELEGATE_REVIEW_MODEL", "DELEGATE_WRITE_MODEL"):
            self.env.pop(k, None)
        cfg = self.work / "config/delegate/config.json"
        cfg.parent.mkdir(parents=True, exist_ok=True)
        cfg.write_text(
            json.dumps(
                {
                    "models": {
                        "agy-m": {"model": "agy/gemini-4-neon", "thinking": "high"},
                    },
                    "tiers": {"scout": "agy-m", "review": "agy-m", "write": "agy-m"},
                }
            ),
            encoding="utf-8",
        )

    def install_scripted_agy(self, script_body: str) -> Path:
        log_file = self.work / "scripted-agy-log.jsonl"
        self.env["SCRIPTED_AGY_LOG"] = str(log_file)
        agy = self.bin / "agy"
        agy.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "args = sys.argv[1:]\n"
            "if '--version' in args:\n"
            "    print('1.5.15')\n"
            "    sys.exit(0)\n"
            "raw = sys.stdin.readline()\n"
            "msg = json.loads(raw.strip().splitlines()[-1]) if raw.strip() else {}\n"
            "content = ''\n"
            "if isinstance(msg, dict) and isinstance(msg.get('message'), dict):\n"
            "    content = str(msg['message'].get('content', ''))\n"
            "is_probe = ('只回复 ok' in content) or (len(content) < 40 and 'ok' in content.lower())\n"
            "log_path = Path(os.environ['SCRIPTED_AGY_LOG'])\n"
            "prev = log_path.read_text().splitlines() if log_path.is_file() else []\n"
            "call_idx = len(prev) + 1\n"
            "with log_path.open('a') as fh:\n"
            "    fh.write(json.dumps({'call': call_idx, 'args': args, 'is_probe': is_probe, 'content': content}, ensure_ascii=False) + '\\n')\n"
            "def emit_agy(text, inp=100, out=20, cache=1000, cid='conv-1'):\n"
            "    print(json.dumps({'event': 'init', 'conversation_id': cid}), flush=True)\n"
            "    print(json.dumps({'event': 'step_update', 'step_update': {'step_index': 1, 'step_type': 'agent_response', 'state': 'DONE', 'text_delta': text}}), flush=True)\n"
            "    print(json.dumps({'event': 'result', 'result': {'conversation_id': cid, 'status': 'SUCCESS', 'response': text, 'usage': {'input_tokens': inp, 'output_tokens': out, 'cache_read_tokens': cache}}}), flush=True)\n"
            + script_body
            + "\n",
            encoding="utf-8",
        )
        agy.chmod(0o755)
        self.env["PATH"] = f"{agy.parent}:{self.env.get('PATH', '')}"
        return log_file

    def install_scripted_pi(self, script_body: str) -> Path:
        log_file = self.work / "scripted-pi-rework.jsonl"
        self.env["SCRIPTED_PI_LOG"] = str(log_file)
        pi = self.bin / "pi"
        pi.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "args = sys.argv[1:]\n"
            "if args == ['--version']:\n"
            "    print('fake-pi 1.0')\n"
            "    sys.exit(0)\n"
            "if len(args) >= 2 and args[1] == '--list-models':\n"
            "    print('provider model context max-out thinking images')\n"
            "    print('p m 1M 1K yes yes')\n"
            "    print('p review-m 1M 1K yes yes')\n"
            "    print('p write-m 1M 1K yes yes')\n"
            "    sys.exit(0)\n"
            "stdin_text = sys.stdin.read()\n"
            "sdir = ''\n"
            "sid = ''\n"
            "for i, a in enumerate(args):\n"
            "    if a == '--session-dir' and i + 1 < len(args): sdir = args[i + 1]\n"
            "    elif a == '--session-id' and i + 1 < len(args): sid = args[i + 1]\n"
            "if sdir:\n"
            "    Path(sdir).mkdir(parents=True, exist_ok=True)\n"
            "    (Path(sdir) / f't_{sid or os.getpid()}.jsonl').write_text('{}\\n')\n"
            "log_path = Path(os.environ['SCRIPTED_PI_LOG'])\n"
            "prev = log_path.read_text().splitlines() if log_path.is_file() else []\n"
            "call_idx = len(prev) + 1\n"
            "with log_path.open('a') as fh:\n"
            "    fh.write(json.dumps({'call': call_idx, 'args': args, 'stdin': stdin_text}, ensure_ascii=False) + '\\n')\n"
            "def emit_answer(text):\n"
            "    print(json.dumps({'type': 'message_end', 'message': {'role': 'assistant', 'stopReason': 'stop', 'model': 'p/m', 'usage': {'input': 10, 'output': 5, 'cacheRead': 0}, 'content': [{'type': 'text', 'text': text}]}}))\n"
            "    print(json.dumps({'type': 'agent_settled'}))\n"
            + script_body
            + "\n",
            encoding="utf-8",
        )
        pi.chmod(0o755)
        return log_file

    # ------------------------------------------------------------------
    # Item 1: concurrent hotspot / migration / version collision warnings
    # ------------------------------------------------------------------
    def test_1_start_write_warns_on_unapplied_hotspot_migration_and_version_without_blocking(self):
        repo = self.repo(
            {
                "Cargo.toml": '[package]\nname = "demo"\nversion = "6.36.0"\n',
                "CHANGELOG.md": "# Changelog\n\n## Unreleased\n",
                "migrations/0253_init.sql": "-- init\n",
                "src/other.rs": "pub fn other() {}\n",
            }
        )
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre=(
                "printf '[package]\\nname = \"demo\"\\nversion = \"6.37.0\"\\n' > Cargo.toml; "
                "printf '# Changelog\\n\\n## Unreleased\\n- demo 6.37.0\\n' > CHANGELOG.md; "
                "printf '-- col\\n' > migrations/0254_add_col.sql"
            ),
        )
        first = self.outcome(
            self.cli("run", "--tier", "write", "--worktree", "--workdir", repo, "--accept", "true", "--name", "first-hot", "bump 6.37.0 and add 0254")
        )
        self.assertEqual(first["state"], "delivered")

        # Second write start collides on Cargo.toml, CHANGELOG.md, migration 0254, and version 6.37.0.
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
        hit = self.cli(
            "start", "--tier", "write", "--worktree", "--workdir", repo, "--name", "second-hot",
            "更新 Cargo.toml 和 CHANGELOG.md 到 6.37.0 并新增 migrations/ 0254 迁移",
        )
        self.assertEqual(hit.returncode, 0, hit.stdout + hit.stderr)
        overlap_lines = [l for l in hit.stderr.splitlines() if "overlap:" in l]
        self.assertEqual(len(overlap_lines), 1, f"expected single stderr hint line, got: {hit.stderr}")
        line = overlap_lines[0]
        self.assertIn(first["run"], line)
        self.assertIn(f"--after {first['run']}", line)
        self.assertIn("Cargo.toml", line)
        self.assertIn("CHANGELOG.md", line)
        self.assertIn("0254", line)
        self.assertIn("6.37.0", line)

        # Unrelated write task does NOT trigger overlap warning from unapplied run.
        unrelated = self.cli(
            "start", "--tier", "write", "--worktree", "--workdir", repo, "--name", "third-clean",
            "refactor src/other.rs helper",
        )
        self.assertEqual(unrelated.returncode, 0, unrelated.stdout + unrelated.stderr)
        self.assertNotIn("overlap:", unrelated.stderr)

        self.cli("wait", "--no-result", "second-hot", "third-clean")

    # ------------------------------------------------------------------
    # Item 3: review reads full reply brief chain (reproducing c9cb)
    # ------------------------------------------------------------------
    def test_3_review_reads_full_reply_brief_chain_reproducing_c9cb(self):
        repo = self.repo(
            {
                "src/mode.rs": "pub const MODE: &str = \"OLD\";\n",
                ".delegate.json": json.dumps(
                    {
                        "review": {"enabled": True, "tier": "review"},
                        "accept": "true",
                    }
                ),
            }
        )
        # Call 1: root author writes MODE=A
        # Call 2: root reviewer sees MODE=A and PASSes
        # Call 3: reply author switches to MODE=B per reply brief
        # Call 4: reply reviewer checks that its prompt includes BOTH root brief AND reply direction change!
        #         If reply brief were dropped (the c9cb bug), reviewer would escalate NEEDS_CONTROLLER.
        self.install_scripted_pi(
            "if call_idx == 1:\n"
            "    Path('src/mode.rs').write_text('pub const MODE: &str = \"A\";\\n')\n"
            "    emit_answer('implemented plan A')\n"
            "elif call_idx == 2:\n"
            "    emit_answer('PASS\\nplan A looks good\\n\\n## 主控重点看\\n无')\n"
            "elif call_idx == 3:\n"
            "    Path('src/mode.rs').write_text('pub const MODE: &str = \"B\";\\n')\n"
            "    emit_answer('switched to plan B per controller decision')\n"
            "else:\n"
            "    if '改为方案 B' in stdin_text and '按方案 A 实现' in stdin_text:\n"
            "        emit_answer('PASS\\nmatches latest controller decision (plan B)\\n\\n## 主控重点看\\n无')\n"
            "    else:\n"
            "        emit_answer('NEEDS_CONTROLLER\\ndiff writes MODE=B but brief only says plan A\\n\\n## 主控重点看\\n无')\n"
        )
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "c9cb-root", "按方案 A 实现 src/mode.rs")
        )
        self.assertEqual(r1["state"], "delivered")
        self.assertEqual(r1["review"]["verdict"], "PASS")

        r2 = self.outcome(
            self.cli("reply", "--wait", r1["run"], "主控拍板：改为方案 B（写 MODE=B）")
        )
        self.assertEqual(r2["state"], "delivered", r2)
        self.assertEqual(r2["review"]["verdict"], "PASS", r2)

        rev_prompt = (Path(r2["dir"]) / "review/round-1/prompt.md").read_text(encoding="utf-8")
        self.assertIn("按方案 A 实现 src/mode.rs", rev_prompt)
        self.assertIn("主控拍板：改为方案 B", rev_prompt)
        self.assertLess(rev_prompt.index("按方案 A 实现"), rev_prompt.index("主控拍板：改为方案 B"))

    # ------------------------------------------------------------------
    # Item 4: agy OAuth2 401 backs off & re-probes once; still failing -> permanent failure
    # ------------------------------------------------------------------
    def test_4a_agy_401_recovers_after_single_backoff_and_reprobe(self):
        self.write_agy_config()
        self.env["DELEGATE_RESUME_BACKOFF"] = "0.05,0.05,0.05"
        repo = self.repo({"a.txt": "old\n"})
        log_file = self.install_scripted_agy(
            "if call_idx == 1:\n"
            "    print(json.dumps({'event': 'init', 'conversation_id': 'conv-401-ok'}), flush=True)\n"
            "    sys.stderr.write('UNAUTHENTICATED (code 401): Expected OAuth 2 access token\\n')\n"
            "    sys.exit(1)\n"
            "elif is_probe:\n"
            "    emit_agy('ok', cid='conv-probe')\n"
            "else:\n"
            "    Path('a.txt').write_text('recovered\\n')\n"
            "    emit_agy('recovered after token refresh', cid='conv-401-ok')\n"
        )
        res = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "update a.txt"))
        self.assertEqual(res["state"], "delivered", res)
        self.assertEqual(res["resumes"], 1)
        self.assertEqual(res["attempts"], 2)
        calls = [json.loads(l) for l in log_file.read_text().splitlines() if l.strip()]
        probe_calls = [c for c in calls if c["is_probe"]]
        self.assertEqual(len(probe_calls), 1)

    def test_4b_agy_401_permanent_failure_after_one_failed_reprobe_reports_account_status(self):
        self.write_agy_config()
        self.env["DELEGATE_RESUME_BACKOFF"] = "0.05,0.05,0.05"
        repo = self.repo({"a.txt": "old\n"})
        log_file = self.install_scripted_agy(
            "if not is_probe:\n"
            "    print(json.dumps({'event': 'init', 'conversation_id': 'conv-401-dead'}), flush=True)\n"
            "sys.stderr.write('UNAUTHENTICATED (code 401): Expected OAuth 2 access token\\n')\n"
            "sys.exit(1)\n"
        )
        res = self.cli("run", "--worktree", "--workdir", repo, "update a.txt")
        self.assertNotEqual(res.returncode, 0)
        out = self.outcome(res)
        self.assertEqual(out["state"], "failed")
        self.assertIn("agy 登录失效", out.get("error", ""))
        self.assertIn("delegate agy account status", out.get("error", ""))
        # Must re-probe strictly ONCE (not 3 times!)
        calls = [json.loads(l) for l in log_file.read_text().splitlines() if l.strip()]
        probe_calls = [c for c in calls if c["is_probe"]]
        self.assertEqual(len(probe_calls), 1, f"expected exactly 1 re-probe, got {len(probe_calls)}: {calls}")
        # Human status line also surfaces the login hint
        st = self.cli("status", out["run"], human=True)
        self.assertIn("agy 登录失效", st.stdout)
        self.assertIn("delegate agy account status", st.stdout)

    # ------------------------------------------------------------------
    # Item 5: tokensDelta in summary, history, show, and card
    # ------------------------------------------------------------------
    def test_5_tokens_delta_displayed_in_history_show_and_card(self):
        self.write_agy_config()
        repo = self.repo({"a.txt": "v0\n"})
        self.install_scripted_agy(
            "if call_idx == 1:\n"
            "    Path('a.txt').write_text('v1\\n')\n"
            "    emit_agy('round 1 done', inp=100, out=20, cache=5000, cid='conv-tok')\n"
            "else:\n"
            "    Path('a.txt').write_text('v2\\n')\n"
            "    emit_agy('round 2 done', inp=160, out=45, cache=8500, cid='conv-tok')\n"
        )
        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "--name", "tok-r1", "v1"))
        self.assertEqual(r1["tokens"], {"input": 100, "output": 20, "cacheRead": 5000})
        self.assertNotIn("tokensDelta", r1)

        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "v2"))
        self.assertEqual(r2["tokens"], {"input": 160, "output": 45, "cacheRead": 8500})
        self.assertEqual(r2["tokensDelta"], {"input": 60, "output": 25, "cacheRead": 3500})

        # `delegate history --json` carries tokensDelta; human history shows incremental tokens (NOT 8500)
        hist_json = [json.loads(l) for l in self.cli("history", "--json", cwd=repo).stdout.splitlines() if l.strip()]
        r2_hist = [e for e in hist_json if e["run"] == r2["run"]][0]
        self.assertEqual(r2_hist["tokensDelta"], {"input": 60, "output": 25, "cacheRead": 3500})
        hist_human = self.cli("history", human=True, cwd=repo).stdout
        r2_line = [l for l in hist_human.splitlines() if r2["run"] in l][0]
        self.assertIn("in 60 / out 25 / cache 3500", r2_line)
        self.assertNotIn("cache 8500", r2_line)

        # `delegate show` (human + json) uses incremental tokens
        show_human = self.cli("show", r2["run"], human=True, cwd=repo).stdout
        self.assertIn("in 60 / out 25 / cache 3500（本轮增量）", show_human)
        self.assertNotIn("cache 8500", show_human)
        show_json = json.loads(self.cli("show", r2["run"], "--json", cwd=repo).stdout)
        self.assertEqual(show_json["tokensDelta"], {"input": 60, "output": 25, "cacheRead": 3500})

        # `delegate card` (human + json) displays incremental tokens
        card_human = self.cli("card", r2["run"], human=True, cwd=repo).stdout
        self.assertIn("in 60 / out 25 / cache 3500（本轮增量）", card_human)
        self.assertNotIn("cache 8500", card_human)
        card_json = json.loads(self.cli("card", r2["run"], "--json", cwd=repo).stdout)
        self.assertEqual(card_json["tokensDelta"], {"input": 60, "output": 25, "cacheRead": 3500})

    # ------------------------------------------------------------------
    # Item 6: apply archive has no size cap skipping
    # ------------------------------------------------------------------
    def test_6_apply_archive_never_skips_files_by_size_cap(self):
        self.env["DELEGATE_ARCHIVE_MAX_BYTES"] = "5"
        repo = self.repo({"a.txt": "v0\n", ".gitignore": ".local/\n"})
        self.fake_pi(
            [fixtures.answer("done with .local/big1.bin"), fixtures.SETTLED],
            pre="echo v1 > a.txt; mkdir -p .local; printf '0123456789abcdef' > .local/big1.bin; printf 'abcdefghij' > .local/big2.bin",
        )
        r = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "edit a"))
        ap = self.cli("apply", r["run"], cwd=repo)
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)
        self.assertNotIn("archive size cap", ap.stderr)
        arch = self.work / "cache/delegate/archive" / r["run"]
        manifest = json.loads((arch / "manifest.json").read_text(encoding="utf-8"))
        self.assertEqual(manifest["skipped"], [])
        self.assertIn(".local/big1.bin", manifest["files"])
        self.assertIn(".local/big2.bin", manifest["files"])


if __name__ == "__main__":
    unittest.main()
