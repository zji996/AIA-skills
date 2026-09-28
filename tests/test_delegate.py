import json
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

    def cli(self, *args, stdin=None, cwd=None, timeout=30):
        return subprocess.run([str(DELEGATE), *map(str, args)], cwd=cwd or self.work, env=self.env,
                              input=stdin, capture_output=True, text=True, timeout=timeout)

    def outcome(self, result):
        lines = [line for line in result.stdout.splitlines() if line.startswith('{"run"')]
        self.assertTrue(lines, result.stdout + result.stderr)
        return json.loads(lines[0])

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
        holder = subprocess.Popen([str(DELEGATE), "lane", "--label", "caller check", f"sleep {seconds}"], env=self.env)
        self.addCleanup(holder.kill)
        self.until(lambda: "caller check" in self.cli("lane").stdout, "the lane holder")
        return holder

    def test_lane_runs_heavy_commands_one_at_a_time(self):
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
        self.assertEqual(self.outcome(self.cli("stop", run["run"]))["state"], "stopped")
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
        self.assertNotIn("tier", state)  # named outright: no tier, no escalation
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
        started = subprocess.run([str(DELEGATE), "start", "--agent", "codex", "--read-only", "busy"], cwd=self.work,
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
        self.assertEqual(agents["pi"]["shadowed"], [str((shadow / "pi").resolve())])
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
        self.assertTrue((repo / "seen.txt").exists())
        self.cli("clean", state["run"])
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
        third = self.outcome(self.cli("reply", "--wait", first["run"], "and finally"))  # the conversation's latest run
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
        self.outcome(self.cli("reply", "--wait", "conv", "three"))
        (repo / "a.txt").write_text("a\nagent\ncaller-too\n")
        calls = len((self.work / "pi.log").read_text().splitlines())
        refused = self.cli("reply", "conv", "--sync", "four")
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
        refused = self.cli("reply", "conv", "--sync", "three")
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


def delegate_pid_alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0] != "Z"
    except OSError:  # gone between the two checks
        return False


if __name__ == "__main__":
    unittest.main()
