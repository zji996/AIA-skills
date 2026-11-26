"""End-to-end tests for incremental review on rework/reply, pure sync-round skip,
conflict resolution extra-line detection, directory copy diff (-M -C -C),
--review-full override, and card review scope display.
"""
import json
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateIncrementalReviewTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def write_user_config(self, doc: dict) -> None:
        cfg_dir = Path(self.env["XDG_CONFIG_HOME"]) / "delegate"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "config.json").write_text(json.dumps(doc))
        for k in (
            "DELEGATE_SCOUT_MODEL",
            "DELEGATE_REVIEW_MODEL",
            "DELEGATE_WRITE_MODEL",
            "DELEGATE_THINKING",
        ):
            self.env.pop(k, None)

    def install_scripted_agy(self, script_body: str) -> Path:
        log_file = self.work / "agy-calls.jsonl"
        if log_file.exists():
            log_file.unlink()
        agy = self.work / "bin" / "agy"
        agy.parent.mkdir(parents=True, exist_ok=True)
        agy.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "argv = sys.argv[1:]\n"
            "if '--version' in argv:\n"
            "    print('1.5.15')\n"
            "    sys.exit(0)\n"
            "model = ''\n"
            "conv_in = ''\n"
            "for i, a in enumerate(argv):\n"
            "    if a == '--model' and i + 1 < len(argv):\n"
            "        model = argv[i + 1]\n"
            "    elif a == '--conversation' and i + 1 < len(argv):\n"
            "        conv_in = argv[i + 1]\n"
            "if not model:\n"
            "    sys.exit(0)\n"
            "raw_in = sys.stdin.readline()\n"
            "msg = json.loads(raw_in.strip().splitlines()[-1]) if raw_in.strip() else {}\n"
            "content = msg.get('message', {}).get('content', '')\n"
            "is_reviewer = ('独立只读审查员' in content or '增量 diff' in content or '主控重点看' in content) and model == 'rev-model'\n"
            f"log_path = Path({str(log_file)!r})\n"
            "calls = log_path.read_text().splitlines() if log_path.exists() else []\n"
            "call_idx = len(calls) + 1\n"
            "with log_path.open('a') as f:\n"
            "    f.write(json.dumps({'idx': call_idx, 'model': model, 'conv_in': conv_in, 'is_reviewer': is_reviewer, 'content': content}) + '\\n')\n"
            "conv_id = conv_in or f'conv-{model}-{call_idx}'\n"
            "def emit_ok(text, cid=None):\n"
            "    c = cid or conv_id\n"
            "    print(json.dumps({'event':'init','conversation_id':c}), flush=True)\n"
            "    print(json.dumps({'event':'step_update','step_update':{'step_index':1,'step_type':'agent_response','state':'DONE','text_delta':text}}), flush=True)\n"
            "    print(json.dumps({'event':'result','result':{'conversation_id':c,'status':'SUCCESS','response':text,'usage':{'input_tokens':20,'output_tokens':10,'cache_read_tokens':5}}}), flush=True)\n"
            + script_body
            + "\n"
        )
        agy.chmod(0o755)
        self.env["PATH"] = f"{agy.parent}:{self.env.get('PATH', '')}"
        self.write_user_config(
            {
                "models": {
                    "writer": "agy/writer-model",
                    "reviewer": "agy/rev-model",
                },
                "tiers": {
                    "scout": "reviewer",
                    "review": "reviewer",
                    "write": "writer",
                },
            }
        )
        return log_file

    def read_calls(self, log_file: Path) -> list[dict]:
        if not log_file.exists():
            return []
        return [json.loads(line) for line in log_file.read_text().splitlines() if line.strip()]

    def git_commit(self, repo: Path, files: dict[str, str], msg: str) -> None:
        for rel, body in files.items():
            p = repo / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(body)
        subprocess.run(["git", "-C", str(repo), "add", "."], check=True)
        subprocess.run(["git", "-C", str(repo), "commit", "-q", "-m", msg], check=True)

    def test_rework_round_review_gets_incremental_diff_and_prev_verdict(self):
        script = (
            "if model == 'rev-model':\n"
            "    if not conv_in:\n"
            "        emit_ok('REWORK\\n请修复 b.txt 的拼写问题\\n\\n## 主控重点看\\n- [低] b.txt:1 — 拼写\\n')\n"
            "    else:\n"
            "        emit_ok('PASS\\n已修复 b.txt 拼写\\n\\n## 主控重点看\\n无\\n')\n"
            "else:\n"
            "    if '请修复 b.txt 的拼写问题' in content:\n"
            "        Path('b.txt').write_text('b fixed\\n')\n"
            "        emit_ok('已修复 b.txt')\n"
            "    else:\n"
            "        Path('a.txt').write_text('a changed\\n')\n"
            "        Path('b.txt').write_text('b typo\\n')\n"
            "        emit_ok('已改动 a.txt 和 b.txt')\n"
        )
        log_file = self.install_scripted_agy(script)
        repo = self.repo(
            {
                "a.txt": "a orig\n",
                "b.txt": "b orig\n",
                ".delegate.json": json.dumps({"accept": "true", "review": True, "maxRework": 5}),
            }
        )
        res = self.cli("run", "--name", "rework-inc", "--workdir", str(repo), "--worktree", "改 a 和 b")
        self.assertEqual(res.returncode, 0, res.stderr)

        rev_calls = [c for c in self.read_calls(log_file) if c["model"] == "rev-model"]
        self.assertEqual(len(rev_calls), 2, rev_calls)
        r1, r2 = rev_calls
        # Round 1: full diff contains both a.txt and b.txt
        self.assertIn("a.txt", r1["content"])
        self.assertIn("b.txt", r1["content"])
        # Round 2: same session, incremental rule, previous verdict verbatim, ONLY b.txt in diff
        self.assertTrue(r2["conv_in"], "round 2 reviewer should continue round 1 session")
        self.assertIn("上一轮已 PASS 的部分不重审，只核对本轮改动是否引入问题、是否修好上一轮指出的问题", r2["content"])
        self.assertIn("请修复 b.txt 的拼写问题", r2["content"])
        self.assertIn("b fixed", r2["content"])
        self.assertNotIn("a changed", r2["content"])

        card = self.cli("card", "rework-inc", cwd=repo)
        self.assertEqual(card.returncode, 0, card.stderr)
        self.assertIn("3. 测试与验收（审查范围：增量（1 个文件））", card.stdout)

    def test_reply_inherits_review_and_review_full_overrides(self):
        script = (
            "if model == 'rev-model':\n"
            "    emit_ok('PASS\\n初审通过：a.txt 正常\\n\\n## 主控重点看\\n无\\n')\n"
            "else:\n"
            "    if '改 c' in content:\n"
            "        Path('c.txt').write_text('c changed\\n')\n"
            "        emit_ok('改了 c.txt')\n"
            "    elif '改 b' in content:\n"
            "        Path('b.txt').write_text('b changed\\n')\n"
            "        emit_ok('改了 b.txt')\n"
            "    else:\n"
            "        Path('a.txt').write_text('a changed\\n')\n"
            "        emit_ok('改了 a.txt')\n"
        )
        log_file = self.install_scripted_agy(script)
        repo = self.repo(
            {
                "a.txt": "a0\n",
                "b.txt": "b0\n",
                "c.txt": "c0\n",
                ".delegate.json": json.dumps({"accept": "true", "review": True, "maxRework": 5}),
            }
        )
        r1 = self.cli("run", "--name", "chain-rev", "--workdir", str(repo), "--worktree", "改 a")
        self.assertEqual(r1.returncode, 0, r1.stderr)
        c1 = self.cli("card", "chain-rev", cwd=repo)
        self.assertIn("3. 测试与验收（审查范围：全量）", c1.stdout)

        # Reply 1 (default incremental): inherits reviewed tree + reviewer session + prev verdict.
        r2 = self.cli("reply", "chain-rev", "--wait", "改 b", cwd=repo)
        self.assertEqual(r2.returncode, 0, r2.stderr)
        rev_calls = [c for c in self.read_calls(log_file) if c["model"] == "rev-model"]
        self.assertEqual(len(rev_calls), 2, rev_calls)
        self.assertTrue(rev_calls[1]["conv_in"], "reply should continue parent reviewer session")
        self.assertIn("初审通过：a.txt 正常", rev_calls[1]["content"])
        self.assertIn("上一轮已 PASS 的部分不重审", rev_calls[1]["content"])
        self.assertIn("b changed", rev_calls[1]["content"])
        self.assertNotIn("a changed", rev_calls[1]["content"])
        c2 = self.cli("card", "last", cwd=repo)
        self.assertIn("3. 测试与验收（审查范围：增量（1 个文件））", c2.stdout)

        # Reply 2 with --review-full: forces full net diff and fresh reviewer session.
        r3 = self.cli("reply", "last", "--wait", "--review-full", "改 c", cwd=repo)
        self.assertEqual(r3.returncode, 0, r3.stderr)
        rev_calls = [c for c in self.read_calls(log_file) if c["model"] == "rev-model"]
        self.assertEqual(len(rev_calls), 3, rev_calls)
        self.assertEqual(rev_calls[2]["conv_in"], "", "--review-full should start full review")
        self.assertIn("a changed", rev_calls[2]["content"])
        self.assertIn("b changed", rev_calls[2]["content"])
        self.assertIn("c changed", rev_calls[2]["content"])
        c3 = self.cli("card", "last", cwd=repo)
        self.assertIn("3. 测试与验收（审查范围：全量）", c3.stdout)

    def test_pure_sync_round_skips_review_and_same_file_automerge_still_reviews(self):
        script = (
            "if model == 'rev-model':\n"
            "    emit_ok('PASS\\n审查通过\\n\\n## 主控重点看\\n无\\n')\n"
            "else:\n"
            "    if '同步' in content:\n"
            "        emit_ok('已完成主干同步，无行为改动')\n"
            "    else:\n"
            "        Path('branch.txt').write_text('top\\nm2\\nm3\\nm4\\nm5\\nbranch-edit\\n')\n"
            "        emit_ok('改了 branch.txt 底部')\n"
        )
        log_file = self.install_scripted_agy(script)
        repo = self.repo(
            {
                "branch.txt": "top\nm2\nm3\nm4\nm5\nbottom\n",
                "mainline.txt": "m0\n",
                ".delegate.json": json.dumps({"accept": "true", "review": True, "maxRework": 5}),
            }
        )
        r1 = self.cli("run", "--name", "sync-skip", "--workdir", str(repo), "--worktree", "改 branch")
        self.assertEqual(r1.returncode, 0, r1.stderr)
        self.assertEqual(len([c for c in self.read_calls(log_file) if c["model"] == "rev-model"]), 1)

        # 1. Pure disjoint mainline advance -> zero reviewer calls, card marks sync_skip.
        self.git_commit(repo, {"mainline.txt": "m1\n"}, "mainline disjoint advance")
        r2 = self.cli("reply", "sync-skip", "--sync", "--wait", "同步主干", cwd=repo)
        self.assertEqual(r2.returncode, 0, r2.stderr)
        self.assertEqual(len([c for c in self.read_calls(log_file) if c["model"] == "rev-model"]), 1)
        st2 = json.loads(self.cli("status", "last", "--json", cwd=repo).stdout)
        self.assertEqual(st2["state"], "delivered")
        self.assertEqual(st2["review"]["verdict"], "PASS")
        self.assertEqual(st2["review"]["scope"], "sync_skip")
        self.assertTrue(st2["review"]["syncSkip"])
        card2 = self.cli("card", "last", cwd=repo)
        self.assertIn("同步轮，沿用上一轮审查 PASS", card2.stdout)
        self.assertIn("3. 测试与验收（审查范围：同步轮未审）", card2.stdout)

        # 2. Pure sync with FAILING acceptance -> must NOT pass straight through!
        self.git_commit(repo, {"mainline.txt": "m2\n"}, "mainline disjoint advance 2")
        r3 = self.cli("reply", "last", "--sync", "--accept", "false", "--wait", "同步主干", cwd=repo)
        self.assertNotEqual(r3.returncode, 0)
        self.assertEqual(len([c for c in self.read_calls(log_file) if c["model"] == "rev-model"]), 1)
        st3 = json.loads(self.cli("status", "last", "--json", cwd=repo).stdout)
        self.assertEqual(st3["state"], "rejected")

        # 3. Mainline touches SAME file `branch.txt` (auto-mergeable top line) -> must still review incrementally!
        self.git_commit(repo, {"branch.txt": "top-from-mainline\nm2\nm3\nm4\nm5\nbottom\n"}, "mainline edits branch.txt top")
        r4 = self.cli("reply", "last", "--sync", "--accept", "true", "--wait", "同步主干", cwd=repo)
        self.assertEqual(r4.returncode, 0, r4.stderr)
        rev_calls = [c for c in self.read_calls(log_file) if c["model"] == "rev-model"]
        self.assertEqual(len(rev_calls), 2, "same-file auto-merge must trigger incremental review")
        self.assertIn("top-from-mainline", rev_calls[1]["content"])
        card4 = self.cli("card", "last", cwd=repo)
        self.assertIn("3. 测试与验收（审查范围：增量（1 个文件））", card4.stdout)

    def test_conflict_resolution_pure_vs_extra_line_edit(self):
        script = (
            "if model == 'rev-model':\n"
            "    emit_ok('PASS\\n审查通过\\n\\n## 主控重点看\\n无\\n')\n"
            "else:\n"
            "    if '只解冲突' in content:\n"
            "        Path('shared.txt').write_text('h1\\nh2\\nresolved-clean\\nf1\\nf2\\n')\n"
            "        emit_ok('只解了冲突，无行为改动')\n"
            "    elif '改额外行' in content:\n"
            "        Path('shared.txt').write_text('h1\\nh2\\nresolved-v2\\nf1\\nf2-extra-edit\\n')\n"
            "        emit_ok('解了冲突并顺手改了 f2')\n"
            "    else:\n"
            "        Path('shared.txt').write_text('h1\\nh2\\nbranch-mid\\nf1\\nf2\\n')\n"
            "        emit_ok('改了 shared.txt 中间行')\n"
        )
        log_file = self.install_scripted_agy(script)
        repo = self.repo(
            {
                "shared.txt": "h1\nh2\norig-mid\nf1\nf2\n",
                ".delegate.json": json.dumps({"accept": "true", "review": True, "maxRework": 5}),
            }
        )
        r1 = self.cli("run", "--name", "conflict-sync", "--workdir", str(repo), "--worktree", "改 shared")
        self.assertEqual(r1.returncode, 0, r1.stderr)
        self.assertEqual(len([c for c in self.read_calls(log_file) if c["model"] == "rev-model"]), 1)

        # Case A: conflict on middle line, author ONLY resolves conflict hunk -> skip review!
        self.git_commit(repo, {"shared.txt": "h1\nh2\nmain-mid-1\nf1\nf2\n"}, "main conflict 1")
        r2 = self.cli("reply", "conflict-sync", "--sync=agent", "--wait", "只解冲突", cwd=repo)
        self.assertEqual(r2.returncode, 0, r2.stderr)
        self.assertEqual(len([c for c in self.read_calls(log_file) if c["model"] == "rev-model"]), 1)
        card2 = self.cli("card", "last", cwd=repo)
        self.assertIn("同步轮，沿用上一轮审查 PASS", card2.stdout)
        self.assertIn("3. 测试与验收（审查范围：同步轮未审）", card2.stdout)

        # Case B: another conflict on middle line, author ALSO modifies non-conflict line f2 -> incremental review!
        self.git_commit(repo, {"shared.txt": "h1\nh2\nmain-mid-2\nf1\nf2\n"}, "main conflict 2")
        r3 = self.cli("reply", "last", "--sync=agent", "--wait", "改额外行", cwd=repo)
        self.assertEqual(r3.returncode, 0, r3.stderr)
        rev_calls = [c for c in self.read_calls(log_file) if c["model"] == "rev-model"]
        self.assertEqual(len(rev_calls), 2, "conflict resolution editing non-conflict line must trigger incremental review")
        self.assertIn("f2-extra-edit", rev_calls[1]["content"])
        card3 = self.cli("card", "last", cwd=repo)
        self.assertIn("3. 测试与验收（审查范围：增量（1 个文件））", card3.stdout)

    def test_dir_copy_reviewer_diff_only_shows_modified_file(self):
        script = (
            "if model == 'rev-model':\n"
            "    emit_ok('PASS\\n目录复制仅改了 f2\\n\\n## 主控重点看\\n无\\n')\n"
            "else:\n"
            "    import shutil\n"
            "    shutil.copytree('harness_v1', 'harness_v2')\n"
            "    p = Path('harness_v2/f2.txt')\n"
            "    p.write_text(p.read_text() + 'version 2 tweak\\n')\n"
            "    emit_ok('升级 harness_v2')\n"
        )
        log_file = self.install_scripted_agy(script)
        init_files = {
            ".delegate.json": json.dumps({"accept": "true", "review": True}),
        }
        for i in range(5):
            init_files[f"harness_v1/f{i}.txt"] = f"line1\nline2\nline3\nline4\nfile {i}\n"
        repo = self.repo(init_files)

        res = self.cli("run", "--name", "dir-copy", "--workdir", str(repo), "--worktree", "升级 harness")
        self.assertEqual(res.returncode, 0, res.stderr)
        rev_calls = [c for c in self.read_calls(log_file) if c["model"] == "rev-model"]
        self.assertEqual(len(rev_calls), 1)
        prompt = rev_calls[0]["content"]
        self.assertIn("harness_v2/f2.txt", prompt)
        self.assertIn("version 2 tweak", prompt)
        for i in (0, 1, 3, 4):
            self.assertNotIn(f"harness_v2/f{i}.txt", prompt)

        st = json.loads(self.cli("status", "dir-copy", "--json", cwd=repo).stdout)
        self.assertEqual(st["review"]["scope"], "full")
        self.assertEqual(st["review"]["scopeFiles"], 1)

    def test_stopped_during_rework_preserves_t1_so_reply_reviews_rework_delta(self):
        script = (
            "if model == 'rev-model':\n"
            "    if not conv_in:\n"
            "        emit_ok('REWORK\\n请补充 b.txt\\n\\n## 主控重点看\\n- [中] b.txt:1 — 缺失\\n')\n"
            "    else:\n"
            "        emit_ok('PASS\\n返工与续做均已审过\\n\\n## 主控重点看\\n无\\n')\n"
            "else:\n"
            "    if '修验收' in content:\n"
            "        Path('ok.flag').write_text('yes\\n')\n"
            "        Path('c.txt').write_text('c added\\n')\n"
            "        emit_ok('已修复验收并新增 c.txt')\n"
            "    elif '请补充 b.txt' in content:\n"
            "        # Rework modifies b.txt (T2) and breaks acceptance so fast-accept fails (stoppedDuringRework)\n"
            "        Path('b.txt').write_text('b-from-failed-rework\\n')\n"
            "        Path('ok.flag').unlink()\n"
            "        emit_ok('返工改了 b.txt 但验收坏了')\n"
            "    else:\n"
            "        Path('a.txt').write_text('a-reviewed-in-t1\\n')\n"
            "        emit_ok('初稿改了 a.txt')\n"
        )
        log_file = self.install_scripted_agy(script)
        repo = self.repo(
            {
                "a.txt": "a0\n",
                "b.txt": "b0\n",
                "ok.flag": "yes\n",
                ".delegate.json": json.dumps(
                    {
                        "accept": "test -f ok.flag",
                        "acceptRules": [{"name": "fast-flag", "tier": "fast", "command": "test -f ok.flag", "paths": ["**"]}],
                        "review": True,
                        "maxRework": 5,
                    }
                ),
            }
        )
        r1 = self.cli("run", "--name", "rework-stop", "--workdir", str(repo), "--worktree", "初稿")
        self.assertNotEqual(r1.returncode, 0, "expected rejected when rework breaks fast-accept")
        st1 = json.loads(self.cli("status", "rework-stop", "--json", cwd=repo).stdout)
        self.assertEqual(st1["state"], "rejected")
        self.assertTrue(st1["review"].get("stoppedDuringRework"))
        # Next reply fixes acceptance + adds c.txt -> reviewer MUST see b.txt (from failed rework T1..T2) + c.txt, NOT a.txt!
        r2 = self.cli("reply", "rework-stop", "--wait", "修验收", cwd=repo)
        self.assertEqual(r2.returncode, 0, r2.stderr)
        rev_calls = [c for c in self.read_calls(log_file) if c["model"] == "rev-model"]
        self.assertEqual(len(rev_calls), 2, rev_calls)
        r2_prompt = rev_calls[1]["content"]
        self.assertIn("b-from-failed-rework", r2_prompt, "unreviewed rework delta T1..T2 must not be dropped")
        self.assertIn("c added", r2_prompt)
        self.assertNotIn("a-reviewed-in-t1", r2_prompt)

    def test_reviewer_error_does_not_mark_unreviewed_tree_as_reviewed(self):
        script = (
            "if model == 'rev-model':\n"
            "    if call_idx == 2:\n"
            "        print(json.dumps({'event':'init','conversation_id':conv_id}), flush=True)\n"
            "        print(json.dumps({'event':'result','result':{'conversation_id':conv_id,'status':'ERROR','error':'boom','response':''}}), flush=True)\n"
            "        sys.exit(1)\n"
            "    else:\n"
            "        emit_ok('PASS\\n全量审查通过\\n\\n## 主控重点看\\n无\\n')\n"
            "else:\n"
            "    if '改 b' in content:\n"
            "        Path('b.txt').write_text('b-second-run\\n')\n"
            "        emit_ok('改了 b.txt')\n"
            "    else:\n"
            "        Path('a.txt').write_text('a-unreviewed-first-run\\n')\n"
            "        emit_ok('改了 a.txt')\n"
        )
        log_file = self.install_scripted_agy(script)
        repo = self.repo(
            {
                "a.txt": "a0\n",
                "b.txt": "b0\n",
                ".delegate.json": json.dumps({"accept": "true", "review": True, "maxRework": 5}),
            }
        )
        self.cli("run", "--name", "rev-err", "--workdir", str(repo), "--worktree", "改 a")
        st1 = json.loads(self.cli("status", "rev-err", "--json", cwd=repo).stdout)
        self.assertIn(st1["state"], ("delivered", "rejected"))
        self.assertEqual(st1["review"]["verdict"], "NEEDS_CONTROLLER")
        self.assertEqual(st1["review"].get("upgradedBy"), "reviewerError")
        rev1_doc = json.loads((Path(st1["dir"]) / "review.json").read_text())
        self.assertNotIn("reviewedTree", rev1_doc, "reviewerError must not record unreviewed tree")

        r2 = self.cli("reply", "rev-err", "--wait", "改 b", cwd=repo)
        self.assertEqual(r2.returncode, 0, r2.stderr)
        rev_calls = [c for c in self.read_calls(log_file) if c["model"] == "rev-model"]
        self.assertEqual(len(rev_calls), 2, rev_calls)
        r2_prompt = rev_calls[1]["content"]
        self.assertIn("a-unreviewed-first-run", r2_prompt, "unreviewed changes from reviewerError run must be included")
        self.assertIn("b-second-run", r2_prompt)


if __name__ == "__main__":
    unittest.main()
