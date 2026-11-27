"""End-to-end tests for model alias cost tiers, three fit classes (build/hard/review),
removed class errors, legacy fit.json cleanup, cost display in fit/tiers, and cost echo on start/reply.
"""
import json
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
        self.assertNotIn("scout |", table.stdout)
        self.assertNotIn("design |", table.stdout)
        self.assertIn("first · 抓缺陷稳", table.stdout)
        self.assertIn("avoid · 容易漏边界", table.stdout)
        self.assertIn("review × main", table.stdout)
        self.assertEqual(len(table.stdout.splitlines()), 5)

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
        expected = lambda name: (
            f"delegate: class '{name}' was removed; use build (spec'd implementation, "
            "incl. UI from the controller's design), hard (ledger/concurrency/sandbox/migration) "
            "or review (read-only review)"
        )

        # start --class design / scout
        for removed in ("design", "scout"):
            res = self.cli("start", "--read-only", "--class", removed, "task")
            self.assertEqual(res.returncode, 2, res.stderr)
            self.assertIn(expected(removed), res.stderr)

        # fit set design / scout
        for removed in ("design", "scout"):
            res = self.cli("fit", "set", removed, "main", "ok", "legacy note")
            self.assertEqual(res.returncode, 2, res.stderr)
            self.assertIn(expected(removed), res.stderr)

        # reply --class design / scout
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
        base = self.cli("run", "--read-only", "--model", "main", "base task")
        self.assertEqual(base.returncode, 0, base.stderr)
        run_id = self.outcome(base)["run"]
        for removed in ("design", "scout"):
            rep = self.cli("reply", run_id, "--class", removed, "follow up")
            self.assertEqual(rep.returncode, 2, rep.stderr)
            self.assertIn(expected(removed), rep.stderr)

    def test_legacy_scout_and_design_entries_silently_ignored_and_dropped(self):
        self.write_user_config(self.matrix_config())
        fit_path = Path(self.env["XDG_CONFIG_HOME"]) / "delegate" / "fit.json"
        fit_path.write_text(
            json.dumps(
                {
                    "scout": {"cheap-a": {"verdict": "first", "note": "旧探路", "date": "2026-11-01"}},
                    "design": {"main": {"verdict": "first", "note": "旧界面", "date": "2026-11-01"}},
                    "build": {"main": {"verdict": "first", "note": "保留记录", "date": "2026-11-01"}},
                }
            )
        )

        table = self.cli("fit")
        self.assertEqual(table.returncode, 0, table.stderr)
        self.assertEqual(table.stderr, "")
        self.assertIn("build |", table.stdout)
        self.assertIn("first · 保留记录", table.stdout)
        self.assertNotIn("scout", table.stdout)
        self.assertNotIn("design", table.stdout)
        self.assertNotIn("旧探路", table.stdout)
        self.assertNotIn("旧界面", table.stdout)
        self.assertEqual(len(table.stdout.splitlines()), 5)

        as_json = self.cli("fit", "--json")
        self.assertEqual(as_json.returncode, 0, as_json.stderr)
        self.assertEqual(as_json.stderr, "")
        parsed = json.loads(as_json.stdout)
        self.assertNotIn("scout", parsed)
        self.assertNotIn("design", parsed)
        self.assertIn("build", parsed)

        upd = self.cli("fit", "set", "hard", "advisor", "first", "疑难稳")
        self.assertEqual(upd.returncode, 0, upd.stderr)
        saved = json.loads(fit_path.read_text())
        self.assertEqual(set(saved.keys()), {"build", "hard"})
        self.assertNotIn("scout", saved)
        self.assertNotIn("design", saved)

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


if __name__ == "__main__":
    unittest.main()
