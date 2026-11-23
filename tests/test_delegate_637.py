"""6.37.0 regressions (W051 — compression as index for per-round results):
1. Intra-run review-rework preserves every round's answer as `round-1-result.md`,
   `round-2-result.md`, ... before overwriting `result.md` (which mirrors the latest round).
2. Review card (`delegate card <run>`) section 1 deterministically lists every round's
   original file path, line/char counts, and `delegate result` command for multi-round
   rework runs and `reply` chains (single-round card output stays unchanged).
3. `delegate result <run>` supports `--round N` (with `1..=total` out-of-range error),
   `--all`, `--rounds` (index only, does not mark read), `--path`, and `--json`, while
   keeping default stdout 100% backward compatible (with a 1-line stderr rounds hint).
4. Archival (`archive::preserve`) and lightweight apply retention keep all `round-N-result.md`
   files and scan them for referenced evidence; `card` and `result` still follow archived
   parent runs across a `reply` chain.
"""
import json
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate637Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def cache_dir(self) -> Path:
        return Path(self.env["XDG_CACHE_HOME"]) / "delegate"

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

    def test_review_rework_preserves_every_round_result_and_card_links_them(self):
        repo = self.repo({
            ".delegate.json": json.dumps({
                "review": True,
                "accept": "true",
            }),
            "app.py": "def run():\n    return 0\n",
        })
        self.install_scripted_pi(
            "if call_idx == 1:\n"
            "    Path('app.py').write_text('def run():\\n    return 1\\n')\n"
            "    emit_answer('初稿结论：吞吐 12400 qps，p99=12.4ms。\\n改了 app.py 返回 1。')\n"
            "elif call_idx == 2:\n"
            "    emit_answer('REWORK\\n边界值未处理，改为返回 2。')\n"
            "elif call_idx == 3:\n"
            "    Path('app.py').write_text('def run():\\n    return 2\\n')\n"
            "    emit_answer('返工第 1 轮：改为返回 2，复测 p99=11.8ms。')\n"
            "elif call_idx == 4:\n"
            "    emit_answer('REWORK\\n请再补一行文档注释。')\n"
            "elif call_idx == 5:\n"
            "    Path('app.py').write_text('def run():\\n    \"\"\"ok\"\"\"\\n    return 2\\n')\n"
            "    emit_answer('返工第 2 轮：补齐注释，全绿。')\n"
            "else:\n"
            "    emit_answer('PASS\\nClean and complete.\\n\\n## 主控重点看\\n无')\n"
        )

        st = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "w051-rework", "optimize run"))
        self.assertEqual(st["state"], "delivered")
        self.assertEqual(st["review"]["verdict"], "PASS")
        self.assertEqual(st["review"]["reworkRounds"], 2)

        run_dir = Path(st["dir"])
        run_id = st["run"]
        r1_file = run_dir / "round-1-result.md"
        r2_file = run_dir / "round-2-result.md"
        r3_file = run_dir / "round-3-result.md"
        main_file = run_dir / "result.md"
        for f in (r1_file, r2_file, r3_file, main_file):
            self.assertTrue(f.is_file(), f"missing {f}")

        self.assertIn("p99=12.4ms", r1_file.read_text(encoding="utf-8"))
        self.assertIn("p99=11.8ms", r2_file.read_text(encoding="utf-8"))
        self.assertIn("返工第 2 轮：补齐注释", r3_file.read_text(encoding="utf-8"))
        self.assertEqual(main_file.read_text(encoding="utf-8"), r3_file.read_text(encoding="utf-8"))

        # Review card section 1 points back to all 3 original round files.
        card_human = self.cli("card", run_id, cwd=repo, human=True)
        self.assertEqual(card_human.returncode, 0, card_human.stderr)
        sec1 = card_human.stdout.split("2. 风险项", 1)[0]
        self.assertIn("1. 做了什么", sec1)
        self.assertIn("各轮原文：", sec1)
        self.assertIn(str(r1_file), sec1)
        self.assertIn(str(r2_file), sec1)
        self.assertIn(str(r3_file), sec1)
        self.assertIn(f"delegate result {run_id} --round <1..=3> / --all", sec1)

        card_json_res = self.cli("card", "--json", run_id, cwd=repo)
        self.assertEqual(card_json_res.returncode, 0, card_json_res.stderr)
        card_doc = json.loads(card_json_res.stdout)
        rounds_meta = card_doc["author"]["rounds"]
        self.assertEqual(len(rounds_meta), 3)
        self.assertEqual([r["round"] for r in rounds_meta], [1, 2, 3])
        self.assertEqual([r["kind"] for r in rounds_meta], ["initial", "rework", "rework"])
        self.assertEqual([r["path"] for r in rounds_meta], [str(r1_file), str(r2_file), str(r3_file)])
        self.assertEqual([r["current"] for r in rounds_meta], [False, False, True])

        # `delegate result <run>` default stays backward compatible (latest body on stdout) + stderr hint.
        res_default = self.cli("result", run_id, cwd=repo)
        self.assertEqual(res_default.returncode, 0, res_default.stderr)
        self.assertEqual(res_default.stdout, r3_file.read_text(encoding="utf-8"))
        self.assertIn("rounds: 共 3 轮", res_default.stderr)

        # `--round N` reads each preserved round verbatim.
        res_r1 = self.cli("result", "--round", "1", run_id, cwd=repo)
        self.assertEqual(res_r1.returncode, 0, res_r1.stderr)
        self.assertEqual(res_r1.stdout, r1_file.read_text(encoding="utf-8"))
        res_r2_path = self.cli("result", "--round", "2", "--path", run_id, cwd=repo)
        self.assertEqual(res_r2_path.returncode, 0, res_r2_path.stderr)
        self.assertEqual(res_r2_path.stdout.strip(), str(r2_file))

        # Out-of-range and invalid `--round` fail with exit 2 and valid range hint.
        res_oob = self.cli("result", "--round", "4", run_id, cwd=repo)
        self.assertEqual(res_oob.returncode, 2)
        self.assertIn("1..=3", res_oob.stderr)
        res_zero = self.cli("result", "--round", "0", run_id, cwd=repo)
        self.assertEqual(res_zero.returncode, 2)

        # Mutually exclusive flags fail with exit 2.
        self.assertEqual(self.cli("result", "--round", "1", "--all", run_id, cwd=repo).returncode, 2)
        self.assertEqual(self.cli("result", "--path", "--json", run_id, cwd=repo).returncode, 2)

        # `--all` prints all rounds with headers & paths; `--all --path` prints paths line by line.
        res_all = self.cli("result", "--all", run_id, cwd=repo)
        self.assertEqual(res_all.returncode, 0, res_all.stderr)
        self.assertIn("===== round 1/3", res_all.stdout)
        self.assertIn("p99=12.4ms", res_all.stdout)
        self.assertIn("===== round 2/3", res_all.stdout)
        self.assertIn("p99=11.8ms", res_all.stdout)
        self.assertIn("===== round 3/3", res_all.stdout)
        self.assertIn("返工第 2 轮：补齐注释", res_all.stdout)
        res_all_paths = self.cli("result", "--all", "--path", run_id, cwd=repo)
        self.assertEqual(res_all_paths.stdout.splitlines(), [str(r1_file), str(r2_file), str(r3_file)])

        # `--rounds --json` omits body text; `--all --json` includes body text.
        res_idx_json = json.loads(self.cli("result", "--rounds", "--json", run_id, cwd=repo).stdout)
        self.assertEqual(res_idx_json["totalRounds"], 3)
        self.assertTrue(all("text" not in item for item in res_idx_json["rounds"]))
        res_all_json = json.loads(self.cli("result", "--all", "--json", run_id, cwd=repo).stdout)
        self.assertIn("p99=12.4ms", res_all_json["rounds"][0]["text"])
        self.assertIn("返工第 2 轮", res_all_json["rounds"][2]["text"])

    def test_result_reading_non_current_round_or_rounds_index_does_not_mark_delivered(self):
        repo = self.repo({
            ".delegate.json": json.dumps({"review": True, "accept": "true"}),
            "a.txt": "v0\n",
        })
        self.install_scripted_pi(
            "if call_idx == 1:\n"
            "    Path('a.txt').write_text('v1\\n')\n"
            "    emit_answer('round 1 answer')\n"
            "elif call_idx == 2:\n"
            "    emit_answer('REWORK\\nchange to v2')\n"
            "elif call_idx == 3:\n"
            "    Path('a.txt').write_text('v2\\n')\n"
            "    emit_answer('round 2 answer')\n"
            "else:\n"
            "    emit_answer('PASS\\ngood\\n\\n## 主控重点看\\n无')\n"
        )

        started = self.outcome(
            self.cli("start", "--worktree", "--workdir", repo, "--name", "w051-mark", "update a")
        )
        run_dir = Path(started["dir"])
        run_id = started["run"]
        self.until(lambda: (run_dir / "exit_code").is_file(), "run finished", timeout=20)
        self.assertFalse((run_dir / ".delivered").exists())

        # Listing `--rounds` must NOT mark delivered.
        idx = self.cli("result", "--rounds", run_id, cwd=repo)
        self.assertEqual(idx.returncode, 0, idx.stderr)
        self.assertIn("共 2 轮", idx.stdout)
        self.assertFalse((run_dir / ".delivered").exists())

        # Reading historical round 1 must NOT mark the unread latest round delivered.
        r1 = self.cli("result", "--round", "1", run_id, cwd=repo)
        self.assertEqual(r1.returncode, 0, r1.stderr)
        self.assertEqual(r1.stdout.strip(), "round 1 answer")
        self.assertFalse((run_dir / ".delivered").exists())

        # Reading the current latest round (round 2) marks delivered.
        r2 = self.cli("result", "--round", "2", run_id, cwd=repo)
        self.assertEqual(r2.returncode, 0, r2.stderr)
        self.assertEqual(r2.stdout.strip(), "round 2 answer")
        self.assertTrue((run_dir / ".delivered").exists())

    def test_reply_chain_and_archived_parent_rounds_in_card_and_result(self):
        repo = self.repo({
            ".gitignore": "*.log\n",
            ".delegate.json": json.dumps({"review": True, "accept": "true"}),
            "lib.py": "x = 0\n",
        })
        self.install_scripted_pi(
            "if call_idx == 1:\n"
            "    Path('lib.py').write_text('x = 1\\n')\n"
            "    Path('bench-r1.log').write_text('latency_p99=9.1ms\\n')\n"
            "    emit_answer('r1-initial: 见 bench-r1.log，p99=9.1ms')\n"
            "elif call_idx == 2:\n"
            "    emit_answer('REWORK\\nset x = 2')\n"
            "elif call_idx == 3:\n"
            "    Path('lib.py').write_text('x = 2\\n')\n"
            "    emit_answer('r1-rework: x=2 done')\n"
            "elif call_idx == 4:\n"
            "    emit_answer('PASS\\nok\\n\\n## 主控重点看\\n无')\n"
            "elif call_idx == 5:\n"
            "    Path('lib.py').write_text('x = 3\\n')\n"
            "    emit_answer('r2-reply: x=3 done')\n"
            "else:\n"
            "    emit_answer('PASS\\nok\\n\\n## 主控重点看\\n无')\n"
        )

        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "chain-task", "phase 1"))
        self.assertEqual(r1["state"], "delivered")
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "phase 2: set x=3"))
        self.assertEqual(r2["state"], "delivered")

        r1_dir = Path(r1["dir"])
        r2_dir = Path(r2["dir"])

        # Card on r2 shows all 3 rounds across r1 + r2; `--round` scopes to r2 only.
        card_total = json.loads(self.cli("card", "--json", r2["run"], cwd=repo).stdout)
        self.assertEqual(len(card_total["author"]["rounds"]), 3)
        self.assertEqual(
            [item["kind"] for item in card_total["author"]["rounds"]],
            ["initial", "rework", "reply"],
        )
        self.assertEqual(card_total["author"]["rounds"][0]["run"], r1["run"])
        self.assertEqual(card_total["author"]["rounds"][2]["run"], r2["run"])

        card_round_only = json.loads(self.cli("card", "--json", "--round", r2["run"], cwd=repo).stdout)
        self.assertEqual(len(card_round_only["author"]["rounds"]), 1)
        self.assertEqual(card_round_only["author"]["rounds"][0]["path"], str(r2_dir / "result.md"))

        # `delegate result <conversation-name>` resolves to r2 and sees all 3 rounds.
        by_name_r1 = self.cli("result", "--round", "1", "chain-task", cwd=repo)
        self.assertEqual(by_name_r1.returncode, 0, by_name_r1.stderr)
        self.assertIn("bench-r1.log", by_name_r1.stdout)
        by_name_all = self.cli("result", "--all", "chain-task", cwd=repo)
        self.assertIn("r1-initial", by_name_all.stdout)
        self.assertIn("r1-rework", by_name_all.stdout)
        self.assertIn("r2-reply", by_name_all.stdout)

        # Clean r1 synchronously so r1 is archived to `<cache>/archive/<r1>/record/`.
        cleaned = self.cli("clean", "--force", "--sync", r1["run"], cwd=repo)
        self.assertEqual(cleaned.returncode, 0, cleaned.stdout + cleaned.stderr)
        self.assertFalse(r1_dir.exists())

        archive_dir = self.cache_dir() / "archive" / r1["run"]
        self.assertTrue((archive_dir / "record" / "round-1-result.md").is_file())
        self.assertTrue((archive_dir / "record" / "round-2-result.md").is_file())
        # Ignored evidence file mentioned ONLY in round-1-result.md was archived too.
        self.assertTrue((archive_dir / "files" / "bench-r1.log").is_file())

        # Even after r1 is cleaned, `card r2` and `result r2 --round 1` still find r1's archived rounds!
        card_after_clean = self.cli("card", r2["run"], cwd=repo, human=True)
        self.assertEqual(card_after_clean.returncode, 0, card_after_clean.stderr)
        self.assertIn(str(archive_dir / "record" / "round-1-result.md"), card_after_clean.stdout)
        self.assertIn(str(archive_dir / "record" / "round-2-result.md"), card_after_clean.stdout)
        self.assertIn(str(r2_dir / "result.md"), card_after_clean.stdout)

        res_r1_after_clean = self.cli("result", "--round", "1", r2["run"], cwd=repo)
        self.assertEqual(res_r1_after_clean.returncode, 0, res_r1_after_clean.stderr)
        self.assertIn("p99=9.1ms", res_r1_after_clean.stdout)

    def test_single_round_card_and_result_stay_unchanged(self):
        repo = self.repo({"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("single clean answer"), fixtures.SETTLED])
        st = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "single"))
        run_dir = Path(st["dir"])
        self.assertTrue((run_dir / "result.md").is_file())
        self.assertFalse((run_dir / "round-1-result.md").exists())

        card_human = self.cli("card", st["run"], cwd=repo, human=True)
        self.assertEqual(card_human.returncode, 0, card_human.stderr)
        self.assertNotIn("各轮原文：", card_human.stdout)

        res = self.cli("result", st["run"], cwd=repo)
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(res.stdout, "single clean answer\n")
        self.assertNotIn("rounds:", res.stderr)

        res_r1 = self.cli("result", "--round", "1", st["run"], cwd=repo)
        self.assertEqual(res_r1.returncode, 0, res_r1.stderr)
        self.assertEqual(res_r1.stdout, "single clean answer\n")

    def test_apply_preserves_round_results_in_archive_and_card_result_still_work(self):
        repo = self.repo({
            ".delegate.json": json.dumps({"review": True, "accept": "true"}),
            "mod.py": "VAL = 0\n",
        })
        self.install_scripted_pi(
            "if call_idx == 1:\n"
            "    Path('mod.py').write_text('VAL = 1\\n')\n"
            "    emit_answer('apply-r1: measured 42ms')\n"
            "elif call_idx == 2:\n"
            "    emit_answer('REWORK\\nset VAL = 2')\n"
            "elif call_idx == 3:\n"
            "    Path('mod.py').write_text('VAL = 2\\n')\n"
            "    emit_answer('apply-r2: updated VAL=2')\n"
            "else:\n"
            "    emit_answer('PASS\\nok\\n\\n## 主控重点看\\n无')\n"
        )
        st = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "w051-apply", "set val"))
        self.assertEqual(st["state"], "delivered")
        run_id = st["run"]
        applied = self.cli("apply", run_id, cwd=repo)
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertEqual((repo / "mod.py").read_text(encoding="utf-8"), "VAL = 2\n")

        # After apply (whether auto-cleaned to archive or kept as lightweight `.applied-cleaned`),
        # both round results remain accessible via `delegate result` and `delegate card`.
        r1_out = self.cli("result", "--round", "1", run_id, cwd=repo)
        self.assertEqual(r1_out.returncode, 0, r1_out.stderr)
        self.assertIn("measured 42ms", r1_out.stdout)
        r2_out = self.cli("result", "--round", "2", run_id, cwd=repo)
        self.assertEqual(r2_out.returncode, 0, r2_out.stderr)
        self.assertIn("updated VAL=2", r2_out.stdout)
        card_out = self.cli("card", run_id, cwd=repo, human=True)
        self.assertEqual(card_out.returncode, 0, card_out.stderr)
        self.assertIn("round-1-result.md", card_out.stdout)
        self.assertIn("round-2-result.md", card_out.stdout)


if __name__ == "__main__":
    unittest.main()
