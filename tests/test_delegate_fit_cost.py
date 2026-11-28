"""End-to-end tests for model alias cost tiers, four fit classes (build/hard/review/design),
classModels routing, designPaths inference, removed class errors, legacy fit.json cleanup,
cost display in fit/tiers, and cost echo on start/reply.
"""
import json
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateFitCostTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    fake_pi = fixtures.DelegateTests.fake_pi

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

    def matrix_config(self) -> dict:
        return {
            "models": {
                "cheap-a": {"model": "p/scout-m", "cost": "low"},
                "main": {"model": "p/write-m", "thinking": "medium", "cost": "main"},
                "advisor": {"model": "p/review-m", "cost": "high"},
                "plain": "p/m",
            },
            "thinking": "high",
            "tiers": {
                "scout": "cheap-a",
                "review": "advisor",
                "write": "main",
            },
        }

    def test_cost_validation_accepts_valid_and_rejects_invalid(self):
        self.fake_pi([])
        self.write_user_config(self.matrix_config())
        ok = self.cli("tiers")
        self.assertEqual(ok.returncode, 0, ok.stderr)

        for bad_cost in ("cheap", "medium", "", 10, True, None):
            bad = self.matrix_config()
            bad["models"]["cheap-a"]["cost"] = bad_cost
            self.write_user_config(bad)
            res = self.cli("tiers")
            self.assertEqual(res.returncode, 2, f"bad_cost={bad_cost!r}: {res.stderr}")
            self.assertIn("models.cheap-a.cost", res.stderr)

        unknown = self.matrix_config()
        unknown["models"]["cheap-a"]["price"] = "low"
        self.write_user_config(unknown)
        res = self.cli("tiers")
        self.assertEqual(res.returncode, 2, res.stderr)
        self.assertIn("models.cheap-a", res.stderr)

    def test_fit_fifth_class_review_and_default_thinking(self):
        self.write_user_config(self.matrix_config())
        set1 = self.cli("fit", "set", "review", "advisor", "first", "抓缺陷稳", "--sample", "review-spec.md")
        self.assertEqual(set1.returncode, 0, set1.stderr)
        set2 = self.cli("fit", "set", "review", "cheap-a", "avoid", "容易漏边界")
        self.assertEqual(set2.returncode, 0, set2.stderr)

        table = self.cli("fit")
        self.assertEqual(table.returncode, 0, table.stderr)
        self.assertIn("review |", table.stdout)
        self.assertIn("design |", table.stdout)
        self.assertNotIn("scout |", table.stdout)
        self.assertIn("first · 抓缺陷稳", table.stdout)
        self.assertIn("avoid · 容易漏边界", table.stdout)
        self.assertIn("review × main", table.stdout)
        self.assertEqual(len(table.stdout.splitlines()), 6)

        # Setting a new first in review demotes previous first to ok.
        demote = self.cli("fit", "set", "review", "main", "first", "性价比高")
        self.assertEqual(demote.returncode, 0, demote.stderr)
        self.assertIn("advisor 自动降为 ok", demote.stdout)

        # --class review uses medium default thinking and shows fit card notices.
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        run = self.cli("run", "--read-only", "--model", "cheap-a", "--class", "review", "check diff")
        self.assertEqual(run.returncode, 0, run.stderr)
        self.assertIn("thinking medium〔类别 review〕", run.stderr)
        self.assertIn("cheap-a 在 review 类别标记 avoid：容易漏边界", run.stderr)
        self.assertIn("该类首选是 main", run.stderr)

    def test_fit_and_tiers_show_cost_and_json_includes_cost(self):
        self.fake_pi([])
        self.write_user_config(self.matrix_config())

        fit_table = self.cli("fit")
        self.assertEqual(fit_table.returncode, 0, fit_table.stderr)
        header = fit_table.stdout.splitlines()[0]
        for token in ("cheap-a〔low〕", "main〔main〕", "advisor〔high〕", "plain〔—〕"):
            self.assertIn(token, header)

        fit_json_res = self.cli("fit", "--json")
        self.assertEqual(fit_json_res.returncode, 0, fit_json_res.stderr)
        card = json.loads(fit_json_res.stdout)
        self.assertEqual(
            card["cost"],
            {"cheap-a": "low", "main": "main", "advisor": "high", "plain": None},
        )

        tiers = self.cli("tiers")
        self.assertEqual(tiers.returncode, 0, tiers.stderr)
        self.assertIn("aliases: name → model | cost | context | images | thinking", tiers.stdout)
        self.assertIn("cheap-a → p/scout-m | low |", tiers.stdout)
        self.assertIn("main → p/write-m | main |", tiers.stdout)
        self.assertIn("advisor → p/review-m | high |", tiers.stdout)
        self.assertIn("plain → p/m | — |", tiers.stdout)

    def test_start_and_reply_echo_model_cost_without_blocking(self):
        self.write_user_config(self.matrix_config())
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])

        # Explicit --model with cost
        pinned = self.cli("run", "--read-only", "--model", "main", "task1")
        self.assertEqual(pinned.returncode, 0, pinned.stderr)
        self.assertIn("固定模型 main〔main〕（p/write-m）", pinned.stderr)

        # Explicit --tier with cost, then reply inheriting original session
        tiered = self.cli("run", "--tier", "scout", "task2")
        self.assertEqual(tiered.returncode, 0, tiered.stderr)
        self.assertIn("scout 档，模型 cheap-a〔low〕（p/scout-m）", tiered.stderr)
        run_id = self.outcome(tiered)["run"]

        rep = self.cli("reply", "--wait", run_id, "follow up")
        self.assertEqual(rep.returncode, 0, rep.stderr)
        self.assertIn("沿用原会话的模型 cheap-a〔low〕（p/scout-m）", rep.stderr)

        # Inferred tier with cost
        inferred = self.cli("run", "--read-only", "task3")
        self.assertEqual(inferred.returncode, 0, inferred.stderr)
        self.assertIn("→ advisor〔high〕（p/review-m", inferred.stderr)

        # Alias without cost keeps clean legacy echo
        plain = self.cli("run", "--read-only", "--model", "plain", "task4")
        self.assertEqual(plain.returncode, 0, plain.stderr)
        self.assertIn("固定模型 p/m，thinking", plain.stderr)
        self.assertNotIn("〔—〕", plain.stderr)

    def test_removed_classes_exit_2_with_exact_error(self):
        self.write_user_config(self.matrix_config())
        expected = (
            "delegate: class 'scout' was removed; use build (spec'd implementation), "
            "hard (ledger/concurrency/sandbox/migration), review (read-only review) "
            "or design (design drafts)"
        )

        # start --class scout
        res = self.cli("start", "--read-only", "--class", "scout", "task")
        self.assertEqual(res.returncode, 2, res.stderr)
        self.assertIn(expected, res.stderr)

        # fit set scout
        res = self.cli("fit", "set", "scout", "main", "ok", "legacy note")
        self.assertEqual(res.returncode, 2, res.stderr)
        self.assertIn(expected, res.stderr)

        # reply --class scout
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
        base = self.cli("run", "--read-only", "--model", "main", "base task")
        self.assertEqual(base.returncode, 0, base.stderr)
        run_id = self.outcome(base)["run"]
        rep = self.cli("reply", run_id, "--class", "scout", "follow up")
        self.assertEqual(rep.returncode, 2, rep.stderr)
        self.assertIn(expected, rep.stderr)

    def test_legacy_scout_entry_silently_ignored_and_design_retained(self):
        self.write_user_config(self.matrix_config())
        fit_path = Path(self.env["XDG_CONFIG_HOME"]) / "delegate" / "fit.json"
        fit_path.write_text(
            json.dumps(
                {
                    "scout": {"cheap-a": {"verdict": "first", "note": "旧探路", "date": "2026-11-01"}},
                    "design": {"main": {"verdict": "first", "note": "设计稿稳", "date": "2026-11-01"}},
                    "build": {"main": {"verdict": "first", "note": "保留记录", "date": "2026-11-01"}},
                }
            )
        )

        table = self.cli("fit")
        self.assertEqual(table.returncode, 0, table.stderr)
        self.assertEqual(table.stderr, "")
        self.assertIn("build |", table.stdout)
        self.assertIn("design |", table.stdout)
        self.assertIn("first · 保留记录", table.stdout)
        self.assertIn("first · 设计稿稳", table.stdout)
        self.assertNotIn("scout", table.stdout)
        self.assertNotIn("旧探路", table.stdout)
        self.assertEqual(len(table.stdout.splitlines()), 6)

        as_json = self.cli("fit", "--json")
        self.assertEqual(as_json.returncode, 0, as_json.stderr)
        self.assertEqual(as_json.stderr, "")
        parsed = json.loads(as_json.stdout)
        self.assertNotIn("scout", parsed)
        self.assertIn("design", parsed)
        self.assertIn("build", parsed)

        upd = self.cli("fit", "set", "hard", "advisor", "first", "疑难稳")
        self.assertEqual(upd.returncode, 0, upd.stderr)
        saved = json.loads(fit_path.read_text())
        self.assertEqual(set(saved.keys()), {"build", "design", "hard"})
        self.assertNotIn("scout", saved)

    def test_tiers_hints_all_three_and_json_and_after_update(self):
        self.fake_pi([])
        cfg = self.matrix_config()
        cfg["tiers"] = {"scout": "advisor", "review": "advisor", "write": "main"}
        self.write_user_config(cfg)

        expected = [
            "提示：审查档 advisor（价位 high）比写入档 main（价位 main）贵；审查一般用便宜一档的模型就够，漏检靠 Fixes-Run 统计盯（见 references/model-routing.md「换模型」）。",
            "提示：写入档 main 还没有 build 类适配卡记录；先拿一份规格同题对比再长期用（delegate fit）。",
            "提示：探路档 advisor（价位 high）比写入档 main（价位 main）贵；探路只读不写，一般用中价模型加中等思考。",
        ]

        res = self.cli("tiers")
        self.assertEqual(res.returncode, 0, res.stderr)
        lines = res.stdout.splitlines()
        self.assertEqual(lines[-3:], expected)

        as_json = self.cli("tiers", "--json")
        self.assertEqual(as_json.returncode, 0, as_json.stderr)
        payload = json.loads(as_json.stdout)
        self.assertEqual(payload["hints"], expected)
        self.assertIn("tiers", payload)
        self.assertIn("aliases", payload)
        self.assertIn("unregistered", payload)

        # Updating tiers via CLI also prints hints in both text and --json modes.
        updated = self.cli("tiers", "--scout", "cheap-a")
        self.assertEqual(updated.returncode, 0, updated.stderr)
        self.assertEqual(updated.stdout.splitlines()[-2:], expected[:2])
        self.assertNotIn("提示：探路档", updated.stdout)

        updated_json = self.cli("tiers", "--review", "cheap-a", "--json")
        self.assertEqual(updated_json.returncode, 0, updated_json.stderr)
        self.assertEqual(json.loads(updated_json.stdout)["hints"], [expected[1]])

    def test_tiers_hints_none_when_clean_and_skip_incomparable_costs(self):
        self.fake_pi([])
        cfg = self.matrix_config()
        cfg["tiers"] = {"scout": "main", "review": "cheap-a", "write": "main"}
        self.write_user_config(cfg)

        fit_res = self.cli("fit", "set", "build", "main", "first", "规格实现稳")
        self.assertEqual(fit_res.returncode, 0, fit_res.stderr)

        clean = self.cli("tiers")
        self.assertEqual(clean.returncode, 0, clean.stderr)
        self.assertNotIn("提示：", clean.stdout)

        clean_json = self.cli("tiers", "--json")
        self.assertEqual(clean_json.returncode, 0, clean_json.stderr)
        self.assertEqual(json.loads(clean_json.stdout)["hints"], [])

        # Incomparable cost: review/scout has no cost ("plain") -> no cost hints.
        cfg["tiers"] = {"scout": "plain", "review": "plain", "write": "main"}
        self.write_user_config(cfg)
        incomp = self.cli("tiers")
        self.assertEqual(incomp.returncode, 0, incomp.stderr)
        self.assertNotIn("提示：", incomp.stdout)
        self.assertEqual(json.loads(self.cli("tiers", "--json").stdout)["hints"], [])

        # Incomparable cost: write has no cost ("plain") -> no cost hints even when review/scout are high,
        # but missing build fit record for plain still triggers the build fit hint.
        cfg["tiers"] = {"scout": "advisor", "review": "advisor", "write": "plain"}
        self.write_user_config(cfg)
        incomp_write = self.cli("tiers", "--json")
        self.assertEqual(incomp_write.returncode, 0, incomp_write.stderr)
        self.assertEqual(
            json.loads(incomp_write.stdout)["hints"],
            ["提示：写入档 plain 还没有 build 类适配卡记录；先拿一份规格同题对比再长期用（delegate fit）。"],
        )

    def test_tiers_hints_warns_when_agy_agent_is_configured(self):
        self.fake_pi([])
        expected = (
            "agy.agent=aia：agy 1.5.17 起自定义 agent 的对话按更高费率扣额度"
            "（上游实测约 4 倍），不需要时删掉这一项，规则写进 ~/.gemini/GEMINI.md。"
        )
        # Even without write tier configured, agy.agent warning still appears in hints.
        self.write_user_config({"agy": {"agent": "aia"}})
        res_missing = self.cli("tiers", "--json")
        self.assertEqual(res_missing.returncode, 1, res_missing.stderr)
        self.assertIn(expected, json.loads(res_missing.stdout)["hints"])

        cfg = self.matrix_config()
        cfg["tiers"] = {"scout": "main", "review": "cheap-a", "write": "main"}
        cfg["agy"] = {"agent": "aia"}
        self.write_user_config(cfg)
        self.assertEqual(self.cli("fit", "set", "build", "main", "first", "稳").returncode, 0)

        res_text = self.cli("tiers")
        self.assertEqual(res_text.returncode, 0, res_text.stderr)
        self.assertIn(expected, res_text.stdout.splitlines())
        res_json = self.cli("tiers", "--json")
        self.assertEqual(res_json.returncode, 0, res_json.stderr)
        self.assertEqual(json.loads(res_json.stdout)["hints"], [expected])

    def test_design_class_explicit_inferred_fallback_and_model_precedence(self):
        subprocess.run(["git", "init", "-q"], cwd=self.work, check=True)
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])

        # Prepare config with carbon alias + classModels.design
        cfg = self.matrix_config()
        cfg["models"]["carbon"] = {"model": "p/gpt-m", "cost": "high"}
        cfg["classModels"] = {"design": "carbon"}
        self.write_user_config(cfg)

        # Configure repo .delegate.json with designPaths
        repo_cfg_path = self.work / ".delegate.json"
        repo_cfg_path.write_text(json.dumps({"designPaths": ["apps/ui-sandbox/"]}))

        # 1. Explicit --class design without --model -> uses classModels.design + thinking high
        explicit = self.cli("run", "--read-only", "--class", "design", "review layout")
        self.assertEqual(explicit.returncode, 0, explicit.stderr)
        self.assertIn("类别 design（classModels），模型 carbon〔high〕（p/gpt-m）", explicit.stderr)
        self.assertIn("thinking high〔类别 design〕", explicit.stderr)
        self.assertNotIn("推断", explicit.stderr)
        meta1 = json.loads((Path(self.outcome(explicit)["dir"]) / "meta.json").read_text())
        self.assertEqual(meta1["class"], "design")
        self.assertEqual(meta1["model"], "p/gpt-m")
        self.assertEqual(meta1["thinking"], "high")
        self.assertNotIn("classInferred", meta1)

        # 2. Inferred via designPaths without --class -> uses classModels.design and marks 推断
        inferred = self.cli(
            "run",
            "--read-only",
            "更新 apps/ui-sandbox/Login.tsx 的视觉稿",
        )
        self.assertEqual(inferred.returncode, 0, inferred.stderr)
        self.assertIn("类别 design（推断）（classModels），模型 carbon〔high〕（p/gpt-m）", inferred.stderr)
        self.assertIn("thinking high〔类别 design（推断）〕", inferred.stderr)
        meta2 = json.loads((Path(self.outcome(inferred)["dir"]) / "meta.json").read_text())
        self.assertEqual(meta2["class"], "design")
        self.assertTrue(meta2.get("classInferred"))
        self.assertEqual(meta2["model"], "p/gpt-m")
        self.assertEqual(meta2["thinking"], "high")

        # Keyword-only prompt without matching path MUST NOT infer design
        keyword_only = self.cli(
            "run",
            "--read-only",
            "给登录页出个设计稿和 UI 配色方案",
        )
        self.assertEqual(keyword_only.returncode, 0, keyword_only.stderr)
        self.assertNotIn("p/gpt-m", keyword_only.stderr)
        meta_kw = json.loads((Path(self.outcome(keyword_only)["dir"]) / "meta.json").read_text())
        self.assertNotIn("class", meta_kw)
        self.assertEqual(meta_kw["model"], "p/review-m")

        # 3. Unconfigured classModels.design -> falls back to tier default + prints fallback notice
        cfg_no_class_model = self.matrix_config()
        self.write_user_config(cfg_no_class_model)

        fallback_explicit = self.cli("run", "--read-only", "--class", "design", "layout draft")
        self.assertEqual(fallback_explicit.returncode, 0, fallback_explicit.stderr)
        self.assertIn("未配置 classModels.design", fallback_explicit.stderr)
        self.assertIn("thinking high〔类别 design〕", fallback_explicit.stderr)
        meta3 = json.loads((Path(self.outcome(fallback_explicit)["dir"]) / "meta.json").read_text())
        self.assertEqual(meta3["class"], "design")
        self.assertEqual(meta3["model"], "p/review-m")
        self.assertEqual(meta3["thinking"], "high")

        fallback_inferred = self.cli(
            "run",
            "--read-only",
            "检查 apps/ui-sandbox/button.html 的间距",
        )
        self.assertEqual(fallback_inferred.returncode, 0, fallback_inferred.stderr)
        self.assertIn("类别 design（推断） 未配置 classModels.design", fallback_inferred.stderr)
        self.assertIn("thinking high〔类别 design（推断）〕", fallback_inferred.stderr)
        meta3b = json.loads((Path(self.outcome(fallback_inferred)["dir"]) / "meta.json").read_text())
        self.assertEqual(meta3b["class"], "design")
        self.assertTrue(meta3b.get("classInferred"))
        self.assertEqual(meta3b["model"], "p/review-m")

        # 4. Explicit --model ALWAYS wins over classModels.design (both explicit and inferred)
        self.write_user_config(cfg)
        pinned_explicit = self.cli(
            "run",
            "--read-only",
            "--class",
            "design",
            "--model",
            "plain",
            "layout draft",
        )
        self.assertEqual(pinned_explicit.returncode, 0, pinned_explicit.stderr)
        self.assertIn("固定模型 p/m，thinking high〔类别 design〕（--model）", pinned_explicit.stderr)
        self.assertNotIn("p/gpt-m", pinned_explicit.stderr)
        meta4a = json.loads((Path(self.outcome(pinned_explicit)["dir"]) / "meta.json").read_text())
        self.assertEqual(meta4a["class"], "design")
        self.assertEqual(meta4a["model"], "p/m")

        pinned_inferred = self.cli(
            "run",
            "--read-only",
            "--model",
            "plain",
            "修改 apps/ui-sandbox/card.tsx 配色",
        )
        self.assertEqual(pinned_inferred.returncode, 0, pinned_inferred.stderr)
        self.assertIn("固定模型 p/m，thinking high〔类别 design（推断）〕（--model；类别 design（推断））", pinned_inferred.stderr)
        self.assertNotIn("p/gpt-m", pinned_inferred.stderr)
        meta4b = json.loads((Path(self.outcome(pinned_inferred)["dir"]) / "meta.json").read_text())
        self.assertEqual(meta4b["class"], "design")
        self.assertTrue(meta4b.get("classInferred"))
        self.assertEqual(meta4b["model"], "p/m")

        # Fit card supports design class directly
        fit_set = self.cli("fit", "set", "design", "carbon", "first", "视觉层次稳")
        self.assertEqual(fit_set.returncode, 0, fit_set.stderr)
        fit_tbl = self.cli("fit")
        self.assertEqual(fit_tbl.returncode, 0, fit_tbl.stderr)
        self.assertIn("design |", fit_tbl.stdout)
        self.assertIn("first · 视觉层次稳", fit_tbl.stdout)

    def test_class_models_and_design_paths_validation(self):
        subprocess.run(["git", "init", "-q"], cwd=self.work, check=True)
        self.fake_pi([])

        bad_class = self.matrix_config()
        bad_class["classModels"] = {"unknown": "main"}
        self.write_user_config(bad_class)
        res = self.cli("tiers")
        self.assertEqual(res.returncode, 2, res.stderr)
        self.assertIn("classModels.unknown", res.stderr)

        bad_val = self.matrix_config()
        bad_val["classModels"] = {"design": 123}
        self.write_user_config(bad_val)
        res2 = self.cli("tiers")
        self.assertEqual(res2.returncode, 2, res2.stderr)
        self.assertIn("classModels.design", res2.stderr)

        # designPaths in user config is ignored with warning
        user_dp = self.matrix_config()
        user_dp["designPaths"] = ["apps/ui-sandbox/"]
        self.write_user_config(user_dp)
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
        run_ign = self.cli("run", "--read-only", "check apps/ui-sandbox/a.tsx")
        self.assertEqual(run_ign.returncode, 0, run_ign.stderr)
        self.assertIn("ignoring repository-only fields: designPaths", run_ign.stderr)

        # Invalid designPaths in repo .delegate.json exits 2
        self.write_user_config(self.matrix_config())
        (self.work / ".delegate.json").write_text(json.dumps({"designPaths": [""]}))
        res3 = self.cli("tiers")
        self.assertEqual(res3.returncode, 2, res3.stderr)
        self.assertIn("designPaths", res3.stderr)


if __name__ == "__main__":
    unittest.main()
