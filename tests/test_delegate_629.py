"""End-to-end tests for delegate 6.29.0 (审-改闭环):
1. Automatic read-only review after write acceptance passes (off by default, fresh context, task brief + diff --total + default/custom rubric)
2. Strict first-line controlled verdict (PASS / REWORK / NEEDS_CONTROLLER; invalid format fails closed to NEEDS_CONTROLLER + pauses DAG)
3. Verbatim REWORK reply to author without LLM paraphrasing + incremental diff re-review on reviewer continuation
4. Unlimited rework rounds with needs_attention event from round 3+ (no kill) + live status display (审查中 / REWORK 第N轮 / PASS / NEEDS_CONTROLLER)
5. controllerReviewPaths upgrades any verdict to NEEDS_CONTROLLER, attaches matched diff + SHA-256 hash; delegate sign & 1-char invalidation
6. Reviewer cannot write files (read-only tools + write detection fail-closed) and cannot dispatch subtasks (nesting_error)
7. Review events in events.jsonl, events --rebuild, Stop hook unread logic, and old run backward compatibility
"""

import json
import shlex
import subprocess
import sys
import unittest
from pathlib import Path

import test_delegate as fixtures

ROOT = Path(__file__).resolve().parent.parent
STOP_HOOK = ROOT / "skills/delegate/hooks/claude-code-stop.py"


