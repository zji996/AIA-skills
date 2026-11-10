"""6.14 capacity queue: hermetic CLI tests using per-run release gates."""
import json
import signal
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


class QueueTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until
    user_config = fixtures.DelegateTests.user_config

    def gates(self):
        self.env["DELEGATE_MIN_AVAILABLE_MB"] = "0"
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED],
                     pre='while [ ! -f "$DELEGATE_RUN_DIR/release" ]; do sleep .02; done')

    def start(self, name, *args):
        result = self.cli("start", "--read-only", "--name", name, *args, "task")
        self.assertEqual(result.returncode, 0, result.stderr)
        state = self.outcome(result)
        self.addCleanup(self.cli, "stop", state["dir"])
        return state

    def release(self, state):
        (Path(state["dir"]) / "release").touch()

    def status(self, state):
        return self.outcome(self.cli("status", state["dir"]))

    def started(self, state):
        return (Path(state["dir"]) / "agent.pid").is_file()

    def model_rules(self, overflow=None):
        rule = {"model": "review", "maxActive": 1}
        if overflow is not None:
            rule["overflow"] = overflow
        self.user_config({"tiers": {"scout": "main", "review": [rule, "write"], "write": "review"}})

    def test_total_capacity_returns_id_and_starts_automatically(self):
        self.gates()
        self.env["DELEGATE_MAX_ACTIVE"] = "1"
        first = self.start("first")
        second = self.start("second")
        self.assertEqual(second["state"], "waiting")
        self.assertIn("总路数", second["queue"]["reason"])
        self.assertFalse(self.started(second))
        self.release(first)
        self.until(lambda: self.started(second), "queued start admitted")
        self.release(second)
        self.assertEqual(self.cli("wait", second["dir"]).returncode, 0)

    def test_repo_capacity_does_not_block_another_repo(self):
        self.gates()
        self.env["DELEGATE_REPO_MAX_ACTIVE"] = "1"
        repo = self.repo({"a": "a"})
        first = self.start("first", "--workdir", repo, "--in-place")
        second = self.start("second", "--workdir", repo, "--in-place")
        other = self.start("other")
        self.assertIn("仓库路数", second["queue"]["reason"])
        self.until(lambda: self.started(other), "unrelated repository admitted")
        self.assertFalse(self.started(second))
        self.release(first)
        self.until(lambda: self.started(second), "repository queue admitted")

    def test_default_model_queue_keeps_identity(self):
        self.gates()
        self.model_rules()
        first = self.start("first")
        second = self.start("second")
        self.assertIn("模型并发", second["queue"]["reason"])
        self.assertIn("review", second["queue"]["reason"])
        self.assertIn("maxActive=1", second["queue"]["reason"])
        self.release(first)
        self.until(lambda: self.started(second), "same-model queue admitted")
        self.assertEqual(self.status(second)["model"], "p/review-m")
        self.assertNotIn("queue", self.status(second))

    def test_overflow_next_selects_next_model(self):
        self.gates()
        self.model_rules("next")
        self.start("first")
        second = self.start("second")
        self.until(lambda: self.started(second), "overflow started")
        self.assertEqual(second["model"], "p/write-m")
        self.assertNotIn("queue", second)

    def test_cli_overflow_overrides_both_directions(self):
        self.gates()
        self.model_rules("next")
        self.start("first")
        queued = self.start("queued", "--overflow", "queue")
        self.assertEqual(queued["state"], "waiting")
        self.model_rules()
        overflow = self.start("overflow", "--overflow", "next")
        self.assertEqual(overflow["model"], "p/write-m")
        self.until(lambda: self.started(overflow), "CLI overflow started")

    def test_pinned_model_and_other_tiers_share_model_ceiling(self):
        self.gates()
        self.model_rules()
        self.start("first", "--model", "review")
        second = self.start("second", "--model", "p/review-m")
        self.assertEqual(second["state"], "waiting")
        self.assertEqual(second["model"], "p/review-m")

    def test_blocked_model_does_not_block_later_other_model(self):
        self.gates()
        self.model_rules()
        self.start("first")
        blocked = self.start("blocked")
        other = self.start("other", "--model", "write")
        self.until(lambda: self.started(other), "other model admitted")
        self.assertFalse(self.started(blocked))

    def test_reply_has_priority_over_older_start(self):
        self.gates()
        self.env["DELEGATE_MAX_ACTIVE"] = "1"
        parent = self.start("parent")
        self.release(parent)
        self.assertEqual(self.cli("wait", parent["dir"]).returncode, 0)
        holder = self.start("holder")
        older = self.start("older")
        reply = self.outcome(self.cli("reply", parent["dir"], "continue"))
        self.addCleanup(self.cli, "stop", reply["dir"])
        self.assertEqual(reply["state"], "waiting")
        self.assertEqual(self.status(reply)["queue"]["ahead"], 0)
        self.assertEqual(self.status(older)["queue"]["ahead"], 1)
        self.release(holder)
        self.until(lambda: self.started(reply), "reply admitted first")
        self.assertFalse(self.started(older))
        self.release(reply)
        self.until(lambda: self.started(older), "older start eventually admitted")

    def test_stop_queue_claims_no_worktree_or_pool_and_clean_removes_it(self):
        self.gates()
        self.env["DELEGATE_MAX_ACTIVE"] = "1"
        self.start("holder")
        repo = self.repo({"a": "a"})
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"pool": 1}}))
        queued = self.outcome(self.cli("start", "--worktree", "--workdir", repo, "task"))
        self.addCleanup(self.cli, "stop", queued["dir"])
        meta = json.loads((Path(queued["dir"]) / "meta.json").read_text())
        self.assertFalse(meta["worktree"].get("path"))
        self.assertFalse(meta["worktree"].get("pooled"))
        self.cli("stop", queued["dir"])
        self.assertEqual(self.status(queued)["state"], "stopped")
        self.assertFalse(self.started(queued))
        self.assertEqual(self.cli("clean", queued["dir"]).returncode, 0)
        self.assertFalse(Path(queued["dir"]).exists())

    def test_status_queue_and_wait_any_leave_queued_unfinished(self):
        self.gates()
        self.env["DELEGATE_MAX_ACTIVE"] = "1"
        first = self.start("first")
        second = self.start("second", "--timeout", "0.1")
        third = self.start("third")
        status = self.status(third)
        self.assertEqual(set(status["queue"]), {"reason", "ahead", "since"})
        self.assertEqual(status["queue"]["ahead"], 1)
        self.assertNotIn("elapsedSeconds", status)
        human = self.cli("status", third["dir"], human=True).stdout
        for text in ("排队中 0 分钟", "总路数", "前面还有 1 个"):
            self.assertIn(text, human)
        result = self.cli("wait", "--any", "--max", "0.2", second["dir"], third["dir"])
        self.assertEqual(result.returncode, 75, result.stdout + result.stderr)
        self.assertEqual(self.status(second)["state"], "waiting")
        self.release(first)
        self.until(lambda: self.started(second), "queued task starts after wait timeout")

    def test_only_one_waiter_admitted_per_free_slot_fifo(self):
        self.gates()
        self.env["DELEGATE_MAX_ACTIVE"] = "2"
        first = self.start("first")
        holder = self.start("holder")
        second = self.start("second")
        third = self.start("third")
        self.release(first)
        self.until(lambda: self.started(second), "first waiter admitted")
        time.sleep(.3)
        self.assertFalse(self.started(third))
        self.release(holder)
        self.until(lambda: self.started(third), "second free slot admits next waiter")

    def test_after_enters_capacity_queue_only_after_upstream(self):
        self.gates()
        self.env["DELEGATE_MAX_ACTIVE"] = "1"
        first = self.start("first")
        after = self.start("after", "--after", first["dir"])
        other = self.start("other")
        self.assertNotIn("queue", after)
        self.assertEqual(self.status(other)["queue"]["ahead"], 0)
        self.release(first)
        self.until(lambda: self.started(other), "older ready waiter precedes after")
        self.until(lambda: "queue" in self.status(after), "after joins capacity queue")
        self.release(other)
        self.until(lambda: self.started(after), "after admitted")

    def test_unknown_overflow_rejected_at_config_load(self):
        self.gates()
        for value in ("typo", 1, None):
            self.user_config({"tiers": {"scout": "main", "write": "main", "review": [
                {"model": "review", "maxActive": 1, "overflow": value}, "main"]}})
            result = self.cli("start", "--read-only", "task")
            self.assertEqual(result.returncode, 2)
            self.assertIn("overflow", result.stderr)
        self.assertEqual(self.cli("start", "--overflow", "typo", "task").returncode, 2)

    def test_ordinary_reply_obeys_model_cap(self):
        self.gates()
        self.model_rules()
        parent = self.start("parent")
        self.release(parent)
        self.assertEqual(self.cli("wait", parent["dir"]).returncode, 0)
        holder = self.start("holder", "--model", "review")
        reply = self.outcome(self.cli("reply", parent["dir"], "continue"))
        self.addCleanup(self.cli, "stop", reply["dir"])
        self.assertIn("模型并发", reply["queue"]["reason"])
        self.release(holder)
        self.until(lambda: self.started(reply), "reply model cap released")
        self.assertEqual(self.status(reply)["model"], "p/review-m")

    def test_supervisor_loss_uses_existing_crashed_semantics(self):
        self.gates()
        self.env["DELEGATE_MAX_ACTIVE"] = "1"
        self.start("holder")
        queued = self.start("queued")
        pid = int((Path(queued["dir"]) / "pid").read_text())
        import os
        os.kill(pid, signal.SIGKILL)
        self.until(lambda: self.status(queued)["state"] == "crashed", "dead supervisor reported crashed")
        self.assertNotIn("queue", self.status(queued))
