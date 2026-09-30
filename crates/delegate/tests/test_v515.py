"""5.15-5.17 black-box regressions; loaded by the repository's conformance suite."""
import importlib.util
import json
import shlex
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
fixtures = sys.modules.get("tests.test_delegate") or sys.modules.get("test_delegate")
if fixtures is None:
    spec = importlib.util.spec_from_file_location("delegate_fixtures", ROOT / "tests/test_delegate.py")
    fixtures = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixtures)


class V515Tests(unittest.TestCase):
    def setUp(self):
        fixtures.DelegateTests.setUp(self)
        self.env["XDG_CONFIG_HOME"] = str(self.work / "config")

    fake_pi = fixtures.DelegateTests.fake_pi
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until
    outcome = fixtures.DelegateTests.outcome

    def cli(self, *args, human=False, **kwargs):
        return fixtures.DelegateTests.cli(self, *args, human=human, **kwargs)

    def user_config(self, value):
        path = Path(self.env["XDG_CONFIG_HOME"]) / "delegate/config.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value))
        return path

    def meta(self, state):
        return json.loads((Path(state["dir"]) / "meta.json").read_text())

    def test_v517_user_only_and_home_fallback_outside_git(self):
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre='printf "%s" "$SHARED" > "$DELEGATE_RUN_DIR/injected"; echo changed > a.txt')
        self.user_config({"env": {"SHARED": "user"}, "accept": 'test "$SHARED" = user',
                          "maxRework": 0, "agentDeny": [{"argv": ["cargo", "check"], "hint": "related only"}]})
        first = self.outcome(self.cli("run", "--agent", "pi", "task"))
        self.assertEqual((first["state"], first["configSources"]), ("delivered", ["user"]))
        run = Path(first["dir"])
        self.assertEqual((run / "injected").read_text(), "user")
        self.assertEqual(json.loads((run / "summary.json").read_text())["configSources"], ["user"])
        self.assertEqual(self.outcome(self.cli("status", first["run"]))["configSources"], ["user"])
        self.assertNotIn("configSources", self.cli("status", first["run"], human=True).stdout)
        self.env["HOME"] = str(self.work / "home")
        self.env["XDG_CONFIG_HOME"] = str(self.work / "home/.config")
        self.user_config({"env": {"SHARED": "user"}, "accept": "true"})
        self.env["XDG_CONFIG_HOME"] = ""
        fallback = self.outcome(self.cli("run", "--agent", "pi", "task"))
        self.assertEqual(fallback["configSources"], ["user"])

    def test_v517_repo_only_and_builtin_defaults(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        first = self.outcome(self.cli("run", "--agent", "pi", "--workdir", repo, "task"))
        self.assertEqual(first["configSources"], [])
        self.assertEqual(self.meta(first)["configCapacity"], {"maxActive": 8, "maxCodex": 4, "maxHeavy": 1})
        (repo / ".delegate.json").write_text(json.dumps({"accept": "true", "env": {"LOCAL": "repo"},
                                                        "maxRework": 0}))
        state = self.outcome(self.cli("run", "--agent", "pi", "--workdir", repo, "task"))
        self.assertEqual((state["state"], state["configSources"]), ("delivered", ["repo"]))
        self.assertEqual(self.meta(state)["env"], {"LOCAL": "repo"})
        rejected = self.cli("reply", state["run"], "fix")
        self.assertEqual(rejected.returncode, 2)
        self.assertIn("rework limit reached (0/0)", rejected.stderr)
        self.user_config({"maxHeavy": 7, "env": {"LOCAL": "user"}})
        reply = self.outcome(self.cli("reply", "--wait", "--minor", state["run"], "small fix"))
        self.assertEqual(reply["configSources"], ["user", "repo"])
        self.assertEqual(self.meta(reply)["configCapacity"]["maxHeavy"], 7)
        self.assertEqual(self.meta(reply)["env"], {"LOCAL": "repo"})

    def test_v517_merge_env_accept_deny_and_rework(self):
        repo = self.repo({"a.txt": "old\n"})
        self.user_config({"env": {"SHARED": "user", "USER_ONLY": "yes"}, "accept": "false", "maxRework": 0,
                          "agentDeny": [{"argv": ["cargo", "check"], "hint": "user hint"},
                                        {"argv": ["pnpm", "check"], "hint": "user pnpm"}]})
        (repo / ".delegate.json").write_text(json.dumps({
            "env": {"SHARED": "repo", "REPO_ONLY": "yes"}, "accept": 'test "$SHARED" = repo', "maxRework": None,
            "agentDeny": [{"argv": ["cargo", "check"], "hint": "repo hint"},
                          {"argv": ["cargo", "test"], "hint": "repo test"}]}))
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre='cargo check; cargo test; pnpm check; echo changed >> a.txt')
        state = self.outcome(self.cli("run", "--agent", "pi", "--workdir", repo, "task"))
        self.assertEqual((state["state"], state["configSources"], state["denied"]),
                         ("delivered", ["user", "repo"], 3))
        meta = self.meta(state)
        self.assertEqual(meta["env"], {"SHARED": "repo", "USER_ONLY": "yes", "REPO_ONLY": "yes"})
        self.assertEqual(len(meta["agentDeny"]), 3)
        self.assertEqual(meta["agentDeny"][0]["hint"], "repo hint")
        reply = self.outcome(self.cli("reply", "--wait", state["run"], "fix"))
        self.assertEqual(reply["configSources"], ["user", "repo"])
        self.assertIsNone(self.meta(reply)["rework"]["limit"])
        (repo / ".delegate.json").unlink()
        denied = self.cli("reply", reply["run"], "again")
        self.assertEqual(denied.returncode, 2)
        self.assertIn("rework limit reached", denied.stderr)

    def test_v517_repository_override_cannot_hide_invalid_user_config(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        path = self.user_config({"accept": [], "maxActive": "bad"})
        (repo / ".delegate.json").write_text('{"accept":"true","maxActive":8}')
        self.env["DELEGATE_MAX_ACTIVE"] = "8"
        result = self.cli("start", "--agent", "pi", "--workdir", repo, "task")
        self.assertEqual(result.returncode, 2)
        self.assertIn(str(path), result.stderr)
        self.assertIn("accept", result.stderr)
        self.assertFalse((self.work / "runs").exists())

    def test_v517_repo_allow_revokes_exact_user_argv(self):
        repo = self.deny_repo({"agentDeny": [
            {"argv": ["cargo", "check"], "allow": True},
            {"argv": ["cargo", "check"], "allow": True}]})
        self.user_config({"agentDeny": [
            {"argv": ["cargo", "check"], "hint": "blocked check"},
            {"argv": ["cargo", "test"], "hint": "blocked test"}]})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre='''
cargo check --all > "$DELEGATE_RUN_DIR/allowed"
echo $? > "$DELEGATE_RUN_DIR/allowed-code"
cargo test; echo $? > "$DELEGATE_RUN_DIR/denied-code"
''')
        state = self.outcome(self.cli("run", "--agent", "pi", "--workdir", repo, "task"))
        self.assertEqual(state["denied"], 1)
        run = Path(state["dir"])
        self.assertEqual((run / "allowed-code").read_text(), "0\n")
        self.assertEqual((run / "denied-code").read_text(), "77\n")
        self.assertEqual(self.meta(state)["agentDeny"], [{"argv": ["cargo", "test"], "hint": "blocked test"}])

    def test_v517_capacity_precedence_and_admission(self):
        repo = self.repo({"a.txt": "old\n"})
        self.user_config({"maxActive": 1, "maxCodex": 2, "maxHeavy": 3})
        gate = self.work / "gate"
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre=f'while [ ! -f {gate} ]; do sleep .02; done')
        first = self.outcome(self.cli("start", "--agent", "pi", "--read-only", "--in-place", "--workdir", repo, "task"))
        self.addCleanup(self.cli, "stop", first["run"])
        full = self.cli("start", "--agent", "pi", "--read-only", "task")
        self.assertEqual(full.returncode, 2)
        self.assertIn("MAX_ACTIVE=1", full.stderr)
        (repo / ".delegate.json").write_text(json.dumps({"maxActive": 2, "maxCodex": 3, "maxHeavy": 4}))
        second = self.outcome(self.cli("start", "--agent", "pi", "--read-only", "--in-place", "--workdir", repo, "task"))
        self.addCleanup(self.cli, "stop", second["run"])
        self.assertEqual(self.meta(second)["configCapacity"], {"maxActive": 2, "maxCodex": 3, "maxHeavy": 4})
        self.env.update(DELEGATE_MAX_ACTIVE="0", DELEGATE_MAX_CODEX="0", DELEGATE_MAX_HEAVY="0")
        third = self.outcome(self.cli("start", "--agent", "pi", "--read-only", "--in-place", "--workdir", repo, "task"))
        self.addCleanup(self.cli, "stop", third["run"])
        self.assertEqual(self.meta(third)["configCapacity"], {"maxActive": 0, "maxCodex": 0, "maxHeavy": 0})
        gate.touch()
        self.assertEqual(self.cli("wait", first["run"], second["run"], third["run"]).returncode, 0)

    def test_v517_ignored_repo_fields_warn_once_and_accept_cli_overrides(self):
        repo = self.repo({"a.txt": "old\n"})
        self.user_config({"worktree": 123, "generated": "invalid", "applyVerify": [], "accept": "false"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        result = self.cli("run", "--agent", "pi", "--worktree", "--workdir", repo, "--accept", "true", "task")
        state = self.outcome(result)
        self.assertEqual(state["state"], "delivered")
        self.assertEqual(result.stderr.count("ignoring repository-only fields:"), 1)
        self.assertIn("worktree, generated, applyVerify", result.stderr)
        self.assertEqual(self.cli("apply", state["run"]).returncode, 0)
        for flags in (("--no-accept",), ("--read-only",)):
            state = self.outcome(self.cli("run", "--agent", "pi", "--workdir", repo, *flags, "task"))
            self.assertEqual(state["state"], "answered")

    def test_v517_codex_capacity_and_legacy_env_override(self):
        self.user_config({"maxCodex": 1})
        gate = self.work / "gate"
        fixtures.DelegateTests.fake_codex(self, fixtures.codex_events(),
                                         pre=f'while [ ! -f {gate} ]; do sleep .02; done')
        first = self.outcome(self.cli("start", "--agent", "codex", "--read-only", "task"))
        self.addCleanup(self.cli, "stop", first["run"])
        full = self.cli("start", "--agent", "codex", "--read-only", "task")
        self.assertEqual(full.returncode, 2)
        self.assertIn("MAX_CODEX=1", full.stderr)
        self.env["PI_DELEGATE_MAX_CODEX"] = "0"
        second = self.outcome(self.cli("start", "--agent", "codex", "--read-only", "task"))
        self.addCleanup(self.cli, "stop", second["run"])
        self.assertEqual(self.meta(second)["configCapacity"]["maxCodex"], 0)
        self.env["DELEGATE_MAX_CODEX"] = "1"
        self.assertEqual(self.cli("start", "--agent", "codex", "--read-only", "task").returncode, 2)
        gate.touch()
        self.assertEqual(self.cli("wait", first["run"], second["run"]).returncode, 0)

    def test_v517_lane_capacity_uses_user_repo_and_environment(self):
        repo = self.repo({"a.txt": "old\n"})
        self.user_config({"maxHeavy": 1})
        gate = self.work / "gate"
        command = f'while [ ! -f {gate} ]; do sleep .02; done'
        processes = []
        for label in ("one", "two"):
            proc = subprocess.Popen([str(fixtures.DELEGATE), "lane", "--label", label, command],
                                    env=self.env, cwd=repo, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            processes.append(proc)
            self.addCleanup(lambda p=proc: p.poll() is None and p.terminate())
            self.until(lambda: label in self.cli("lane", cwd=repo).stdout, "lane ticket")
        listing = self.cli("lane", cwd=repo).stdout
        self.assertEqual((listing.count("running"), listing.count("queued")), (1, 1))
        (repo / ".delegate.json").write_text('{"maxHeavy":2}')
        listing = self.cli("lane", cwd=repo).stdout
        self.assertEqual(listing.count("running"), 2)
        self.env["DELEGATE_MAX_HEAVY"] = "1"
        listing = self.cli("lane", cwd=repo).stdout
        self.assertEqual(listing.count("queued"), 1)
        # A task's lane uses the task's repository even when its caller cwd differs.
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre=f'{shlex.quote(str(fixtures.DELEGATE))} lane > "$DELEGATE_RUN_DIR/holders"')
        self.env.pop("DELEGATE_MAX_HEAVY")
        state = self.outcome(self.cli("run", "--agent", "pi", "--worktree", "--workdir", repo, "task"))
        self.assertEqual((Path(state["dir"]) / "holders").read_text().count("running"), 2)
        gate.touch()
        for proc in processes:
            proc.communicate(timeout=10)
            self.assertEqual(proc.returncode, 0)

    def test_v517_invalid_config_reports_file_field_and_never_starts(self):
        repo = self.repo({"a.txt": "old\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        bad = [("env", {"BAD": 1}), ("accept", []), ("maxRework", -1), ("maxActive", "1"),
               ("maxCodex", None), ("maxHeavy", 1.5), ("agentDeny", [{"argv": ["cargo"], "hint": ""}]),
               ("agentDeny", [{"argv": ["cargo"], "allow": "yes"}])]
        for user in (True, False):
            for field, value in bad:
                with self.subTest(user=user, field=field, value=value):
                    path = self.user_config({field: value}) if user else repo / ".delegate.json"
                    path.write_text(json.dumps({field: value}))
                    result = self.cli("start", "--agent", "pi", "--workdir", repo, "task")
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn(str(path), result.stderr)
                    self.assertIn(field, result.stderr)
                    result = self.cli("start", "--agent", "pi", "--read-only", "--workdir", repo, "task")
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn(str(path), result.stderr)
                    self.assertIn(field, result.stderr)
                    path.unlink()
            for text in ("{", "[]"):
                path = self.user_config({}) if user else repo / ".delegate.json"
                path.write_text(text)
                result = self.cli("start", "--agent", "pi", "--workdir", repo, "task")
                self.assertEqual(result.returncode, 2)
                self.assertIn(str(path), result.stderr)
                self.assertIn("config", result.stderr)
                path.unlink()
        for field, value in (("worktree", {"copy": [1]}), ("generated", {"paths": [1]}), ("applyVerify", [])):
            path = repo / ".delegate.json"
            path.write_text(json.dumps({field: value}))
            result = self.cli("start", "--agent", "pi", "--workdir", repo, "task")
            self.assertEqual(result.returncode, 2)
            self.assertIn(str(path), result.stderr)
            self.assertIn(field, result.stderr)
        self.assertFalse((self.work / "runs").exists())
        self.assertFalse((self.work / "pi.log").exists())

    def test_managed_gitlink_and_nonmanaged_skip_worktree(self):
        repo = self.repo({"a.txt": "old\n", "other.txt": "other\n"})
        sub = self.work / "library"
        subprocess.run(["git", "init", "-q", str(sub)], check=True)
        subprocess.run(["git", "-C", str(sub), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "--allow-empty", "-qm", "lib"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "protocol.file.allow=always", "submodule",
                        "add", "-q", str(sub), "third_party/pi"], check=True)
        subprocess.run(["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t",
                        "commit", "-qm", "sub"], check=True)
        (repo / ".delegate.json").write_text(json.dumps({"worktree": {"link": ["third_party/pi"]}}))
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo new > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        self.assertIs(state["accept"]["snapshotComplete"], True)
        self.assertEqual(state["accept"]["excluded"], ["third_party/pi"])
        applied = self.outcome(self.cli("apply", state["run"]))
        self.assertIs(applied["acceptStillValid"], True)
        self.assertEqual(applied["excluded"], ["third_party/pi"])
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre="git update-index --skip-worktree other.txt; echo next > a.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "task"))
        self.assertIs(state["accept"]["snapshotComplete"], False)
        self.assertIn("index", state["accept"]["snapshotReason"])

    def test_generated_protection_allows_generator_rejects_manual_drift(self):
        repo = self.repo({"a.txt": "old\n", "gen/out": "old\n"})
        (repo / ".delegate.json").write_text(json.dumps({"generated": {
            "paths": ["gen/"], "command": "cp a.txt gen/out"}}))
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo new > a.txt; cp a.txt gen/out")
        result = self.cli("run", "--worktree", "--workdir", repo, "--protect-reason", "gen/", "generated",
                          "--accept", "cmp a.txt gen/out", "task")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("may only be changed", result.stderr)
        state = self.outcome(result)
        self.assertNotIn("protectViolation", state)
        self.assertIn("may only be changed by the generator", (Path(state["dir"]) / "prompt.md").read_text())
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo new > a.txt; echo manual > gen/out")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--protect", "gen/out",
                                     "--accept", "true", "task"))
        self.assertEqual(state["state"], "rejected")
        self.assertEqual(state["protectViolation"], ["gen/out"])
        self.assertIn("生成物被手改", state["error"])
        self.assertNotIn("accept", state)
        (repo / ".delegate.json").write_text("{}")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--protect", "gen/", "task"))
        self.assertEqual(state["protectViolation"], ["gen/out"])

    def test_duplicate_wait_and_dead_waiter_lock(self):
        gate = self.work / "gate"
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre=f"while [ ! -f {gate} ]; do sleep .02; done")
        state = self.outcome(self.cli("start", "--read-only", "--name", "one", "task"))
        self.addCleanup(self.cli, "stop", state["run"])
        lock = Path(state["dir"]) / "waiter.lock"
        first = subprocess.Popen([str(fixtures.DELEGATE), "wait", state["run"]],
                                 env=self.env, cwd=self.work, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: first.poll() is None and first.kill())
        self.until(lambda: lock.exists() and lock.read_text().strip() == str(first.pid), "waiter lock")
        for args in ((state["run"],), ("--any", state["run"]), ("--stream", state["run"]), ("--machine",)):
            out = self.cli("wait", *args)
            self.assertEqual(out.returncode, 76, out.stderr)
            self.assertEqual(out.stdout, "")
            self.assertIn(f"pid {first.pid}", out.stderr)
        first.kill()
        first.communicate(timeout=10)
        gate.touch()
        out = self.cli("wait", state["run"], human=True)
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertIn("done", out.stdout)

    def test_wait_any_skips_covered_task_and_delivers_free_task(self):
        gate = self.work / "gate"
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre=f'case "$*" in *one*) while [ ! -f {gate} ]; do sleep .02; done;; esac')
        one = self.outcome(self.cli("start", "--read-only", "--name", "one", "task"))
        self.addCleanup(self.cli, "stop", one["run"])
        two = self.outcome(self.cli("start", "--read-only", "--name", "two", "task"))
        lock = Path(one["dir"]) / "waiter.lock"
        first = subprocess.Popen([str(fixtures.DELEGATE), "wait", one["run"]],
                                 env=self.env, cwd=self.work, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.addCleanup(lambda: first.poll() is None and first.kill())
        self.until(lambda: lock.exists() and lock.read_text().strip() == str(first.pid), "waiter lock")
        out = self.cli("wait", "--any", one["run"], two["run"])
        self.assertEqual(out.returncode, 0, out.stderr)
        self.assertEqual(self.outcome(out)["name"], "two")
        self.assertNotIn('"name":"one"', out.stdout)
        gate.touch()
        first.communicate(timeout=10)

    def deny_repo(self, config=None):
        repo = self.repo({"a.txt": "old\n", "gen/out": "old\n"})
        settings = {"agentDeny": [
            {"argv": ["cargo", "xtask", "check"], "hint": "Full check belongs to the caller; use cargo test -p crate"},
            {"argv": ["pnpm", "check"], "hint": "Use a package check"},
        ]}
        settings.update(config or {})
        (repo / ".delegate.json").write_text(json.dumps(settings))
        tool = self.bin / "tool"
        tool.write_text('#!/bin/sh\n'
                        'if [ "$1 $2" = "xtask generate" ]; then cp a.txt gen/out; exit; fi\n'
                        'printf "%s\\n" "$@"\ncat\nexit "${TOOL_EXIT:-0}"\n')
        tool.chmod(0o755)
        for name in ("cargo", "pnpm"):
            (self.bin / name).symlink_to(tool)
        return repo

    def test_agent_deny_direct_lane_and_environment_cannot_bypass(self):
        repo = self.deny_repo()
        delegate = shlex.quote(str(fixtures.DELEGATE))
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre=f'''
cargo xtask check --all > "$DELEGATE_RUN_DIR/blocked-output"
echo $? > "$DELEGATE_RUN_DIR/direct-code"
DELEGATE_ALLOW_HEAVY=1 cargo xtask check
echo $? > "$DELEGATE_RUN_DIR/env-code"
{delegate} lane cargo xtask check
echo $? > "$DELEGATE_RUN_DIR/lane-code"
{delegate} lane 'pnpm check --all'
echo $? > "$DELEGATE_RUN_DIR/shell-lane-code"
DELEGATE_LANE_HELD=1 {delegate} lane cargo xtask check
echo $? > "$DELEGATE_RUN_DIR/held-code"
''')
        state = self.outcome(self.cli("run", "--agent", "pi", "--workdir", repo,
                                     "--accept", "cargo xtask check", "task"))
        self.assertEqual(state["state"], "delivered")
        self.assertEqual(state["denied"], 5)
        run = Path(state["dir"])
        for name in ("direct", "env", "lane", "shell-lane", "held"):
            self.assertEqual((run / f"{name}-code").read_text(), "77\n")
        self.assertEqual((run / "blocked-output").read_text(), "")
        self.assertIn("Full check belongs to the caller", (run / "stderr.log").read_text())
        self.assertEqual(json.loads((run / "summary.json").read_text())["denied"], 5)
        self.assertIn("xtask\ncheck\n", (run / "accept.log").read_text())
        self.assertIn("拦下 5 次全量检查", self.cli("status", state["run"], human=True).stdout)
        self.assertEqual(self.outcome(self.cli("status", state["run"]))["denied"], 5)
        caller = self.cli("lane", "cargo", "xtask", "check", cwd=repo)
        self.assertEqual((caller.returncode, caller.stdout), (0, "xtask\ncheck\n"))
        reply = self.outcome(self.cli("reply", "--wait", state["run"], "continue"))
        self.assertEqual(reply["denied"], 5)

    def test_agent_deny_passthrough_preserves_argv_exit_stdin_and_stdout(self):
        repo = self.deny_repo()
        args = ["test", "-p", "a b", "", "quote'\"$;", "line\nbreak"]
        command = " ".join(map(shlex.quote, args))
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre=f'''
printf 'input\\n' | TOOL_EXIT=23 cargo {command} > "$DELEGATE_RUN_DIR/output"
echo $? > "$DELEGATE_RUN_DIR/code"
cargo --locked xtask check > "$DELEGATE_RUN_DIR/global-option"
''')
        state = self.outcome(self.cli("run", "--agent", "pi", "--worktree", "--workdir", repo, "task"))
        run = Path(state["dir"])
        self.assertEqual((run / "output").read_text(), "\n".join(args) + "\ninput\n")
        self.assertEqual((run / "code").read_text(), "23\n")
        self.assertEqual((run / "global-option").read_text(), "--locked\nxtask\ncheck\n")
        self.assertEqual(state["denied"], 0)
        shim = (run / "agent-shims/cargo").read_text()
        self.assertIn(str(self.bin / "cargo"), shim)  # keep multicall symlink's name

    def test_agent_deny_setup_generation_and_apply_verify_are_unrestricted(self):
        repo = self.deny_repo({"worktree": {"setup": ["cargo xtask check"]},
                               "agentDeny": [{"argv": ["cargo", "xtask"], "hint": "caller only"}],
                               "generated": {"paths": ["gen/"], "command": "cargo xtask generate"},
                               "applyVerify": "cargo xtask check"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo new > a.txt; cargo xtask generate")
        state = self.outcome(self.cli("run", "--agent", "pi", "--worktree", "--workdir", repo,
                                     "--accept", "cargo xtask check", "task"))
        self.assertEqual(state["state"], "delivered")
        self.assertEqual(state["denied"], 1)
        self.assertIn("[exit 0]", (Path(state["dir"]) / "setup.log").read_text())
        result = self.cli("apply", state["run"], "--verify")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIs(self.outcome(result)["verify"]["ok"], True)
        self.assertEqual((repo / "gen/out").read_text(), "new\n")
        self.assertEqual(self.outcome(self.cli("status", state["run"]))["denied"], 1)

    def test_agent_deny_uses_configured_path_and_both_agents(self):
        repo = self.deny_repo({"env": {"PATH": self.env["PATH"]}})
        fixtures.DelegateTests.fake_codex(self, fixtures.codex_events(),
                                         pre='cargo xtask check; echo $? > "$DELEGATE_RUN_DIR/code"')
        state = self.outcome(self.cli("run", "--agent", "codex", "--workdir", repo, "task"))
        self.assertEqual(state["denied"], 1)
        self.assertEqual((Path(state["dir"]) / "code").read_text(), "77\n")

    def test_agent_deny_missing_config_and_invalid_rules(self):
        repo = self.deny_repo()
        (repo / ".delegate.json").write_text("{}")
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="cargo xtask check")
        state = self.outcome(self.cli("run", "--agent", "pi", "--workdir", repo, "task"))
        self.assertNotIn("denied", state)
        self.assertFalse((Path(state["dir"]) / "agent-shims").exists())
        for rules in ({}, [{"argv": [], "hint": "no"}],
                      [{"argv": ["../cargo"], "hint": "no"}],
                      [{"argv": ["cargo", 1], "hint": "no"}],
                      [{"argv": ["cargo"], "hint": ""}]):
            with self.subTest(rules=rules):
                (repo / ".delegate.json").write_text(json.dumps({"agentDeny": rules}))
                result = self.cli("start", "--agent", "pi", "--workdir", repo, "task")
                self.assertEqual(result.returncode, 2)
                self.assertIn("agentDeny", result.stderr)

    def test_agent_deny_resolves_tools_after_setup(self):
        repo = self.deny_repo({
            "env": {"PATH": "tools:" + self.env["PATH"]},
            "worktree": {"setup": ["mkdir tools; printf '#!/bin/sh\\necho setup-tool\\n' > tools/cargo; chmod +x tools/cargo"]},
        })
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED],
                     pre='cargo test > "$DELEGATE_RUN_DIR/output"')
        state = self.outcome(self.cli("run", "--agent", "pi", "--worktree", "--workdir", repo, "task"))
        self.assertEqual(state["state"], "answered")
        self.assertEqual((Path(state["dir"]) / "output").read_text(), "setup-tool\n")

    def test_timeout_continuations_keep_rework_budget_and_copyable_next(self):
        repo = self.repo({"a.txt": "old\n"})
        self.env["DELEGATE_TIMEOUT_GRACE"] = "0"
        self.fake_pi([fixtures.answer("later"), fixtures.SETTLED], pre="echo partial >> a.txt", sleep=10)
        first = self.outcome(self.cli("run", "--agent", "pi", "--worktree", "--workdir", repo,
                                     "--name", "job", "--timeout", ".2s", "task"))
        self.assertEqual(first["state"], "timeout")
        self.assertTrue(first["next"].startswith(str(fixtures.DELEGATE) + " reply "))
        self.assertIn("不计返工次数", self.cli("status", first["run"], human=True).stdout)
        for _ in range(2):
            self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="seq 1 80 >> a.txt", sleep=10)
            continued = self.outcome(self.cli("reply", "--wait", "job", "continue"))
            self.assertEqual(continued["state"], "timeout")
            meta = json.loads((Path(continued["dir"]) / "meta.json").read_text())
            self.assertEqual(meta["continuation"], "timeout")
            self.assertEqual(meta["rework"]["used"], 0)
            self.assertEqual(continued["worktree"], first["worktree"])
        self.fake_pi([fixtures.answer("finished"), fixtures.SETTLED], pre="echo finished >> a.txt")
        continued = self.outcome(self.cli("reply", "--wait", "--timeout", "5s", "job", "continue"))
        self.assertEqual(continued["state"], "answered")
        self.fake_pi([fixtures.answer("fixed"), fixtures.SETTLED], pre="echo fixed >> a.txt")
        rework = self.outcome(self.cli("reply", "--wait", "job", "fix"))
        meta = json.loads((Path(rework["dir"]) / "meta.json").read_text())
        self.assertNotIn("continuation", meta)
        self.assertEqual(meta["rework"]["used"], 1)
        self.assertEqual(self.cli("reply", "job", "again").returncode, 2)

    def test_timeout_continuation_allowed_after_rework_budget_is_spent(self):
        repo = self.repo({"a.txt": "old\n"})
        self.env["DELEGATE_TIMEOUT_GRACE"] = "0"
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo first >> a.txt")
        self.outcome(self.cli("run", "--agent", "pi", "--worktree", "--workdir", repo,
                             "--name", "job", "--accept", "false", "task"))
        self.fake_pi([fixtures.answer("later"), fixtures.SETTLED], pre="echo fix >> a.txt", sleep=10)
        timed = self.outcome(self.cli("reply", "--wait", "--timeout", ".2s", "job", "fix"))
        self.assertEqual(timed["state"], "timeout")
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="seq 1 80 >> a.txt")
        continued = self.outcome(self.cli("reply", "--wait", "--minor", "--timeout", "5s", "job", "continue"))
        self.assertEqual(continued["state"], "rejected")
        meta = json.loads((Path(continued["dir"]) / "meta.json").read_text())
        self.assertEqual(meta["continuation"], "timeout")
        self.assertEqual(meta["rework"]["used"], 1)
        result = self.cli("reply", "job", "fix rejection")
        self.assertEqual(result.returncode, 2)
        self.assertIn("rework limit reached (1/1)", result.stderr)

    def test_timeout_next_handles_missing_session_and_zero_budget(self):
        repo = self.repo({"a.txt": "old\n"})
        (repo / ".delegate.json").write_text(json.dumps({"maxRework": 0}))
        self.env["DELEGATE_TIMEOUT_GRACE"] = "0"
        self.fake_pi([fixtures.answer("later"), fixtures.SETTLED], pre="echo partial >> a.txt", sleep=10)
        first = self.outcome(self.cli("run", "--agent", "pi", "--worktree", "--workdir", repo,
                                     "--timeout", ".2s", "task"))
        session = Path(first["dir"]) / "session"
        for file in session.iterdir():
            file.unlink()
        state = self.outcome(self.cli("status", first["run"]))
        self.assertIn("--fresh", state["next"])
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo finished >> a.txt")
        command = shlex.split(state["next"])
        continued = self.outcome(self.cli(*command[1:], "--timeout", "5s", "--wait"))
        self.assertEqual(continued["state"], "answered")
        meta = json.loads((Path(continued["dir"]) / "meta.json").read_text())
        self.assertEqual(meta["continuation"], "timeout")
        self.assertEqual(meta["rework"]["used"], 0)

if __name__ == "__main__":
    unittest.main()
