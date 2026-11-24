"""6.34.0 regressions:
1. Merge Review Card progressive disclosure:
   - L0 one-liner in status, wait, and delivery events (acceptance, review, files/+A/-D, risk counts,
     fast-forward placeholder `快进 —`, next command `delegate card <run>`).
   - L1 `delegate card <run>` (<= 1 screen: author summary + result.md pointer, reviewer conclusion +
     verdict pointer, deduplicated sorted risk items from built-in + custom rules + reviewer focus,
     test change counts + tiered acceptance layers, directory-folded remaining changes, next steps).
   - L2 `delegate diff <run> --risky` (only risk-matched hunks with surrounding context).
   - L3 `delegate diff <run>` unchanged.
2. Tiered path-matched acceptance (`acceptRules`):
   - Runs only matched layers, skips unrelated layers with reasons, backward compatible with `accept`.
   - Fail-closed full fallback on uncovered paths or build config files (`Cargo.*`, `xtask/**`,
     `package.json`, lockfiles).
3. Parallel slow acceptance + read-only reviewer:
   - Runs concurrently after fast layers pass; fast failure skips both slow and reviewer.
   - Slow failure cancels reviewer immediately and NEVER records reviewer as PASS; run is rejected.
   - Reviewer REWORK cancels slow acceptance immediately; rework re-runs ONLY affected/unpassed layers.
"""
import json
import os
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate634Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def write_user_config(self, doc: dict) -> None:
        cfg_dir = Path(self.env["XDG_CONFIG_HOME"]) / "delegate"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "config.json").write_text(json.dumps(doc))
        for k in ("DELEGATE_SCOUT_MODEL", "DELEGATE_REVIEW_MODEL", "DELEGATE_WRITE_MODEL"):
            self.env.pop(k, None)

    def install_scripted_pi(self, body_py: str) -> Path:
        log_file = self.work / "pi-calls.jsonl"
        self.env["SCRIPTED_PI_LOG"] = str(log_file)
        pi = self.bin / "pi"
        pi.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys, time\n"
            "from pathlib import Path\n"
            "args = sys.argv[1:]\n"
            "if args == ['--version']:\n"
            "    print('fake-pi 1.0'); sys.exit(0)\n"
            "if len(args) >= 2 and args[1] == '--list-models':\n"
            "    print('provider model context max-out thinking images')\n"
            "    print('p write-m 1M 1K yes yes')\n"
            "    print('p review-m 1M 1K yes yes')\n"
            "    sys.exit(0)\n"
            "stdin_text = sys.stdin.read()\n"
            "sdir = args[args.index('--session-dir') + 1] if '--session-dir' in args else ''\n"
            "sid = args[args.index('--session-id') + 1] if '--session-id' in args else ''\n"
            "fork = args[args.index('--fork') + 1] if '--fork' in args else ''\n"
            "if sdir:\n"
            "    Path(sdir).mkdir(parents=True, exist_ok=True)\n"
            "    (Path(sdir) / f'1_{sid or os.getpid()}.jsonl').write_text('{\"type\":\"session\"}\\n')\n"
            "lp = Path(os.environ['SCRIPTED_PI_LOG'])\n"
            "call_idx = len(lp.read_text().splitlines()) + 1 if lp.is_file() else 1\n"
            "with lp.open('a') as fh:\n"
            "    fh.write(json.dumps({'call': call_idx, 'cwd': os.getcwd(), 'fork': fork}) + '\\n')\n"
            "def emit_answer(text):\n"
            "    print(json.dumps({'type':'message_end','message':{'role':'assistant','stopReason':'stop','model':'p/m','usage':{'input':10,'output':5,'cacheRead':0},'content':[{'type':'text','text':text}]}}), flush=True)\n"
            "    print(json.dumps({'type':'agent_settled'}), flush=True)\n"
            + body_py
            + "\n"
        )
        pi.chmod(0o755)
        self.write_user_config({
            "models": {"w": "p/write-m", "r": "p/review-m"},
            "tiers": {"scout": "r", "review": "r", "write": "w"},
        })
        return log_file

    def read_events(self) -> list[dict]:
        ev_file = self.state_dir() / "events.jsonl"
        if not ev_file.is_file():
            return []
        return [json.loads(ln) for ln in ev_file.read_text().splitlines() if ln.strip()]

    def test_l0_l1_l2_l3_norvane_shaped_progressive_disclosure_and_dedup(self):
        repo = self.repo({
            ".delegate.json": json.dumps({
                "accept": "true",
                "acceptRules": [
                    {
                        "name": "rust-fast",
                        "tier": "fast",
                        "paths": ["crates/**/*.rs"],
                        "command": "true",
                    },
                    {
                        "name": "rust-slow",
                        "tier": "slow",
                        "paths": ["crates/**/*.rs"],
                        "command": "true",
                    },
                    {
                        "name": "migrations",
                        "tier": "fast",
                        "paths": ["migrations/**"],
                        "command": "true",
                    },
                    {
                        "name": "web",
                        "tier": "fast",
                        "paths": ["apps/web/**"],
                        "command": "true",
                    },
                    {
                        "name": "prompts-and-docs",
                        "tier": "fast",
                        "paths": ["skills/**", "docs/**"],
                        "command": "true",
                    },
                ],
                "review": True,
                "controllerReviewPaths": ["skills/*/SKILL.md"],
            }),
            "skills/chat/SKILL.md": "---\nname: chat\n---\n# Chat v1\nUse concise replies.\n",
            "migrations/0041_init.sql": "CREATE TABLE items (id INT PRIMARY KEY);\n",
            "crates/control-plane/src/pool.rs": (
                "const MAX_POOL: usize = 8;\n"
                "pub fn size() -> usize { MAX_POOL }\n"
            ),
            "crates/control-plane/src/pool_test.rs": (
                "#[test]\n"
                "fn test_existing_pool() {\n"
                "    assert_eq!(1, 1);\n"
                "    assert_eq!(2, 2);\n"
                "}\n"
            ),
            "crates/server/src/handler.rs": (
                "pub fn handle_req(v: i32) -> i32 {\n"
                "    v + 1\n"
                "}\n"
            ),
            "docs/notes.md": "# Notes\nInitial note.\n",
            "apps/web/src/App.tsx": "export const App = () => null;\n",
        })

        self.install_scripted_pi(
            "if call_idx == 1:\n"
            "    Path('skills/chat/SKILL.md').write_text('---\\nname: chat\\n---\\n# Chat v2\\nAlways confirm destructive actions.\\n')\n"
            "    Path('migrations/0042_add_slot_priority.sql').write_text('ALTER TABLE items ADD COLUMN priority INT NOT NULL DEFAULT 0;\\n')\n"
            "    Path('crates/control-plane/src/pool.rs').write_text(\n"
            "        'use std::sync::Mutex;\\n'\n"
            "        'const MAX_POOL: usize = 16;\\n'\n"
            "        'pub fn size() -> usize { let _g = Mutex::new(MAX_POOL); MAX_POOL }\\n'\n"
            "    )\n"
            "    Path('crates/control-plane/src/pool_test.rs').write_text(\n"
            "        '#[test]\\n'\n"
            "        '#[ignore]\\n'\n"
            "        'fn test_existing_pool() {\\n'\n"
            "        '    assert_eq!(1, 1);\\n'\n"
            "        '}\\n'\n"
            "        '#[test]\\n'\n"
            "        'fn test_new_pool_limit() {\\n'\n"
            "        '    assert_eq!(16, 16);\\n'\n"
            "        '}\\n'\n"
            "    )\n"
            "    Path('crates/server/src/handler.rs').write_text(\n"
            "        'pub fn handle_req(v: i32) -> i32 {\\n'\n"
            "        '    if v < 0 { return 0; }\\n'\n"
            "        '    v + 1\\n'\n"
            "        '}\\n'\n"
            "    )\n"
            "    Path('docs/notes.md').write_text('# Notes\\nInitial note.\\nUpdated pool docs.\\n')\n"
            "    emit_answer(\n"
            "        '已将连接池上限调整至 16 并新增 priority 迁移列。\\n'\n"
            "        '补齐了负数请求兜底与新单测，并同步更新 chat 技能说明。\\n'\n"
            "        '额外细节见完整报告段落（保留在 result.md 原文）。\\n'\n"
            "        '第四行不该出现在 L1 摘要中，但必须完整保留在 result.md。'\n"
            "    )\n"
            "else:\n"
            "    emit_answer(\n"
            "        'PASS\\n'\n"
            "        '核心逻辑与迁移一致，但并发锁与被忽略旧测需主控确认。\\n\\n'\n"
            "        '## 主控重点看\\n'\n"
            "        '- [高] `crates/control-plane/src/pool.rs:3` — 确认新引入的 Mutex 与调用方锁序不冲突\\n'\n"
            "        '- [中] crates/server/src/handler.rs:2 — 负数静默归零可能掩盖上游非法输入\\n'\n"
            "    )\n"
        )

        res = self.cli("run", "--worktree", "--workdir", repo, "--name", "norvane-card", "adjust pool and migration", human=True)
        self.assertEqual(res.returncode, 0, res.stderr)

        st_json = self.outcome(self.cli("status", "norvane-card"))
        run_id = st_json["run"]
        run_dir = Path(st_json["dir"])

        # L0 in status (human)
        l0_status = self.cli("status", run_id, human=True).stdout.splitlines()[0]
        self.assertIn("验收通过", l0_status)
        self.assertIn("跳过 web", l0_status)
        self.assertIn("NEEDS_CONTROLLER", l0_status)
        self.assertIn("改 6 个文件", l0_status)
        self.assertIn("风险 ", l0_status)
        self.assertIn("快进 —", l0_status)
        self.assertIn(f"delegate card {run_id}", l0_status)

        # L0 in delivered event (events)
        evs = self.read_events()
        deliv = [e for e in evs if e["run"] == run_id and e["kind"] == "delivered"]
        self.assertEqual(len(deliv), 1)
        self.assertIn("card", deliv[0]["payload"])
        self.assertGreater(deliv[0]["payload"]["card"]["riskTotal"], 0)
        self.assertIsNone(deliv[0]["payload"]["card"]["fastForward"])
        ev_human = self.cli("events", "--since", "0", human=True, cwd=repo).stdout
        self.assertIn("快进 —", ev_human)
        self.assertIn(f"delegate card {run_id}", ev_human)

        # Compact L0 in summary.json & index.json
        summary_doc = json.loads((run_dir / "summary.json").read_text())
        self.assertLess(len(json.dumps(summary_doc["card"])), 512)

        # L1: delegate card <run> (human + json)
        card_human = self.cli("card", run_id, human=True, cwd=repo)
        self.assertEqual(card_human.returncode, 0, card_human.stderr)
        ctext = card_human.stdout
        for expected in (
            "1. 做了什么",
            "2. 风险项",
            "3. 测试与验收",
            "4. 其余改动",
            "5. 下一步",
            "快进 —",
            "result.md",
            "模型可见文字",
            "迁移/Schema",
            "测试减弱",
            "规则+审查员",
            "审查员提示（审查员）",
            "跳过",
            "docs/",
            f"delegate diff {run_id} --risky",
            f"delegate diff {run_id} --total",
            f"delegate apply {run_id}",
            f"delegate sign {run_id}",
        ):
            self.assertIn(expected, ctext, f"missing {expected!r} in L1 card:\n{ctext}")
        # Author summary truncated to 3 lines in L1, while result.md on disk is untouched!
        self.assertNotIn("第四行不该出现在 L1 摘要中", ctext)
        self.assertIn("第四行不该出现在 L1 摘要中", (run_dir / "result.md").read_text())

        card_json_res = self.cli("card", run_id, "--json", cwd=repo)
        self.assertEqual(card_json_res.returncode, 0, card_json_res.stderr)
        card_doc = json.loads(card_json_res.stdout)
        # Verify deduplication: pool.rs has rule hit + reviewer overlap merged into `规则+审查员`,
        # NOT a duplicate separate reviewer_focus entry at pool.rs:3!
        pool_risks = [r for r in card_doc["risks"] if r["file"] == "crates/control-plane/src/pool.rs"]
        self.assertTrue(any(r["source"] == "规则+审查员" for r in pool_risks), pool_risks)
        self.assertFalse(any(r["rule"] == "reviewer_focus" for r in pool_risks), pool_risks)
        # Verify handler.rs reviewer-only item is preserved with source `审查员`
        handler_risks = [r for r in card_doc["risks"] if r["file"] == "crates/server/src/handler.rs"]
        self.assertEqual(len(handler_risks), 1)
        self.assertEqual(handler_risks[0]["source"], "审查员")
        # Verify docs/notes.md (non-risky) is folded into otherChanges
        self.assertTrue(any(g["dir"] == "docs/" for g in card_doc["otherChanges"]), card_doc["otherChanges"])
        # Verify test counts
        self.assertGreaterEqual(card_doc["tests"]["added"], 1)

        # L2: delegate diff <run> --risky -> ONLY risky hunks, NO docs/notes.md!
        l2 = self.cli("diff", run_id, "--risky", cwd=repo)
        self.assertEqual(l2.returncode, 0, l2.stderr)
        self.assertIn("# 风险命中:", l2.stdout)
        self.assertIn("skills/chat/SKILL.md", l2.stdout)
        self.assertIn("migrations/0042_add_slot_priority.sql", l2.stdout)
        self.assertIn("crates/control-plane/src/pool.rs", l2.stdout)
        self.assertIn("crates/server/src/handler.rs", l2.stdout)
        self.assertNotIn("docs/notes.md", l2.stdout)

        # L3: delegate diff <run> --total -> full diff unchanged (includes docs/notes.md!)
        l3 = self.cli("diff", run_id, "--total", cwd=repo)
        self.assertEqual(l3.returncode, 0, l3.stderr)
        self.assertIn("docs/notes.md", l3.stdout)
        self.assertIn("Updated pool docs.", l3.stdout)

    def test_custom_risk_rules_and_zero_risk_card(self):
        repo = self.repo({
            ".delegate.json": json.dumps({
                "accept": "true",
                "riskRules": {
                    "disable": ["config_constant"],
                    "extra": [
                        {
                            "id": "sandbox_policy",
                            "label": "沙箱策略",
                            "severity": "high",
                            "paths": ["crates/sandbox/**"],
                            "content": ["seccomp"],
                            "reason": "沙箱 seccomp 策略改动",
                        }
                    ],
                },
            }),
            "crates/sandbox/src/policy.rs": "pub fn init() {}\n",
            "docs/readme.md": "hello\n",
        })
        # Run 1: clean doc change -> 0 risks
        self.fake_pi([fixtures.answer("updated docs"), fixtures.SETTLED], pre="echo more >> docs/readme.md")
        r0 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "zero-risk", "docs"))
        self.assertEqual(r0["card"]["riskTotal"], 0)
        l0 = self.cli("status", r0["run"], human=True).stdout.splitlines()[0]
        self.assertIn("风险 0", l0)
        l1 = self.cli("card", r0["run"], human=True, cwd=repo).stdout
        self.assertIn("2. 风险项（共 0 项", l1)
        self.assertIn("   无", l1)
        l2 = self.cli("diff", r0["run"], "--risky", cwd=repo).stdout
        self.assertIn("无命中风险项的改动", l2)

        # Run 2: triggers custom riskRules extra rule
        self.fake_pi(
            [fixtures.answer("added seccomp"), fixtures.SETTLED],
            pre="printf 'pub fn init() { let _ = \"seccomp\"; }\\n' > crates/sandbox/src/policy.rs",
        )
        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "custom-risk", "sandbox"))
        self.assertEqual(r1["card"]["riskTotal"], 1)
        c1 = json.loads(self.cli("card", r1["run"], "--json", cwd=repo).stdout)
        self.assertEqual(c1["risks"][0]["rule"], "sandbox_policy")
        self.assertEqual(c1["risks"][0]["category"], "沙箱策略")

    def test_tiered_accept_rules_path_matching_and_fail_closed(self):
        log_file = self.work / "accept-layers.log"
        repo = self.repo({
            ".delegate.json": json.dumps({
                "accept": f"echo full >> {log_file}",
                "acceptRules": [
                    {
                        "name": "rust-fast",
                        "tier": "fast",
                        "paths": ["crates/**/*.rs"],
                        "command": f"echo rust-fast >> {log_file}",
                    },
                    {
                        "name": "rust-slow",
                        "tier": "slow",
                        "paths": ["crates/**/*.rs"],
                        "command": f"echo rust-slow >> {log_file}",
                    },
                    {
                        "name": "web",
                        "tier": "fast",
                        "paths": ["apps/web/**"],
                        "command": f"echo web >> {log_file}",
                    },
                ],
            }),
            "crates/core/src/lib.rs": "pub fn f() -> i32 { 1 }\n",
            "apps/web/src/App.tsx": "export const x = 1;\n",
            "Cargo.toml": "[workspace]\nmembers = []\n",
            "scripts/tool.sh": "#!/bin/sh\necho hi\n",
        })

        # Case 1: Web-only change -> runs ONLY `web`, skips rust-fast & rust-slow!
        self.fake_pi([fixtures.answer("web edit"), fixtures.SETTLED], pre="echo 'export const x = 2;' > apps/web/src/App.tsx")
        st_web = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "web-only", "web"))
        self.assertEqual(st_web["state"], "delivered")
        self.assertTrue(st_web["accept"]["tiered"])
        self.assertFalse(st_web["accept"]["failClosed"])
        self.assertEqual(st_web["accept"]["ranLayers"], ["web"])
        skipped_names = [x["name"] for x in st_web["accept"]["skippedLayers"]]
        self.assertEqual(skipped_names, ["rust-fast", "rust-slow"])
        self.assertEqual(log_file.read_text().splitlines(), ["web"])

        # Case 2: Build config change (`Cargo.toml`) -> fail-closed runs full!
        log_file.unlink()
        self.fake_pi([fixtures.answer("cargo edit"), fixtures.SETTLED], pre="echo '# bump' >> Cargo.toml")
        st_cfg = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "build-cfg", "cargo"))
        self.assertEqual(st_cfg["state"], "delivered")
        self.assertTrue(st_cfg["accept"]["failClosed"])
        self.assertIn("Cargo.toml", st_cfg["accept"]["failClosedReason"])
        self.assertEqual(st_cfg["accept"]["ranLayers"], ["full"])
        self.assertEqual(log_file.read_text().splitlines(), ["full"])

        # Case 3: Uncovered file (`scripts/tool.sh`) -> fail-closed runs full!
        log_file.unlink()
        self.fake_pi([fixtures.answer("script edit"), fixtures.SETTLED], pre="echo 'echo v2' >> scripts/tool.sh")
        st_unc = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "uncovered", "script"))
        self.assertEqual(st_unc["state"], "delivered")
        self.assertTrue(st_unc["accept"]["failClosed"])
        self.assertIn("scripts/tool.sh", st_unc["accept"]["failClosedReason"])
        self.assertEqual(log_file.read_text().splitlines(), ["full"])

    def test_parallel_slow_accept_and_reviewer_overlap_and_fast_failure_skips_both(self):
        slow_started = self.work / "slow-started"
        rev_started = self.work / "rev-started"
        overlap_ok = self.work / "overlap-ok"
        slow_cmd = (
            f"touch {slow_started}; "
            f"for i in $(seq 1 60); do [ -f {rev_started} ] && touch {overlap_ok} && exit 0; sleep 0.05; done; "
            f"exit 1"
        )
        repo = self.repo({
            ".delegate.json": json.dumps({
                "review": True,
                "accept": "true",
                "acceptRules": [
                    {
                        "name": "fast-check",
                        "tier": "fast",
                        "paths": ["src/**"],
                        "command": "grep -q 'GOOD' src/a.rs",
                    },
                    {
                        "name": "slow-check",
                        "tier": "slow",
                        "paths": ["src/**"],
                        "command": slow_cmd,
                    },
                ],
            }),
            "src/a.rs": "pub fn v() { /* INIT */ }\n",
        })

        # 1) Fast failure -> neither slow-check nor reviewer runs!
        log_file = self.install_scripted_pi(
            "Path('src/a.rs').write_text('pub fn v() { /* BAD */ }\\n')\n"
            "emit_answer('wrote bad')\n"
        )
        st_bad = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "fast-fail", "bad"))
        self.assertEqual(st_bad["state"], "rejected")
        self.assertFalse(slow_started.exists(), "slow layer must not run after fast failure")
        self.assertEqual(len(log_file.read_text().splitlines()), 1, "reviewer must not run after fast failure")

        # 2) Fast pass -> slow-check and reviewer overlap concurrently and both pass -> delivered!
        log_file.unlink()
        self.install_scripted_pi(
            f"if call_idx == 1:\n"
            f"    Path('src/a.rs').write_text('pub fn v() {{ /* GOOD */ }}\\n')\n"
            f"    emit_answer('wrote good')\n"
            f"else:\n"
            f"    Path({str(rev_started)!r}).touch()\n"
            f"    for _ in range(60):\n"
            f"        if Path({str(slow_started)!r}).exists():\n"
            f"            break\n"
            f"        time.sleep(0.05)\n"
            f"    assert Path({str(slow_started)!r}).exists(), 'slow accept did not run in parallel!'\n"
            f"    emit_answer('PASS\\nParallel review verified.')\n"
        )
        st_ok = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "parallel-ok", "good"))
        self.assertEqual(st_ok["state"], "delivered")
        self.assertTrue(overlap_ok.exists(), "slow accept and reviewer must overlap in parallel")
        self.assertTrue(st_ok["accept"]["parallelWithReview"])
        self.assertEqual(st_ok["review"]["verdict"], "PASS")

    def test_slow_accept_failure_cancels_reviewer_never_marks_pass(self):
        rev_started = self.work / "rev-started-cancel"
        slow_cmd = (
            f"for i in $(seq 1 40); do [ -f {rev_started} ] && break; sleep 0.05; done; "
            f"exit 1"
        )
        repo = self.repo({
            ".delegate.json": json.dumps({
                "review": True,
                "accept": "true",
                "acceptRules": [
                    {"name": "fast", "tier": "fast", "paths": ["src/**"], "command": "true"},
                    {"name": "slow", "tier": "slow", "paths": ["src/**"], "command": slow_cmd},
                ],
            }),
            "src/a.rs": "pub fn v() {}\n",
        })
        self.install_scripted_pi(
            f"if call_idx == 1:\n"
            f"    Path('src/a.rs').write_text('pub fn v() {{ let _ = 1; }}\\n')\n"
            f"    emit_answer('done')\n"
            f"else:\n"
            f"    Path({str(rev_started)!r}).touch()\n"
            f"    time.sleep(25)\n"
            f"    emit_answer('PASS\\nshould have been cancelled')\n"
        )
        t0 = time.monotonic()
        st = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "slow-fail-cancel", "task"))
        elapsed = time.monotonic() - t0
        self.assertLess(elapsed, 15.0, f"reviewer should have been cancelled promptly, took {elapsed:.1f}s")
        self.assertEqual(st["state"], "rejected")
        self.assertFalse(st["accept"]["ok"])
        # Cancelled reviewer MUST NEVER be marked PASS!
        self.assertNotEqual(st["review"].get("verdict"), "PASS")
        self.assertEqual(st["review"]["status"], "cancelled")
        self.assertTrue(st["review"]["cancelled"])
        self.assertEqual(st["review"]["cancelledBy"], "acceptFailed")
        self.assertIsNone(st["review"].get("verdict"))

    def test_reviewer_rework_cancels_slow_accept_and_reruns_only_affected_layers(self):
        layer_log = self.work / "rework-layers.log"
        slow_started = self.work / "rework-slow-started"
        # Round 1 slow command blocks for 25s (must be cancelled by REWORK); Round 2 finishes immediately!
        round2_flag = self.work / "allow-slow-pass"
        slow_cmd = (
            f"echo slow >> {layer_log}; "
            f"touch {slow_started}; "
            f"if [ ! -f {round2_flag} ]; then sleep 25; fi; "
            f"exit 0"
        )
        repo = self.repo({
            ".delegate.json": json.dumps({
                "review": True,
                "accept": "true",
                "acceptRules": [
                    {
                        "name": "web",
                        "tier": "fast",
                        "paths": ["apps/web/**"],
                        "command": f"echo web >> {layer_log}",
                    },
                    {
                        "name": "rust-fast",
                        "tier": "fast",
                        "paths": ["crates/**"],
                        "command": f"echo rust-fast >> {layer_log}",
                    },
                    {
                        "name": "rust-slow",
                        "tier": "slow",
                        "paths": ["crates/**"],
                        "command": slow_cmd,
                    },
                ],
            }),
            "apps/web/src/App.tsx": "export const v = 0;\n",
            "crates/core/src/lib.rs": "pub fn v() -> i32 { 0 }\n",
        })

        self.install_scripted_pi(
            f"if call_idx == 1:\n"
            f"    # Round 1 author touches BOTH web and rust\n"
            f"    Path('apps/web/src/App.tsx').write_text('export const v = 1;\\n')\n"
            f"    Path('crates/core/src/lib.rs').write_text('pub fn v() -> i32 {{ 1 }}\\n')\n"
            f"    emit_answer('r1 done')\n"
            f"elif call_idx == 2:\n"
            f"    # Round 1 reviewer waits briefly for slow to start, then returns REWORK -> cancels slow!\n"
            f"    for _ in range(40):\n"
            f"        if Path({str(slow_started)!r}).exists():\n"
            f"            break\n"
            f"        time.sleep(0.05)\n"
            f"    emit_answer('REWORK\\nFix crates/core/src/lib.rs to return 2.')\n"
            f"elif call_idx == 3:\n"
            f"    # Author rework touches ONLY crates/core/src/lib.rs (NOT apps/web!)\n"
            f"    Path('crates/core/src/lib.rs').write_text('pub fn v() -> i32 {{ 2 }}\\n')\n"
            f"    Path({str(round2_flag)!r}).touch()\n"
            f"    emit_answer('r2 fixed rust only')\n"
            f"else:\n"
            f"    emit_answer('PASS\\nFixed cleanly.\\n\\n## 主控重点看\\n无')\n"
        )

        t0 = time.monotonic()
        st = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "rework-inc", "update both"))
        elapsed = time.monotonic() - t0
        self.assertLess(elapsed, 15.0, f"REWORK should cancel slow accept promptly, took {elapsed:.1f}s")
        self.assertEqual(st["state"], "delivered")
        self.assertEqual(st["review"]["verdict"], "PASS")
        self.assertEqual(st["review"]["reworkRounds"], 1)

        # Verify `web` ran ONLY ONCE (reused on rework because rework only touched crates/**!),
        # while `rust-fast` ran TWICE (round 1 + rework round 2)!
        ran = layer_log.read_text().splitlines()
        self.assertEqual(ran.count("web"), 1, f"web should not rerun on rust-only rework: {ran}")
        self.assertEqual(ran.count("rust-fast"), 2, f"rust-fast must rerun on rust rework: {ran}")
        self.assertIn("web", st["accept"].get("reusedLayers", []), st["accept"])

    def test_size_budget_risk_flags_crossing_and_ignores_untouched(self):
        repo = self.repo({
            ".delegate.json": json.dumps({
                "accept": "true",
                "riskRules": {
                    "sizeBudgets": [
                        {"paths": ["src/**/*.rs"], "maxLines": 10, "maxTestFnLines": 6},
                    ],
                },
            }),
            ".repo-governance.json": json.dumps({
                "budgets": {
                    "skills/*/SKILL.md": 20,
                },
            }),
            "src/already_huge_untouched.rs": "".join(f"fn f{i}() {{}}\n" for i in range(30)),
            "src/cross.rs": "".join(f"fn c{i}() {{}}\n" for i in range(9)),
            "src/shrunk.rs": "".join(f"fn s{i}() {{}}\n" for i in range(15)),
            "src/unit_tests.rs": (
                "#[test]\nfn untouched_big() {\n"
                "    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n"
                "    assert_eq!(a + b + c + d, 10);\n}\n\n"
                "#[test]\nfn small_test() {\n    let x = 1;\n    assert_eq!(x, 1);\n}\n"
            ),
            "skills/demo/SKILL.md": "中" * 15 + "\n",
        })

        # Touch cross.rs (9 -> 12 lines), shrunk.rs (15 -> 12 lines),
        # unit_tests.rs (small_test 4 -> 8 lines, untouched_big unchanged at 7 lines),
        # skills/demo/SKILL.md (15 -> 26 tokens); leave already_huge_untouched.rs untouched.
        self.install_scripted_pi(
            "Path('src/cross.rs').write_text(''.join(f'fn c{i}() {{}}\\n' for i in range(12)))\n"
            "Path('src/shrunk.rs').write_text(''.join(f'fn s{i}() {{}}\\n' for i in range(12)))\n"
            "Path('src/unit_tests.rs').write_text(\n"
            "    '#[test]\\nfn untouched_big() {\\n'\n"
            "    '    let a = 1;\\n    let b = 2;\\n    let c = 3;\\n    let d = 4;\\n'\n"
            "    '    assert_eq!(a + b + c + d, 10);\\n}\\n\\n'\n"
            "    '#[test]\\nfn small_test() {\\n'\n"
            "    '    let x = 1;\\n    let y = 2;\\n    let z = 3;\\n    let w = 4;\\n'\n"
            "    '    let v = 5;\\n    assert_eq!(x + y + z + w + v, 15);\\n}\\n'\n"
            ")\n"
            "Path('skills/demo/SKILL.md').write_text('中' * 26 + '\\n', encoding='utf-8')\n"
            "emit_answer('updated size test files')\n"
        )
        st = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "size-budget", "bump files"))
        self.assertEqual(st["state"], "delivered")

        card = json.loads(self.cli("card", st["run"], "--json", cwd=repo).stdout)
        size_risks = [r for r in card["risks"] if r["rule"] == "size_budget"]
        risk_files = {r["file"] for r in size_risks}
        self.assertIn("src/cross.rs", risk_files, size_risks)
        self.assertIn("skills/demo/SKILL.md", risk_files, size_risks)
        self.assertIn("src/unit_tests.rs", risk_files, size_risks)
        self.assertNotIn("src/already_huge_untouched.rs", risk_files, size_risks)
        self.assertNotIn("src/shrunk.rs", risk_files, size_risks)

        test_fn_risks = [r for r in size_risks if r["file"] == "src/unit_tests.rs" and "测试函数" in r["reason"]]
        self.assertEqual(len(test_fn_risks), 1, test_fn_risks)
        self.assertIn("small_test", test_fn_risks[0]["reason"])
        self.assertEqual(test_fn_risks[0]["severity"], "medium")

        human = self.cli("card", st["run"], human=True, cwd=repo).stdout
        self.assertIn("体量", human)


if __name__ == "__main__":
    unittest.main()
