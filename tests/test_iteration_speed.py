import csv
import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "skills/iteration-speed/scripts/iteration-speed"
SPEC = importlib.util.spec_from_file_location("iteration_gate", SCRIPT.with_name("gate.py"))
GATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATE)

CARGO_OUTPUT = """    Finished `test` profile [unoptimized + debuginfo] target(s) in 3.00s
     Running unittests src/lib.rs (target/debug/deps/runtime-123)
test runtime::SECRET_TOKEN ... ok <12.30s>
test runtime::fast ... ok <0.01s>
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 20.00s
     Running tests/schema.rs (target/debug/deps/schema-123)
test schema::validate ... ok <5.00s>
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 10.00s
     Running tests/hash.rs (target/debug/deps/hash-123)
test hash::calculate ... FAILED <6.00s>
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 8.00s
     Running tests/regex.rs (target/debug/deps/regex-123)
test regex::match ... ok <4.00s>
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 7.00s
     Running tests/json.rs (target/debug/deps/json-123)
test json::decode ... ok <3.00s>
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 6.00s
     Running tests/tiny.rs (target/debug/deps/tiny-123)
test tiny::quick ... ok <0.02s>
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
   Doc-tests sample
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 30.00s
unrecognized credentials: SECRET_PASSWORD
"""


class IterationSpeedTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="iteration-speed-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / "fake-repo"
        self.home = self.root / "home"
        self.repo.mkdir()
        self.home.mkdir()
        self.env = {key: value for key, value in os.environ.items()
                    if not key.startswith(("CARGO_", "RUST", "UV_", "XDG_"))}
        self.env.update(HOME=str(self.home), XDG_STATE_HOME=str(self.root / "state"))

    def write(self, relative, text):
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def run_script(self, *args):
        return subprocess.run([str(SCRIPT), *args], cwd=self.repo, env=self.env,
                              capture_output=True, text=True, timeout=15)

    def snapshot(self, path):
        return {str(file.relative_to(path)): file.read_bytes()
                for file in path.rglob("*") if file.is_file()}

    def gate_rows(self):
        log = self.root / "state/iteration-speed/fake-repo.tsv"
        with log.open() as file:
            return list(csv.reader(file, delimiter="\t"))

    def test_gate_budget_exit_codes_and_history_privacy(self):
        result = self.run_script("gate", "--name", "full", "--budget", "1m30s", "--",
                                 "sh", "-c", "printf SECRET_TOKEN; printf SECRET_ERROR >&2; exit 7")
        self.assertEqual(result.returncode, 7)
        self.assertEqual(result.stdout, "SECRET_TOKEN")
        self.assertIn("SECRET_ERROR", result.stderr)
        row = self.gate_rows()[-1]
        self.assertEqual(row[2:5], ["7", "gate", "full"])
        self.assertNotIn("SECRET", "".join(row))
        self.assertNotIn(str(self.repo), "".join(row))
        for command_status, expected in ((0, 3), (7, 7), (3, 3)):
            with self.subTest(command_status=command_status):
                result = self.run_script("gate", "--budget", "0.000001s", "--",
                                         "sh", "-c", f"exit {command_status}")
                self.assertEqual(result.returncode, expected, result.stderr)
                self.assertEqual(self.gate_rows()[-1][2], str(command_status))
                if command_status == 0:
                    for text in ("over budget", "exceeded by", "slowest steps/tests", "SKILL.md"):
                        self.assertIn(text, result.stderr)
                else:
                    self.assertNotIn("over budget", result.stderr)
        result = self.run_script("gate", "--", "sh", "-c", "exit 0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("over budget", result.stderr)
        self.assertIn("\tgate\tdefault\t", self.run_script("history").stdout)

    def test_gate_repository_budget_and_explicit_override_from_subdirectory(self):
        subprocess.run(["git", "init", "-q", str(self.repo)], check=True)
        self.write(".iteration-speed.json", '{"gates":{"full":"0.000001s"}}')
        self.write("nested/marker", "")
        self.repo = self.repo / "nested"
        result = self.run_script("gate", "--name", "full", "--", "sh", "-c", "exit 0")
        self.assertEqual(result.returncode, 3, result.stderr)
        result = self.run_script("gate", "--name", "full", "--budget", "1h", "--", "sh", "-c", "exit 0")
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.run_script("gate", "--name", "unconfigured", "--", "sh", "-c", "exit 0")
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_gate_invalid_budget_and_arguments_do_not_run(self):
        for options in (("--budget", "bogus"), ("--budget", "0s"),
                        ("--budget", "-1m"), ("--budget", "NaN"), ("--name", "bad\tname")):
            with self.subTest(options=options):
                result = self.run_script("gate", *options, "--", "sh", "-c", "touch marker")
                self.assertEqual(result.returncode, 2)
                self.assertFalse((self.repo / "marker").exists())
        for args in (("gate",), ("gate", "--")):
            self.assertEqual(self.run_script(*args).returncode, 2)
        self.write(".iteration-speed.json", '{"gates":{"default":12}}')
        self.assertEqual(self.run_script("gate", "--", "sh", "-c", "touch marker").returncode, 2)
        self.assertFalse((self.repo / "marker").exists())
        self.assertFalse((self.root / "state").exists())

    def test_gate_missing_command_and_signal_status(self):
        self.assertEqual(self.run_script("gate", "--", "./missing").returncode, 127)
        self.assertEqual(self.gate_rows()[-1][2], "127")
        result = self.run_script("gate", "--", "sh", "-c", "kill -TERM $$")
        self.assertEqual(result.returncode, 143)
        self.assertEqual(self.gate_rows()[-1][2], "143")

    def test_gate_recent_five_successful_median_and_regression(self):
        log = self.root / "state/iteration-speed/fake-repo.tsv"
        log.parent.mkdir(parents=True)
        rows = [["UTC", str(seconds), "0", "gate", "full", "{}"]
                for seconds in (100, .000001, .000002, .000003, .000004, .000005)]
        rows += [["UTC", "100", "7", "gate", "full", "{}"],
                 ["UTC", "100", "0", "gate", "other", "{}"],
                 ["UTC", "100", "0", "cwd", "full"],
                 ["UTC", "invalid", "0", "gate", "full", "{}"]]
        with log.open("w", newline="") as file:
            csv.writer(file, delimiter="\t", lineterminator="\n").writerows(rows)
        self.assertEqual(GATE.recent_median(log, "full"), .000003)
        result = self.run_script("gate", "--name", "full", "--", "sh", "-c", "exit 0")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("Gate regression", result.stderr)
        self.assertNotIn("Gate regression", self.run_script("gate", "--name", "other", "--",
                                                          "sh", "-c", "exit 0").stderr)

    def test_gate_budget_and_regression_boundaries(self):
        self.assertEqual(GATE.conclusion(10, 10, 0, None), (0, []))
        self.assertEqual(GATE.conclusion(13, None, 0, 10), (0, []))
        code, messages = GATE.conclusion(13.01, 20, 0, 10)
        self.assertEqual(code, 0)
        self.assertEqual(len(messages), 1)
        self.assertIn("regression", messages[0])
        code, messages = GATE.conclusion(12, 10, 0, None)
        self.assertEqual(code, 3)
        self.assertIn("20.0%", messages[0])

    def test_gate_cargo_fixture_top_five_and_numeric_history(self):
        self.write("cargo-output.txt", CARGO_OUTPUT)
        result = self.run_script("gate", "--", "cat", "cargo-output.txt")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, CARGO_OUTPUT)
        binaries = result.stderr.split("Slowest test binaries: ")[1].splitlines()[0]
        tests = result.stderr.split("Slowest tests: ")[1].splitlines()[0]
        self.assertEqual(binaries.split("; "), ["runtime-123 20.000s", "schema-123 10.000s",
                                               "hash-123 8.000s", "regex-123 7.000s", "json-123 6.000s"])
        self.assertEqual(tests.split("; "), ["runtime::SECRET_TOKEN 12.300s", "hash::calculate 6.000s",
                                            "schema::validate 5.000s", "regex::match 4.000s", "json::decode 3.000s"])
        row = self.gate_rows()[-1]
        self.assertEqual(json.loads(row[5]), {"binary_seconds": [20, 10, 8, 7, 6],
                                            "test_seconds": [12.3, 6, 5, 4, 3]})
        self.assertNotIn("SECRET", "".join(row))
        self.assertNotIn("runtime", "".join(row))

    def test_cargo_parser_handles_split_streams_ansi_and_unknown_output(self):
        summary = GATE.CargoSummary()
        stderr = GATE.OutputLines(summary)
        stdout = GATE.OutputLines(summary)
        stderr.feed(b"\x1b[32m     Running unittests src/lib.rs (target/debug/deps/sample-123)\x1b[0m\n")
        stdout.feed(b"test sample::slow ... ok <1.")
        stdout.feed(b"23s>\ntest result: ok. 1 passed; 0 failed; finished in 2.34s", final=True)
        self.assertEqual(summary.slowest(), ([(2.34, "sample-123")], [(1.23, "sample::slow")]))
        unknown = GATE.CargoSummary()
        for line in ("ordinary output", "finished in 100s", "test malformed ... ok <oops>",
                     "Running custom (target/debug/custom)"):
            unknown.feed(line)
        self.assertEqual(unknown.slowest(), ([], []))
        parser = GATE.OutputLines(unknown)
        parser.feed(b"x" * 70000)
        self.assertLessEqual(len(parser.pending), 65536)
        parser.feed(b"\n", final=True)
        self.assertEqual(unknown.slowest(), ([], []))

    def test_detect_mixed_repo_read_only_and_workspace_chain(self):
        self.write("Cargo.toml", '[workspace]\nmembers = ["packages/*"]\n'
                   '[profile.dev]\ndebug = "line-tables-only"\n')
        self.write("packages/up/Cargo.toml", '[package]\nname = "up"\nversion = "0.1.0"\n')
        self.write("packages/up/src/lib.rs", "// upstream\npub fn up() {}\n")
        self.write("packages/down/Cargo.toml", '[package]\nname = "down"\nversion = "0.1.0"\n'
                   '[dependencies.renamed]\npackage = "up"\npath = "../up"\n')
        self.write("package.json", '{"packageManager": "pnpm@10.0.0"}\n')
        self.write("pnpm-lock.yaml", "lockfileVersion: '9.0'\n")
        self.write(".npmrc", f"store-dir={self.root / 'store'}\n")
        self.write("tsconfig.json", '{"compilerOptions": {"incremental": true}}\n')
        self.write("pyproject.toml", '[project]\nname = "fake"\n')
        self.write("go.mod", "module example.test/fake\n")
        self.write("Dockerfile", "FROM scratch\n")
        self.write("third_party/hidden/Cargo.toml", '[package]\nname = "hidden"\n')
        before = self.snapshot(self.repo)
        result = self.run_script("detect")
        self.assertEqual(result.returncode, 0, result.stderr)
        for ecosystem in ("Rust", "Node", "Python", "Go", "Docker"):
            self.assertIn(ecosystem, result.stdout)
        self.assertIn("up -> down", result.stdout)
        self.assertIn("2 行", result.stdout)
        self.assertIn("pnpm", result.stdout)
        self.assertIn("同一文件系统", result.stdout)
        self.assertNotIn("hidden", result.stdout)
        self.assertEqual(before, self.snapshot(self.repo))
        self.assertEqual({}, self.snapshot(self.home))

    def test_time_exit_status_output_and_history(self):
        result = self.run_script("time", "--", "sh", "-c", "printf 'output'; printf 'error' >&2; exit 7")
        self.assertEqual(result.returncode, 7)
        self.assertEqual(result.stdout, "output")
        self.assertIn("error", result.stderr)
        log = self.root / "state/iteration-speed/fake-repo.tsv"
        fields = log.read_text().rstrip("\n").split("\t")
        self.assertEqual(len(fields), 5)
        self.assertGreaterEqual(float(fields[1]), 0)
        self.assertEqual(fields[2], "7")
        self.assertIn("exit", fields[4])
        self.assertIn(log.read_text(), self.run_script("history").stdout)
        self.assertEqual([], list(log.parent.glob(".timing.*")))
        self.assertEqual(self.run_script("time", "--", "exit", "3").returncode, 3)
        self.assertEqual(log.read_text().splitlines()[-1].split("\t")[2], "3")

    def test_workspace_inherited_alias_and_excluded_member(self):
        self.write("Cargo.toml", '[workspace]\nmembers = ["packages/*"]\n'
                   'exclude = ["packages/ignored"]\n[workspace.dependencies]\n'
                   'alias = { package = "base", path = "packages/base" }\n')
        self.write("packages/base/Cargo.toml", '[package]\nname = "base"\n')
        self.write("packages/leaf/Cargo.toml", '[package]\nname = "leaf"\n'
                   '[dependencies]\nalias = { workspace = true }\n')
        self.write("packages/ignored/Cargo.toml", '[package]\nname = "ignored"\n')
        result = self.run_script("detect")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("base -> leaf", result.stdout)
        self.assertNotIn("packages/ignored", result.stdout)

    def test_setup_print_write_preserves_settings_and_backs_up(self):
        cargo = self.home / ".cargo/config.toml"
        cache = self.home / ".config/sccache/config"
        cargo.parent.mkdir()
        cache.parent.mkdir(parents=True)
        original = ('[build]\njobs = 2\n[alias]\nquick = "test -p fake"\n'
                    '[target.x86_64-unknown-linux-gnu]\nrustflags = ["-C", "debuginfo=1"]\n')
        cargo.write_text(original)
        cache.write_text('[cache.disk]\nsize = 42\ndir = "/tmp/example"\n')
        before = self.snapshot(self.root)
        result = self.run_script("rust-setup", "--print")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(before, self.snapshot(self.root))
        result = self.run_script("rust-setup", "--write")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('rustc-wrapper = "sccache"', cargo.read_text())
        self.assertIn("jobs = 2", cargo.read_text())
        self.assertIn('quick = "test -p fake"', cargo.read_text())
        self.assertIn('"debuginfo=1"', cargo.read_text())
        self.assertIn('dir = "/tmp/example"', cache.read_text())
        self.assertIn("10737418240", cache.read_text())
        self.assertEqual(list(cargo.parent.glob("config.toml.bak.*"))[0].read_text(), original)
        content = cargo.read_text()
        self.assertEqual(self.run_script("rust-setup", "--write").returncode, 0)
        self.assertEqual(cargo.read_text(), content)
        self.assertEqual(len(list(cargo.parent.glob("config.toml.bak.*"))), 2)
        self.assertEqual({}, self.snapshot(self.repo))

    def test_setup_refuses_symlink_into_repository(self):
        target = self.repo / "config.toml"
        target.write_text("# keep\n")
        (self.home / ".cargo").mkdir()
        (self.home / ".cargo/config.toml").symlink_to(target)
        self.assertNotEqual(self.run_script("rust-setup", "--write").returncode, 0)
        self.assertEqual(target.read_text(), "# keep\n")
        self.assertFalse((self.home / ".config").exists())

    def test_setup_conflicting_linker_does_not_write(self):
        if os.uname().sysname != "Linux":
            self.skipTest("Linux target configuration")
        cargo = self.home / ".cargo/config.toml"
        cargo.parent.mkdir()
        cargo.write_text(f'[target.{os.uname().machine}-unknown-linux-gnu]\n'
                         'rustflags = ["-C", "link-arg=-fuse-ld=lld"]\n')
        before = self.snapshot(self.home)
        self.assertNotEqual(self.run_script("rust-setup", "--write").returncode, 0)
        self.assertEqual(before, self.snapshot(self.home))

    def test_time_missing_command_does_not_write(self):
        self.assertNotEqual(self.run_script("time", "--").returncode, 0)
        self.assertFalse((self.root / "state").exists())


if __name__ == "__main__":
    unittest.main()
