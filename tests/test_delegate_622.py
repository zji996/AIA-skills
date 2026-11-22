"""6.22.0 regressions: --after removal, reply --allow, prompt protected-path hint,
wait --any multi-question reporting, status default scoping, and worktree.cleanOnApply."""
import json
import os
import shlex
import subprocess
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate622Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def test_after_removed_and_legacy_waiting_after_run_shows_redispatch(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        upstream = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "up", "first"))
        # --after is rejected with exit 2 and a clear removal message.
        removed = self.cli("start", "--read-only", "--after", upstream["run"], "chained")
        self.assertEqual(removed.returncode, 2)
        self.assertIn("--after was removed in 6.22.0", removed.stderr)
        # --in still works for finished runs and refuses active runs.
        reviewed = self.outcome(self.cli("run", "--read-only", "--in", upstream["run"], "review"))
        self.assertEqual(reviewed["state"], "answered")
        gate = self.work / "in-gate"
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre=f"while [ ! -f {shlex.quote(str(gate))} ]; do sleep .05; done")
        running = self.outcome(self.cli("start", "--worktree", "--workdir", repo, "--name", "busy", "busy"))
        self.addCleanup(lambda: (gate.touch(), self.cli("stop", running["run"])))
        refused_in = self.cli("start", "--read-only", "--in", running["run"], "review busy")
        self.assertEqual(refused_in.returncode, 2)
        self.assertIn("finished run", refused_in.stderr)
        gate.touch()
        self.cli("wait", running["run"])
        # Simulate a 6.21.x run still waiting on --after when upgraded to 6.22.0.
        legacy_dir = Path(upstream["dir"]).parent / "20261121-010000-legacy-after"
        legacy_dir.mkdir()
        (legacy_dir / "prompt.md").write_text("legacy follow-up task\n")
        (legacy_dir / ".waiting").write_text("")
        (legacy_dir / "meta.json").write_text(json.dumps({
            "run": legacy_dir.name,
            "dir": str(legacy_dir),
            "name": "legacy-after",
            "mode": "read-only",
            "agent": "pi",
            "tier": "review",
            "workdir": str(repo),
            "after": upstream["dir"],
            "startedEpoch": int(time.time()) - 10,
            "startedNs": int(time.time() * 1e9),
        }))
        status_json = self.outcome(self.cli("status", legacy_dir.name))
        self.assertEqual(status_json["state"], "needs-redispatch")
        self.assertIn("start", status_json["redispatch"])
        self.assertIn(str(legacy_dir / "prompt.md"), status_json["redispatch"])
        status_human = self.cli("status", legacy_dir.name, human=True).stdout
        self.assertIn("需要重派", status_human)
        self.assertIn("--prompt-file", status_human)
        # wait returns immediately (does not hang) with exit 1 and the re-dispatch command.
        waited = self.cli("wait", legacy_dir.name, human=True, timeout=5)
        self.assertEqual(waited.returncode, 1)
        self.assertIn("需要重派", waited.stdout)
        self.assertIn("--prompt-file", waited.stdout)
        # Running the printed re-dispatch command succeeds.
        self.fake_pi([fixtures.answer("redispatched"), fixtures.SETTLED])
        cmd = shlex.split(status_json["redispatch"])
        rerun = subprocess.run(cmd, cwd=self.work, env=self.env, capture_output=True, text=True, timeout=15)
        self.assertEqual(rerun.returncode, 0, rerun.stdout + rerun.stderr)
        # Wait for the newly re-dispatched run to finish cleanly before temp dir teardown.
        new_run = [p for p in Path(upstream["dir"]).parent.iterdir()
                   if p.is_dir() and p.name not in (upstream["run"], reviewed["run"], running["run"], legacy_dir.name)]
        for p in new_run:
            self.assertEqual(self.cli("wait", p.name).returncode, 0)

    def test_reply_allow_opens_protected_path_for_reply_chain_and_echoes(self):
        config = {"defaults": {"protect": ["apps/", "tests/"]}}
        repo = self.repo({
            "apps/web/a": "old\n",
            "apps/api/a": "old\n",
            "tests/t": "old\n",
            ".delegate.json": json.dumps(config),
        })
        self.fake_pi([fixtures.answer("需要主控决定\n可以修改 apps/web/ 吗？"), fixtures.SETTLED])
        asked = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "modify web"))
        self.assertTrue(asked["question"])
        # reply --allow opens apps/web for this reply and echoes it.
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo ok > apps/web/a")
        replied = self.cli("reply", "--wait", "--allow", "apps/web", asked["run"], "放开 apps/web")
        self.assertEqual(replied.returncode, 0, replied.stdout + replied.stderr)
        self.assertIn("apps/web/", replied.stderr)
        r1 = self.outcome(replied)
        self.assertNotIn("protectViolation", r1)
        self.assertIn("Open for this task only: apps/web/", (Path(r1["dir"]) / "prompt.md").read_text())
        # Subsequent reply inherits the newly allowed path, while still protecting apps/api/.
        self.fake_pi([fixtures.answer("more"), fixtures.SETTLED], pre="echo ok2 > apps/web/a")
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "continue on web"))
        self.assertNotIn("protectViolation", r2)
        self.fake_pi([fixtures.answer("oops"), fixtures.SETTLED], pre="echo bad > apps/api/a")
        r3 = self.outcome(self.cli("reply", "--minor", "--wait", r2["run"], "touch api"))
        self.assertEqual(r3["protectViolation"], ["apps/api/a"])
        # --allow outside protected paths is rejected with exit 2; repo config stays untouched.
        bad = self.cli("reply", "--allow", "outside/", r2["run"], "nope")
        self.assertEqual(bad.returncode, 2)
        self.assertEqual(json.loads((repo / ".delegate.json").read_text()), config)

    def test_start_warns_prompt_protected_paths_and_marks_status_and_wait(self):
        repo = self.repo({
            "apps/web/App.tsx": "old\n",
            "Cargo.toml": "[package]\n",
            ".delegate.json": json.dumps({"defaults": {"protect": ["apps/", "Cargo.toml"]}}),
        })
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        started = self.cli(
            "start", "--worktree", "--workdir", repo, "--allow", "Cargo.toml",
            "请参考 Cargo.toml 并修改 apps/web/App.tsx:12 的渲染逻辑",
        )
        self.assertEqual(started.returncode, 0, started.stdout + started.stderr)
        # Hint lists unallowed protected path and suggests --allow, does not block, omits already-allowed Cargo.toml.
        self.assertIn("apps/web/App.tsx", started.stderr)
        self.assertIn("--allow apps/web/App.tsx", started.stderr)
        self.assertNotIn("--allow Cargo.toml", started.stderr)
        run = self.outcome(started)
        self.assertEqual(run["promptProtectedPaths"], ["apps/web/App.tsx"])
        status_human = self.cli("status", run["run"], human=True).stdout
        self.assertIn("正文提及保护路径：apps/web/App.tsx", status_human)
        self.assertIn("--allow apps/web/App.tsx", status_human)
        waited = self.cli("wait", run["run"], human=True)
        self.assertEqual(waited.returncode, 0, waited.stderr)
        self.assertIn("正文提及保护路径 1 处", waited.stdout)
        # Read-only tasks do not emit the protected-path edit hint.
        ro = self.cli("start", "--read-only", "--workdir", repo, "read apps/web/App.tsx")
        self.assertEqual(ro.returncode, 0, ro.stderr)
        self.assertNotIn("正文提到受保护路径", ro.stderr)
        self.cli("wait", self.outcome(ro)["run"])

    def test_wait_any_reports_all_questions_and_completions_in_interval(self):
        gate = self.work / "slow-gate"
        self.fake_pi(
            [fixtures.answer("需要主控决定\n请决定方案：A 还是 B？"), fixtures.SETTLED],
            pre=f'case "$*" in *slow*) while [ ! -f {shlex.quote(str(gate))} ]; do sleep .05; done;; esac',
        )
        q1 = self.outcome(self.cli("start", "--read-only", "--name", "q1", "task one"))
        q2 = self.outcome(self.cli("start", "--read-only", "--name", "q2", "task two"))
        slow = self.outcome(self.cli("start", "--read-only", "--name", "slow", "task slow"))
        self.addCleanup(lambda: (gate.touch(), self.cli("stop", slow["run"])))
        self.until(lambda: (Path(q1["dir"]) / "exit_code").exists() and (Path(q2["dir"]) / "exit_code").exists(),
                   "both questions settled")
        waited = self.cli("wait", "--any", human=True)
        self.assertEqual(waited.returncode, 0, waited.stderr)
        # Both questions are reported in one wait --any call!
        self.assertIn("q1", waited.stdout)
        self.assertIn("q2", waited.stdout)
        # First gets full answer; second gets one-line summary + result view command (no second full block).
        self.assertIn(f"===== result: {q1['run']}", waited.stdout)
        self.assertNotIn(f"===== result: {q2['run']}", waited.stdout)
        self.assertIn(f"result {q2['run']}", waited.stdout)
        self.assertIn("提问", waited.stdout)
        self.assertIn("回答提问：delegate reply", waited.stdout)
        # q2 keeps .truncated until read with result, then clears it.
        self.assertTrue((Path(q2["dir"]) / ".truncated").exists())
        full_q2 = self.cli("result", q2["run"])
        self.assertEqual(full_q2.returncode, 0)
        self.assertIn("A 还是 B", full_q2.stdout)
        self.assertFalse((Path(q2["dir"]) / ".truncated").exists())
        # JSON mode also reports both with summaryOnly/view on the second.
        for run in (q1, q2):
            (Path(run["dir"]) / ".delivered").unlink(missing_ok=True)
        waited_json = self.cli("wait", "--any")
        json_lines = [json.loads(line) for line in waited_json.stdout.splitlines() if line.startswith('{"run"')]
        self.assertEqual([line["name"] for line in json_lines], ["q1", "q2"])
        self.assertTrue(json_lines[0]["question"])
        self.assertTrue(json_lines[1]["question"])
        self.assertTrue(json_lines[1]["summaryOnly"])
        self.assertIn(f"result {q2['run']}", json_lines[1]["view"])
        gate.touch()
        self.cli("wait", slow["run"])

    def test_status_default_shows_current_and_summarizes_older_uncleaned(self):
        gate = self.work / "status-gate"
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED],
                     pre=f'case "$*" in *live*) while [ ! -f {shlex.quote(str(gate))} ]; do sleep .05; done;; esac')
        self.env["DELEGATE_CALLER"] = "other-session"
        live = self.outcome(self.cli("start", "--read-only", "--name", "live", "running"))
        self.addCleanup(lambda: (gate.touch(), self.cli("stop", live["run"])))
        recent = self.outcome(self.cli("run", "--read-only", "--name", "recent", "recent"))
        old_a = self.outcome(self.cli("run", "--read-only", "--name", "old-a", "old a"))
        old_b = self.outcome(self.cli("run", "--read-only", "--name", "old-b", "old b"))
        old_cleaned = self.outcome(self.cli("run", "--read-only", "--name", "old-cleaned", "old cleaned"))
        self.env["DELEGATE_CALLER"] = "current-session"
        own_old = self.outcome(self.cli("run", "--read-only", "--name", "own-old", "own old"))

        three_days_ago = time.time() - 3 * 86400
        three_days_iso = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(three_days_ago))
        for idx, run in enumerate((old_a, old_b, old_cleaned, own_old)):
            d = Path(run["dir"])
            summary = json.loads((d / "summary.json").read_text())
            summary["finishedAt"] = three_days_iso
            (d / "summary.json").write_text(json.dumps(summary))
            os.utime(d / "exit_code", (three_days_ago, three_days_ago))
            if run in (old_a, old_b):
                wt = self.work / f"fake-wt-{idx}"
                wt.mkdir()
                meta = json.loads((d / "meta.json").read_text())
                meta["worktree"] = {"path": str(wt), "source": str(self.work)}
                meta["worktreeBytes"] = (idx + 1) * 1024 ** 3  # 1 GiB + 2 GiB = 3.0 GiB
                (d / "meta.json").write_text(json.dumps(meta))
        (Path(old_cleaned["dir"]) / ".applied-cleaned").write_text("1\n")

        # Default human status: shows live, recent, own-old; summarizes old-a + old-b in one line.
        default_human = self.cli("status", human=True)
        self.assertEqual(default_human.returncode, 0, default_human.stderr)
        for name in ("live", "recent", "own-old"):
            self.assertIn(name, default_human.stdout)
        for name in ("old-a", "old-b", "old-cleaned"):
            self.assertNotIn(name, default_human.stdout)
        self.assertIn("更早未清理：2 个，占 3.0 GiB", default_human.stdout)
        self.assertIn("status --all", default_human.stdout)
        self.assertIn("clean --finished", default_human.stdout)

        # Default JSON status: 3 runs + 1 olderUncleaned summary line.
        default_json = self.cli("status")
        objs = [json.loads(line) for line in default_json.stdout.splitlines() if line.strip()]
        run_names = [o["name"] for o in objs if "run" in o]
        self.assertEqual(set(run_names), {"live", "recent", "own-old"})
        summary_lines = [o["olderUncleaned"] for o in objs if "olderUncleaned" in o]
        self.assertEqual(len(summary_lines), 1)
        self.assertEqual(summary_lines[0]["count"], 2)
        self.assertEqual(summary_lines[0]["bytes"], 3 * 1024 ** 3)

        # --all lists every run and omits the summary line.
        all_json = self.cli("status", "--all")
        all_objs = [json.loads(line) for line in all_json.stdout.splitlines() if line.strip()]
        self.assertEqual(
            {o["name"] for o in all_objs if "run" in o},
            {"live", "recent", "old-a", "old-b", "old-cleaned", "own-old"},
        )
        self.assertFalse(any("olderUncleaned" in o for o in all_objs))
        gate.touch()
        self.cli("wait", live["run"])

    def test_clean_on_apply_config_and_reply_chain_worktree_cleanup(self):
        # 1. Default cleanOnApply=true archives evidence then deletes worktree, keeping archive.
        repo = self.repo({"a.txt": "old\n", ".gitignore": ".local/\n"})
        self.fake_pi([fixtures.answer("wrote .local/report.txt"), fixtures.SETTLED],
                     pre="echo new > a.txt; mkdir -p .local; echo evidence > .local/report.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        wt = Path(state["worktree"])
        self.assertTrue(wt.is_dir())
        applied = self.cli("apply", "--json", state["run"])
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertFalse(wt.exists(), "worktree should be deleted after apply by default")
        archive = Path(self.outcome(self.cli("status", state["run"]))["archive"])
        self.assertTrue((archive / "files/.local/report.txt").is_file())

        # 2. worktree.cleanOnApply=false keeps worktree after apply.
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"cleanOnApply": False}}))
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo v2 > a.txt")
        kept = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "keep-wt"))
        self.assertEqual(self.cli("apply", "--json", kept["run"]).returncode, 0)
        self.assertTrue(Path(kept["worktree"]).is_dir(), "cleanOnApply=false must keep worktree")

        # 3. Invalid cleanOnApply fails config validation with exit 2.
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"cleanOnApply": "yes"}}))
        self.assertEqual(self.cli("config", cwd=repo).returncode, 2)

        # 4. Reply chain sharing a worktree keeps it while a reply is active/unapplied,
        #    and deletes it once the whole chain is applied.
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"cleanOnApply": True}}))
        self.fake_pi([fixtures.answer("r1"), fixtures.SETTLED], pre="echo r1 > a.txt")
        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "chain"))
        gate = self.work / "reply-gate"
        self.fake_pi([fixtures.answer("r2"), fixtures.SETTLED],
                     pre=f"while [ ! -f {shlex.quote(str(gate))} ]; do sleep .05; done; echo r2 > a.txt")
        r2 = self.outcome(self.cli("reply", r1["run"], "followup"))
        self.addCleanup(lambda: (gate.touch(), self.cli("stop", r2["run"])))
        self.assertEqual(r1["worktree"], r2["worktree"])
        # While r2 is running in the shared worktree, apply refuses and keeps the worktree.
        refused = self.cli("apply", "--json", r1["run"])
        self.assertNotEqual(refused.returncode, 0)
        self.assertTrue(Path(r1["worktree"]).is_dir(), "worktree must stay while reply r2 is active")
        # Once r2 finishes and the chain is applied, archive is written and worktree is deleted.
        gate.touch()
        self.assertEqual(self.cli("wait", r2["run"]).returncode, 0)
        res = self.cli("apply", "--json", r2["run"])
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        self.assertFalse(Path(r1["worktree"]).exists(), "worktree should be deleted once whole chain is applied")
        self.assertTrue((Path(r1["dir"]) / ".applied").exists())
        self.assertTrue((Path(r2["dir"]) / ".applied").exists())


if __name__ == "__main__":
    unittest.main()
