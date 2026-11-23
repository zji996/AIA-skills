"""End-to-end tests for delegate 6.30.0 (合入员与发散侦察 Radar):
1. `delegate merge --ready` / `<run>...` serial 3-way merge in an isolated integration
   worktree based on source HEAD, generated.inputs regeneration, per-run accept + final
   mergeAccept through lane, verbatim commit titles with `Delegate-Run: <run>` trailer,
   `merge-report.md`, `merge_started`/`merge_item`/`merge_done` events, and landing via
   `delegate apply --keep-commits <merge-run>`.
2. Skipping & atomic rollback to last green commit on: semantic conflict, red accept
   test, unsigned `controllerReviewPaths`, invalidated signature (`sign --check` fail),
   and protected paths (`defaults.protect` / ADR), continuing with subsequent runs.
3. `autoMerge` whitelist with `!` exclusions and `add:` add-only globs (e.g. `docs/**`,
   `!docs/decision/**`, `add:**/tests/**`, `apps/web/src/styles/**`) + review PASS gate,
   plus validly signed `controllerReviewPaths` accepted as 待主控点头.
4. `delegate radar`: deterministic `radar.collect` + SHA-256 fingerprint, zero-token exit
   when unchanged (`radar_unchanged`), read-only scout on change (cannot write or delegate),
   Host-side evidence anchor validation (drops anchorless candidates), fingerprint dedup
   into `<state>/radar/<repo>/board.md`, `delegate radar pick <id>` brief draft without
   auto-dispatching, and `delegate radar schedule` instructions without installing.
"""

