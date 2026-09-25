import importlib.util
import json
import os
import shlex
import shutil
import subprocess
import tempfile
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


if __name__ == "__main__":
    unittest.main()