class Delegate629Tests(unittest.TestCase):
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

    def install_scripted_pi(self, script_body: str):
        """Install a custom Python-driven fake `pi` that logs argv + stdin per call."""
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
            "fork = ''\n"
            "tools = ''\n"
            "for i, a in enumerate(args):\n"
            "    if a == '--session-dir' and i + 1 < len(args): sdir = args[i + 1]\n"
            "    elif a == '--session-id' and i + 1 < len(args): sid = args[i + 1]\n"
            "    elif a == '--fork' and i + 1 < len(args): fork = args[i + 1]\n"
            "    elif a == '--tools' and i + 1 < len(args): tools = args[i + 1]\n"
            "if sdir:\n"
            "    Path(sdir).mkdir(parents=True, exist_ok=True)\n"
            "    (Path(sdir) / f't_{sid or os.getpid()}.jsonl').write_text('{}\\n')\n"
            "log_path = Path(os.environ['SCRIPTED_PI_LOG'])\n"
            "prev = log_path.read_text().splitlines() if log_path.is_file() else []\n"
            "call_idx = len(prev) + 1\n"
            "rec = {'call': call_idx, 'args': args, 'stdin': stdin_text, 'sdir': sdir, 'sid': sid, 'fork': fork, 'tools': tools, 'cwd': os.getcwd(), 'active': os.environ.get('PI_DELEGATE_ACTIVE', ''), 'agent': os.environ.get('DELEGATE_AGENT', '')}\n"
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

    def test_auto_review_disabled_by_default_and_enabled_runs_fresh_read_only_reviewer(self):
        # 1) Default (no `review` in .delegate.json): no review runs.
        repo_off = self.make_repo("repo-off", {"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo v1 > a.txt")
        st_off = self.outcome(
            self.cli("run", "--workdir", repo_off, "--worktree", "--name", "no-rev", "--accept", "true", "edit a")
        )
        self.assertEqual(st_off["state"], "delivered")
        self.assertNotIn("review", st_off)
        self.assertFalse((Path(st_off["dir"]) / "review.json").exists())

        # 2) Enabled with default rubric: fresh read-only reviewer sees brief + diff --total + default rubric.
        repo = self.make_repo("repo-default-rubric", {
            "a.txt": "v0\n",
            ".delegate.json": json.dumps({"review": {"tier": "review"}}),
        })
        log_file = self.install_scripted_pi(
            "if call_idx == 1:\n"
            "    Path('a.txt').write_text('v0\\nv1-feature\\n')\n"
            "    emit_answer('implemented feature')\n"
            "else:\n"
            "    emit_answer('PASS\\n改动正确，无回归。')\n"
        )
        res = self.cli("run", "--workdir", repo, "--worktree", "--name", "with-rev", "--accept", "true", "Implement feature X in a.txt")
        self.assertEqual(res.returncode, 0, res.stderr)
        st = self.outcome(res)
        self.assertEqual(st["state"], "delivered")
        self.assertEqual(st["review"]["verdict"], "PASS")
        self.assertEqual(st["review"]["round"], 1)
        self.assertEqual(st["review"]["reworkRounds"], 0)
        self.assertTrue((Path(st["dir"]) / "review.json").is_file())
        summary = json.loads((Path(st["dir"]) / "summary.json").read_text())
        self.assertEqual(summary["review"]["verdict"], "PASS")

        calls = [json.loads(ln) for ln in log_file.read_text().splitlines()]
        self.assertEqual(len(calls), 2)
        author_call, review_call = calls
        # Reviewer has fresh context (no --fork, different --session-dir) and read-only tools!
        self.assertEqual(review_call["fork"], "")
        self.assertNotEqual(review_call["sdir"], author_call["sdir"])
        self.assertEqual(review_call["tools"], "read,grep,find,ls")
        self.assertEqual(review_call["active"], "1")
        # Reviewer input contains original brief, net diff, and default rubric sections.
        self.assertIn("Implement feature X in a.txt", review_call["stdin"])
        self.assertIn("+v1-feature", review_call["stdin"])
        self.assertIn("默认代码审查要点", review_call["stdin"])

        # Human status shows PASS.
        hum = self.cli("status", st["run"], human=True)
        self.assertIn("审查 PASS", hum.stdout)

        # Events stream includes review_started + review_verdict.
        evs = [e for e in self.read_events() if e["run"] == st["run"]]
        kinds = [e["kind"] for e in evs]
        self.assertIn("review_started", kinds)
        self.assertIn("review_verdict", kinds)

        # 3) Custom repo rubric override is loaded.
        repo_custom = self.make_repo("repo-custom-rubric", {
            "a.txt": "v0\n",
            "docs/my-rubric.md": "CUSTOM_RUBRIC_MARKER_999\nCheck domain invariants.\n",
            ".delegate.json": json.dumps({"review": {"tier": "review", "rubric": "docs/my-rubric.md"}}),
        })
        (self.work / "scripted-pi-calls.jsonl").unlink(missing_ok=True)
        log_file2 = self.install_scripted_pi(
            "if call_idx == 1:\n"
            "    Path('a.txt').write_text('v2\\n')\n"
            "    emit_answer('done')\n"
            "else:\n"
            "    emit_answer('PASS\\nok')\n"
        )
        st2 = self.outcome(
            self.cli("run", "--workdir", repo_custom, "--worktree", "--name", "custom-rub", "--accept", "true", "task")
        )
        self.assertEqual(st2["review"]["verdict"], "PASS")
        calls2 = [json.loads(ln) for ln in log_file2.read_text().splitlines()]
        self.assertIn("CUSTOM_RUBRIC_MARKER_999", calls2[1]["stdin"])

    def test_invalid_verdict_format_fails_closed_to_needs_controller_and_pauses_dag(self):
        repo = self.repo({
            "a.txt": "v0\n",
            ".delegate.json": json.dumps({"review": True}),
        })
        gate = self.work / "gate-bad-fmt"
        self.install_scripted_pi(
            f"import time\n"
            f"if call_idx == 1:\n"
            f"    while not Path({str(gate)!r}).exists(): time.sleep(0.05)\n"
            f"    Path('a.txt').write_text('v1\\n')\n"
            f"    emit_answer('done')\n"
            f"else:\n"
            f"    emit_answer('等日志跑完再说。\\n暂时还没下结论。')\n"
        )
        run_a = self.outcome(
            self.cli("start", "--workdir", repo, "--worktree", "--name", "bad-fmt", "--accept", "true", "task")
        )
        self.addCleanup(lambda: gate.touch())

        # Queue a downstream DAG step after run_a.
        next_prompt = self.work / "next-step.md"
        next_prompt.write_text("downstream task\n")
        add = self.cli("queue", "add", "down-1", "--after", run_a["run"], "--prompt-file", str(next_prompt), "--read-only", "--json", cwd=repo)
        self.assertEqual(add.returncode, 0, add.stderr)

        gate.touch()
        self.until(
            lambda: (Path(run_a["dir"]) / "exit_code").exists()
            and fixtures.process_gone(Path(run_a["dir"]) / "pid"),
            "bad-fmt finished",
        )
        st = self.outcome(self.cli("status", run_a["run"]))
        self.assertEqual(st["state"], "delivered")
        self.assertEqual(st["review"]["verdict"], "NEEDS_CONTROLLER")
        self.assertTrue(st["review"]["formatError"])
        self.assertEqual(st["review"]["upgradedBy"], "formatError")

        # DAG downstream step MUST be paused (not dispatched)!
        q_list = self.cli("queue", "list", "--json", cwd=repo)
        q_items = [json.loads(ln) for ln in q_list.stdout.splitlines() if ln.strip()]
        down = next(i for i in q_items if i["name"] == "down-1")
        self.assertEqual(down["status"], "paused")
        self.assertIn("NEEDS_CONTROLLER", down["pauseReason"])

        # Both review_verdict(NEEDS_CONTROLLER) and needs_attention events emitted.
        evs = [e for e in self.read_events() if e["run"] == run_a["run"]]
        self.assertTrue(any(e["kind"] == "review_verdict" and e["payload"]["verdict"] == "NEEDS_CONTROLLER" for e in evs))
        self.assertTrue(any(e["kind"] == "needs_attention" and e["payload"]["reason"] == "formatError" for e in evs))

    def test_rework_sends_verdict_verbatim_and_rereviews_only_incremental_diff(self):
        repo = self.repo({
            "a.txt": "orig-a\n",
            "b.txt": "orig-b\n",
            ".delegate.json": json.dumps({"review": {"tier": "review"}}),
        })
        raw_rework_msg = "REWORK\n1. a.txt 漏改了 b.txt\n2. 保留原样标记: VERBATIM_PAYLOAD_777"
        gate = self.work / "gate-rw"
        log_file = self.install_scripted_pi(
            f"import time\n"
            f"if call_idx == 1:\n"
            f"    while not Path({str(gate)!r}).exists(): time.sleep(0.05)\n"
            f"    Path('a.txt').write_text('round1-a\\n')\n"
            f"    emit_answer('round 1 author answer')\n"
            f"elif call_idx == 2:\n"
            f"    emit_answer({raw_rework_msg!r})\n"
            f"elif call_idx == 3:\n"
            f"    Path('b.txt').write_text('round2-b\\n')\n"
            f"    emit_answer('round 2 author fixed b.txt')\n"
            f"else:\n"
            f"    emit_answer('PASS\\nb.txt 已修复，增量 diff 干净。')\n"
        )

        run_a = self.outcome(
            self.cli("start", "--workdir", repo, "--worktree", "--name", "rw-loop", "--accept", "true", "update a and b")
        )
        self.addCleanup(lambda: gate.touch())

        # Queue a downstream DAG step: should stay waiting during REWORK and auto-dispatch after round 2 PASS!
        next_prompt = self.work / "dag-next.md"
        next_prompt.write_text("step after pass\n")
        self.assertEqual(
            self.cli("queue", "add", "after-pass", "--after", run_a["run"], "--prompt-file", str(next_prompt), "--read-only", "--json", cwd=repo).returncode,
            0,
        )

        gate.touch()
        self.until(lambda: (Path(run_a["dir"]) / "exit_code").exists(), "rw-loop finished")

        st = self.outcome(self.cli("status", run_a["run"]))
        self.assertEqual(st["state"], "delivered")
        self.assertEqual(st["review"]["verdict"], "PASS")
        self.assertEqual(st["review"]["round"], 2)
        self.assertEqual(st["review"]["reworkRounds"], 1)

        calls = [json.loads(ln) for ln in log_file.read_text().splitlines()]
        # calls[0]=author r1, calls[1]=reviewer r1, calls[2]=author r2 (rework), calls[3]=reviewer r2 (incremental)
        self.assertGreaterEqual(len(calls), 4)
        author_r1, rev_r1, author_r2, rev_r2 = calls[0], calls[1], calls[2], calls[3]

        # Author r2 received the EXACT raw verdict file without any paraphrasing!
        verdict_file = Path(st["dir"]) / "review" / "round-1-verdict.md"
        self.assertEqual(verdict_file.read_text(), raw_rework_msg)
        self.assertEqual(author_r2["stdin"], raw_rework_msg)
        self.assertNotEqual(author_r2["fork"], "")

        # Reviewer r2 continued ITS OWN session (not author's) and saw ONLY incremental diff (b.txt, NOT a.txt)!
        self.assertNotEqual(rev_r2["fork"], "")
        self.assertIn("round-2/fork", rev_r2["fork"])
        self.assertIn("round2-b", rev_r2["stdin"])
        self.assertNotIn("round1-a", rev_r2["stdin"])

        # Events include review_started x2 (2nd incremental=True), review_verdict (REWORK + PASS), rework_sent, and NO needs_attention (<3 rounds).
        evs = [e for e in self.read_events() if e["run"] == run_a["run"]]
        starts = [e for e in evs if e["kind"] == "review_started"]
        self.assertEqual(len(starts), 2)
        self.assertFalse(starts[0]["payload"]["incremental"])
        self.assertTrue(starts[1]["payload"]["incremental"])
        self.assertEqual(len([e for e in evs if e["kind"] == "rework_sent"]), 1)
        self.assertEqual(len([e for e in evs if e["kind"] == "needs_attention"]), 0)

        # Downstream DAG step auto-dispatched after final PASS!
        def dag_dispatched():
            q_list = self.cli("queue", "list", "--json", cwd=repo)
            items = [json.loads(ln) for ln in q_list.stdout.splitlines() if ln.strip()]
            return any(i["name"] == "after-pass" and i["status"] == "dispatched" for i in items)

        self.until(dag_dispatched, "DAG step dispatched after review PASS")
        q_list = self.cli("queue", "list", "--json", cwd=repo)
        for ln in q_list.stdout.splitlines():
            if ln.strip():
                it = json.loads(ln)
                if it.get("name") == "after-pass" and it.get("runId"):
                    self.cli("wait", "--no-result", it["runId"])

    def test_unlimited_rework_rounds_3_plus_emits_needs_attention_and_live_status_shows_states(self):
        repo = self.repo({
            "counter.txt": "0\n",
            ".delegate.json": json.dumps({"review": True}),
        })
        rev_gate = self.work / "gate-reviewing"
        rw_gate = self.work / "gate-reworking"
        self.install_scripted_pi(
            f"import time\n"
            f"# Calls: 1=A1, 2=R1(REWORK), 3=A2, 4=R2(REWORK), 5=A3, 6=R3(REWORK->needs_attention), 7=A4, 8=R4(PASS)\n"
            f"if call_idx == 2:\n"
            f"    while not Path({str(rev_gate)!r}).exists(): time.sleep(0.05)\n"
            f"    emit_answer('REWORK\\nround 1 issue')\n"
            f"elif call_idx == 3:\n"
            f"    while not Path({str(rw_gate)!r}).exists(): time.sleep(0.05)\n"
            f"    Path('counter.txt').write_text('2\\n')\n"
            f"    emit_answer('author round 2')\n"
            f"elif call_idx in (1, 5, 7):\n"
            f"    Path('counter.txt').write_text(f'{{call_idx}}\\n')\n"
            f"    emit_answer(f'author call {{call_idx}}')\n"
            f"elif call_idx in (4, 6):\n"
            f"    emit_answer(f'REWORK\\nissue at call {{call_idx}}')\n"
            f"else:\n"
            f"    emit_answer('PASS\\nfinal pass on round 4')\n"
        )
        run_a = self.outcome(
            self.cli("start", "--workdir", repo, "--worktree", "--name", "four-rounds", "--accept", "true", "iterate")
        )
        self.addCleanup(lambda: (rev_gate.touch(), rw_gate.touch()))

        # Live status during review round 1 shows "审查中".
        def is_reviewing():
            st = self.outcome(self.cli("status", run_a["run"]))
            return st.get("review", {}).get("status") == "reviewing"

        self.until(is_reviewing, "status shows reviewing")
        hum_rev = self.cli("status", run_a["run"], human=True)
        self.assertIn("审查中", hum_rev.stdout)
        rev_gate.touch()

        # Live status during rework round 1 shows "REWORK 第 1 轮".
        def is_reworking():
            st = self.outcome(self.cli("status", run_a["run"]))
            return st.get("review", {}).get("status") == "reworking"

        self.until(is_reworking, "status shows reworking")
        hum_rw = self.cli("status", run_a["run"], human=True)
        self.assertIn("REWORK 第 1 轮", hum_rw.stdout)
        rw_gate.touch()

        # Wait for all 4 rounds to finish (not killed at round 3!).
        self.until(lambda: (Path(run_a["dir"]) / "exit_code").exists(), "four-rounds finished")
        st = self.outcome(self.cli("status", run_a["run"]))
        self.assertEqual(st["state"], "delivered")
        self.assertEqual(st["review"]["verdict"], "PASS")
        self.assertEqual(st["review"]["round"], 4)
        self.assertEqual(st["review"]["reworkRounds"], 3)
        self.assertTrue(st["review"]["needsAttention"])

        # needs_attention emitted specifically at round 3 (not rounds 1 or 2).
        att = [e for e in self.read_events() if e["run"] == run_a["run"] and e["kind"] == "needs_attention"]
        self.assertEqual(len(att), 1, att)
        self.assertEqual(att[0]["payload"]["reworkRounds"], 3)
        self.assertEqual(att[0]["payload"]["reason"], "rework_rounds")

    def test_controller_review_paths_upgrades_verdict_attaches_diff_hash_and_sign_invalidation(self):
        repo = self.repo({
            "src/code.rs": "fn f() -> i32 { 1 }\n",
            "prompts/system.md": "Hello model v1\n",
            ".delegate.json": json.dumps({
                "review": True,
                "controllerReviewPaths": ["prompts/**", "skills/*/SKILL.md"],
            }),
        })
        self.install_scripted_pi(
            "if call_idx == 1:\n"
            "    Path('src/code.rs').write_text('fn f() -> i32 { 2 }\\n')\n"
            "    Path('prompts/system.md').write_text('Hello model v2\\n')\n"
            "    emit_answer('updated code and prompt')\n"
            "else:\n"
            "    emit_answer('PASS\\ncode looks fine')\n"
        )
        run_res = self.cli(
            "run", "--workdir", repo, "--worktree", "--name", "ctrl-path", "--accept", "true", "update prompt",
            human=True,
        )
        self.assertEqual(run_res.returncode, 0, run_res.stderr)
        # Delivery output attaches matched path, diff full text, and SHA-256 sign hash!
        self.assertIn("controller-review (NEEDS_CONTROLLER)", run_res.stdout)
        self.assertIn("prompts/system.md", run_res.stdout)
        self.assertIn("签字哈希：sha256:", run_res.stdout)
        self.assertIn("+Hello model v2", run_res.stdout)
        self.assertIn("delegate sign", run_res.stdout)

        st = self.outcome(self.cli("status", "ctrl-path"))
        run_id = st["run"]
        self.assertEqual(st["review"]["verdict"], "NEEDS_CONTROLLER")
        self.assertEqual(st["review"]["originalVerdict"], "PASS")
        self.assertEqual(st["review"]["upgradedBy"], "controllerReviewPaths")
        cr = st["review"]["controllerReview"]
        self.assertEqual(cr["paths"], ["prompts/system.md"])
        self.assertTrue(cr["hash"].startswith("sha256:"))
        self.assertNotIn("diff", cr)
        self.assertTrue(Path(cr["diffFile"]).is_file())
        self.assertGreater(cr["diffBytes"], 0)
        diff_text = Path(cr["diffFile"]).read_text()
        self.assertIn("+Hello model v2", diff_text)
        self.assertNotIn("src/code.rs", diff_text)
        self.assertTrue(st["sign"]["required"])
        self.assertFalse(st["sign"]["signed"])
        self.assertFalse(st["sign"]["valid"])

        # Before signing: `delegate sign --check` exits 1.
        chk0 = self.cli("sign", "--check", "--json", run_id, cwd=repo)
        self.assertEqual(chk0.returncode, 1)
        self.assertFalse(json.loads(chk0.stdout.strip())["valid"])

        # Controller signs: `delegate sign <run>` exits 0 and writes sign.json.
        signed = self.cli("sign", "--json", run_id, cwd=repo)
        self.assertEqual(signed.returncode, 0, signed.stderr)
        sign_obj = json.loads(signed.stdout.strip())
        self.assertEqual(sign_obj["hash"], cr["hash"])
        self.assertTrue(sign_obj["valid"])

        # After signing: `delegate sign --check` exits 0 and status shows 已签字.
        chk1 = self.cli("sign", "--check", "--json", run_id, cwd=repo)
        self.assertEqual(chk1.returncode, 0, chk1.stderr)
        self.assertTrue(json.loads(chk1.stdout.strip())["valid"])
        self.assertIn("已签字", self.cli("status", run_id, human=True).stdout)

        # Change ONE character in prompts/system.md -> signature immediately invalidates!
        wt_prompt = Path(st["worktree"]) / "prompts/system.md"
        wt_prompt.write_text("Hello model v3\n")
        chk2 = self.cli("sign", "--check", "--json", run_id, cwd=repo)
        self.assertEqual(chk2.returncode, 1)
        chk2_obj = json.loads(chk2.stdout.strip())
        self.assertFalse(chk2_obj["valid"])
        self.assertTrue(chk2_obj["invalidated"])
        self.assertNotEqual(chk2_obj["hash"], chk2_obj["signedHash"])
        self.assertIn("签字已失效", self.cli("status", run_id, human=True).stdout)

        # Apply semantics remain completely unchanged (apply --dry-run still succeeds).
        dry = self.cli("apply", "--dry-run", "--json", run_id, cwd=repo)
        self.assertEqual(dry.returncode, 0, dry.stderr)

        # Also verify controllerReviewPaths works even when auto-review is OFF!
        repo2 = self.make_repo("repo-ctrl-no-rev", {
            "prompts/tool.md": "v1\n",
            ".delegate.json": json.dumps({"controllerReviewPaths": ["prompts/**"]}),
        })
        self.fake_pi([fixtures.answer("edited prompt"), fixtures.SETTLED], pre="echo v2 > prompts/tool.md")
        st2 = self.outcome(
            self.cli("run", "--workdir", repo2, "--worktree", "--name", "ctrl-no-rev", "--accept", "true", "edit tool")
        )
        self.assertEqual(st2["review"]["verdict"], "NEEDS_CONTROLLER")
        self.assertEqual(st2["review"]["upgradedBy"], "controllerReviewPaths")
        self.assertEqual(st2["review"]["controllerReview"]["paths"], ["prompts/tool.md"])

    def test_reviewer_cannot_write_files_or_dispatch_subtasks(self):
        repo = self.repo({
            "a.txt": "v0\n",
            ".delegate.json": json.dumps({"review": True}),
        })
        delegate_bin = str(fixtures.DELEGATE)
        probe_out = self.work / "reviewer-probe.json"
        self.install_scripted_pi(
            f"import subprocess\n"
            f"if call_idx == 1:\n"
            f"    Path('a.txt').write_text('v1\\n')\n"
            f"    emit_answer('done')\n"
            f"else:\n"
            f"    # Attempt to dispatch via delegate start and delegate queue add (must be refused!)\n"
            f"    r1 = subprocess.run([{delegate_bin!r}, 'start', '--read-only', 'nested'], capture_output=True, text=True)\n"
            f"    pfile = Path({str(self.work / 'q.md')!r})\n"
            f"    pfile.write_text('x\\n')\n"
            f"    r2 = subprocess.run([{delegate_bin!r}, 'queue', 'add', 'nested-q', '--after', 'last', '--prompt-file', str(pfile)], capture_output=True, text=True)\n"
            f"    Path({str(probe_out)!r}).write_text(json.dumps({{'start_rc': r1.returncode, 'start_err': r1.stderr, 'q_rc': r2.returncode, 'q_err': r2.stderr}}))\n"
            f"    # Simulate rogue write inside workdir -> must be caught and fail closed!\n"
            f"    Path('rogue.txt').write_text('illegal write\\n')\n"
            f"    emit_answer('PASS\\ntried to cheat')\n"
        )
        st = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "ro-guard", "--accept", "true", "edit a")
        )
        probe = json.loads(probe_out.read_text())
        self.assertNotEqual(probe["start_rc"], 0)
        self.assertIn("refusing nested delegation", probe["start_err"])
        self.assertNotEqual(probe["q_rc"], 0)
        self.assertIn("refusing nested delegation", probe["q_err"])

        # Rogue write caught -> fail-closed to NEEDS_CONTROLLER with roViolation index pointer!
        self.assertEqual(st["review"]["verdict"], "NEEDS_CONTROLLER")
        self.assertEqual(st["review"]["upgradedBy"], "readOnlyViolation")
        self.assertGreaterEqual(st["review"]["roViolationCount"], 1)
        self.assertIn("rogue.txt", st["review"]["roViolationSample"])
        ro_file = Path(st["review"]["roViolationFile"])
        self.assertTrue(ro_file.is_file())
        self.assertIn("rogue.txt", ro_file.read_text())

    def test_stop_hook_events_rebuild_and_old_run_compatibility(self):
        repo = self.repo({"a.txt": "v0\n"})
        no_runs_env = {k: v for k, v in self.env.items() if k != "DELEGATE_RUNS"}

        def hook():
            env = {
                **no_runs_env,
                "DELEGATE_BIN": str(fixtures.DELEGATE),
                "DELEGATE_STOP_HOOK_WAITERS": "",
            }
            res = subprocess.run(
                [sys.executable, str(STOP_HOOK)],
                input=json.dumps({"cwd": str(repo), "stop_hook_active": False}),
                capture_output=True,
                text=True,
                env=env,
                timeout=15,
            )
            self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
            out = res.stdout.strip()
            return json.loads(out) if out else None

        # Write synthetic events directly to test exact Stop hook filtering on review event kinds:
        sdir = self.state_dir()
        sdir.mkdir(parents=True, exist_ok=True)
        ev_file = sdir / "events.jsonl"
        # 1) Non-alert review events (review_started, review_verdict REWORK, rework_sent) -> MUST NOT block!
        ev_file.write_text(
            "\n".join([
                json.dumps({"seq": 1, "at": "2026-11-23T10:00:00Z", "run": "r1", "repo": str(repo), "kind": "review_started", "payload": {"name": "t1", "round": 1}}),
                json.dumps({"seq": 2, "at": "2026-11-23T10:00:10Z", "run": "r1", "repo": str(repo), "kind": "review_verdict", "payload": {"name": "t1", "round": 1, "verdict": "REWORK"}}),
                json.dumps({"seq": 3, "at": "2026-11-23T10:00:11Z", "run": "r1", "repo": str(repo), "kind": "rework_sent", "payload": {"name": "t1", "round": 1, "reworkRounds": 1}}),
            ]) + "\n"
        )
        self.assertIsNone(hook())

        # 2) needs_attention event -> MUST block!
        with ev_file.open("a") as fh:
            fh.write(json.dumps({"seq": 4, "at": "2026-11-23T10:01:00Z", "run": "r1", "repo": str(repo), "kind": "needs_attention", "payload": {"name": "t1", "round": 3, "reason": "rework_rounds"}}) + "\n")
        blocked = hook()
        self.assertIsNotNone(blocked)
        self.assertEqual(blocked["decision"], "block")
        self.assertIn("需主控关注:t1", blocked["reason"])

        # 3) Advance cursor past seq 4 via wait --since 0, then emit review_verdict(NEEDS_CONTROLLER) -> MUST block!
        w = subprocess.run([str(fixtures.DELEGATE), "wait", "--since", "0", "--json"], cwd=repo, capture_output=True, text=True, env=no_runs_env, timeout=15)
        self.assertEqual(w.returncode, 0, w.stderr)
        self.assertIsNone(hook())
        with ev_file.open("a") as fh:
            fh.write(json.dumps({"seq": 5, "at": "2026-11-23T10:02:00Z", "run": "r2", "repo": str(repo), "kind": "review_verdict", "payload": {"name": "t2", "round": 1, "verdict": "NEEDS_CONTROLLER"}}) + "\n")
        blocked2 = hook()
        self.assertIsNotNone(blocked2)
        self.assertIn("审查判定:t2", blocked2["reason"])

        # 4) Old pre-6.29 run compatibility (no review.json, no sign.json) + events --rebuild with both old & new runs!
        runs_dir = Path(self.env["DELEGATE_RUNS"])
        old_run = runs_dir / "20261101-010000-legacy"
        old_run.mkdir(parents=True, exist_ok=True)
        (old_run / "meta.json").write_text(json.dumps({
            "run": "20261101-010000-legacy",
            "name": "legacy-task",
            "mode": "read-only",
            "workdir": str(repo),
            "startedAt": "2026-11-01T01:00:00Z",
            "startedEpoch": 1793494800,
        }))
        (old_run / "summary.json").write_text(json.dumps({
            "state": "answered",
            "finishedAt": "2026-11-01T01:01:00Z",
        }))
        (old_run / "result.md").write_text("legacy answer\n")
        (old_run / "exit_code").write_text("0\n")

        old_st = self.outcome(self.cli("status", "20261101-010000-legacy"))
        self.assertEqual(old_st["state"], "answered")
        self.assertNotIn("review", old_st)
        self.assertNotIn("sign", old_st)
        self.assertEqual(self.cli("sign", "--check", "20261101-010000-legacy").returncode, 0)

        # Also create a 6.29 run with review.json and rebuild events.jsonl.
        rev_run = runs_dir / "20261123-020000-rebuilt-rev"
        rev_run.mkdir(parents=True, exist_ok=True)
        (rev_run / "meta.json").write_text(json.dumps({
            "run": "20261123-020000-rebuilt-rev",
            "name": "rebuilt-rev",
            "mode": "write",
            "workdir": str(repo),
            "startedAt": "2026-11-23T02:00:00Z",
            "startedEpoch": 1795399200,
        }))
        (rev_run / "summary.json").write_text(json.dumps({
            "state": "delivered",
            "finishedAt": "2026-11-23T02:05:00Z",
            "accept": {"ok": True, "exitCode": 0, "command": "true"},
        }))
        (rev_run / "review.json").write_text(json.dumps({
            "status": "done",
            "verdict": "PASS",
            "round": 2,
            "reworkRounds": 1,
            "rounds": [
                {"round": 1, "verdict": "REWORK", "verdictFile": "/tmp/v1.md", "incremental": False, "startedAt": "2026-11-23T02:01:00Z", "finishedAt": "2026-11-23T02:02:00Z"},
                {"round": 2, "verdict": "PASS", "verdictFile": "/tmp/v2.md", "incremental": True, "startedAt": "2026-11-23T02:03:00Z", "finishedAt": "2026-11-23T02:04:00Z"},
            ],
        }))
        (rev_run / "exit_code").write_text("0\n")

        rebuilt = self.cli("events", "--rebuild", "--json")
        self.assertEqual(rebuilt.returncode, 0, rebuilt.stderr)
        rebuilt_evs = self.read_events()
        rev_kinds = [e["kind"] for e in rebuilt_evs if e["run"] == "20261123-020000-rebuilt-rev"]
        self.assertIn("review_started", rev_kinds)
        self.assertIn("review_verdict", rev_kinds)
        self.assertIn("rework_sent", rev_kinds)
        legacy_evs = [e for e in rebuilt_evs if e["run"] == "20261101-010000-legacy"]
        self.assertTrue(any(e["kind"] == "delivered" for e in legacy_evs))


if __name__ == "__main__":
    unittest.main()
