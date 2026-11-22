"""6.15 parallel-write safety regressions, through the source CLI only."""
import json
import os
import shlex
import signal
import subprocess
import threading
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate615Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def run_write(self, repo, pre, name="task", *options):
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre=pre)
        return self.outcome(self.cli("run", "--worktree", "--workdir", repo,
                                     "--name", name, *options, "task"))

    def test_conflict_markers_block_whole_run_and_dry_run_until_resolved(self):
        repo = self.repo({"a.txt": "base\n", "side.txt": "side\n"})
        first = self.run_write(repo, "echo first > a.txt", "first")
        second = self.run_write(repo, "echo second > a.txt; echo changed > side.txt", "second")
        unrelated = self.run_write(repo, "echo other > other.txt", "unrelated")
        (repo / "a.txt").write_text("current\n")
        merged = self.cli("apply", "--merge", first["run"])
        self.assertEqual(merged.returncode, 1, merged.stderr)
        self.assertIn("conflict-markers", merged.stdout)
        self.assertIn("applied with conflict markers", merged.stderr)
        summary = json.loads((Path(first["dir"]) / "summary.json").read_text())
        self.assertEqual(summary["conflictMarkers"], ["a.txt"])
        self.assertIn("已合入·有冲突待解决", self.cli("status", first["run"], human=True).stdout)
        marked = (repo / "a.txt").read_bytes()
        for options in (("--merge",), ("--merge", "--dry-run"), ()):
            blocked = self.cli("apply", *options, second["run"])
            self.assertNotEqual(blocked.returncode, 0)
            self.assertIn("a.txt", blocked.stderr)
            self.assertIn(first["run"], blocked.stderr)
            self.assertEqual((repo / "a.txt").read_bytes(), marked)
            self.assertEqual((repo / "side.txt").read_text(), "side\n")
        self.assertEqual(self.cli("apply", unrelated["run"]).returncode, 0)
        (repo / "a.txt").write_text("base\n")
        self.assertNotIn("有冲突待解决", self.cli("status", first["run"], human=True).stdout)
        self.assertEqual(self.cli("apply", second["run"]).returncode, 0)
        self.assertEqual((repo / "a.txt").read_text(), "second\n")
        self.assertEqual((repo / "side.txt").read_text(), "changed\n")

    def test_conflict_from_earlier_round_does_not_block_disjoint_reply(self):
        repo = self.repo({"a.txt": "base\n", "side.txt": "side\n"})
        first = self.run_write(repo, "echo first > a.txt", "first")
        (repo / "a.txt").write_text("caller\n")
        self.assertEqual(self.cli("apply", "--merge", first["run"]).returncode, 1)
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo changed > side.txt")
        reply = self.outcome(self.cli("reply", "--wait", first["run"], "change side only"))
        self.assertEqual(self.cli("apply", reply["run"]).returncode, 0)
        self.assertEqual((repo / "side.txt").read_text(), "changed\n")
        self.assertIn("<<<<<<< current", (repo / "a.txt").read_text())
        self.assertIn("有冲突待解决", self.cli("status", first["run"], human=True).stdout)

    def test_delivered_markers_also_get_conflict_conclusion_and_summary(self):
        repo = self.repo({"a.txt": "base\n"})
        first = self.run_write(repo, "printf '%s\\n' '<<<<<<< current' 'oops' '>>>>>>> earlier-run' > a.txt")
        applied = self.cli("apply", first["run"])
        self.assertEqual(applied.returncode, 1, applied.stderr)
        self.assertIn("applied with conflict markers", applied.stdout.splitlines()[-1])
        self.assertEqual(json.loads((Path(first["dir"]) / "summary.json").read_text())["conflictMarkers"], ["a.txt"])

    def test_marker_preflight_includes_delete_and_keep_commits(self):
        repo = self.repo({"a.txt": "base\n"})
        state = self.run_write(repo, "rm a.txt; git add -A; git -c user.name=t -c user.email=t@t commit -qm delete")
        (repo / "a.txt").write_text(">>>>>>> old-run\n")
        for options in (("--merge",), ("--keep-commits", "--dry-run")):
            result = self.cli("apply", *options, state["run"])
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("old-run", result.stderr)
            self.assertEqual((repo / "a.txt").read_text(), ">>>>>>> old-run\n")

    def test_keep_commits_conflicts_are_summarized_and_visible(self):
        repo = self.repo({"a.txt": "base\n"})
        first = self.run_write(repo, "echo colleague > a.txt; git add a.txt; git -c user.name=t -c user.email=t@t commit -qm colleague")
        (repo / "a.txt").write_text("caller\n")
        subprocess.run(["git", "-C", str(repo), "add", "a.txt"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "caller"], check=True)
        result = self.cli("apply", "--keep-commits", first["run"])
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("applied with conflict markers", result.stdout.splitlines()[-1])
        self.assertEqual(json.loads((Path(first["dir"]) / "summary.json").read_text())["conflictMarkers"], ["a.txt"])
        self.assertIn("已合入·有冲突待解决", self.cli("status", first["run"], human=True).stdout)

    def test_patch_survives_apply_and_filters_space_paths_and_binary(self):
        repo = self.repo({"space name.txt": "old\n", "gone.txt": "remove\n", "bin.dat": "\x00old"})
        state = self.run_write(repo, "echo new > 'space name.txt'; rm gone.txt; printf '\\000new' > bin.dat")
        result = self.cli("apply", state["run"])
        self.assertEqual(result.returncode, 0, result.stderr)
        run = Path(state["dir"])
        for file in ("result.md", "summary.json", "meta.json", "applied.patch"):
            self.assertTrue((run / file).is_file(), file)
        self.assertFalse(Path(state["worktree"]).exists())
        patch = self.cli("patch", state["run"])
        self.assertEqual(patch.returncode, 0, patch.stderr)
        self.assertIn("GIT binary patch", patch.stdout)
        filtered = self.cli("patch", state["run"], "--", "space name.txt")
        self.assertEqual(filtered.returncode, 0, filtered.stderr)
        self.assertIn("space name.txt", filtered.stdout)
        self.assertNotIn("gone.txt", filtered.stdout)
        self.assertNotIn("bin.dat", filtered.stdout)
        subprocess.run(["git", "-C", str(repo), "restore", "--worktree", "."], check=True)
        restored = subprocess.run(["git", "-C", str(repo), "apply", "--3way"],
                                  input=patch.stdout, text=True, capture_output=True)
        self.assertEqual(restored.returncode, 0, restored.stderr)
        self.assertEqual((repo / "bin.dat").read_bytes(), b"\x00new")
        self.assertFalse((repo / "gone.txt").exists())
        self.assertEqual(self.cli("clean", "--finished").returncode, 0)
        self.assertFalse(run.exists())

    def test_patch_retention_releases_pool_slot(self):
        repo = self.repo({"a.txt": "old\n", ".delegate.json": json.dumps({"worktree": {"pool": 1}})})
        first = self.run_write(repo, "echo one > a.txt", "one")
        self.assertEqual(self.cli("apply", first["run"]).returncode, 0)
        self.assertTrue((Path(first["dir"]) / "applied.patch").is_file())
        second = self.run_write(repo, "echo two > a.txt", "two")
        self.assertEqual(first["worktree"], second["worktree"])
        self.assertIn("+one", self.cli("patch", first["run"]).stdout)
        self.assertNotIn("+two", self.cli("patch", first["run"]).stdout)
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo reply > a.txt")
        reply = self.outcome(self.cli("reply", "--wait", second["run"], "more"))
        self.assertEqual(self.cli("apply", reply["run"]).returncode, 0)
        third = self.run_write(repo, "echo three > a.txt", "three")
        self.assertEqual(first["worktree"], third["worktree"])

    def test_source_write_is_rejected_instead_of_question_and_warns_prompt(self):
        repo = self.repo({"a.txt": "old\n", ".delegate.json": '{"worktree":{"pool":1}}'})
        (repo / "already.txt").write_text("existing dirty\n")
        self.fake_pi([
            {"type": "tool_execution_start", "toolName": "write", "args": {"path": str(repo / "a.txt")}},
            fixtures.answer("需要主控决定\nWhat next?"),
            fixtures.SETTLED,
        ], pre=f"echo rogue > {shlex.quote(str(repo / 'a.txt'))}")
        result = self.cli("run", "--worktree", "--workdir", repo, f"inspect {repo}", human=True)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("note: prompt contains", result.stderr)
        self.assertIn("写到了源仓库：1 个文件", result.stdout)
        self.assertNotIn(" · 提问", result.stdout)
        state = self.outcome(self.cli("status", "last"))
        self.assertIn("写到了源仓库：1 个文件（a.txt）", self.cli("status", state["run"], human=True).stdout)
        self.assertEqual(state["sourceWriteViolation"], ["a.txt"])
        self.assertEqual(state["state"], "rejected")
        self.assertNotIn("question", state)
        meta = json.loads((Path(state["dir"]) / "meta.json").read_text())
        self.assertIn("already.txt", meta["sourceStatusBaseline"]["files"])
        self.assertIn("只在当前工作目录内改文件；提示里出现的其他绝对路径只读",
                      (Path(state["dir"]) / "prompt.md").read_text())

    def start_gated(self, repo, *options):
        gate = self.work / "release"
        ready = self.work / "ready"
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre=f"touch {ready}; while [ ! -f {gate} ]; do sleep .05; done")
        state = self.outcome(self.cli("start", "--workdir", repo, "--name", "slow", *options, "task"))
        self.addCleanup(lambda: self.cli("stop", state["run"]))
        self.until(ready.exists, "agent started")
        return gate, state

    def test_source_writes_visible_while_running_and_on_completion(self):
        repo = self.repo({"a.txt": "old\n"})
        gate, state = self.start_gated(repo, "--worktree")
        (repo / "a.txt").write_text("rogue\n")
        self.until(lambda: self.outcome(self.cli("status", state["run"])).get("sourceWorkspaceChanges") == ["a.txt"], "cached periodic source observation")
        gate.touch()
        result = self.cli("wait", "--all", state["run"])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        done = self.outcome(result)
        self.assertEqual(done["sourceWorkspaceChanges"], ["a.txt"])
        self.assertNotIn("sourceWriteViolation", done)

    def test_cumulative_reply_apply_registers_parent_paths(self):
        repo = self.repo({"a.txt": "old\n", "side.txt": "side\n"})
        gate, slow = self.start_gated(repo, "--worktree")
        first = self.run_write(repo, "echo parent > a.txt", "parent")
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo reply > side.txt")
        reply = self.outcome(self.cli("reply", "--wait", first["run"], "change side"))
        self.assertEqual(self.cli("apply", reply["run"]).returncode, 0)
        gate.touch()
        finished = self.cli("wait", "--all", slow["run"])
        self.assertEqual(finished.returncode, 0, finished.stderr)
        self.assertNotIn("sourceWriteViolation", self.outcome(finished))
        self.fake_pi([fixtures.answer("continued"), fixtures.SETTLED])
        continued = self.cli("reply", "--wait", reply["run"], "continue after cumulative apply")
        self.assertEqual(continued.returncode, 0, continued.stderr)
        prompt = (Path(self.outcome(continued)["dir"]) / "prompt.md").read_text()
        self.assertIn("continue after cumulative apply", prompt)
        self.assertIn("原会话已合入", continued.stderr)

    def test_large_untracked_text_is_recoverable_from_retained_patch(self):
        repo = self.repo({"a.txt": "old\n"})
        self.env["DELEGATE_SNAPSHOT_MAX_BYTES"] = "10"
        state = self.run_write(repo, "printf 'large text, not binary\\nmore lines\\n' > large.txt")
        self.assertEqual(self.cli("apply", state["run"]).returncode, 0)
        patch = self.cli("patch", state["run"], "--", "large.txt")
        self.assertEqual(patch.returncode, 0, patch.stderr)
        self.assertIn("+large text, not binary", patch.stdout)
        (repo / "large.txt").unlink()
        applied = subprocess.run(["git", "-C", str(repo), "apply", "--3way"],
                                 input=patch.stdout, text=True, capture_output=True)
        self.assertEqual(applied.returncode, 0, applied.stderr)
        self.assertEqual((repo / "large.txt").read_text(), "large text, not binary\nmore lines\n")

    def test_large_binary_patch_manifest_can_be_filtered(self):
        repo = self.repo({"a.txt": "old\n"})
        state = self.run_write(repo, "dd if=/dev/zero of=large.bin bs=1048576 count=3 2>/dev/null")
        self.assertEqual(self.cli("apply", state["run"]).returncode, 0)
        filtered = self.cli("patch", state["run"], "--", "large.bin")
        self.assertEqual(filtered.returncode, 0, filtered.stderr)
        self.assertIn("binary/large file omitted", filtered.stdout)
        self.assertIn("large.bin", filtered.stdout)
        self.assertEqual(self.cli("patch", state["run"], "--", "a.txt").stdout, "")

    def test_delegate_apply_is_exempt_but_later_edit_of_same_path_is_not(self):
        repo = self.repo({"a.txt": "old\n"})
        gate, slow = self.start_gated(repo, "--worktree")
        fast = self.run_write(repo, "echo merged > a.txt", "fast")
        self.assertEqual(self.cli("apply", fast["run"]).returncode, 0)
        self.assertNotIn("sourceWorkspaceChanges", self.outcome(self.cli("status", slow["run"])))
        (repo / "a.txt").write_text("rogue after apply\n")
        self.until(lambda: self.outcome(self.cli("status", slow["run"])).get("sourceWorkspaceChanges") == ["a.txt"], "cached observation after edit")
        (repo / "a.txt").write_text("merged\n")
        gate.touch()
        result = self.cli("wait", "--all", slow["run"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("sourceWriteViolation", self.outcome(result))

    def test_wait_all_with_already_delivered_run_waits_and_times_out_with_names(self):
        repo = self.repo({"a.txt": "old\n"})
        gate, slow = self.start_gated(repo, "--read-only")
        self.fake_pi([fixtures.answer("fast"), fixtures.SETTLED])
        fast = self.outcome(self.cli("run", "--read-only", "--workdir", repo, "--name", "fast", "task"))
        default = self.cli("wait", fast["run"], slow["run"], human=True)
        self.assertTrue(default.stdout.startswith("以下 run 已交付，未等待："), default.stdout)
        limited = self.cli("wait", "--all", fast["run"], slow["run"], "--max", ".1")
        self.assertEqual(limited.returncode, 75, limited.stderr)
        self.assertIn(slow["run"], limited.stderr)
        timer = threading.Timer(.2, gate.touch)
        timer.start()
        self.addCleanup(timer.cancel)
        finished = self.cli("wait", "--all", fast["run"], slow["run"])
        self.assertEqual(finished.returncode, 0, finished.stderr)
        self.assertEqual(self.outcome(self.cli("status", slow["run"]))["state"], "answered")

    def test_protect_overlap_without_input_changes_does_not_run_generator(self):
        cfg = {"generated": {"paths": ["src/generated/"], "inputs": ["src/"],
                             "command": "echo should-not-run; sleep 60"}}
        repo = self.repo({"src/generated/out.txt": "output\n", "docs.md": "doc\n", ".delegate.json": json.dumps(cfg)})
        self.env["DELEGATE_GENERATE_TIMEOUT"] = ".1s"
        state = self.run_write(repo, "echo updated > docs.md", "docs", "--protect", "src/")
        self.assertEqual(state["state"], "answered")
        self.assertFalse((Path(state["dir"]) / "protect-generate.log").exists())
        applied = self.cli("apply", state["run"])
        self.assertEqual(applied.returncode, 0, applied.stderr)
        self.assertIn("not regenerated", applied.stdout)
        self.assertIn("no merged path under generated.inputs", applied.stdout)

    def test_protected_generation_still_runs_when_input_changes(self):
        cfg = {"generated": {"paths": ["src/generated/"], "inputs": ["input.txt"],
                             "command": "cat input.txt > src/generated/out.txt"}}
        repo = self.repo({"src/generated/out.txt": "old\n", "input.txt": "old\n", ".delegate.json": json.dumps(cfg)})
        state = self.run_write(repo, "echo new > input.txt; cat input.txt > src/generated/out.txt",
                               "input", "--protect", "src/generated/")
        self.assertEqual(state["state"], "answered")
        self.assertTrue((Path(state["dir"]) / "protect-generate.log").exists())
        self.assertEqual(self.cli("apply", state["run"]).returncode, 0)
        self.assertEqual((repo / "src/generated/out.txt").read_text(), "new\n")

    def test_shell_readonly_tiers_can_pin_model_and_clean_queries_succeed(self):
        repo = self.repo({"a.txt": "old\n"})
        for tier in ("scout", "review"):
            self.fake_pi([fixtures.answer("looked"), fixtures.SETTLED], pre="git log -1 --oneline >/dev/null; test -f a.txt")
            result = self.cli("run", "--tier", tier, "--model", "named", "--shell", "--workdir", repo, "logs and SELECT")
            state = self.outcome(result)
            self.assertEqual(result.returncode, 0, result.stderr)
            meta = json.loads((Path(state["dir"]) / "meta.json").read_text())
            self.assertEqual((state["mode"], meta["model"], state["tier"]), ("read-only", "x/named", tier))
            self.assertIn("--tools read,grep,find,ls,bash", (self.work / "pi.log").read_text().splitlines()[-1])
            self.assertNotEqual(self.cli("apply", state["run"]).returncode, 0)
        write = self.run_write(repo, "echo new > a.txt", "write", "--tier", "write", "--model", "named")
        self.assertEqual(json.loads((Path(write["dir"]) / "meta.json").read_text())["model"], "x/named")
        self.assertEqual(write["mode"], "write")
        for options in (("--shell",), ("--shell", "--read-only", "--in-place")):
            self.assertEqual(self.cli("start", "--workdir", repo, *options, "task").returncode, 2)

    def test_shell_readonly_rejects_worktree_and_source_writes_and_apply(self):
        repo = self.repo({"a.txt": "old\n"})
        cases = (
            ("echo local > a.txt", []),
            (
                f"echo source > {shlex.quote(str(repo / 'a.txt'))}",
                [{"type": "tool_execution_start", "toolName": "bash", "args": {"command": f"echo source > {shlex.quote(str(repo / 'a.txt'))}"}}],
            ),
        )
        for pre, tool_events in cases:
            self.fake_pi([*tool_events, fixtures.answer("需要主控决定\nwhy?"), fixtures.SETTLED], pre=pre)
            result = self.cli("run", "--read-only", "--shell", "--workdir", repo, "inspect", human=True)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("只读任务产生了写入", result.stdout)
            state = self.outcome(self.cli("status", "last"))
            self.assertEqual(state["state"], "rejected")
            self.assertIn("readOnlyViolation", state)
            self.assertNotIn("question", state)
            self.assertNotEqual(self.cli("apply", "--keep-commits", state["run"]).returncode, 0)

    def start_gated_question(self, repo, name="slow", *options):
        gate = self.work / f"release-{name}"
        ready = self.work / f"ready-{name}"
        self.fake_pi([fixtures.answer("需要主控决定\nWhich option?"), fixtures.SETTLED],
                     pre=f"touch {ready}; while [ ! -f {gate} ]; do sleep .05; done")
        state = self.outcome(self.cli("start", "--workdir", repo, "--name", name, "--worktree", *options, "task"))
        self.addCleanup(lambda: self.cli("stop", state["run"]))
        self.until(ready.exists, f"agent {name} started")
        return gate, state

    def test_three_round_sync_apply_interrupted_during_generate_registers_written_paths(self):
        gen_started = self.work / "gen-started"
        gen_gate = self.work / "gen-gate"
        gen_pid = self.work / "gen-pid"
        gen_cmd = (
            f"echo $$ > {shlex.quote(str(gen_pid))}; "
            f"touch {shlex.quote(str(gen_started))}; "
            f"while [ ! -f {shlex.quote(str(gen_gate))} ]; do sleep .05; done; "
            f"cp c.txt gen/out.txt"
        )
        cfg = {"generated": {"paths": ["gen/"], "inputs": ["c.txt"], "command": gen_cmd}}
        repo = self.repo({
            "a.txt": "a0\n",
            "b.txt": "b0\n",
            "c.txt": "c0\n",
            "other.txt": "o0\n",
            "gen/out.txt": "gen0\n",
            ".delegate.json": json.dumps(cfg),
        })
        first = self.run_write(repo, "echo a1 > a.txt; echo n1 > new1.txt", "round1")
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo b2 > b.txt")
        reply1 = self.outcome(self.cli("reply", "--wait", first["run"], "round2"))
        (repo / "other.txt").write_text("o1\n")
        subprocess.run(["git", "-C", str(repo), "add", "other.txt"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "-qm", "caller update"], check=True)
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre="echo a3 > a.txt; echo c3 > c.txt")
        reply2 = self.outcome(self.cli("reply", "--wait", "--sync=agent", "--over-limit", "third round",
                                       reply1["run"], "round3"))

        gate, slow = self.start_gated_question(repo, "question-neon")
        proc = subprocess.Popen(
            [str(fixtures.DELEGATE), "apply", reply2["run"]],
            env=self.env, cwd=repo, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.addCleanup(lambda: proc.poll() is None and proc.kill())
        def kill_gen():
            if gen_pid.is_file():
                try:
                    os.killpg(int(gen_pid.read_text().strip()), signal.SIGKILL)
                except OSError:
                    pass
        self.addCleanup(kill_gen)
        self.until(gen_started.exists, "generator started")
        proc.send_signal(signal.SIGTERM)
        proc.communicate(timeout=10)
        self.assertNotEqual(proc.returncode, 0)
        # Generator child group must be stopped cleanly rather than mutating source as an orphan.
        self.until(lambda: fixtures.process_gone(gen_pid), "generator child terminated on SIGTERM")
        self.assertEqual((repo / "a.txt").read_text(), "a3\n")
        self.assertEqual((repo / "new1.txt").read_text(), "n1\n")
        self.assertEqual((repo / "b.txt").read_text(), "b2\n")
        self.assertEqual((repo / "c.txt").read_text(), "c3\n")
        self.assertEqual((repo / "gen/out.txt").read_text(), "gen0\n")

        gate.touch()
        finished = self.cli("wait", "--all", slow["run"])
        self.assertEqual(finished.returncode, 0, finished.stdout + finished.stderr)
        state = self.outcome(finished)
        self.assertEqual(state["state"], "answered")
        self.assertTrue(state.get("question"))
        self.assertNotIn("sourceWriteViolation", state)
        self.assertNotIn("sourceWorkspaceChanges", state)

        # Retrying apply finishes pending generation and registers generated outputs too.
        gate2, slow2 = self.start_gated_question(repo, "question-retry")
        gen_gate.touch()
        retried = self.cli("apply", reply2["run"])
        self.assertEqual(retried.returncode, 0, retried.stdout + retried.stderr)
        self.assertEqual((repo / "gen/out.txt").read_text(), "c3\n")
        gate2.touch()
        finished2 = self.cli("wait", "--all", slow2["run"])
        self.assertEqual(finished2.returncode, 0, finished2.stdout + finished2.stderr)
        self.assertNotIn("sourceWriteViolation", self.outcome(finished2))

    def test_apply_sigkill_during_generate_still_registers_already_written_paths(self):
        gen_started = self.work / "kill-gen-started"
        gen_gate = self.work / "kill-gen-gate"
        gen_cmd = (
            f"touch {shlex.quote(str(gen_started))}; "
            f"if [ ! -f {shlex.quote(str(gen_gate))} ]; then sleep 2; exit 1; fi; "
            f"cp a.txt gen/out.txt"
        )
        cfg = {"generated": {"paths": ["gen/"], "inputs": ["a.txt"], "command": gen_cmd}}
        repo = self.repo({"a.txt": "a0\n", "b.txt": "b0\n", "gen/out.txt": "gen0\n",
                          ".delegate.json": json.dumps(cfg)})
        first = self.run_write(repo, "echo a1 > a.txt; echo b1 > b.txt", "first")
        gate, slow = self.start_gated_question(repo, "question-kill")
        proc = subprocess.Popen(
            [str(fixtures.DELEGATE), "apply", first["run"]],
            env=self.env, cwd=repo, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        )
        self.addCleanup(lambda: proc.poll() is None and proc.kill())
        self.until(gen_started.exists, "generator started before SIGKILL")
        proc.send_signal(signal.SIGKILL)
        proc.communicate(timeout=10)

        gate.touch()
        finished = self.cli("wait", "--all", slow["run"])
        self.assertEqual(finished.returncode, 0, finished.stdout + finished.stderr)
        self.assertNotIn("sourceWriteViolation", self.outcome(finished))

        gen_gate.touch()
        retried = self.cli("apply", first["run"])
        self.assertEqual(retried.returncode, 0, retried.stdout + retried.stderr)
        self.assertEqual((repo / "gen/out.txt").read_text(), "a1\n")

    def test_commit_after_apply_exempts_baseline_dirty_files_only_when_committed_and_logged(self):
        repo = self.repo({"a.txt": "a0\n", "other.txt": "o0\n", "manual.txt": "m0\n"})
        first = self.run_write(repo, "echo a1 > a.txt; echo new > added.txt", "first")
        self.assertEqual(self.cli("apply", first["run"]).returncode, 0)

        # Case 1: applied before slow started (baseline sees dirty a.txt & added.txt), then committed during slow.
        gate1, slow1 = self.start_gated_question(repo, "commit-applied")
        subprocess.run(["git", "-C", str(repo), "add", "a.txt", "added.txt"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "-qm", "land applied"], check=True)
        gate1.touch()
        res1 = self.cli("wait", "--all", slow1["run"])
        self.assertEqual(res1.returncode, 0, res1.stdout + res1.stderr)
        self.assertNotIn("sourceWriteViolation", self.outcome(res1))
        self.assertNotIn("sourceWorkspaceChanges", self.outcome(res1))

        # Case 2: dirty at baseline, updated by apply during slow, then committed during slow.
        (repo / "a.txt").write_text("a1\nextra-line\n")
        second = self.run_write(repo, "echo a2 > a.txt", "second")
        gate2, slow2 = self.start_gated_question(repo, "apply-then-commit")
        self.assertEqual(self.cli("apply", "--merge", second["run"]).returncode, 0)
        subprocess.run(["git", "-C", str(repo), "add", "a.txt"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "-qm", "land second"], check=True)
        gate2.touch()
        res2 = self.cli("wait", "--all", slow2["run"])
        self.assertEqual(res2.returncode, 0, res2.stdout + res2.stderr)
        self.assertNotIn("sourceWriteViolation", self.outcome(res2))

        # Case 3 (guardrail): reverting an applied file while committing an UNRELATED file must NOT be exempted.
        third = self.run_write(repo, "echo a3 > a.txt", "third")
        self.assertEqual(self.cli("apply", third["run"]).returncode, 0)
        gate3, slow3 = self.start_gated_question(repo, "revert-not-committed")
        (repo / "other.txt").write_text("o1\n")
        subprocess.run(["git", "-C", str(repo), "add", "other.txt"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "-qm", "unrelated commit"], check=True)
        subprocess.run(["git", "-C", str(repo), "checkout", "--", "a.txt"], check=True)
        gate3.touch()
        res3 = self.cli("wait", "--all", slow3["run"])
        self.assertEqual(res3.returncode, 0, res3.stdout + res3.stderr)
        out3 = self.outcome(res3)
        self.assertEqual(out3["sourceWorkspaceChanges"], ["a.txt"])
        self.assertNotIn("sourceWriteViolation", out3)

        # Case 4 (6.25.0): controller committing their own dirty file (advancing HEAD) is exempted
        # because the working tree has no uncommitted diff left for that path.
        (repo / "manual.txt").write_text("m1\n")
        gate4, slow4 = self.start_gated_question(repo, "manual-committed")
        subprocess.run(["git", "-C", str(repo), "add", "manual.txt"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "-qm", "commit manual"], check=True)
        gate4.touch()
        res4 = self.cli("wait", "--all", slow4["run"])
        self.assertEqual(res4.returncode, 0, res4.stdout + res4.stderr)
        out4 = self.outcome(res4)
        self.assertNotIn("sourceWorkspaceChanges", out4)
        self.assertNotIn("sourceWriteViolation", out4)


if __name__ == "__main__":
    unittest.main()
