"""Tests: writer command large-output spill-to-disk A/B experiment (`cmdOut`)."""
import json
import os
import stat
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateCmdOutTests(unittest.TestCase):
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def setUp(self):
        fixtures.DelegateTests.setUp(self)

    def _install_fake_cargo(self) -> Path:
        bin_dir = self.work / "fake-tools"
        bin_dir.mkdir(exist_ok=True)
        cargo = bin_dir / "cargo"
        cargo.write_text(
            "#!/usr/bin/env python3\n"
            "import sys\n"
            "mode = sys.argv[1] if len(sys.argv) > 1 else 'small'\n"
            "if mode == 'small':\n"
            "    print('small stdout line')\n"
            "    print('small stderr line', file=sys.stderr)\n"
            "    sys.exit(3)\n"
            "elif mode == 'big-err':\n"
            "    for i in range(1, 241):\n"
            "        if i == 12:\n"
            "            print('error[E0308]: mismatched types in src/main.rs', file=sys.stderr)\n"
            "        else:\n"
            "            print(f'build line {i:03d} ' + 'x' * 60)\n"
            "    sys.exit(101)\n"
            "elif mode == 'big-ok':\n"
            "    for i in range(1, 241):\n"
            "        print(f'ok line {i:03d} ' + 'y' * 60)\n"
            "    sys.exit(0)\n",
            encoding="utf-8",
        )
        cargo.chmod(cargo.stat().st_mode | stat.S_IXUSR)
        self.env["PATH"] = f"{bin_dir}:{self.env['PATH']}"
        return cargo

    def _install_pi_running_cargo(self) -> None:
        probe_script = self.work / "run_probe.py"
        probe_script.write_text(
            "import json, subprocess\n"
            "from pathlib import Path\n"
            "results = {}\n"
            "for cmd in (['cargo', 'small'], ['cargo', 'big-err'], ['cargo', 'big-ok']):\n"
            "    p = subprocess.run(cmd, capture_output=True, text=True)\n"
            "    results[cmd[1]] = {'code': p.returncode, 'stdout': p.stdout, 'stderr': p.stderr}\n"
            "Path('probe-results.json').write_text(json.dumps(results), encoding='utf-8')\n"
            "Path('work.txt').write_text('done\\n', encoding='utf-8')\n",
            encoding="utf-8",
        )
        events = [
            {"type": "tool_execution_start", "toolName": "bash", "args": {"command": "cargo test"}},
            {"type": "tool_execution_start", "toolName": "write", "args": {"path": "work.txt"}},
            {
                "type": "message_end",
                "message": {
                    "role": "assistant",
                    "stopReason": "stop",
                    "model": "fake-model",
                    "content": [{"type": "text", "text": "completed"}],
                    "usage": {"input": 2000, "output": 100, "cacheRead": 8000},
                },
            },
            {"type": "agent_end"},
            fixtures.SETTLED,
        ]
        self.fake_pi(events, pre=f"python3 {probe_script}")

    def test_ab_alternation_spill_passthrough_and_reports(self):
        self._install_fake_cargo()
        self._install_pi_running_cargo()
        repo = self.repo({"work.txt": "init\n", ".delegate.json": json.dumps({"scoutFirst": False})})

        # Run 1 -> Group B (treatment, enabled=true)
        st_b = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--no-scout", "--name", "cmdout-b", "--accept", "true", "do B")
        )
        run_b = Path(st_b["dir"])
        sum_b = json.loads((run_b / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(st_b["state"], "delivered", (st_b, sum_b))
        meta_b = json.loads((run_b / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta_b["cmdOutGroup"], "B")
        self.assertTrue(meta_b["cmdOut"]["enabled"])

        wt_b = Path(meta_b["workdir"])
        res_b = json.loads((wt_b / "probe-results.json").read_text(encoding="utf-8"))

        # <= 12KB: verbatim passthrough keeping stdout and stderr strictly separate + exit code preserved
        self.assertEqual(res_b["small"]["code"], 3)
        self.assertIn("small stdout line", res_b["small"]["stdout"])
        self.assertNotIn("small stderr line", res_b["small"]["stdout"])
        self.assertIn("small stderr line", res_b["small"]["stderr"])
        self.assertNotIn("small stdout line", res_b["small"]["stderr"])
        self.assertNotIn("[输出", res_b["small"]["stdout"])

        # > 12KB with error[: spilled to 001.log, exact header + error block + tail 40 + exit code 101
        self.assertEqual(res_b["big-err"]["code"], 101)
        log1 = run_b / "cmd-out/001.log"
        self.assertTrue(log1.is_file())
        out_err = res_b["big-err"]["stdout"]
        self.assertIn(f"全文在 {log1}；下面是首个错误起 40 行与末尾 40 行]", out_err)
        self.assertIn("error[E0308]: mismatched types in src/main.rs", out_err)
        self.assertIn("build line 240", out_err)

        # > 12KB without error: spilled to 002.log, exact no-error header + tail 40 + exit code 0
        self.assertEqual(res_b["big-ok"]["code"], 0)
        log2 = run_b / "cmd-out/002.log"
        self.assertTrue(log2.is_file())
        out_ok = res_b["big-ok"]["stdout"]
        self.assertIn(f"全文在 {log2}；下面是末尾 40 行]", out_ok)
        self.assertIn("ok line 240", out_ok)
        self.assertNotIn("ok line 001", out_ok)

        # Summary records triggers, truncatedBytes, authorSteps, compileTestSteps
        self.assertEqual(sum_b["cmdOut"]["group"], "B")
        self.assertEqual(sum_b["cmdOut"]["triggers"], 2)
        self.assertGreater(sum_b["cmdOut"]["truncatedBytes"], 10_000)
        self.assertGreaterEqual(sum_b["cmdOut"]["authorSteps"], 2)
        self.assertGreaterEqual(sum_b["cmdOut"]["compileTestSteps"], 1)

        # Run 2 -> Group A (control, enabled=false): NOT wrapped
        st_a = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--no-scout", "--name", "cmdout-a", "--accept", "true", "do A")
        )
        self.assertEqual(st_a["state"], "delivered")
        run_a = Path(st_a["dir"])
        meta_a = json.loads((run_a / "meta.json").read_text(encoding="utf-8"))
        sum_a = json.loads((run_a / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(meta_a["cmdOutGroup"], "A")
        self.assertFalse(meta_a["cmdOut"]["enabled"])
        self.assertFalse((run_a / "cmd-out").exists())

        wt_a = Path(meta_a["workdir"])
        res_a = json.loads((wt_a / "probe-results.json").read_text(encoding="utf-8"))
        self.assertEqual(res_a["big-err"]["code"], 101)
        self.assertNotIn("[输出", res_a["big-err"]["stdout"])
        self.assertIn("build line 001", res_a["big-err"]["stdout"])
        self.assertEqual(sum_a["cmdOut"]["triggers"], 0)
        self.assertEqual(sum_a["cmdOut"]["truncatedBytes"], 0)

        # `delegate usage --by cmd-out --all --json` reports A vs B
        u_res = self.cli("usage", "--since", "24h", "--by", "cmd-out", "--all", "--json")
        self.assertEqual(u_res.returncode, 0, u_res.stderr)
        u_doc = json.loads(u_res.stdout)
        self.assertIn("A", u_doc["cmdOutAb"])
        self.assertIn("B", u_doc["cmdOutAb"])
        self.assertEqual(u_doc["cmdOutAb"]["B"]["triggers"], 2)
        self.assertEqual(u_doc["cmdOutAb"]["A"]["triggers"], 0)
        self.assertIsNotNone(u_doc["cmdOutAb"]["B"]["avgInputPerStep"])
        self.assertGreaterEqual(u_doc["cmdOutAb"]["B"]["compileTestSteps"], 1)
        self.assertEqual(u_doc["cmdOutAb"]["B"]["acceptFirstPassRate"], 1.0)
        self.assertEqual(u_doc["cmdOutAb"]["A"]["acceptFirstPassRate"], 1.0)

        # `delegate speed --since 7d --json` also exposes cmdOutAb
        sp_res = self.cli("speed", "--since", "7d", "--json")
        self.assertEqual(sp_res.returncode, 0, sp_res.stderr)
        sp_doc = json.loads(sp_res.stdout)
        self.assertIn("cmdOutAb", sp_doc)
        self.assertEqual(sp_doc["cmdOutAb"]["B"]["triggers"], 2)
        self.assertEqual(sp_doc["cmdOutAb"]["A"]["triggers"], 0)
        self.assertEqual(sp_doc["cmdOutAb"]["B"]["acceptFirstPassRate"], 1.0)

    def test_disabled_in_repo_config_skips_grouping_and_wrapping(self):
        self._install_fake_cargo()
        self._install_pi_running_cargo()
        repo = self.repo({
            "work.txt": "init\n",
            ".delegate.json": json.dumps({"scoutFirst": False, "cmdOut": False}),
        })
        st = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--no-scout", "--name", "cmdout-off", "--accept", "true", "do off")
        )
        self.assertEqual(st["state"], "delivered")
        run_dir = Path(st["dir"])
        meta = json.loads((run_dir / "meta.json").read_text(encoding="utf-8"))
        self.assertNotIn("cmdOutGroup", meta)
        self.assertNotIn("cmdOut", meta)
        self.assertFalse((run_dir / "cmd-out").exists())
        wt = Path(meta["workdir"])
        res = json.loads((wt / "probe-results.json").read_text(encoding="utf-8"))
        self.assertNotIn("[输出", res["big-err"]["stdout"])
        self.assertIn("build line 001", res["big-err"]["stdout"])


if __name__ == "__main__":
    unittest.main()
