"""Tests for delegate tui exec wrapper, lane --json, result --no-mark, and extra-binaries release/fetch."""
import hashlib
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

import test_delegate as fixtures

ROOT = Path(__file__).resolve().parent.parent
FETCH = ROOT / "scripts/fetch-binary.sh"
RELEASE = ROOT / "scripts/release-binary.sh"


class DelegateTuiTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    repo = fixtures.DelegateTests.repo

    def test_delegate_tui_missing_binary_prints_install_hint(self):
        missing = self.work / "no-such-delegate-tui"
        res = subprocess.run(
            [str(fixtures.DELEGATE), "tui"],
            cwd=self.work,
            env={**self.env, "DELEGATE_TUI_BIN": str(missing)},
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(res.returncode, 1)
        self.assertIn("delegate-tui", res.stderr)
        self.assertIn("fetch-binary.sh", res.stderr)
        self.assertIn("cargo build", res.stderr)

    def test_delegate_tui_execs_binary_and_forwards_args_and_delegate_bin(self):
        stub = self.work / "stub-delegate-tui"
        stub.write_text(
            "#!/bin/sh\n"
            'printf "BIN=%s\\nARGS=%s\\n" "$DELEGATE_BIN" "$*"\n'
        )
        stub.chmod(0o755)
        env = {**self.env, "DELEGATE_TUI_BIN": str(stub)}
        env.pop("DELEGATE_BIN", None)
        res = subprocess.run(
            [str(fixtures.DELEGATE), "tui", "--interval", "3", "--repo", "/tmp/x"],
            cwd=self.work,
            env=env,
            capture_output=True,
            text=True,
            timeout=15,
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn(f"BIN={fixtures.DELEGATE}", res.stdout)
        self.assertIn("ARGS=--interval 3 --repo /tmp/x", res.stdout)

    def test_delegate_lane_json_lists_occupancy_and_rejects_with_command(self):
        res = self.cli("lane", "--json")
        self.assertEqual(res.returncode, 0, res.stderr)
        data = json.loads(res.stdout)
        self.assertIn("maxHeavy", data)
        self.assertIsInstance(data["local"], list)
        self.assertIn("remoteMaxHeavy", data)
        self.assertIsInstance(data["remote"], list)

        bad = self.cli("lane", "--json", "echo hi")
        self.assertEqual(bad.returncode, 2)
        self.assertIn("--json", bad.stderr)

    def test_delegate_result_no_mark_does_not_mutate_read_state(self):
        import time

        repo = self.repo({"a.txt": "hello\n"})
        self.fake_pi([fixtures.answer("结果全文内容"), fixtures.SETTLED])
        start = self.cli("start", "--read-only", "--name", "peek-test", "只读检查", cwd=repo)
        self.assertEqual(start.returncode, 0, start.stderr)
        status = json.loads(self.cli("status", "--json", "--all", cwd=repo).stdout.splitlines()[0])
        run_id = status["run"]
        run_dir = Path(status["dir"])
        for _ in range(100):
            if (run_dir / "exit_code").is_file():
                break
            time.sleep(0.05)
        self.assertTrue((run_dir / "result.md").is_file())
        self.assertFalse((run_dir / ".delivered").exists())

        peek = self.cli("result", "--no-mark", run_id, cwd=repo)
        self.assertEqual(peek.returncode, 0, peek.stderr)
        self.assertIn("结果全文内容", peek.stdout)
        self.assertFalse((run_dir / ".delivered").exists())
        meta = json.loads((run_dir / "meta.json").read_text())
        self.assertNotIn("resultReadAt", meta)

        # Normal result marks it read
        normal = self.cli("result", run_id, cwd=repo)
        self.assertEqual(normal.returncode, 0, normal.stderr)
        self.assertTrue((run_dir / ".delivered").exists())
        meta_after = json.loads((run_dir / "meta.json").read_text())
        self.assertIn("resultReadAt", meta_after)

    def test_extra_binaries_release_verify_and_fetch_build(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = Path(tmp) / "repo"
            skill = repo / "skills/demo"
            dist = repo / "dist/demo-v1.2.3"
            skill.mkdir(parents=True)
            dist.mkdir(parents=True)
            (skill / "SKILL.md").write_text(
                '---\nname: demo\nmetadata:\n  version: "1.2.3"\n  binary: demo\n  extra-binaries: demo-tui\n---\n'
            )
            checksum_lines = []
            for name in ("demo", "demo-tui"):
                crate = repo / "crates" / name
                crate.mkdir(parents=True)
                (crate / "Cargo.toml").write_text(f'[package]\nname = "{name}"\nversion = "1.2.3"\n')
                (crate / "Cargo.lock").write_text("# lock\n")
                asset = f"{name}-x86_64-unknown-linux-musl"
                payload = f"bin-{name}".encode()
                (dist / asset).write_bytes(payload)
                checksum_lines.append(f"{hashlib.sha256(payload).hexdigest()}  {asset}\n")
            (skill / "bin.sha256").write_text("".join(checksum_lines))
            subprocess.run(["git", "-C", str(repo), "init", "-q"], check=True)
            subprocess.run(["git", "-C", str(repo), "add", "skills", "crates"], check=True)
            subprocess.run(
                ["git", "-C", str(repo), "-c", "user.name=T", "-c", "user.email=t@e", "commit", "-q", "-m", "init"],
                check=True,
            )

            env = {**os.environ, "AIA_SKILLS_REPO_ROOT": str(repo)}
            ver = subprocess.run(["bash", str(RELEASE), "verify", "demo"], capture_output=True, text=True, env=env)
            self.assertEqual(ver.returncode, 0, ver.stdout + ver.stderr)

            # Version mismatch in extra-binaries crate must fail verify
            (repo / "crates/demo-tui/Cargo.toml").write_text('[package]\nname = "demo-tui"\nversion = "9.9.9"\n')
            bad_ver = subprocess.run(["bash", str(RELEASE), "verify", "demo"], capture_output=True, text=True, env=env)
            self.assertNotEqual(bad_ver.returncode, 0)

            # fetch-binary --build installs both primary and extra-binaries
            fake = Path(tmp) / "fake-bin"
            fake.mkdir()
            cargo = fake / "cargo"
            cargo.write_text(
                "#!/bin/sh\n"
                'name="$(basename "$PWD")"\n'
                'mkdir -p target/release\n'
                'printf "bin-%s" "$name" > "target/release/$name"\n'
            )
            cargo.chmod(0o755)
            fetch_env = {**env, "PATH": f"{fake}:{os.environ['PATH']}"}
            fetched = subprocess.run(["bash", str(FETCH), "--build", "demo"], capture_output=True, text=True, env=fetch_env)
            self.assertEqual(fetched.returncode, 0, fetched.stdout + fetched.stderr)
            self.assertEqual((skill / "bin/demo").read_bytes(), b"bin-demo")
            self.assertEqual((skill / "bin/demo-tui").read_bytes(), b"bin-demo-tui")


if __name__ == "__main__":
    unittest.main()
