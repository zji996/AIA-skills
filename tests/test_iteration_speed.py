import os
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "skills/iteration-speed/scripts/iteration-speed"


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
