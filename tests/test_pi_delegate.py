import importlib.util
import json
import os
import shlex
import shutil
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DELEGATE = ROOT / "skills/pi-delegation/scripts/pi_delegate.py"
SHIM = ROOT / "skills/pi-delegation/scripts/pi-delegate.sh"

spec = importlib.util.spec_from_file_location("pi_delegate", DELEGATE)
pi_delegate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pi_delegate)


def answer(text, stop="stop"):
    content = [{"type": "text", "text": text}] if text is not None else []
    return {"type": "message_end", "message": {"role": "assistant", "stopReason": stop, "model": "fake-model",
                                               "usage": {"input": 10, "output": 5, "cacheRead": 0},
                                               "content": content}}


SETTLED = {"type": "agent_settled"}
# Verbatim shape of a real failure: the model printed its tool call as text and stopped.
LEAKED = "Let's read `trainingStyles.ts` as well.call:default_api:read{limit:120,offset:1,path:src/trainingStyles.ts}"


def codex_events(answer_text="final answer", files=(), fail=None):
    """Shape of real `codex exec --json` output (codex-cli 0.156), incl. its config-warning items."""
    events = [{"type": "thread.started", "thread_id": "t1"},
              {"type": "item.completed", "item": {"id": "item_0", "type": "error",
                                                  "message": "Codex is ignoring 3 unrecognized configuration settings."}},
              {"type": "turn.started"},
              {"type": "item.completed", "item": {"id": "item_1", "type": "agent_message", "text": "Let me look first."}},
              {"type": "item.started", "item": {"id": "item_2", "type": "command_execution", "command": "cat calc.py"}},
              {"type": "item.completed", "item": {"id": "item_2", "type": "command_execution", "command": "cat calc.py",
                                                  "exit_code": 0, "status": "completed"}}]
    if files:
        events.append({"type": "item.completed", "item": {"id": "item_3", "type": "file_change",
                                                          "changes": [{"path": f, "kind": "update"} for f in files]}})
    if fail:
        events.append({"type": "turn.failed", "error": {"message": fail}})
        return events
    if answer_text is not None:
        events.append({"type": "item.completed", "item": {"id": "item_4", "type": "agent_message", "text": answer_text}})
    events.append({"type": "turn.completed", "usage": {"input_tokens": 52618, "cached_input_tokens": 38784,
                                                       "output_tokens": 430}})
    return events


class PiDelegateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="pi-delegate-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.bin = self.work / "bin"
        self.bin.mkdir()
        self.env = {**os.environ, "PATH": f"{self.bin}:{os.environ['PATH']}", "PI_DELEGATE_POLL": "0.1",
                    "PI_DELEGATE_RUNS": str(self.work / "runs"), "PI_LOG": str(self.work / "pi.log")}
        self.env.pop("PI_DELEGATE_ACTIVE", None)

    def fake_pi(self, *attempts, pre="", sleep=0, code=0):
        """Each attempt is a list of events; later calls reuse the last attempt."""
        lines = ["#!/bin/sh", "cat > /dev/null", 'echo "$$ $PI_DELEGATE_ACTIVE $*" >> "$PI_LOG"',
                 'n=$(wc -l < "$PI_LOG")', pre]
        for index, events in enumerate(attempts, 1):
            test = "true" if index == len(attempts) else f'[ "$n" -eq {index} ]'
            body = " ".join(shlex.quote(json.dumps(e)) for e in events)
            lines.append(f"if {test}; then sleep {sleep}; printf '%s\\n' {body}; exit {code}; fi")
        pi = self.bin / "pi"
        pi.write_text("\n".join(lines) + "\n")
        pi.chmod(0o755)

    def fake_codex(self, events, pre=""):
        lines = ["#!/bin/sh", 'cat > "$PI_LOG.codex-prompt"',
                 'echo "$$ ${PI_DELEGATE_AGENT:-} ${PI_DELEGATE_ACTIVE:-} $*" >> "$PI_LOG.codex"', pre,
                 "printf '%s\\n' " + " ".join(shlex.quote(json.dumps(e)) for e in events)]
        codex = self.bin / "codex"
        codex.write_text("\n".join(lines) + "\n")
        codex.chmod(0o755)

    def cli(self, *args, stdin=None, cwd=None, timeout=30):
        return subprocess.run([str(DELEGATE), *map(str, args)], cwd=cwd or self.work, env=self.env,
                              input=stdin, capture_output=True, text=True, timeout=timeout)

    def outcome(self, result):
        return next(json.loads(line) for line in result.stdout.splitlines() if line.startswith('{"run"'))

    def test_answered_run_reports_once_and_shim_forwards(self):
        write = {"type": "tool_execution_start", "toolName": "write", "args": {"path": "a.txt", "content": "x" * 5000}}
        self.fake_pi([write, answer("final delivery"), SETTLED])
        result = subprocess.run([str(SHIM), "run", "--name", "quick", "do", "it"], cwd=self.work, env=self.env,
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stderr)
        state = self.outcome(result)
        self.assertEqual((state["state"], state["files"], state["resultChars"], state["model"]),
                         ("answered", ["a.txt"], 15, "fake-model"))
        self.assertIn("final delivery", result.stdout)
        run = Path(state["dir"])
        self.assertNotIn("xxxxx", (run / "events.jsonl").read_text())
        self.assertEqual((run / "exit_code").read_text().strip(), "0")
        self.assertTrue((run / ".delivered").exists())
        self.assertEqual(self.cli("result", "last").stdout, "final delivery\n")
        self.assertIn("no active or undelivered", self.cli("wait", "--all").stderr)

    def test_accept_command_decides_delivery(self):
        self.fake_pi([answer("done"), SETTLED], pre="touch made.txt")
        result = self.cli("run", "--accept", "test -f made.txt", "make it")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.outcome(result)["state"], "delivered")
        self.assertTrue(self.outcome(result)["accept"]["ok"])
        result = self.cli("run", "--accept", "echo checking; echo boom >&2; exit 3", "make it")
        self.assertEqual(result.returncode, 1)
        state = self.outcome(result)
        self.assertEqual((state["state"], state["accept"]["exitCode"]), ("rejected", 3))
        self.assertIn("boom", state["accept"]["tail"])
        self.assertIn("[exit 3]", (Path(state["dir"]) / "accept.log").read_text())

    def test_acceptance_command_is_shared_unless_hidden(self):
        self.fake_pi([answer("done"), SETTLED])
        state = self.outcome(self.cli("run", "--name", "zh", "--accept", "make test", "修复解析器"))
        prompt = (Path(state["dir"]) / "prompt.md").read_text()
        self.assertTrue(prompt.startswith("修复解析器\n"))
        self.assertIn("完成标准", prompt)
        self.assertIn("```sh\nmake test\n```", prompt)
        self.assertEqual(state["name"], "zh")
        state = self.outcome(self.cli("run", "--accept", "npm test", "Fix the parser"))
        prompt = (Path(state["dir"]) / "prompt.md").read_text()
        self.assertIn("Definition of done", prompt)
        self.assertEqual(state["name"], "Fix the parser")
        state = self.outcome(self.cli("run", "--accept", "true", "--hide-accept", "Fix it"))
        self.assertEqual((Path(state["dir"]) / "prompt.md").read_text(), "Fix it\n")
        self.assertEqual(state["state"], "delivered")

    def test_accept_timeout_is_rejected(self):
        self.fake_pi([answer("done"), SETTLED])
        state = self.outcome(self.cli("run", "--accept", "sleep 5", "--accept-timeout", "1", "task"))
        self.assertEqual((state["state"], state["accept"]["exitCode"]), ("rejected", 124))

    def test_leaked_tool_call_is_rerun_once(self):
        self.fake_pi([answer(LEAKED), SETTLED], [answer("real answer"), SETTLED])
        result = self.cli("run", "--read-only", "review")
        self.assertEqual(result.returncode, 0, result.stderr)
        state = self.outcome(result)
        self.assertEqual((state["state"], state["attempts"]), ("answered", 2))
        self.assertIn("real answer", result.stdout)
        self.assertIn('"rerun"', (Path(state["dir"]) / "events.jsonl").read_text())
        self.assertIn("--tools read,grep,find,ls", (self.work / "pi.log").read_text())

    def test_malformed_answers_fail_after_reruns(self):
        self.fake_pi([answer(LEAKED), SETTLED])
        state = self.outcome(self.cli("run", "--retries", "0", "task"))
        self.assertEqual((state["state"], state["attempts"]), ("malformed", 1))
        self.assertIn("leaked tool call", state["error"])
        self.fake_pi([answer("earlier answer"), answer(None), SETTLED])
        (self.work / "pi.log").unlink()
        result = self.cli("run", "task")
        self.assertEqual(result.returncode, 1)
        self.assertEqual((self.outcome(result)["state"], self.outcome(result)["attempts"]), ("malformed", 2))

    def test_error_turn_timeout_and_kill_are_distinct(self):
        error = answer(None, stop="error")
        error["message"]["errorMessage"] = "provider failed"
        self.fake_pi([error, SETTLED])
        state = self.outcome(self.cli("run", "task"))
        self.assertEqual(state["state"], "failed")
        self.assertIn("provider failed", state["error"])
        self.fake_pi([answer("late"), SETTLED], sleep=5)
        state = self.outcome(self.cli("run", "--timeout", "1", "task"))
        self.assertEqual((state["state"], state["attempts"]), ("timeout", 1))
        self.fake_pi([answer("x"), SETTLED], code=137)
        self.assertEqual(self.outcome(self.cli("run", "task"))["state"], "killed")

    def test_leak_detector_ignores_normal_prose(self):
        self.assertTrue(pi_delegate.leaked_tool_call(LEAKED))
        self.assertFalse(pi_delegate.leaked_tool_call("Use `call:default_api:read{...}` carefully.\nDone."))
        self.assertFalse(pi_delegate.leaked_tool_call("Result: {a: 1}"))

    def test_nested_delegation_is_refused_and_guard_exported(self):
        self.fake_pi([answer("ok"), SETTLED])
        self.assertEqual(self.cli("run", "task").returncode, 0)
        self.assertEqual((self.work / "pi.log").read_text().split()[1], "1")
        self.env["PI_DELEGATE_ACTIVE"] = "1"
        result = self.cli("start", "task")
        self.assertEqual(result.returncode, 2)
        self.assertIn("nested delegation", result.stderr)

    def test_missing_pi_names_bundled_installer(self):
        if shutil.which("pi", path="/usr/bin:/bin"):
            self.skipTest("pi is installed in a system directory")
        self.env["PATH"] = "/usr/bin:/bin"
        result = self.cli("start", "task")
        self.assertEqual(result.returncode, 2)
        self.assertIn("missing required tools: pi", result.stderr)
        self.assertIn(f"sh {ROOT}/third_party/pi-kit/install.sh --additive", result.stderr)
        self.assertFalse((self.work / "runs").exists())

    def test_wait_slices_guard_writes_and_stop(self):
        self.fake_pi([answer("slow"), SETTLED], sleep=30)
        self.assertEqual(self.cli("start", "--name", "slow", "work").returncode, 0)
        result = self.cli("wait", "slow", "--max", "0.3")
        self.assertEqual(result.returncode, 75)
        self.assertEqual(self.outcome(result)["state"], "running")
        result = self.cli("start", "second", "writer")
        self.assertEqual(result.returncode, 2)
        self.assertIn("still active", result.stderr)
        self.assertEqual(self.cli("start", "--read-only", "reviewer").returncode, 0)
        pids = [int(line.split()[0]) for line in (self.work / "pi.log").read_text().splitlines()]
        result = self.cli("stop", "slow", "last")
        self.assertEqual([json.loads(line)["state"] for line in result.stdout.splitlines()], ["stopped", "stopped"])
        for pid in pids:
            with self.assertRaises(ProcessLookupError):
                os.kill(pid, 0)
        self.assertEqual(self.cli("clean", "--finished").stdout.count("removed"), 2)

    def test_concurrent_writers_share_start_lock(self):
        self.fake_pi([answer("slow"), SETTLED], sleep=30)
        barrier, results = threading.Barrier(3), []

        def start(name):
            barrier.wait()
            results.append(self.cli("start", "--name", name, "task"))

        workers = [threading.Thread(target=start, args=(name,)) for name in ("first", "second")]
        for worker in workers:
            worker.start()
        barrier.wait()
        for worker in workers:
            worker.join(timeout=20)
        self.assertEqual(sorted(r.returncode for r in results), [0, 2])
        self.assertIn("still active", next(r.stderr for r in results if r.returncode == 2))
        winner = json.loads(next(r.stdout for r in results if r.returncode == 0))["run"]
        self.assertEqual(self.cli("stop", winner).returncode, 0)

    def test_stale_starting_run_does_not_block_writer(self):
        stale = self.work / "runs/stale"
        stale.mkdir(parents=True)
        (stale / "meta.json").write_text(json.dumps({"run": "stale", "dir": str(stale), "workdir": str(self.work),
                                                     "mode": "write", "name": "stale",
                                                     "startedEpoch": int(time.time()) - 60, "startedNs": 1}))
        self.assertEqual(json.loads(self.cli("status", "stale").stdout)["state"], "crashed")
        self.fake_pi([answer("ok"), SETTLED])
        result = self.cli("run", "--name", "next", "task")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_long_answer_shows_tail_and_no_result_keeps_it_pending(self):
        self.fake_pi([answer("draft " * 1200 + "real answer"), SETTLED])
        self.assertEqual(self.cli("start", "question").returncode, 0)
        result = self.cli("wait", "--no-result")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("real answer", result.stdout)
        result = self.cli("wait", "--all")
        self.assertIn("showing the last 6000 chars", result.stdout)
        self.assertTrue(result.stdout.split("===== end")[0].rstrip().endswith("real answer"))
        self.assertLess(len(result.stdout), 8000)
        self.assertIn("draft draft", self.cli("wait", "last", "--full").stdout)

    def test_clean_keeps_unreported_and_start_prunes_expired(self):
        self.fake_pi([answer("ok"), SETTLED])
        result = self.cli("start", "--name", "old", "--prompt-file", "-", stdin="multi\nline\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        old = Path(json.loads(result.stdout)["dir"])
        self.assertEqual((old / "prompt.md").read_text(), "multi\nline\n")
        self.assertEqual(self.cli("wait", "old", "--no-result").returncode, 0)
        self.assertIn("keep unreported run", self.cli("clean", "--finished").stderr)
        self.assertTrue(old.exists())
        self.assertEqual(self.cli("wait", "old").returncode, 0)
        os.utime(old / "exit_code", (1, 1))
        self.assertEqual(self.cli("run", "--name", "new", "task").returncode, 0)
        self.assertFalse(old.exists())
        self.assertEqual(self.cli("start", "--name", "pending", "task").returncode, 0)
        self.assertEqual(self.cli("wait", "pending", "--no-result").returncode, 0)
        self.assertEqual(self.cli("clean", "--finished", "--force").stdout.count("removed"), 2)

    def test_git_reports_files_changed_outside_edit_tools(self):
        repo = self.work / "repo"
        repo.mkdir()
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        (repo / "dirty.txt").write_text("already modified before the run\n")
        self.fake_pi([answer("edited via shell"), SETTLED], pre="echo new > via-shell.txt")
        state = self.outcome(self.cli("run", "--workdir", repo, "--accept", "echo cache > accept-byproduct.txt", "task"))
        self.assertEqual(state["files"], ["via-shell.txt"])

    def test_runs_default_to_git_root_of_caller_with_gitignore(self):
        repo = self.work / "repo"
        (repo / "sub").mkdir(parents=True)
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        del self.env["PI_DELEGATE_RUNS"]
        self.fake_pi([answer("ok"), SETTLED])
        state = self.outcome(self.cli("run", "--read-only", "task", cwd=repo / "sub"))
        self.assertTrue(state["dir"].startswith(str(repo / ".local/run/pi/")))
        self.assertEqual((repo / ".local/run/pi/.gitignore").read_text(), "*\n")
        self.assertEqual(oct(Path(state["dir"]).stat().st_mode & 0o777), "0o700")

    def test_invalid_arguments_are_usage_errors(self):
        self.fake_pi([answer("ok"), SETTLED])
        self.assertEqual(self.cli("start", "--timeout", "0", "task").returncode, 2)
        self.assertEqual(self.cli("start", "   ").returncode, 2)
        self.assertEqual(self.cli("start", "--workdir", self.work / "missing", "task").returncode, 2)

    def test_codex_backend_delivers_through_the_same_contract(self):
        self.fake_codex(codex_events("fixed add()", files=[str(self.work / "calc.py")]), pre="touch made.txt")
        result = self.cli("run", "--agent", "codex", "--model", "gpt-test", "--thinking", "low",
                          "--accept", "test -f made.txt", "修复 add")
        self.assertEqual(result.returncode, 0, result.stderr)
        state = self.outcome(result)
        self.assertEqual((state["state"], state["agent"], state["model"], state["turns"]),
                         ("delivered", "codex", "gpt-test", 1))
        self.assertEqual(state["files"], ["calc.py"])
        self.assertEqual(state["tokens"], {"input": 52618, "output": 430, "cacheRead": 38784})
        self.assertIn("fixed add()", result.stdout)
        self.assertNotIn("Let me look first", result.stdout)
        call = (self.work / "pi.log.codex").read_text().split()
        self.assertEqual(call[1], "codex")  # the agent knows who it is, for nesting rules
        args = " ".join(call[2:])
        for flag in ("exec --json", "--skip-git-repo-check", "-m gpt-test", 'model_reasoning_effort="low"',
                     'approval_policy="never"'):
            self.assertIn(flag, args)
        self.assertNotIn("--sandbox", args)
        self.assertIn("```sh\ntest -f made.txt\n```", (self.work / "pi.log.codex-prompt").read_text())

    def test_codex_failures_and_empty_answers(self):
        self.fake_codex(codex_events(fail="stream disconnected"))
        state = self.outcome(self.cli("run", "--agent", "codex", "task"))
        self.assertEqual(state["state"], "failed")
        self.assertIn("stream disconnected", state["error"])
        self.fake_codex(codex_events(answer_text=None))
        state = self.outcome(self.cli("run", "--agent", "codex", "--retries", "0", "task"))
        self.assertEqual(state["state"], "answered")  # the earlier progress message is its last word
        events = codex_events(answer_text=None)
        events = [e for e in events if (e.get("item") or {}).get("type") != "agent_message"]
        self.fake_codex(events)
        self.assertEqual(self.outcome(self.cli("run", "--agent", "codex", "--retries", "0", "task"))["state"], "malformed")

    def test_codex_read_only_is_stated_and_checked_by_outcome(self):
        repo = self.work / "repo"
        repo.mkdir()
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        self.fake_codex(codex_events("looked"), pre="echo oops > stray.txt")
        state = self.outcome(self.cli("run", "--agent", "codex", "--read-only", "--workdir", repo, "审查一下"))
        self.assertEqual((state["state"], state["readOnlyViolation"]), ("failed", ["stray.txt"]))
        self.assertIn("read-only run changed files", state["error"])
        self.assertIn("只读任务", (self.work / "pi.log.codex-prompt").read_text())
        (repo / "stray.txt").unlink()
        self.fake_codex(codex_events("looked"))
        self.assertEqual(self.outcome(self.cli("run", "--agent", "codex", "--read-only", "--workdir", repo,
                                               "review"))["state"], "answered")

    def test_nesting_rules(self):
        self.fake_pi([answer("ok"), SETTLED])
        self.fake_codex(codex_events("ok"))
        self.env["PI_DELEGATE_AGENT"] = "pi"
        for agent in ("pi", "codex"):
            result = self.cli("start", "--agent", agent, "task")
            self.assertEqual(result.returncode, 2)
            self.assertIn("Pi run cannot delegate", result.stderr)
        self.env["PI_DELEGATE_AGENT"] = "codex"
        result = self.cli("start", "--agent", "codex", "task")
        self.assertEqual(result.returncode, 2)
        self.assertIn("not to Codex", result.stderr)
        self.assertEqual(self.outcome(self.cli("run", "--agent", "pi", "task"))["state"], "answered")
        guard = (self.work / "pi.log").read_text().split()
        self.assertEqual(guard[1], "1")  # Pi started by Codex still carries the no-delegation flag

    def test_helper_may_write_inside_its_parents_workdir(self):
        self.fake_pi([answer("slow"), SETTLED], sleep=30)
        parent = json.loads(self.cli("start", "--name", "parent", "task").stdout)["run"]
        self.assertEqual(self.cli("start", "--name", "rival", "task").returncode, 2)
        self.fake_pi([answer("helper done"), SETTLED])
        self.env.update(PI_DELEGATE_AGENT="codex", PI_DELEGATE_PARENT_RUN=parent)
        self.assertEqual(self.outcome(self.cli("run", "--name", "helper", "task"))["state"], "answered")
        del self.env["PI_DELEGATE_AGENT"], self.env["PI_DELEGATE_PARENT_RUN"]
        self.cli("stop", parent)


if __name__ == "__main__":
    unittest.main()
