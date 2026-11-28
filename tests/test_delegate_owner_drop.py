"""End-to-end CLI tests for ownerDrop metadata perception, act delivery location,
human: verification on unrun items, unbriefed work-item hygiene, and work wait fixes.
"""
import json
import os
import stat
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateOwnerDropTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def work_show_json(self, wid: str) -> dict:
        res = self.cli("work", "show", wid, "--json", human=True)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        return json.loads(res.stdout)

    def hook_check(self, repo: Path) -> dict | None:
        env = dict(self.env)
        env.pop("DELEGATE_AGENT", None)
        res = subprocess.run(
            [str(fixtures.DELEGATE), "hook", "check", "--format", "claude"],
            input=json.dumps({"cwd": str(repo), "stop_hook_active": False}),
            capture_output=True,
            text=True,
            env=env,
            cwd=str(repo),
            timeout=15,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        out = res.stdout.strip()
        if not out:
            return None
        return json.loads(out)

    def test_owner_drop_baseline_chmod000_hook_ack_close_and_delivery_location(self):
        repo = self.repo({"README.md": "demo\n"})
        (repo / ".delegate.json").write_text(
            json.dumps({"ownerDrop": ["tmp/"]}), encoding="utf-8"
        )
        (repo / "tmp").mkdir(parents=True, exist_ok=True)
        (repo / "tmp" / "old-existing.txt").write_text("legacy\n", encoding="utf-8")

        # Create Q01 (act) + W001 blocked on Q01 via --needs-owner act; this write cmd also builds baseline.
        add = self.cli(
            "work", "add", "接入 Acme OAuth 登录",
            "--repo", repo,
            "--needs-owner", "act",
            "--ask", "请在后台建好 Acme OAuth App 并放好凭据",
            "--recommendation", "建演示应用即可",
            "--if-undecided", "登录链路暂缓",
            "--cost", "5 分钟",
            human=True,
        )
        self.assertEqual(add.returncode, 0, add.stdout + add.stderr)

        # Delivery location appears on both `work show Q01` and `work --owner`.
        show_q = self.cli("work", "show", "Q01", human=True)
        self.assertEqual(show_q.returncode, 0, show_q.stdout + show_q.stderr)
        self.assertIn(
            "交付位置：本机材料可放 tmp/，主控会自动看到（跨机或网页操作请在对话说一声）",
            show_q.stdout,
        )
        owner_list = self.cli("work", "--owner", human=True)
        self.assertEqual(owner_list.returncode, 0, owner_list.stdout + owner_list.stderr)
        self.assertIn(
            "交付位置：本机材料可放 tmp/，主控会自动看到（跨机或网页操作请在对话说一声）",
            owner_list.stdout,
        )

        # Defer Q01; existing old-existing.txt was silently baselined -> hook check allows!
        self.assertEqual(
            self.cli("work", "defer", "Q01", "--reason", "等所有者有空", "--until", "2099-01-01", human=True).returncode,
            0,
        )
        self.assertEqual(self.work_show_json("Q01")["state"], "deferred")
        self.assertIsNone(self.hook_check(repo))

        # Drop unrelated file -> hook check blocks, but Q01 stays deferred!
        (repo / "tmp" / "random.log").write_text("unrelated\n", encoding="utf-8")
        blk_random = self.hook_check(repo)
        self.assertIsNotNone(blk_random)
        self.assertIn("tmp/random.log", blk_random["reason"])
        self.assertIn("delegate work ack-drop tmp/random.log", blk_random["reason"])
        self.assertEqual(self.work_show_json("Q01")["state"], "deferred")

        # Ack unrelated file -> hook check allows again!
        ack1 = self.cli("work", "ack-drop", "tmp/random.log", "--repo", repo, human=True)
        self.assertEqual(ack1.returncode, 0, ack1.stdout + ack1.stderr)
        self.assertIsNone(self.hook_check(repo))

        # Drop chmod 000 secret file matching Q01 WITHOUT running _gc-worker!
        secret_dir = repo / "tmp" / "secret"
        secret_dir.mkdir(parents=True, exist_ok=True)
        secret_file = secret_dir / "acme-oauth"
        secret_file.write_text("client_id=fake\n", encoding="utf-8")
        os.chmod(secret_file, 0)
        try:
            blk = self.hook_check(repo)
            self.assertIsNotNone(blk)
            self.assertIn("tmp/secret/acme-oauth", blk["reason"])
            self.assertIn("可能对应 [Q01]", blk["reason"])
            self.assertIn("delegate work close Q01", blk["reason"])
            # Read-only `work show Q01` immediately sees Q01 woken to attention!
            q1_ro = self.work_show_json("Q01")
            self.assertEqual(q1_ro["state"], "attention")
            self.assertIn("tmp/secret/acme-oauth", q1_ro["owner"].get("lastDropHint", ""))

            # Re-defer Q01 WITHOUT calling `ack-drop`: `work defer` syncs current (mtime, size)
            # then persists deferred -> read-only `work show Q01` must respect deferred!
            self.assertEqual(
                self.cli("work", "defer", "Q01", "--reason", "暂不处理", "--until", "2099-01-01", human=True).returncode,
                0,
            )
            q1_def_no_ack = self.work_show_json("Q01")
            self.assertEqual(q1_def_no_ack["state"], "deferred")
            self.assertEqual(q1_def_no_ack["owner"].get("lastDropHint", ""), "")

            # Now ack and verify hook_check stops blocking as well
            ack2 = self.cli("work", "ack-drop", "tmp/secret/acme-oauth", "--repo", repo, human=True)
            self.assertEqual(ack2.returncode, 0, ack2.stdout + ack2.stderr)
            self.assertEqual(self.work_show_json("Q01")["state"], "deferred")
            self.assertIsNone(self.hook_check(repo))

            # Update file (chmod + write + chmod 000 again) -> triggers again; closing Q01 auto-acks!
            os.chmod(secret_file, stat.S_IRUSR | stat.S_IWUSR)
            secret_file.write_text("client_id=fake-v2-longer\n", encoding="utf-8")
            os.chmod(secret_file, 0)
            blk2 = self.hook_check(repo)
            self.assertIsNotNone(blk2)
            self.assertIn("可能对应 [Q01]", blk2["reason"])

            close_q = self.cli("work", "close", "Q01", "--evidence", "凭据已核实", human=True)
            self.assertEqual(close_q.returncode, 0, close_q.stdout + close_q.stderr)
            # Unlocked W001 has no brief -> unlock hint must NOT suggest `delegate start`!
            self.assertNotIn("delegate start", close_q.stdout)
            self.assertIn("无任务说明：补 delegate work wait W001 --brief <文件> 再派", close_q.stdout)
            # Consume the `work_ready` event emitted when W001 unlocked; ownerDrop itself is auto-acked!
            blk_after_close = self.hook_check(repo)
            if blk_after_close is not None:
                self.assertNotIn("acme-oauth", blk_after_close["reason"])
            self.cli("events", "--since", "0", human=True)
            self.assertIsNone(self.hook_check(repo))
        finally:
            if secret_file.exists():
                os.chmod(secret_file, stat.S_IRUSR | stat.S_IWUSR)

    def test_human_verify_and_unbriefed_item_behavior(self):
        repo = self.repo({"a.txt": "v0\n"})
        brief = repo / "docs" / "task.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# Task\n", encoding="utf-8")

        # W001: unbriefed, unrun -> `work` & `work show` must not show 40-60m ETA or `delegate start`
        self.assertEqual(
            self.cli("work", "add", "人工核对演示环境", "--repo", repo, human=True).returncode,
            0,
        )
        w_list = self.cli("work", human=True)
        self.assertEqual(w_list.returncode, 0, w_list.stdout + w_list.stderr)
        self.assertIn("缺说明", w_list.stdout)
        self.assertNotIn("预计耗时", w_list.stdout)
        self.assertNotIn("start --work W001", w_list.stdout)

        w_show = self.cli("work", "show", "W001", human=True)
        self.assertEqual(w_show.returncode, 0, w_show.stdout + w_show.stderr)
        self.assertNotIn("预计耗时", w_show.stdout)
        self.assertNotIn("现派", w_show.stdout)

        # Adding `--verify-on human:...` to unbriefed unrun W001 immediately transitions to `verify`!
        upd = self.cli("work", "wait", "W001", "--verify-on", "human:核对网页登录正常", human=True)
        self.assertEqual(upd.returncode, 0, upd.stdout + upd.stderr)
        w1 = self.work_show_json("W001")
        self.assertEqual(w1["state"], "verify")
        self.assertEqual(w1["verifyOn"], ["human:核对网页登录正常"])
        self.assertIn("待验(核对网页登录正常)", self.cli("work", human=True).stdout)

        # Close via --evidence succeeds
        cls = self.cli("work", "close", "W001", "--evidence", "已手工验证通过", human=True)
        self.assertEqual(cls.returncode, 0, cls.stdout + cls.stderr)
        self.assertEqual(self.work_show_json("W001")["state"], "done")

        # W002: WITH brief + `--verify-on human:...` stays `ready` until run!
        self.assertEqual(
            self.cli(
                "work", "add", "带说明需人工复核",
                "--repo", repo,
                "--brief", str(brief),
                "--verify-on", "human:确认上线指标",
                human=True,
            ).returncode,
            0,
        )
        self.assertEqual(self.work_show_json("W002")["state"], "ready")

    def test_work_wait_rejects_multiline_run_id_and_clear_preserves_auto(self):
        repo = self.repo({"a.txt": "v0\n"})
        brief = repo / "docs" / "auto.md"
        brief.parent.mkdir(parents=True, exist_ok=True)
        brief.write_text("# Auto task\n", encoding="utf-8")

        self.assertEqual(
            self.cli(
                "work", "add", "自动续派项",
                "--repo", repo,
                "--brief", str(brief),
                "--after-delivered", "20261128-100001-upstream-a",
                "--auto", "--", "--worktree",
                human=True,
            ).returncode,
            0,
        )
        w1 = self.work_show_json("W001")
        self.assertEqual(w1["state"], "blocked")
        self.assertIsInstance(w1.get("autoDispatch"), dict)

        # Multiline run id (`by\n<run-id>`) is rejected immediately
        bad = self.cli(
            "work", "wait", "W001",
            "--after-delivered", "by\n20261128-100002-upstream-b",
            human=True,
        )
        self.assertNotEqual(bad.returncode, 0)
        self.assertIn("invalid", bad.stderr.lower())

        # `--clear` + new `--after-delivered` atomically replaces waitOn, keeps autoDispatch, stays blocked!
        rep = self.cli(
            "work", "wait", "W001",
            "--clear",
            "--after-delivered", "20261128-100002-upstream-b",
            human=True,
        )
        self.assertEqual(rep.returncode, 0, rep.stdout + rep.stderr)
        w1_after = self.work_show_json("W001")
        self.assertEqual(w1_after["state"], "blocked")
        self.assertEqual(w1_after["waitOn"], ["run_delivered:20261128-100002-upstream-b"])
        self.assertIsInstance(w1_after.get("autoDispatch"), dict)
        self.assertEqual(w1_after["autoDispatch"].get("dispatchedRun", ""), "")

        # `--no-auto` explicitly clears autoDispatch without touching waitOn
        no_auto = self.cli("work", "wait", "W001", "--no-auto", human=True)
        self.assertEqual(no_auto.returncode, 0, no_auto.stdout + no_auto.stderr)
        w1_no_auto = self.work_show_json("W001")
        self.assertIsNone(w1_no_auto.get("autoDispatch"))
        self.assertEqual(w1_no_auto["waitOn"], ["run_delivered:20261128-100002-upstream-b"])

    def test_config_rejects_invalid_owner_drop_and_user_level(self):
        repo = self.repo({"a.txt": "v0\n"})
        (repo / ".delegate.json").write_text(
            json.dumps({"ownerDrop": ["../escape"]}), encoding="utf-8"
        )
        res = self.cli("config", cwd=repo, human=True)
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("ownerDrop", res.stderr)

        (repo / ".delegate.json").write_text(
            json.dumps({"ownerDrop": ["/abs/tmp"]}), encoding="utf-8"
        )
        res2 = self.cli("config", cwd=repo, human=True)
        self.assertNotEqual(res2.returncode, 0)
        self.assertIn("ownerDrop", res2.stderr)

        # Clean repo config, put ownerDrop in user config -> must reject!
        (repo / ".delegate.json").unlink()
        user_cfg = Path(self.env["XDG_CONFIG_HOME"]) / "delegate" / "config.json"
        user_cfg.parent.mkdir(parents=True, exist_ok=True)
        user_cfg.write_text(json.dumps({"ownerDrop": ["tmp/"]}), encoding="utf-8")
        res3 = self.cli("config", cwd=repo, human=True)
        self.assertNotEqual(res3.returncode, 0)
        self.assertIn("ownerDrop", res3.stderr)


if __name__ == "__main__":
    unittest.main()
