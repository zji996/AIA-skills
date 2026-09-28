import base64
import importlib.util
import json
import re
import os
import shutil
import subprocess
import sys
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
SNAPSHOT = ROOT / "skills/agent-handoff/scripts/handoff-snapshot.sh"
AUDIT = ROOT / "skills/repo-governance/scripts/audit-context.py"
BACKGROUND_HOOK = ROOT / "skills/delegate/hooks/claude-code-background.py"
ADR_INDEX = ROOT / "skills/repo-governance/scripts/adr-index.py"


class ImageHandler(BaseHTTPRequestHandler):
    response_status = 200
    response_data = {"data": [{"b64_json": base64.b64encode(b"image-bytes").decode()}]}
    request_path = None
    request_data = None
    request_auth = None

    def do_POST(self):
        type(self).request_path = self.path
        type(self).request_auth = self.headers.get("Authorization")
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
        # The installer's own tests must not reach the network or a compiler for delegate's binary.
        env = {**(env if env is not None else os.environ), "AIA_SKILLS_SKIP_BINARIES": "1"}
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

    def test_install_keeps_delegate_out_of_pi_directories(self):
        home = self.work / "home"
        shared = home / ".agents/skills"
        shared.mkdir(parents=True)
        (shared / "delegate").symlink_to(ROOT / "skills/delegate")
        (shared / "agent-delegation").symlink_to(ROOT / "skills/agent-delegation")
        (home / ".codex/skills").mkdir(parents=True)
        # Pre-4.0 name whose checkout still holds a cache directory after the rename.
        leftover = ROOT / "skills/zz-renamed-skill-test"
        (leftover / "scripts/__pycache__").mkdir(parents=True)
        self.addCleanup(shutil.rmtree, leftover, True)
        (home / ".codex/skills/pi-delegation").symlink_to(leftover)
        env = {**os.environ, "HOME": str(home)}
        result = self.run_script(INSTALL, "delegate", "repo-governance", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((shared / "delegate").is_symlink())
        self.assertFalse((home / ".codex/skills/pi-delegation").is_symlink())
        self.assertFalse((shared / "agent-delegation").is_symlink())
        self.assertEqual((shared / "repo-governance").resolve(), ROOT / "skills/repo-governance")
        for directory in (".codex", ".cursor", ".claude", ".kilo"):
            self.assertEqual((home / directory / "skills/delegate").resolve(), ROOT / "skills/delegate")
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
        self.assertIn("version=2.1.2", marker)
        self.assertTrue((copy / "scripts/handoff-snapshot.sh").is_file())
        self.assertNotIn("outdated", self.run_script(INSTALL, "--status", env=env).stdout)
        (copy / ".aia-skills-install").write_text(marker.replace("version=2.1.2", "version=0.1.0"))
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

    def test_bootstrap_with_pi_fetches_relative_pi_kit_submodule(self):
        git_env = {**os.environ, "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t",
                   "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t",
                   # Local test remotes use the file transport, which Git blocks for submodules by default.
                   "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "protocol.file.allow",
                   "GIT_CONFIG_VALUE_0": "always"}

        def git(repo, *args):
            subprocess.run(["git", "-C", str(repo), *args], env=git_env, check=True, capture_output=True)

        pi_kit = self.work / "pi-kit"
        pi_kit.mkdir()
        (pi_kit / "install.sh").write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$PI_KIT_LOG"\n')
        for args in (["init", "-q", "-b", "main"], ["add", "-A"], ["commit", "-q", "-m", "kit"]):
            git(pi_kit, *args)
        upstream = self.work / "upstream"
        for part in ("scripts", "skills"):
            shutil.copytree(ROOT / part, upstream / part, ignore=shutil.ignore_patterns("__pycache__"))
        git(upstream, "init", "-q", "-b", "main")
        git(upstream, "submodule", "add", "-q", str(pi_kit), "third_party/pi-kit")
        git(upstream, "config", "-f", ".gitmodules", "submodule.third_party/pi-kit.url", "../pi-kit")
        for args in (["add", "-A"], ["commit", "-q", "-m", "v1"]):
            git(upstream, *args)

        home = self.work / "home"
        checkout = self.work / "checkout"
        log = self.work / "pi-kit.log"
        env = {**git_env, "HOME": str(home), "AIA_SKILLS_PRIMARY": str(upstream),
               "AIA_SKILLS_MIRROR": str(self.work / "missing.git"), "PI_KIT_LOG": str(log)}
        result = self.run_script(BOOTSTRAP, "--dir", checkout, "agent-handoff", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((checkout / "third_party/pi-kit/install.sh").exists())
        self.assertFalse(log.exists())
        result = self.run_script(BOOTSTRAP, "--dir", checkout, "--with-pi", "agent-handoff", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue((checkout / "third_party/pi-kit/install.sh").is_file())
        result = self.run_script(BOOTSTRAP, "--dir", checkout, "--with-pi-sync", "agent-handoff", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(log.read_text().splitlines(), ["--additive", "--sync"])
        self.assertEqual((home / ".agents/skills/agent-handoff").resolve(), checkout / "skills/agent-handoff")

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
        new_run = repo / ".local/run/delegate/20260102-000000-build"
        new_run.mkdir(parents=True)
        (new_run / "meta.json").write_text("{}")
        (new_run / "exit_code").write_text("0")
        self.assertIn("20260102-000000-build 已结束，结果未读取",
                      self.run_script(SNAPSHOT, "--repo", repo).stdout)
        (new_run / ".delivered").touch()
        (run / ".delivered").touch()
        self.assertNotIn("review", self.run_script(SNAPSHOT, "--repo", repo).stdout)
        tree = repo.parent / "wt"
        tree.mkdir()
        # Any JSON formatting: the Python delegate writes ", "/": ", the Rust one compact separators.
        for separators in ((", ", ": "), (",", ":")):
            meta = {"mode": "write", "worktree": {"source": str(repo), "path": str(tree)}}
            (run / "meta.json").write_text(json.dumps(meta, separators=separators))
            self.assertIn(f"20260101-000000-review 的 worktree 改动未合并：{tree}",
                          self.run_script(SNAPSHOT, "--repo", repo).stdout)
            (run / "meta.json").write_text(json.dumps({**meta, "mode": "read-only"}, separators=separators))
            self.assertNotIn("未合并", self.run_script(SNAPSHOT, "--repo", repo).stdout)  # a snapshot to read
            (run / "meta.json").write_text(json.dumps(meta, separators=separators))
        (run / ".applied").touch()
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
        result = self.run_script(AUDIT, "--repo", repo, "--max-entry-tokens", "3")
        self.assertRegex(result.stdout, r"AGENTS.md: ~\d+ tokens \(> 3\)")
        self.assertIn("broken link: docs/missing.md", result.stdout)
        self.assertIn("7 next actions", result.stdout)
        (repo / "AGENTS.md").write_text("# ok\n")
        (repo / "docs/current.md").write_text("# Next steps\n- one\n")
        result = self.run_script(AUDIT, "--repo", repo)
        self.assertEqual(result.returncode, 0, result.stdout)

    def test_audit_context_reports_bloat_and_names_that_do_not_exist(self):
        repo = self.git_repo()
        (repo / ".gitignore").write_text(".local/\n")
        (repo / "Makefile").write_text("check:\n\ttrue\ndev-up dev-down:\n\ttrue\n")
        (repo / "src/pkg").mkdir(parents=True)
        (repo / "src/pkg/mod.py").write_text("")
        (repo / "docs/decision").mkdir(parents=True)
        (repo / "docs/decision/0001-a.md").write_text("# a\n")
        (repo / "AGENTS.md").write_text(
            "Run `make check`, `make dev-up`, `make te-*` and `make gone`.\n"
            "See `src/pkg/mod.py`, `pkg/mod.py`, `src/pkg/old.py`, `src/missing/`, `async/await`, `3/5`, "
            "`.local/plan/plan.md`, `a/.../b`.\n"
            "```bash\nmake vanished\n```\n")
        (repo / "docs/current.md").write_text(
            "# Now\nRead `decision/0001-a.md`.\n" + "".join(f"- 2026-01-0{i} shipped\n" for i in range(1, 8))
            + "x" * 11 * 1024 + "\n")
        out = self.run_script(AUDIT, "--repo", repo).stdout
        self.assertRegex(out, r"docs/current.md: ~\d+ tokens \(> 2500\)")
        self.assertIn("docs/current.md: 7 dated entries (> 5)", out)
        missing = sorted(re.findall(r"names `([^`]+)`", out))
        self.assertEqual(missing, ["make gone", "make vanished", "src/missing/", "src/pkg/old.py"])
        out = self.run_script(AUDIT, "--repo", repo, "--max-current-kb", "16", "--max-dated-items", "10").stdout
        self.assertNotIn("KB (>", out)
        self.assertNotIn("dated entries", out)

    def test_audit_token_estimate_counts_cjk_ascii_and_mixed_text(self):
        spec = importlib.util.spec_from_file_location("audit_context", AUDIT)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.assertEqual(module.estimate_tokens("中文かな한글，。"), 8)
        self.assertEqual(module.estimate_tokens("abcde"), 2)
        self.assertEqual(module.estimate_tokens("中abcde文"), 4)

    def test_audit_config_glob_cli_override_and_report(self):
        repo = self.git_repo()
        (repo / "AGENTS.md").write_text("abcd" * 8)
        (repo / "docs").mkdir()
        (repo / "docs/current.md").write_text("abcd" * 4)
        (repo / "skills/demo").mkdir(parents=True)
        (repo / "skills/demo/SKILL.md").write_text("中文" * 6)
        (repo / ".repo-governance.json").write_text(json.dumps({
            "budgets": {"AGENTS.md": 7, "docs/current.md": 3, "skills/*/SKILL.md": 10},
            "maxNextActions": 1,
        }))
        result = self.run_script(AUDIT, "--repo", repo, "--only", "entry,current,budget")
        self.assertIn("AGENTS.md: ~8 tokens (> 7)", result.stdout)
        self.assertIn("docs/current.md: ~4 tokens (> 3)", result.stdout)
        self.assertIn("skills/demo/SKILL.md: ~12 tokens (> 10)", result.stdout)
        result = self.run_script(AUDIT, "--repo", repo, "--only", "entry,current,budget",
                                 "--max-entry-tokens", "9", "--max-current-tokens", "5",
                                 "--fail-on", "budget")
        self.assertNotIn("AGENTS.md: ~", result.stdout)
        self.assertNotIn("docs/current.md: ~", result.stdout)
        self.assertIn("skills/demo/SKILL.md: ~12 tokens", result.stdout)
        self.assertEqual(result.returncode, 1)
        result = self.run_script(AUDIT, "--repo", repo, "--report")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertLess(result.stdout.index("skills/demo/SKILL.md"), result.stdout.index("AGENTS.md"))
        self.assertIn("every-session baseline: ~12 tokens", result.stdout)

    def test_background_hook_denies_only_foreground_blocking_delegate_calls(self):
        def decision(command, background=False, tool="Bash"):
            payload = json.dumps({"tool_name": tool, "tool_input": {"command": command, "run_in_background": background}})
            result = subprocess.run([sys.executable, str(BACKGROUND_HOOK)], input=payload, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            return json.loads(result.stdout)["hookSpecificOutput"]["permissionDecision"] if result.stdout else "allow"

        d = "$" + "D"  # keeps this file's own text from looking like an invocation
        deny = [f"~/.claude/skills/delegate/bin/delegate wait --max 9m", f'{d} run --accept "make check" "x"',
                f'${{D}} reply impl --wait "fix"', 'ssh h "~/s/bin/delegate wait r1"', f"{d} run --help; {d} wait",
                f"{d} run --prompt-file - <<'EOF'\nfix it\nEOF"]
        allow = [f'{d} start --read-only "x"', f'{d} reply impl "more"', f"{d} status; {d} result r", f"{d} wait --help",
                 'git commit -m "a bare delegate wait now collects only this session"',
                 f"python3 - <<'EOF'\ncases = ['{d} wait', '{d} run x']\nEOF\necho done"]
        for command in deny:
            self.assertEqual(decision(command), "deny", command)
        for command in allow:
            self.assertEqual(decision(command), "allow", command)
        self.assertEqual(decision(f"{d} wait", background=True), "allow")
        self.assertEqual(decision(f"{d} wait", tool="Read"), "allow")

    def test_audit_exact_budget_overrides_a_glob_written_after_it(self):
        repo = self.git_repo()
        (repo / "AGENTS.md").write_text("ok")
        (repo / "docs").mkdir()
        (repo / "docs/big.md").write_text("abcd" * 20)
        (repo / ".repo-governance.json").write_text(
            '{"budgets": {"docs/big.md": 30, "docs/*.md": 5}}')
        result = self.run_script(AUDIT, "--repo", repo, "--report")
        self.assertIn("docs/big.md: ~20 tokens / 30", result.stdout)

    def test_audit_counts_a_symlinked_entry_file_once(self):
        repo = self.git_repo()
        (repo / "AGENTS.md").write_text("abcd" * 8)
        (repo / "CLAUDE.md").symlink_to("AGENTS.md")
        result = self.run_script(AUDIT, "--repo", repo, "--report")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("every-session baseline: ~8 tokens", result.stdout)
        self.assertNotIn("CLAUDE.md", result.stdout)

    def test_audit_only_and_configured_current_limits(self):
        repo = self.git_repo()
        (repo / "AGENTS.md").write_text("ok")
        (repo / "docs").mkdir()
        (repo / "docs/current.md").write_text("# Next steps\n- one\n- two\n- 2026-01-01 done\n")
        (repo / ".repo-governance.json").write_text(json.dumps({
            "maxNextActions": 1, "maxDatedItems": 0, "staleDays": 0,
        }))
        result = self.run_script(AUDIT, "--repo", repo, "--only", "current")
        self.assertIn("3 next actions (> 1)", result.stdout)
        self.assertIn("1 dated entries (> 0)", result.stdout)
        result = self.run_script(AUDIT, "--repo", repo, "--only", "budget")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertNotIn("next actions", result.stdout)
        result = self.run_script(AUDIT, "--repo", repo, "--only", "current",
                                 "--max-dated-items", "2", "--stale-days", "99999")
        self.assertNotIn("dated entries", result.stdout)
        self.assertNotIn("last changed", result.stdout)

    def test_audit_legacy_limits_warn_and_still_apply(self):
        repo = self.git_repo()
        (repo / "AGENTS.md").write_text("line\n" * 3)
        (repo / "docs").mkdir()
        (repo / "docs/current.md").write_text("x" * 2048)
        result = self.run_script(AUDIT, "--repo", repo, "--max-agents-lines", "2",
                                 "--max-current-kb", "1", "--only", "entry,current")
        self.assertIn("3 lines (> 2)", result.stdout)
        self.assertIn("2 KB (> 1)", result.stdout)
        self.assertIn("DEPRECATED --max-agents-lines", result.stderr)
        self.assertIn("DEPRECATED --max-current-kb", result.stderr)

    def test_audit_rejects_invalid_config_with_exit_two(self):
        repo = self.git_repo()
        config = repo / ".repo-governance.json"
        for value in ('{', '{"budgets":{"AGENTS.md":true}}', '{"maxNextActions":"5"}'):
            config.write_text(value)
            result = self.run_script(AUDIT, "--repo", repo)
            self.assertEqual(result.returncode, 2)
            self.assertEqual(len(result.stderr.splitlines()), 1)
            self.assertIn(".repo-governance.json", result.stderr)

    def test_check_uses_token_budget_without_line_warning(self):
        text = CHECK.read_text()
        self.assertNotIn("MAX_SKILL_LINES", text)
        self.assertIn("--only budget", text)
        self.assertNotIn("SKILL.md has", text)

    def test_adr_index_is_generated_from_each_adr_status_line(self):
        repo = self.git_repo()
        (repo / ".gitignore").write_text(".local/\n")
        (repo / "AGENTS.md").write_text("# ok\n")
        adr = repo / "docs/decision"
        adr.mkdir(parents=True)
        (adr / "0001-a.md").write_text("# ADR 0001: 第一 | 决定\n\n状态: active（见 0002）\n")
        (adr / "0002-b.md").write_text("# ADR-0002: Second\n\n- **Status**: Accepted（2026-06-23）\n")
        (adr / "0003-c.md").write_text("# 0003 Third\n\n- 状态：superseded by ADR 0002\n")
        (adr / "0004-d.md").write_text("# Fourth\n\nno status here\n")
        (adr / "INDEX.md").write_text("# Index\n\nintro\n")
        self.assertEqual(self.run_script(ADR_INDEX, "--repo", repo).returncode, 2)  # no markers: not managed
        (adr / "INDEX.md").write_text("# Index\n\nintro\n<!-- adr-index:start -->\nold\n<!-- adr-index:end -->\ntail\n")
        stale = self.run_script(ADR_INDEX, "--repo", repo)
        self.assertEqual(stale.returncode, 1)
        audit = self.run_script(AUDIT, "--repo", repo)
        self.assertIn("INDEX.md: out of date", audit.stdout)
        self.assertEqual(audit.returncode, 1)
        self.assertEqual(self.run_script(AUDIT, "--repo", repo, "--fail-on", "links,names").returncode, 0)
        self.assertEqual(self.run_script(AUDIT, "--repo", repo, "--fail-on", "nope").returncode, 2)
        self.assertEqual(self.run_script(ADR_INDEX, "--repo", repo, "--write").returncode, 0)
        text = (adr / "INDEX.md").read_text()
        self.assertIn("| 0001 | [第一 \\| 决定](0001-a.md) | active（见 0002） |", text)
        self.assertIn("| 0002 | [Second](0002-b.md) | Accepted（2026-06-23） |", text)
        self.assertIn("| 0003 | [Third](0003-c.md) | superseded by ADR 0002 |", text)
        self.assertIn("| 0004 | [Fourth](0004-d.md) | （未标状态） |", text)
        self.assertTrue(text.startswith("# Index\n\nintro\n") and text.endswith("\ntail\n"))
        self.assertEqual(self.run_script(ADR_INDEX, "--repo", repo).returncode, 0)
        self.assertEqual(self.run_script(AUDIT, "--repo", repo).returncode, 0)

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

    def codex_home(self, provider_lines, auth_key=None):
        config = self.work / "codex"
        config.mkdir(exist_ok=True)
        (config / "config.toml").write_text('model_provider = "selected"\n[model_providers.selected]\n' + provider_lines)
        if auth_key:
            (config / "auth.json").write_text(json.dumps({"OPENAI_API_KEY": auth_key}))
        env = {key: value for key, value in os.environ.items()
               if key not in ("OPENAI_API_KEY", "OPENAI_BASE_URL")}
        env["CODEX_HOME"] = str(config)
        return env

    def test_image_selected_provider_wins_over_auth_json(self):
        server, handler = self.image_server()
        env = self.codex_home(
            f'base_url = "http://127.0.0.1:{server.server_port}"\n'
            'http_headers = { "Authorization" = "Bearer provider-key" }\n', auth_key="auth-key")
        result = self.run_script(IMAGE, "-p", "demo", "-o", self.work / "image.png", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(handler.request_auth, "Bearer provider-key")

    def test_image_auth_json_follows_provider_that_requires_openai_auth(self):
        server, handler = self.image_server()
        env = self.codex_home(
            f'base_url = "http://127.0.0.1:{server.server_port}/v1"\nrequires_openai_auth = true\n',
            auth_key="auth-key")
        result = self.run_script(IMAGE, "-p", "demo", "-o", self.work / "image.png", env=env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(handler.request_auth, "Bearer auth-key")

    def test_image_missing_value_shows_usage(self):
        result = self.run_script(IMAGE, "--prompt", "demo", "--output")
        self.assertEqual(result.returncode, 2)
        self.assertIn("Usage:", result.stderr)


if __name__ == "__main__":
    unittest.main()
