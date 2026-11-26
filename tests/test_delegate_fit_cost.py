"""End-to-end tests for model alias cost tiers, fifth fit class (review),
cost display in fit/tiers, and cost echo on start/reply.
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
        self.assertIn("first · 抓缺陷稳", table.stdout)
        self.assertIn("avoid · 容易漏边界", table.stdout)
        self.assertIn("review × main", table.stdout)
        self.assertEqual(len(table.stdout.splitlines()), 7)

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


if __name__ == "__main__":
    unittest.main()
