import json
import importlib.util
import os
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
# Black-box conformance suite (docs/delegate-spec.md): every test drives the CLI as a subprocess, so any
# implementation can be checked by pointing DELEGATE_BIN at its executable.
# By default the suite builds crates/delegate and tests that, so a passing run always covers the current
# source; the installed skills/delegate/bin/delegate is only used when there is no source or no cargo.


def source_binary():
    crate = ROOT / "crates/delegate"
    if not (crate / "Cargo.toml").is_file() or not shutil.which("cargo"):
        return None
    built = subprocess.run(["cargo", "build", "--locked", "--quiet", "--message-format=json-render-diagnostics"],
                           cwd=crate, stdout=subprocess.PIPE, text=True)
    if built.returncode:
        raise RuntimeError("cargo build of crates/delegate failed")
    for line in built.stdout.splitlines():
        event = json.loads(line)
        if event.get("reason") == "compiler-artifact" and event.get("executable") \
                and event["target"]["name"] == "delegate":
            return Path(event["executable"])
    raise RuntimeError("cargo build of crates/delegate produced no delegate executable")


DELEGATE = Path(os.environ.get("DELEGATE_BIN") or source_binary() or ROOT / "skills/delegate/bin/delegate").resolve()
if not os.environ.get("DELEGATE_BIN") and DELEGATE == (ROOT / "skills/delegate/bin/delegate").resolve():
    print(f"note: no cargo or crates/delegate; testing the installed {DELEGATE}", file=sys.stderr)
if not os.access(DELEGATE, os.X_OK):
    raise RuntimeError(f"{DELEGATE} is missing: install cargo, set DELEGATE_BIN, or run scripts/fetch-binary.sh delegate")


def process_gone(pid_file):
    try:
        pid = int(Path(pid_file).read_text())
    except (OSError, ValueError):
        return True
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0] == "Z"
    except OSError:
        return True


def answer(text, stop="stop"):
    content = [{"type": "text", "text": text}] if text is not None else []
    return {"type": "message_end", "message": {"role": "assistant", "stopReason": stop, "model": "fake-model",
                                               "usage": {"input": 10, "output": 5, "cacheRead": 0},
                                               "content": content}}


