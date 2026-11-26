"""End-to-end regressions for the three 11-24 norvane delegate fixes:
1. `apply` dirty-overlap guard (refuse by default, ignore own identical write, `--allow-dirty-overlap`)
   + last-line `delegate files <run>` pointer & `delegate files <run>` subcommand (surviving archive).
2. Zero `forbidden slow operation (git/subprocess)` while holding `state/.state.lock` during `apply`.
3. Gemini/agy content-policy refusal detection, single same-session tail-append rewrite (cache-first),
   and refused fallback (`result.md` pointing to `review.md` + `delegate files <run>`, status label
   「结论被模型服务拒绝」, not treated as a normal conclusion).
"""
import json
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


POLICY_REFUSAL_TEXT = (
    "The prompt could not be submitted. The prompt contains sensitive words that "
    "violate Google's Generative AI Prohibited Use policy..."
)


class Delegate1124FixesTests(unittest.TestCase):
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def setUp(self):
        fixtures.DelegateTests.setUp(self)
        self.env.pop("DELEGATE_AGY_BIN", None)

    def _git(self, repo: Path, *args: str) -> str:
        res = subprocess.run(
            ["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t", *args],
            check=True,
            capture_output=True,
            text=True,
        )
        return res.stdout.strip()

    def _write_user_config(self, doc: dict) -> None:
        self.fake_home = self.work / "home"
        self.fake_home.mkdir(mode=0o700, exist_ok=True)
        self.env["HOME"] = str(self.fake_home)
        self.env["XDG_CONFIG_HOME"] = str(self.fake_home / ".config")
        cfg_dir = Path(self.env["XDG_CONFIG_HOME"]) / "delegate"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "config.json").write_text(json.dumps(doc), encoding="utf-8")
        for k in ("DELEGATE_SCOUT_MODEL", "DELEGATE_REVIEW_MODEL", "DELEGATE_WRITE_MODEL"):
            self.env.pop(k, None)

    def _install_scripted_agy(self, script_body: str) -> tuple[Path, Path]:
        bin_dir = self.work / "agy-bin"
        bin_dir.mkdir(exist_ok=True)
        agy = bin_dir / "agy"
        counter_file = self.work / "agy-calls.txt"
        log_file = self.work / "agy-invocations.jsonl"
        for f in (counter_file, log_file):
            if f.exists():
                f.unlink()
        agy.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "argv = sys.argv[1:]\n"
            "if '--version' in argv:\n"
            "    print('1.6.0')\n"
            "    sys.exit(0)\n"
            f"counter = Path({str(counter_file)!r})\n"
            f"inv_log = Path({str(log_file)!r})\n"
            "call_idx = int(counter.read_text()) + 1 if counter.exists() else 1\n"
            "counter.write_text(str(call_idx))\n"
            "raw_in = sys.stdin.readline()\n"
            "msg = json.loads(raw_in.strip().splitlines()[-1]) if raw_in.strip() else {}\n"
            "content = msg.get('message', {}).get('content', '')\n"
            "conv_id = ''\n"
            "model_id = ''\n"
            "for i, a in enumerate(argv):\n"
            "    if a == '--conversation' and i + 1 < len(argv):\n"
            "        conv_id = argv[i + 1]\n"
            "    if a == '--model' and i + 1 < len(argv):\n"
            "        model_id = argv[i + 1]\n"
            "with inv_log.open('a', encoding='utf-8') as f:\n"
            "    f.write(json.dumps({'call': call_idx, 'argv': argv, 'conv': conv_id, 'model': model_id, 'content': content}) + '\\n')\n"
            "def emit_agy(text, cid='conv-1'):\n"
            "    c = conv_id or cid\n"
            "    print(json.dumps({'event':'init','conversation_id':c}), flush=True)\n"
            "    print(json.dumps({'event':'step_update','step_update':{'step_index':1,'step_type':'agent_response','state':'DONE','text_delta':text}}), flush=True)\n"
            "    print(json.dumps({'event':'result','result':{'conversation_id':c,'status':'SUCCESS','response':text,'usage':{'input_tokens':100,'output_tokens':20,'cache_read_tokens':1000}}}), flush=True)\n"
            + script_body
            + "\n",
            encoding="utf-8",
        )
        agy.chmod(0o755)
        self.env["PATH"] = f"{bin_dir}:{self.env.get('PATH', '')}"
        self._write_user_config(
            {
                "models": {"neon": "agy/gemini-4-neon"},
                "tiers": {"scout": "neon", "review": "neon", "write": "neon"},
            }
        )
        return counter_file, log_file

    # ------------------------------------------------------------------
    # Fix 1: apply dirty-overlap guard + --allow-dirty-overlap + delegate files
    # ------------------------------------------------------------------
    def test_apply_refuses_dirty_overlap_ignores_self_write_and_prints_files_command(self):
        repo = self.repo(
            {
                "shared.rs": "line1\nline2\nline3\nline4\nline5\n",
                "other.rs": "orig\n",
            }
        )
        # Parallel runs A, B (both touch shared.rs) and C (disjoint).
        self.fake_pi(
            [fixtures.answer("done A"), fixtures.SETTLED],
            pre="sed -i 's/line1/from_A/' shared.rs; echo a_only > a_only.rs",
        )
        run_a = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "run-a", "--accept", "true", "task A")
        )
        self.fake_pi(
            [fixtures.answer("done B"), fixtures.SETTLED],
            pre="sed -i 's/line5/from_B/' shared.rs; echo b_only > b_only.rs",
        )
        run_b = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "run-b", "--accept", "true", "task B")
        )
        self.fake_pi(
            [fixtures.answer("done C"), fixtures.SETTLED],
            pre="echo c_only > c_only.rs",
        )
        run_c = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "run-c", "--accept", "true", "task C")
        )

        # 1. Apply A cleanly (leaves uncommitted changes in source, auto-cleans & archives run A).
        ap_a = self.cli("apply", "--keep", run_a["run"], human=True)
        self.assertEqual(ap_a.returncode, 0, ap_a.stdout + ap_a.stderr)
        ap_a_lines = ap_a.stdout.splitlines()
        self.assertEqual(ap_a_lines[0], f"APPLY OK {run_a['run']}")
        self.assertEqual(
            ap_a_lines[-2],
            f"本 run 改动文件（2 个）：delegate files {run_a['run']}",
        )
        self.assertEqual(ap_a_lines[-1], f"APPLY OK {run_a['run']}")
        # `delegate files <runA>` returns exact repo-relative paths (one per line).
        files_a = self.cli("files", run_a["run"], cwd=repo)
        self.assertEqual(files_a.returncode, 0, files_a.stderr)
        self.assertEqual(files_a.stdout.splitlines(), ["a_only.rs", "shared.rs"])

        # 2. Re-applying A onto its OWN identical uncommitted content is ignored and succeeds!
        re_a = self.cli("apply", "--keep", run_a["run"], human=True)
        self.assertEqual(re_a.returncode, 0, re_a.stdout + re_a.stderr)

        # 3. Disjoint run C applies normally even though shared.rs & a_only.rs are dirty!
        ap_c = self.cli("apply", run_c["run"], human=True)
        self.assertEqual(ap_c.returncode, 0, ap_c.stdout + ap_c.stderr)
        ap_c_lines = ap_c.stdout.splitlines()
        self.assertEqual(ap_c_lines[0], f"APPLY OK {run_c['run']}")
        self.assertEqual(
            ap_c_lines[-2],
            f"本 run 改动文件（1 个）：delegate files {run_c['run']}",
        )
        self.assertEqual(ap_c_lines[-1], f"APPLY OK {run_c['run']}")
        # Even AFTER auto-clean archived run C, `delegate files <runC>` still works!
        self.assertFalse(Path(run_c["dir"]).exists())
        files_c = self.cli("files", run_c["run"], cwd=repo)
        self.assertEqual(files_c.returncode, 0, files_c.stderr)
        self.assertEqual(files_c.stdout.splitlines(), ["c_only.rs"])

        # 4. Default apply B MUST REFUSE because shared.rs has uncommitted changes from run A!
        refused_b = self.cli("apply", run_b["run"], human=True)
        self.assertEqual(refused_b.returncode, 1, refused_b.stdout + refused_b.stderr)
        payload_b = self.outcome(refused_b)
        self.assertFalse(payload_b["apply"]["ok"])
        self.assertEqual(payload_b["apply"]["failedStep"], "dirty-overlap")
        self.assertEqual(payload_b["apply"]["dirtyOverlap"], ["shared.rs"])
        self.assertEqual(payload_b["apply"]["writtenFiles"], [])
        self.assertIn("shared.rs", refused_b.stderr)
        self.assertIn("--allow-dirty-overlap", refused_b.stderr)
        self.assertIn("先提交源工作区现有改动", refused_b.stderr)
        # Zero files written by refused B: b_only.rs must NOT exist, shared.rs untouched!
        self.assertFalse((repo / "b_only.rs").exists())
        self.assertEqual((repo / "shared.rs").read_text(), "from_A\nline2\nline3\nline4\nline5\n")

        # 5. Explicit `--allow-dirty-overlap` accepts 3-way merge and prints `delegate files <runB>`!
        ok_b = self.cli("apply", "--allow-dirty-overlap", run_b["run"], human=True)
        self.assertEqual(ok_b.returncode, 0, ok_b.stdout + ok_b.stderr)
        self.assertEqual((repo / "shared.rs").read_text(), "from_A\nline2\nline3\nline4\nfrom_B\n")
        self.assertEqual((repo / "b_only.rs").read_text(), "b_only\n")
        ok_b_lines = ok_b.stdout.splitlines()
        self.assertEqual(ok_b_lines[0], f"APPLY OK {run_b['run']}")
        self.assertEqual(
            ok_b_lines[-2],
            f"本 run 改动文件（2 个）：delegate files {run_b['run']}",
        )
        self.assertEqual(ok_b_lines[-1], f"APPLY OK {run_b['run']}")
        files_b_json = self.cli("files", "--json", run_b["run"], cwd=repo)
        self.assertEqual(files_b_json.returncode, 0, files_b_json.stderr)
        doc_b = json.loads(files_b_json.stdout)
        self.assertEqual(doc_b["run"], run_b["run"])
        self.assertEqual(doc_b["count"], 2)
        self.assertEqual(doc_b["files"], ["b_only.rs", "shared.rs"])

    # ------------------------------------------------------------------
    # Fix 2: apply never triggers `forbidden slow operation` under state/.state.lock
    # ------------------------------------------------------------------
    def test_apply_does_not_trigger_forbidden_slow_operation_under_state_lock(self):
        repo = self.repo({"a.txt": "a0\n", "b.txt": "b0\n"})
        self.fake_pi([fixtures.answer("done a"), fixtures.SETTLED], pre="echo a1 > a.txt")
        run_a = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "dep-a", "--accept", "true", "edit a")
        )
        self.fake_pi([fixtures.answer("done b"), fixtures.SETTLED], pre="echo b1 > b.txt")
        run_b = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "indep-b", "--accept", "true", "edit b")
        )

        # Strip `finalEntries` from run A's changes.json so checking `is_applied_in_repo_readonly`
        # inside `.state.lock` would have called `recorded_entries` -> `final_changes` -> `git`
        # and panicked in debug / printed `forbidden slow operation (git/subprocess)` in release!
        changes_path = Path(run_a["dir"]) / "changes.json"
        ch = json.loads(changes_path.read_text(encoding="utf-8"))
        ch.pop("finalEntries", None)
        changes_path.write_text(json.dumps(ch), encoding="utf-8")

        # Add a work item waiting on run_applied:<run_a> AND a DAG queue item waiting on --after-applied <run_a>.
        w_add = self.cli(
            "work", "add", "downstream task", "--after-applied", run_a["run"], "--json", cwd=repo
        )
        self.assertEqual(w_add.returncode, 0, w_add.stdout + w_add.stderr)
        w_id = json.loads(w_add.stdout)["id"]

        prompt_file = self.work / "q-prompt.md"
        prompt_file.write_text("follow up\n", encoding="utf-8")
        q_add = self.cli(
            "queue", "add", "q-after-a", "--after-applied", run_a["run"],
            "--prompt-file", str(prompt_file), "--read-only", cwd=repo
        )
        self.assertEqual(q_add.returncode, 0, q_add.stdout + q_add.stderr)

        # Applying run B reconciles work items & advances DAG queue (evaluating run_applied:<run_a>):
        # MUST NOT panic or emit `forbidden slow operation`.
        ap_b = self.cli("apply", run_b["run"], cwd=repo, human=True)
        self.assertEqual(ap_b.returncode, 0, ap_b.stdout + ap_b.stderr)
        self.assertNotIn("forbidden slow operation", ap_b.stderr)
        self.assertNotIn("panicked", ap_b.stderr)

        # Now apply run A -> MUST also succeed cleanly with zero slow-op diagnostics AND unlock W001!
        ap_a = self.cli("apply", run_a["run"], cwd=repo, human=True)
        self.assertEqual(ap_a.returncode, 0, ap_a.stdout + ap_a.stderr)
        self.assertNotIn("forbidden slow operation", ap_a.stderr)
        self.assertNotIn("panicked", ap_a.stderr)
        w_show = self.cli("work", "show", w_id, "--json", cwd=repo)
        self.assertEqual(w_show.returncode, 0, w_show.stderr)
        self.assertEqual(json.loads(w_show.stdout)["state"], "ready")

    # ------------------------------------------------------------------
    # Fix 3A: agy policy refusal recovers on single same-session tail-append rewrite
    # ------------------------------------------------------------------
    def test_agy_policy_refusal_retries_once_on_same_session_and_recovers(self):
        repo = self.repo({"cfg.rs": "old\n"})
        counter_file, inv_log = self._install_scripted_agy(
            f"if call_idx == 1:\n"
            f"    Path('cfg.rs').write_text('updated\\n', encoding='utf-8')\n"
            f"    emit_agy({POLICY_REFUSAL_TEXT!r}, cid='conv-pol-ok')\n"
            f"else:\n"
            f"    emit_agy('已完成配置文件与凭据文件加载重构，验收通过。', cid='conv-pol-ok')\n"
        )
        res = self.cli(
            "run", "--worktree", "--workdir", repo, "--tier", "write",
            "--accept", "grep -q updated cfg.rs", "--name", "pol-recover", "refactor auth",
            human=True,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        self.assertEqual(int(counter_file.read_text().strip()), 2)

        invocations = [json.loads(ln) for ln in inv_log.read_text().splitlines() if ln.strip()]
        self.assertEqual(len(invocations), 2)
        # Second invocation MUST append to the SAME conversation & SAME model (cache-first!).
        self.assertEqual(invocations[1]["conv"], "conv-pol-ok")
        self.assertEqual(invocations[1]["model"], invocations[0]["model"])
        self.assertIn("凭据文件", invocations[1]["content"])

        st = self.outcome(self.cli("status", "pol-recover"))
        self.assertEqual(st["state"], "delivered")
        self.assertNotIn("policyRefused", st)
        self.assertTrue(st.get("policyRewriteAttempted"))
        result_md = (Path(st["dir"]) / "result.md").read_text(encoding="utf-8")
        self.assertIn("已完成配置文件与凭据文件加载重构", result_md)
        self.assertNotIn("Prohibited Use policy", result_md)

    # ------------------------------------------------------------------
    # Fix 3B: persistent policy refusal stops after 1 retry, marks refused, writes fallback
    # ------------------------------------------------------------------
    def test_agy_policy_refusal_persistent_marks_refused_and_writes_fallback(self):
        repo = self.repo({"cfg.rs": "old\n"})
        (repo / ".delegate.json").write_text(
            json.dumps({"accept": "grep -q updated cfg.rs", "review": {"enabled": True, "tier": "review"}}),
            encoding="utf-8",
        )
        counter_file, inv_log = self._install_scripted_agy(
            # Reviewer receives the review rubric prompt -> emit PASS!
            # Author calls 1 and 2 both hit Google Prohibited Use policy refusal!
            f"if 'PASS' in content and 'REWORK' in content:\n"
            f"    emit_agy('PASS\\n\\n## 摘要\\n改动正确，测试通过。\\n', cid='conv-rev')\n"
            f"elif call_idx == 1:\n"
            f"    Path('cfg.rs').write_text('updated\\n', encoding='utf-8')\n"
            f"    emit_agy({POLICY_REFUSAL_TEXT!r}, cid='conv-pol-fail')\n"
            f"else:\n"
            f"    emit_agy({POLICY_REFUSAL_TEXT!r}, cid='conv-pol-fail')\n"
        )
        res = self.cli(
            "run", "--worktree", "--workdir", repo, "--tier", "write",
            "--name", "pol-fail", "refactor auth",
            human=True,
        )
        # Must NOT be treated as a normal successful conclusion (exit 1)!
        self.assertEqual(res.returncode, 1, res.stdout + res.stderr)

        invocations = [json.loads(ln) for ln in inv_log.read_text().splitlines() if ln.strip()]
        author_calls = [x for x in invocations if not ("PASS" in x["content"] and "REWORK" in x["content"])]
        # Exactly 2 author calls (initial + 1 retry — NEVER loops!).
        self.assertEqual(len(author_calls), 2, author_calls)
        self.assertEqual(author_calls[1]["conv"], "conv-pol-fail")
        self.assertEqual(author_calls[1]["model"], author_calls[0]["model"])

        st_json = self.outcome(self.cli("status", "pol-fail"))
        self.assertTrue(st_json.get("policyRefused"), st_json)
        self.assertTrue(st_json.get("policyRewriteAttempted"), st_json)
        self.assertEqual(st_json["review"]["verdict"], "PASS")

        st_human = self.cli("status", "pol-fail", human=True).stdout
        self.assertIn("结论被模型服务拒绝", st_human)
        self.assertNotIn("已交付", st_human)

        run_dir = Path(st_json["dir"])
        result_md = (run_dir / "result.md").read_text(encoding="utf-8")
        self.assertNotIn("Prohibited Use policy", result_md)
        self.assertIn("结论被模型服务拒绝", result_md)
        self.assertIn("review.md", result_md)
        self.assertIn(f"delegate files {st_json['run']}", result_md)
        self.assertIn("cfg.rs", result_md)

        # Worktree edits were preserved and can still be applied by controller after reviewing!
        ap = self.cli("apply", st_json["run"], cwd=repo, human=True)
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)
        self.assertEqual((repo / "cfg.rs").read_text(), "updated\n")

    # ------------------------------------------------------------------
    # Fix 5: areas A/B auto-alternation (W038)
    # ------------------------------------------------------------------
    def test_areas_ab_alternates_deterministically_appends_l0_and_records_meta(self):
        repo = self.repo(
            {
                "crates/delegate/src/lib.rs": "pub fn f() {}\n",
                "crates/tui/src/main.rs": "fn main() {}\n",
                "docs/readme.md": "# doc\n",
                ".gitignore": ".local/\n",
            }
        )
        # 1. Without `areas` configured: completely inactive.
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
        no_cfg = self.outcome(
            self.cli("run", "--read-only", "--workdir", repo, "--name", "no-areas", "read crates/delegate/src/lib.rs")
        )
        no_cfg_meta = json.loads((Path(no_cfg["dir"]) / "meta.json").read_text(encoding="utf-8"))
        self.assertNotIn("area", no_cfg_meta)
        self.assertNotIn("areaGroup", no_cfg_meta)

        # 2. Configure two areas in `.delegate.json` with a 5-line L0 file (only first 3 non-empty lines append).
        l0_dir = repo / ".local/notes/areas"
        l0_dir.mkdir(parents=True, exist_ok=True)
        (l0_dir / "delegate.l0.md").write_text(
            "L0-DEL-1: 持锁规范\n\nL0-DEL-2: 改动口径\nL0-DEL-3: 验收路径\nL0-DEL-4: 不应追加\n",
            encoding="utf-8",
        )
        (l0_dir / "tui.l0.md").write_text("L0-TUI-1: 窄屏约束\nL0-TUI-2: 零锁快照\n", encoding="utf-8")
        (repo / ".delegate.json").write_text(
            json.dumps(
                {
                    "areas": {
                        "delegate": {
                            "paths": ["crates/delegate/"],
                            "l0": ".local/notes/areas/delegate.l0.md",
                        },
                        "tui": {
                            "paths": ["crates/tui/"],
                            "l0": ".local/notes/areas/tui.l0.md",
                        },
                        "missing": {
                            "paths": ["crates/missing/"],
                            "l0": ".local/notes/areas/missing.l0.md",
                        },
                    }
                }
            ),
            encoding="utf-8",
        )

        # Create a work item whose artifact hits `crates/delegate/` (brief itself has NO path!).
        w_add = self.cli(
            "work", "add", "delegate work item", "--artifact", "crates/delegate/src/lib.rs", "--json", cwd=repo
        )
        self.assertEqual(w_add.returncode, 0, w_add.stdout + w_add.stderr)
        w_id = json.loads(w_add.stdout)["id"]

        # 3. Four dispatches hitting `delegate` (mix of brief path and `--work` artifact path):
        #    MUST strictly alternate B (seq 1), A (seq 2), B (seq 3), A (seq 4).
        runs_spec = [
            ("del-1", ["--read-only", "--workdir", repo, "--name", "del-1", "检查 crates/delegate/src/lib.rs"]),
            ("del-2", ["--read-only", "--workdir", repo, "--work", w_id, "--name", "del-2", "按工作项检查"]),
            ("del-3", ["--read-only", "--workdir", repo, "--name", "del-3", "继续看 crates/delegate/src/lib.rs"]),
            ("del-4", ["--read-only", "--workdir", repo, "--work", w_id, "--name", "del-4", "再次核对"]),
        ]
        expected_groups = [("B", True, 1), ("A", False, 2), ("B", True, 3), ("A", False, 4)]
        for (_, args), (exp_group, exp_injected, exp_seq) in zip(runs_spec, expected_groups):
            self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
            st = self.outcome(self.cli("run", *args))
            meta = json.loads((Path(st["dir"]) / "meta.json").read_text(encoding="utf-8"))
            prompt_md = (Path(st["dir"]) / "prompt.md").read_text(encoding="utf-8")
            self.assertEqual(meta["area"], "delegate")
            self.assertEqual(meta["areaGroup"], exp_group)
            self.assertEqual(meta["areaExperiment"]["seq"], exp_seq)
            self.assertEqual(meta["areaExperiment"]["injected"], exp_injected)
            if exp_injected:
                self.assertIn("L0-DEL-1: 持锁规范", prompt_md)
                self.assertIn("L0-DEL-2: 改动口径", prompt_md)
                self.assertIn("L0-DEL-3: 验收路径", prompt_md)
                self.assertNotIn("L0-DEL-4: 不应追加", prompt_md)
            else:
                self.assertNotIn("L0-DEL-1: 持锁规范", prompt_md)

        # 4. Separate area (`tui`) maintains its own independent parity counter (starts at seq 1 -> B, seq 2 -> A).
        for idx, (exp_group, exp_injected) in enumerate([("B", True), ("A", False)], start=1):
            self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
            st = self.outcome(
                self.cli("run", "--read-only", "--workdir", repo, "--name", f"tui-{idx}", "检查 crates/tui/src/main.rs")
            )
            meta = json.loads((Path(st["dir"]) / "meta.json").read_text(encoding="utf-8"))
            prompt_md = (Path(st["dir"]) / "prompt.md").read_text(encoding="utf-8")
            self.assertEqual(meta["area"], "tui")
            self.assertEqual(meta["areaGroup"], exp_group)
            self.assertEqual(meta["areaExperiment"]["seq"], idx)
            self.assertEqual("L0-TUI-1: 窄屏约束" in prompt_md, exp_injected)

        # 5. Unmatched path & missing L0 file: neither crashes nor records fake A/B group.
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
        unmatched = self.outcome(
            self.cli("run", "--read-only", "--workdir", repo, "--name", "doc-only", "检查 docs/readme.md")
        )
        self.assertNotIn("area", json.loads((Path(unmatched["dir"]) / "meta.json").read_text(encoding="utf-8")))

        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
        missing_res = self.cli(
            "run", "--read-only", "--workdir", repo, "--name", "miss-l0", "检查 crates/missing/foo.rs"
        )
        self.assertEqual(missing_res.returncode, 0, missing_res.stdout + missing_res.stderr)
        self.assertIn("L0 指针文件缺失或为空", missing_res.stderr)
        miss_st = self.outcome(missing_res)
        self.assertNotIn("area", json.loads((Path(miss_st["dir"]) / "meta.json").read_text(encoding="utf-8")))


if __name__ == "__main__":
    unittest.main()
