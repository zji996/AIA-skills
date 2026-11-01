"""6.9.0 `probe`: check the tier models through Pi itself, before dispatching.

Loaded by the repository's conformance suite; every assertion drives the CLI as a subprocess
with a fake `pi` on PATH, so no test contacts a real model.
"""
import importlib.util
import json
import re
import sys
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
fixtures = sys.modules.get("tests.test_delegate") or sys.modules.get("test_delegate")
if fixtures is None:
    spec = importlib.util.spec_from_file_location("delegate_fixtures", ROOT / "tests/test_delegate.py")
    fixtures = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixtures)

ANSWER = fixtures.answer("ok")
SETTLED = fixtures.SETTLED


class ProbeTests(unittest.TestCase):
    def setUp(self):
        fixtures.DelegateTests.setUp(self)
        self.prompts = self.work / "prompts.log"

    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    user_config = fixtures.DelegateTests.user_config

    def streaming_pi(self, delay=0.6):
        """Pi that streams a start event, waits, then answers; records stdin and argv."""
        model_rows = ("provider model context max-out thinking images\\n"
                      "p cheap-m 1M 1K yes yes\\np strong-m 1M 1K yes yes\\np m 1M 1K yes yes\\n")
        self.env["PROMPT_LOG"] = str(self.prompts)
        script = self.bin / "pi"
        script.write_text("\n".join([
            "#!/bin/sh",
            '[ "$1" = --version ] && { echo "fake-pi 1.0"; exit 0; }',
            f'[ "$2" = --list-models ] && {{ printf "{model_rows}"; exit 0; }}',
            'prompt=$(cat)',
            'printf "%s\\n" "$prompt" >> "$PROMPT_LOG"',
            'echo "$*" >> "$PI_LOG"',
            "printf '%s\\n' '{\"type\":\"message_start\"}'",
            f"sleep {delay}",
            "printf '%s\\n' " + "'" + json.dumps(ANSWER) + "'",
            "printf '%s\\n' " + "'" + json.dumps(SETTLED) + "'",
            "",
        ]))
        script.chmod(0o755)

    def runs_dir_untouched(self):
        self.assertFalse(Path(self.env["DELEGATE_RUNS"]).exists())
        state = Path(self.env["XDG_STATE_HOME"]) / "delegate"
        self.assertEqual(list(state.glob("*.slot")) if state.exists() else [], [])

    def test_probe_answers_through_pi_for_every_tier_model(self):
        self.streaming_pi()
        self.user_config({"tiers": {"cheap": "p/cheap-m",
                                    "strong": [{"model": "p/strong-m", "maxActive": 5}, "p/m"]}})
        result = self.cli("probe", timeout=60)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        lines = result.stdout.splitlines()
        # One line per distinct model the tiers name, rules expanded and deduplicated.
        self.assertEqual([line.split()[0] for line in lines[:3]], ["ok", "ok", "ok"])
        self.assertIn("p/cheap-m", lines[0])
        self.assertIn("p/strong-m", lines[1])
        self.assertIn("p/m", lines[2])
        self.assertIn("maxActive 0/5", lines[1])
        # Without a default tier, start picks cheap for reads and strong for writes.
        self.assertIn("现在选：只读 cheap p/cheap-m", lines[-1])
        self.assertIn("写入 strong p/strong-m", lines[-1])
        elapsed = [float(m) for m in re.findall(r"\s(\d+\.\d)s\s+首字", result.stdout)]
        first = [float(m) for m in re.findall(r"首字 (\d+\.\d)s", result.stdout)]
        self.assertEqual(len(elapsed), 3)
        self.assertTrue(all(f < e for f, e in zip(first, elapsed)), result.stdout)
        # Same invocation a real dispatch makes: print mode, JSON events, thinking off.
        calls = Path(self.env["PI_LOG"]).read_text().splitlines()
        self.assertEqual(len(calls), 3)
        for call in calls:
            self.assertIn("--mode json", call)
            self.assertIn("--thinking off", call)
            self.assertTrue(call.endswith(" -p"), call)
        self.assertEqual(self.prompts.read_text().split("\n")[:3], ["只回复 ok"] * 3)
        self.runs_dir_untouched()

    def test_probe_prints_pis_first_error_line_and_fails_the_default_pick(self):
        self.fake_pi([{"type": "message_end", "message": {
            "role": "assistant", "stopReason": "error", "content": [],
            "errorMessage": "HTTP 502: Upstream access forbidden\nupstream detail line"}}], code=1)
        self.user_config({"tiers": {"default": "p/strong-m", "cheap": "p/cheap-m", "strong": "p/m"}})
        result = self.cli("probe")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        lines = result.stdout.splitlines()
        self.assertTrue(lines[0].startswith("FAIL"), lines[0])
        self.assertIn("p/strong-m", lines[0])
        self.assertIn("HTTP 502: Upstream access forbidden", lines[0])
        self.assertNotIn("upstream detail line", result.stdout)
        self.assertIn("现在选：p/strong-m（FAIL）", lines[-1])
        self.runs_dir_untouched()

    def test_probe_times_out_and_stops_pi(self):
        self.fake_pi([ANSWER, SETTLED], sleep=30)
        self.user_config({"tiers": {"cheap": "p/m", "strong": "p/m"}})
        started = time.monotonic()
        result = self.cli("probe", "--timeout", "0.5", timeout=60)
        self.assertLess(time.monotonic() - started, 25)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("FAIL", result.stdout)
        self.assertIn("timeout 0.5s", result.stdout)

    def test_probe_reports_rule_windows_and_the_pick_it_lands_on(self):
        self.fake_pi([ANSWER, SETTLED])
        rules = [{"model": "p/strong-m", "days": "mon-fri", "hours": "09:00-12:00"}, "p/m"]
        self.user_config({"tiers": {"cheap": "p/cheap-m", "strong": rules}})
        local = time.localtime()
        inside = 1 <= local.tm_wday + 1 <= 5 and 9 * 60 <= local.tm_hour * 60 + local.tm_min < 12 * 60
        expected = "p/strong-m" if inside else "p/m"
        result = self.cli("probe", "--tier", "strong")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        lines = result.stdout.splitlines()
        self.assertEqual(len(lines), 3, result.stdout)
        rule_line = next(line for line in lines if "p/strong-m" in line)
        self.assertIn("mon-fri 09:00-12:00", rule_line)
        self.assertIn("hit" if inside else "miss", rule_line)
        self.assertEqual("=now" in rule_line, inside)
        self.assertIn(expected, lines[-1])
        # --tier limits what is probed, so the other tier's model is not tested here.
        self.assertIn("（未测）", lines[-1])

    def test_probe_argument_errors_exit_two(self):
        self.user_config({"tiers": {"cheap": "p/cheap-m", "strong": "p/strong-m"}})
        for args in (("--tier", "bogus"), ("--model", "p/nope"), ("--model", "cheap-m"),
                     ("--tier", "cheap", "--model", "p/m"), ("--timeout", "0")):
            with self.subTest(args=args):
                self.assertEqual(self.cli("probe", *args).returncode, 2)
        self.env.pop("DELEGATE_CHEAP_MODEL", None)
        self.env.pop("DELEGATE_STRONG_MODEL", None)
        (Path(self.env["XDG_CONFIG_HOME"]) / "delegate/config.json").unlink()
        unset = self.cli("probe")
        self.assertEqual(unset.returncode, 2)
        self.assertIn("no model to probe", unset.stderr)


if __name__ == "__main__":
    unittest.main()
