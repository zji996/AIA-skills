import importlib.util
import json
import os
import shlex
import shutil
import signal
import subprocess
import tempfile
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DELEGATE = ROOT / "skills/delegate/scripts/delegate.py"

spec = importlib.util.spec_from_file_location("delegate", DELEGATE)
delegate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(delegate)
from delegate_core import agents  # noqa: E402 (importable once delegate.py has run)


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


class DelegateTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="delegate-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.bin = self.work / "bin"
        self.bin.mkdir()
        self.env = {k: v for k, v in os.environ.items() if not k.startswith(("DELEGATE_", "PI_DELEGATE_"))}
        self.env.update(PATH=f"{self.bin}:{os.environ['PATH']}", DELEGATE_POLL="0.1",
                        DELEGATE_RUNS=str(self.work / "runs"), XDG_STATE_HOME=str(self.work / "state"),
                        XDG_CACHE_HOME=str(self.work / "cache"),
                        PI_LOG=str(self.work / "pi.log"))

    def fake_pi(self, *attempts, pre="", sleep=0, code=0):
        """Each attempt is a list of events; later calls reuse the last attempt."""
        lines = ["#!/bin/sh", "cat > /dev/null", 'echo "$$ $PI_DELEGATE_ACTIVE $*" >> "$PI_LOG"',
                 # Like Pi, keep the session as <dir>/<time>_<id>.jsonl.
                 'dir=; id=; prev=; for a in "$@"; do case "$prev" in --session-dir) dir=$a;; --session-id) id=$a;; '
                 'esac; prev=$a; done; [ -n "$dir" ] && mkdir -p "$dir" && touch "$dir/t_${id:-f$$}.jsonl"',
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
                 'echo "$$ ${DELEGATE_AGENT:-} ${PI_DELEGATE_ACTIVE:-} $*" >> "$PI_LOG.codex"', pre,
                 "printf '%s\\n' " + " ".join(shlex.quote(json.dumps(e)) for e in events)]
        codex = self.bin / "codex"
        codex.write_text("\n".join(lines) + "\n")
        codex.chmod(0o755)

    def cli(self, *args, stdin=None, cwd=None, timeout=30):
        return subprocess.run([str(DELEGATE), *map(str, args)], cwd=cwd or self.work, env=self.env,
                              input=stdin, capture_output=True, text=True, timeout=timeout)

    def outcome(self, result):
        lines = [line for line in result.stdout.splitlines() if line.startswith('{"run"')]
        self.assertTrue(lines, result.stdout + result.stderr)
        return json.loads(lines[0])

    def test_answered_run_reports_once(self):
        write = {"type": "tool_execution_start", "toolName": "write", "args": {"path": "a.txt", "content": "x" * 5000}}
        self.fake_pi([write, answer("final delivery"), SETTLED])
        result = self.cli("run", "--name", "quick", "do", "it")
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
        self.assertIn(f"reply {state['run']}", state["next"])
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
        self.assertTrue(agents.leaked_tool_call(LEAKED))
        self.assertFalse(agents.leaked_tool_call("Use `call:default_api:read{...}` carefully.\nDone."))
        self.assertFalse(agents.leaked_tool_call("Result: {a: 1}"))

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
        del self.env["DELEGATE_RUNS"]
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
                     "--dangerously-bypass-approvals-and-sandbox"):
            self.assertIn(flag, args)  # full access regardless of the host's config.toml
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
        # The answer stands; the stray write stayed in the run's own worktree and is only reported.
        self.assertEqual((state["state"], state["readOnlyViolation"]), ("answered", ["stray.txt"]))
        self.assertIn("never applied", state["warning"])
        self.assertNotIn("error", state)
        self.assertFalse((repo / "stray.txt").exists())
        self.assertTrue((Path(state["worktree"]) / "stray.txt").exists())
        self.assertIn("只读任务", (self.work / "pi.log.codex-prompt").read_text())
        self.assertIn("nothing to apply", self.cli("apply", state["run"]).stderr)
        self.fake_codex(codex_events("looked"))
        clean = self.outcome(self.cli("run", "--agent", "codex", "--read-only", "--workdir", repo, "review"))
        self.assertEqual(clean["state"], "answered")
        self.assertNotIn("readOnlyViolation", clean)
        self.assertNotIn("next", clean)
        # In place, a change cannot be told apart from the caller's own; it is reported, not judged.
        self.fake_codex(codex_events("looked"), pre="echo oops > stray.txt")
        state = self.outcome(self.cli("run", "--agent", "codex", "--read-only", "--in-place", "--workdir", repo,
                                      "review"))
        self.assertEqual((state["state"], state["workspaceChanged"]), ("answered", ["stray.txt"]))
        self.assertNotIn("worktree", state)
        self.assertEqual(self.cli("run", "--in-place", "--workdir", repo, "task").returncode, 2)

    def test_callers_edits_during_a_read_only_run_are_not_its_changes(self):
        repo = self.repo({"a.txt": "a\n", ".gitignore": "setup-ran\n"})
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"setup": ["touch setup-ran"]}}))
        # While the reviewer reads, the caller keeps editing the real working tree.
        self.fake_codex(codex_events("found a bug"), pre=f"echo caller >> {repo}/a.txt; echo new > {repo}/new.txt")
        (repo / "a.txt").write_text("a\nuncommitted\n")
        result = self.cli("run", "--agent", "codex", "--read-only", "--workdir", repo, "review")
        state = self.outcome(result)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(state["state"], "answered")
        for key in ("readOnlyViolation", "workspaceChanged", "warning", "error"):
            self.assertNotIn(key, state)
        worktree = Path(state["worktree"])
        self.assertEqual((worktree / "a.txt").read_text(), "a\nuncommitted\n")  # it read what the caller had
        self.assertTrue((worktree / "setup-ran").exists())  # Codex may run commands, so setup runs
        self.assertEqual((repo / "a.txt").read_text(), "a\nuncommitted\ncaller\n")
        self.fake_pi([answer("ok"), SETTLED])
        pi = self.outcome(self.cli("run", "--read-only", "--workdir", repo, "review"))
        self.assertFalse((Path(pi["worktree"]) / "setup-ran").exists())  # read-only Pi has no shell
        self.assertFalse(delegate.unmerged_worktree(Path(pi["dir"])))
        self.assertIn("removed", self.cli("clean", pi["run"]).stdout)
        self.assertFalse(Path(pi["worktree"]).exists())

    def test_nesting_rules(self):
        self.fake_pi([answer("ok"), SETTLED])
        self.fake_codex(codex_events("ok"))
        self.env["DELEGATE_AGENT"] = "pi"
        for agent in ("pi", "codex"):
            result = self.cli("start", "--agent", agent, "task")
            self.assertEqual(result.returncode, 2)
            self.assertIn("Pi run cannot delegate", result.stderr)
        self.env["DELEGATE_AGENT"] = "codex"
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
        self.env.update(PI_DELEGATE_AGENT="codex", PI_DELEGATE_PARENT_RUN=parent)  # pre-4.0 names still count
        self.assertEqual(self.outcome(self.cli("run", "--name", "helper", "task"))["state"], "answered")
        del self.env["PI_DELEGATE_AGENT"], self.env["PI_DELEGATE_PARENT_RUN"]
        self.cli("stop", parent)

    def test_images_are_attached_for_both_agents(self):
        image = self.work / "shot.png"
        image.write_bytes(b"png")
        self.fake_pi([answer("blue"), SETTLED])
        self.assertEqual(self.cli("run", "--image", "shot.png", "what color?").returncode, 0)
        self.assertTrue((self.work / "pi.log").read_text().split()[-1] == f"@{image}")
        self.fake_codex(codex_events("blue"))
        self.assertEqual(self.cli("run", "--agent", "codex", "--image", image, "what color?").returncode, 0)
        args = (self.work / "pi.log.codex").read_text().split()
        self.assertEqual(args[-2:], [f"--image={image}", "-"])
        result = self.cli("start", "--image", "missing.png", "task")
        self.assertEqual(result.returncode, 2)
        self.assertIn("image does not exist", result.stderr)

    def test_machine_wide_limits_refuse_extra_runs(self):
        self.fake_pi([answer("slow"), SETTLED], sleep=30)
        self.fake_codex(codex_events("ok"), pre="sleep 30")
        self.env.update(DELEGATE_MAX_ACTIVE="2", DELEGATE_MAX_CODEX="1")
        other = self.work / "other"
        other.mkdir()
        first = json.loads(self.cli("start", "--agent", "codex", "--name", "gpt", "task").stdout)["run"]
        result = self.cli("start", "--agent", "codex", "--workdir", other, "task")
        self.assertEqual(result.returncode, 2)
        self.assertIn("DELEGATE_MAX_CODEX=1", result.stderr)
        # Runs from another project (another runs root) share the machine's pool.
        self.env["DELEGATE_RUNS"] = str(self.work / "runs2")
        self.assertEqual(self.cli("start", "--read-only", "--name", "pi", "task").returncode, 0)
        result = self.cli("start", "--read-only", "task")
        self.assertEqual(result.returncode, 2)
        self.assertIn("DELEGATE_MAX_ACTIVE=2", result.stderr)
        self.assertIn(str(self.work / "runs"), result.stderr)
        self.env["DELEGATE_RUNS"] = str(self.work / "runs")
        self.cli("stop", first)
        self.assertEqual(self.cli("start", "--read-only", "--name", "freed", "task").returncode, 0)
        self.env["DELEGATE_MAX_ACTIVE"] = "0"  # 0 lifts the limit
        self.assertEqual(self.cli("start", "--read-only", "task").returncode, 0)
        self.env["DELEGATE_MAX_ACTIVE"] = "many"
        self.assertIn("non-negative integer", self.cli("start", "--read-only", "task").stderr)
        for root in ("runs", "runs2"):
            self.env["DELEGATE_RUNS"] = str(self.work / root)
            for line in self.cli("status").stdout.splitlines():
                self.cli("stop", json.loads(line)["dir"])

    def repo(self, files):
        repo = self.work / "repo"
        repo.mkdir()
        git = ["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t"]
        subprocess.run(git[:3] + ["init", "-q"], check=True)
        for name, text in files.items():
            (repo / name).parent.mkdir(parents=True, exist_ok=True)
            (repo / name).write_text(text)
        subprocess.run(git + ["add", "-A"], check=True)
        subprocess.run(git + ["commit", "-qm", "init"], check=True)
        return repo

    def test_changes_are_exact_and_listed_like_a_diffstat(self):
        repo = self.repo({"dirty.txt": "a\n", "gone.txt": "bye\n", "kept.txt": "same\n", ".gitignore": "cache/\n"})
        (repo / "dirty.txt").write_text("a\nmine\n")  # the caller's edit, before the run
        (repo / "big.bin").write_bytes(b"0" * 64)
        self.env["DELEGATE_SNAPSHOT_MAX_BYTES"] = "32"
        self.fake_pi([answer("done"), SETTLED], pre="echo theirs >> dirty.txt; rm gone.txt; echo n > new.txt; "
                     "echo x > kept.txt; echo same > kept.txt; mkdir cache; echo c > cache/f; echo 1 >> big.bin")
        result = self.cli("run", "--workdir", repo, "task")
        state = self.outcome(result)
        self.assertEqual(state["files"], ["big.bin", "dirty.txt", "gone.txt", "new.txt"])
        self.assertEqual({k: state["changes"][k] for k in ("files", "added", "deleted")},
                         {"files": 4, "added": 2, "deleted": 1})
        self.assertIn(" M dirty.txt  +1 -0", result.stdout)
        self.assertIn(" D gone.txt  +0 -1", result.stdout)
        self.assertIn(" M big.bin  large file", result.stdout)
        self.assertLess(result.stdout.index("===== changes"), result.stdout.index("===== result"))
        diff = self.cli("diff", "last").stdout
        self.assertIn("+theirs", diff)
        self.assertNotIn("+mine", diff)  # dirty before the run: not the agent's work
        self.assertEqual(subprocess.run(["git", "-C", str(repo), "diff", "--cached", "--name-only"],
                                        capture_output=True, text=True).stdout, "")  # real index untouched
        self.fake_pi([answer("looked"), SETTLED])
        self.assertIn("===== changes: ", self.cli("run", "--workdir", repo, "task").stdout.split("none")[0])

    def test_worktree_isolates_the_run_and_apply_merges_it_back(self):
        repo = self.repo({"a.txt": "1\n2\n3\n4\n5\n", "b.txt": "b\n", ".gitignore": ".env\ndata/\n"})
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {
            "copy": [".env"], "link": ["data"], "setup": ["test -f .env && touch setup-ran"]}}))
        (repo / ".env").write_text("SECRET=1\n")
        (repo / "data").mkdir()
        (repo / "b.txt").write_text("b\nuncommitted\n")
        self.fake_pi([answer("done"), SETTLED], pre='pwd > "$PI_LOG.cwd"; sed -i s/1/one/ a.txt; '
                     'grep -q uncommitted b.txt && echo seeded > seen.txt; test -L data && rm setup-ran')
        result = self.cli("run", "--worktree", "--accept", "test -f seen.txt", "--workdir", repo, "task")
        state = self.outcome(result)
        self.assertEqual(state["state"], "delivered", result.stdout + result.stderr)
        tree = Path(state["worktree"])
        self.assertEqual((self.work / "pi.log.cwd").read_text().strip(), str(tree))
        self.assertTrue(str(tree).startswith(str(self.work / "cache/delegate/worktrees")))
        self.assertEqual(state["files"], ["a.txt", "seen.txt"])  # link, copy and setup output are not its work
        self.assertEqual((repo / "a.txt").read_text(), "1\n2\n3\n4\n5\n")  # source untouched until apply
        self.assertIn(f"apply {state['run']}", state["next"])
        self.assertIn("apply", result.stdout)
        (repo / "a.txt").write_text("1\n2\n3\n4\nfive\n")  # the caller keeps working meanwhile
        self.assertEqual(self.cli("apply", "--dry-run").returncode, 0)
        self.assertFalse((repo / "seen.txt").exists())
        applied = self.cli("apply", state["run"])
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertEqual((repo / "a.txt").read_text(), "one\n2\n3\n4\nfive\n")
        self.assertEqual((repo / "seen.txt").read_text(), "seeded\n")
        self.assertNotIn("next", self.outcome(self.cli("status", state["run"])))  # merged: nothing left to do
        (repo / "a.txt").write_text("uno\n2\n3\n4\nfive\n")
        (repo / "seen.txt").unlink()
        refused = self.cli("apply", state["run"])
        self.assertEqual(refused.returncode, 1)
        self.assertIn("nothing applied", refused.stderr)
        self.assertFalse((repo / "seen.txt").exists())
        merged = self.cli("apply", "--merge", state["run"])
        self.assertEqual(merged.returncode, 1)
        self.assertIn("<<<<<<< current", (repo / "a.txt").read_text())
        self.assertTrue((repo / "seen.txt").exists())
        self.cli("clean", state["run"])
        self.assertFalse(tree.exists())
        self.assertNotIn(str(tree), subprocess.run(["git", "-C", str(repo), "worktree", "list"],
                                                   capture_output=True, text=True).stdout)

    def test_worktree_setup_failure_and_non_git_are_reported(self):
        repo = self.repo({"a.txt": "a\n"})
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"setup": "exit 7"}}))
        self.fake_pi([answer("never"), SETTLED])
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.assertEqual(state["state"], "failed")
        self.assertIn("exit 7", state["error"])
        self.assertFalse((self.work / "pi.log").exists())
        result = self.cli("start", "--worktree", "task")
        self.assertEqual(result.returncode, 2)
        self.assertIn("needs a git repository", result.stderr)

    def test_reply_continues_the_session_in_the_same_place(self):
        repo = self.repo({"a.txt": "a\n"})
        self.fake_pi([answer("first"), SETTLED], pre="echo more >> a.txt")
        first = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        self.fake_pi([answer("second"), SETTLED], pre="echo again >> a.txt")
        result = self.cli("reply", first["run"], "one more thing")
        second = self.outcome(result)
        self.assertEqual((second["state"], second["parent"], second["worktree"]),
                         ("delivered", first["run"], first["worktree"]))
        calls = [line.split() for line in (self.work / "pi.log").read_text().splitlines()]
        fork = Path(calls[1][calls[1].index("--fork") + 1])  # a copy of the first run's session
        self.assertEqual(fork.name, f"t_{calls[0][calls[0].index('--session-id') + 1]}.jsonl")
        self.assertEqual(fork.parent, Path(second["dir"]) / "fork")
        self.assertEqual((Path(second["dir"]) / "prompt.md").read_text(), "one more thing\n")
        self.assertIn("+again", self.cli("diff", second["run"]).stdout)
        self.assertNotIn("+more", self.cli("diff", second["run"]).stdout)
        self.assertIn("+more", self.cli("diff", "--total", second["run"]).stdout)
        self.fake_pi([answer("third"), SETTLED], pre="echo last >> a.txt")
        third = self.outcome(self.cli("reply", first["run"], "and finally"))  # the conversation's latest run
        self.assertEqual(third["parent"], second["run"])
        self.assertEqual(self.cli("apply", first["run"]).returncode, 0)
        self.assertEqual((repo / "a.txt").read_text(), "a\nmore\nagain\nlast\n")
        self.assertTrue(all((Path(r["dir"]) / ".applied").exists() for r in (first, second, third)))
        self.fake_codex(codex_events("ok"))
        codex = self.outcome(self.cli("run", "--agent", "codex", "--read-only", "look"))
        self.assertEqual(self.outcome(self.cli("reply", codex["run"], "and?"))["state"], "answered")
        call = (self.work / "pi.log.codex").read_text().splitlines()[-1]
        self.assertIn("exec fork t1 --json", call)
        self.assertNotIn(" -C ", call)

    def test_apply_handles_modes_large_files_and_unsafe_paths(self):
        repo = self.repo({"run.sh": "echo hi\n", "img.bin": "\0old", "sub/f.txt": "f\n"})
        self.env["DELEGATE_SNAPSHOT_MAX_BYTES"] = "32"
        self.fake_pi([answer("done"), SETTLED], pre="chmod +x run.sh; head -c 100 /dev/zero > big.dat; "
                     "printf '\\0new' > img.bin; echo g > sub/f.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.assertIn(" A big.dat  large file", self.cli("wait", state["run"]).stdout)
        (repo / "img.bin").write_bytes(b"\0mine")  # binary edited on both sides
        outside = self.work / "outside"
        outside.mkdir()
        (outside / "f.txt").write_text("f\n")
        shutil.rmtree(repo / "sub")
        (repo / "sub").symlink_to(outside)  # writing through it would leave the repository
        result = self.cli("apply", "--merge", state["run"])
        self.assertEqual(result.returncode, 1)
        self.assertIn("skipped          img.bin", result.stdout)
        self.assertIn("skipped          sub/f.txt", result.stdout)
        self.assertEqual((outside / "f.txt").read_text(), "f\n")
        self.assertTrue(os.access(repo / "run.sh", os.X_OK))
        self.assertEqual((repo / "big.dat").stat().st_size, 100)
        self.assertFalse((Path(state["dir"]) / ".applied").exists())  # skipped files keep the worktree listed

    def test_reply_survives_cleaning_earlier_rounds(self):
        self.fake_pi([answer("one"), SETTLED])
        first = self.outcome(self.cli("run", "--read-only", "task"))
        second = self.outcome(self.cli("reply", first["run"], "more"))
        self.cli("clean", first["run"])
        self.assertEqual(self.outcome(self.cli("reply", second["run"], "again"))["state"], "answered")
        calls = [line.split() for line in (self.work / "pi.log").read_text().splitlines()]
        self.assertTrue(all("--fork" in call for call in calls[1:]))

    def test_agent_that_outlives_its_supervisor_blocks_clean_until_stopped(self):
        self.fake_pi([answer("slow"), SETTLED], sleep=30)
        run = Path(json.loads(self.cli("start", "--name", "orphan", "task").stdout)["dir"])
        for _ in range(50):
            if (run / "agent.pid").is_file():
                break
            time.sleep(0.1)
        os.kill(int((run / "pid").read_text()), signal.SIGKILL)
        agent = int((run / "agent.pid").read_text())
        self.assertIn("outlived the supervisor", self.cli("clean", "orphan").stderr)
        self.assertEqual(self.cli("start", "--name", "rival", "task").returncode, 2)  # still writing here
        self.cli("stop", "orphan")
        time.sleep(0.3)
        self.assertFalse(delegate_pid_alive(agent))
        self.assertIn("removed", self.cli("clean", "orphan").stdout)

    def test_snapshot_survives_undecodable_names_and_sees_submodules(self):
        repo = self.repo({"a.txt": "a\n"})
        (repo / os.fsdecode(b"caf\xe9.txt")).write_text("x")
        self.fake_codex(codex_events("looked"), pre="echo oops > stray.txt")
        state = self.outcome(self.cli("run", "--agent", "codex", "--read-only", "--workdir", repo, "review"))
        self.assertEqual(state["readOnlyViolation"], ["stray.txt"])
        sub = self.work / "lib"
        subprocess.run(["git", "init", "-q", str(sub)], check=True)
        (sub / "lib.txt").write_text("l\n")
        git = ["-c", "user.name=t", "-c", "user.email=t@t", "-c", "protocol.file.allow=always"]
        subprocess.run(["git", "-C", str(sub), *git, "add", "-A"], check=True)
        subprocess.run(["git", "-C", str(sub), *git, "commit", "-qm", "lib"], check=True)
        subprocess.run(["git", "-C", str(repo), *git, "submodule", "add", "-q", str(sub), "vendor"], check=True,
                       capture_output=True)
        subprocess.run(["git", "-C", str(repo), *git, "commit", "-qm", "sub"], check=True)
        self.fake_codex(codex_events("looked"), pre="echo edit >> vendor/lib.txt")
        # In place: a new worktree leaves submodules empty.
        state = self.outcome(self.cli("run", "--agent", "codex", "--read-only", "--in-place", "--workdir", repo,
                                      "review"))
        self.assertEqual(state["workspaceChanged"], ["vendor"])
        self.assertIn(" M vendor  submodule contents", self.cli("wait", state["run"]).stdout)

    def test_worktree_in_a_repository_without_commits(self):
        repo = self.work / "fresh"
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        (repo / "draft.txt").write_text("draft\n")
        self.fake_pi([answer("done"), SETTLED], pre="grep -q draft draft.txt && echo more >> draft.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.assertEqual((state["state"], state["files"]), ("answered", ["draft.txt"]))
        self.assertEqual(self.cli("apply", state["run"]).returncode, 0)
        self.assertEqual((repo / "draft.txt").read_text(), "draft\nmore\n")

    def test_reply_tells_the_agent_when_the_acceptance_command_changes(self):
        self.fake_pi([answer("ok"), SETTLED])
        first = self.outcome(self.cli("run", "--accept", "true", "--prompt", "fix it"))
        second = self.outcome(self.cli("reply", first["run"], "--accept", "", "--prompt", "now tidy up"))
        self.assertNotIn("accept", second)
        self.assertIn("no longer applies", (Path(second["dir"]) / "prompt.md").read_text())
        third = self.outcome(self.cli("reply", second["run"], "--prompt", "and more"))
        self.assertEqual((Path(third["dir"]) / "prompt.md").read_text(), "and more\n")

    def test_reply_keeps_the_session_when_its_parent_is_pruned(self):
        self.fake_pi([answer("ok"), SETTLED])
        first = self.outcome(self.cli("run", "--read-only", "task"))
        os.utime(Path(first["dir"]) / "exit_code", (1, 1))
        second = self.outcome(self.cli("reply", first["run"], "more"))
        self.assertFalse(Path(first["dir"]).exists())  # pruned by the reply's own start
        self.assertTrue(any((Path(second["dir"]) / "session").glob("t_*.jsonl")))
        self.assertEqual(self.outcome(self.cli("reply", second["run"], "again"))["state"], "answered")

    def test_malformed_reply_is_retried_from_the_parent_and_skipped(self):
        self.fake_pi([answer("ok"), SETTLED])
        first = self.outcome(self.cli("run", "--read-only", "task"))
        self.fake_pi([answer(LEAKED), SETTLED])
        broken = self.outcome(self.cli("reply", first["run"], "more"))
        self.assertEqual((broken["state"], broken["attempts"]), ("malformed", 2))
        calls = [line.split() for line in (self.work / "pi.log").read_text().splitlines()]
        forks = {call[call.index("--fork") + 1] for call in calls[1:]}
        self.assertEqual(len(forks), 1)  # the rerun started again from the parent's conversation
        self.fake_pi([answer("better"), SETTLED])
        retry = self.outcome(self.cli("reply", first["run"], "more, again"))
        self.assertEqual((retry["state"], retry["parent"]), ("answered", first["run"]))
        repo = self.repo({"a.txt": "a\n"})
        self.fake_pi([answer("ok"), SETTLED], pre="echo b >> a.txt")
        root = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.fake_pi([answer(LEAKED), SETTLED])
        dead_end = self.outcome(self.cli("reply", root["run"], "more"))
        self.assertEqual(self.cli("apply", root["run"]).returncode, 0)
        self.assertTrue((Path(dead_end["dir"]) / ".applied").exists())
        self.assertNotIn("never applied", self.cli("clean", dead_end["run"]).stdout)

    def test_fresh_reply_and_apply_take_the_worktree_as_it_is(self):
        repo = self.repo({"a.txt": "a\n"})
        self.fake_pi([answer("first"), SETTLED], pre="echo one >> a.txt")
        first = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        self.fake_pi([answer("second"), SETTLED], pre="echo two >> a.txt")
        second = self.outcome(self.cli("reply", "--fresh", first["run"], "--prompt", "standalone task"))
        call = (self.work / "pi.log").read_text().splitlines()[-1].split()
        self.assertIn("--session-id", call)
        self.assertNotIn("--fork", call)
        self.assertIn("Definition of done", (Path(second["dir"]) / "prompt.md").read_text())
        self.assertEqual((second["parent"], second["worktree"]), (first["run"], first["worktree"]))
        with open(Path(first["worktree"]) / "a.txt", "a") as touch_up:  # the caller finishes it by hand
            touch_up.write("three\n")
        self.assertEqual(self.cli("apply", first["run"]).returncode, 0)
        self.assertEqual((repo / "a.txt").read_text(), "a\none\ntwo\nthree\n")


def delegate_pid_alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return Path(f"/proc/{pid}/stat").read_text().split()[2] != "Z"


if __name__ == "__main__":
    unittest.main()