import json
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate630Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    until = fixtures.DelegateTests.until
    repo = fixtures.DelegateTests.repo

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def read_events(self) -> list[dict]:
        p = self.state_dir() / "events.jsonl"
        if not p.is_file():
            return []
        return [json.loads(line) for line in p.read_text().splitlines() if line.strip()]

    def install_scripted_pi(self, script_body: str) -> Path:
        log_file = self.work / "scripted-pi-calls.jsonl"
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
            "    print('p scout-m 64k 1K no no')\n"
            "    print('p review-m 1M 1K yes yes')\n"
            "    print('p write-m 1M 1K yes yes')\n"
            "    sys.exit(0)\n"
            "stdin_text = sys.stdin.read()\n"
            "sdir = ''\n"
            "sid = ''\n"
            "tools = ''\n"
            "for i, a in enumerate(args):\n"
            "    if a == '--session-dir' and i + 1 < len(args): sdir = args[i + 1]\n"
            "    elif a == '--session-id' and i + 1 < len(args): sid = args[i + 1]\n"
            "    elif a == '--tools' and i + 1 < len(args): tools = args[i + 1]\n"
            "if sdir:\n"
            "    Path(sdir).mkdir(parents=True, exist_ok=True)\n"
            "    (Path(sdir) / f't_{sid or os.getpid()}.jsonl').write_text('{}\\n')\n"
            "log_path = Path(os.environ['SCRIPTED_PI_LOG'])\n"
            "prev = log_path.read_text().splitlines() if log_path.is_file() else []\n"
            "call_idx = len(prev) + 1\n"
            "rec = {'call': call_idx, 'args': args, 'stdin': stdin_text, 'tools': tools, 'cwd': os.getcwd(), 'active': os.environ.get('PI_DELEGATE_ACTIVE', '')}\n"
            "with log_path.open('a') as fh:\n"
            "    fh.write(json.dumps(rec, ensure_ascii=False) + '\\n')\n"
            "def emit_answer(text):\n"
            "    print(json.dumps({'type': 'message_end', 'message': {'role': 'assistant', 'stopReason': 'stop', 'model': 'fake-model', 'usage': {'input': 10, 'output': 5, 'cacheRead': 0}, 'content': [{'type': 'text', 'text': text}]}}))\n"
            "    print(json.dumps({'type': 'agent_settled'}))\n"
            + script_body
            + "\n"
        )
        pi.chmod(0o755)
        return log_file

    def make_repo(self, name: str, files: dict[str, str]) -> Path:
        repo = self.work / name
        repo.mkdir()
        git = ["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t"]
        subprocess.run(git[:3] + ["init", "-q"], check=True)
        for rel, text in files.items():
            (repo / rel).parent.mkdir(parents=True, exist_ok=True)
            (repo / rel).write_text(text)
        subprocess.run(git + ["add", "-A"], check=True)
        subprocess.run(git + ["commit", "-qm", "init"], check=True)
        return repo

    def test_merge_serial_generated_accept_merge_accept_and_apply_keep_commits(self):
        acc_log = self.work / "acc.log"
        macc_log = self.work / "macc.log"
        repo = self.make_repo(
            "merge-basic",
            {
                "src/model.txt": "v1\n",
                "gen/out.txt": "GENERATED:v1\n",
                "docs/readme.md": "# Readme\n",
                ".delegate.json": json.dumps(
                    {
                        "remote": False,
                        "accept": f"echo accept >> {acc_log}",
                        "mergeAccept": f"echo mergeAccept >> {macc_log}",
                        "review": True,
                        "autoMerge": [
                            "docs/**",
                            "!docs/decision/**",
                            "apps/web/src/styles/**",
                            "add:**/tests/**",
                        ],
                        "generated": {
                            "paths": ["gen/"],
                            "inputs": ["src/"],
                            "command": "python3 -c \"from pathlib import Path; Path('gen/out.txt').write_text('GENERATED:' + Path('src/model.txt').read_text())\"",
                        },
                        "defaults": {"worktree": True, "protect": ["docs/decision/"]},
                    }
                ),
            },
        )
        self.install_scripted_pi(
            """
import subprocess
if '独立只读审查员' in stdin_text:
    emit_answer('PASS\\nLGTM')
elif 'TASK_MODEL' in stdin_text:
    Path('src/model.txt').write_text('v2\\n')
    Path('gen/out.txt').write_text('GENERATED:v2\\n')
    subprocess.run(['git', '-c', 'user.name=dev', '-c', 'user.email=d@t', 'add', '-A'], check=True)
    subprocess.run(['git', '-c', 'user.name=dev', '-c', 'user.email=d@t', 'commit', '-qm', 'feat: bump model to v2'], check=True)
    emit_answer('updated model')
elif 'TASK_DOCS' in stdin_text:
    Path('docs/guide.md').write_text('# Guide\\n')
    subprocess.run(['git', '-c', 'user.name=dev', '-c', 'user.email=d@t', 'add', '-A'], check=True)
    subprocess.run(['git', '-c', 'user.name=dev', '-c', 'user.email=d@t', 'commit', '-qm', 'docs: add user guide'], check=True)
    emit_answer('added guide')
else:
    emit_answer('noop')
"""
        )

        r1 = self.cli(
            "run", "--worktree", "--name", "model-v2", "TASK_MODEL update model", cwd=repo
        )
        self.assertEqual(r1.returncode, 0, r1.stderr)
        run1 = self.outcome(r1)["run"]

        r2 = self.cli(
            "run", "--worktree", "--name", "docs-guide", "TASK_DOCS add guide", cwd=repo
        )
        self.assertEqual(r2.returncode, 0, r2.stderr)
        run2 = self.outcome(r2)["run"]

        acc_log.write_text("")
        macc_log.write_text("")
        source_head_before = subprocess.check_output(
            ["git", "-C", str(repo), "rev-parse", "HEAD"], text=True
        ).strip()

        merged = self.cli("merge", "--ready", "--json", cwd=repo)
        self.assertEqual(merged.returncode, 0, merged.stderr + merged.stdout)
        rep = json.loads(merged.stdout)
        self.assertEqual(rep["mergedCount"], 2)
        self.assertEqual(rep["skippedCount"], 0)
        self.assertEqual(rep["autoMergeCount"], 1)
        self.assertTrue(rep["mergeAccept"]["ok"])

        # Source repo HEAD must NOT be touched by `delegate merge`.
        source_head_after_merge = subprocess.check_output(
            ["git", "-C", str(repo), "rev-parse", "HEAD"], text=True
        ).strip()
        self.assertEqual(source_head_before, source_head_after_merge)

        # Per-run accept ran twice (once per merged run) and mergeAccept ran once at end.
        self.assertEqual(len(acc_log.read_text().splitlines()), 2)
        self.assertEqual(len(macc_log.read_text().splitlines()), 1)

        items_by_run = {it["run"]: it for it in rep["items"]}
        self.assertEqual(items_by_run[run1]["status"], "MERGED")
        self.assertEqual(items_by_run[run1]["title"], "feat: bump model to v2")
        self.assertTrue(items_by_run[run1]["regenerated"])
        self.assertFalse(items_by_run[run1]["autoMerge"])

        self.assertEqual(items_by_run[run2]["status"], "MERGED")
        self.assertEqual(items_by_run[run2]["title"], "docs: add user guide")
        self.assertTrue(items_by_run[run2]["autoMerge"])

        # Report markdown has both rows, autoMerge labels, and apply --keep-commits hint.
        report_md = Path(rep["reportPath"]).read_text()
        self.assertIn("可自主合入", report_md)
        self.assertIn("待主控点头", report_md)
        self.assertIn(f"delegate apply --keep-commits {rep['mergeRun']}", report_md)

        # Events stream recorded merge_started, merge_item (x2), merge_done AND lane events.
        run_events = [e["kind"] for e in self.read_events() if e.get("run") == rep["mergeRun"]]
        merge_ev_kinds = [k for k in run_events if k.startswith("merge_")]
        self.assertEqual(merge_ev_kinds, ["merge_started", "merge_item", "merge_item", "merge_done"])
        # 1 generate + 2 per-run accept + 1 mergeAccept = 4 lane acquisitions
        self.assertEqual(run_events.count("lane_acquired"), 4)

        # `delegate merge list --json` exposes the merge run for CLI & TUI.
        listed = self.cli("merge", "list", "--json", cwd=repo)
        self.assertEqual(listed.returncode, 0, listed.stderr)
        listed_objs = [json.loads(ln) for ln in listed.stdout.splitlines() if ln.strip()]
        self.assertEqual(len(listed_objs), 1)
        self.assertEqual(listed_objs[0]["mergeRun"], rep["mergeRun"])

        # Controller lands the commit chain verbatim via `delegate apply --keep-commits <merge-run>`.
        applied = self.cli("apply", "--keep-commits", rep["mergeRun"], cwd=repo)
        self.assertEqual(applied.returncode, 0, applied.stderr + applied.stdout)

        log_out = subprocess.check_output(
            ["git", "-C", str(repo), "log", "--reverse", "--format=%B---END---", f"{source_head_before}..HEAD"],
            text=True,
        )
        msgs = [m.strip() for m in log_out.split("---END---") if m.strip()]
        self.assertEqual(len(msgs), 2)
        self.assertTrue(msgs[0].startswith("feat: bump model to v2"))
        self.assertIn(f"Delegate-Run: {run1}", msgs[0])
        self.assertTrue(msgs[1].startswith("docs: add user guide"))
        self.assertIn(f"Delegate-Run: {run2}", msgs[1])
        self.assertEqual((repo / "gen/out.txt").read_text(), "GENERATED:v2\n")
        self.assertEqual((repo / "docs/guide.md").read_text(), "# Guide\n")

        # Source runs are marked applied once the merge-run lands.
        runs_dir = Path(self.env["DELEGATE_RUNS"])
        self.assertTrue((runs_dir / run1 / ".applied").is_file())
        self.assertTrue((runs_dir / run2 / ".applied").is_file())

    def test_merge_skips_and_rolls_back_conflict_red_test_unsigned_invalid_sign_and_protect(self):
        repo = self.make_repo(
            "merge-skips",
            {
                "app.txt": "line1\n",
                "prompts/sys.md": "hello model\n",
                "docs/decision/001.md": "ADR 1\n",
                ".delegate.json": json.dumps(
                    {
                        "remote": False,
                        # Fails if BAD_MARKER appears in app.txt or extra.txt
                        "accept": "python3 -c \"from pathlib import Path; import sys; bad = any('BAD_MARKER' in p.read_text() for p in [Path('app.txt'), Path('extra.txt')] if p.exists()); sys.exit(1 if bad else 0)\"",
                        "review": False,
                        "controllerReviewPaths": ["prompts/**"],
                        "defaults": {"worktree": True, "protect": ["docs/decision/"]},
                    }
                ),
            },
        )
        self.install_scripted_pi(
            """
import subprocess
def commit(msg):
    subprocess.run(['git', '-c', 'user.name=dev', '-c', 'user.email=d@t', 'add', '-A'], check=True)
    subprocess.run(['git', '-c', 'user.name=dev', '-c', 'user.email=d@t', 'commit', '-qm', msg], check=True)

if 'RUN_GOOD_1' in stdin_text:
    Path('app.txt').write_text('from-run-1\\n')
    commit('feat: run 1 baseline')
elif 'RUN_CONFLICT' in stdin_text:
    Path('app.txt').write_text('from-run-conflict\\n')
    commit('feat: conflicting edit')
elif 'RUN_RED_TEST' in stdin_text:
    # Passes on its own? Wait: we inject BAD_MARKER only when combined with from-run-1!
    Path('extra.txt').write_text('ok\\n')
    commit('feat: breaks combined test')
elif 'RUN_UNSIGNED' in stdin_text:
    Path('prompts/sys.md').write_text('unsigned prompt change\\n')
    commit('prompt: unsigned')
elif 'RUN_BADSIGN' in stdin_text:
    Path('prompts/sys.md').write_text('signed then mutated\\n')
    commit('prompt: signed then mutated')
elif 'RUN_ADR' in stdin_text:
    Path('docs/decision/001.md').write_text('ADR 1 modified\\n')
    commit('docs: touch adr')
elif 'RUN_GOOD_2' in stdin_text:
    Path('other.txt').write_text('clean-run-2\\n')
    commit('feat: clean run 2')
emit_answer('done')
"""
        )

        # Update accept so RUN_RED_TEST fails ONLY when both app.txt=='from-run-1' and extra.txt exists!
        cfg = json.loads((repo / ".delegate.json").read_text())
        cfg["accept"] = (
            "python3 -c \"from pathlib import Path; import sys; "
            "sys.exit(1 if (Path('app.txt').read_text().strip() == 'from-run-1' and Path('extra.txt').exists()) else 0)\""
        )
        (repo / ".delegate.json").write_text(json.dumps(cfg))
        subprocess.run(
            ["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qam", "cfg"],
            check=True,
        )

        r_good1 = self.outcome(self.cli("run", "--worktree", "--name", "good1", "RUN_GOOD_1", cwd=repo))["run"]
        r_conf = self.outcome(self.cli("run", "--worktree", "--name", "conflict", "RUN_CONFLICT", cwd=repo))["run"]
        r_red = self.outcome(self.cli("run", "--worktree", "--name", "redtest", "RUN_RED_TEST", cwd=repo))["run"]
        r_unsig = self.outcome(self.cli("run", "--worktree", "--name", "unsigned", "RUN_UNSIGNED", cwd=repo))["run"]
        r_badsig = self.outcome(self.cli("run", "--worktree", "--name", "badsign", "RUN_BADSIGN", cwd=repo))["run"]
        # Sign r_badsig, then tamper with 1 character in its worktree so sign --check fails!
        sig = self.cli("sign", r_badsig, cwd=repo)
        self.assertEqual(sig.returncode, 0, sig.stderr)
        badsig_meta = json.loads((Path(self.env["DELEGATE_RUNS"]) / r_badsig / "meta.json").read_text())
        wt_badsig = Path(badsig_meta["worktree"]["path"])
        (wt_badsig / "prompts/sys.md").write_text("signed then mutated!\n")

        # Run touching protected ADR path using --allow at start (still rejected by merger protect rule!)
        r_adr = self.outcome(
            self.cli("run", "--worktree", "--allow", "docs/decision/", "--name", "adr", "RUN_ADR", cwd=repo)
        )["run"]
        r_good2 = self.outcome(self.cli("run", "--worktree", "--name", "good2", "RUN_GOOD_2", cwd=repo))["run"]

        res = self.cli(
            "merge",
            r_good1,
            r_conf,
            r_red,
            r_unsig,
            r_badsig,
            r_adr,
            r_good2,
            "--json",
            cwd=repo,
        )
        self.assertEqual(res.returncode, 0, res.stderr + res.stdout)
        rep = json.loads(res.stdout)
        self.assertEqual(rep["mergedCount"], 2)
        self.assertEqual(rep["skippedCount"], 5)

        by_run = {it["run"]: it for it in rep["items"]}
        self.assertEqual(by_run[r_good1]["status"], "MERGED")
        self.assertEqual(by_run[r_conf]["status"], "SKIPPED")
        self.assertIn("冲突", by_run[r_conf]["reason"])
        self.assertEqual(by_run[r_red]["status"], "SKIPPED")
        self.assertIn("验收未通过", by_run[r_red]["reason"])
        self.assertEqual(by_run[r_unsig]["status"], "SKIPPED")
        self.assertIn("未签字", by_run[r_unsig]["reason"])
        self.assertEqual(by_run[r_badsig]["status"], "SKIPPED")
        self.assertIn("签字已失效", by_run[r_badsig]["reason"])
        self.assertEqual(by_run[r_adr]["status"], "SKIPPED")
        self.assertIn("受保护路径", by_run[r_adr]["reason"])
        self.assertEqual(by_run[r_good2]["status"], "MERGED")

        # Integration worktree must have rolled back cleanly after every skip and contain only good1 + good2.
        wt = Path(rep["worktree"])
        self.assertEqual((wt / "app.txt").read_text(), "from-run-1\n")
        self.assertFalse((wt / "extra.txt").exists())
        self.assertEqual((wt / "prompts/sys.md").read_text(), "hello model\n")
        self.assertEqual((wt / "docs/decision/001.md").read_text(), "ADR 1\n")
        self.assertEqual((wt / "other.txt").read_text(), "clean-run-2\n")
        status_clean = subprocess.check_output(["git", "-C", str(wt), "status", "--porcelain"], text=True)
        self.assertEqual(status_clean.strip(), "")

    def test_merge_signed_controller_paths_and_automerge_add_only_tests(self):
        repo = self.make_repo(
            "merge-auto",
            {
                "prompts/sys.md": "v1\n",
                "tests/existing_test.py": "def test_ok(): pass\n",
                ".delegate.json": json.dumps(
                    {
                        "remote": False,
                        "accept": "true",
                        "review": True,
                        "controllerReviewPaths": ["prompts/**"],
                        "autoMerge": [
                            "docs/**",
                            "!docs/decision/**",
                            "add:**/tests/**",
                            "apps/web/src/styles/**",
                        ],
                        "defaults": {"worktree": True},
                    }
                ),
            },
        )
        self.install_scripted_pi(
            """
import subprocess
def commit(msg):
    subprocess.run(['git', '-c', 'user.name=dev', '-c', 'user.email=d@t', 'add', '-A'], check=True)
    subprocess.run(['git', '-c', 'user.name=dev', '-c', 'user.email=d@t', 'commit', '-qm', msg], check=True)

if '独立只读审查员' in stdin_text:
    emit_answer('PASS\\nLGTM')
elif 'TASK_PROMPT' in stdin_text:
    Path('prompts/sys.md').write_text('v2 signed\\n')
    commit('prompt: signed update')
    emit_answer('done')
elif 'TASK_NEW_TEST' in stdin_text:
    Path('tests/brand_new_test.py').write_text('def test_new(): pass\\n')
    commit('test: add brand new test')
    emit_answer('done')
elif 'TASK_MOD_TEST' in stdin_text:
    Path('tests/existing_test.py').write_text('def test_ok(): assert True\\n')
    commit('test: modify existing test')
    emit_answer('done')
"""
        )

        r_prompt = self.outcome(self.cli("run", "--worktree", "--name", "prompt", "TASK_PROMPT", cwd=repo))["run"]
        # Sign the controllerReviewPaths run so --ready picks it up!
        self.assertEqual(self.cli("sign", r_prompt, cwd=repo).returncode, 0)

        r_new_test = self.outcome(self.cli("run", "--worktree", "--name", "newtest", "TASK_NEW_TEST", cwd=repo))["run"]
        r_mod_test = self.outcome(self.cli("run", "--worktree", "--name", "modtest", "TASK_MOD_TEST", cwd=repo))["run"]

        res = self.cli("merge", "--ready", "--json", cwd=repo)
        self.assertEqual(res.returncode, 0, res.stderr + res.stdout)
        rep = json.loads(res.stdout)
        self.assertEqual(rep["mergedCount"], 3)
        by_run = {it["run"]: it for it in rep["items"]}
        # Signed controllerReviewPaths -> merged, NOT autoMerge
        self.assertEqual(by_run[r_prompt]["status"], "MERGED")
        self.assertFalse(by_run[r_prompt]["autoMerge"])
        # Purely added test file + PASS -> 可自主合入
        self.assertEqual(by_run[r_new_test]["status"], "MERGED")
        self.assertTrue(by_run[r_new_test]["autoMerge"])
        # Modified existing test file -> NOT autoMerge (add:**/tests/** rejects modifications)
        self.assertEqual(by_run[r_mod_test]["status"], "MERGED")
        self.assertFalse(by_run[r_mod_test]["autoMerge"])

    def test_radar_collect_fingerprint_zero_token_anchors_dedup_pick_and_schedule(self):
        repo = self.make_repo(
            "radar-demo",
            {
                "src/app.py": "def main():\n    return 1\n",
                "gaps.txt": "gap-1: missing retry\n",
                ".delegate.json": json.dumps(
                    {
                        "remote": False,
                        "radar": {
                            "collect": [
                                {"name": "recent-log", "command": "git log -n 3 --oneline"},
                                {"name": "gaps", "command": "cat gaps.txt"},
                            ],
                            "tier": "scout",
                        },
                    }
                ),
            },
        )
        head_short = subprocess.check_output(
            ["git", "-C", str(repo), "rev-parse", "--short", "HEAD"], text=True
        ).strip()

        pi_log = self.install_scripted_pi(
            f"""
if call_idx == 1:
    cands = [
        {{
            "title": "补齐重试逻辑单测",
            "evidence": ["src/app.py:1", "collect#2:L1"],
            "suggestion": "为 main 补齐失败重试测试。",
            "cost": "低（约 15 分钟）",
            "briefReady": True,
            "briefNote": "边界清楚"
        }},
        {{
            "title": "没有锚点的空泛建议",
            "evidence": ["感觉代码有点乱"],
            "suggestion": "重构一下",
            "cost": "高",
            "briefReady": False
        }},
        {{
            "title": "核对初始化提交边界",
            "evidence": ["commit {head_short}"],
            "suggestion": "确认初始化脚本幂等性。",
            "cost": "低",
            "briefReady": False,
            "briefNote": "需主控明确范围"
        }},
    ]
    emit_answer("```json\\n" + json.dumps(cands, ensure_ascii=False) + "\\n```")
else:
    # Second scout invocation: repeats first candidate (dedup) + adds 1 new candidate.
    cands = [
        {{
            "title": "补齐重试逻辑单测",
            "evidence": ["src/app.py:1", "collect#2:L1"],
            "suggestion": "重复条目应被去重。",
            "cost": "低",
            "briefReady": True
        }},
        {{
            "title": "处理新增缺口 gap-2",
            "evidence": ["collect#2:L2"],
            "suggestion": "补齐 gap-2 处理路径。",
            "cost": "中",
            "briefReady": True
        }},
    ]
    emit_answer(json.dumps(cands, ensure_ascii=False))
"""
        )

        # 1st run: fingerprint new -> scout runs in read-only mode, drops anchorless item, writes board.md.
        r1 = self.cli("radar", "--json", cwd=repo)
        self.assertEqual(r1.returncode, 0, r1.stderr + r1.stdout)
        d1 = json.loads(r1.stdout)
        self.assertTrue(d1["changed"])
        self.assertEqual(d1["added"], 2)
        self.assertEqual(d1["droppedNoAnchor"], 1)
        self.assertEqual(d1["total"], 2)

        calls_after_1 = [json.loads(ln) for ln in pi_log.read_text().splitlines() if ln.strip()]
        self.assertEqual(len(calls_after_1), 1)
        # Scout ran read-only tools and nesting guard is active (cannot delegate).
        self.assertEqual(calls_after_1[0]["tools"], "read,grep,find,ls")
        self.assertEqual(calls_after_1[0]["active"], "1")

        board_path = Path(d1["boardPath"])
        self.assertEqual(board_path, self.state_dir() / "radar" / "radar-demo" / "board.md")
        board_md = board_path.read_text()
        self.assertIn("补齐重试逻辑单测", board_md)
        self.assertIn("核对初始化提交边界", board_md)
        self.assertNotIn("没有锚点的空泛建议", board_md)
        self.assertIn("粗估成本", board_md)
        self.assertIn("可直接写成任务说明", board_md)

        # 2nd run: fingerprint unchanged -> ZERO tokens, scout NOT called, radar_unchanged event emitted!
        r2 = self.cli("radar", "--json", cwd=repo)
        self.assertEqual(r2.returncode, 0, r2.stderr + r2.stdout)
        d2 = json.loads(r2.stdout)
        self.assertFalse(d2["changed"])
        self.assertEqual(d2["tokens"], 0)
        calls_after_2 = [json.loads(ln) for ln in pi_log.read_text().splitlines() if ln.strip()]
        self.assertEqual(len(calls_after_2), 1, "scout must not run when fingerprint is unchanged")

        # 3rd run: change gaps.txt -> fingerprint changes -> scout sees previous board, Host deduplicates!
        (repo / "gaps.txt").write_text("gap-1: missing retry\ngap-2: timeout log\n")
        r3 = self.cli("radar", "--json", cwd=repo)
        self.assertEqual(r3.returncode, 0, r3.stderr + r3.stdout)
        d3 = json.loads(r3.stdout)
        self.assertTrue(d3["changed"])
        self.assertEqual(d3["added"], 1)
        self.assertEqual(d3["deduped"], 1)
        self.assertEqual(d3["total"], 3)

        calls_after_3 = [json.loads(ln) for ln in pi_log.read_text().splitlines() if ln.strip()]
        self.assertEqual(len(calls_after_3), 2)
        self.assertIn("补齐重试逻辑单测", calls_after_3[1]["stdin"])

        # List & pick: generates task brief draft without auto-dispatching any run!
        listed = self.cli("radar", "list", "--json", cwd=repo)
        self.assertEqual(listed.returncode, 0, listed.stderr)
        cands = [json.loads(ln) for ln in listed.stdout.splitlines() if ln.strip()]
        self.assertEqual(len(cands), 3)
        target_id = cands[0]["id"]

        draft_file = self.work / "picked-brief.md"
        picked = self.cli("radar", "pick", target_id, "--out", str(draft_file), cwd=repo)
        self.assertEqual(picked.returncode, 0, picked.stderr + picked.stdout)
        self.assertTrue(draft_file.is_file())
        draft_text = draft_file.read_text()
        self.assertIn(target_id, draft_text)
        self.assertIn("证据锚点", draft_text)
        self.assertIn("不会自动派活", draft_text)
        # Confirm no delegate run was created by pick.
        runs_dir = repo / ".local/run/delegate"
        self.assertFalse(runs_dir.exists() and any(runs_dir.iterdir()))

        # Events stream recorded radar_done, radar_unchanged, radar_done.
        radar_events = [e["kind"] for e in self.read_events() if e["kind"].startswith("radar_")]
        self.assertEqual(radar_events, ["radar_done", "radar_unchanged", "radar_done"])

        # Schedule prints systemd timer & cron instructions for 00:30-04:30 every 2h without installing.
        sched = self.cli("radar", "schedule", cwd=repo)
        self.assertEqual(sched.returncode, 0, sched.stderr)
        self.assertIn("OnCalendar=*-*-* 00,02,04:30:00", sched.stdout)
        self.assertIn("30 0,2,4 * * *", sched.stdout)

    def test_radar_scout_write_violation_fails_closed(self):
        repo = self.make_repo(
            "radar-ro-fail",
            {
                "README.md": "hi\n",
                ".delegate.json": json.dumps(
                    {
                        "remote": False,
                        "radar": {"collect": ["echo changed"]},
                    }
                ),
            },
        )
        self.install_scripted_pi(
            """
Path('ROGUE_WRITE.txt').write_text('oops\\n')
emit_answer('[{"title":"x","evidence":["README.md:1"],"suggestion":"s","cost":"低","briefReady":true}]')
"""
        )
        res = self.cli("radar", cwd=repo)
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("read-only", res.stderr)
        self.assertFalse((self.state_dir() / "radar" / "radar-ro-fail" / "board.md").exists())

    def test_concurrency_auto_derivation_overrides_env_and_shows_explicit_heavy_jobs(self):
        repo = self.make_repo("conc-demo", {"README.md": "hi\n", ".delegate.json": json.dumps({"remote": False})})
        cfg_file = Path(self.env["XDG_CONFIG_HOME"]) / "delegate/config.json"
        cfg = json.loads(cfg_file.read_text())
        # Legacy env.CARGO_BUILD_JOBS and host CARGO_BUILD_JOBS must NOT take effect.
        cfg["env"] = {"CARGO_BUILD_JOBS": "888"}
        cfg["maxHeavy"] = 6
        cfg_file.write_text(json.dumps(cfg))
        self.env["CARGO_BUILD_JOBS"] = "999"

        lane_json = json.loads(self.cli("lane", "--json", cwd=repo).stdout)
        threads = lane_json["threads"]
        budget = max(1, threads - 2)
        expected_pool = max(0, budget - 6)
        self.assertEqual(lane_json["budget"], budget)
        self.assertEqual(lane_json["maxHeavy"], 6)
        self.assertEqual(lane_json["poolTokens"], expected_pool)
        self.assertEqual(lane_json["pool"]["target"], expected_pool)
        self.assertEqual(lane_json["pool"]["free"], expected_pool)
        self.assertFalse(lane_json["heavyJobsExplicit"])
        expected_summary = f"线程 {threads} − 2 = {budget} · 令牌 {expected_pool}/{expected_pool} 空闲 {expected_pool}"
        self.assertIn(expected_summary, lane_json["concurrencySummary"])

        lane_text = self.cli("lane", cwd=repo).stdout
        self.assertIn(expected_summary, lane_text)

        # `delegate lane` injects proxy FIFO MAKEFLAGS/CARGO_MAKEFLAGS and overrides old CARGO_BUILD_JOBS (999/888).
        lane_run = self.cli(
            "lane",
            "--",
            "sh",
            "-c",
            'printf "%s|%s|%s" "$CARGO_BUILD_JOBS" "$MAKEFLAGS" "$CARGO_MAKEFLAGS"',
            cwd=repo,
        )
        self.assertEqual(lane_run.returncode, 0, lane_run.stderr)
        jobs_s, mf, cmf = lane_run.stdout.strip().split("|", 2)
        self.assertEqual(jobs_s, str(budget))
        self.assertIn("--jobserver-auth=fifo:", mf)
        self.assertEqual(mf, cmf)

        # Two concurrent lane commands display dynamic seniority shares `#1 u/s` and `#2 u/s`.
        p1 = subprocess.Popen(
            [str(fixtures.DELEGATE), "lane", "--label", "first-build", "sleep 20"],
            cwd=repo,
            env=self.env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        p2 = None
        try:
            deadline = time.time() + 10
            while time.time() < deadline:
                if "first-build" in self.cli("lane", cwd=repo).stdout:
                    break
                time.sleep(0.05)
            p2 = subprocess.Popen(
                [str(fixtures.DELEGATE), "lane", "--label", "second-build", "sleep 20"],
                cwd=repo,
                env=self.env,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            deadline = time.time() + 10
            live_json = None
            while time.time() < deadline:
                data = json.loads(self.cli("lane", "--json", cwd=repo).stdout)
                if len(data.get("pool", {}).get("leases", [])) >= 2:
                    live_json = data
                    break
                time.sleep(0.05)
            self.assertIsNotNone(live_json, "expected 2 active jobserver leases")
            leases = live_json["pool"]["leases"]
            self.assertEqual([l["rank"] for l in leases[:2]], [1, 2])
            self.assertGreaterEqual(leases[0]["share"], leases[1]["share"])
            self.assertGreaterEqual(leases[1]["share"], min(2, budget))
            if budget >= 4:
                self.assertEqual(leases[0]["share"] + leases[1]["share"], budget)
            live_text = self.cli("lane", cwd=repo).stdout
            self.assertIn("#1 ", live_text)
            self.assertIn("#2 ", live_text)
        finally:
            for proc in (p2, p1):
                if proc is not None:
                    proc.terminate()
                    try:
                        proc.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait()

        # Colleague agent & accept command also receive derived env (never 888 or 999), and accept gets jobserver FIFO.
        seen_file = self.work / "seen-jobs.txt"
        self.install_scripted_pi(
            f"Path({str(seen_file)!r}).write_text(os.environ.get('CARGO_BUILD_JOBS', ''))\n"
            "emit_answer('done')\n"
        )
        acc_file = self.work / "acc-jobs.txt"
        run_res = self.cli(
            "run",
            "--worktree",
            "--accept",
            f'printf "%s|%s" "$CARGO_BUILD_JOBS" "$MAKEFLAGS" > {acc_file}',
            "check jobs",
            cwd=repo,
        )
        self.assertEqual(run_res.returncode, 0, run_res.stderr)
        self.assertEqual(seen_file.read_text().strip(), str(budget))
        acc_jobs, acc_mf = acc_file.read_text().strip().split("|", 1)
        self.assertEqual(acc_jobs, str(budget))
        self.assertIn("--jobserver-auth=fifo:", acc_mf)

        # Explicit `heavyJobs` overrides per-command cap and marks 「显式」 in both `lane` and `status`.
        cfg["heavyJobs"] = 7
        cfg_file.write_text(json.dumps(cfg))

        lane_exp_json = json.loads(self.cli("lane", "--json", cwd=repo).stdout)
        self.assertEqual(lane_exp_json["heavyJobs"], 7)
        self.assertTrue(lane_exp_json["heavyJobsExplicit"])
        self.assertIn("显式", self.cli("lane", cwd=repo).stdout)

        st_human = self.cli("status", cwd=repo).stdout
        self.assertIn("显式", st_human)
        st_lines = [json.loads(ln) for ln in self.cli("status", "--json", cwd=repo).stdout.splitlines() if ln.strip()]
        summary_line = [x for x in st_lines if "statusSummary" in x][0]["statusSummary"]
        self.assertTrue(summary_line["concurrency"]["heavyJobsExplicit"])
        self.assertEqual(summary_line["concurrency"]["heavyJobs"], 7)
        self.assertEqual(summary_line["concurrency"]["pool"]["target"], expected_pool)

        lane_exp_run = self.cli("lane", "--", "sh", "-c", 'printf "%s" "$CARGO_BUILD_JOBS"', cwd=repo)
        self.assertEqual(lane_exp_run.stdout.strip(), "7")

        # Invalid heavyJobs: 0 is rejected.
        cfg["heavyJobs"] = 0
        cfg_file.write_text(json.dumps(cfg))
        bad = self.cli("lane", cwd=repo)
        self.assertEqual(bad.returncode, 2)
        self.assertIn("heavyJobs", bad.stderr)

    def test_concurrency_remote_probes_once_caches_and_falls_back_on_failure(self):
        import shutil

        if not shutil.which("rsync"):
            self.skipTest("rsync required")
        remote_home = self.work / "rhome"
        (remote_home / "bin").mkdir(parents=True)
        nproc_hits = self.work / "remote-nproc-hits"
        fake_nproc = remote_home / "bin/nproc"
        fake_nproc.write_text(
            f"#!{sys.executable}\n"
            f"with open({str(nproc_hits)!r}, 'a') as f: f.write('1\\n')\n"
            "print(20)\n"
        )
        fake_nproc.chmod(0o755)
        fake_cargo = remote_home / "bin/cargo"
        fake_cargo.write_text(
            f"#!{sys.executable}\n"
            "import sys\n"
            "if '--version' in sys.argv: print('cargo 1.0')\n"
        )
        fake_cargo.chmod(0o755)
        fake_ssh = self.bin / "ssh"
        offline_flag = self.work / "ssh-offline"
        fake_ssh.write_text(
            f"#!{sys.executable}\n"
            "import os, sys\n"
            "from pathlib import Path\n"
            f"if Path({str(offline_flag)!r}).exists():\n"
            "    print('offline', file=sys.stderr)\n"
            "    sys.exit(255)\n"
            f"os.environ['HOME'] = {str(remote_home)!r}\n"
            f"os.environ['PATH'] = {str(remote_home / 'bin')!r} + ':/usr/bin:/bin'\n"
            "os.execv('/bin/sh', ['sh', '-c', sys.argv[-1]])\n"
        )
        fake_ssh.chmod(0o755)
        real_rsync = shutil.which("rsync")
        fake_rsync = self.bin / "rsync"
        fake_rsync.write_text(
            f"#!{sys.executable}\n"
            "import os, sys\n"
            "a = sys.argv[1:]\n"
            "i = a.index('-e'); del a[i:i+2]\n"
            "a = [s.removeprefix('ci-box:') for s in a]\n"
            f"os.execv({real_rsync!r}, ['rsync', *a])\n"
        )
        fake_rsync.chmod(0o755)

        repo = self.make_repo("conc-remote", {"README.md": "hi\n"})
        cfg_file = Path(self.env["XDG_CONFIG_HOME"]) / "delegate/config.json"
        cfg = json.loads(cfg_file.read_text())
        cfg["remote"] = {"host": "ci-box", "root": "build"}
        cfg_file.write_text(json.dumps(cfg))

        # 1) `remote check` probes remote threads (20), caches result, derives budget=18 -> max(2, 18/12)=2 x 9.
        chk = self.cli("remote", "check", cwd=repo)
        self.assertEqual(chk.returncode, 0, chk.stderr + chk.stdout)
        self.assertIn("线程 20 − 2 = 18 → 2 条 × 9", chk.stdout)
        self.assertEqual(len(nproc_hits.read_text().splitlines()), 1)

        # 2) Second `remote check` reuses cache (nproc still hit only once!).
        chk2 = self.cli("remote", "check", cwd=repo)
        self.assertEqual(chk2.returncode, 0, chk2.stderr + chk2.stdout)
        self.assertEqual(len(nproc_hits.read_text().splitlines()), 1)

        lane_data = json.loads(self.cli("lane", "--json", cwd=repo).stdout)
        self.assertEqual(lane_data["remoteMaxHeavy"], 2)
        self.assertEqual(lane_data["remoteConcurrency"]["threads"], 20)
        self.assertEqual(lane_data["remoteConcurrency"]["heavyJobs"], 9)

        # 3) Uncached remote host that fails SSH falls back to configured remote.maxHeavy with diagnostic.
        cfg["remote"] = {"host": "ci-down", "root": "build", "maxHeavy": 3}
        cfg_file.write_text(json.dumps(cfg))
        offline_flag.touch()
        down = self.cli("remote", "check", cwd=repo)
        self.assertEqual(down.returncode, 75)
        self.assertIn("回退到配置值 maxHeavy=3", down.stderr)


if __name__ == "__main__":
    unittest.main()