# Session variables delegate reads as the caller id; host sessions must not leak into tests.
CALLER_ENV = ("DELEGATE_CALLER", "CLAUDE_CODE_SESSION_ID", "CODEX_THREAD_ID", "PI_SESSION_ID")
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
        self.env = {k: v for k, v in os.environ.items()
                    if not k.startswith(("DELEGATE_", "PI_DELEGATE_")) and k not in CALLER_ENV}
        self.env.update(PATH=f"{self.bin}:{os.environ['PATH']}", DELEGATE_POLL="0.1",
                        DELEGATE_RUNS=str(self.work / "runs"), XDG_STATE_HOME=str(self.work / "state"),
                        XDG_CACHE_HOME=str(self.work / "cache"),
                        PI_LOG=str(self.work / "pi.log"),
                        # Both tiers map to the fake Pi unless a test says otherwise (see the tier tests).
                        DELEGATE_STRONG_AGENT="pi")

    def fake_pi(self, *attempts, pre="", sleep=0, code=0):
        """Each attempt is a list of events; later calls reuse the last attempt."""
        lines = ["#!/bin/sh", '[ "$1" = --version ] && { echo "fake-pi 1.0"; exit 0; }',
                 "cat > /dev/null", 'echo "$$ $PI_DELEGATE_ACTIVE $*" >> "$PI_LOG"',
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
        lines = ["#!/bin/sh", '[ "$1" = --version ] && { echo "fake-codex 1.0"; exit 0; }',
                 'cat > "$PI_LOG.codex-prompt"',
                 'echo "$$ ${DELEGATE_AGENT:-} ${PI_DELEGATE_ACTIVE:-} $*" >> "$PI_LOG.codex"', pre,
                 "printf '%s\\n' " + " ".join(shlex.quote(json.dumps(e)) for e in events)]
        codex = self.bin / "codex"
        codex.write_text("\n".join(lines) + "\n")
        codex.chmod(0o755)

    def cli(self, *args, stdin=None, cwd=None, timeout=30, human=False):
        if not human and args and args[0] in {"start", "run", "reply", "wait", "status", "list", "stop"} \
                and "--json" not in args:
            args = (args[0], "--json", *args[1:])
        return subprocess.run([str(DELEGATE), *map(str, args)], cwd=cwd or self.work, env=self.env,
                              capture_output=True, text=True, timeout=timeout,
                              **({"input": stdin} if stdin is not None else {"stdin": subprocess.DEVNULL}))

    def outcome(self, result):
        lines = [line for line in result.stdout.splitlines() if line.startswith('{"run"')]
        self.assertTrue(lines, result.stdout + result.stderr)
        return json.loads(lines[0])

    def test_evidence_records_failure_without_rejecting_and_inherits(self):
        self.fake_pi([answer("ok"), SETTLED])
        repo = self.repo({"a.txt": "old\n"})
        (repo / ".delegate.json").write_text(json.dumps({"evidence": "echo proof; exit 9"}))
        result = self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task")
        state = self.outcome(result)
        self.assertEqual((result.returncode, state["state"], state["evidence"]["exit"]), (0, "delivered", 9))
        self.assertFalse(state["evidence"]["timedOut"])
        self.assertIn("proof", state["evidence"]["tail"])
        self.assertTrue(Path(state["evidence"]["log"]).is_file())
        self.assertEqual(json.loads((Path(state["dir"]) / "meta.json").read_text())["evidence"], state["evidence"])
        self.assertIn("证据 失败", self.cli("status", state["run"], human=True).stdout)
        reply = self.outcome(self.cli("reply", "--wait", state["run"], "more"))
        self.assertEqual(reply["evidence"]["exit"], 9)
        disabled = self.outcome(self.cli("reply", "--wait", reply["run"], "--no-evidence", "more"))
        self.assertNotIn("evidence", disabled)
        read = self.outcome(self.cli("run", "--read-only", "--workdir", repo, "task"))
        self.assertNotIn("evidence", read)
        off = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--no-evidence", "task"))
        self.assertNotIn("evidence", off)

    def test_evidence_timeout_and_success_use_original_path_and_environment(self):
        self.fake_pi([answer("ok"), SETTLED])
        repo = self.repo({"a.txt": "old\n"})
        (self.bin / "proof").write_text('#!/bin/sh\necho "$PROOF_ENV:$PWD"\n')
        (self.bin / "proof").chmod(0o755)
        (repo / ".delegate.json").write_text(json.dumps({"env": {"PROOF_ENV": "injected"},
            "agentDeny": [{"argv": ["proof"], "hint": "no full proof"}]}))
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--evidence", "proof", "task"))
        self.assertEqual((state["state"], state["evidence"]["exit"]), ("answered", 0))
        self.assertIn("injected:" + state["worktree"], state["evidence"]["tail"])
        self.assertIn("证据 通过", self.cli("status", state["run"], human=True).stdout)
        result = self.cli("reply", "--wait", state["run"], "--evidence", "echo slow; sleep 10",
                          "--evidence-timeout", "0.1s", "more")
        timed = self.outcome(result)
        self.assertEqual((result.returncode, timed["state"], timed["evidence"]["exit"]), (0, "answered", 124))
        self.assertTrue(timed["evidence"]["timedOut"])
        self.assertGreaterEqual(timed["evidence"]["seconds"], 0.1)
        self.assertIn("证据 超时", self.cli("status", timed["run"], human=True).stdout)

    def test_evidence_log_error_is_only_diagnostic(self):
        self.fake_pi([answer("ok"), SETTLED], pre='mkdir "$DELEGATE_RUN_DIR/evidence.log"')
        result = self.cli("run", "--evidence", "false", "task")
        state = self.outcome(result)
        self.assertEqual((result.returncode, state["state"]), (0, "answered"))
        self.assertIsNone(state["evidence"]["exit"])
        self.assertFalse(state["evidence"]["timedOut"])
        self.assertIn("directory", state["evidence"]["tail"])

    def test_evidence_queue_excludes_timeout_and_reclaims_children(self):
        self.fake_pi([answer("ok"), SETTLED])
        self.hold_lane(2)
        state = self.outcome(self.cli("run", "--evidence",
            "sleep 30 & echo $! > evidence-bg.pid; echo proof", "--evidence-timeout", "0.5s", "task"))
        self.assertEqual((state["state"], state["evidence"]["exit"]), ("answered", 0))
        self.assertGreaterEqual(state["queuedSeconds"], 1)
        self.assertFalse(state["evidence"]["timedOut"])
        self.assertGreaterEqual(state["cleanup"]["terminated"], 1)
        pid = int((self.work / "evidence-bg.pid").read_text())
        self.until(lambda: not Path(f"/proc/{pid}").exists(), "the evidence child to go")
        meta = json.loads((Path(state["dir"]) / "meta.json").read_text())
        self.assertEqual(meta["evidenceTimeoutSeconds"], 0.5)
        inherited = self.outcome(self.cli("reply", "--wait", state["run"], "--evidence", "sleep 10", "more"))
        self.assertTrue(inherited["evidence"]["timedOut"])
        self.assertEqual(inherited["state"], "answered")

    def test_evidence_skipped_on_rejection_and_user_default_overridden(self):
        self.fake_pi([answer("ok"), SETTLED])
        self.env["XDG_CONFIG_HOME"] = str(self.work / "config")
        config = self.work / "config/delegate/config.json"
        config.parent.mkdir(parents=True)
        config.write_text(json.dumps({"evidence": "echo user-proof"}))
        repo = self.repo({"a.txt": "old\n"})
        user = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.assertIn("user-proof", user["evidence"]["tail"])
        (repo / ".delegate.json").write_text(json.dumps({"evidence": "echo repo-proof"}))
        override = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--evidence", "echo cli-proof", "task"))
        self.assertIn("cli-proof", override["evidence"]["tail"])
        rejected = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "false", "task"))
        self.assertEqual(rejected["state"], "rejected")
        self.assertNotIn("evidence", rejected)
        self.fake_pi([answer("ok"), SETTLED], pre="echo changed > a.txt")
        protected = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--protect", "a.txt", "task"))
        self.assertEqual(protected["state"], "rejected")
        self.assertNotIn("evidence", protected)
        config.write_text(json.dumps({"evidence": 1}))
        bad = self.cli("start", "task")
        self.assertEqual(bad.returncode, 2)
        self.assertIn("evidence", bad.stderr)

    def test_help_and_version_are_there_for_humans(self):
        top = self.cli("--help")
        self.assertEqual(top.returncode, 0, top.stderr)
        for command in ("start", "run", "reply", "wait", "status", "result", "diff", "apply", "lane", "stop", "clean"):
            self.assertIn(command, top.stdout)
        for command, options in (("start", ("--tier", "--agent", "--read-only", "--accept", "--worktree", "--in-place")),
                                 ("reply", ("--fresh", "--accept", "--wait")), ("wait", ("--max", "--no-result")),
                                 ("lane", ("--label",)), ("apply", ("--merge", "--dry-run"))):
            result = self.cli(command, "--help")
            self.assertEqual(result.returncode, 0, (command, result.stderr))
            for option in options:
                self.assertIn(option, result.stdout, command)
        self.assertEqual(self.cli("-h").returncode, 0)
        self.assertIn("default: 2", self.cli("lane", "--help").stdout)
        wait_help = self.cli("wait", "--help").stdout
        self.assertIn("regardless of caller", " ".join(wait_help.split()))
        self.assertNotIn("(the default)", wait_help)
        skill = (ROOT / "skills/delegate/SKILL.md").read_text()
        version = re.search(r'^\s*version:\s*"?([\d.]+)', skill, re.M).group(1)
        result = self.cli("--version")
        self.assertEqual((result.returncode, result.stdout.strip()), (0, f"delegate {version}"))

    def test_outcome_line_leads_with_what_matters(self):
        self.fake_pi([answer("ok"), SETTLED])
        line = next(l for l in self.cli("run", "--read-only", "task").stdout.splitlines() if l.startswith('{"run"'))
        self.assertEqual(list(json.loads(line))[:3], ["run", "name", "state"])  # read at a glance, not alphabetically

    def test_default_output_is_short_relative_and_keeps_report(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi([answer("report body"), SETTLED],
                     pre="mkdir -p crates/protocol; echo new > crates/protocol/new; echo new > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "compact",
                                     "--accept", "true", "task"))
        for command in ("status", "wait"):
            out = self.cli(command, "compact", human=True)
            self.assertEqual(out.returncode, 0, out.stderr)
            line = out.stdout.splitlines()[0]
            self.assertTrue(line.startswith("compact · 已交付"), line)
            for text in ("pi/strong", "验收通过", "改 2 个文件 +2/-1", "crates/protocol 1", "delegate diff"):
                self.assertIn(text, line)
            self.assertNotIn(str(self.work), line)
            self.assertNotIn(state["worktree"], line)
            self.assertNotIn("===== changes", out.stdout)
            if command == "wait":
                self.assertIn("report body", out.stdout)
        complete = self.outcome(self.cli("status", "compact"))
        self.assertEqual(complete["changes"], state["changes"])
        self.assertEqual(complete["files"], ["a.txt", "crates/protocol/new"])
        self.assertIn("shape", complete)

    def test_running_default_output_has_relative_changes_and_truncated_command(self):
        repo = self.repo({"a.txt": "old\n"})
        gate = self.work / "gate"
        command = "cargo test " + "x" * 120
        event = {"type": "tool_execution_start", "toolName": "bash", "args": {"command": command}}
        self.fake_pi([answer("done"), SETTLED],
                     pre=f"echo new > a.txt; printf '%s\\n' {shlex.quote(json.dumps(event))}; "
                         f"while [ ! -f {gate} ]; do sleep .02; done")
        state = self.outcome(self.cli("start", "--worktree", "--workdir", repo, "--name", "running", "task"))
        self.addCleanup(self.cli, "stop", state["run"])
        run = Path(state["dir"])
        self.until(lambda: "cargo test" in (run / "events.jsonl").read_text(), "command event")
        out = self.cli("status", "running", human=True)
        self.assertEqual(out.returncode, 0, out.stderr)
        for field in ("running · 运行中", "pi/strong", "改 1 个文件 +1/-1", "最近：cargo test", "下一步："):
            self.assertIn(field, out.stdout)
        recent = out.stdout.split("最近：", 1)[1].split(" · ", 1)[0]
        self.assertLessEqual(len(recent), 100)
        self.assertTrue(recent.endswith("..."))
        self.assertNotIn(str(self.work), out.stdout)
        gate.touch()
        self.assertEqual(self.cli("wait", state["run"]).returncode, 0)

    def test_agent_executable_and_version_are_recorded_in_meta_and_status(self):
        self.fake_pi([answer("ok"), SETTLED])
        state = self.outcome(self.cli("run", "--agent", "pi", "task"))
        meta = json.loads((Path(state["dir"]) / "meta.json").read_text())
        self.assertEqual(meta["agentBin"], str((self.bin / "pi").resolve()))
        self.assertTrue(Path(meta["agentBin"]).is_absolute())
        self.assertEqual(meta["agentVersion"], "fake-pi 1.0")
        self.assertEqual(state["agentBin"], meta["agentBin"])
        self.assertEqual(self.outcome(self.cli("status", state["run"]))["agentBin"], meta["agentBin"])

    def test_supervisor_startup_timeout_reaps_process_before_crashed_result(self):
        self.fake_pi([answer("late"), SETTLED])
        self.env.update(DELEGATE_TEST_SUPERVISOR_DELAY="2s", DELEGATE_TEST_STARTUP_TIMEOUT="0.2s")
        result = self.cli("start", "--agent", "pi", "task")
        self.assertNotEqual(result.returncode, 0)
        run, = (self.work / "runs").glob("*/meta.json")
        run = run.parent
        self.assertEqual(self.outcome(self.cli("status", str(run)))["state"], "crashed")
        self.assertIn("finishedAt", self.outcome(self.cli("status", str(run))))
        self.assertTrue(process_gone(run / "startup.pid"))
        before = {p.name: p.stat().st_mtime_ns for p in run.iterdir()}
        time.sleep(2.1)
        self.assertEqual(before, {p.name: p.stat().st_mtime_ns for p in run.iterdir()})
        self.assertFalse((run / "pid").exists())

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

    def until(self, condition, what, timeout=10):
        deadline = time.time() + timeout
        while not condition():
            if time.time() > deadline:
                self.fail(f"timed out waiting for {what}")
            time.sleep(0.05)

    def hold_lane(self, seconds):
        """Occupy the heavy lane from another process, as a caller's own check would."""
        self.env["DELEGATE_MAX_HEAVY"] = "1"
        holder = subprocess.Popen([str(DELEGATE), "lane", "--label", "caller check", f"sleep {seconds}"], env=self.env)
        self.addCleanup(holder.kill)
        self.until(lambda: "caller check" in self.cli("lane").stdout, "the lane holder")
        return holder

    def test_lane_runs_heavy_commands_one_at_a_time(self):
        self.env["DELEGATE_MAX_HEAVY"] = "1"
        # mkdir fails if another command still holds the directory: overlap would show as a failure.
        command = "mkdir held && sleep 0.6 && rmdir held"
        runs = [subprocess.Popen([str(DELEGATE), "lane", command], cwd=self.work, env=self.env,
                                 stderr=subprocess.PIPE, text=True) for _ in range(3)]
        self.until(lambda: self.cli("lane").stdout.count("\n") == 3, "three commands in the lane")
        listing = self.cli("lane").stdout
        self.assertEqual((listing.count("running"), listing.count("queued")), (1, 2), listing)
        self.assertEqual([r.wait(timeout=20) for r in runs], [0, 0, 0])
        self.assertTrue(any("queued behind" in r.stderr.read() for r in runs))
        self.assertEqual(self.cli("lane").stdout, "")
        # Inside a slot, a nested lane runs at once instead of waiting for itself; exit codes pass through.
        self.assertEqual(self.cli("lane", f"{DELEGATE} lane true", timeout=10).returncode, 0)
        self.assertEqual(self.cli("lane", "--", "sh", "-c", "exit 3").returncode, 3)
        self.env["DELEGATE_MAX_HEAVY"] = "x"
        self.assertEqual(self.cli("lane", "true").returncode, 2)

    def test_acceptance_queues_in_the_lane_and_its_leftovers_are_killed(self):
        self.fake_pi([answer("done"), SETTLED])
        self.hold_lane(2)
        # The accept timeout starts once the lane lets it run, so queueing longer than it is fine.
        state = self.outcome(self.cli("run", "--accept", "sleep 30 & echo $! > bg.pid; true", "--accept-timeout", "1",
                                      "task"))
        self.assertEqual(state["state"], "delivered")
        self.assertGreaterEqual(state["accept"]["queuedSeconds"], 1)
        self.assertGreaterEqual(state["cleanup"]["terminated"], 1)
        self.assertIn(f"{DELEGATE} lane 'sleep 30 & echo $! > bg.pid; true'",
                      (Path(state["dir"]) / "prompt.md").read_text())
        pid = int((self.work / "bg.pid").read_text())
        self.until(lambda: not Path(f"/proc/{pid}").exists(), "the background leftover to go")  # killed with its group
        state = self.outcome(self.cli("run", "--accept", "sleep 30 & echo $! > bg.pid; wait", "--accept-timeout", "1",
                                      "task"))
        self.assertEqual((state["state"], state["accept"]["exitCode"]), ("rejected", 124))
        self.assertGreaterEqual(state["cleanup"]["terminated"], 1)
        self.assertIn("[accept timed out]", state["accept"]["tail"])
        pid = int((self.work / "bg.pid").read_text())
        self.until(lambda: not Path(f"/proc/{pid}").exists(), "the timed-out check's group to go")

    def test_stop_while_queued_never_accepts_and_lane_held_does_not_leak(self):
        self.fake_pi([answer("done"), SETTLED])
        holder = self.hold_lane(30)
        run = self.outcome(self.cli("start", "--accept", "touch accepted", "task"))
        self.until(lambda: f"accept {run['run']}" in self.cli("lane").stdout, "the acceptance to queue")
        stopped = self.outcome(self.cli("stop", run["run"]))
        self.assertEqual(stopped["state"], "stopped")
        self.assertIn("finishedAt", stopped)
        holder.kill()
        self.until(lambda: process_gone(Path(run["dir"]) / "pid"), "the supervisor to exit")
        self.assertFalse((self.work / "accepted").exists())
        self.assertEqual(self.cli("lane").stdout, "")
        # Started from inside a lane command, a run still queues: the slot it was started in is not its own.
        self.hold_lane(2)
        self.env["DELEGATE_LANE_HELD"] = "1"
        state = self.outcome(self.cli("run", "--accept", "true", "task"))
        self.assertGreaterEqual(state["accept"]["queuedSeconds"], 1)

    def test_terminating_lane_ends_its_command_before_the_slot_is_free(self):
        lane = subprocess.Popen([str(DELEGATE), "lane", "sleep 30 & echo $! > bg.pid; wait"], cwd=self.work,
                                env=self.env)
        self.until(lambda: (self.work / "bg.pid").is_file() and (self.work / "bg.pid").read_text().strip(),
                   "the command to start")
        pid = int((self.work / "bg.pid").read_text())
        lane.terminate()
        self.assertEqual(lane.wait(timeout=10), 128 + signal.SIGTERM)
        self.assertFalse(Path(f"/proc/{pid}").exists() and "Z" not in Path(f"/proc/{pid}/stat").read_text().split()[2])
        self.assertEqual(self.cli("lane").stdout, "")

    def test_timeout_grace_while_at_work_and_queue_time_is_free(self):
        start = {"type": "tool_execution_start", "toolName": "bash", "args": {"command": "make check"}}
        end = {"type": "tool_execution_end", "toolName": "bash"}
        lines = ["#!/bin/sh", "cat > /dev/null", f"printf '%s\\n' {shlex.quote(json.dumps(start))}", "sleep 1.8",
                 f"printf '%s\\n' {' '.join(shlex.quote(json.dumps(e)) for e in (end, answer('checked'), SETTLED))}"]
        (self.bin / "pi").write_text("\n".join(lines) + "\n")
        (self.bin / "pi").chmod(0o755)
        # A command in progress at the timeout earns up to DELEGATE_TIMEOUT_GRACE percent more.
        self.env["DELEGATE_TIMEOUT_GRACE"] = "100"
        state = self.outcome(self.cli("run", "--timeout", "1", "task"))
        self.assertEqual(state["state"], "answered")
        self.assertGreaterEqual(state["graceSeconds"], 0)
        self.env["DELEGATE_TIMEOUT_GRACE"] = "0"
        self.assertEqual(self.outcome(self.cli("run", "--timeout", "1", "task"))["state"], "timeout")
        # Waiting in the lane for its own check does not use up the agent's time.
        self.fake_pi([answer("checked"), SETTLED], pre=f"{DELEGATE} lane true")
        self.hold_lane(2.5)
        state = self.outcome(self.cli("run", "--timeout", "1", "task"))
        self.assertEqual(state["state"], "answered")
        self.assertGreaterEqual(state["queuedSeconds"], 1)

    def test_low_memory_refuses_to_start(self):
        self.fake_pi([answer("ok"), SETTLED])
        self.env["DELEGATE_MIN_AVAILABLE_MB"] = str(1 << 40)
        result = self.cli("start", "--read-only", "task")
        self.assertEqual(result.returncode, 2)
        self.assertIn("MB of memory available", result.stderr)
        self.env["DELEGATE_MIN_AVAILABLE_MB"] = "0"
        self.assertEqual(self.outcome(self.cli("run", "--read-only", "task"))["state"], "answered")

    def test_config_env_reaches_agent_acceptance_and_setup(self):
        repo = self.repo({"a.txt": "a\n", ".gitignore": "setup.env\n"})
        (repo / ".delegate.json").write_text(json.dumps({"env": {"CUDA_VISIBLE_DEVICES": ""}, "worktree": {
            "setup": ['echo "[${CUDA_VISIBLE_DEVICES-unset}]" > setup.env']}}))
        self.env["CUDA_VISIBLE_DEVICES"] = "0"
        self.fake_pi([answer("done"), SETTLED], pre='echo "[${CUDA_VISIBLE_DEVICES-unset}]" > agent.env')
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo,
                                      "--accept", 'test "[${CUDA_VISIBLE_DEVICES-unset}]" = "[]"', "task"))
        self.assertEqual(state["state"], "delivered")
        tree = Path(state["worktree"])
        self.assertEqual(((tree / "agent.env").read_text(), (tree / "setup.env").read_text()), ("[]\n", "[]\n"))
        self.fake_pi([answer("again"), SETTLED], pre='echo "[${CUDA_VISIBLE_DEVICES-unset}]" > reply.env')
        self.outcome(self.cli("reply", "--wait", state["run"], "more"))
        self.assertEqual((tree / "reply.env").read_text(), "[]\n")  # a reply keeps its conversation's env
        (repo / ".delegate.json").write_text(json.dumps({"env": {"X": 1}}))
        self.assertIn("env must map names to strings", self.cli("run", "--workdir", repo, "task").stderr)

    def detached_server(self, label):
        script = self.work / "server.py"
        script.write_text("import os, socket, sys, time\n"
                          "sock = socket.socket()\n"
                          "sock.bind(('127.0.0.1', 0))\n"
                          "sock.listen()\n"
                          "open(sys.argv[1] + '.pid', 'w').write(str(os.getpid()))\n"
                          "open(sys.argv[1] + '.port', 'w').write(str(sock.getsockname()[1]))\n"
                          "time.sleep(60)\n")
        prefix = self.work / label
        self.addCleanup(lambda: os.kill(int(Path(str(prefix) + ".pid").read_text()), signal.SIGKILL)
                        if Path(str(prefix) + ".pid").exists() and not process_gone(str(prefix) + ".pid") else None)
        return f"setsid python3 {shlex.quote(str(script))} {shlex.quote(str(prefix))} >/dev/null 2>&1 & sleep .3"

    def test_detached_agent_server_is_reported_and_terminated(self):
        command = self.detached_server("agent-server")
        self.fake_pi([answer("preview at localhost"), SETTLED], pre=command)
        result = self.cli("run", "task")
        state = self.outcome(result)
        port = int((self.work / "agent-server.port").read_text())
        self.assertGreaterEqual(state["cleanup"]["terminated"], 1)
        self.assertIn(port, state["cleanup"]["ports"])
        self.assertTrue(any("server.py" in command for command in state["cleanup"]["commands"]))
        self.assertIn(f"端口 {port}", result.stdout)
        self.assertTrue(process_gone(self.work / "agent-server.pid"))

    def test_timeout_and_stop_clean_detached_processes(self):
        self.env["DELEGATE_TIMEOUT_GRACE"] = "0"
        timeout_command = self.detached_server("timeout-server")
        self.fake_pi([answer("too late"), SETTLED], pre=timeout_command, sleep=30)
        timed = self.outcome(self.cli("run", "--timeout", "1", "task"))
        self.assertEqual(timed["state"], "timeout")
        self.assertGreaterEqual(timed["cleanup"]["terminated"], 1)
        self.assertTrue(process_gone(self.work / "timeout-server.pid"))

        stop_command = self.detached_server("stop-server")
        self.fake_pi([answer("too late"), SETTLED], pre=stop_command, sleep=30)
        started = self.outcome(self.cli("start", "--name", "stopper", "task"))
        self.until(lambda: (self.work / "stop-server.port").exists(), "detached stop server")
        stopped = self.outcome(self.cli("stop", started["run"]))
        self.assertEqual(stopped["state"], "stopped")
        self.assertGreaterEqual(stopped["cleanup"]["terminated"], 1)
        self.assertTrue(process_gone(self.work / "stop-server.pid"))

    def test_setup_and_accept_detached_processes_are_cleaned(self):
        repo = self.repo({"a.txt": "a\n"})
        setup = self.detached_server("setup-server")
        accept = self.detached_server("accept-server")
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"setup": [setup + '; test -n "$DELEGATE_RUN_DIR"']}}))
        self.fake_pi([answer("done"), SETTLED])
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo,
                                      "--accept", accept + '; test -n "$DELEGATE_RUN_DIR"', "task"))
        self.assertEqual(state["state"], "delivered")
        self.assertGreaterEqual(state["cleanup"]["terminated"], 2)
        for label in ("setup-server", "accept-server"):
            self.assertIn(int((self.work / f"{label}.port").read_text()), state["cleanup"]["ports"])
            self.assertTrue(process_gone(self.work / f"{label}.pid"))

    def test_cgroup_cleans_untagged_detached_agent_and_acceptance(self):
        if not shutil.which("systemd-run"):
            self.skipTest("systemd-run unavailable")
        probe = subprocess.run(["systemd-run", "--user", "--scope", "--quiet", "--", "true"],
                               env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        if probe.returncode:
            self.skipTest("systemd user scope unavailable")
        for role in ("agent", "accept"):
            with self.subTest(role=role):
                pid_file = self.work / f"{role}-untagged.pid"
                command = ("env -i setsid sh -c "
                           + shlex.quote(f"echo $$ > {shlex.quote(str(pid_file))}; exec sleep 300")
                           + " </dev/null >/dev/null 2>&1 & sleep .2")
                self.fake_pi([answer("done"), SETTLED], pre=command if role == "agent" else "")
                result = self.cli("run", *( ["--accept", command] if role == "accept" else []), "task")
                state = self.outcome(result)
                self.assertEqual(state["state"], "delivered" if role == "accept" else "answered", result.stderr)
                pid = int(pid_file.read_text())
                self.addCleanup(lambda p=pid: os.kill(p, signal.SIGKILL) if Path(f"/proc/{p}").exists() else None)
                self.assertTrue(process_gone(pid_file))
                self.assertIn(str(pid), json.loads((Path(state["dir"]) / "cleanup.json").read_text())["processes"])
                self.assertGreaterEqual(state["cleanup"]["terminated"], 1)
                self.assertIn(f"-{role}", (Path(state["dir"]) / "scopes").read_text())

        repo = self.repo({"a.txt": "a\n"})
        setup_pid = self.work / "setup-untagged.pid"
        setup_command = ("env -i setsid sh -c "
                         + shlex.quote(f"echo $$ > {shlex.quote(str(setup_pid))}; exec sleep 300")
                         + " </dev/null >/dev/null 2>&1 & sleep .2")
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"setup": [setup_command]}}))
        self.fake_pi([answer("done"), SETTLED])
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        pid = int(setup_pid.read_text())
        self.addCleanup(lambda p=pid: os.kill(p, signal.SIGKILL) if Path(f"/proc/{p}").exists() else None)
        self.assertTrue(process_gone(setup_pid))
        self.assertIn(str(pid), json.loads((Path(state["dir"]) / "cleanup.json").read_text())["processes"])
        self.assertIn("-setup-1", (Path(state["dir"]) / "scopes").read_text())

    def test_cgroup_opt_out_keeps_group_and_marker_cleanup(self):
        self.env["DELEGATE_CGROUP"] = "0"
        command = self.detached_server("opt-out-server")
        self.fake_pi([answer("done"), SETTLED], pre=command)
        state = self.outcome(self.cli("run", "task"))
        self.assertFalse((Path(state["dir"]) / "scopes").exists())
        self.assertFalse((Path(state["dir"]) / "scopes.lock").exists())
        self.assertIn(str(int((self.work / "opt-out-server.pid").read_text())),
                      json.loads((Path(state["dir"]) / "cleanup.json").read_text())["processes"])
        self.assertTrue(process_gone(self.work / "opt-out-server.pid"))

    def fake_systemctl(self):
        self.env.update(DELEGATE_CGROUP="0", TEST_SYSTEMD_DB=str(self.work / "units.json"),
                        TEST_SYSTEMD_LOG=str(self.work / "systemctl.jsonl"))
        (self.work / "units.json").write_text("{}")
        script = self.bin / "systemctl"
        script.write_text(f"#!{sys.executable}\n" + '''import json, os, sys, time
from pathlib import Path
args = sys.argv[1:]
assert args[0] == "--user", args
with open(os.environ["TEST_SYSTEMD_LOG"], "a") as log:
    log.write(json.dumps(args) + "\\n")
if os.environ.get("TEST_SYSTEMD_NO_BUS"):
    print("Failed to connect to bus", file=sys.stderr)
    sys.exit(1)
db = Path(os.environ["TEST_SYSTEMD_DB"])
units = json.loads(db.read_text())
if "list-units" in args:
    for unit, props in units.items():
        print(unit, "loaded", props.get("ActiveState", "active"), "running", "test unit")
    sys.exit(0)
unit = args[-1]
props = units.get(unit, {"ActiveState": "inactive"})
if "show" in args:
    keys = [args[i + 1] for i, arg in enumerate(args) if arg == "-p"]
    for key in keys:
        print(props.get(key, "") if "--value" in args else key + "=" + props.get(key, ""))
elif "stop" in args:
    # Prove clean invokes stop before deleting the worktree or run marker.
    assert Path(props["existingPath"]).exists(), props
    if props.get("stopFail"):
        sys.exit(1)
    if props.get("hang"):
        time.sleep(30)
    if not props.get("stubborn"):
        props["ActiveState"] = "inactive"
    db.write_text(json.dumps(units))
elif "kill" in args:
    assert "--signal=SIGKILL" in args
    props["ActiveState"] = "inactive"
    db.write_text(json.dumps(units))
else:
    sys.exit(1)
''')
        script.chmod(0o755)
        register = self.bin / "register-unit"
        register.write_text(f"#!{sys.executable}\n" + '''import json, os, sys
from pathlib import Path
unit, mode = sys.argv[1:]
cwd, marker = os.getcwd(), os.environ["DELEGATE_RUN_DIR"]
props = {"ActiveState": "active", "existingPath": cwd}
if mode in ("cwd", "stubborn", "hang", "fail"):
    props["WorkingDirectory"] = cwd
elif mode == "exec":
    props["ExecStart"] = "{ path=/usr/bin/sh ; argv[]=/usr/bin/sh " + json.dumps(cwd + "/script.sh") + " ; ignore_errors=no ; }"
elif mode == "escaped-exec":
    props["ExecStart"] = "{ path=" + (cwd + "/script.sh").replace(" ", r"\\x20") + " ; argv[]=script ; ignore_errors=no ; }"
elif mode == "source":
    props["WorkingDirectory"] = os.environ["TEST_SYSTEMD_SOURCE"]
elif mode == "quoted-command":
    props["ExecStart"] = "{ path=/usr/bin/sh ; argv[]=/usr/bin/sh -c " + json.dumps("echo ; path=" + cwd + "/script.sh ; echo") + " ; ignore_errors=no ; }"
elif mode == "quoted-fields":
    props["ExecStart"] = '{ path=/usr/bin/printf ; argv[]=/usr/bin/printf "%s" ";" ' + json.dumps("path=" + cwd + "/script.sh") + " ; ignore_errors=no ; }"
elif mode == "near":
    props["WorkingDirectory"] = cwd + "-other"
elif mode == "parent":
    props["WorkingDirectory"] = cwd + "/../source"
elif mode == "symlink":
    (Path(cwd) / "source-link").symlink_to(os.environ["TEST_SYSTEMD_SOURCE"])
    props["ExecStart"] = "{ path=" + cwd + "/source-link/script.sh ; argv[]=script ; }"
elif mode in ("marker", "wrong-marker"):
    props["Environment"] = 'OTHER="contains DELEGATE_RUN_DIR=' + marker + '" ' + json.dumps("DELEGATE_RUN_DIR=" + marker + ("-other" if mode == "wrong-marker" else ""))
    props["existingPath"] = marker
props.update(stubborn=mode == "stubborn", hang=mode == "hang", stopFail=mode == "fail")
db = Path(os.environ["TEST_SYSTEMD_DB"])
units = json.loads(db.read_text())
units[unit] = props
db.write_text(json.dumps(units))
''')
        register.chmod(0o755)

    def stopped_units(self):
        log = self.work / "systemctl.jsonl"
        return [args[-1] for args in map(json.loads, log.read_text().splitlines()) if "stop" in args]

    def test_user_units_match_worktree_paths_and_preserve_unrelated_units(self):
        self.fake_systemctl()
        self.env["XDG_CACHE_HOME"] = str(self.work / "cache with spaces")
        repo = self.repo({"a.txt": "a\n"})
        self.env["TEST_SYSTEMD_SOURCE"] = str(repo)
        modes = ("cwd", "exec", "escaped-exec", "source", "near", "parent", "symlink", "wrong-marker", "quoted-command", "quoted-fields")
        self.fake_pi([answer("done"), SETTLED], pre="\n".join(
            f"register-unit local-dev-{mode}.service {mode}" for mode in modes)
            + "\nregister-unit matching.timer cwd\nregister-unit task.scope marker")
        result = self.cli("run", "--worktree", "--workdir", repo, "task")
        state = self.outcome(result)
        self.assertEqual((result.returncode, state["state"]), (0, "answered"), result.stderr)
        expected = {"local-dev-cwd.service", "local-dev-exec.service", "local-dev-escaped-exec.service", "task.scope"}
        self.assertEqual(set(self.stopped_units()), expected)
        self.assertEqual(state["cleanup"]["systemdStopped"], len(expected))
        self.assertEqual(state["cleanup"]["units"]["local-dev-exec.service"]["matchedBy"], "exec-start")
        self.assertIn("任务结束时停止了 4 个 systemd 服务", result.stdout)

    def test_user_units_in_place_require_exact_marker(self):
        self.fake_systemctl()
        self.env["DELEGATE_RUNS"] = str(self.work / "runs with spaces")
        self.fake_pi([answer("done"), SETTLED], pre="register-unit tagged.service marker\n"
                     "register-unit untagged.service cwd\nregister-unit other.service wrong-marker")
        state = self.outcome(self.cli("run", "task"))
        self.assertEqual(self.stopped_units(), ["tagged.service"])
        self.assertTrue(any("without DELEGATE_RUN_DIR" in d for d in state["cleanup"]["diagnostics"]))

    def test_user_units_setup_and_accept_are_reclaimed_at_each_stage(self):
        self.fake_systemctl()
        repo = self.repo({"a.txt": "a\n"})
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"setup": [
            "register-unit setup.service cwd"]}}))
        self.fake_pi([answer("done"), SETTLED], pre="register-unit agent.service cwd")
        result = self.cli("run", "--worktree", "--workdir", repo,
                          "--accept", "register-unit acceptance.service cwd", "task")
        state = self.outcome(result)
        self.assertEqual((result.returncode, state["state"]), (0, "delivered"))
        self.assertEqual(self.stopped_units()[0], "setup.service")
        self.assertEqual(set(self.stopped_units()), {"setup.service", "agent.service", "acceptance.service"})
        self.assertEqual(state["cleanup"]["systemdStopped"], 3)

    def test_user_units_stop_timeout_kills_and_failures_are_only_diagnostic(self):
        self.fake_systemctl()
        repo = self.repo({"a.txt": "a\n"})
        self.fake_pi([answer("done"), SETTLED], pre="register-unit stubborn.service stubborn\n"
                     "register-unit hanging.service hang\nregister-unit failure.service fail")
        result = self.cli("run", "--worktree", "--workdir", repo, "task")
        state = self.outcome(result)
        self.assertEqual((result.returncode, state["state"]), (0, "answered"))
        self.assertEqual(state["cleanup"]["systemdStopped"], 2)
        self.assertTrue(all(unit["killed"] for unit in state["cleanup"]["units"].values()))
        self.assertTrue(any("stop failure.service" in d for d in state["cleanup"]["diagnostics"]))

    def test_user_units_missing_systemctl_or_bus_skip_without_changing_outcome(self):
        self.fake_systemctl()
        self.fake_pi([answer("done"), SETTLED])
        self.env["TEST_SYSTEMD_NO_BUS"] = "1"
        result = self.cli("run", "task")
        state = self.outcome(result)
        self.assertEqual((result.returncode, state["state"]), (0, "answered"))
        self.assertIn("discovery skipped", " ".join(state["cleanup"]["diagnostics"]))
        self.assertNotIn("Failed to connect", result.stdout + result.stderr)
        (self.bin / "systemctl").unlink()
        # Limit PATH so a host-installed systemctl cannot satisfy the missing-command case.
        for command in ("cat", "wc", "mkdir", "touch", "sleep", "sh", "git"):
            (self.bin / command).symlink_to(shutil.which(command))
        self.env["PATH"] = str(self.bin)
        result = self.cli("run", "task")
        state = self.outcome(result)
        self.assertEqual((result.returncode, state["state"]), (0, "answered"), result.stderr)
        self.assertIn("No such file", " ".join(state["cleanup"]["diagnostics"]))

    def test_clean_reclaims_historical_user_units_before_removing_worktree(self):
        self.fake_systemctl()
        repo = self.repo({"a.txt": "a\n"})
        self.env["TEST_SYSTEMD_SOURCE"] = str(repo)
        self.fake_pi([answer("done"), SETTLED])
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        db = self.work / "units.json"
        db.write_text(json.dumps({"leftover.service": {"WorkingDirectory": state["worktree"],
            "ActiveState": "active", "existingPath": state["worktree"]}, "source.service": {
            "WorkingDirectory": str(repo), "ActiveState": "active", "existingPath": str(repo)}}))
        result = self.cli("clean", state["run"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.stopped_units(), ["leftover.service"])
        self.assertIn("停止了 1 个 systemd 服务", result.stdout)
        self.assertFalse(Path(state["worktree"]).exists())
        self.assertFalse(Path(state["dir"]).exists())

    def test_empty_copy_and_uninitialized_gitlink_report_warnings(self):
        repo = self.repo({"a.txt": "a\n"})
        (repo / "empty").mkdir()
        (repo / "module").mkdir()
        head = subprocess.check_output(["git", "-C", str(repo), "rev-parse", "HEAD"], text=True).strip()
        subprocess.run(["git", "-C", str(repo), "update-index", "--add", "--cacheinfo",
                        f"160000,{head},module"], check=True)
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"copy": ["empty"], "link": ["module"]}}))
        self.fake_pi([answer("done"), SETTLED])
        result = self.cli("run", "--worktree", "--workdir", repo, "task")
        state = self.outcome(result)
        warnings = state["warnings"]
        self.assertEqual(len(warnings), 2)
        self.assertTrue(any("uninitialized submodule" in warning for warning in warnings))
        self.assertIn("worktree.copy source is empty", result.stderr)
        self.assertIn("warning: worktree.link source is empty", result.stdout)
        self.assertEqual(json.loads((Path(state["dir"]) / "summary.json").read_text())["warnings"], warnings)
        self.assertTrue((repo / "module").is_dir())

    def test_accept_also_composes_in_order_and_records_final_command(self):
        self.env["XDG_CONFIG_HOME"] = str(self.work / "config")
        self.fake_pi([answer("done"), SETTLED])
        repo = self.repo({"a.txt": "a\n"})
        default = "echo default >> accept-order"
        explicit = "echo explicit >> accept-order"
        first = "echo first >> accept-order"
        second = "echo second >> accept-order"
        cases = [
            (default, ["--accept-also", first], [default, first], ["default", "first"]),
            (default, ["--accept-also", first, "--accept-also=" + second],
             [default, first, second], ["default", "first", "second"]),
            (default, ["--accept-also", first, "--accept", explicit, "--accept-also", second],
             [explicit, first, second], ["explicit", "first", "second"]),
            (None, ["--accept", explicit, "--accept-also", first],
             [explicit, first], ["explicit", "first"]),
            (None, ["--accept-also", first], [first], ["first"]),
            (None, ["--accept-also", first, "--accept-also", second],
             [first, second], ["first", "second"]),
            ("", ["--accept-also", first], [first], ["first"]),
            (default, ["--accept", "", "--accept-also", first], [first], ["first"]),
        ]
        for configured, args, commands, order in cases:
            with self.subTest(configured=configured, args=args):
                (repo / ".delegate.json").write_text(json.dumps(
                    {} if configured is None else {"accept": configured}))
                (repo / "accept-order").unlink(missing_ok=True)
                result = self.cli("run", "--workdir", repo, *args, "task")
                state = self.outcome(result)
                self.assertEqual((result.returncode, state["state"]), (0, "delivered"), result.stderr)
                command = " && ".join(commands)
                run = Path(state["dir"])
                self.assertEqual(state["accept"]["command"], command)
                self.assertEqual(json.loads((run / "meta.json").read_text())["accept"], command)
                self.assertEqual(json.loads((run / "summary.json").read_text())["accept"]["command"], command)
                self.assertIn(command, (run / "prompt.md").read_text())
                self.assertEqual((repo / "accept-order").read_text().splitlines(), order)

    def test_accept_also_short_circuits_and_reply_inherits_composed_command(self):
        self.env["XDG_CONFIG_HOME"] = str(self.work / "config")
        self.fake_pi([answer("done"), SETTLED])
        repo = self.repo({"a.txt": "a\n"})
        (repo / ".delegate.json").write_text(json.dumps({"accept": "false"}))
        result = self.cli("run", "--workdir", repo, "--accept-also", "touch unexpected", "task")
        state = self.outcome(result)
        self.assertEqual((result.returncode, state["state"]), (1, "rejected"))
        self.assertFalse((repo / "unexpected").exists())
        result = self.cli("run", "--workdir", repo, "--accept", "true", "--accept-also", "false",
                          "--accept-also", "touch unexpected", "task")
        self.assertEqual((result.returncode, self.outcome(result)["state"]), (1, "rejected"))
        self.assertFalse((repo / "unexpected").exists())
        parent = self.outcome(self.cli("run", "--workdir", repo, "--accept", "true",
                                      "--accept-also", "echo first", "task"))
        reply = self.outcome(self.cli("reply", "--wait", parent["run"], "--accept-also", "echo second", "more"))
        self.assertEqual(reply["accept"]["command"], "true && echo first && echo second")
        inherited = self.outcome(self.cli("reply", "--wait", reply["run"], "more"))
        self.assertEqual(inherited["accept"]["command"], reply["accept"]["command"])
        replaced = self.outcome(self.cli("reply", "--wait", inherited["run"], "--accept", "echo new",
                                        "--accept-also", "true", "more"))
        self.assertEqual(replaced["accept"]["command"], "echo new && true")
        disabled = self.outcome(self.cli("reply", "--wait", replaced["run"], "--no-accept", "more"))
        standalone = self.outcome(self.cli("reply", "--wait", disabled["run"], "--accept-also", "true", "more"))
        self.assertEqual(standalone["accept"]["command"], "true")

    def test_accept_also_conflicts_with_no_accept_in_any_order(self):
        for command in ("start", "run", "reply"):
            for args in (["--no-accept", "--accept-also", "true"],
                         ["--accept-also=true", "--no-accept"],
                         ["--no-accept", "--accept", "true", "--accept-also", "true"]):
                with self.subTest(command=command, args=args):
                    result = self.cli(command, *(["parent"] if command == "reply" else []), *args, "task")
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn("--no-accept and --accept-also", result.stderr)
        self.assertFalse((self.work / "runs").exists())

    def test_accept_override_warns_only_when_replacing_nonempty_default(self):
        self.env["XDG_CONFIG_HOME"] = str(self.work / "config")
        self.fake_pi([answer("done"), SETTLED])
        repo = self.repo({"a.txt": "a\n"})
        default = "echo configured-default"
        cases = [
            (default, ["--accept", "true"], True),
            (default, ["--accept", "", "--accept-also", "true"], True),
            (default, ["--accept", "true", "--accept-also", "true"], True),
            (default, ["--accept", default, "--accept-also", "true"], False),
            (default, ["--accept", default], False),
            (default, ["--accept-also", "true"], False),
            (default, ["--no-accept"], False),
            ("", ["--accept", "true"], False),
            (None, ["--accept", "true"], False),
            (default, ["--read-only", "--accept", "true"], False),
        ]
        for configured, args, warn in cases:
            with self.subTest(configured=configured, args=args):
                (repo / ".delegate.json").write_text(json.dumps(
                    {} if configured is None else {"accept": configured}))
                result = self.cli("start", "--workdir", repo, *args, "task")
                self.assertEqual(result.returncode, 0, result.stderr)
                warnings = [line for line in result.stderr.splitlines() if "replaces default accept" in line]
                self.assertEqual(len(warnings), int(warn), result.stderr)
                if warn:
                    self.assertIn(default, warnings[0])
                    self.assertIn("--accept-also COMMAND", warnings[0])
                    self.assertIn("替换了默认验收", warnings[0])
                self.assertEqual(self.cli("wait", self.outcome(result)["run"]).returncode, 0)
        user_config = self.work / "config" / "delegate" / "config.json"
        user_config.parent.mkdir(parents=True)
        user_config.write_text(json.dumps({"accept": default}))
        (repo / ".delegate.json").write_text("{}")
        appended = self.outcome(self.cli("run", "--workdir", repo, "--accept-also", "true", "task"))
        self.assertEqual(appended["accept"]["command"], default + " && true")
        result = self.cli("run", "--workdir", repo, "--accept", "true", "task")
        self.assertIn('replaces default accept "' + default + '"', result.stderr)

    def test_repository_default_accept_applies_only_to_writes(self):
        repo = self.repo({"a.txt": "a\n"})
        (repo / ".delegate.json").write_text(json.dumps({"accept": "test -n \"$DELEGATE_RUN_DIR\" && touch accepted"}))
        self.fake_pi([answer("done"), SETTLED])
        default = self.outcome(self.cli("run", "--workdir", repo, "task"))
        self.assertEqual(default["state"], "delivered")
        self.assertTrue((repo / "accepted").exists())
        self.assertIn("touch accepted", (Path(default["dir"]) / "prompt.md").read_text())
        isolated = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.assertEqual(isolated["state"], "delivered")
        self.assertEqual(isolated["accept"]["command"], "test -n \"$DELEGATE_RUN_DIR\" && touch accepted")
        self.assertTrue((Path(isolated["worktree"]) / "accepted").exists())
        override = self.outcome(self.cli("run", "--workdir", repo, "--accept", "true", "task"))
        self.assertEqual(override["accept"]["command"], "true")
        disabled = self.outcome(self.cli("run", "--workdir", repo, "--no-accept", "task"))
        self.assertEqual(disabled["state"], "answered")
        self.assertNotIn("accept", disabled)
        read_only = self.outcome(self.cli("run", "--workdir", repo, "--read-only", "task"))
        self.assertEqual(read_only["state"], "answered")
        self.assertNotIn("accept", read_only)
        (repo / ".delegate.json").write_text('{"accept": 3}')
        self.assertIn("accept must be a string", self.cli("run", "--workdir", repo, "task").stderr)

    def test_leaked_tool_call_is_rerun_once(self):
        self.fake_pi([answer(LEAKED), SETTLED], [answer("real answer"), SETTLED])
        result = self.cli("run", "--read-only", "review")
        self.assertEqual(result.returncode, 0, result.stderr)
        state = self.outcome(result)
        self.assertEqual((state["state"], state["attempts"]), ("answered", 2))
        self.assertIn("real answer", result.stdout)
        self.assertIn('"rerun"', (Path(state["dir"]) / "events.jsonl").read_text())
        # Outside git nothing isolates or checks a read-only run: Pi keeps only its reading tools here.
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
        for text, state in ((LEAKED, "malformed"), ("Use `call:default_api:read{...}` carefully.\nDone.", "answered"),
                            ("Result: {a: 1}", "answered")):
            self.fake_pi([answer(text), SETTLED])
            self.assertEqual(self.outcome(self.cli("run", "--retries", "0", "task"))["state"], state, text)

    def test_undecodable_output_lines_are_skipped_not_fatal(self):
        # One bad line must not stop the reader: the agent would block on a full pipe and the answer be lost.
        self.fake_pi([answer("still read"), SETTLED], pre="printf '\\377\\376 not utf-8\\n'; printf '{\\\"x\\\": \\377}\\n'")
        state = self.outcome(self.cli("run", "task"))
        self.assertEqual(state["state"], "answered")
        self.assertIn("still read", (Path(state["dir"]) / "result.md").read_text())

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
        self.assertIn("省略 1212 字符", result.stdout)
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

    def test_clean_keeps_answers_shown_truncated_until_read_in_full(self):
        self.fake_pi([answer("x" * 50), SETTLED])
        self.env["DELEGATE_RESULT_CHARS"] = "10"
        run = Path(json.loads(self.cli("start", "--name", "long", "task").stdout)["dir"])
        self.assertIn("省略 41 字符", self.cli("wait", "long").stdout)
        self.assertIn("only shown truncated", self.cli("clean", "--finished").stderr)
        self.assertTrue(run.exists())
        os.utime(run / "exit_code", (1, 1))
        self.fake_pi([answer("ok"), SETTLED])
        self.assertEqual(self.cli("run", "--name", "later", "task").returncode, 0)
        self.assertTrue(run.exists())  # expired but only shown truncated: not pruned
        self.assertEqual(self.cli("result", "long").stdout.strip(), "x" * 50)
        self.assertIn("-long (answered)", self.cli("clean", "--finished").stdout)

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
        self.assertTrue(state["dir"].startswith(str(repo / ".local/run/delegate/")))
        self.assertEqual((repo / ".local/run/delegate/.gitignore").read_text(), "*\n")
        self.assertEqual(oct(Path(state["dir"]).stat().st_mode & 0o777), "0o700")

    def test_old_default_run_directory_remains_resolvable(self):
        repo = self.repo({"a.txt": "a\n"})
        del self.env["DELEGATE_RUNS"]
        self.fake_pi([answer("old answer"), SETTLED])
        state = self.outcome(self.cli("run", "--read-only", "--name", "legacy", "task", cwd=repo))
        old = repo / ".local/run/pi" / state["run"]
        old.parent.mkdir(parents=True)
        shutil.move(state["dir"], old)
        meta = json.loads((old / "meta.json").read_text())
        meta["dir"] = str(old)
        (old / "meta.json").write_text(json.dumps(meta))
        self.assertEqual(self.outcome(self.cli("status", "legacy", cwd=repo))["dir"], str(old))
        self.assertIn("old answer", self.cli("result", state["run"], cwd=repo).stdout)
        self.assertIn("old answer", self.cli("wait", "legacy", cwd=repo).stdout)
        self.assertEqual(self.cli("diff", "legacy", cwd=repo).returncode, 0)
        self.assertEqual(self.cli("clean", state["run"], cwd=repo).returncode, 0)
        self.assertFalse(old.exists())

    def test_long_unicode_answer_keeps_head_and_tail(self):
        self.env["DELEGATE_RESULT_CHARS"] = "12"
        self.fake_pi([answer("开" * 8 + "中" * 20 + "终" * 4), SETTLED])
        result = self.cli("run", "--read-only", "task")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("开" * 8, result.stdout)
        self.assertIn("终" * 3, result.stdout)
        self.assertNotIn("中" * 2, result.stdout)
        self.assertIn("省略 21 字符", result.stdout)  # result.md includes its final newline
        self.assertIn("中" * 20, self.cli("result", "last").stdout)

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

    def test_write_setup_runs_only_for_write_tasks(self):
        repo = self.repo({"a.txt": "a\n", ".gitignore": "setup-ran\nwrite-setup-ran\n"})
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {
            "setup": ["touch setup-ran"], "writeSetup": ["test -f setup-ran && touch write-setup-ran"]}}))
        self.fake_pi([answer("ok"), SETTLED])
        read = self.outcome(self.cli("run", "--read-only", "--workdir", repo, "review"))
        self.assertTrue((Path(read["worktree"]) / "setup-ran").exists())
        self.assertFalse((Path(read["worktree"]) / "write-setup-ran").exists())
        self.fake_codex(codex_events("done"))
        write = self.outcome(self.cli("run", "--agent", "codex", "--worktree", "--workdir", repo, "task"))
        self.assertTrue((Path(write["worktree"]) / "write-setup-ran").exists())  # after setup, in order
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"writeSetup": "  "}}))
        self.assertEqual(self.cli("run", "--worktree", "--workdir", repo, "task").returncode, 2)

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
        # ...and sees it as the caller does: uncommitted, on top of the caller's HEAD.
        self.assertIn("+uncommitted", subprocess.run(["git", "-C", str(worktree), "diff", "HEAD"],
                                                     capture_output=True, text=True).stdout)
        self.assertTrue((worktree / "setup-ran").exists())  # Codex may run commands, so setup runs
        self.assertEqual((repo / "a.txt").read_text(), "a\nuncommitted\ncaller\n")
        self.fake_pi([answer("ok"), SETTLED])
        pi = self.outcome(self.cli("run", "--read-only", "--workdir", repo, "review"))
        # In its own worktree a read-only Pi is isolated and checked like Codex: full tools (git log/diff, tests),
        # the boundary in writing, and the worktree set up for commands.
        self.assertNotIn("--tools", (self.work / "pi.log").read_text().splitlines()[-1])
        self.assertIn("Read-only task", (Path(pi["dir"]) / "prompt.md").read_text())
        self.assertTrue((Path(pi["worktree"]) / "setup-ran").exists())
        # Reading the live tree (--in-place) there is no isolation, so the tool limit comes back.
        self.fake_pi([answer("ok"), SETTLED])
        self.outcome(self.cli("run", "--read-only", "--in-place", "--workdir", repo, "review"))
        self.assertIn("--tools read,grep,find,ls", (self.work / "pi.log").read_text().splitlines()[-1])
        cleaned = self.cli("clean", pi["run"]).stdout
        self.assertIn("removed", cleaned)
        self.assertNotIn("never applied", cleaned)  # a read-only worktree has nothing to merge
        self.assertFalse(Path(pi["worktree"]).exists())

    def test_delegated_agents_cannot_delegate_at_all(self):
        self.fake_pi([answer("ok"), SETTLED])
        self.fake_codex(codex_events("ok"))
        self.assertEqual(self.outcome(self.cli("run", "--agent", "codex", "task"))["state"], "answered")
        self.assertIn(" codex ", (self.work / "pi.log.codex").read_text())  # its agent carries DELEGATE_AGENT
        # One level only, whoever the caller is and whatever it asks for: results come back to the caller.
        for marker in ({"DELEGATE_AGENT": "codex"}, {"DELEGATE_AGENT": "pi"}, {"PI_DELEGATE_AGENT": "codex"}):
            env = {**self.env, **marker}
            for args in (["start", "--agent", "pi", "task"], ["run", "--tier", "cheap", "--read-only", "task"],
                         ["reply", "last", "more"]):
                result = subprocess.run([str(DELEGATE), *args], cwd=self.work, env=env, capture_output=True,
                                        text=True, timeout=30)
                self.assertEqual(result.returncode, 2, (marker, args))
                self.assertIn("refusing nested delegation", result.stderr)
            lane = subprocess.run([str(DELEGATE), "lane", "true"], cwd=self.work, env=env, timeout=30)
            self.assertEqual(lane.returncode, 0)  # heavy checks still queue

    def test_tier_picks_the_colleague(self):
        del self.env["DELEGATE_STRONG_AGENT"]  # the real mapping: cheap = pi, strong = codex
        self.fake_pi([answer("read it"), SETTLED])
        self.fake_codex(codex_events("wrote it"))
        state = self.outcome(self.cli("run", "--read-only", "task"))
        self.assertEqual((state["agent"], state["tier"]), ("pi", "cheap"))  # reading defaults to cheap
        state = self.outcome(self.cli("run", "task"))
        self.assertEqual((state["agent"], state["tier"]), ("codex", "strong"))  # writing defaults to strong
        state = self.outcome(self.cli("run", "--read-only", "--tier", "strong", "task"))
        self.assertEqual(state["agent"], "codex")
        state = self.outcome(self.cli("run", "--agent", "pi", "task"))
        self.assertEqual(state["agent"], "pi")
        self.assertEqual((state["tier"], state["agentPinned"]), ("cheap", True))  # named outright: display only
        self.assertEqual(self.cli("run", "--agent", "pi", "--tier", "cheap", "task").returncode, 2)
        self.env["DELEGATE_CHEAP_AGENT"] = "gemini"
        self.assertIn("DELEGATE_CHEAP_AGENT", self.cli("run", "--read-only", "task").stderr)
        del self.env["DELEGATE_CHEAP_AGENT"]
        (self.bin / "pi").unlink()  # a host without Pi still works: the cheap tier gives way
        self.env["PATH"] = f"{self.bin}:/usr/bin:/bin"
        result = self.cli("run", "--read-only", "task")
        self.assertEqual((self.outcome(result)["agent"], self.outcome(result)["tier"]), ("codex", "strong"))
        self.assertIn("using the strong tier", result.stderr)

    def test_failed_cheap_run_escalates_once_when_nothing_changed(self):
        del self.env["DELEGATE_STRONG_AGENT"]
        self.fake_pi([answer(LEAKED), SETTLED])
        self.fake_codex(codex_events("found it"))
        result = self.cli("run", "--read-only", "--retries", "0", "审查一下")
        state = self.outcome(result)
        self.assertEqual(result.returncode, 0)
        self.assertEqual((state["state"], state["agent"], state["tier"], state["escalatedFrom"], state["attempts"]),
                         ("answered", "codex", "strong", "pi", 2))
        self.assertIn("只读任务", (self.work / "pi.log.codex-prompt").read_text())  # Codex is told; Pi needed no words
        self.assertEqual((self.work / "pi.log.codex-prompt").read_text().count("只读任务"), 1)
        self.assertIn("found it", (Path(state["dir"]) / "result.md").read_text())
        self.fake_codex(codex_events("more"))
        self.assertEqual(self.outcome(self.cli("reply", "--wait", state["run"], "and?"))["agent"], "codex")
        # A write run that changed nothing and failed its check is handed over too...
        repo = self.repo({"a.txt": "a\n"})
        self.fake_pi([answer("done"), SETTLED])
        self.fake_codex(codex_events("fixed"), pre="touch made.txt")
        state = self.outcome(self.cli("run", "--tier", "cheap", "--workdir", repo, "--accept", "test -f made.txt",
                                      "task"))
        self.assertEqual((state["state"], state["escalatedFrom"], state["files"]), ("delivered", "pi", ["made.txt"]))
        # ...but not one that left changes behind, nor one whose colleague was named outright.
        (repo / "made.txt").unlink()
        self.fake_pi([answer("done"), SETTLED], pre="echo half > partial.txt")
        state = self.outcome(self.cli("run", "--tier", "cheap", "--workdir", repo, "--accept", "test -f made.txt",
                                      "task"))
        self.assertEqual((state["state"], state["agent"]), ("rejected", "pi"))
        self.assertNotIn("escalatedFrom", state)
        self.fake_pi([answer(LEAKED), SETTLED])
        state = self.outcome(self.cli("run", "--agent", "pi", "--read-only", "--retries", "0", "task"))
        self.assertEqual((state["state"], state["agent"]), ("malformed", "pi"))
        # In git the isolated Pi was told already; escalating must not say it twice.
        self.fake_pi([answer(LEAKED), SETTLED])
        self.fake_codex(codex_events("found it"))
        state = self.outcome(self.cli("run", "--read-only", "--retries", "0", "--workdir", repo, "审查一下"))
        self.assertEqual(state["escalatedFrom"], "pi")
        self.assertEqual((self.work / "pi.log.codex-prompt").read_text().count("只读任务"), 1)

    def test_escalation_waits_for_room_in_the_strong_tier(self):
        del self.env["DELEGATE_STRONG_AGENT"]
        self.fake_pi([answer(LEAKED), SETTLED])
        self.fake_codex(codex_events("ok"), pre='[ -n "$SLOW" ] && sleep 30')
        env = {**self.env, "SLOW": "1"}
        started = subprocess.run([str(DELEGATE), "start", "--json", "--agent", "codex", "--read-only", "busy"], cwd=self.work,
                                 env=env, capture_output=True, text=True, timeout=30)
        busy = json.loads(started.stdout.splitlines()[0])
        self.env["DELEGATE_MAX_CODEX"] = "1"
        state = self.outcome(self.cli("run", "--read-only", "--retries", "0", "task"))
        self.assertEqual((state["state"], state["agent"]), ("malformed", "pi"))  # no room: not escalated
        self.assertIn("escalate_skipped", (Path(state["dir"]) / "events.jsonl").read_text())
        self.cli("stop", busy["run"])

    def test_wait_without_runs_collects_everything_pending(self):
        self.fake_pi([answer("ok"), SETTLED])
        for name in ("one", "two"):
            self.assertEqual(self.cli("start", "--read-only", "--name", name, "task").returncode, 0)
        result = self.cli("wait")
        self.assertEqual(result.returncode, 0)
        self.assertEqual(sorted(json.loads(line)["name"] for line in result.stdout.splitlines()
                                if line.startswith('{"run"')), ["one", "two"])
        self.assertIn("no active or undelivered runs", self.cli("wait").stderr)

    def names(self, result):
        return [json.loads(line)["name"] for line in result.stdout.splitlines() if line.startswith('{"run"')]

    def test_wait_any_returns_each_run_as_it_finishes(self):
        self.fake_pi([answer("ok"), SETTLED], pre='case "$*" in *slow*) sleep 4;; esac')
        for name in ("slow", "fast"):
            self.assertEqual(self.cli("start", "--read-only", "--name", name, "task").returncode, 0)
        self.assertEqual(self.cli("wait", "--any", "--stream").returncode, 2)
        first = self.cli("wait", "--any")
        self.assertEqual((first.returncode, self.names(first)), (0, ["fast"]), first.stderr)
        self.assertIn("ok", first.stdout)  # the answer comes with it
        self.assertIn("1 still running", first.stderr)
        self.assertIn("wait --any", first.stderr)
        second = self.cli("wait", "--any")
        self.assertEqual((second.returncode, self.names(second)), (0, ["slow"]), second.stderr)
        self.assertNotIn("still running", second.stderr)
        self.assertIn("no active or undelivered runs", self.cli("wait", "--any").stderr)
        # Named runs that were already reported are skipped, so the same command can be repeated.
        self.assertIn("no active or undelivered runs", self.cli("wait", "--any", "slow", "fast").stderr)

    def test_wait_any_max_exits_75_when_nothing_finished(self):
        self.fake_pi([answer("ok"), SETTLED], sleep=30)
        self.assertEqual(self.cli("start", "--read-only", "--name", "slow", "task").returncode, 0)
        result = self.cli("wait", "--any", "--max", "0.3")
        self.assertEqual((result.returncode, self.names(result)), (75, []))
        self.assertIn("call wait --any again", result.stderr)
        self.cli("stop", "slow")

    def test_wait_completion_timing_and_legacy_finished_at(self):
        self.fake_pi([answer("ok"), SETTLED])
        for name in ("old-a", "old-b"):
            started = self.outcome(self.cli("start", "--read-only", "--name", name, "task"))
            self.until(lambda: (Path(started["dir"]) / "exit_code").exists(), name)
        old_run = Path(started["dir"])
        summary = json.loads((old_run / "summary.json").read_text())
        self.assertRegex(summary.pop("finishedAt"), r"^\d{4}-\d{2}-\d{2}T")
        (old_run / "summary.json").write_text(json.dumps(summary))
        gate = self.work / "release"
        self.fake_pi([answer("new"), SETTLED], pre=f"while [ ! -f {gate} ]; do sleep .05; done")
        self.cli("start", "--read-only", "--name", "new", "task")
        old = self.cli("wait", "--any")
        lines = [json.loads(line) for line in old.stdout.splitlines() if line.startswith('{"run"')]
        self.assertEqual([line["name"] for line in lines], ["old-a", "old-b"])
        self.assertTrue(all(line["completionTiming"] == "already-finished" for line in lines))
        self.assertIn("finishedAt", lines[0])
        self.assertNotIn("finishedAt", lines[1])
        waiter = subprocess.Popen([str(DELEGATE), "wait", "--json", "--any"], env=self.env,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(waiter.kill)
        threading.Timer(.5, gate.touch).start()
        out, err = waiter.communicate(timeout=10)
        self.assertEqual(waiter.returncode, 0, err)
        self.assertEqual(json.loads(out.splitlines()[0])["completionTiming"], "finished-during-wait")
        self.assertIn("no active or undelivered", self.cli("wait", "--any", "old-a", "old-b", "new").stderr)

    def test_wait_stream_prints_one_line_per_run_and_picks_up_new_runs(self):
        self.env["DELEGATE_CALLER"] = "streamer"
        self.fake_pi([answer("streamed answer"), SETTLED], pre='case "$*" in *slow*) sleep 4;; esac')
        for name in ("slow", "fast"):
            self.assertEqual(self.cli("start", "--read-only", "--name", name, "task").returncode, 0)
        stream = subprocess.Popen([str(DELEGATE), "wait", "--json", "--stream"], cwd=self.work, env=self.env,
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(stream.kill)
        first = json.loads(stream.stdout.readline())
        self.assertEqual((first["name"], first["state"]), ("fast", "answered"))
        self.assertTrue(first["report"].endswith(f"wait {first['run']}"))
        self.assertEqual(self.cli("start", "--read-only", "--name", "late", "task").returncode, 0)
        out, err = stream.communicate(timeout=30)
        self.assertEqual(stream.returncode, 0, err)
        rest = [json.loads(line)["name"] for line in out.splitlines()]
        self.assertEqual(sorted(rest), ["late", "slow"])
        self.assertNotIn("streamed answer", out)  # one line each; the answer is read with wait
        self.assertIn("all 3 runs reported", err)
        collected = self.cli("wait")
        self.assertEqual(sorted(self.names(collected)), ["fast", "late", "slow"])
        self.assertIn("streamed answer", collected.stdout)

    def test_outcome_reports_source_changes_since_the_worktree_snapshot(self):
        repo = self.repo({"a.txt": "a\n", "b.txt": "b\n", "c.txt": "c\n"})
        self.fake_pi([answer("done"), SETTLED], pre="echo run > a.txt; echo run > b.txt")
        result = self.cli("run", "--worktree", "--workdir", repo, "task")
        state = self.outcome(result)
        self.assertNotIn("sourceDrift", state)
        (repo / "b.txt").write_text("caller\n")
        (repo / "c.txt").write_text("caller\n")
        (repo / "d.txt").write_text("new\n")
        again = self.outcome(self.cli("wait", state["run"]))
        self.assertEqual(again["sourceDrift"], {"files": 3, "overlap": ["b.txt"]})
        self.assertIn("reply", again["next"])
        self.assertIn("--sync", again["next"])
        self.assertNotIn("sourceDrift", self.outcome(self.cli("status", state["run"])))  # status stays cheap
        (repo / "b.txt").write_text("b\n")
        calm = self.outcome(self.cli("wait", state["run"]))
        self.assertEqual(calm["sourceDrift"], {"files": 2, "overlap": []})
        self.assertIn(f"apply {state['run']}", calm["next"])
        self.assertNotIn("--sync", calm["next"])

    def test_caller_is_written_for_start_run_reply_and_waiting_run(self):
        self.fake_pi([answer("ok"), SETTLED])
        self.env["CLAUDE_CODE_SESSION_ID"] = "claude-session"
        first = self.outcome(self.cli("start", "--read-only", "task"))
        self.assertEqual(json.loads((Path(first["dir"]) / "meta.json").read_text())["caller"], "claude-session")
        self.env["DELEGATE_CALLER"] = "explicit-session"
        waiting = self.outcome(self.cli("start", "--read-only", "--after", first["run"], "later"))
        self.assertEqual(json.loads((Path(waiting["dir"]) / "meta.json").read_text())["caller"],
                         "explicit-session")
        synchronous = self.outcome(self.cli("run", "--read-only", "another"))
        self.assertEqual(json.loads((Path(synchronous["dir"]) / "meta.json").read_text())["caller"],
                         "explicit-session")
        self.assertEqual(self.cli("wait", first["run"]).returncode, 0)
        replied = self.outcome(self.cli("reply", first["run"], "more"))
        self.assertEqual(json.loads((Path(replied["dir"]) / "meta.json").read_text())["caller"],
                         "explicit-session")
        self.assertEqual(self.cli("wait", "--all").returncode, 0)
        self.env.pop("DELEGATE_CALLER")
        self.env.pop("CLAUDE_CODE_SESSION_ID")
        unknown = self.outcome(self.cli("run", "--read-only", "unknown"))
        self.assertIsNone(json.loads((Path(unknown["dir"]) / "meta.json").read_text())["caller"])
        # Other harnesses: Codex and Pi export their session to the commands they run.
        self.env.update(PI_SESSION_ID="pi-session", CODEX_THREAD_ID="codex-thread")
        for expected, source in (("codex-thread", "CODEX_THREAD_ID"), ("pi-session", "PI_SESSION_ID")):
            meta = json.loads((Path(self.outcome(self.cli("run", "--read-only", source))["dir"])
                               / "meta.json").read_text())
            self.assertEqual((meta["caller"], meta["callerSource"]), (expected, source))
            self.env.pop(source)

    def test_protocol_reports_caller_agents_and_shadowed_binaries(self):
        self.fake_pi([answer("ok"), SETTLED])
        shadow = self.work / "shadow"
        shadow.mkdir()
        shutil.copy(self.bin / "pi", shadow / "pi")
        self.env.update(PATH=f"{self.bin}:{shadow}:/usr/bin:/bin", CODEX_THREAD_ID="thread-7")
        result = self.cli("protocol")
        self.assertEqual(result.returncode, 0, result.stderr)
        line = json.loads(result.stdout)
        self.assertEqual((line["protocol"], line["caller"]), (1, {"id": "thread-7", "source": "CODEX_THREAD_ID"}))
        agents = {agent["name"]: agent for agent in line["agents"]}
        self.assertEqual(agents["pi"]["tiers"], ["cheap", "strong"])  # setUp maps strong to the fake Pi
        self.assertEqual(agents["pi"]["bin"], str((self.bin / "pi").resolve()))
        self.assertEqual(agents["pi"]["version"], "fake-pi 1.0")
        self.assertEqual(agents["pi"]["shadowed"], [str(shadow / "pi")])  # as found on PATH, not resolved
        self.assertEqual((agents["codex"]["tiers"], agents["codex"]["available"], agents["codex"]["bin"]),
                         ([], False, None))
        self.env.pop("DELEGATE_STRONG_AGENT")
        self.env.pop("CODEX_THREAD_ID")
        line = json.loads(self.cli("protocol").stdout)
        self.assertIsNone(line["caller"])
        self.assertEqual({a["name"]: a["tiers"] for a in line["agents"]}, {"pi": ["cheap"], "codex": ["strong"]})
        self.assertEqual(self.cli("protocol", "extra").returncode, 2)

    def test_wait_collects_only_current_caller_and_reports_other_failure(self):
        self.fake_pi([answer("ok"), SETTLED])
        self.env["DELEGATE_CALLER"] = "other"
        failed = self.outcome(self.cli("start", "--name", "old-failure", "--accept", "false", "task"))
        self.until(lambda: (Path(failed["dir"]) / "exit_code").exists(), "failed run")
        self.assertEqual(json.loads(self.cli("status", failed["run"]).stdout)["state"], "rejected")
        self.env["DELEGATE_CALLER"] = "mine"
        started = self.cli("start", "--read-only", "--name", "new-success", "task")
        self.assertEqual(started.returncode, 0, started.stderr)
        self.assertIn("old-failure", started.stderr)
        self.assertIn("wait --all", started.stderr)
        self.assertRegex(started.stderr, r"old-failure \d+[smhd]")
        own = self.outcome(started)
        result = self.cli("wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(own["run"], result.stdout)
        self.assertNotIn(failed["run"], result.stdout)
        self.assertIn("1 other runs", result.stderr)
        self.assertFalse((Path(failed["dir"]) / ".delivered").exists())
        old_meta_path = Path(failed["dir"]) / "meta.json"
        old_meta = json.loads(old_meta_path.read_text())
        del old_meta["caller"]  # pre-5.8 records also belong to the other-run summary
        old_meta["name"] = "old\nfailure"
        old_meta_path.write_text(json.dumps(old_meta))
        run_hint = self.cli("run", "--read-only", "quick").stderr
        self.assertIn("old failure", run_hint)
        self.assertEqual(sum("other runs" in line for line in run_hint.splitlines()), 1)
        replied = self.cli("reply", own["run"], "more")
        self.assertIn("old failure", replied.stderr)
        self.assertEqual(self.cli("wait", self.outcome(replied)["run"]).returncode, 0)
        self.assertEqual(self.cli("wait").returncode, 0)
        self.assertEqual(self.cli("wait", "--all").returncode, 1)

    def test_unknown_caller_wait_collects_all_and_status_reports_age(self):
        self.fake_pi([answer("ok"), SETTLED])
        self.env["DELEGATE_CALLER"] = "old"
        old = self.outcome(self.cli("start", "--read-only", "--name", "older", "task"))
        self.until(lambda: (Path(old["dir"]) / "exit_code").exists(), "older run to finish")
        meta_path = Path(old["dir"]) / "meta.json"
        meta = json.loads(meta_path.read_text())
        meta["startedEpoch"] = int(time.time()) - 2 * 86400
        meta_path.write_text(json.dumps(meta))
        self.env.pop("DELEGATE_CALLER")
        launched = self.cli("start", "--read-only", "--name", "newer", "task")
        self.assertIn("older 2d", launched.stderr)
        new = self.outcome(launched)
        status = json.loads(self.cli("status", old["run"]).stdout)
        self.assertGreaterEqual(status["ageSeconds"], 2 * 86400)
        result = self.cli("wait")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn(old["run"], result.stdout)
        self.assertIn(new["run"], result.stdout)
        self.assertNotIn("other runs", result.stderr)
        del meta["startedEpoch"]
        meta_path.write_text(json.dumps(meta))
        self.assertNotIn("ageSeconds", json.loads(self.cli("status", old["run"]).stdout))

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
        # git-style path limiting, with or without the `--` separator
        for args in (("diff", "last", "--", "dirty.txt"), ("diff", "last", "dirty.txt"), ("diff", "--", "dirty.txt")):
            limited = self.cli(*args)
            self.assertEqual(limited.returncode, 0, (args, limited.stderr))
            self.assertIn("+theirs", limited.stdout, args)
            self.assertNotIn("new.txt", limited.stdout, args)
        self.assertNotIn("+mine", diff)  # dirty before the run: not the agent's work
        self.assertEqual(subprocess.run(["git", "-C", str(repo), "diff", "--cached", "--name-only"],
                                        capture_output=True, text=True).stdout, "")  # real index untouched
        self.fake_pi([answer("looked"), SETTLED])
        unchanged = self.cli("run", "--workdir", repo, "task")
        self.assertIn("===== changes: ", unchanged.stdout.split("none")[0])
        self.assertNotIn("shape", self.outcome(unchanged))

    def test_write_shape_uses_snapshot_diff_and_truncates_each_list(self):
        repo = self.repo({"apps/api/service.py": "keep1\nkeep2\nold\n", "apps/api/other.py": "x\n",
                          "packages/ui/button.ts": "old\n", "package.json": "{}\n",
                          "pnpm-lock.yaml": "lock\n", "gone-a.txt": "a\n",
                          "gone-b.txt": "b\n", "gone-c.txt": "c\n"})
        self.env["DELEGATE_SHAPE_LIMIT"] = "2"
        self.fake_pi([answer("done"), SETTLED], pre=(
            "printf 'keep1\\nkeep2\\none\\ntwo\\nthree\\n' > apps/api/service.py; "
            "printf 'one\\ntwo\\n' > apps/api/other.py; "
            "printf 'x\\ny\\nz\\nq\\n' > packages/ui/button.ts; "
            "echo x > package.json; echo y > pnpm-lock.yaml; "
            "rm gone-a.txt gone-b.txt gone-c.txt; "
            "mkdir -p .github/workflows; echo ci > .github/workflows/check.yml"))
        result = self.cli("run", "--workdir", repo, "task")
        state = self.outcome(result)
        shape = state["shape"]
        self.assertEqual(len(shape["dirs"]), 2)
        self.assertGreaterEqual(shape["dirsMore"], 1)
        self.assertEqual(shape["largest"], [
            {"path": "apps/api/service.py", "lines": 5},
            {"path": "packages/ui/button.ts", "lines": 4}])
        self.assertEqual(shape["largestMore"], 4)
        self.assertEqual(shape["config"], [".github/workflows/check.yml", "package.json"])
        self.assertEqual(shape["configMore"], 1)
        self.assertEqual(shape["removed"], ["gone-a.txt", "gone-b.txt"])
        self.assertEqual(shape["removedMore"], 1)
        self.assertIn("apps/api +5 -2", result.stdout)
        self.assertIn("largest after: apps/api/service.py (5 lines)", result.stdout)
        self.assertLess(result.stdout.index("===== changes"), result.stdout.index("===== shape"))
        self.assertLess(result.stdout.index("===== shape"), result.stdout.index("===== result"))
        self.assertEqual(json.loads((Path(state["dir"]) / "summary.json").read_text())["shape"], shape)

    def test_read_only_shape_is_omitted_even_when_workspace_changes(self):
        repo = self.repo({"a.txt": "a\n"})
        self.fake_pi([answer("done"), SETTLED], pre="echo changed >> a.txt")
        result = self.cli("run", "--read-only", "--workdir", repo, "task")
        self.assertNotIn("shape", self.outcome(result))
        self.assertNotIn("===== shape =====", result.stdout)

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
        self.assertIn("1 个文件有冲突标记待解决: a.txt", merged.stderr)
        self.assertNotIn("apply failed", merged.stderr)
        self.assertTrue((repo / "seen.txt").exists())
        self.assertTrue((Path(state["dir"]) / ".applied").exists())  # markers landed every change
        self.assertNotIn("never applied", self.cli("clean", state["run"]).stdout)
        self.assertFalse(tree.exists())
        self.assertNotIn(str(tree), subprocess.run(["git", "-C", str(repo), "worktree", "list"],
                                                   capture_output=True, text=True).stdout)

    def test_worktree_copy_includes_ignored_files_and_directories_and_skips_missing(self):
        repo = self.repo({"a.txt": "a\n", ".gitignore": ".local/\n.env\n"})
        (repo / ".local/scan").mkdir(parents=True)
        (repo / ".local/scan/material.txt").write_text("prepared\n")
        (repo / ".env").write_text("TOKEN=local\n")
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {
            "copy": [".local/scan", ".env", ".local/missing"]}}))
        self.fake_pi([answer("done"), SETTLED], pre="test -f .local/scan/material.txt && test -f .env")
        result = self.cli("run", "--worktree", "--workdir", repo, "task")
        state = self.outcome(result)
        tree = Path(state["worktree"])
        self.assertEqual((tree / ".local/scan/material.txt").read_text(), "prepared\n")
        self.assertEqual((tree / ".env").read_text(), "TOKEN=local\n")
        (tree / ".local/scan/material.txt").write_text("colleague\n")
        self.assertEqual((repo / ".local/scan/material.txt").read_text(), "prepared\n")
        self.assertEqual(state["state"], "answered")
        self.assertEqual(state.get("files", []), [])
        self.assertIn(".local/missing", result.stderr)
        log = (Path(state["dir"]) / "supervisor.log").read_text()
        self.assertIn("worktree source missing, skipping", log)
        self.assertIn(".local/missing", log)

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
        result = self.cli("reply", "--wait", first["run"], "one more thing")
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
        third = self.outcome(self.cli("reply", "--over-limit", "reply mechanics", "--wait", first["run"], "and finally"))  # the conversation's latest run
        self.assertEqual(third["parent"], second["run"])
        self.assertEqual(self.cli("apply", first["run"]).returncode, 0)
        self.assertEqual((repo / "a.txt").read_text(), "a\nmore\nagain\nlast\n")
        self.assertTrue(all((Path(r["dir"]) / ".applied").exists() for r in (first, second, third)))
        self.fake_codex(codex_events("ok"))
        codex = self.outcome(self.cli("run", "--agent", "codex", "--read-only", "look"))
        self.assertEqual(self.outcome(self.cli("reply", "--wait", codex["run"], "and?"))["state"], "answered")
        call = (self.work / "pi.log.codex").read_text().splitlines()[-1]
        self.assertIn("exec fork t1 --json", call)
        self.assertNotIn(" -C ", call)

    def test_reply_returns_immediately_unless_wait_is_requested(self):
        self.fake_pi([answer("first"), SETTLED])
        first = self.outcome(self.cli("run", "task"))
        self.fake_pi([answer("later"), SETTLED], sleep=2)
        launched = self.cli("reply", first["run"], "more")
        self.assertEqual(launched.returncode, 0, launched.stderr)
        state = self.outcome(launched)
        self.assertEqual(state["state"], "running")
        self.assertNotIn("later", launched.stdout)
        self.assertFalse((Path(state["dir"]) / "exit_code").exists())
        self.assertIn("later", self.cli("wait", state["run"]).stdout)
        self.fake_pi([answer("waited"), SETTLED])
        waited = self.cli("reply", "--wait", state["run"], "one more")
        self.assertEqual(self.outcome(waited)["state"], "answered")
        self.assertIn("waited", waited.stdout)
        self.assertEqual(self.cli("reply", "--max", "1s", state["run"], "invalid").returncode, 2)

    def test_a_conversation_name_refers_to_its_newest_reply(self):
        self.fake_pi([answer("first"), SETTLED])
        first = self.outcome(self.cli("run", "--name", "conv", "task"))
        self.fake_pi([answer("second"), SETTLED])
        second = self.outcome(self.cli("reply", "--wait", "conv", "more"))
        self.assertEqual(self.outcome(self.cli("status", "conv"))["run"], second["run"])
        self.assertIn("second", self.cli("result", "conv").stdout)
        self.assertEqual(self.outcome(self.cli("status", first["run"]))["run"], first["run"])  # a run id stays exact

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
        second = self.outcome(self.cli("reply", "--wait", first["run"], "more"))
        self.cli("clean", first["run"])
        self.assertEqual(self.outcome(self.cli("reply", "--wait", second["run"], "again"))["state"], "answered")
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
        self.until(lambda: not delegate_pid_alive(agent), "the orphaned agent to go")  # not a fixed sleep: load varies
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

    def test_read_only_snapshot_keeps_uncommitted_submodule_pointers(self):
        # The caller has moved a submodule forward without committing the pointer yet: a reader must see that,
        # and must not be blamed for it (its worktree's index keeps the snapshot, only HEAD is the caller's).
        repo = self.repo({"a.txt": "a\n"})
        sub = self.work / "lib"
        git = ["-c", "user.name=t", "-c", "user.email=t@t", "-c", "protocol.file.allow=always"]
        subprocess.run(["git", "init", "-q", str(sub)], check=True)
        (sub / "lib.txt").write_text("v1\n")
        subprocess.run(["git", "-C", str(sub), *git, "add", "-A"], check=True)
        subprocess.run(["git", "-C", str(sub), *git, "commit", "-qm", "v1"], check=True)
        subprocess.run(["git", "-C", str(repo), *git, "submodule", "add", "-q", str(sub), "vendor"], check=True,
                       capture_output=True)
        subprocess.run(["git", "-C", str(repo), *git, "commit", "-qm", "vendor"], check=True)
        (repo / "vendor/lib.txt").write_text("v2\n")
        subprocess.run(["git", "-C", str(repo / "vendor"), *git, "commit", "-qam", "v2"], check=True)
        self.fake_codex(codex_events("looked"))
        state = self.outcome(self.cli("run", "--agent", "codex", "--read-only", "--workdir", repo, "review"))
        self.assertEqual(state["state"], "answered")
        for key in ("readOnlyViolation", "workspaceChanged", "warning"):
            self.assertNotIn(key, state)
        staged = subprocess.run(["git", "-C", state["worktree"], "diff", "--cached", "--name-only", "HEAD"],
                                capture_output=True, text=True).stdout.split()
        self.assertIn("vendor", staged)  # the new pointer shows against the caller's HEAD

    def sub_repo(self):
        """A repository with a committed submodule `vendor` (a worktree starts with it uninitialized)."""
        repo = self.repo({"a.txt": "a\n"})
        sub = self.work / "lib"
        git = ["-c", "user.name=t", "-c", "user.email=t@t", "-c", "protocol.file.allow=always"]
        subprocess.run(["git", "init", "-q", str(sub)], check=True)
        (sub / "lib.txt").write_text("v1\n")
        subprocess.run(["git", "-C", str(sub), *git, "add", "-A"], check=True)
        subprocess.run(["git", "-C", str(sub), *git, "commit", "-qm", "v1"], check=True)
        subprocess.run(["git", "-C", str(repo), *git, "submodule", "add", "-q", sub.as_uri(), "vendor"], check=True,
                       capture_output=True)
        subprocess.run(["git", "-C", str(repo), *git, "commit", "-qm", "vendor"], check=True)
        return repo

    def test_initializing_a_submodule_to_run_tests_is_not_a_change(self):
        repo = self.sub_repo()
        init = "git -c protocol.file.allow=always submodule update --init -q vendor"
        self.fake_pi([answer("done"), SETTLED], pre=f"{init}; echo more >> a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.assertEqual(state["files"], ["a.txt"])  # the checkout it needed for tests is not its work
        applied = self.cli("apply", state["run"])
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertEqual((repo / "a.txt").read_text(), "a\nmore\n")
        # Editing inside the submodule it initialized is still a change.
        self.fake_pi([answer("done"), SETTLED], pre=f"{init}; echo v2 >> vendor/lib.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.assertEqual(state["files"], ["vendor"])

    def test_protected_paths_reject_a_run_that_touches_them(self):
        repo = self.repo({"a.txt": "a\n", "tests/t.txt": "t\n", "docs/spec.md": "s\n"})
        self.fake_pi([answer("done"), SETTLED], pre="echo x >> a.txt; echo cheat >> tests/t.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--protect", "tests/",
                                      "--protect", "docs/spec.md", "--accept", "true", "task"))
        self.assertEqual((state["state"], state["protectViolation"]), ("rejected", ["tests/t.txt"]))
        self.assertNotIn("accept", state)  # acceptance never runs on a forbidden change
        self.assertIn("protect", (Path(state["dir"]) / "prompt.md").read_text().lower())  # it was told
        self.fake_pi([answer("done"), SETTLED], pre="echo x >> a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--protect", "tests/",
                                      "--accept", "true", "task"))
        self.assertEqual(state["state"], "delivered")
        self.assertNotIn("protectViolation", state)

    def test_after_runs_a_step_when_its_upstream_is_done(self):
        # A pipeline the caller declares: B starts when A ends, knows where A's answer is, and every result
        # still comes back to the caller. B waits in the kernel on A's lifetime lock, not in a polling loop.
        self.fake_pi([answer("scout: the callers are x.py and y.py"), SETTLED], sleep=1.5)
        a = self.outcome(self.cli("start", "--read-only", "--name", "scout", "list the callers"))
        self.env["DELEGATE_MAX_ACTIVE"] = "1"  # a step that is only waiting takes no slot
        started = self.cli("start", "--read-only", "--after", "scout", "--name", "impl", "use the list")
        self.assertEqual(started.returncode, 0, started.stderr)
        b = self.outcome(started)
        self.assertEqual(b["state"], "waiting")
        self.assertEqual(b["after"], a["run"])
        done = self.cli("wait", "impl", timeout=60)
        b = self.outcome(done)
        self.assertEqual(b["state"], "answered")
        prompt = (Path(b["dir"]) / "prompt.md").read_text()
        self.assertIn(str(Path(a["dir"]) / "result.md"), prompt)  # where the upstream answer is
        self.assertIn("scout", prompt)
        # An upstream that did not deliver skips the step without starting a colleague.
        del self.env["DELEGATE_MAX_ACTIVE"]
        self.fake_pi([answer(LEAKED), SETTLED])
        bad = self.outcome(self.cli("run", "--agent", "pi", "--read-only", "--retries", "0", "--name", "bad", "x"))
        self.assertEqual(bad["state"], "malformed")
        calls = len((self.work / "pi.log").read_text().splitlines())
        skipped = self.outcome(self.cli("run", "--read-only", "--after", "bad", "--name", "next", "y"))
        self.assertEqual(skipped["state"], "skipped")
        self.assertIn("finishedAt", skipped)
        self.assertIn("malformed", skipped["error"])
        self.assertEqual(len((self.work / "pi.log").read_text().splitlines()), calls)
        self.assertEqual(self.cli("start", "--after", "no-such-run", "x").returncode, 2)

    def test_in_reviews_an_upstream_worktree_without_touching_it(self):
        repo = self.repo({"a.txt": "a\n"})
        self.fake_pi([answer("changed"), SETTLED], pre="echo impl >> a.txt")
        impl = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "impl", "change a"))
        # The reviewer reads what impl produced, in a snapshot of its worktree, with impl's changes against HEAD.
        self.fake_pi([answer("reviewed"), SETTLED], pre='pwd > "$PI_LOG.cwd"; git diff HEAD > "$PI_LOG.diff"; '
                                                        'echo stray > stray.txt')
        review = self.outcome(self.cli("run", "--read-only", "--after", "impl", "--in", "impl", "--name", "rv",
                                       "review impl"))
        self.assertEqual(review["state"], "answered")
        self.assertNotEqual(review["worktree"], impl["worktree"])
        self.assertEqual((self.work / "pi.log.cwd").read_text().strip(), review["worktree"])
        self.assertIn("+impl", (self.work / "pi.log.diff").read_text())
        self.assertEqual(review["readOnlyViolation"], ["stray.txt"])
        self.assertFalse((Path(impl["worktree"]) / "stray.txt").exists())  # impl's work is untouched
        self.assertEqual(self.cli("apply", "impl").returncode, 0)
        self.assertFalse((repo / "stray.txt").exists())
        self.assertEqual(self.cli("run", "--in", "impl", "--name", "w", "write").returncode, 2)  # read-only only
        # Cleaning removes the reviewer's worktree from the repository's registry too, not just the directory,
        # even when the upstream it was taken from has been cleaned first.
        self.assertIn("removed", self.cli("clean", "impl").stdout)
        self.assertIn("removed", self.cli("clean", "rv").stdout)
        listed = subprocess.run(["git", "-C", str(repo), "worktree", "list", "--porcelain"], capture_output=True,
                                text=True).stdout
        self.assertNotIn(review["worktree"], listed)
        self.assertNotIn("prunable", listed)

    def test_in_ignores_linked_paths_of_the_upstream_worktree(self):
        # A submodule the upstream worktree links (.delegate.json) is not its change: a reviewer working --in it
        # must not see the link as a gitlink turned into a symlink, nor be blamed for it.
        repo = self.sub_repo()
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"link": ["vendor"]}}))
        self.fake_pi([answer("changed"), SETTLED], pre="echo impl >> a.txt")
        self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "impl", "change a"))
        self.fake_pi([answer("reviewed"), SETTLED], pre='git diff HEAD --name-only > "$PI_LOG.names"')
        review = self.outcome(self.cli("run", "--read-only", "--in", "impl", "--name", "rv", "review impl"))
        self.assertEqual(review["state"], "answered")
        self.assertEqual((self.work / "pi.log.names").read_text().split(), ["a.txt"])
        for key in ("readOnlyViolation", "warning"):
            self.assertNotIn(key, review)

    def test_reply_can_hand_the_worktree_to_the_other_colleague(self):
        repo = self.repo({"a.txt": "a\n"})
        self.fake_pi([answer("draft"), SETTLED], pre="echo draft >> a.txt")
        draft = self.outcome(self.cli("run", "--worktree", "--agent", "pi", "--workdir", repo, "--name", "d", "draft"))
        self.fake_codex(codex_events("polished"), pre="echo polish >> a.txt")
        polished = self.outcome(self.cli("reply", "--wait", "d", "--agent", "codex", "polish the draft"))
        self.assertEqual((polished["state"], polished["agent"], polished["worktree"]),
                         ("answered", "codex", draft["worktree"]))
        self.assertNotIn(" fork ", (self.work / "pi.log.codex").read_text())  # another model: a fresh session
        self.assertEqual(self.cli("apply", "d").returncode, 0)
        self.assertEqual((repo / "a.txt").read_text(), "a\ndraft\npolish\n")

    def test_apply_after_an_earlier_apply_merges_only_what_is_new(self):
        # Round one is applied (and committed); round two edits the very lines round one added. Applying the
        # conversation again must start from what was already applied, not from the conversation's start.
        repo = self.repo({"a.txt": "a\nb\nc\n", "b.txt": "x\n"})
        git = ["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t"]
        self.fake_pi([answer("one"), SETTLED], pre="sed -i s/b/round-one/ a.txt")
        first = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "conv", "one"))
        self.assertEqual(self.cli("apply", first["run"]).returncode, 0)
        subprocess.run([*git, "commit", "-qam", "round one"], check=True)
        (repo / "b.txt").write_text("x\ncaller\n")  # the caller keeps working elsewhere
        self.fake_pi([answer("two"), SETTLED], pre="sed -i s/round-one/round-two/ a.txt")
        self.outcome(self.cli("reply", "--wait", "conv", "two"))
        applied = self.cli("apply", "conv")
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertEqual((repo / "a.txt").read_text(), "a\nround-two\nc\n")
        self.assertEqual((repo / "b.txt").read_text(), "x\ncaller\n")
        self.assertIn("no changes to apply", self.cli("apply", "conv").stderr)  # applying twice is harmless

    def test_reply_sync_brings_the_callers_later_work_into_the_worktree(self):
        # The caller wrote a test after the run started; `reply --sync` lets the colleague see it, and the
        # caller's own work is not merged back twice.
        repo = self.repo({"a.txt": "a\n", "b.txt": "b\n"})
        self.fake_pi([answer("one"), SETTLED], pre="echo agent >> a.txt")
        self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "conv", "one"))
        (repo / "tests").mkdir()
        (repo / "tests/new.txt").write_text("new test\n")
        (repo / "b.txt").write_text("b\ncaller\n")
        self.fake_pi([answer("two"), SETTLED], pre="cat tests/new.txt b.txt > seen.txt")
        second = self.outcome(self.cli("reply", "--wait", "conv", "--sync", "use the new test"))
        self.assertEqual(second["state"], "answered")
        self.assertEqual((Path(second["worktree"]) / "seen.txt").read_text(), "new test\nb\ncaller\n")
        self.assertEqual(sorted(second["files"]), ["seen.txt"])  # synced files are the caller's, not its work
        applied = self.cli("apply", "conv")
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertEqual((repo / "a.txt").read_text(), "a\nagent\n")
        self.assertEqual((repo / "b.txt").read_text(), "b\ncaller\n")
        self.assertTrue((repo / "seen.txt").exists())
        # A sync that would conflict stops before the colleague starts, leaving the worktree as it was.
        self.fake_pi([answer("three"), SETTLED], pre="echo agent2 >> a.txt")
        self.outcome(self.cli("reply", "--over-limit", "reply mechanics", "--wait", "conv", "three"))
        (repo / "a.txt").write_text("a\nagent\ncaller-too\n")
        calls = len((self.work / "pi.log").read_text().splitlines())
        refused = self.cli("reply", "--over-limit", "reply mechanics", "conv", "--sync", "four")
        self.assertEqual(refused.returncode, 2)
        self.assertIn("conflict", refused.stderr)
        self.assertIn("a.txt", refused.stderr)
        self.assertEqual(len((self.work / "pi.log").read_text().splitlines()), calls)

    def test_reply_sync_ignores_unchanged_gitlinks_but_rejects_pointer_changes(self):
        repo = self.sub_repo()
        init = "git -c protocol.file.allow=always submodule update --init -q vendor"
        self.fake_pi([answer("one"), SETTLED], pre=init)
        self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "conv", "one"))
        (repo / "a.txt").write_text("a\ncaller\n")
        self.fake_pi([answer("two"), SETTLED], pre="test -f vendor/lib.txt && cat a.txt > seen.txt")
        second = self.outcome(self.cli("reply", "--wait", "conv", "--sync", "two"))
        self.assertEqual(second["state"], "answered")
        self.assertEqual((Path(second["worktree"]) / "seen.txt").read_text(), "a\ncaller\n")
        self.assertEqual(json.loads((Path(second["dir"]) / "sync.json").read_text())["files"], ["a.txt"])
        vendor = repo / "vendor"
        (vendor / "lib.txt").write_text("v2\n")
        subprocess.run(["git", "-C", str(vendor), "add", "lib.txt"], check=True)
        subprocess.run(["git", "-C", str(vendor), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "-qm", "v2"], check=True)
        calls = len((self.work / "pi.log").read_text().splitlines())
        refused = self.cli("reply", "--over-limit", "reply mechanics", "conv", "--sync", "three")
        self.assertEqual(refused.returncode, 2)
        self.assertIn("--sync conflict in: vendor", refused.stderr)
        self.assertEqual(len((self.work / "pi.log").read_text().splitlines()), calls)

    def test_wait_machine_collects_runs_from_every_repository(self):
        del self.env["DELEGATE_RUNS"]  # records live under each repository, as in real use
        self.fake_pi([answer("done"), SETTLED], sleep=1)
        names = []
        for name in ("alpha", "beta"):
            repo = self.work / name
            repo.mkdir()
            subprocess.run(["git", "init", "-q", str(repo)], check=True)
            self.assertEqual(self.cli("start", "--read-only", "--name", name, "task", cwd=repo).returncode, 0)
            names.append(name)
        elsewhere = self.work / "elsewhere"
        elsewhere.mkdir()
        self.assertIn("no active or undelivered runs", self.cli("wait", cwd=elsewhere).stderr)  # local only
        result = self.cli("wait", "--machine", cwd=elsewhere, timeout=60)
        self.assertEqual(result.returncode, 0, result.stderr)
        got = sorted(json.loads(l)["name"] for l in result.stdout.splitlines() if l.startswith('{"run"'))
        self.assertEqual(got, names)

    def test_wait_by_run_id_from_another_directory_finds_an_active_run(self):
        del self.env["DELEGATE_RUNS"]
        self.fake_pi([answer("done"), SETTLED], sleep=1)
        repo = self.work / "alpha"
        repo.mkdir()
        subprocess.run(["git", "init", "-q", str(repo)], check=True)
        started = self.cli("start", "--read-only", "--name", "alpha", "task", cwd=repo)
        self.assertEqual(started.returncode, 0, started.stderr)
        run_id = json.loads(started.stdout.splitlines()[0])["run"]
        elsewhere = self.work / "elsewhere"
        elsewhere.mkdir()
        result = self.cli("wait", run_id, cwd=elsewhere, timeout=60)  # the id printed in `next`
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout.splitlines()[0])["run"], run_id)

    def test_generated_files_are_regenerated_after_apply_not_merged(self):
        repo = self.repo({"src.txt": "1\n2\n3\n", "gen/out.txt": "1\n2\n3\n"})
        (repo / ".delegate.json").write_text(json.dumps({"generated": {"paths": ["gen/"],
                                                                       "command": "cp src.txt gen/out.txt"}}))
        self.fake_pi([answer("done"), SETTLED], pre="sed -i s/3/three/ src.txt; cp src.txt gen/out.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        # Meanwhile the caller changes the same source elsewhere and regenerates: gen/ now differs on both sides.
        (repo / "src.txt").write_text("one\n2\n3\n")
        (repo / "gen/out.txt").write_text("one\n2\n3\n")
        applied = self.cli("apply", state["run"])
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertEqual((repo / "src.txt").read_text(), "one\n2\nthree\n")
        self.assertEqual((repo / "gen/out.txt").read_text(), "one\n2\nthree\n")  # regenerated from the merge
        self.assertIn("regenerated", applied.stdout + applied.stderr)

    def test_generated_inputs_skip_regeneration_for_unrelated_changes(self):
        repo = self.repo({"src.txt": "1\n", "docs/a.md": "a\n", "gen/out.txt": "1\n"})
        (repo / ".delegate.json").write_text(json.dumps({"generated": {
            "paths": ["gen/"], "inputs": ["src.txt"], "command": "cp src.txt gen/out.txt; touch ran"}}))
        self.fake_pi([answer("done"), SETTLED], pre="echo b > docs/a.md")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        applied = self.cli("apply", state["run"])
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertEqual((repo / "docs/a.md").read_text(), "b\n")
        self.assertFalse((repo / "ran").exists())
        self.assertIn("not regenerated", applied.stdout)
        self.assertFalse((Path(state["dir"]) / ".generate-pending").exists())
        self.fake_pi([answer("done"), SETTLED], pre="echo 2 > src.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        applied = self.cli("apply", state["run"])
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertTrue((repo / "ran").exists())
        self.assertEqual((repo / "gen/out.txt").read_text(), "2\n")

    def test_apply_removes_directories_left_empty_by_deletions(self):
        repo = self.repo({"old/pkg/a.txt": "a\n", "old/keep.txt": "k\n", "src.txt": "1\n"})
        self.fake_pi([answer("done"), SETTLED], pre="rm -r old/pkg")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        applied = self.cli("apply", state["run"])
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertFalse((repo / "old/pkg").exists())
        self.assertTrue((repo / "old/keep.txt").exists())

    def test_apply_delete_failure_keeps_worktree_unapplied(self):
        repo = self.repo({"locked/old.txt": "old\n"})
        self.fake_pi([answer("done"), SETTLED], pre="rm locked/old.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        directory = repo / "locked"
        directory.chmod(0o555)
        self.addCleanup(directory.chmod, 0o755)
        if os.access(directory, os.W_OK):
            self.skipTest("chmod cannot deny writes for this user")
        failed = self.cli("apply", state["run"])
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("locked/old.txt", failed.stderr)
        self.assertIn("Permission denied", failed.stderr)
        run = Path(state["dir"])
        self.assertFalse((run / ".applied").exists())
        self.assertFalse((run / ".sync-base").exists())
        self.assertTrue(Path(state["worktree"]).exists())
        self.cli("clean", "--finished")
        self.assertTrue(run.exists())
        self.assertTrue(Path(state["worktree"]).exists())

    def test_generation_timeout_fails_apply_and_releases_lane(self):
        repo = self.repo({"src.txt": "old\n", "gen/out.txt": "old\n"})
        (repo / ".delegate.json").write_text(json.dumps({"generated": {
            "paths": ["gen/"], "command": "sleep 30; cp src.txt gen/out.txt"}}))
        self.fake_pi([answer("done"), SETTLED], pre="echo new > src.txt; cp src.txt gen/out.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.env["DELEGATE_GENERATE_TIMEOUT"] = "0.2s"
        failed = self.cli("apply", state["run"], timeout=15)
        self.assertEqual(failed.returncode, 1, failed.stderr)
        self.assertIn("timed out", failed.stderr)
        run = Path(state["dir"])
        self.assertFalse((run / ".applied").exists())
        self.assertFalse((run / ".sync-base").exists())
        self.assertEqual((repo / "src.txt").read_text(), "new\n")
        started = time.monotonic()
        lane = self.cli("lane", "true", timeout=5)
        self.assertEqual(lane.returncode, 0, lane.stderr)
        self.assertLess(time.monotonic() - started, 3)
        self.assertTrue((run / ".generate-pending").exists())
        pending = self.outcome(self.cli("status", state["run"]))
        self.assertNotIn("applied", pending)
        self.assertIn("apply", pending["next"])
        # Zero file actions must still retry generation and advance the merge marker.
        (repo / ".delegate.json").write_text(json.dumps({"generated": {
            "paths": ["gen/"], "command": "cp src.txt gen/out.txt; echo retried > retry.txt"}}))
        retried = self.cli("apply", state["run"])
        self.assertEqual(retried.returncode, 0, retried.stderr)
        self.assertEqual((repo / "retry.txt").read_text(), "retried\n")
        self.assertTrue((run / ".applied").exists())
        self.assertFalse((run / ".generate-pending").exists())

    def test_apply_progress_is_visible_while_queueing_and_generating(self):
        repo = self.repo({"src.txt": "old\n", "gen/out.txt": "old\n"})
        gate = self.work / "generate-release"
        (repo / ".delegate.json").write_text(json.dumps({"generated": {
            "paths": ["gen/"], "command": f"while [ ! -f {gate} ]; do sleep .05; done; cp src.txt gen/out.txt"}}))
        self.fake_pi([answer("done"), SETTLED], pre="echo new > src.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        holder = self.hold_lane(30)
        process = subprocess.Popen([str(DELEGATE), "apply", state["run"]], env=self.env,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.addCleanup(process.kill)
        self.addCleanup(process.stdout.close)
        self.addCleanup(process.stderr.close)
        lines = []
        thread = threading.Thread(target=lambda: lines.extend(process.stderr), daemon=True)
        thread.start()
        self.until(lambda: any("generator queued" in line for line in lines), "queue progress")
        self.assertIsNone(process.poll())
        self.assertIn("caller check", "".join(lines))
        holder.terminate()
        holder.wait(timeout=10)
        self.until(lambda: any("generating /" in line for line in lines), "generation progress")
        self.assertIsNone(process.poll())
        self.assertIn(str(Path(state["dir"]) / "generate.log"), "".join(lines))
        gate.touch()
        out = process.stdout.read()
        process.wait(timeout=10)
        thread.join(timeout=2)
        self.assertEqual(process.returncode, 0, "".join(lines) + out)

    def test_protect_reasons_normalize_validate_and_inherit(self):
        repo = self.repo({"tests/t.txt": "old\n"})
        self.fake_pi([answer("done"), SETTLED])
        reason = "owned elsewhere = keep logic here"
        first = self.outcome(self.cli("run", "--worktree", "--workdir", repo,
                                     "--protect-reason", "tests//", reason,
                                     "--protect-reason", "tests/", reason, "task"))
        meta = json.loads((Path(first["dir"]) / "meta.json").read_text())
        self.assertEqual(meta["protect"], ["tests/"])
        self.assertEqual(meta["protectReasons"], {"tests/": reason})
        for fresh, prompt in ((False, "more"), (True, "继续")):
            args = ["reply", "--over-limit", "reply mechanics", "--wait", first["run"], prompt] + (["--fresh"] if fresh else [])
            result = self.outcome(self.cli(*args))
            text = (Path(result["dir"]) / "prompt.md").read_text()
            self.assertIn(reason, text)
            self.assertIn("不得为避开保护" if fresh else "Do not move or copy logic", text)
            self.assertEqual(json.loads((Path(result["dir"]) / "meta.json").read_text())["protectReasons"], meta["protectReasons"])
            first = result
        for args in (("tests/", ""), ("../tests", "why")):
            self.assertEqual(self.cli("start", "--workdir", repo, "--protect-reason", *args, "task").returncode, 2)
        self.assertEqual(self.cli("start", "--workdir", repo, "--protect-reason", "tests//", "a",
                                 "--protect-reason", "tests/", "b", "task").returncode, 2)
        self.assertEqual(self.cli("reply", first["run"], "--protect-reason", "tests/", "new", "task").returncode, 2)
        self.fake_pi([answer("done"), SETTLED], pre="echo bad >> tests/t.txt")
        rejected = self.outcome(self.cli("reply", "--over-limit", "reply mechanics", "--wait", first["run"], "--accept", "echo ran > accepted", "fix"))
        self.assertEqual(rejected["state"], "rejected")
        self.assertEqual(rejected["protectViolationReasons"], {"tests/": reason})
        self.assertNotIn("accept", rejected)
        self.assertFalse((Path(rejected["worktree"]) / "accepted").exists())
        # Old meta has only the existing protect array; reply must still inherit it.
        meta_path = Path(rejected["dir"]) / "meta.json"
        legacy = json.loads(meta_path.read_text())
        legacy.pop("protectReasons")
        meta_path.write_text(json.dumps(legacy))
        self.fake_pi([answer("legacy"), SETTLED], pre="echo old > tests/t.txt")
        inherited = self.outcome(self.cli("reply", "--over-limit", "reply mechanics", "--wait", rejected["run"], "--no-accept", "restore"))
        inherited_meta = json.loads((Path(inherited["dir"]) / "meta.json").read_text())
        self.assertEqual(inherited_meta["protect"], ["tests/"])
        self.assertNotIn("protectReasons", inherited_meta)
        self.fake_pi([answer("equal name"), SETTLED])
        equals = self.outcome(self.cli("run", "--worktree", "--workdir", repo,
                                      "--protect", "name=literal", "--protect-reason", "name=literal", reason, "task"))
        equals_meta = json.loads((Path(equals["dir"]) / "meta.json").read_text())
        self.assertEqual(equals_meta["protect"], ["name=literal"])
        self.assertEqual(equals_meta["protectReasons"], {"name=literal": reason})

    def test_zero_change_reply_reports_pending_total_changes(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi([answer("first"), SETTLED], pre="echo new > a.txt")
        first = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.fake_pi([answer("checked"), SETTLED])
        second = self.outcome(self.cli("reply", "--wait", first["run"], "--accept", "true", "check"))
        self.assertEqual(second["changes"]["files"], 0)
        self.assertEqual(second["pendingChanges"]["files"], 1)
        self.assertIn("--total", second["next"])
        self.assertIn("apply", second["next"])

    def test_numbered_prefix_conflicts_across_runs_and_dry_run(self):
        repo = self.repo({"migrations/base.sql": "base\n"})
        runs = []
        for suffix in ("a", "b"):
            self.fake_pi([answer("done"), SETTLED], pre=f"echo sql > migrations/0032_{suffix}.sql")
            runs.append(self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task")))
        self.assertNotIn("numberedPrefixConflicts", self.outcome(self.cli("apply", runs[0]["run"])))
        dry = self.cli("apply", runs[1]["run"], "--dry-run")
        expected = [{"directory": "migrations", "prefix": "0032",
                     "paths": ["migrations/0032_a.sql", "migrations/0032_b.sql"]}]
        self.assertEqual(self.outcome(dry)["numberedPrefixConflicts"], expected)
        self.assertIn("numbered prefix warning", dry.stderr)
        self.assertFalse((repo / "migrations/0032_b.sql").exists())
        applied = self.cli("apply", runs[1]["run"])
        self.assertEqual(applied.returncode, 0, applied.stderr)
        self.assertEqual(self.outcome(applied)["numberedPrefixConflicts"], expected)
        self.assertTrue((repo / "migrations/0032_a.sql").exists())
        self.assertTrue((repo / "migrations/0032_b.sql").exists())
        self.assertNotIn("numberedPrefixConflicts", self.outcome(self.cli("apply", runs[1]["run"])))

    def test_numbered_prefix_final_plan_excludes_deleted_and_generated(self):
        repo = self.repo({"m/001_old.sql": "old\n", "m/base": "base\n", "gen/001_old": "old\n"})
        (repo / ".delegate.json").write_text(json.dumps({"generated": {"paths": ["gen/"], "command": "true"}}))
        self.fake_pi([answer("done"), SETTLED], pre="rm m/001_old.sql; echo new > m/001_new.sql; "
                     "echo a > m/002_a.sql; echo b > m/002_b.sql; echo c > m/003_new.sql; "
                     "mkdir elsewhere; echo d > elsewhere/002_other.sql; echo z > m/2_short.sql; "
                     "echo normal > m/plain; echo generated > gen/001_new")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        (repo / "m/003_untracked.sql").write_text("untracked\n")
        result = self.outcome(self.cli("apply", state["run"]))
        self.assertEqual([item["prefix"] for item in result["numberedPrefixConflicts"]], ["002", "003"])
        self.assertEqual(result["numberedPrefixConflicts"][1]["paths"], ["m/003_new.sql", "m/003_untracked.sql"])
        self.assertFalse((repo / "m/001_old.sql").exists())
        self.assertFalse((repo / "gen/001_new").exists())

    def test_apply_acceptance_reuse_checks_full_tree_without_rerunning(self):
        repo = self.repo({"a.txt": "old\n", "unrelated.txt": "original\n"})
        # The baseline includes uncommitted files and is deliberately different from HEAD.
        (repo / "dirty.txt").write_text("dirty\n")
        counter = self.work / "accept-count"
        self.fake_pi([answer("done"), SETTLED], pre="echo new > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo,
                                     "--accept", f"echo checked >> {counter}", "task"))
        result = self.outcome(self.cli("apply", state["run"]))
        self.assertIs(result["acceptStillValid"], True)
        self.assertEqual(result["acceptValidityScope"], "repository-snapshot")
        self.assertEqual(result["apply"], {"ok": True, "dryRun": False})
        self.assertIs(self.outcome(self.cli("apply", state["run"]))["acceptStillValid"], True)
        (repo / "unrelated.txt").write_text("caller edit\n")
        self.assertIs(self.outcome(self.cli("apply", state["run"]))["acceptStillValid"], False)
        self.assertEqual(counter.read_text(), "checked\n")
        dry = self.outcome(self.cli("apply", state["run"], "--dry-run"))
        self.assertNotIn("acceptStillValid", dry)
        self.assertIn("dry run", dry["acceptValidityReason"])

    def test_apply_acceptance_reuse_rejects_post_accept_and_accept_edits(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi([answer("done"), SETTLED], pre="echo new > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        (Path(state["worktree"]) / "a.txt").write_text("manual\n")
        self.assertIs(self.outcome(self.cli("apply", state["run"]))["acceptStillValid"], False)
        self.fake_pi([answer("done"), SETTLED], pre="echo again > a.txt")
        changed = self.outcome(self.cli("run", "--worktree", "--workdir", repo,
                                       "--accept", "echo accept-edit >> a.txt", "task"))
        result = self.outcome(self.cli("apply", changed["run"]))
        self.assertIs(result["acceptStillValid"], False)
        self.assertIn("acceptance changed", result["acceptValidityReason"])

    def test_apply_acceptance_reuse_generated_result_and_conflict_markers(self):
        repo = self.repo({"a.txt": "old\n", "gen/out.txt": "old\n"})
        config = {"generated": {"paths": ["gen/"], "command": "cp a.txt gen/out.txt"}}
        (repo / ".delegate.json").write_text(json.dumps(config))
        self.fake_pi([answer("done"), SETTLED], pre="echo new > a.txt; cp a.txt gen/out.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        self.assertIs(self.outcome(self.cli("apply", state["run"]))["acceptStillValid"], True)
        self.fake_pi([answer("done"), SETTLED], pre="echo next > a.txt; echo different > gen/out.txt")
        different = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        self.assertIs(self.outcome(self.cli("apply", different["run"]))["acceptStillValid"], False)
        self.fake_pi([answer("done"), SETTLED], pre="echo theirs > a.txt")
        conflict = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        (repo / "a.txt").write_text("mine\n")
        marked = self.cli("apply", conflict["run"], "--merge")
        self.assertEqual(marked.returncode, 1, marked.stderr)
        self.assertIs(self.outcome(marked)["acceptStillValid"], False)
        self.assertTrue((Path(conflict["dir"]) / ".applied").exists())

    def test_apply_acceptance_reuse_omits_incomplete_legacy_and_failed_snapshots(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi([answer("done"), SETTLED], pre="echo new > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        run = Path(state["dir"])
        summary = json.loads((run / "summary.json").read_text())
        legacy = dict(summary)
        legacy["accept"] = {"ok": True, "exitCode": 0, "command": "true"}
        (run / "summary.json").write_text(json.dumps(legacy))
        result = self.outcome(self.cli("apply", state["run"]))
        self.assertNotIn("acceptStillValid", result)
        self.assertIn("incomplete acceptance", result["acceptValidityReason"])
        (run / "summary.json").write_text(json.dumps(summary))
        (repo / "large-untracked").write_bytes(b"x" * 2097153)
        result = self.outcome(self.cli("apply", state["run"]))
        self.assertNotIn("acceptStillValid", result)
        self.assertIn("large untracked", result["acceptValidityReason"])
        (repo / "large-untracked").unlink()
        wrapper = self.bin / "git"
        real_git = shutil.which("git", path=os.environ["PATH"])
        wrapper.write_text(f'#!/bin/sh\ncase "$*" in *"write-tree"*) exit 1;; esac\nexec {shlex.quote(real_git)} "$@"\n')
        wrapper.chmod(0o755)
        result = self.outcome(self.cli("apply", state["run"]))
        self.assertNotIn("acceptStillValid", result)
        self.assertIn("snapshot failed", result["acceptValidityReason"])

    def test_apply_verify_runs_repository_acceptance_on_merged_tree(self):
        repo = self.repo({"a.txt": "old\n", "b.txt": "old\n"})
        counter = self.work / "verify-count"
        (repo / ".delegate.json").write_text(json.dumps(
            {"accept": f"echo checked >> {counter}; ! grep -q broken b.txt", "applyVerify": True}))
        self.fake_pi([answer("done"), SETTLED], pre="echo new > a.txt")
        still = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        result = self.outcome(self.cli("apply", still["run"]))
        self.assertIs(result["acceptStillValid"], True)
        self.assertEqual(result["verify"], {"skipped": "acceptStillValid"})
        self.assertEqual(counter.read_text(), "checked\n")
        self.fake_pi([answer("done"), SETTLED], pre="echo next > a.txt")
        drifted = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        (repo / "b.txt").write_text("broken\n")
        failed = self.cli("apply", drifted["run"])
        self.assertEqual(failed.returncode, 1, failed.stderr)
        outcome = self.outcome(failed)
        self.assertTrue(outcome["apply"]["ok"])
        self.assertIs(outcome["verify"]["ok"], False)
        self.assertEqual((repo / "a.txt").read_text(), "next\n")
        self.assertIn("verify failed", failed.stderr)
        (repo / "b.txt").write_text("fixed\n")
        skipped = self.outcome(self.cli("apply", drifted["run"], "--no-verify"))
        self.assertNotIn("verify", skipped)
        forced = self.cli("apply", drifted["run"], "--verify")
        self.assertEqual(forced.returncode, 0, forced.stderr)
        self.assertIs(self.outcome(forced)["verify"]["ok"], True)
        self.assertNotIn("verify", self.outcome(self.cli("apply", drifted["run"], "--dry-run", "--verify")))

    def test_apply_verify_is_opt_in_and_validates_config(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi([answer("done"), SETTLED], pre="echo new > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.assertNotIn("verify", self.outcome(self.cli("apply", state["run"])))
        (repo / ".delegate.json").write_text(json.dumps({"applyVerify": "echo own-command"}))
        result = self.outcome(self.cli("apply", state["run"]))
        self.assertEqual(result["verify"]["command"], "echo own-command")
        self.assertIs(result["verify"]["ok"], True)
        (repo / ".delegate.json").write_text(json.dumps({"applyVerify": 3}))
        self.assertIn("applyVerify must be", self.cli("apply", state["run"]).stderr)
        self.assertEqual(self.cli("apply", state["run"], "--verify", "--no-verify").returncode, 2)

    def test_write_conversations_get_one_rework_before_the_caller_takes_over(self):
        repo = self.repo({"a.txt": "a\n"})
        self.fake_pi([answer("first"), SETTLED], pre="echo one >> a.txt")
        first = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "job", "task"))
        self.fake_pi([answer("asked"), SETTLED])
        question = self.outcome(self.cli("reply", "--wait", "job", "why?"))  # no changes: not rework
        self.assertEqual(json.loads((Path(question["dir"]) / "meta.json").read_text())["rework"]["kind"], "rework")
        self.fake_pi([answer("fixed"), SETTLED], pre="echo two >> a.txt")
        self.outcome(self.cli("reply", "--wait", "job", "fix it"))
        self.fake_pi([answer("tweak"), SETTLED], pre="echo three >> a.txt")
        minor = self.outcome(self.cli("reply", "--wait", "--minor", "job", "rename x"))
        self.assertEqual(json.loads((Path(minor["dir"]) / "meta.json").read_text())["timeout"], "10m")
        refused = self.cli("reply", "job", "rework again")
        self.assertEqual(refused.returncode, 2)
        self.assertIn("rework limit reached (1/1) for job", refused.stderr)
        self.assertIn("apply job", refused.stderr)
        self.assertIn("--minor is for short fixes", self.cli("reply", "--minor", "job", "x" * 601).stderr)
        self.fake_pi([answer("big"), SETTLED], pre="seq 1 80 >> a.txt")
        self.outcome(self.cli("reply", "--wait", "--minor", "job", "small?"))
        self.fake_pi([answer("ok"), SETTLED], pre="echo four >> a.txt")
        forced = self.outcome(self.cli("reply", "--wait", "--over-limit", "needs its context", "job", "again"))
        rework = json.loads((Path(forced["dir"]) / "meta.json").read_text())["rework"]
        self.assertEqual(rework, {"kind": "rework", "used": 3, "limit": 1, "overLimit": "needs its context"})
        (repo / ".delegate.json").write_text(json.dumps({"maxRework": None}))
        self.fake_pi([answer("free"), SETTLED])
        self.assertEqual(self.cli("reply", "--wait", "job", "unlimited").returncode, 0)
        (repo / ".delegate.json").write_text(json.dumps({"maxRework": "one"}))
        self.assertIn("maxRework must be", self.cli("reply", "job", "bad").stderr)
        # Invalid configuration refuses every start; restore it before the read-only run.
        (repo / ".delegate.json").write_text(json.dumps({"maxRework": 1}))
        self.fake_pi([answer("look"), SETTLED])
        reader = self.outcome(self.cli("run", "--read-only", "--workdir", repo, "--name", "look", "read"))
        for _ in range(3):
            self.fake_pi([answer("more"), SETTLED])
            self.assertEqual(self.cli("reply", "--wait", "look", "more").returncode, 0)
        self.assertIn("apply to write conversations", self.cli("reply", "--minor", reader["run"], "x").stderr)

    def test_start_reports_running_write_tasks_and_named_overlap(self):
        repo = self.repo({"src/hot_file.rs": "old\n", "other.txt": "old\n"})
        self.fake_pi([answer("done"), SETTLED], pre="echo new > src/hot_file.rs", sleep=20)
        first = self.outcome(self.cli("start", "--worktree", "--workdir", repo, "--name", "hotwork", "task"))
        edited = Path(first["worktree"]) / "src/hot_file.rs"
        deadline = time.time() + 10
        while edited.read_text() != "new\n" and time.time() < deadline:
            time.sleep(0.05)
        try:
            named = self.cli("start", "--worktree", "--workdir", repo, "--name", "second", "fix src/hot_file.rs")
            self.assertIn("overlap: write task hotwork is running on this source (1 files changed so far: src/hot_file.rs)",
                          named.stderr)
            self.assertIn("--after hotwork", named.stderr)
            unrelated = self.cli("start", "--worktree", "--workdir", repo, "--name", "third", "edit other.txt")
            self.assertIn("note: write task hotwork", unrelated.stderr)
            self.assertNotIn("overlap:", unrelated.stderr)
            reader = self.cli("start", "--read-only", "--workdir", repo, "look at hot_file.rs")
            self.assertNotIn("hotwork", reader.stderr)
        finally:
            self.cli("stop", "hotwork", "second", "third")

    def test_in_place_write_moves_to_worktree_while_another_write_runs(self):
        repo = self.repo({"a.txt": "old\n", "b.txt": "old\n"})
        self.fake_pi([answer("done"), SETTLED], pre="echo new > a.txt", sleep=20)
        first = self.outcome(self.cli("start", "--worktree", "--workdir", repo, "--name", "busy", "task"))
        try:
            moved = self.cli("start", "--workdir", repo, "--name", "later", "edit b.txt")
            self.assertIn("write task busy running on this source; this run uses a worktree", moved.stderr)
            self.assertTrue(self.outcome(moved)["worktree"])
            self.assertNotEqual(self.outcome(moved)["worktree"], str(repo))
        finally:
            self.cli("stop", "busy", "later")
        alone = self.cli("start", "--workdir", repo, "--name", "alone", "edit b.txt")
        try:
            self.assertNotIn("uses a worktree", alone.stderr)
            self.assertFalse(self.outcome(alone).get("worktree"))
        finally:
            self.cli("stop", "alone")

    def test_apply_warns_while_an_in_place_write_edits_the_source(self):
        repo = self.repo({"a.txt": "old\n", "b.txt": "old\n"})
        self.fake_pi([answer("done"), SETTLED], pre="echo new > a.txt")
        done = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.fake_pi([answer("done"), SETTLED], pre="echo new > b.txt", sleep=20)
        self.cli("start", "--workdir", repo, "--name", "inplace", "edit b.txt")
        try:
            applied = self.cli("apply", done["run"])
            self.assertIn("in-place write task inplace is editing this tree", applied.stderr)
            self.assertTrue(self.outcome(applied)["apply"]["ok"])
        finally:
            self.cli("stop", "inplace")
        quiet = self.cli("apply", "--dry-run", done["run"])
        self.assertNotIn("is editing this tree", quiet.stderr)

    def test_apply_acceptance_reuse_failed_and_absent_acceptance(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi([answer("done"), SETTLED], pre="echo new > a.txt")
        no_accept = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.assertNotIn("acceptStillValid", self.outcome(self.cli("apply", no_accept["run"])))
        self.fake_pi([answer("done"), SETTLED], pre="echo rejected > a.txt")
        failed = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "false", "task"))
        result = self.outcome(self.cli("apply", failed["run"]))
        self.assertIs(result["acceptStillValid"], False)
        self.assertTrue(result["apply"]["ok"])
        self.assertEqual(self.outcome(self.cli("status", failed["run"]))["state"], "rejected")

    def test_apply_acceptance_reuse_omits_failed_historical_snapshot(self):
        repo = self.repo({"a.txt": "old\n"})
        marker = self.work / "break-snapshot"
        real_git = shutil.which("git", path=os.environ["PATH"])
        wrapper = self.bin / "git"
        wrapper.write_text(f'#!/bin/sh\ncase "$*" in *"write-tree"*) [ -f {shlex.quote(str(marker))} ] && exit 1;; esac\nexec {shlex.quote(real_git)} "$@"\n')
        wrapper.chmod(0o755)
        self.fake_pi([answer("done"), SETTLED], pre="echo new > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo,
                                     "--accept", f"touch {shlex.quote(str(marker))}", "task"))
        self.assertTrue(state["accept"]["ok"])
        self.assertIn("snapshot failed", state["accept"]["snapshotReason"])
        marker.unlink()
        result = self.outcome(self.cli("apply", state["run"]))
        self.assertNotIn("acceptStillValid", result)
        self.assertIn("snapshot failed", result["acceptValidityReason"])

    def test_apply_acceptance_reuse_omits_dirty_submodule_and_index_flags(self):
        repo = self.repo({"a.txt": "old\n"})
        sub = self.work / "lib"
        git = ["-c", "user.name=t", "-c", "user.email=t@t", "-c", "protocol.file.allow=always"]
        subprocess.run(["git", "init", "-q", str(sub)], check=True)
        (sub / "lib.txt").write_text("lib\n")
        subprocess.run(["git", "-C", str(sub), *git, "add", "-A"], check=True)
        subprocess.run(["git", "-C", str(sub), *git, "commit", "-qm", "lib"], check=True)
        subprocess.run(["git", "-C", str(repo), *git, "submodule", "add", "-q", str(sub), "vendor"],
                       check=True, capture_output=True)
        subprocess.run(["git", "-C", str(repo), *git, "commit", "-qm", "sub"], check=True)
        self.fake_pi([answer("done"), SETTLED], pre="git -c protocol.file.allow=always submodule update --init -q; echo new > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        self.assertIs(self.outcome(self.cli("apply", state["run"]))["acceptStillValid"], True)
        (repo / "vendor/lib.txt").write_text("dirty\n")
        result = self.outcome(self.cli("apply", state["run"]))
        self.assertNotIn("acceptStillValid", result)
        self.assertIn("submodules", result["acceptValidityReason"])
        (repo / "vendor/lib.txt").write_text("lib\n")
        subprocess.run(["git", "-C", str(repo), "update-index", "--assume-unchanged", "a.txt"], check=True)
        result = self.outcome(self.cli("apply", state["run"]))
        self.assertNotIn("acceptStillValid", result)
        self.assertIn("index", result["acceptValidityReason"])
        subprocess.run(["git", "-C", str(repo), "update-index", "--no-assume-unchanged", "a.txt"], check=True)
        subprocess.run(["git", "-C", str(repo / "vendor"), "update-index", "--assume-unchanged", "lib.txt"], check=True)
        (repo / "vendor/lib.txt").write_text("hidden dirty\n")
        result = self.outcome(self.cli("apply", state["run"]))
        self.assertNotIn("acceptStillValid", result)
        self.assertIn("submodules", result["acceptValidityReason"])

    def test_uninitialized_reference_is_excluded_without_masking_later_dirty_contents(self):
        repo = self.sub_repo()
        subprocess.run(["git", "-C", str(repo), "submodule", "deinit", "-f", "vendor"],
                       check=True, capture_output=True)
        self.fake_pi([answer("done"), SETTLED], pre="echo new > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        self.assertIs(state["accept"]["snapshotComplete"], True)
        self.assertNotIn("snapshotReason", state["accept"])
        self.assertEqual(state["accept"]["excluded"], ["vendor"])
        applied = self.outcome(self.cli("apply", state["run"]))
        self.assertIs(applied["acceptStillValid"], True)
        self.assertEqual(applied["excluded"], ["vendor"])
        subprocess.run(["git", "-C", str(repo), "-c", "protocol.file.allow=always",
                        "submodule", "update", "--init", "-q", "vendor"], check=True)
        self.assertIs(self.outcome(self.cli("apply", state["run"]))["acceptStillValid"], True)
        (repo / "vendor/lib.txt").write_text("dirty\n")
        dirty = self.outcome(self.cli("apply", state["run"]))
        self.assertNotIn("acceptStillValid", dirty)
        self.assertIn("submodules", dirty["acceptValidityReason"])
        (repo / "vendor/lib.txt").write_text("v1\n")
        subprocess.run(["git", "-C", str(repo / "vendor"), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "--allow-empty", "-qm", "moved pointer"], check=True)
        moved = self.outcome(self.cli("apply", state["run"]))
        self.assertIs(moved["acceptStillValid"], False)
        self.assertIn("differs", moved["acceptValidityReason"])

    def test_uninitialized_reference_with_contents_or_changed_pointer_is_incomplete(self):
        repo = self.sub_repo()
        subprocess.run(["git", "-C", str(repo), "submodule", "deinit", "-f", "vendor"],
                       check=True, capture_output=True)
        self.fake_pi([answer("done"), SETTLED])
        (repo / "vendor/manual.txt").write_text("untracked contents\n")
        nonempty = self.outcome(self.cli("run", "--workdir", repo, "--accept", "true", "task"))
        self.assertFalse(nonempty["accept"].get("snapshotComplete", False))
        self.assertIn("snapshotReason", nonempty["accept"])
        (repo / "vendor/manual.txt").unlink()
        subprocess.run(["git", "-C", str(repo), "update-index", "--cacheinfo",
                        "160000," + "1" * 40 + ",vendor"], check=True)
        moved = self.outcome(self.cli("run", "--workdir", repo, "--accept", "true", "task"))
        self.assertIs(moved["accept"]["snapshotComplete"], False)
        self.assertIn("submodules", moved["accept"]["snapshotReason"])

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
        second = self.outcome(self.cli("reply", "--wait", first["run"], "--accept", "", "--prompt", "now tidy up"))
        self.assertNotIn("accept", second)
        self.assertIn("no longer applies", (Path(second["dir"]) / "prompt.md").read_text())
        third = self.outcome(self.cli("reply", "--wait", second["run"], "--prompt", "and more"))
        self.assertEqual((Path(third["dir"]) / "prompt.md").read_text(), "and more\n")

    def test_reply_keeps_the_session_when_its_parent_is_pruned(self):
        self.fake_pi([answer("ok"), SETTLED])
        first = self.outcome(self.cli("run", "--read-only", "task"))
        os.utime(Path(first["dir"]) / "exit_code", (1, 1))
        second = self.outcome(self.cli("reply", "--wait", first["run"], "more"))
        self.assertFalse(Path(first["dir"]).exists())  # pruned by the reply's own start
        self.assertTrue(any((Path(second["dir"]) / "session").glob("t_*.jsonl")))
        self.assertEqual(self.outcome(self.cli("reply", "--wait", second["run"], "again"))["state"], "answered")

    def test_malformed_reply_is_retried_from_the_parent_and_skipped(self):
        self.fake_pi([answer("ok"), SETTLED])
        first = self.outcome(self.cli("run", "--read-only", "task"))
        self.fake_pi([answer(LEAKED), SETTLED])
        broken = self.outcome(self.cli("reply", "--wait", first["run"], "more"))
        self.assertEqual((broken["state"], broken["attempts"]), ("malformed", 2))
        calls = [line.split() for line in (self.work / "pi.log").read_text().splitlines()]
        forks = {call[call.index("--fork") + 1] for call in calls[1:]}
        self.assertEqual(len(forks), 1)  # the rerun started again from the parent's conversation
        self.fake_pi([answer("better"), SETTLED])
        retry = self.outcome(self.cli("reply", "--wait", first["run"], "more, again"))
        self.assertEqual((retry["state"], retry["parent"]), ("answered", first["run"]))
        repo = self.repo({"a.txt": "a\n"})
        self.fake_pi([answer("ok"), SETTLED], pre="echo b >> a.txt")
        root = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.fake_pi([answer(LEAKED), SETTLED])
        dead_end = self.outcome(self.cli("reply", "--wait", root["run"], "more"))
        self.assertEqual(self.cli("apply", root["run"]).returncode, 0)
        self.assertTrue((Path(dead_end["dir"]) / ".applied").exists())
        self.assertNotIn("never applied", self.cli("clean", dead_end["run"]).stdout)

    def test_fresh_reply_and_apply_take_the_worktree_as_it_is(self):
        repo = self.repo({"a.txt": "a\n"})
        self.fake_pi([answer("first"), SETTLED], pre="echo one >> a.txt")
        first = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        self.fake_pi([answer("second"), SETTLED], pre="echo two >> a.txt")
        second = self.outcome(self.cli("reply", "--wait", "--fresh", first["run"], "--prompt", "standalone task"))
        call = (self.work / "pi.log").read_text().splitlines()[-1].split()
        self.assertIn("--session-id", call)
        self.assertNotIn("--fork", call)
        self.assertIn("Definition of done", (Path(second["dir"]) / "prompt.md").read_text())
        self.assertEqual((second["parent"], second["worktree"]), (first["run"], first["worktree"]))
        with open(Path(first["worktree"]) / "a.txt", "a") as touch_up:  # the caller finishes it by hand
            touch_up.write("three\n")
        self.assertEqual(self.cli("apply", first["run"]).returncode, 0)
        self.assertEqual((repo / "a.txt").read_text(), "a\none\ntwo\nthree\n")


    def test_v518_exact_deny_filtered_arguments_and_original_accept_path(self):
        repo = self.repo({"a.txt": "old\n"})
        rules = [{"argv": ["cargo", "xtask", "infra-test"], "exact": True, "hint": "use a filter"},
                 {"argv": ["cargo", "check"], "hint": "related checks only"}]
        (repo / ".delegate.json").write_text(json.dumps({"agentDeny": rules}))
        tool = self.bin / "cargo"
        tool.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\n')
        tool.chmod(0o755)
        delegate = shlex.quote(str(DELEGATE))
        self.fake_pi([answer("done"), SETTLED], pre=f'''
cargo xtask infra-test; echo $? > "$DELEGATE_RUN_DIR/exact-code"
cargo xtask infra-test --filter database > "$DELEGATE_RUN_DIR/filtered"
echo $? > "$DELEGATE_RUN_DIR/filtered-code"
{delegate} lane cargo xtask infra-test --filter database
echo $? > "$DELEGATE_RUN_DIR/lane-code"
cargo check --package small; echo $? > "$DELEGATE_RUN_DIR/prefix-code"
''')
        state = self.outcome(self.cli("run", "--agent", "pi", "--workdir", repo,
                                      "--accept", "cargo xtask infra-test", "task"))
        self.assertEqual((state["state"], state["denied"]), ("delivered", 2))
        run = Path(state["dir"])
        for name, code in (("exact", 77), ("filtered", 0), ("lane", 0), ("prefix", 77)):
            self.assertEqual((run / f"{name}-code").read_text().strip(), str(code))
        self.assertIn("database", (run / "filtered").read_text())
        self.assertIn("infra-test", (run / "accept.log").read_text())

    def test_v518_deny_merge_and_allow_distinguish_exact_from_prefix(self):
        repo = self.repo({"a.txt": "old\n"})
        self.env["XDG_CONFIG_HOME"] = str(self.work / "config")
        user_file = self.work / "config/delegate/config.json"
        user_file.parent.mkdir(parents=True)
        prefix = {"argv": ["cargo", "check"], "hint": "prefix"}
        exact = {"argv": ["cargo", "check"], "exact": True, "hint": "exact"}
        user_file.write_text(json.dumps({"agentDeny": [prefix, exact]}))
        tool = self.bin / "cargo"
        tool.write_text("#!/bin/sh\nexit 0\n")
        tool.chmod(0o755)
        self.fake_pi([answer("done"), SETTLED], pre='cargo check; cargo check --package small')
        config = repo / ".delegate.json"
        for revoke, remaining, denied in ((False, exact, 1), (True, prefix, 2)):
            config.write_text(json.dumps({"agentDeny": [{"argv": ["cargo", "check"],
                                                         "exact": revoke, "allow": True}]}))
            state = self.outcome(self.cli("run", "--agent", "pi", "--workdir", repo, "task"))
            meta = json.loads((Path(state["dir"]) / "meta.json").read_text())
            self.assertEqual(meta["agentDeny"], [remaining])
            self.assertEqual(state["denied"], denied)
        config.write_text(json.dumps({"agentDeny": [{**exact, "exact": "yes"}]}))
        rejected = self.cli("start", "--agent", "pi", "--workdir", repo, "task")
        self.assertEqual(rejected.returncode, 2)
        self.assertIn("agentDeny.exact must be a boolean", rejected.stderr)

    def test_v518_repository_capacity_normalizes_worktrees_and_legacy_meta(self):
        repo = self.repo({"a.txt": "old\n"})
        linked = self.work / "linked"
        subprocess.run(["git", "-C", str(repo), "worktree", "add", "-q", "--detach", str(linked)], check=True)
        other = self.work / "other"
        other.mkdir()
        self.fake_pi([answer("done"), SETTLED], sleep=30)
        self.env.update(DELEGATE_REPO_MAX_ACTIVE="1", DELEGATE_MAX_ACTIVE="3")
        first = self.outcome(self.cli("start", "--agent", "pi", "--read-only", "--in-place",
                                      "--workdir", repo, "first"))
        self.addCleanup(self.cli, "stop", first["dir"])
        meta_file = Path(first["dir"]) / "meta.json"
        meta = json.loads(meta_file.read_text())
        self.assertEqual(meta["repoKey"], str((repo / ".git").resolve()))
        del meta["repoKey"]  # Old task records and single-line slots still count.
        meta_file.write_text(json.dumps(meta))
        self.env["DELEGATE_RUNS"] = str(self.work / "runs2")
        second = self.outcome(self.cli("start", "--agent", "pi", "--read-only", "--workdir", other, "second"))
        self.addCleanup(self.cli, "stop", second["dir"])
        blocked = self.cli("start", "--agent", "pi", "--read-only", "--in-place", "--workdir", linked, "third")
        self.assertEqual(blocked.returncode, 2)
        self.assertIn("1 runs are active in this repository", blocked.stderr)
        self.assertIn("DELEGATE_REPO_MAX_ACTIVE=1", blocked.stderr)
        self.assertIn(first["dir"], blocked.stderr)
        self.assertNotIn(second["dir"], blocked.stderr)
        self.env["DELEGATE_REPO_MAX_ACTIVE"] = "0"
        third = self.outcome(self.cli("start", "--agent", "pi", "--read-only", "--in-place", "--workdir", linked, "third"))
        self.addCleanup(self.cli, "stop", third["dir"])
        blocked = self.cli("start", "--agent", "pi", "--read-only", "task")
        self.assertEqual(blocked.returncode, 2)
        self.assertIn("3 runs are active on this machine", blocked.stderr)
        for state in (first, second, third):
            self.assertIn(state["dir"], blocked.stderr)

    def test_v518_repository_codex_limits_and_non_git_normalization(self):
        self.fake_codex(codex_events("done"), pre="sleep 30")
        alias = self.work / "alias"
        alias.symlink_to(self.work, target_is_directory=True)
        self.env.update(DELEGATE_REPO_MAX_CODEX="1", DELEGATE_MAX_CODEX="3")
        first = self.outcome(self.cli("start", "--agent", "codex", "--read-only", "first"))
        self.addCleanup(self.cli, "stop", first["dir"])
        blocked = self.cli("start", "--agent", "codex", "--read-only", "--workdir", alias, "second")
        self.assertEqual(blocked.returncode, 2)
        self.assertIn("1 Codex runs are active in this repository", blocked.stderr)
        self.assertIn("DELEGATE_REPO_MAX_CODEX=1", blocked.stderr)
        self.env["DELEGATE_REPO_MAX_CODEX"] = "0"
        second = self.outcome(self.cli("start", "--agent", "codex", "--read-only", "--workdir", alias, "second"))
        self.addCleanup(self.cli, "stop", second["dir"])
        self.env["DELEGATE_REPO_MAX_CODEX"] = "bad"
        blocked = self.cli("start", "--agent", "codex", "--read-only", "task")
        self.assertEqual(blocked.returncode, 2)
        self.assertIn("DELEGATE_REPO_MAX_CODEX", blocked.stderr)

    def test_v518_capacity_defaults_config_precedence_and_protocol(self):
        defaults = {"maxActive": 12, "maxCodex": 6, "maxHeavy": 2, "repoMaxActive": 8, "repoMaxCodex": 4}
        self.fake_pi([answer("done"), SETTLED])
        state = self.outcome(self.cli("run", "--agent", "pi", "--read-only", "task"))
        self.assertEqual(json.loads((Path(state["dir"]) / "meta.json").read_text())["configCapacity"], defaults)
        self.assertEqual(json.loads(self.cli("protocol").stdout)["capacityDefaults"], defaults)
        repo = self.repo({"a.txt": "old\n"})
        self.env["XDG_CONFIG_HOME"] = str(self.work / "config")
        path = self.work / "config/delegate/config.json"
        path.parent.mkdir(parents=True)
        path.write_text(json.dumps({"repoMaxActive": 3, "repoMaxCodex": 2}))
        (repo / ".delegate.json").write_text('{"repoMaxActive":5}')
        self.env["DELEGATE_REPO_MAX_CODEX"] = "0"
        state = self.outcome(self.cli("run", "--agent", "pi", "--read-only", "--workdir", repo, "task"))
        capacity = json.loads((Path(state["dir"]) / "meta.json").read_text())["configCapacity"]
        self.assertEqual((capacity["repoMaxActive"], capacity["repoMaxCodex"]), (5, 0))
        (repo / ".delegate.json").write_text('{"repoMaxActive":-1}')
        self.assertIn("repoMaxActive must be", self.cli("start", "--agent", "pi", "--workdir", repo, "task").stderr)

    def test_v518_default_heavy_lane_runs_two_commands(self):
        gate = self.work / "release-lane"
        command = f'while [ ! -f {shlex.quote(str(gate))} ]; do sleep .02; done'
        runners = []
        for index in range(3):
            process = subprocess.Popen([str(DELEGATE), "lane", "--label", f"default-{index}", command],
                                       cwd=self.work, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            self.addCleanup(process.kill)
            runners.append(process)
        self.until(lambda: self.cli("lane").stdout.count("\n") == 3, "three default lane tickets")
        listing = self.cli("lane").stdout
        self.assertEqual((listing.count("running"), listing.count("queued")), (2, 1), listing)
        gate.touch()
        for process in runners:
            process.communicate(timeout=10)
            self.assertEqual(process.returncode, 0)

    def test_v518_repository_codex_capacity_blocks_cheap_escalation(self):
        self.env.pop("DELEGATE_STRONG_AGENT")
        self.env.update(DELEGATE_REPO_MAX_CODEX="1", DELEGATE_MAX_CODEX="3")
        self.fake_pi([answer(LEAKED), SETTLED])
        self.fake_codex(codex_events("done"), pre="sleep 30")
        busy = self.outcome(self.cli("start", "--agent", "codex", "--read-only", "busy"))
        self.addCleanup(self.cli, "stop", busy["dir"])
        state = self.outcome(self.cli("run", "--tier", "cheap", "--read-only", "--retries", "0", "task"))
        self.assertEqual((state["state"], state["agent"]), ("malformed", "pi"))
        self.assertNotIn("escalatedFrom", state)
        self.assertIn("escalate_skipped", (Path(state["dir"]) / "events.jsonl").read_text())

    def test_v518_after_waits_for_repository_capacity(self):
        repo = self.repo({"a.txt": "old\n"})
        gate = self.work / "gate"
        self.fake_pi([answer("done"), SETTLED], pre=f'''
if [ "$PWD" = {shlex.quote(str(repo))} ]; then
  while [ ! -f {shlex.quote(str(gate))} ]; do sleep .02; done
fi
''')
        self.env["DELEGATE_REPO_MAX_ACTIVE"] = "1"
        first = self.outcome(self.cli("start", "--agent", "pi", "--read-only", "--in-place", "--workdir", repo, "blocker"))
        self.addCleanup(self.cli, "stop", first["dir"])
        upstream = self.outcome(self.cli("run", "--agent", "pi", "--read-only", "upstream"))
        waiting = self.outcome(self.cli("start", "--agent", "pi", "--read-only", "--in-place", "--workdir", repo,
                                        "--after", upstream["run"], "queued"))
        self.addCleanup(self.cli, "stop", waiting["dir"])
        self.assertEqual(waiting["state"], "waiting")
        time.sleep(.2)
        self.assertEqual(self.outcome(self.cli("status", waiting["run"]))["state"], "waiting")
        gate.touch()
        finished = self.outcome(self.cli("wait", waiting["run"]))
        self.assertEqual(finished["state"], "answered")


    def test_detect_manual_merge_from_working_tree_contents_and_clean(self):
        repo = self.repo({"a.txt": "old\n", "gone.txt": "delete\n", "run.sh": "echo old\n",
                          "unrelated.txt": "untouched\n"})
        self.fake_pi([answer("done"), SETTLED], pre="echo new > a.txt; rm gone.txt; chmod +x run.sh; "
                     "echo added > new.txt; ln -s new.txt link")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        tree = Path(state["worktree"])
        (repo / "a.txt").write_bytes((tree / "a.txt").read_bytes())
        partial = self.outcome(self.cli("status", state["run"]))
        self.assertNotIn("applied", partial)
        self.assertIn("apply", partial["next"])
        (repo / "gone.txt").unlink()
        (repo / "new.txt").write_bytes((tree / "new.txt").read_bytes())
        (repo / "link").symlink_to("new.txt")
        self.assertNotIn("applied", self.outcome(self.cli("status", state["run"])))  # executable mode differs
        (repo / "run.sh").chmod(0o755)
        (repo / "unrelated.txt").write_text("caller edit\n")
        detected = self.outcome(self.cli("wait", state["run"]))
        self.assertEqual((detected["applied"], detected["appliedBy"]), (True, "detected"))
        self.assertIn("clean", detected["next"])
        self.assertEqual(json.loads((Path(state["dir"]) / ".applied").read_text())["appliedBy"], "detected")
        human = self.cli("status", state["run"], human=True).stdout
        self.assertIn("已合入（主干已含改动）", human)
        self.assertIn("下一步：清理", human)
        self.assertNotIn("delegate apply", human)
        self.assertEqual(self.cli("clean", "--finished").returncode, 0)
        self.assertFalse(tree.exists())
        self.assertFalse(Path(state["dir"]).exists())

    def test_detect_manual_merge_freezes_final_files_and_handles_old_records(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi([answer("done"), SETTLED], pre="echo task > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        (Path(state["worktree"]) / "a.txt").write_text("later\n")
        (repo / "a.txt").write_text("later\n")
        self.assertNotIn("applied", self.outcome(self.cli("status", state["run"])))
        (repo / "a.txt").write_text("task\n")
        run = Path(state["dir"])
        rec = json.loads((run / "changes.json").read_text())
        del rec["finalEntries"]
        (run / "changes.json").write_text(json.dumps(rec))
        detected = self.outcome(self.cli("status", state["run"]))
        self.assertEqual(detected["appliedBy"], "detected")

    def test_clean_detects_manual_merge_without_status_and_large_files(self):
        repo = self.repo({"a.txt": "old\n"})
        self.env["DELEGATE_SNAPSHOT_MAX_BYTES"] = "8"
        self.fake_pi([answer("done"), SETTLED], pre="echo changed > a.txt; head -c 100 /dev/zero > big.dat")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        tree = Path(state["worktree"])
        (repo / "a.txt").write_bytes((tree / "a.txt").read_bytes())
        (repo / "big.dat").write_bytes(b"x" * 100)
        self.assertNotIn("applied", self.outcome(self.cli("status", state["run"])))  # same size, different hash
        (repo / "big.dat").write_bytes((tree / "big.dat").read_bytes())
        self.assertEqual(self.cli("clean", "--finished").returncode, 0)
        self.assertFalse(tree.exists())

    def test_detect_manual_merge_requires_every_cumulative_reply_change(self):
        repo = self.repo({"a.txt": "old\n", "b.txt": "old\n"})
        self.fake_pi([answer("first"), SETTLED], pre="echo first > a.txt")
        first = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "task"))
        self.fake_pi([answer("second"), SETTLED], pre="echo second > b.txt")
        second = self.outcome(self.cli("reply", "--wait", first["run"], "continue"))
        (repo / "b.txt").write_text("second\n")
        self.assertNotIn("applied", self.outcome(self.cli("status", second["run"])))
        (repo / "a.txt").write_text("first\n")
        self.assertEqual(self.outcome(self.cli("status", second["run"]))["appliedBy"], "detected")

    def test_pinned_agent_displays_default_tier_and_never_escalates(self):
        self.env.pop("DELEGATE_STRONG_AGENT")
        self.fake_pi([answer(LEAKED), SETTLED])
        self.fake_codex(codex_events("fallback"))
        pinned = self.outcome(self.cli("run", "--read-only", "--agent", "pi", "task"))
        self.assertEqual((pinned["agent"], pinned["tier"], pinned["agentPinned"], pinned["state"]),
                         ("pi", "cheap", True, "malformed"))
        self.assertFalse((self.work / "pi.log.codex").exists())
        self.assertIn("pi/cheap", self.cli("status", pinned["run"], human=True).stdout)
        codex = self.outcome(self.cli("run", "--read-only", "--agent", "codex", "task"))
        self.assertEqual((codex["tier"], codex["agentPinned"]), ("strong", True))
        self.assertIn("codex/strong", self.cli("status", codex["run"], human=True).stdout)

    def test_pinned_reply_and_legacy_tier_display_do_not_rewrite_meta(self):
        self.fake_pi([answer("first"), SETTLED])
        first = self.outcome(self.cli("run", "--read-only", "--agent", "pi", "task"))
        path = Path(first["dir"]) / "meta.json"
        legacy = json.loads(path.read_text())
        legacy["tier"] = None
        del legacy["agentPinned"]
        original = json.dumps(legacy)
        path.write_text(original)
        shown = self.outcome(self.cli("status", first["run"]))
        self.assertEqual((shown["tier"], shown["agentPinned"]), ("cheap", True))
        self.assertEqual(path.read_text(), original)
        reply = self.outcome(self.cli("reply", "--wait", first["run"], "continue"))
        self.assertEqual((reply["tier"], reply["agentPinned"]), ("cheap", True))
        self.fake_codex(codex_events("switched"))
        switched = self.outcome(self.cli("reply", "--wait", "--agent", "codex", reply["run"], "continue"))
        self.assertEqual((switched["tier"], switched["agentPinned"]), ("strong", True))


def delegate_pid_alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0] != "Z"
    except OSError:  # gone between the two checks
        return False


def load_tests(loader, suite, pattern):
    regression_file = ROOT / "crates/delegate/tests/test_v515.py"
    if regression_file.is_file():
        spec = importlib.util.spec_from_file_location("delegate_v515_tests", regression_file)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        suite.addTests(loader.loadTestsFromTestCase(module.V515Tests))
    return suite


if __name__ == "__main__":
    unittest.main()
