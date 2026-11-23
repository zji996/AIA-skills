"""6.25.0 regressions:
- `delegate note <run> "<text>"`, list, `--done [<index>|--all]`, chain sharing
- `status` one-line pending todo count (`待办 N`) & `status <run>` full todo details
- `status` collapsing reply chains to one line (`合入员（第 3 轮） · ...`), `--all`/`--json` keeping every round
- `clean` preserving pending notes in `<runs>/notes.json` until `--done`
- `handoff` listing per-run pending notes and a `## 合入后待办` section for applied/cleaned runs
- `skills/delegate/hooks/claude-code-stop.py` (notes, unmerged+deferred.tsv, waiter, questioning, cross-repo)
"""
import json
import os
import shlex
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures

ROOT = Path(__file__).resolve().parent.parent
STOP_HOOK = ROOT / "skills/delegate/hooks/claude-code-stop.py"


class Delegate625Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def run_stop_hook(self, payload, env=None):
        run_env = {
            **self.env,
            "DELEGATE_BIN": str(fixtures.DELEGATE),
            "DELEGATE_STOP_HOOK_WAITERS": "",
        }
        if env:
            run_env.update(env)
        res = subprocess.run(
            [sys.executable, str(STOP_HOOK)],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            env=run_env,
            timeout=15,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        out = res.stdout.strip()
        return json.loads(out) if out else None

    def make_repo_at(self, path, files):
        path.mkdir(parents=True, exist_ok=True)
        for rel, content in files.items():
            target = path / rel
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content)
        subprocess.run(["git", "init", "-q"], cwd=path, check=True)
        subprocess.run(["git", "config", "user.email", "t@example.com"], cwd=path, check=True)
        subprocess.run(["git", "config", "user.name", "test"], cwd=path, check=True)
        subprocess.run(["git", "add", "."], cwd=path, check=True)
        subprocess.run(["git", "commit", "-qm", "init"], cwd=path, check=True)
        return path

    def test_note_add_list_done_and_chain_sharing(self):
        repo = self.repo({"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("round 1"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "服务升级", "--accept", "true", "task")
        )
        # Add note #1 on round 1.
        add1 = self.cli("note", r1["run"], "换线上服务", human=True)
        self.assertEqual(add1.returncode, 0, add1.stdout + add1.stderr)
        self.assertIn("#1", add1.stdout)
        self.assertIn("换线上服务", add1.stdout)

        # Reply to create round 2, then attach note #2 via round 2.
        self.fake_pi([fixtures.answer("round 2"), fixtures.SETTLED], pre="echo v2 > a.txt")
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "adjust"))
        add2 = self.cli("note", r2["run"], "跑一次补数据", human=True)
        self.assertEqual(add2.returncode, 0, add2.stdout + add2.stderr)
        self.assertIn("#2", add2.stdout)

        # Listing via round 1 or round 2 sees BOTH notes.
        for ref in (r1["run"], r2["run"]):
            listed = self.cli("note", ref, human=True)
            self.assertEqual(listed.returncode, 0, listed.stderr)
            self.assertIn("1. [ ] 换线上服务", listed.stdout)
            self.assertIn("2. [ ] 跑一次补数据", listed.stdout)

        # `status` one-line shows `待办 2`; `status <run>` detail lists full note text.
        status_list = self.cli("status", human=True)
        self.assertEqual(status_list.returncode, 0, status_list.stderr)
        self.assertIn("待办 2", status_list.stdout)
        self.assertNotIn("换线上服务", status_list.stdout)

        status_detail = self.cli("status", r2["run"], human=True)
        self.assertEqual(status_detail.returncode, 0, status_detail.stderr)
        self.assertIn("待办（未完成 2 / 共 2）", status_detail.stdout)
        self.assertIn("1. [ ] 换线上服务", status_detail.stdout)
        self.assertIn("2. [ ] 跑一次补数据", status_detail.stdout)

        # `--done` with >1 pending and no index/all gives actionable error.
        ambiguous = self.cli("note", r2["run"], "--done", human=True)
        self.assertNotEqual(ambiguous.returncode, 0)
        self.assertIn("--done 1", ambiguous.stderr)
        self.assertIn("--all", ambiguous.stderr)

        # Mark #1 done.
        done1 = self.cli("note", r2["run"], "--done", "1", human=True)
        self.assertEqual(done1.returncode, 0, done1.stdout + done1.stderr)
        self.assertIn("剩余 1 条未完成", done1.stdout)
        self.assertIn("待办 1", self.cli("status", human=True).stdout)

        # Now only 1 pending remains: bare `--done` completes it.
        done2 = self.cli("note", r1["run"], "--done", human=True)
        self.assertEqual(done2.returncode, 0, done2.stdout + done2.stderr)
        self.assertIn("剩余 0 条未完成", done2.stdout)
        self.assertNotIn("待办 ", self.cli("status", human=True).stdout)

        # Add two more and clear with `--done --all`.
        self.cli("note", r2["run"], "真机验证", human=True)
        self.cli("note", r2["run"], "改能力矩阵", human=True)
        done_all = self.cli("note", r2["run"], "--done", "--all", human=True)
        self.assertEqual(done_all.returncode, 0, done_all.stdout + done_all.stderr)
        self.assertIn("剩余 0 条未完成", done_all.stdout)

    def test_note_survives_clean_and_shows_in_status_and_handoff(self):
        repo = self.repo({"a.txt": "v0\n", "b.txt": "v0\n", "c.txt": "v0\n", "d.txt": "v0\n"})

        # Run A: unmerged with pending note.
        self.fake_pi([fixtures.answer("a done"), fixtures.SETTLED], pre="echo a1 > a.txt")
        run_a = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "task-a", "--accept", "true", "a")
        )
        self.cli("note", run_a["run"], "合入后检查告警", human=True)

        # Run B: applied (cleanOnApply -> .applied-cleaned) with pending note.
        self.fake_pi([fixtures.answer("b done"), fixtures.SETTLED], pre="echo b1 > b.txt")
        run_b = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "task-b", "--accept", "true", "b")
        )
        self.cli("note", run_b["run"], "换线上服务", human=True)
        self.assertEqual(self.cli("apply", "--json", run_b["run"]).returncode, 0)

        # Run C: cleaned completely with pending note.
        self.fake_pi([fixtures.answer("c done"), fixtures.SETTLED], pre="echo c1 > c.txt")
        run_c = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "task-c", "--accept", "true", "c")
        )
        self.cli("note", run_c["run"], "跑一次补数据", human=True)
        cleaned_c = self.cli("clean", "--no-archive", run_c["run"], human=True)
        self.assertEqual(cleaned_c.returncode, 0, cleaned_c.stdout + cleaned_c.stderr)
        self.assertIn("保留 1 条未完成待办", cleaned_c.stdout)
        self.assertFalse(Path(run_c["dir"]).exists(), "run directory should be removed by clean")

        # Run D: cleaned after its note was already marked done -> pruned completely.
        self.fake_pi([fixtures.answer("d done"), fixtures.SETTLED], pre="echo d1 > d.txt")
        run_d = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "task-d", "--accept", "true", "d")
        )
        self.cli("note", run_d["run"], "已完成事项", human=True)
        self.cli("note", run_d["run"], "--done", human=True)
        self.assertEqual(self.cli("clean", "--no-archive", run_d["run"], human=True).returncode, 0)

        # Since 6.31, `delegate note` is backed by machine-wide `<state>/work.jsonl` (not notes.json).
        work_file = Path(self.env["XDG_STATE_HOME"]) / "delegate" / "work.jsonl"
        self.assertTrue(work_file.is_file())

        # `status` from a fresh caller session STILL displays B (applied-cleaned) and C (cleaned), not D.
        prev_caller = self.env.get("DELEGATE_CALLER")
        self.env["DELEGATE_CALLER"] = "brand-new-session-after-clear"
        try:
            st_human = self.cli("status", human=True)
            self.assertEqual(st_human.returncode, 0, st_human.stderr)
            self.assertIn("task-a", st_human.stdout)
            self.assertIn("task-b", st_human.stdout)
            self.assertIn("task-c", st_human.stdout)
            self.assertIn("已清理 · 待办 1", st_human.stdout)
            self.assertNotIn("task-d", st_human.stdout)

            # `status <cleaned-run>` resolves from notes.json and prints full note text.
            st_c = self.cli("status", run_c["run"], human=True)
            self.assertEqual(st_c.returncode, 0, st_c.stderr)
            self.assertIn("跑一次补数据", st_c.stdout)

            # `handoff` shows A's note under unmerged tasks, and B + C under `## 合入后待办` (not D).
            ho = self.cli("handoff", cwd=repo, human=True)
            self.assertEqual(ho.returncode, 0, ho.stdout + ho.stderr)
            md = (Path(self.env["DELEGATE_RUNS"]) / "HANDOFF.md").read_text()
            self.assertIn("### 未合入任务的待办", md)
            self.assertIn("合入后检查告警", md)
            self.assertIn("## 合入后待办", md)
            post_section = md.split("## 合入后待办", 1)[1]
            self.assertIn("task-b", post_section)
            self.assertIn("换线上服务", post_section)
            self.assertIn("task-c", post_section)
            self.assertIn("跑一次补数据", post_section)
            self.assertNotIn("task-d", post_section)

            # Completing C's note removes it from status and handoff.
            done_c = self.cli("note", run_c["run"], "--done", human=True)
            self.assertEqual(done_c.returncode, 0, done_c.stdout + done_c.stderr)
            self.assertNotIn("task-c", self.cli("status", human=True).stdout)
            ho2 = self.cli("handoff", cwd=repo, human=True)
            self.assertEqual(ho2.returncode, 0)
            self.assertNotIn("task-c", (Path(self.env["DELEGATE_RUNS"]) / "HANDOFF.md").read_text())
        finally:
            if prev_caller is None:
                self.env.pop("DELEGATE_CALLER", None)
            else:
                self.env["DELEGATE_CALLER"] = prev_caller

    def test_status_collapses_reply_chain_while_all_and_json_keep_every_round(self):
        repo = self.repo({"a.txt": "v0\n"})

        # Round 1: asks a question.
        self.fake_pi([fixtures.answer("需要主控决定\n方案 A 还是 B？"), fixtures.SETTLED])
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "合入员", "--accept", "test -f ok.txt", "do work")
        )
        self.assertTrue(r1.get("question"), r1)

        # Round 2: reply -> fails acceptance (`rejected`).
        self.fake_pi([fixtures.answer("tried"), fixtures.SETTLED], pre="echo partial > a.txt")
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "选方案 A"))
        self.assertEqual(r2["state"], "rejected", r2)

        # Round 3: reply -> passes acceptance (`delivered`).
        self.fake_pi([fixtures.answer("fixed"), fixtures.SETTLED], pre="touch ok.txt")
        r3 = self.outcome(self.cli("reply", "--wait", r2["run"], "补上 ok.txt"))
        self.assertEqual(r3["state"], "delivered", r3)

        # Default human `status`: counts header + 1 collapsed line showing latest round & chain round count + history footer.
        default_out = self.cli("status", human=True)
        self.assertEqual(default_out.returncode, 0, default_out.stderr)
        lines = [ln for ln in default_out.stdout.splitlines() if ln.strip()]
        self.assertIn("运行中 0 · 待合入 1", lines[0])
        run_lines = [ln for ln in lines[1:] if not ln.startswith("另有 ") and not ln.startswith("归档占用 ")]
        self.assertEqual(len(run_lines), 1, default_out.stdout)
        self.assertTrue(run_lines[0].startswith("合入员（第 3 轮） · 已交付"), run_lines[0])
        self.assertNotIn("提问", run_lines[0])
        self.assertNotIn("未通过", run_lines[0])

        # `status --all` (human): shows every round individually.
        all_out = self.cli("status", "--all", human=True)
        self.assertEqual(all_out.returncode, 0, all_out.stderr)
        all_lines = [ln for ln in all_out.stdout.splitlines() if ln.strip()]
        self.assertEqual(len(all_lines), 3, all_out.stdout)
        self.assertIn("提问", all_out.stdout)
        self.assertIn("未通过", all_out.stdout)
        self.assertIn("已交付", all_out.stdout)

        # `status --json` returns actionable leaf + statusSummary; `status --all --json` returns all 3 rounds.
        json_out = self.cli("status")
        objs = [json.loads(ln) for ln in json_out.stdout.splitlines() if ln.strip()]
        run_ids = [o["run"] for o in objs if "run" in o]
        self.assertEqual(set(run_ids), {r3["run"]})
        self.assertTrue(any("statusSummary" in o for o in objs))

        all_json_out = self.cli("status", "--all")
        all_objs = [json.loads(ln) for ln in all_json_out.stdout.splitlines() if ln.strip()]
        all_run_ids = [o["run"] for o in all_objs if "run" in o]
        self.assertEqual(set(all_run_ids), {r1["run"], r2["run"], r3["run"]})

    def test_stop_hook_notes_unmerged_deferred_waiter_and_cross_repo(self):
        # Set up two separate git repositories with default `<repo>/.local/run/delegate` dirs
        # (unset DELEGATE_RUNS so delegate uses each repo's own `.local/run/delegate` and registers in XDG_STATE_HOME).
        repo_a = self.make_repo_at(self.work / "repo-alpha", {"a.txt": "v0\n"})
        repo_b = self.make_repo_at(self.work / "repo-beta", {"b.txt": "v0\n"})
        no_runs_env = {k: v for k, v in self.env.items() if k != "DELEGATE_RUNS"}

        def run_in(repo_dir, *args, human=False):
            cmd = [str(fixtures.DELEGATE), *args]
            if not human and args[0] in {"start", "run", "reply", "wait", "status", "stop", "clean"} and "--json" not in args:
                cmd.insert(2, "--json")
            return subprocess.run(cmd, cwd=repo_dir, capture_output=True, text=True, env=no_runs_env, timeout=30)

        def hook(cwd, extra_env=None, active=False):
            env = {
                **no_runs_env,
                "DELEGATE_BIN": str(fixtures.DELEGATE),
                "DELEGATE_STOP_HOOK_WAITERS": "",
            }
            if extra_env:
                env.update(extra_env)
            payload = {"cwd": str(cwd), "stop_hook_active": active}
            res = subprocess.run(
                [sys.executable, str(STOP_HOOK)],
                input=json.dumps(payload),
                capture_output=True,
                text=True,
                env=env,
                timeout=15,
            )
            self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
            out = res.stdout.strip()
            return json.loads(out) if out else None

        # 1) Unmerged delivered run in repo_a blocks; deferred.tsv silences it until stale.
        self.fake_pi([fixtures.answer("done a"), fixtures.SETTLED], pre="echo a1 > a.txt")
        ra = self.outcome(run_in(repo_a, "run", "--worktree", "--name", "feat-a", "--accept", "true", "task a"))

        # stop_hook_active=True ALWAYS passes through.
        self.assertIsNone(hook(repo_a, active=True))

        blocked = hook(repo_a)
        self.assertIsNotNone(blocked)
        self.assertEqual(blocked["decision"], "block")
        self.assertIn("feat-a", blocked["reason"])
        self.assertIn("delegate apply", blocked["reason"])

        # Add fresh deferral in repo_a/.local/delegate/deferred.tsv -> passes without mutating disk (6.32 zero-side-effect Stop hook).
        deferred = repo_a / ".local/delegate/deferred.tsv"
        deferred.parent.mkdir(parents=True, exist_ok=True)
        deferred.write_text(f"{ra['run']}\t等上游窗口\n")
        self.assertIsNone(hook(repo_a))
        self.assertEqual(deferred.read_text(), f"{ra['run']}\t等上游窗口\n")

        # Make deferral stale (> 4 hours old) -> blocks again with stale notice.
        old_ts = int(time.time()) - 5 * 3600
        deferred.write_text(f"{ra['run']}\t等上游窗口\t{old_ts}\n")
        stale_block = hook(repo_a)
        self.assertIsNotNone(stale_block)
        self.assertIn("超过 4 小时", stale_block["reason"])

        # Attach a post-merge note to ra, apply & clean ra -> deferred.tsv is tidied;
        # under 6.31 graded Stop hook, fresh (<24h) verify/note does not block turn end.
        self.assertEqual(run_in(repo_a, "note", ra["run"], "重启线上进程", human=True).returncode, 0)
        self.assertEqual(run_in(repo_a, "apply", "--json", ra["run"]).returncode, 0)
        self.assertEqual(run_in(repo_a, "clean", "--no-archive", ra["run"], human=True).returncode, 0)

        self.assertIsNone(hook(repo_a))
        # In 6.32, Stop hook is strictly zero-side-effect so it does not rewrite deferred.tsv on disk;
        # ra is already applied & cleaned so the hook passes regardless.
        # Note is still preserved in `delegate note` until `--done`.
        self.assertIn("重启线上进程", run_in(repo_a, "note", ra["run"], human=True).stdout)

        # Mark note done -> repo_a is now completely clean!
        self.assertEqual(run_in(repo_a, "note", ra["run"], "--done", human=True).returncode, 0)
        self.assertIsNone(hook(repo_a))

        # 2) Active running task without waiter blocks; with waiter passes.
        gate = self.work / "live-gate"
        self.fake_pi(
            [fixtures.answer("done live"), fixtures.SETTLED],
            pre=f"while [ ! -f {shlex.quote(str(gate))} ]; do sleep 0.05; done",
        )
        live = self.outcome(run_in(repo_a, "start", "--read-only", "--name", "long-job", "wait for gate"))
        self.addCleanup(lambda: (gate.touch(), run_in(repo_a, "stop", live["run"])))
        no_waiter = hook(repo_a)
        self.assertIsNotNone(no_waiter)
        self.assertIn("long-job", no_waiter["reason"])
        self.assertIn("wait", no_waiter["reason"])

        with_waiter = hook(repo_a, extra_env={"DELEGATE_STOP_HOOK_WAITERS": f"{fixtures.DELEGATE} wait"})
        self.assertIsNone(with_waiter)
        gate.touch()
        self.assertEqual(run_in(repo_a, "wait", live["run"]).returncode, 0)
        self.assertEqual(run_in(repo_a, "clean", "--no-archive", live["run"], human=True).returncode, 0)
        self.assertIsNone(hook(repo_a))

        # 3) Cross-repo scenario: controller is in repo_a, while repo_b has a questioning run!
        self.fake_pi([fixtures.answer("需要主控决定\n要改协议吗？"), fixtures.SETTLED])
        qb = self.outcome(run_in(repo_b, "run", "--read-only", "--name", "protocol-check", "check"))
        self.assertTrue(qb.get("question"), qb)

        # Even if a waiter is active in repo_a, repo_b's questioning run MUST block and name repo-beta!
        cross_q = hook(repo_a, extra_env={"DELEGATE_STOP_HOOK_WAITERS": f"{fixtures.DELEGATE} wait"})
        self.assertIsNotNone(cross_q)
        self.assertIn("【repo-beta】protocol-check", cross_q["reason"])
        self.assertIn(f"delegate reply {qb['run']}", cross_q["reason"])

        # Clean qb and create an unmerged write run + a cleaned note run in repo_b -> both alert from repo_a!
        self.assertEqual(run_in(repo_b, "clean", "--no-archive", qb["run"], human=True).returncode, 0)
        self.fake_pi([fixtures.answer("b work"), fixtures.SETTLED], pre="echo b2 > b.txt")
        ub = self.outcome(run_in(repo_b, "run", "--worktree", "--name", "beta-feat", "--accept", "true", "b work"))
        self.assertEqual(run_in(repo_b, "note", ub["run"], "验证 beta 真机", human=True).returncode, 0)

        cross_unmerged = hook(repo_a)
        self.assertIsNotNone(cross_unmerged)
        self.assertIn("【repo-beta】beta-feat", cross_unmerged["reason"])
        self.assertIn("delegate apply", cross_unmerged["reason"])


if __name__ == "__main__":
    unittest.main()
