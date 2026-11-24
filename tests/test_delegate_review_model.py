"""End-to-end tests for review model selection/fallback, --class default thinking,
reviewer format-error single follow-up, and review.extra supplementary review.
"""
import json
import unittest
from pathlib import Path

import test_delegate as fixtures


EXPECTED_FORMAT_RETRY_PROMPT = (
    "上一条回复缺少结论。验收已由流水线跑完，不用等命令。"
    "请直接给出结论：首行只写 PASS、REWORK 或 NEEDS_CONTROLLER，下面写理由，并附「## 主控重点看」。"
)


class DelegateReviewModelTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def write_user_config(self, doc: dict) -> None:
        cfg_dir = Path(self.env["XDG_CONFIG_HOME"]) / "delegate"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "config.json").write_text(json.dumps(doc))
        for k in (
            "DELEGATE_SCOUT_MODEL",
            "DELEGATE_REVIEW_MODEL",
            "DELEGATE_WRITE_MODEL",
            "DELEGATE_THINKING",
        ):
            self.env.pop(k, None)

    def write_quota_cache(self, five_hour: float, weekly: float, account_id: str = "acct1") -> None:
        sd = self.state_dir()
        sd.mkdir(parents=True, exist_ok=True)
        (sd / "agy-quota.json").write_text(
            json.dumps(
                {
                    "schemaVersion": 1,
                    "updatedAt": "2026-11-24T20:00:00Z",
                    "updatedEpoch": 1795521600,
                    "accounts": {
                        account_id: {
                            "fetchedAt": "2026-11-24T20:00:00Z",
                            "fetchedEpoch": 1795521600,
                            "ok": True,
                            "entries": [
                                {
                                    "pool": "Gemini Models",
                                    "window": "weekly",
                                    "windowLabel": "Weekly Limit Remaining",
                                    "remainingPct": 95.0,
                                    "resetAt": "2026-12-01T00:00:00Z",
                                },
                                {
                                    "pool": "Claude and GPT models",
                                    "window": "fiveHour",
                                    "windowLabel": "Five Hour Limit Remaining",
                                    "remainingPct": five_hour,
                                    "resetAt": "2026-11-25T01:00:00Z",
                                },
                                {
                                    "pool": "Claude and GPT models",
                                    "window": "weekly",
                                    "windowLabel": "Weekly Limit Remaining",
                                    "remainingPct": weekly,
                                    "resetAt": "2026-12-01T00:00:00Z",
                                },
                            ],
                        }
                    },
                }
            )
        )

    def install_scripted_agy(self, script_body: str, models: dict, tiers: dict, extra_user: dict | None = None) -> Path:
        log_file = self.work / "agy-calls.jsonl"
        agy = self.work / "bin" / "agy"
        agy.parent.mkdir(parents=True, exist_ok=True)
        agy.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "argv = sys.argv[1:]\n"
            "if '--version' in argv:\n"
            "    print('1.5.15')\n"
            "    sys.exit(0)\n"
            "model = ''\n"
            "effort = ''\n"
            "conv_in = ''\n"
            "for i, a in enumerate(argv):\n"
            "    if a == '--model' and i + 1 < len(argv):\n"
            "        model = argv[i + 1]\n"
            "    elif a == '--effort' and i + 1 < len(argv):\n"
            "        effort = argv[i + 1]\n"
            "    elif a == '--conversation' and i + 1 < len(argv):\n"
            "        conv_in = argv[i + 1]\n"
            "raw_in = sys.stdin.read()\n"
            "msg = json.loads(raw_in.strip().splitlines()[-1]) if raw_in.strip() else {}\n"
            "content = msg.get('message', {}).get('content', '')\n"
            f"log_path = Path({str(log_file)!r})\n"
            "calls = log_path.read_text().splitlines() if log_path.exists() else []\n"
            "call_idx = len(calls) + 1\n"
            "with log_path.open('a') as f:\n"
            "    f.write(json.dumps({'idx': call_idx, 'model': model, 'effort': effort, 'conv_in': conv_in, 'content': content}) + '\\n')\n"
            "conv_id = conv_in or f'conv-{model}-{call_idx}'\n"
            "def emit_ok(text, cid=None):\n"
            "    c = cid or conv_id\n"
            "    print(json.dumps({'event':'init','conversation_id':c}), flush=True)\n"
            "    print(json.dumps({'event':'step_update','step_update':{'step_index':1,'step_type':'agent_response','state':'DONE','text_delta':text}}), flush=True)\n"
            "    print(json.dumps({'event':'result','result':{'conversation_id':c,'status':'SUCCESS','response':text,'usage':{'input_tokens':20,'output_tokens':10,'cache_read_tokens':5}}}), flush=True)\n"
            "def emit_err(err_msg, code=1):\n"
            "    print(json.dumps({'event':'init','conversation_id':conv_id}), flush=True)\n"
            "    print(json.dumps({'event':'result','result':{'conversation_id':conv_id,'status':'ERROR','error':err_msg,'response':''}}), flush=True)\n"
            "    sys.stderr.write(err_msg + '\\n')\n"
            "    sys.exit(code)\n"
            + script_body
            + "\n"
        )
        agy.chmod(0o755)
        self.env["PATH"] = f"{agy.parent}:{self.env.get('PATH', '')}"
        cfg = {"models": models, "tiers": tiers}
        if extra_user:
            cfg.update(extra_user)
        self.write_user_config(cfg)
        return log_file

    def test_review_model_four_layer_precedence_and_display(self):
        script = (
            "if '审查以下写入任务' in content:\n"
            "    emit_ok(f'PASS\\n审查模型为 {model}\\n\\n## 主控重点看\\n无')\n"
            "else:\n"
            "    Path('work.txt').write_text('updated\\n')\n"
            "    emit_ok('done')\n"
        )
        models = {
            "neon": "agy/gemini-4-neon",
            "tier-rev": "agy/tier-rev-model",
            "user-rev": "agy/user-rev-model",
            "hydrogen": "agy/gemini-4-hydrogen",
            "cli-rev": "agy/cli-rev-model",
        }
        tiers = {"scout": "neon", "review": "tier-rev", "write": "neon"}

        # Layer 4: repo `"review": true`, no user/repo/CLI review model -> review tier (`tier-rev`).
        self.install_scripted_agy(script, models, tiers)
        repo = self.repo(
            {"work.txt": "old\n", ".delegate.json": json.dumps({"accept": "true", "review": True})}
        )
        res = self.cli("run", "--worktree", "--name", "layer4", "edit work", cwd=repo)
        self.assertEqual(res.returncode, 0, res.stderr + res.stdout)
        card = self.cli("card", "layer4", cwd=repo)
        self.assertEqual(card.returncode, 0, card.stderr)
        self.assertIn("tier-rev", card.stdout)
        st = self.cli("status", "layer4", cwd=repo, human=True)
        self.assertIn("tier-rev", st.stdout)

        # Layer 3: user config `review.model = "user-rev"`, repo `"review": true` -> `user-rev`.
        self.install_scripted_agy(
            script,
            models,
            tiers,
            extra_user={"review": {"enabled": True, "model": "user-rev", "thinking": "high"}},
        )
        res = self.cli("run", "--worktree", "--name", "layer3", "edit work", cwd=repo)
        self.assertEqual(res.returncode, 0, res.stderr + res.stdout)
        card = self.cli("card", "layer3", cwd=repo)
        self.assertIn("user-rev", card.stdout)
        st = self.cli("status", "layer3", cwd=repo, human=True)
        self.assertIn("user-rev", st.stdout)

        # Layer 2: repo `review.model = "hydrogen"` overrides user `user-rev`.
        (repo / ".delegate.json").write_text(
            json.dumps(
                {
                    "accept": "true",
                    "review": {"enabled": True, "model": "hydrogen", "thinking": "high"},
                }
            )
        )
        res = self.cli("run", "--worktree", "--name", "layer2", "edit work", cwd=repo)
        self.assertEqual(res.returncode, 0, res.stderr + res.stdout)
        card = self.cli("card", "layer2", cwd=repo)
        self.assertIn("hydrogen", card.stdout)
        st = self.cli("status", "layer2", cwd=repo, human=True)
        self.assertIn("hydrogen", st.stdout)

        # Layer 1: `start/run --review-model cli-rev` overrides repo + user + tier.
        res = self.cli(
            "run",
            "--worktree",
            "--name",
            "layer1",
            "--review-model",
            "cli-rev",
            "edit work",
            cwd=repo,
        )
        self.assertEqual(res.returncode, 0, res.stderr + res.stdout)
        card = self.cli("card", "layer1", cwd=repo)
        self.assertIn("cli-rev", card.stdout)
        st = self.cli("status", "layer1", cwd=repo, human=True)
        self.assertIn("cli-rev", st.stdout)

    def test_review_model_unavailable_falls_back_with_explicit_card_notice(self):
        script = (
            "if '审查以下写入任务' in content:\n"
            "    if model == 'gemini-4-hydrogen':\n"
            "        emit_err('429 RESOURCE_EXHAUSTED: quota exceeded')\n"
            "    if model == 'refuse-model':\n"
            "        emit_ok('The prompt could not be submitted. The prompt contains sensitive words that violate Google\\'s Generative AI Prohibited Use policy.')\n"
            "    emit_ok('PASS\\n备用模型通过\\n\\n## 主控重点看\\n无')\n"
            "else:\n"
            "    Path('work.txt').write_text('updated\\n')\n"
            "    emit_ok('done')\n"
        )
        models = {
            "neon": "agy/gemini-4-neon",
            "hydrogen": "agy/gemini-4-hydrogen",
            "refuse": "agy/refuse-model",
            "rev-fb": "agy/rev-fallback",
        }
        tiers = {"scout": "neon", "review": "rev-fb", "write": "neon"}
        self.install_scripted_agy(script, models, tiers)
        repo = self.repo(
            {
                "work.txt": "old\n",
                ".delegate.json": json.dumps(
                    {
                        "accept": "true",
                        "review": {"enabled": True, "model": "hydrogen", "thinking": "high"},
                    }
                ),
            }
        )

        # Quota exhausted -> fallback to review tier `rev-fb` with explicit card line.
        res = self.cli("run", "--worktree", "--name", "fb-quota", "edit work", cwd=repo)
        self.assertEqual(res.returncode, 0, res.stderr + res.stdout)
        card = self.cli("card", "fb-quota", cwd=repo)
        self.assertEqual(card.returncode, 0, card.stderr)
        self.assertIn("审查改用 rev-fb（额度用尽）", card.stdout)
        self.assertIn("rev-fb", card.stdout)
        st = self.cli("status", "fb-quota", cwd=repo, human=True)
        self.assertIn("rev-fb", st.stdout)
        self.assertIn("审查改用 rev-fb（额度用尽）", st.stdout)

        # Upstream refusal -> fallback to review tier `rev-fb` with explicit card line.
        res2 = self.cli(
            "run",
            "--worktree",
            "--name",
            "fb-refuse",
            "--review-model",
            "refuse",
            "edit work",
            cwd=repo,
        )
        self.assertEqual(res2.returncode, 0, res2.stderr + res2.stdout)
        card2 = self.cli("card", "fb-refuse", cwd=repo)
        self.assertIn("审查改用 rev-fb（上游拒绝）", card2.stdout)

    def test_class_default_thinking_and_explicit_override(self):
        script = "emit_ok('ok')\n"
        models = {
            "neon": {"model": "agy/gemini-4-neon", "thinking": "low"},
        }
        tiers = {"scout": "neon", "review": "neon", "write": "neon"}
        log_file = self.install_scripted_agy(script, models, tiers)
        repo = self.repo({"work.txt": "old\n"})

        # --class build -> medium〔类别 build〕
        r_build = self.cli("run", "--worktree", "--class", "build", "--name", "cls-build", "task", cwd=repo)
        self.assertEqual(r_build.returncode, 0, r_build.stderr + r_build.stdout)
        self.assertIn("thinking medium〔类别 build〕", r_build.stderr)

        # --class hard -> high〔类别 hard〕
        r_hard = self.cli("run", "--worktree", "--class", "hard", "--name", "cls-hard", "task", cwd=repo)
        self.assertEqual(r_hard.returncode, 0, r_hard.stderr + r_hard.stdout)
        self.assertIn("thinking high〔类别 hard〕", r_hard.stderr)

        # --class design -> medium〔类别 design〕
        r_design = self.cli("run", "--worktree", "--class", "design", "--name", "cls-design", "task", cwd=repo)
        self.assertEqual(r_design.returncode, 0, r_design.stderr + r_design.stdout)
        self.assertIn("thinking medium〔类别 design〕", r_design.stderr)

        # --class scout --read-only -> medium〔类别 scout〕
        r_scout = self.cli("run", "--read-only", "--class", "scout", "--name", "cls-scout", "read", cwd=repo)
        self.assertEqual(r_scout.returncode, 0, r_scout.stderr + r_scout.stdout)
        self.assertIn("thinking medium〔类别 scout〕", r_scout.stderr)

        # Explicit --thinking overrides --class hard.
        r_cli = self.cli(
            "run",
            "--worktree",
            "--class",
            "hard",
            "--thinking",
            "low",
            "--name",
            "cls-override",
            "task",
            cwd=repo,
        )
        self.assertEqual(r_cli.returncode, 0, r_cli.stderr + r_cli.stdout)
        self.assertIn("thinking low〔CLI〕", r_cli.stderr)

        # No --class preserves existing alias default (`low`).
        r_none = self.cli("run", "--worktree", "--name", "cls-none", "task", cwd=repo)
        self.assertEqual(r_none.returncode, 0, r_none.stderr + r_none.stdout)
        self.assertIn("thinking low", r_none.stderr)
        self.assertIn("〔别名〕", r_none.stderr)

        efforts = [json.loads(line)["effort"] for line in log_file.read_text().splitlines()]
        self.assertEqual(efforts, ["medium", "high", "medium", "medium", "low", "low"])

    def test_reviewer_format_error_retries_once_and_adopts_or_escalates(self):
        # Case A: First reviewer reply lacks verdict -> retry receives verbatim prompt on same session -> PASS adopted.
        script_recover = (
            "if '审查以下写入任务' in content:\n"
            "    emit_ok('等 clippy 跑完再给结论。')\n"
            "elif '上一条回复缺少结论' in content:\n"
            "    emit_ok('PASS\\n补问后给出通过结论\\n\\n## 主控重点看\\n无')\n"
            "else:\n"
            "    Path('work.txt').write_text('updated\\n')\n"
            "    emit_ok('done')\n"
        )
        models = {"neon": "agy/gemini-4-neon", "hydrogen": "agy/gemini-4-hydrogen"}
        tiers = {"scout": "neon", "review": "neon", "write": "neon"}
        log_a = self.install_scripted_agy(script_recover, models, tiers)
        repo = self.repo(
            {
                "work.txt": "old\n",
                ".delegate.json": json.dumps(
                    {
                        "accept": "true",
                        "review": {"enabled": True, "model": "hydrogen", "thinking": "high"},
                    }
                ),
            }
        )
        res_a = self.cli("run", "--worktree", "--name", "fmt-ok", "edit work", cwd=repo)
        self.assertEqual(res_a.returncode, 0, res_a.stderr + res_a.stdout)
        st_a = json.loads(self.cli("status", "fmt-ok", "--json", cwd=repo).stdout)
        self.assertEqual(st_a["state"], "delivered")
        self.assertEqual(st_a["review"]["verdict"], "PASS")
        self.assertTrue(st_a["review"].get("formatRetried"))
        self.assertFalse(st_a["review"].get("formatError", False))

        calls_a = [json.loads(line) for line in log_a.read_text().splitlines()]
        rev_calls_a = [c for c in calls_a if c["model"] == "gemini-4-hydrogen"]
        self.assertEqual(len(rev_calls_a), 2, calls_a)
        self.assertTrue(rev_calls_a[1]["conv_in"], "retry must continue same conversation")
        self.assertIn(EXPECTED_FORMAT_RETRY_PROMPT, rev_calls_a[1]["content"])

        # Case B: Retry also lacks verdict -> only 1 retry, NEEDS_CONTROLLER (formatError), card notes 补问 1 次仍无结论.
        log_a.unlink()
        script_fail = (
            "if '审查以下写入任务' in content:\n"
            "    emit_ok('等 clippy 跑完再给结论。')\n"
            "elif '上一条回复缺少结论' in content:\n"
            "    emit_ok('还是先看看日志吧')\n"
            "else:\n"
            "    Path('work.txt').write_text('updated\\n')\n"
            "    emit_ok('done')\n"
        )
        log_b = self.install_scripted_agy(script_fail, models, tiers)
        res_b = self.cli("run", "--worktree", "--name", "fmt-fail", "edit work", cwd=repo)
        self.assertEqual(res_b.returncode, 0, res_b.stderr + res_b.stdout)
        st_b = json.loads(self.cli("status", "fmt-fail", "--json", cwd=repo).stdout)
        self.assertEqual(st_b["review"]["verdict"], "NEEDS_CONTROLLER")
        self.assertEqual(st_b["review"]["upgradedBy"], "formatError")
        self.assertTrue(st_b["review"]["formatError"])
        self.assertTrue(st_b["review"]["formatRetryFailed"])
        card_b = self.cli("card", "fmt-fail", cwd=repo)
        self.assertIn("补问 1 次仍无结论", card_b.stdout)

        calls_b = [json.loads(line) for line in log_b.read_text().splitlines()]
        rev_calls_b = [c for c in calls_b if c["model"] == "gemini-4-hydrogen"]
        self.assertEqual(len(rev_calls_b), 2, "must retry at most once")

    def test_extra_review_triggers_quota_guard_and_disagreement(self):
        script = (
            "if '独立只读补充审查员' in content:\n"
            "    emit_ok('REWORK\\n补充审查发现高风险缺陷\\n\\n## 主控重点看\\n- [高] migrations/001.sql:1 — 迁移风险')\n"
            "elif '独立只读审查员' in content:\n"
            "    emit_ok('PASS\\n主审查通过\\n\\n## 主控重点看\\n无')\n"
            "else:\n"
            "    if 'MIGRATION' in content:\n"
            "        Path('migrations').mkdir(exist_ok=True)\n"
            "        Path('migrations/001.sql').write_text('ALTER TABLE t ADD COLUMN x INT;\\n')\n"
            "    else:\n"
            "        Path('plain.txt').write_text('normal edit\\n')\n"
            "    emit_ok('done')\n"
        )
        models = {
            "neon": "agy/gemini-4-neon",
            "hydrogen": "agy/gemini-4-hydrogen",
            "sonnet": "agy/claude-sonnet-4",
        }
        tiers = {"scout": "neon", "review": "neon", "write": "neon"}
        log_file = self.install_scripted_agy(script, models, tiers)
        repo = self.repo(
            {
                "plain.txt": "old\n",
                ".delegate.json": json.dumps(
                    {
                        "accept": "true",
                        "review": {
                            "enabled": True,
                            "model": "hydrogen",
                            "thinking": "high",
                            "extra": {
                                "model": "sonnet",
                                "on": ["controllerReviewPaths", "risk:high"],
                                "minQuota": {"fiveHour": 50, "weekly": 30},
                            },
                        },
                    }
                ),
            }
        )

        # 1. Ordinary edit (no controllerReviewPaths, no high risk) -> extra reviewer NOT started.
        self.write_quota_cache(five_hour=80.0, weekly=70.0)
        res1 = self.cli("run", "--worktree", "--name", "ex-nohit", "edit plain", cwd=repo)
        self.assertEqual(res1.returncode, 0, res1.stderr + res1.stdout)
        calls1 = [json.loads(line) for line in log_file.read_text().splitlines()]
        self.assertFalse(any(c["model"] == "claude-sonnet-4" for c in calls1), calls1)
        st1 = json.loads(self.cli("status", "ex-nohit", "--json", cwd=repo).stdout)
        self.assertEqual(st1["review"]["verdict"], "PASS")
        self.assertNotIn("extra", st1["review"])

        # 2. High-risk edit (migrations/001.sql) with low Claude quota (40% < 50%) -> skipped with exact card line.
        log_file.unlink()
        self.write_quota_cache(five_hour=40.0, weekly=60.0)
        res2 = self.cli("run", "--worktree", "--name", "ex-lowq", "MIGRATION add col", cwd=repo)
        self.assertEqual(res2.returncode, 0, res2.stderr + res2.stdout)
        calls2 = [json.loads(line) for line in log_file.read_text().splitlines()]
        self.assertFalse(any(c["model"] == "claude-sonnet-4" for c in calls2), calls2)
        card2 = self.cli("card", "ex-lowq", cwd=repo)
        self.assertIn("补充审查跳过：Claude 池剩余 5 小时 40% / 本周 60%", card2.stdout)
        st2 = json.loads(self.cli("status", "ex-lowq", "--json", cwd=repo).stdout)
        self.assertEqual(st2["review"]["verdict"], "PASS")
        self.assertEqual(st2["review"]["extra"]["status"], "skipped")

        # 3. High-risk edit with sufficient quota (80% / 70%) -> sonnet runs in parallel; PASS vs REWORK -> NEEDS_CONTROLLER + 意见不一致.
        log_file.unlink()
        self.write_quota_cache(five_hour=80.0, weekly=70.0)
        res3 = self.cli("run", "--worktree", "--name", "ex-disagree", "MIGRATION add col", cwd=repo)
        self.assertEqual(res3.returncode, 0, res3.stderr + res3.stdout)
        calls3 = [json.loads(line) for line in log_file.read_text().splitlines()]
        self.assertTrue(any(c["model"] == "claude-sonnet-4" for c in calls3), calls3)
        st3 = json.loads(self.cli("status", "ex-disagree", "--json", cwd=repo).stdout)
        self.assertEqual(st3["review"]["verdict"], "NEEDS_CONTROLLER")
        self.assertEqual(st3["review"]["originalVerdict"], "PASS")
        self.assertTrue(st3["review"]["extra"]["disagreed"])
        card3 = self.cli("card", "ex-disagree", cwd=repo)
        self.assertIn("意见不一致", card3.stdout)
        self.assertIn("sonnet", card3.stdout)


if __name__ == "__main__":
    unittest.main()
