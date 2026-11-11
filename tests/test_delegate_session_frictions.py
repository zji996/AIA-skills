import fcntl
import hashlib
import json
import shlex
import subprocess
import threading
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateSessionFrictionsTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def write_run(self, repo, command, *options):
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre=command)
        result = self.cli("run", "--worktree", "--workdir", repo, *options, "task")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return self.outcome(result)

    def gated(self, repo=None):
        gate = self.work / "release"
        ready = self.work / "ready"
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre=f'touch {ready}; while [ ! -f {gate} ]; do sleep .05; done')
        options = ("--workdir", repo, "--worktree") if repo else ("--read-only",)
        state = self.outcome(self.cli("start", *options, "--name", "slow", "task"))
        self.addCleanup(lambda: self.cli("stop", state["run"]))
        self.until(ready.exists, "agent started")
        return gate, state

    def test_allow_subtree_inherits_without_changing_repo_defaults(self):
        config = {"defaults": {"protect": ["apps/", "tests/"]}}
        repo = self.repo({"apps/web/a": "old\n", "apps/api/a": "old\n",
                          "tests/a": "old\n", ".delegate.json": json.dumps(config)})
        state = self.write_run(repo, "echo new > apps/web/a", "--allow", "apps/web", "--allow", "tests/")
        prompt = (Path(state["dir"]) / "prompt.md").read_text()
        self.assertIn("Open for this task only: apps/web/, tests/", prompt)
        self.assertIn("Protected paths: apps/", prompt)
        self.assertNotIn("Protected paths: tests/", prompt)
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo reply > apps/web/a")
        reply = self.cli("reply", "--wait", state["run"], "continue")
        self.assertEqual(reply.returncode, 0, reply.stdout + reply.stderr)
        self.assertNotIn("protectViolation", self.outcome(reply))
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo forbidden > apps/api/a")
        violation = self.outcome(self.cli("reply", "--minor", "--wait", self.outcome(reply)["run"], "continue"))
        self.assertEqual(violation["protectViolation"], ["apps/api/a"])
        self.assertEqual(json.loads((repo / ".delegate.json").read_text()), config)
        for path in ("outside/", "../apps/", str(repo / "apps")):
            self.assertEqual(self.cli("start", "--workdir", repo, "--allow", path, "task").returncode, 2)

    def test_allow_entire_protected_directory_is_echoed_and_not_listed_closed(self):
        repo = self.repo({"apps/web/a": "old\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo new > apps/web/a")
        result = self.cli("run", "--workdir", repo, "--worktree", "--protect", "apps/web/", "--allow", "apps/web/", "task")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("apps/web/", result.stderr)
        self.assertNotIn("Protected paths:", (Path(self.outcome(result)["dir"]) / "prompt.md").read_text())

    def test_pending_reply_detaches_records_message_and_launches_once(self):
        gate, state = self.gated()
        refused = self.cli("reply", state["run"], "later")
        self.assertIn("--when-done", refused.stderr)
        queued = self.cli("reply", "--when-done", state["run"], "--prompt-file", "-", stdin="follow up\n")
        self.assertEqual(queued.returncode, 0, queued.stderr)
        self.assertEqual(self.outcome(queued)["pendingReply"]["message"], "follow up\n")
        self.assertIn("pending reply", self.cli("status", state["run"], human=True).stdout)
        self.assertEqual(self.cli("reply", "--when-done", state["run"], "duplicate").returncode, 2)
        # The queuing controller subprocess has already exited; worker still delivers.
        gate.touch()
        receipt = Path(state["dir"]) / "reply-delivered.json"
        self.until(receipt.exists, "pending reply delivered")
        child = json.loads(receipt.read_text())["run"]
        result = self.cli("wait", "--all", child)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("follow up", (Path(child) / "prompt.md").read_text())
        self.assertFalse((Path(state["dir"]) / "pending-reply.json").exists())
        self.assertEqual(len((self.work / "pi.log").read_text().splitlines()), 2)

    def test_any_wait_ignores_stopped_and_skipped_runs(self):
        gate, state = self.gated()
        skipped = self.outcome(self.cli("start", "--read-only", "--after", state["run"], "--name", "queued", "task"))
        stopped = self.outcome(self.cli("start", "--read-only", "--name", "stopped", "task"))
        timer = threading.Timer(.1, lambda: self.cli("stop", skipped["run"], stopped["run"]))
        timer.start()
        self.addCleanup(timer.join)
        result = self.cli("wait", "--any", "--max", "1.5", skipped["run"], stopped["run"], state["run"])
        timer.join()
        self.assertEqual(result.returncode, 75, result.stdout + result.stderr)
        gate.touch()
        result = self.cli("wait", "--any", skipped["run"], stopped["run"], state["run"])
        self.assertEqual(self.outcome(result)["run"], state["run"])
        self.assertNotIn("已跳过", result.stdout)
        self.assertNotIn("已停止", result.stdout)

    def test_pending_reply_survives_apply_cleanup(self):
        repo = self.repo({"a": "old\n"})
        gate, state = self.gated(repo)
        queued = self.cli("reply", "--when-done", state["run"], "follow after apply")
        self.assertEqual(queued.returncode, 0, queued.stderr)
        # Hold delivery while applying the just-settled parent, then release it.
        with (Path(state["dir"]) / "reply.lock").open("a+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            gate.touch()
            self.until(lambda: (Path(state["dir"]) / "exit_code").exists(), "parent settled")
            applied = self.cli("apply", state["run"])
            self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
            self.assertTrue((Path(state["dir"]) / "pending-reply.json").exists())
        receipt = Path(state["dir"]) / "reply-delivered.json"
        self.until(receipt.exists, "post-apply pending reply")
        child = json.loads(receipt.read_text())["run"]
        result = self.cli("wait", "--all", child)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("follow after apply", (Path(child) / "prompt.md").read_text())

    def test_stop_running_parent_cancels_pending_reply(self):
        _, state = self.gated()
        self.assertEqual(self.cli("reply", "--when-done", state["run"], "later").returncode, 0)
        self.cli("stop", state["run"])
        self.assertNotIn("pendingReply", self.outcome(self.cli("status", state["run"])))
        self.assertFalse((Path(state["dir"]) / "reply-delivered.json").exists())

    def test_pool_apply_and_zero_change_reply_keep_applied_bookkeeping(self):
        repo = self.repo({"a": "old\n", ".delegate.json": '{"worktree":{"pool":1}}'})
        state = self.write_run(repo, "echo new > a")
        # A no-edit reply can have all cumulative changes already included manually.
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        reply = self.outcome(self.cli("reply", "--wait", state["run"], "confirm"))
        (repo / "a").write_text("new\n")
        result = self.cli("apply", "--merge", reply["run"])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("could not remove worktree", result.stderr)
        self.assertTrue(Path(state["worktree"]).exists(), "pool slot stays cached")
        for run in (state, reply):
            self.assertTrue((Path(run["dir"]) / ".applied").exists())
            self.assertIn("已合入", self.cli("status", run["run"], human=True).stdout)
            self.assertNotIn("never applied", self.cli("clean", "--force", run["run"]).stdout)

    def test_pool_cleanup_releases_slot_without_failed_removal_warning(self):
        repo = self.repo({"a": "old\n", ".delegate.json": '{"worktree":{"pool":1}}'})
        state = self.write_run(repo, "echo new > a")
        result = self.cli("apply", state["run"])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("could not remove worktree", result.stderr)
        self.assertTrue(Path(state["worktree"]).exists())
        self.assertTrue(self.outcome(self.cli("status", state["run"]))["applied"])

    def test_successful_merge_marks_run_and_clean_does_not_warn(self):
        repo = self.repo({"a": "one\ntwo\nthree\nfour\nfive\n"})
        state = self.write_run(repo, "sed -i 's/one/theirs/' a")
        (repo / "a").write_text("one\ntwo\nthree\nfour\nmine\n")
        result = self.cli("apply", "--merge", state["run"])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue(self.outcome(self.cli("status", state["run"]))["applied"])
        self.assertNotIn("never applied", self.cli("clean", "--force", state["run"]).stdout)

    def test_status_is_cached_without_git_and_does_not_wait_on_source_or_start_lock(self):
        repo = self.repo({"a": "old\n"})
        settled = self.write_run(repo, "echo new > a")
        # Old records without frozen entries must not fall back to git diff in status.
        record = Path(settled["dir"]) / "changes.json"
        data = json.loads(record.read_text())
        data.pop("finalEntries", None)
        record.write_text(json.dumps(data))
        self.env["DELEGATE_MAX_ACTIVE"] = "1"
        gate, active = self.gated(repo)
        queued = self.outcome(self.cli("start", "--read-only", "--workdir", repo, "task"))
        self.addCleanup(lambda: self.cli("stop", queued["run"]))
        locks = [self.work / "state/delegate/.start.lock",
                 self.work / "state/delegate/source-watch" / hashlib.sha1(str(repo).encode()).hexdigest() / "lock"]
        held = []
        for path in locks:
            file = path.open("a+")
            fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
            held.append(file)
        # If any path invokes git, fail quickly and leave evidence instead of sleeping.
        real_git = subprocess.check_output(["which", "git"], text=True).strip()
        git_log = self.work / "status-git"
        git = self.bin / "git"
        git.write_text(f'#!/bin/sh\n[ -z "$STATUS_GIT_TRACE" ] || echo "$*" >> {git_log}\nexec {real_git} "$@"\n')
        git.chmod(0o755)
        self.env["STATUS_GIT_TRACE"] = "1"
        try:
            result = self.cli("status", settled["run"], active["run"], queued["run"], human=True, timeout=3)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("busy", result.stdout)
            calls = git_log.read_text().splitlines() if git_log.exists() else []
            self.assertTrue(all("rev-parse" in line for line in calls), calls)
        finally:
            for file in held:
                file.close()
        gate.touch()

    def test_bash_guard_allows_reads_and_local_but_refuses_source_writes(self):
        repo = self.repo({"a": "old\n", ".gitignore": "fixtures/\n"})
        (repo / "fixtures").mkdir()
        (repo / "fixtures/x.csv").write_text("fixture\n")
        source = shlex.quote(str(repo))
        commands = [(f"cd {source} && cat a", 0), (f"cat {source}/a", 0),
                    (f"mkdir -p {source}/.local; echo log > {source}/.local/log", 0),
                    (f"cd {source} && echo rogue > a", 77), (f"echo rogue > {source}/a", 77),
                    (f"cd {source} && touch rogue", 77), (f"cd {source} && sh helper.sh", 77),
                    (f"cp {source}/fixtures/x.csv copied", 0),
                    (f"cp copied {source}/a", 77), (f"cd {source} && cp a b", 77),
                    (f"cp -r {source}/fixtures imported", 0),
                    (f"install -m 644 {source}/a installed", 0),
                    (f"ln -s {source}/fixtures linked", 0),
                    (f"cp -t . {source}/a", 0), (f"cp -t {source} copied", 77)]
        codes = self.work / "codes"
        # Fake Pi, like the real bash tool, launches a noninteractive Bash for each call.
        pre = "\n".join(f"bash -c {shlex.quote(command)}; echo $? >> {codes}" for command, _ in commands)
        state = self.write_run(repo, pre)
        self.assertEqual(codes.read_text().splitlines(), [str(code) for _, code in commands])
        self.assertEqual((Path(state["worktree"]) / "copied").read_text(), "fixture\n")
        self.assertEqual((repo / "a").read_text(), "old\n")
        self.assertFalse((repo / "rogue").exists())
        self.assertFalse((repo / "b").exists())
        errors = (Path(state["dir"]) / "stderr.log").read_text().splitlines()
        refusals = [line for line in errors if "source checkout is read-only" in line]
        self.assertEqual(len(refusals), sum(code == 77 for _, code in commands))
        prefix = "source checkout is read-only for this task (write to "
        suffix = f" refused); work in {state['worktree']}"
        self.assertTrue(all(line.startswith(prefix) and line.endswith(suffix) for line in refusals))
        self.assertIn(f"{prefix}{repo / 'a'}{suffix}", refusals)
        self.assertIn(f"{prefix}{repo / 'b'}{suffix}", refusals)


if __name__ == "__main__":
    unittest.main()
