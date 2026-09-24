import base64
import json
import os
import shlex
import shutil
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
INSTALL = ROOT / "scripts/install.sh"
CHECK = ROOT / "scripts/check.sh"
BOOTSTRAP = ROOT / "scripts/bootstrap.sh"
IMAGE = ROOT / "skills/openai-image-gen/scripts/generate-image.sh"
DELEGATION = ROOT / "skills/pi-delegation/scripts/pi-json-stream.sh"
DELEGATE = ROOT / "skills/pi-delegation/scripts/pi-delegate.sh"
SNAPSHOT = ROOT / "skills/agent-handoff/scripts/handoff-snapshot.sh"
AUDIT = ROOT / "skills/repo-governance/scripts/audit-context.py"


class ImageHandler(BaseHTTPRequestHandler):
    response_status = 200
    response_data = {"data": [{"b64_json": base64.b64encode(b"image-bytes").decode()}]}
    request_path = None
    request_data = None

    def do_POST(self):
        type(self).request_path = self.path
        type(self).request_data = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        payload = json.dumps(self.response_data).encode()
        self.send_response(self.response_status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_args):
        pass


class ScriptTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="aia-scripts-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)

    def run_script(self, script, *args, env=None):
        return subprocess.run([str(script), *map(str, args)], cwd=self.work, env=env,
                              capture_output=True, text=True, timeout=15)

    def test_install_rejects_traversal_without_touching_existing_data(self):
        home = self.work / "home"
        sentinel = home / ".agents" / "keep.txt"
        sentinel.parent.mkdir(parents=True)
        sentinel.write_text("keep")
        result = self.run_script(INSTALL, "..", env={**os.environ, "HOME": str(home)})
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(sentinel.read_text(), "keep")
        self.assertFalse((home / ".kilo").exists())

    def test_install_is_idempotent_and_never_removes_directories(self):
        home = self.work / "home"
        env = {**os.environ, "HOME": str(home)}
        for _ in range(2):
            self.assertEqual(self.run_script(INSTALL, "agent-handoff", env=env).returncode, 0)
        link = home / ".agents/skills/agent-handoff"
        self.assertEqual(link.resolve(), ROOT / "skills/agent-handoff")
        link.unlink()
        link.mkdir()
        (link / "keep.txt").write_text("keep")
        result = self.run_script(INSTALL, "--force", "agent-handoff", env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual((link / "keep.txt").read_text(), "keep")

    def test_install_force_replaces_only_foreign_links(self):
        home = self.work / "home"
        link = home / ".agents/skills/agent-handoff"
        link.parent.mkdir(parents=True)
        link.symlink_to(self.work / "another-skill")
        env = {**os.environ, "HOME": str(home)}
        self.assertNotEqual(self.run_script(INSTALL, "agent-handoff", env=env).returncode, 0)
        self.assertEqual(os.readlink(link), str(self.work / "another-skill"))
        self.assertEqual(self.run_script(INSTALL, "agent-handoff", "--force", env=env).returncode, 0)
        self.assertEqual(link.resolve(), ROOT / "skills/agent-handoff")

    def test_install_keeps_pi_delegation_out_of_pi_directories(self):
        home = self.work / "home"
        shared = home / ".agents/skills"
        shared.mkdir(parents=True)
        (shared / "pi-delegation").symlink_to(ROOT / "skills/pi-delegation")
        (shared / "agent-delegation").symlink_to(ROOT / "skills/agent-delegation")
        env = {**os.environ, "HOME": str(home)}
        result = self.run_script(INSTALL, "pi-delegation", "repo-governance", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((shared / "pi-delegation").is_symlink())
        self.assertFalse((shared / "agent-delegation").is_symlink())
        self.assertEqual((shared / "repo-governance").resolve(), ROOT / "skills/repo-governance")
        for directory in (".codex", ".cursor", ".claude", ".kilo"):
            self.assertEqual((home / directory / "skills/pi-delegation").resolve(), ROOT / "skills/pi-delegation")
        self.assertFalse((home / ".codex/skills/repo-governance").exists())
        self.assertFalse((home / ".kilo/skills/repo-governance").exists())

    def test_install_removes_redundant_links_and_uninstalls_only_owned(self):
        home = self.work / "home"
        kilo = home / ".kilo/skills"
        kilo.mkdir(parents=True)
        (kilo / "repo-governance").symlink_to(ROOT / "skills/repo-governance")
        foreign = home / ".agents/skills/foreign"
        foreign.mkdir(parents=True)
        env = {**os.environ, "HOME": str(home)}
        self.assertEqual(self.run_script(INSTALL, "repo-governance", env=env).returncode, 0)
        self.assertFalse((kilo / "repo-governance").is_symlink())
        result = self.run_script(INSTALL, "--uninstall", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((home / ".agents/skills/repo-governance").is_symlink())
        self.assertFalse((home / ".claude/skills/repo-governance").is_symlink())
        self.assertTrue(foreign.is_dir())

    def test_install_copy_mode_reports_outdated_and_switches_back_to_link(self):
        home = self.work / "home"
        env = {**os.environ, "HOME": str(home)}
        self.assertEqual(self.run_script(INSTALL, "--copy", "agent-handoff", env=env).returncode, 0)
        copy = home / ".agents/skills/agent-handoff"
        self.assertTrue(copy.is_dir() and not copy.is_symlink())
        marker = (copy / ".aia-skills-install").read_text()
        self.assertIn(f"source={ROOT / 'skills'}", marker)
        self.assertIn("version=2.0.0", marker)
        self.assertTrue((copy / "scripts/handoff-snapshot.sh").is_file())
        self.assertNotIn("outdated", self.run_script(INSTALL, "--status", env=env).stdout)
        (copy / ".aia-skills-install").write_text(marker.replace("version=2.0.0", "version=0.1.0"))
        self.assertIn("outdated", self.run_script(INSTALL, "--status", env=env).stdout)
        self.assertEqual(self.run_script(INSTALL, "agent-handoff", env=env).returncode, 0)
        self.assertEqual(copy.resolve(), ROOT / "skills/agent-handoff")
        self.assertTrue(copy.is_symlink())

    def test_bootstrap_clones_with_fallback_then_updates(self):
        upstream = self.work / "upstream"
        for part in ("scripts", "skills"):
            shutil.copytree(ROOT / part, upstream / part, ignore=shutil.ignore_patterns("__pycache__"))
        git_env = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t",
                   "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"}
        for args in (["init", "-q", "-b", "main"], ["add", "-A"], ["commit", "-q", "-m", "v1"]):
            subprocess.run(["git", "-C", str(upstream), *args], env=git_env, check=True, capture_output=True)
        home = self.work / "home"
        checkout = self.work / "checkout"
        env = {**git_env, "HOME": str(home), "AIA_SKILLS_PRIMARY": str(self.work / "missing.git"),
               "AIA_SKILLS_MIRROR": str(upstream)}
        result = self.run_script(BOOTSTRAP, "--dir", checkout, "agent-handoff", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Clone failed", result.stderr)
        self.assertEqual((home / ".agents/skills/agent-handoff").resolve(), checkout / "skills/agent-handoff")
        (upstream / "NEW.md").write_text("update\n")
        for args in (["add", "-A"], ["commit", "-q", "-m", "v2"]):
            subprocess.run(["git", "-C", str(upstream), *args], env=git_env, check=True, capture_output=True)
        result = self.run_script(BOOTSTRAP, "--dir", checkout, "agent-handoff", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((checkout / "NEW.md").is_file())
        (self.work / "occupied").mkdir()
        result = self.run_script(BOOTSTRAP, "--dir", self.work / "occupied", env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not a git checkout", result.stderr)

    def test_check_rejects_invalid_metadata_and_links(self):
        repository = self.work / "repository"
        (repository / "scripts").mkdir(parents=True)
        shutil.copy2(CHECK, repository / "scripts/check.sh")
        skill = repository / "skills/example"
        skill.mkdir(parents=True)
        (repository / "evals").mkdir()
        (repository / "evals/example.md").write_text("# example\n")
        (repository / "README.md").write_text("| **`example`** | `skills/example/` | example |\n")
        skill_file = skill / "SKILL.md"
        skill_file.write_text("---\nname: example\ndescription: Use when testing.\nmetadata:\n  version: \"1.0.0\"\n---\n\n# Example\n")
        check = repository / "scripts/check.sh"
        self.assertEqual(self.run_script(check).returncode, 0)
        skill_file.write_text("---\nname: example\ndescription: usable\n---\n\nrun ./skills/example/x.sh\n")
        result = self.run_script(check)
        self.assertIn("say when to use", result.stderr)
        self.assertIn("repo-relative", result.stderr)
        skill_file.write_text("---\nname: example\ndescription: 测试时使用\nmetadata:\n  version: \"1.0\"\n  exclude-agents: bogus\n---\n")
        result = self.run_script(check)
        self.assertIn("unknown agent", result.stderr)
        self.assertIn("metadata.version", result.stderr)
        (repository / "evals/example.md").unlink()
        self.assertIn("missing evals", self.run_script(check).stderr)
        skill_file.write_text("---\nname: example-other\ndescription: usable\n---\n\n[bad](missing.md)\n")
        result = self.run_script(check)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("frontmatter name", result.stderr)
        self.assertIn("broken link", result.stderr)
        skill_file.write_text("---\nname: [unterminated\ndescription: usable\n---\n")
        self.assertIn("invalid YAML", self.run_script(check).stderr)
        (repository / "README.md").write_text("# Empty index\n")
        self.assertIn("missing skill index", self.run_script(check).stderr)

    def test_pi_filter_keeps_exit_status(self):
        binary = self.work / "bin"
        binary.mkdir()
        pi = binary / "pi"
        pi.write_text("#!/bin/sh\n[ \"$4\" = '-p' ] && [ \"$(cat)\" = '- test prompt' ] || exit 9\nprintf '%s\\n' '{\"type\":\"tool_execution_start\",\"toolName\":\"bash\",\"args\":{\"command\":\"pwd\"}}' '{\"type\":\"agent_end\"}'\nexit 7\n")
        pi.chmod(0o755)
        (self.work / "prompt.txt").write_text("- test prompt")
        env = {**os.environ, "PATH": f"{binary}:{os.environ['PATH']}"}
        result = self.run_script(DELEGATION, "5s", self.work, "prompt.txt", "events.jsonl", env=env)
        self.assertEqual(result.returncode, 7)
        self.assertEqual([json.loads(line)["e"] for line in (self.work / "events.jsonl").read_text().splitlines()],
                         ["bash", "agent_end", "summary"])
        self.assertEqual(json.loads((self.work / "events.jsonl").read_text().splitlines()[-1])["status"], "failed")
        result = self.run_script(DELEGATION, "5s", self.work, "prompt.txt", "prompt.txt", env=env)
        self.assertEqual(result.returncode, 2)
        self.assertEqual((self.work / "prompt.txt").read_text(), "- test prompt")
        (self.work / "log-link").symlink_to(self.work / "prompt.txt")
        result = self.run_script(DELEGATION, "5s", self.work, "prompt.txt", "log-link", env=env)
        self.assertEqual(result.returncode, 2)
        self.assertEqual((self.work / "prompt.txt").read_text(), "- test prompt")

    def test_pi_explicit_model_read_only_and_turn_metadata(self):
        binary = self.work / "bin"
        binary.mkdir()
        pi = binary / "pi"
        pi.write_text(
            "#!/bin/sh\n"
            "[ \"$4\" = '--provider' ] && [ \"$5\" = 'example-provider' ] || exit 9\n"
            "[ \"$6\" = '--model' ] && [ \"$7\" = 'gemini-test' ] || exit 9\n"
            "[ \"$8\" = '--tools' ] && [ \"$9\" = 'read,grep,find,ls' ] || exit 9\n"
            "printf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"provider\":\"example-provider\",\"model\":\"gemini-test\",\"stopReason\":\"stop\",\"usage\":{\"input\":12,\"output\":4},\"content\":[{\"type\":\"thinking\",\"text\":\"private thought\"},{\"type\":\"text\",\"text\":\"final answer\"}]}}' '{\"type\":\"agent_settled\"}'\n"
        )
        pi.chmod(0o755)
        (self.work / "prompt.txt").write_text("inspect")
        env = {**os.environ, "PATH": f"{binary}:{os.environ['PATH']}"}
        result = self.run_script(DELEGATION, "--provider", "example-provider", "--model", "gemini-test",
                                 "--read-only", "5s", self.work, "prompt.txt", "events.jsonl", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        events = [json.loads(line) for line in (self.work / "events.jsonl").read_text().splitlines()]
        turn = events[0]
        self.assertEqual(turn["model"], "gemini-test")
        self.assertEqual(turn["usage"]["input"], 12)
        self.assertEqual(events[1]["e"], "result")
        self.assertEqual(events[1]["text"], "final answer")
        self.assertTrue(all(event.get("at", "").endswith("Z") for event in events))
        self.assertEqual(events[-1]["tokens"]["input"], 12)
        self.assertNotIn("private thought", result.stdout)

    def test_pi_error_turn_fails_even_when_cli_exits_zero(self):
        binary = self.work / "bin"
        binary.mkdir()
        pi = binary / "pi"
        pi.write_text("#!/bin/sh\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"error\",\"errorMessage\":\"provider failed\"}}' '{\"type\":\"agent_settled\"}'\n")
        pi.chmod(0o755)
        (self.work / "prompt.txt").write_text("inspect")
        env = {**os.environ, "PATH": f"{binary}:{os.environ['PATH']}"}
        result = self.run_script(DELEGATION, "5s", self.work, "prompt.txt", "events.jsonl", env=env)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("did not settle", result.stderr)

    def test_pi_empty_final_answer_is_not_success(self):
        binary = self.work / "bin"
        binary.mkdir()
        pi = binary / "pi"
        pi.write_text("#!/bin/sh\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":[]}}' '{\"type\":\"agent_settled\"}'\n")
        pi.chmod(0o755)
        (self.work / "prompt.txt").write_text("inspect")
        env = {**os.environ, "PATH": f"{binary}:{os.environ['PATH']}"}
        result = self.run_script(DELEGATION, "5s", self.work, "prompt.txt", "events.jsonl", env=env)
        self.assertEqual(result.returncode, 1)
        self.assertIn("nonempty final result", result.stderr)
        events = [json.loads(line) for line in (self.work / "events.jsonl").read_text().splitlines()]
        self.assertNotIn("result", [event["e"] for event in events])
        self.assertEqual(events[-1]["status"], "failed")

    def test_pi_timeout_is_reported(self):
        binary = self.work / "bin"
        binary.mkdir()
        pi = binary / "pi"
        pi.write_text("#!/bin/sh\nsleep 3\n")
        pi.chmod(0o755)
        (self.work / "prompt.txt").write_text("inspect")
        env = {**os.environ, "PATH": f"{binary}:{os.environ['PATH']}"}
        result = self.run_script(DELEGATION, "1s", self.work, "prompt.txt", "events.jsonl", env=env)
        self.assertEqual(result.returncode, 124)
        self.assertIn("timed out", result.stderr)
        self.assertEqual(json.loads((self.work / "events.jsonl").read_text().splitlines()[-1])["status"], "timeout")

    def test_pi_short_invocation_and_explicit_timeout(self):
        binary = self.work / "bin"
        binary.mkdir()
        pi = binary / "pi"
        pi.write_text("#!/bin/sh\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}' '{\"type\":\"agent_settled\"}'\n")
        pi.chmod(0o755)
        (self.work / "prompt.txt").write_text("inspect")
        env = {**os.environ, "PATH": f"{binary}:{os.environ['PATH']}"}
        result = self.run_script(DELEGATION, "prompt.txt", "default.events.jsonl", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.run_script(DELEGATION, "--timeout", "2s", "--workdir", self.work,
                                 "prompt.txt", "explicit.events.jsonl", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.run_script(DELEGATION, "--timeout", "0", "prompt.txt", "invalid.events.jsonl", env=env)
        self.assertEqual(result.returncode, 2)
        self.assertIn("Invalid positive timeout", result.stderr)

    def test_pi_last_empty_turn_does_not_inherit_previous_result(self):
        binary = self.work / "bin"
        binary.mkdir()
        pi = binary / "pi"
        pi.write_text("#!/bin/sh\nprintf '%s\\n' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":[{\"type\":\"text\",\"text\":\"earlier answer\"}]}}' '{\"type\":\"message_end\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":[]}}' '{\"type\":\"agent_settled\"}'\n")
        pi.chmod(0o755)
        (self.work / "prompt.txt").write_text("inspect")
        env = {**os.environ, "PATH": f"{binary}:{os.environ['PATH']}"}
        result = self.run_script(DELEGATION, "prompt.txt", "events.jsonl", env=env)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual(json.loads((self.work / "events.jsonl").read_text().splitlines()[-1])["status"], "failed")

    def test_pi_killed_is_not_mislabeled_timeout(self):
        binary = self.work / "bin"
        binary.mkdir()
        pi = binary / "pi"
        pi.write_text("#!/bin/sh\nexit 137\n")
        pi.chmod(0o755)
        (self.work / "prompt.txt").write_text("inspect")
        env = {**os.environ, "PATH": f"{binary}:{os.environ['PATH']}"}
        result = self.run_script(DELEGATION, "prompt.txt", "events.jsonl", env=env)
        self.assertEqual(result.returncode, 137)
        self.assertEqual(json.loads((self.work / "events.jsonl").read_text().splitlines()[-1])["status"], "killed")

    def test_pi_long_result_is_previewed_on_stdout_but_retained_in_log(self):
        binary = self.work / "bin"
        binary.mkdir()
        answer = "setup details " * 90 + "final delivery"
        event = {"type": "message_end", "message": {"role": "assistant", "stopReason": "stop",
                                                     "content": [{"type": "text", "text": answer}]}}
        pi = binary / "pi"
        pi.write_text("#!/bin/sh\nprintf '%s\\n' " + shlex.quote(json.dumps(event)) +
                      " '{\"type\":\"agent_settled\"}'\n")
        pi.chmod(0o755)
        (self.work / "prompt.txt").write_text("inspect")
        env = {**os.environ, "PATH": f"{binary}:{os.environ['PATH']}"}
        result = self.run_script(DELEGATION, "prompt.txt", "events.jsonl", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        preview = next(item for item in map(json.loads, result.stdout.splitlines()) if item["e"] == "result")
        logged = next(item for item in map(json.loads, (self.work / "events.jsonl").read_text().splitlines())
                      if item["e"] == "result")
        self.assertTrue(preview["truncated"])
        self.assertTrue(preview["text"].endswith("final delivery"))
        self.assertEqual(logged["text"], answer)

    def test_pi_repeated_probes_are_compacted_but_writes_visible(self):
        binary = self.work / "bin"
        binary.mkdir()
        pi = binary / "pi"
        events = []
        for i in range(8):
            events.append({"type": "tool_execution_start", "toolName": "bash",
                           "args": {"command": f"node -e '\\nconsole.log({i})'"}})
            events.append({"type": "message_end", "message": {"role": "assistant",
                           "stopReason": "toolUse", "usage": {"input": 10, "output": 2}}})
        events.extend([
            {"type": "tool_execution_end", "toolName": "bash", "isError": True,
             "result": {"content": [{"text": "syntax error\nCommand exited with code 1\n"}]}},
            {"type": "tool_execution_start", "toolName": "write", "args": {"path": "index.html"}},
            {"type": "message_end", "message": {"role": "assistant", "provider": "example-provider",
             "model": "gemini-test", "stopReason": "stop",
             "usage": {"input": 5, "output": 3}, "content": [{"type": "text", "text": "done"}]}},
            {"type": "agent_settled"},
        ])
        pi.write_text("#!/bin/sh\nprintf '%s\\n' " + " ".join(shlex.quote(json.dumps(event)) for event in events) + "\n")
        pi.chmod(0o755)
        (self.work / "prompt.txt").write_text("inspect")
        env = {**os.environ, "PATH": f"{binary}:{os.environ['PATH']}"}
        result = self.run_script(DELEGATION, "5s", self.work, "prompt.txt", "events.jsonl", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        shown = [json.loads(line) for line in result.stdout.splitlines()]
        self.assertEqual([item["count"] for item in shown if item["e"] == "checks"], [1, 5])
        self.assertTrue(any(item["e"] == "check_done" and item["count"] == 8
                            and item["ok"] is False for item in shown))
        self.assertEqual(sum(item["e"] == "write" for item in shown), 1)
        self.assertEqual(next(item["detail"] for item in shown if item["e"] == "tool_error"),
                         "syntax error (Command exited with code 1)")
        self.assertEqual(sum(item["e"] == "turn" for item in shown), 1)
        self.assertEqual(shown[-1]["writes"], 1)
        self.assertEqual(shown[-1]["bashRuns"], 1)
        self.assertEqual(shown[-1]["failedBashRuns"], 1)
        self.assertEqual(shown[-1]["toolErrors"], 1)
        self.assertEqual(shown[-1]["tokens"]["input"], 85)

    def delegate_env(self, answer="done", sleep=0):
        binary = self.work / "bin"
        binary.mkdir(exist_ok=True)
        event = {"type": "message_end", "message": {"role": "assistant", "model": "fake", "stopReason": "stop",
                                                    "content": [{"type": "text", "text": answer}]}}
        pi = binary / "pi"
        pi.write_text("#!/bin/sh\ncat > /dev/null\necho $$ > \"$PI_PID_FILE\"\n"
                      f"sleep {sleep}\n"
                      "printf '%s\\n' '{\"type\":\"tool_execution_start\",\"toolName\":\"write\",\"args\":{\"path\":\"a.txt\"}}' "
                      + shlex.quote(json.dumps(event)) + " '{\"type\":\"agent_settled\"}'\n")
        pi.chmod(0o755)
        return {**os.environ, "PATH": f"{binary}:{os.environ['PATH']}", "PI_DELEGATE_POLL": "0.2",
                "PI_DELEGATE_RUNS": str(self.work / "runs"), "PI_PID_FILE": str(self.work / "pi.pid")}

    def test_delegate_run_reports_result_once(self):
        env = self.delegate_env(answer="final delivery")
        result = self.run_script(DELEGATE, "run", "--name", "quick", "do", "it", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        status = next(json.loads(line) for line in result.stdout.splitlines() if '"state"' in line)
        self.assertEqual((status["state"], status["files"], status["resultChars"]), ("ok", ["a.txt"], 15))
        self.assertIn("final delivery", result.stdout)
        self.assertEqual(self.run_script(DELEGATE, "result", "last", env=env).stdout, "final delivery\n")
        result = self.run_script(DELEGATE, "wait", "--all", env=env)
        self.assertEqual(result.returncode, 0)
        self.assertIn("no active or undelivered", result.stderr)

    def test_delegate_refuses_nested_runs_and_exports_guard(self):
        env = self.delegate_env()
        pi = self.work / "bin/pi"
        pi.write_text(pi.read_text().replace("cat > /dev/null\n",
                                             "cat > /dev/null\necho \"$PI_DELEGATE_ACTIVE\" > \"$PI_PID_FILE.guard\"\n"))
        self.assertEqual(self.run_script(DELEGATE, "run", "task", env=env).returncode, 0)
        self.assertEqual((self.work / "pi.pid.guard").read_text().strip(), "1")
        nested = {**env, "PI_DELEGATE_ACTIVE": "1"}
        result = self.run_script(DELEGATE, "start", "task", env=nested)
        self.assertEqual(result.returncode, 2)
        self.assertIn("nested delegation", result.stderr)
        (self.work / "prompt.txt").write_text("inspect")
        result = self.run_script(DELEGATION, "prompt.txt", "events.jsonl", env=nested)
        self.assertEqual(result.returncode, 2)
        self.assertIn("nested delegation", result.stderr)

    def test_delegate_wait_slices_guard_writes_and_stop(self):
        env = self.delegate_env(sleep=30)
        self.assertEqual(self.run_script(DELEGATE, "start", "--name", "slow", "work", env=env).returncode, 0)
        result = self.run_script(DELEGATE, "wait", "slow", "--max", "0", env=env)
        self.assertEqual(result.returncode, 75)
        self.assertEqual(json.loads(result.stdout.splitlines()[-1])["state"], "running")
        result = self.run_script(DELEGATE, "start", "second", "writer", env=env)
        self.assertEqual(result.returncode, 2)
        self.assertIn("still active", result.stderr)
        self.assertEqual(self.run_script(DELEGATE, "start", "--read-only", "reviewer", env=env).returncode, 0)
        pi_pid = int((self.work / "pi.pid").read_text())
        result = self.run_script(DELEGATE, "stop", "slow", "last", env=env)
        self.assertEqual([json.loads(line)["state"] for line in result.stdout.splitlines()], ["stopped", "stopped"])
        with self.assertRaises(ProcessLookupError):
            os.kill(pi_pid, 0)
        result = self.run_script(DELEGATE, "clean", "--finished", env=env)
        self.assertEqual(result.stdout.count("removed"), 2)

    def test_delegate_long_result_shows_tail_and_no_result_keeps_it_pending(self):
        env = self.delegate_env(answer="draft " * 1200 + "real answer")
        self.assertEqual(self.run_script(DELEGATE, "start", "question", env=env).returncode, 0)
        result = self.run_script(DELEGATE, "wait", "--no-result", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("real answer", result.stdout)
        result = self.run_script(DELEGATE, "wait", "--all", env=env)
        self.assertIn("showing the last 6000 chars", result.stdout)
        self.assertTrue(result.stdout.split("===== end")[0].rstrip().endswith("real answer"))
        self.assertLess(len(result.stdout), 8000)
        self.assertIn("real answer", self.run_script(DELEGATE, "wait", "last", "--full", env=env).stdout)

    def test_delegate_clean_keeps_unreported_and_start_prunes_expired(self):
        env = self.delegate_env()
        result = subprocess.run([str(DELEGATE), "start", "--name", "old", "--prompt-file", "-"], cwd=self.work,
                                env=env, input="multi\nline\n", capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        old = Path(json.loads(result.stdout)["dir"])
        self.assertEqual((old / "prompt.md").read_text(), "multi\nline\n")
        self.assertEqual(self.run_script(DELEGATE, "wait", "old", "--no-result", env=env).returncode, 0)
        result = self.run_script(DELEGATE, "clean", "--finished", env=env)
        self.assertIn("keep unreported run", result.stderr)
        self.assertTrue(old.exists())
        self.assertEqual(self.run_script(DELEGATE, "wait", "old", env=env).returncode, 0)
        os.utime(old / "exit_code", (1, 1))
        self.assertEqual(self.run_script(DELEGATE, "run", "--name", "new", "task", env=env).returncode, 0)
        self.assertFalse(old.exists())
        self.assertEqual(self.run_script(DELEGATE, "start", "--name", "pending", "task", env=env).returncode, 0)
        self.assertEqual(self.run_script(DELEGATE, "wait", "pending", "--no-result", env=env).returncode, 0)
        result = self.run_script(DELEGATE, "clean", "--finished", "--force", env=env)
        self.assertEqual(result.stdout.count("removed"), 2)

    def git_repo(self):
        repo = self.work / "repo"
        repo.mkdir()
        env = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t",
               "GIT_COMMITTER_EMAIL": "t@t"}
        for args in (["init", "-q"], ["commit", "-q", "--allow-empty", "-m", "first commit"]):
            subprocess.run(["git", "-C", str(repo), *args], env=env, check=True, capture_output=True)
        return repo

    def test_handoff_snapshot_collects_facts(self):
        repo = self.git_repo()
        (repo / "AGENTS.md").write_text("# rules\n")
        run = repo / ".local/run/pi/20260101-000000-review"
        run.mkdir(parents=True)
        (run / "meta.json").write_text("{}")
        (run / "exit_code").write_text("0")
        result = self.run_script(SNAPSHOT, "--repo", repo)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("first commit", result.stdout)
        self.assertIn("`AGENTS.md`", result.stdout)
        self.assertIn("?? AGENTS.md", result.stdout)
        self.assertIn("20260101-000000-review 已结束，结果未读取", result.stdout)
        (run / ".delivered").touch()
        self.assertNotIn("review", self.run_script(SNAPSHOT, "--repo", repo).stdout)

    def test_audit_context_reports_drift(self):
        repo = self.git_repo()
        result = self.run_script(AUDIT, "--repo", repo)
        self.assertEqual(result.returncode, 1)
        self.assertIn("AGENTS.md: missing", result.stdout)
        self.assertIn(".local/ is not ignored", result.stdout)
        (repo / ".gitignore").write_text(".local/\n")
        (repo / "AGENTS.md").write_text("line\n" * 5 + "[doc](docs/missing.md)\n")
        (repo / "docs").mkdir()
        (repo / "docs/current.md").write_text("# 下一步\n" + "".join(f"{i}. item\n" for i in range(1, 8)) +
                                               "# Notes\n- not counted\n")
        result = self.run_script(AUDIT, "--repo", repo, "--max-agents-lines", "3")
        self.assertIn("6 lines", result.stdout)
        self.assertIn("broken link: docs/missing.md", result.stdout)
        self.assertIn("7 next actions", result.stdout)
        (repo / "AGENTS.md").write_text("# ok\n")
        (repo / "docs/current.md").write_text("# Next steps\n- one\n")
        result = self.run_script(AUDIT, "--repo", repo)
        self.assertEqual(result.returncode, 0, result.stdout)

    def image_server(self, status=200, data=None):
        handler = type("Response", (ImageHandler,), {
            "response_status": status,
            "response_data": data if data is not None else ImageHandler.response_data,
        })
        server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        return server, handler

    def image_env(self):
        return {**os.environ, "OPENAI_API_KEY": "test-only", "CODEX_HOME": str(self.work / "no-codex")}

    def test_image_success_uses_valid_model_and_one_v1(self):
        server, handler = self.image_server()
        output = self.work / "image.png"
        output.write_bytes(b"original")
        result = self.run_script(IMAGE, "-p", "demo", "-o", output, "-b",
                                 f"http://127.0.0.1:{server.server_port}/v1", env=self.image_env())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(output.read_bytes(), b"image-bytes")
        self.assertEqual(handler.request_path, "/v1/images/generations")
        self.assertEqual(handler.request_data["model"], "gpt-image-2.5-sunburst")

    def test_image_failure_preserves_existing_output(self):
        for status, data in [(500, {"error": {"message": "rejected"}}),
                             (200, {"data": [{"b64_json": "invalid!"}]}),
                             (200, {"data": [{"url": "http://example.com/image.png"}]})]:
            with self.subTest(status=status, data=data):
                server, _ = self.image_server(status, data)
                output = self.work / "image.png"
                output.write_bytes(b"original")
                result = self.run_script(IMAGE, "-p", "demo", "-o", output, "-b",
                                         f"http://127.0.0.1:{server.server_port}", env=self.image_env())
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(output.read_bytes(), b"original")

    def test_image_config_uses_selected_provider(self):
        server, handler = self.image_server()
        config = self.work / "codex"
        config.mkdir()
        (config / "config.toml").write_text(
            'model_provider = "selected"\n'
            '[model_providers.decoy]\nbase_url = "http://invalid.example/v1"\n'
            '[model_providers.selected]\n'
            f'base_url = "http://127.0.0.1:{server.server_port}/v1"\n'
            '[model_providers.selected.http_headers]\nAuthorization = "Bearer selected-key"\n'
        )
        env = {key: value for key, value in os.environ.items()
               if key not in ("OPENAI_API_KEY", "OPENAI_BASE_URL")}
        env["CODEX_HOME"] = str(config)
        result = self.run_script(IMAGE, "-p", "demo", "-o", self.work / "image.png", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(handler.request_path, "/v1/images/generations")

    def test_image_missing_value_shows_usage(self):
        result = self.run_script(IMAGE, "--prompt", "demo", "--output")
        self.assertEqual(result.returncode, 2)
        self.assertIn("Usage:", result.stderr)


if __name__ == "__main__":
    unittest.main()
