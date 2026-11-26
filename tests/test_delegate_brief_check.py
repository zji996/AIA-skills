"""End-to-end tests for zero-token static brief preflight check on `start` / `reply`."""
import json
import os
import shutil
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateBriefCheckTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    fake_pi = fixtures.DelegateTests.fake_pi
    repo = fixtures.DelegateTests.repo

    def _run_prompt_md(self, run_id: str) -> str:
        return (self.work / "runs" / run_id / "prompt.md").read_text(encoding="utf-8")

    def test_missing_path_and_line_suffix_stripping(self):
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        repo = self.repo({
            "src/real.rs": "pub fn real_fn() {}\n",
            "README.md": "# demo\n",
        })
        prompt = (
            "改 `src/real.rs:10`、`README.md#L1-L2`、`src/missing.rs:42`、"
            "`src/absent.rs#L12-L20` 与 `ghost.toml`；忽略 `fill.target` 与 `6.49.0`。"
        )
        res = self.cli("start", "--read-only", "--workdir", repo, prompt)
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("brief: missing path src/missing.rs", res.stderr)
        self.assertIn("brief: missing path src/absent.rs", res.stderr)
        self.assertIn("brief: missing path ghost.toml", res.stderr)
        self.assertNotIn("src/real.rs", res.stderr)
        self.assertNotIn("README.md", res.stderr)
        self.assertNotIn("fill.target", res.stderr)
        self.assertNotIn("6.49.0", res.stderr)

        # Diagnostics must go to stderr only; stdout status line and prompt.md stay clean.
        self.assertNotIn("brief:", res.stdout)
        state = json.loads(res.stdout.splitlines()[0])
        w = self.cli("wait", state["run"])
        self.assertEqual(w.returncode, 0, w.stderr)
        saved_prompt = self._run_prompt_md(state["run"])
        self.assertNotIn("brief: missing path", saved_prompt)
        self.assertIn("`src/missing.rs:42`", saved_prompt)

    def test_zero_reference_symbol_and_scoped_last_segment(self):
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        repo = self.repo({
            "src/lib.rs": "pub struct RealType;\npub fn real_helper() {}\n",
        })
        prompt = (
            "核对 `src/lib.rs` 里的 `real_helper`、`RealType`、`ghost_symbol`、"
            "`MissingWidget`、`Foo::vanished_fn` 与 `Foo::bar`（短名忽略）和 `start`/`stderr`。"
        )
        res = self.cli("start", "--read-only", "--workdir", repo, prompt)
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("brief: symbol ghost_symbol not found", res.stderr)
        self.assertIn("brief: symbol MissingWidget not found", res.stderr)
        self.assertIn("brief: symbol vanished_fn not found", res.stderr)
        self.assertNotIn("symbol real_helper", res.stderr)
        self.assertNotIn("symbol RealType", res.stderr)
        self.assertNotIn("symbol bar", res.stderr)
        self.assertNotIn("symbol start", res.stderr)
        self.assertNotIn("symbol stderr", res.stderr)

        state = json.loads(res.stdout.splitlines()[0])
        w = self.cli("wait", state["run"])
        self.assertEqual(w.returncode, 0, w.stderr)

    def test_widely_referenced_symbol_threshold_and_counts(self):
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        files = {
            f"src/f{i}.rs": (
                "pub fn call() { wide_symbol(); }\n"
                + ("pub fn call2() { border_symbol(); }\n" if i < 5 else "")
            )
            for i in range(6)
        }
        repo = self.repo(files)
        # Brief names 1 path (M=1), so threshold is M + 5 = 6:
        # wide_symbol is in 6 files (>= 6 -> warn), border_symbol is in 5 files (< 6 -> silent).
        prompt = "改 `src/f0.rs` 里的 `wide_symbol` 和 `border_symbol`。"
        res = self.cli("start", "--read-only", "--workdir", repo, prompt)
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("brief: symbol wide_symbol is referenced in 6 files (brief names 1)", res.stderr)
        self.assertNotIn("border_symbol", res.stderr)

        state = json.loads(res.stdout.splitlines()[0])
        w = self.cli("wait", state["run"])
        self.assertEqual(w.returncode, 0, w.stderr)

    def test_no_brief_check_flag_and_config_disable(self):
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        repo = self.repo({"src/a.rs": "pub fn keep_me() {}\n"})
        prompt = "改 `src/nope.rs` 和 `missing_symbol`。"

        # --no-brief-check suppresses all brief: stderr diagnostics on start.
        res = self.cli("start", "--read-only", "--no-brief-check", "--workdir", repo, prompt)
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertNotIn("brief:", res.stderr)
        state = json.loads(res.stdout.splitlines()[0])
        w = self.cli("wait", state["run"])
        self.assertEqual(w.returncode, 0, w.stderr)

        # reply runs brief check by default, and --no-brief-check suppresses it.
        self.fake_pi([fixtures.answer("done2"), fixtures.SETTLED])
        rep = self.cli("reply", state["run"], "继续查 `src/missing_reply.rs` 和 `missing_reply_sym`")
        self.assertEqual(rep.returncode, 0, rep.stderr)
        self.assertIn("brief: missing path src/missing_reply.rs", rep.stderr)
        self.assertIn("brief: symbol missing_reply_sym not found", rep.stderr)
        rep_state = json.loads(rep.stdout.splitlines()[0])
        w2 = self.cli("wait", rep_state["run"])
        self.assertEqual(w2.returncode, 0, w2.stderr)

        self.fake_pi([fixtures.answer("done3"), fixtures.SETTLED])
        rep_off = self.cli(
            "reply", rep_state["run"], "--no-brief-check",
            "继续查 `src/missing_reply2.rs` 和 `missing_reply_sym2`",
        )
        self.assertEqual(rep_off.returncode, 0, rep_off.stderr)
        self.assertNotIn("brief:", rep_off.stderr)
        rep_off_state = json.loads(rep_off.stdout.splitlines()[0])
        w3 = self.cli("wait", rep_off_state["run"])
        self.assertEqual(w3.returncode, 0, w3.stderr)

        # .delegate.json {"briefCheck": false} suppresses diagnostics without CLI flag.
        (repo / ".delegate.json").write_text(json.dumps({"briefCheck": False}), encoding="utf-8")
        self.fake_pi([fixtures.answer("done4"), fixtures.SETTLED])
        pf = self.work / "brief.md"
        pf.write_text(prompt, encoding="utf-8")
        res_cfg = self.cli("start", "--read-only", "--workdir", repo, "--prompt-file", str(pf))
        self.assertEqual(res_cfg.returncode, 0, res_cfg.stderr)
        self.assertNotIn("brief:", res_cfg.stderr)
        cfg_state = json.loads(res_cfg.stdout.splitlines()[0])
        w4 = self.cli("wait", cfg_state["run"])
        self.assertEqual(w4.returncode, 0, w4.stderr)

        # Invalid briefCheck type fails config validation cleanly.
        (repo / ".delegate.json").write_text(json.dumps({"briefCheck": "no"}), encoding="utf-8")
        bad = self.cli("start", "--read-only", "--workdir", repo, "hi")
        self.assertEqual(bad.returncode, 2, bad.stderr)
        self.assertIn("briefCheck", bad.stderr)

    def test_missing_rg_and_rg_failure_do_not_block_dispatch(self):
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        repo = self.repo({"src/a.rs": "pub fn alpha_fn() {}\n"})

        # Build a hermetic PATH without `rg`.
        no_rg_bin = self.work / "no-rg-bin"
        no_rg_bin.mkdir()
        for tool in (
            "git", "sh", "bash", "env", "python3", "cat", "rm", "cp", "mv",
            "mkdir", "chmod", "ls", "ln", "true", "false", "sleep", "kill",
            "ps", "uname", "readlink", "stat", "touch", "dirname", "basename",
        ):
            found = shutil.which(tool)
            if found:
                (no_rg_bin / tool).symlink_to(found)
        no_rg_path = f"{self.bin}:{no_rg_bin}"
        self.assertIsNone(shutil.which("rg", path=no_rg_path))

        saved_path = self.env["PATH"]
        try:
            self.env["PATH"] = no_rg_path
            res = self.cli(
                "start", "--read-only", "--workdir", repo,
                "改 `src/nope.rs` 和 `missing_one` 以及 `missing_two`。",
            )
        finally:
            self.env["PATH"] = saved_path

        self.assertEqual(res.returncode, 0, res.stderr)
        # Path check still ran; missing rg emitted one note and skipped symbol checks.
        self.assertIn("brief: missing path src/nope.rs", res.stderr)
        self.assertEqual(res.stderr.count("rg not found"), 1, res.stderr)
        self.assertNotIn("symbol missing_one not found", res.stderr)
        state = json.loads(res.stdout.splitlines()[0])
        w = self.cli("wait", state["run"])
        self.assertEqual(w.returncode, 0, w.stderr)

        # Fake `rg` exiting with code 2 (search error) prints one line per failed symbol and never blocks.
        self.fake_pi([fixtures.answer("done2"), fixtures.SETTLED])
        fake_rg = self.bin / "rg"
        fake_rg.write_text("#!/bin/sh\nexit 2\n", encoding="utf-8")
        fake_rg.chmod(0o755)
        try:
            res_err = self.cli(
                "start", "--read-only", "--workdir", repo,
                "查 `src/a.rs` 里的 `alpha_fn`。",
            )
        finally:
            fake_rg.unlink(missing_ok=True)
        self.assertEqual(res_err.returncode, 0, res_err.stderr)
        self.assertIn("brief: symbol alpha_fn search failed", res_err.stderr)
        state2 = json.loads(res_err.stdout.splitlines()[0])
        w2 = self.cli("wait", state2["run"])
        self.assertEqual(w2.returncode, 0, w2.stderr)

    def test_git_ignored_paths_note_appended_for_worktree_only(self):
        repo = self.repo({
            ".gitignore": ".local/\n",
            "src/real.rs": "pub fn real_fn() {}\n",
        })
        plan_file = repo / ".local" / "plan" / "x.md"
        plan_file.parent.mkdir(parents=True, exist_ok=True)
        plan_file.write_text("# secret plan\n", encoding="utf-8")

        prompt = "按 `.local/plan/x.md` 修改 `src/real.rs`。"
        expected_note = f"以下文件不在工作区（被 git 忽略），可按绝对路径读：{plan_file}"

        # 1. --worktree (with --no-scout so writer launches directly) appends factual footer
        #    without modifying the original brief text.
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        res_wt = self.cli(
            "start", "--worktree", "--no-scout", "--accept", "true",
            "--workdir", repo, prompt,
        )
        self.assertEqual(res_wt.returncode, 0, res_wt.stderr)
        state_wt = json.loads(res_wt.stdout.splitlines()[0])
        w_wt = self.cli("wait", state_wt["run"])
        self.assertEqual(w_wt.returncode, 0, w_wt.stderr)
        prompt_wt = self._run_prompt_md(state_wt["run"])
        self.assertIn(prompt, prompt_wt)
        self.assertIn(expected_note, prompt_wt)
        self.assertNotIn(str(repo / "src" / "real.rs"), prompt_wt)

        # 2. Default --read-only (uses read-only worktree) also appends the factual footer.
        self.fake_pi([fixtures.answer("done-ro"), fixtures.SETTLED])
        res_ro = self.cli("start", "--read-only", "--workdir", repo, prompt)
        self.assertEqual(res_ro.returncode, 0, res_ro.stderr)
        state_ro = json.loads(res_ro.stdout.splitlines()[0])
        w_ro = self.cli("wait", state_ro["run"])
        self.assertEqual(w_ro.returncode, 0, w_ro.stderr)
        prompt_ro = self._run_prompt_md(state_ro["run"])
        self.assertIn(prompt, prompt_ro)
        self.assertIn(expected_note, prompt_ro)

        # 3. --in-place runs directly in source repo, so the note must NOT be appended.
        self.fake_pi([fixtures.answer("done-ip"), fixtures.SETTLED])
        res_ip = self.cli("start", "--read-only", "--in-place", "--workdir", repo, prompt)
        self.assertEqual(res_ip.returncode, 0, res_ip.stderr)
        state_ip = json.loads(res_ip.stdout.splitlines()[0])
        w_ip = self.cli("wait", state_ip["run"])
        self.assertEqual(w_ip.returncode, 0, w_ip.stderr)
        prompt_ip = self._run_prompt_md(state_ip["run"])
        self.assertIn(prompt, prompt_ip)
        self.assertNotIn("以下文件不在工作区（被 git 忽略）", prompt_ip)


if __name__ == "__main__":
    unittest.main()
