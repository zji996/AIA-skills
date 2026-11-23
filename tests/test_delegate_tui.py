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

    def test_owner_work_items_cli_unlock_and_snapshot_wait_status(self):
        repo = self.repo({"a.txt": "hello\n"})
        card = repo / "docs/owner/directions/routing.md"
        card.parent.mkdir(parents=True)
        card.write_text("# Q20 路由方案\n\n建议选方案 A。\n")

        # 1. Add owner items (act, nod, card, direction) with explicit Q/D IDs and auto-allocated ID
        r1 = self.cli(
            "work", "add",
            "--owner-kind", "act",
            "--ask", "清理旧配置",
            "--recommendation", "直接删除旧项",
            "--cost", "2 分钟",
            "--qid", "Q18",
            "--repo", str(repo),
            cwd=repo,
        )
        self.assertEqual(r1.returncode, 0, r1.stderr)
        r2 = self.cli(
            "work", "add",
            "--owner-kind", "card",
            "--ask", "选方案 A 还是 B？",
            "--recommendation", "建议选 A",
            "--if-undecided", "沿用旧逻辑",
            "--cost", "2 天",
            "--brief", "docs/owner/directions/routing.md",
            "--qid", "Q20",
            "--repo", str(repo),
            cwd=repo,
        )
        self.assertEqual(r2.returncode, 0, r2.stderr)
        r3 = self.cli(
            "work", "add",
            "--owner-kind", "direction",
            "--ask", "流式动作方向调研",
            "--qid", "D002",
            "--repo", str(repo),
            cwd=repo,
        )
        self.assertEqual(r3.returncode, 0, r3.stderr)
        r4 = self.cli(
            "work", "add",
            "--owner-kind", "nod",
            "--ask", "买直连 key？",
            "--qid", "Q19",
            "--repo", str(repo),
            cwd=repo,
        )
        self.assertEqual(r4.returncode, 0, r4.stderr)

        # 2. Drop Q19 and verify it appears under 已否
        drop = self.cli("work", "drop", "Q19", "--reason", "现有渠道够用", cwd=repo)
        self.assertEqual(drop.returncode, 0, drop.stderr)

        # 3. Add a work item waiting on owner:Q20
        w_add = self.cli(
            "work", "add", "实施路由方案 A",
            "--wait-owner", "Q20",
            "--repo", str(repo),
            cwd=repo,
        )
        self.assertEqual(w_add.returncode, 0, w_add.stderr)

        # 4. `delegate work --owner` outputs grouped owner view + 已否
        owner_view = self.cli("work", "--owner", cwd=repo)
        self.assertEqual(owner_view.returncode, 0, owner_view.stderr)
        self.assertIn("等你 3", owner_view.stdout)
        self.assertIn("已否 1", owner_view.stdout)
        self.assertIn("## 动手", owner_view.stdout)
        self.assertIn("## 看卡", owner_view.stdout)
        self.assertIn("## 方向", owner_view.stdout)
        self.assertIn("## 已否", owner_view.stdout)
        self.assertIn("Q18", owner_view.stdout)
        self.assertIn("Q20", owner_view.stdout)
        self.assertIn("D002", owner_view.stdout)
        self.assertIn("Q19", owner_view.stdout)

        # 5. `delegate snapshot --json` exposes owner fields & evaluated waitOnStatus
        snap = json.loads(self.cli("snapshot", "--json", cwd=repo).stdout)
        by_id = {w["id"]: w for w in snap["work"]}
        self.assertEqual(by_id["Q20"]["owner"]["kind"], "card")
        self.assertEqual(by_id["Q20"]["owner"]["recommendation"], "建议选 A")
        self.assertEqual(by_id["W001"]["state"], "blocked")
        self.assertFalse(by_id["W001"]["waitOnStatus"][0]["satisfied"])

        # 6. Closing Q20 unlocks W001 automatically
        close = self.cli("work", "close", "Q20", "--evidence", "所有者：同意选 A", cwd=repo)
        self.assertEqual(close.returncode, 0, close.stderr)
        self.assertIn("已解锁 [W001]", close.stdout)

        snap2 = json.loads(self.cli("snapshot", "--json", cwd=repo).stdout)
        by_id2 = {w["id"]: w for w in snap2["work"]}
        self.assertEqual(by_id2["Q20"]["state"], "done")
        self.assertEqual(by_id2["W001"]["state"], "ready")
        self.assertTrue(by_id2["W001"]["waitOnStatus"][0]["satisfied"])


if __name__ == "__main__":
    unittest.main()
