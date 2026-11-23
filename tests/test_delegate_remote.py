"""Remote commands use fake SSH/rsync transports, not a reachable SSH daemon."""
import json
import fcntl
import hashlib
import threading
import os
import shlex
import shutil
import signal
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateRemoteTests(unittest.TestCase):
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until
    user_config = fixtures.DelegateTests.user_config

    def setUp(self):
        fixtures.DelegateTests.setUp(self)
        self.remote = self.work / "remote-home"
        self.remote.mkdir()
        (self.remote / "bin").mkdir()
        self.calls = self.work / "remote-calls"
        self.ssh_log = self.work / "ssh-calls"
        self.user_config({"models": fixtures.REGISTERED_MODELS,
                          "remote": {"host": "ci", "root": "build", "maxHeavy": 1}})
        ssh = self.bin / "ssh"
        ssh.write_text(f"#!{sys.executable}\n" + f'''
import os, sys, subprocess
from pathlib import Path
with open({str(self.ssh_log)!r}, "a") as log:
    log.write(repr(sys.argv[1:]) + "\\n")
if Path({str(self.work / 'offline')!r}).exists():
    print("test transport offline", file=sys.stderr)
    sys.exit(255)
assert sys.argv[-2] == "ci", sys.argv
os.environ["HOME"] = {str(self.remote)!r}
os.environ["PATH"] = {str(self.remote / 'bin')!r} + ":/usr/bin:/bin"
script = sys.argv[-1]
if Path({str(self.work / 'no-remote-link')!r}).exists() and "LINKED:" in script:
    script = script.replace("if [ -e ", "if false && [ -e ")
os.execv("/bin/sh", ["sh", "-c", script])
''')
        ssh.chmod(0o755)
        real_rsync = shutil.which("rsync")
        if not real_rsync:
            self.skipTest("rsync required for filter/delete conformance")
        rsync = self.bin / "rsync"
        rsync.write_text(f"#!{sys.executable}\n" + f'''
import os, sys
from pathlib import Path
if Path({str(self.work / 'offline')!r}).exists() or Path({str(self.work / 'rsync-offline')!r}).exists():
    print("test rsync offline", file=sys.stderr)
    sys.exit(12)
a = sys.argv[1:]
i = a.index("-e")
del a[i:i+2]
a = [s.removeprefix("ci:") for s in a]
os.execv({real_rsync!r}, ["rsync", *a])
''')
        rsync.chmod(0o755)
        cargo = self.remote / "bin/cargo"
        cargo.write_text(f"#!{sys.executable}\n" + f'''
import json, os, sys, time, subprocess
from pathlib import Path
with open({str(self.calls)!r}, "a") as log:
    log.write(json.dumps({{"argv":sys.argv[1:], "cwd":os.getcwd(), "pid":os.getpid(), "env":dict(os.environ)}}) + "\\n")
if sys.argv[1:2] == ["--version"]:
    print("cargo fake")
    sys.exit(0)
if sys.argv[1:2] == ["sleep"]:
    child = subprocess.Popen(["sleep", "60"])
    Path({str(self.work / 'remote-pids')!r}).write_text(str(os.getpid()) + " " + str(child.pid))
    time.sleep(60)
if sys.argv[1:2] == ["binary"]:
    os.write(1, b"raw\\xff:" + os.getcwd().encode() + b"/a.txt")
    os.write(2, b"err\\xfe")
    sys.exit(7)
if sys.argv[1:2] == ["fmt"]:
    Path("a.txt").write_text("formatted\\n")
    Path("added.txt").write_text("new\\n")
    Path("remove.txt").unlink(missing_ok=True)
    Path("target").mkdir(exist_ok=True)
    Path("target/warm").write_text("cache")
print(os.getcwd() + "/a.txt:1: diagnostic")
print("stderr: " + os.getcwd(), file=sys.stderr)
if sys.argv[1:2] == ["exit"]:
    sys.exit(int(sys.argv[2]))
if sys.argv[1:2] == ["check-managed"]:
    assert Path(".local/providers/model.json").read_text().strip() == "ok", "missing .local/providers"
    assert Path("third_party/pi/spec.md").read_text().strip() == "pi-spec", "missing third_party/pi"
    print("managed-ok")
    sys.exit(0)
''')
        cargo.chmod(0o755)
        local = self.bin / "cargo"
        local.write_text(f"#!{sys.executable}\n" + '''
import sys
from pathlib import Path
if sys.argv[1:2] == ["check-managed-local"]:
    assert Path(".local/providers/big.bin").is_file(), "missing local big.bin"
    print("local-managed-ok")
    sys.exit(0)
print("local:" + " ".join(sys.argv[1:]))
''')
        local.chmod(0o755)

    def records(self):
        return [json.loads(line) for line in self.calls.read_text().splitlines()] if self.calls.exists() else []

    def run_remote(self, pre="true", accept="cargo fmt", config=None):
        repo = self.work / "repo"
        if not repo.exists():
            repo = self.repo({"a.txt": "old\n", "remove.txt": "delete me\n", ".gitignore": "target/\nnode_modules/\n"})
        if config is not None:
            (repo / ".delegate.json").write_text(json.dumps(config))
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre=pre)
        result = self.cli("run", "--worktree", "--workdir", repo, "--accept", accept, "task")
        return repo, self.outcome(result)

    def test_remote_off_survives_reply_fresh_and_explicit_environment_overrides(self):
        self.env["DELEGATE_REMOTE"] = "0"
        _, state = self.run_remote(accept="cargo check")
        self.assertEqual(self.records(), [])
        for options, override, expected in (((), None, False), (("--fresh",), None, False),
                                            ((), "1", True), ((), "0", False)):
            if override is None:
                self.env.pop("DELEGATE_REMOTE", None)
            else:
                self.env["DELEGATE_REMOTE"] = override
            before = len(self.records())
            self.fake_pi([fixtures.answer("continued"), fixtures.SETTLED])
            reply = self.cli("reply", "--wait", *options, state["run"], "continue")
            self.assertEqual(reply.returncode, 0, reply.stdout + reply.stderr)
            state = self.outcome(reply)
            meta = json.loads((Path(state["dir"]) / "meta.json").read_text())
            self.assertEqual(meta["remoteOff"], not expected)
            self.assertEqual(len(self.records()) > before, expected)

    def test_verify_transport_not_executed_is_distinct_from_program_failure(self):
        repo, state = self.run_remote(pre="echo changed > a.txt", accept="cargo check")
        (self.work / "offline").touch()
        failed = self.cli("apply", "--verify", "--keep", state["run"], human=True)
        self.assertEqual(failed.returncode, 1, failed.stdout + failed.stderr)
        self.assertIn("验收未完成", failed.stdout)
        self.assertIn("重排", failed.stdout)
        self.assertIn("DELEGATE_REMOTE=0", failed.stdout)
        self.assertNotIn("合并后验收失败", failed.stdout)
        self.assertEqual((repo / "a.txt").read_text(), "changed\n")
        (self.work / "offline").unlink()
        # Exit 75 from a program is a real failure, not a broken transport.
        cfg = repo / ".delegate.json"
        cfg.write_text(json.dumps({"applyVerify": "cargo exit 75"}))
        failed = self.outcome(self.cli("apply", "--json", "--verify", "--keep", state["run"]))
        self.assertFalse(failed["verify"]["notExecuted"])
        self.assertFalse(failed["verify"]["infrastructureFailure"])
        self.assertEqual(failed["verify"]["exitCode"], 75)

    def test_unexecuted_acceptance_does_not_mark_apply_invalid(self):
        (self.work / "offline").touch()
        _, state = self.run_remote(pre="echo changed > a.txt", accept="cargo check")
        result = self.cli("apply", "--no-verify", "--keep", state["run"], human=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("验收未完成", result.stdout)
        self.assertNotIn("验收无效", result.stdout)
        self.assertIn("重排", result.stdout)
        self.assertIn("DELEGATE_REMOTE=0", result.stdout)
        result = self.outcome(self.cli("apply", "--json", "--no-verify", "--keep", state["run"]))
        self.assertNotIn("acceptStillValid", result)
        self.assertTrue(result["acceptPending"])
        self.assertIn("not established", result["acceptValidityReason"])

    def test_remote_execution_timeout_remains_failure_not_unexecuted(self):
        repo, state = self.run_remote(accept="true")
        run = Path(state["dir"])
        meta = json.loads((run / "meta.json").read_text())
        meta["acceptTimeoutSeconds"] = 2
        (run / "meta.json").write_text(json.dumps(meta))
        (repo / ".delegate.json").write_text(json.dumps({"applyVerify": "cargo sleep"}))
        result = self.cli("apply", "--json", "--verify", "--keep", state["run"], timeout=20)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        verify = self.outcome(result)["verify"]
        self.assertTrue(verify["timedOut"])
        self.assertFalse(verify["notExecuted"])
        self.assertFalse(verify["infrastructureFailure"])
        self.assertTrue((self.work / "remote-pids").exists(), "command really executed")

    def test_remote_slot_queue_does_not_consume_verify_execution_timeout(self):
        repo, state = self.run_remote(pre="echo changed > a.txt", accept="cargo check")
        run = Path(state["dir"])
        meta = json.loads((run / "meta.json").read_text())
        meta["acceptTimeoutSeconds"] = 3
        (run / "meta.json").write_text(json.dumps(meta))
        # apply uses the source directory's remote slot. Hold that transaction lock,
        # not a heavy lane: this is the second queue inside run_shell.
        dest = "build/" + Path(self.records()[0]["cwd"]).parent.name + "/" + repo.name
        slot = self.work / "state/delegate/lane-remote/slots" / hashlib.sha1(("ci:" + dest).encode()).hexdigest()
        slot.parent.mkdir(parents=True, exist_ok=True)
        with slot.open("a+") as held:
            fcntl.flock(held, fcntl.LOCK_EX)
            proc = subprocess.Popen([str(fixtures.DELEGATE), "apply", "--json", "--verify", "--keep", state["run"]],
                                    cwd=repo, env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            self.addCleanup(lambda: proc.poll() is None and proc.terminate())
            self.until(lambda: bool(list(run.glob("lane-waiting-*"))), "remote slot queued")
            time.sleep(4)
            self.assertIsNone(proc.poll(), "queue must not spend the 3s execution timeout")
        stdout, stderr = proc.communicate(timeout=15)
        self.assertEqual(proc.returncode, 0, stdout + stderr)
        self.assertTrue(self.outcome(subprocess.CompletedProcess([], proc.returncode, stdout, stderr))["verify"]["ok"])

    def test_forwarding_upload_download_paths_arguments_and_exit_codes(self):
        _, state = self.run_remote(pre='cargo fmt; cargo exit 23 "a b" "x\'y"; echo $? > "$DELEGATE_RUN_DIR/code"', accept="cargo exit 255")
        tree = Path(state["worktree"])
        self.assertEqual((tree / "a.txt").read_text(), "formatted\n")
        self.assertEqual((tree / "added.txt").read_text(), "new\n")
        self.assertFalse((tree / "remove.txt").exists())
        self.assertFalse((tree / "target").exists())
        self.assertEqual((Path(state["dir"]) / "code").read_text().strip(), "23")
        self.assertEqual(state["accept"]["exitCode"], 255)
        self.assertEqual(self.records()[1]["argv"], ["exit", "23", "a b", "x'y"])
        self.assertIn(str(tree), state["accept"]["tail"])
        self.assertNotIn(str(self.remote), state["accept"]["tail"])
        slot = Path(self.records()[0]["cwd"])
        self.assertTrue((slot / "target/warm").exists())
        self.assertFalse((slot / ".git").exists())

    def test_ssh_and_rsync_unreachable_never_fall_back_and_accept_is_infrastructure(self):
        for failure in ("offline", "rsync-offline"):
            with self.subTest(failure=failure):
                (self.work / failure).touch()
                _, state = self.run_remote(accept="cargo fmt")
                self.assertEqual(state["accept"]["exitCode"], 75)
                self.assertTrue(state["infrastructureFailure"])
                self.assertIn("remote mode does not fall back to local", state["accept"]["tail"])
                self.assertNotIn("local:", state["accept"]["tail"])
                self.assertEqual(self.records(), [])
                (self.work / failure).unlink()

    def test_agent_deny_precedes_remote_and_remote_precedes_resources(self):
        _, state = self.run_remote('cargo check; echo $? > "$DELEGATE_RUN_DIR/denied-code"; cargo fmt',
            accept="cargo check", config={"agentDeny": [{"argv": ["cargo", "check"], "hint": "small checks only"}],
                "resources": {"compiler": {"commands": [["cargo"]]}}})
        self.assertEqual((Path(state["dir"]) / "denied-code").read_text().strip(), "77")
        self.assertEqual([r["argv"] for r in self.records()], [["fmt"], ["check"]])
        self.assertEqual(state["accept"]["exitCode"], 0)
        self.assertFalse((self.work / "state/delegate/resources").exists())

    def test_measure_is_local_and_unmatched_measure_goes_remote(self):
        cpu = str(min(os.sched_getaffinity(0)))
        _, state = self.run_remote('cargo bench > "$DELEGATE_RUN_DIR/bench"; cargo fmt', accept="true",
            config={"measure": [["cargo", "bench"]], "measureCpus": cpu})
        self.assertIn("local:bench", (Path(state["dir"]) / "bench").read_text())
        self.assertEqual([r["argv"] for r in self.records()], [["fmt"]])

    def test_repository_host_rejected_and_effective_config_overrides_and_disable(self):
        repo = self.repo({"a.txt": "old\n"})
        cfg = repo / ".delegate.json"
        cfg.write_text(json.dumps({"remote": {"host": "ci"}}))
        bad = self.cli("config", cwd=repo)
        self.assertEqual(bad.returncode, 2)
        self.assertIn("remote.host is forbidden", bad.stderr)
        cfg.write_text(json.dumps({"remote": {"programs": ["rustc"], "forwardEnv": ["RUST_LOG"]}}))
        config = json.loads(self.cli("config", cwd=repo).stdout)
        self.assertEqual(config["remote"]["host"], "ci")
        self.assertEqual(config["remote"]["programs"], ["rustc"])
        self.assertIn('"host":"ci"', self.cli("--help", cwd=repo).stdout)
        cfg.write_text(json.dumps({"remote": False}))
        self.assertFalse(json.loads(self.cli("config", cwd=repo).stdout)["remote"])
        cfg.unlink()
        self.env["DELEGATE_REMOTE"] = "0"
        self.assertFalse(json.loads(self.cli("config", cwd=repo).stdout)["remote"])

    def test_environment_profile_setup_evidence_and_verify(self):
        (self.remote / "build").mkdir()
        (self.remote / "build/env.sh").write_text('export PROFILE_VALUE=ready\n')
        self.env["RUST_LOG"] = "trace"
        repo, state = self.run_remote(accept="cargo check", config={
            "env": {"FROM_META": "with spaces", "HOME": str(self.work), "PATH": self.env["PATH"]},
            "remote": {"profile": "env.sh", "forwardEnv": ["RUST_LOG", "PATH", "HOME"]},
            "evidence": "cargo evidence", "worktree": {"setup": ["cargo setup"]}})
        self.assertEqual(state["state"], "delivered", state)
        records = self.records()
        self.assertEqual([r["argv"] for r in records], [["setup"], ["check"], ["evidence"]])
        for record in records:
            self.assertEqual(record["env"]["FROM_META"], "with spaces")
            self.assertEqual(record["env"]["PROFILE_VALUE"], "ready")
            self.assertEqual(record["env"]["RUST_LOG"], "trace")
            self.assertEqual(record["env"]["HOME"], str(self.remote))
            self.assertNotIn(str(self.bin), record["env"]["PATH"])
        # Force a delivered change so apply --verify executes on the merged source.
        (Path(state["worktree"]) / "a.txt").write_text("changed\n")
        applied = self.cli("apply", "--verify", "--keep", state["run"])
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertEqual(self.records()[-1]["argv"], ["check"])
        self.assertEqual(Path(self.records()[-1]["cwd"]).name, repo.name)

    def test_cancel_stops_remote_process_group_and_children(self):
        _, state = self.run_remote(accept="true")
        run = Path(state["dir"])
        env = dict(self.env)
        env.pop("DELEGATE_LANE_HELD", None)
        proc = subprocess.Popen([str(fixtures.DELEGATE), "_remote", str(run), "cargo", "sleep"],
                                cwd=state["worktree"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: proc.poll() is None and proc.kill())
        marker = self.work / "remote-pids"
        deadline = time.monotonic() + 10
        while not marker.exists() and time.monotonic() < deadline:
            time.sleep(.02)
        self.assertTrue(marker.exists())
        pids = [int(p) for p in marker.read_text().split()]
        proc.send_signal(signal.SIGTERM)
        stdout, stderr = proc.communicate(timeout=10)
        self.assertEqual(proc.returncode, 143, stdout + stderr)
        for pid in pids:
            stat = Path(f"/proc/{pid}/stat")
            self.assertTrue(not stat.exists() or stat.read_text().rsplit(") ", 1)[1].startswith("Z "), f"orphan {pid}")
        self.assertIn("/bin/kill -TERM", self.ssh_log.read_text())

    def test_clean_removes_nonpooled_remote_slot(self):
        _, state = self.run_remote()
        slot = Path(self.records()[0]["cwd"])
        self.assertTrue(slot.exists())
        cleaned = self.cli("clean", "--sync", "--no-archive", state["run"])
        self.assertEqual(cleaned.returncode, 0, cleaned.stdout + cleaned.stderr)
        self.assertFalse(slot.exists())

    def test_pool_clean_keeps_remote_warm_target_and_offline_clean_still_removes_local(self):
        _, state = self.run_remote(config={"worktree": {"pool": 1}})
        slot = Path(self.records()[0]["cwd"])
        cleaned = self.cli("clean", "--sync", "--no-archive", state["run"])
        self.assertEqual(cleaned.returncode, 0, cleaned.stdout + cleaned.stderr)
        self.assertTrue((slot / "target/warm").exists())
        # A fresh nonpooled run still cleans locally when SSH is down.
        (self.work / "repo/.delegate.json").unlink()
        _, state = self.run_remote()
        (self.work / "offline").touch()
        cleaned = self.cli("clean", "--sync", "--no-archive", state["run"])
        self.assertEqual(cleaned.returncode, 0, cleaned.stdout + cleaned.stderr)
        self.assertFalse(Path(state["worktree"]).exists())
        self.assertIn("unreachable", cleaned.stderr)

    def test_remote_lane_lists_queue_accounts_wait_and_is_independent_of_local_lane(self):
        _, state = self.run_remote(accept="true")
        run = Path(state["dir"])
        env = dict(self.env, DELEGATE_MAX_HEAVY="1")
        # Hold the local lane while a remote command successfully starts.
        local = subprocess.Popen([str(fixtures.DELEGATE), "lane", "sleep 60"], env=env,
                                 cwd=state["worktree"], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: local.poll() is None and local.terminate())
        env.pop("DELEGATE_LANE_HELD", None)
        first = subprocess.Popen([str(fixtures.DELEGATE), "_remote", str(run), "cargo", "sleep"],
                                 cwd=state["worktree"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: first.poll() is None and first.terminate())
        deadline = time.monotonic() + 10
        while not (self.work / "remote-pids").exists() and time.monotonic() < deadline:
            time.sleep(.02)
        self.assertTrue((self.work / "remote-pids").exists(), "remote must not queue behind local")
        second = subprocess.Popen([str(fixtures.DELEGATE), "_remote", str(run), "cargo", "check"],
                                  cwd=state["worktree"], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: second.poll() is None and second.terminate())
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            status = self.outcome(self.cli("status", state["run"]))
            if status["remoteLane"]["queued"] == 1:
                break
            time.sleep(.02)
        self.assertEqual(status["remoteLane"]["queued"], 1)
        listing = self.cli("lane", cwd=state["worktree"]).stdout
        self.assertIn("remote running", listing)
        self.assertIn("remote queued", listing)
        time.sleep(.2)
        first.terminate()
        first.communicate(timeout=10)
        stdout, stderr = second.communicate(timeout=10)
        self.assertEqual(second.returncode, 0, stdout + stderr)
        self.assertIn(b"queued behind", stderr)
        self.assertGreater(sum(float(s) for s in (run / "lane-wait").read_text().split()), .1)
        local.terminate()
        local.communicate(timeout=10)

    def test_infrastructure_failure_retry_does_not_spend_rework_even_at_zero_budget(self):
        (self.work / "offline").touch()
        _, state = self.run_remote('echo changed > a.txt', config={"maxRework": 0})
        self.assertTrue(state["infrastructureFailure"])
        (self.work / "offline").unlink()
        self.fake_pi([fixtures.answer("retry"), fixtures.SETTLED])
        retry = self.cli("reply", "--wait", state["run"], "retry now")
        self.assertEqual(retry.returncode, 0, retry.stdout + retry.stderr)
        state = self.outcome(retry)
        meta = json.loads((Path(state["dir"]) / "meta.json").read_text())
        self.assertEqual(meta["rework"]["kind"], "continuation")
        self.assertEqual(meta["rework"]["used"], 0)

    def test_non_utf8_unterminated_output_is_preserved_and_stdin_is_forwarded(self):
        _, state = self.run_remote(accept="true")
        result = subprocess.run([str(fixtures.DELEGATE), "_remote", state["dir"], "cargo", "binary"],
                                cwd=state["worktree"], env=self.env, capture_output=True)
        self.assertEqual(result.returncode, 7, result.stderr)
        self.assertEqual(result.stdout, b"raw\xff:" + state["worktree"].encode() + b"/a.txt")
        self.assertEqual(result.stderr, b"err\xfe")
        # Use a separately routed program so it can consume stdin unchanged.
        cat = self.remote / "bin/read-input"
        cat.write_text("#!/bin/sh\ncat\n")
        cat.chmod(0o755)
        result = subprocess.run([str(fixtures.DELEGATE), "_remote", state["dir"], "read-input"],
                                cwd=state["worktree"], env=self.env, input=b"hello\x00world", capture_output=True)
        self.assertEqual((result.returncode, result.stdout), (0, b"hello\x00world"))

    def test_slice_sources_profile_inside_service_and_cancels_unit(self):
        calls = self.work / "systemd-calls"
        service = self.remote / "bin/systemd-run"
        service.write_text(f"#!{sys.executable}\n" + f'''import os, sys, json
from pathlib import Path
with open({str(calls)!r}, "a") as log: log.write(json.dumps(sys.argv[1:]) + "\\n")
a = sys.argv[1:]
while a and a[0].startswith("-"):
    key = a.pop(0)
    if key == "-E":
        k, v = a.pop(0).split("=",1)
        os.environ[k] = v
    if key.startswith("--working-directory="): os.chdir(key.split("=",1)[1])
os.execvp(a[0],a)
''')
        service.chmod(0o755)
        ctl = self.remote / "bin/systemctl"
        ctl.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> ' + shlex.quote(str(calls)) + '\n')
        ctl.chmod(0o755)
        (self.remote / "build").mkdir()
        (self.remote / "build/env.sh").write_text('export PROFILE_VALUE=service\n')
        _, state = self.run_remote(accept="cargo check", config={"remote": {"slice": "build.slice", "profile": "env.sh"}})
        self.assertEqual(state["state"], "delivered", state)
        args = json.loads(calls.read_text().splitlines()[0])
        for flag in ("--user", "--wait", "--pipe", "--collect", "--quiet", "--slice=build.slice"):
            self.assertIn(flag, args)
        self.assertEqual(self.records()[0]["env"]["PROFILE_VALUE"], "service")
        proc = subprocess.Popen([str(fixtures.DELEGATE), "_remote", state["dir"], "cargo", "sleep"],
                                cwd=state["worktree"], env=self.env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: proc.poll() is None and proc.terminate())
        deadline = time.monotonic() + 10
        while not (self.work / "remote-pids").exists() and time.monotonic() < deadline:
            time.sleep(.02)
        self.assertTrue((self.work / "remote-pids").exists())
        proc.terminate()
        proc.communicate(timeout=10)
        self.assertEqual(proc.returncode, 143)
        self.assertIn("--user stop delegate-", calls.read_text())

    def test_remote_check_and_lane_command_stays_local(self):
        repo = self.repo({"a.txt": "old\n"})
        checked = self.cli("remote", "check", cwd=repo)
        self.assertEqual(checked.returncode, 0, checked.stderr)
        self.assertIn("ok (ssh + rsync + programs)", checked.stdout)
        self.assertEqual(self.records()[0]["argv"], ["--version"])
        lane = self.cli("lane", "cargo", "check", cwd=repo)
        self.assertEqual(lane.returncode, 0, lane.stderr)
        self.assertIn("local:check", lane.stdout)
        self.assertEqual(len(self.records()), 1)

    def test_remote_syncs_worktree_copy_and_link_for_acceptance_equivalence(self):
        repo = self.repo({
            "a.txt": "old\n",
            ".gitignore": "target/\nnode_modules/\n.local/\n",
            ".delegate.json": json.dumps({
                "worktree": {"copy": [".local/providers"], "link": ["third_party/pi"]}
            }),
        })
        (repo / ".local/providers").mkdir(parents=True)
        (repo / ".local/providers/model.json").write_text("ok\n")
        (repo / "third_party/pi").mkdir(parents=True)
        (repo / "third_party/pi/spec.md").write_text("pi-spec\n")
        # Case 1: remote host has the same path for worktree.link (symlinked on remote) + copy synced.
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "cargo check-managed", "task"))
        self.assertEqual(state["state"], "delivered", state)
        self.assertEqual(self.records()[-1]["argv"], ["check-managed"])
        remote_slot = Path(self.records()[-1]["cwd"])
        self.assertTrue((remote_slot / "third_party/pi").is_symlink())
        self.assertEqual((remote_slot / ".local/providers/model.json").read_text(), "ok\n")
        # Local worktree managed paths remain intact after download sync.
        tree = Path(state["worktree"])
        self.assertTrue((tree / "third_party/pi").is_symlink())
        self.assertEqual((tree / ".local/providers/model.json").read_text(), "ok\n")
        # Case 2: remote host does NOT have worktree.link at the same path; small target is synced directly.
        (self.work / "no-remote-link").touch()
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        state2 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "cargo check-managed", "task2"))
        self.assertEqual(state2["state"], "delivered", state2)
        remote_slot2 = Path(self.records()[-1]["cwd"])
        self.assertFalse((remote_slot2 / "third_party/pi").is_symlink())
        self.assertEqual((remote_slot2 / "third_party/pi/spec.md").read_text(), "pi-spec\n")

    def test_remote_acceptance_falls_back_locally_when_managed_path_exceeds_limit(self):
        repo = self.repo({
            "a.txt": "old\n",
            ".gitignore": "target/\n.local/\n",
            ".delegate.json": json.dumps({
                "worktree": {"copy": [".local/providers"]}
            }),
        })
        (repo / ".local/providers").mkdir(parents=True)
        (repo / ".local/providers/big.bin").write_bytes(b"x" * 128)
        self.env["DELEGATE_REMOTE_COPY_MAX_BYTES"] = "16"
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo,
                                      "--accept", "cargo check-managed-local", "task"))
        self.assertEqual(state["state"], "delivered", state)
        self.assertEqual(self.records(), [], "remote cargo should not run when managed path is missing")
        self.assertEqual(state["accept"]["localFallback"]["paths"], [".local/providers"])
        self.assertIn("本地回退", self.cli("status", state["run"], human=True).stdout)
        # SSH offline still fails closed and never falls back locally.
        (self.work / "offline").touch()
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        failed = self.outcome(self.cli("run", "--worktree", "--workdir", repo,
                                       "--accept", "cargo check-managed-local", "offline-task"))
        self.assertEqual(failed["state"], "rejected")
        self.assertTrue(failed["infrastructureFailure"])
        self.assertNotIn("localFallback", failed["accept"])


if __name__ == "__main__":
    unittest.main()
