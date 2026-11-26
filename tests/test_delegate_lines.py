"""Tests: `delegate lines` deterministic line-change stats across registered repos,
excludes & `!` re-inclusion, classification, rename detection, merge exclusion,
window boundaries, reply-chain token ratio, caching, error tolerance, and `apply --commit` trailer.
"""
import json
import os
import subprocess
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


def _git(repo: Path, *args: str, commit_ts: int | None = None) -> str:
    env = dict(os.environ)
    env.setdefault("GIT_AUTHOR_NAME", "tester")
    env.setdefault("GIT_AUTHOR_EMAIL", "tester@example.test")
    env.setdefault("GIT_COMMITTER_NAME", "tester")
    env.setdefault("GIT_COMMITTER_EMAIL", "tester@example.test")
    if commit_ts is not None:
        stamp = f"{commit_ts} +0000"
        env["GIT_AUTHOR_DATE"] = stamp
        env["GIT_COMMITTER_DATE"] = stamp
    proc = subprocess.run(
        ["git", "-C", str(repo), *args],
        env=env,
        text=True,
        capture_output=True,
        check=True,
    )
    return proc.stdout.strip()


def _init_repo(path: Path, base_ts: int, initial_files: dict[str, str | bytes] | None = None) -> Path:
    path.mkdir(parents=True, exist_ok=True)
    _git(path, "init", "-q", "-b", "main")
    _git(path, "config", "user.name", "tester")
    _git(path, "config", "user.email", "tester@example.test")
    files = initial_files if initial_files is not None else {"seed.txt": "seed\n"}
    for rel, content in files.items():
        fp = path / rel
        fp.parent.mkdir(parents=True, exist_ok=True)
        if isinstance(content, bytes):
            fp.write_bytes(content)
        else:
            fp.write_text(content, encoding="utf-8")
    _git(path, "add", "-A")
    _git(path, "commit", "-qm", "init", commit_ts=base_ts)
    return path


def _commit_files(
    repo: Path,
    files: dict[str, str | bytes | None],
    message: str,
    commit_ts: int,
) -> str:
    for rel, content in files.items():
        fp = repo / rel
        if content is None:
            if fp.exists():
                fp.unlink()
        else:
            fp.parent.mkdir(parents=True, exist_ok=True)
            if isinstance(content, bytes):
                fp.write_bytes(content)
            else:
                fp.write_text(content, encoding="utf-8")
    _git(repo, "add", "-A")
    _git(repo, "commit", "-qm", message, commit_ts=commit_ts)
    return _git(repo, "rev-parse", "HEAD")


class DelegateLinesTests(unittest.TestCase):
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def run_write(self, repo, pre, name="task", *options):
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre=pre)
        return self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", name, *options, "task")
        )

    def setUp(self):
        fixtures.DelegateTests.setUp(self)
        self.state_dir = Path(self.env["XDG_STATE_HOME"]) / "delegate"
        self.cache_dir = Path(self.env["XDG_CACHE_HOME"]) / "delegate"
        self.state_dir.mkdir(parents=True, exist_ok=True)
        self.cache_dir.mkdir(parents=True, exist_ok=True)
        self.env.pop("DELEGATE_RUNS", None)

    def _register_repos(self, repos: list[Path]) -> None:
        doc = {"repos": {}}
        for idx, rp in enumerate(repos):
            rdir = rp / ".local/run/delegate"
            rdir.mkdir(parents=True, exist_ok=True)
            doc["repos"][f"k{idx}"] = {
                "repo": str(rp),
                "runs": str(rdir),
            }
        (self.state_dir / "repos.json").write_text(json.dumps(doc), encoding="utf-8")

    def test_classification_excludes_renames_merges_boundaries_and_tokens(self):
        now = int(time.time())
        ts_old_10d = now - 10 * 86400
        ts_3d = now - 3 * 86400
        ts_just_outside_24h = now - 86400 - 120
        ts_inside_24h = now - 3600

        # Initial repo state 10 days ago (outside 7d window).
        seed_lines = "\n".join(f"fn f{i}() {{ println!(\"{i}\"); }}" for i in range(40)) + "\n"
        repo_a = _init_repo(
            self.work / "repo-alpha",
            ts_old_10d,
            {
                "harness-v1/core.rs": seed_lines,
                "harness-v1/helper.rs": seed_lines,
                "src/app.rs": "fn a() {}\nfn b() {}\n",
            },
        )
        # Repo `.delegate.json`: exclude vendor/**, re-include vendor/keep.rs and generated/keep.rs via `!`.
        (repo_a / ".delegate.json").write_text(
            json.dumps(
                {
                    "lineStats": {
                        "exclude": [
                            "vendor/**",
                            "!vendor/keep.rs",
                            "!**/generated/keep.rs",
                        ]
                    }
                }
            ),
            encoding="utf-8",
        )
        _git(repo_a, "add", ".delegate.json")
        _git(repo_a, "commit", "-qm", "chore: config", commit_ts=ts_old_10d + 10)

        # Commit 1 (just outside 24h, inside 7d) — OTHER (no Delegate-Run trailer):
        # 5 lines code + 100 lines excluded lockfile + binary file.
        _commit_files(
            repo_a,
            {
                "src/other.rs": "a\nb\nc\nd\ne\n",
                "Cargo.lock": "".join(f"lock {i}\n" for i in range(100)),
                "pnpm-lock.yaml": "lockfileVersion: 9\n" * 50,
                "package-lock.json": "{}\n" * 50,
                "yarn.lock": "pkg\n" * 50,
                "assets/logo.bin": b"\x00\x01\x02\x03\x04\x05\xff",
            },
            "chore: outside 24h manual edit",
            ts_just_outside_24h,
        )

        # Commit 2 (3 days ago, inside 7d) — whole directory rename `harness-v1 -> harness-v2`
        # with only 2 real modified lines in core.rs (0 lines in helper.rs); OTHER commit.
        (repo_a / "harness-v2").mkdir()
        (repo_a / "harness-v1/core.rs").rename(repo_a / "harness-v2/core.rs")
        (repo_a / "harness-v1/helper.rs").rename(repo_a / "harness-v2/helper.rs")
        (repo_a / "harness-v2/core.rs").write_text(seed_lines + "// added 1\n// added 2\n", encoding="utf-8")
        _git(repo_a, "add", "-A")
        _git(repo_a, "commit", "-qm", "refactor: bump harness dir", commit_ts=ts_3d)

        # Merge commit inside 24h — MUST be excluded (`--no-merges`).
        _git(repo_a, "checkout", "-q", "-b", "side")
        _commit_files(repo_a, {"src/branch_only.rs": "b1\nb2\n"}, "feat: side branch", ts_old_10d + 20)
        _git(repo_a, "checkout", "-q", "main")
        _git(repo_a, "merge", "--no-ff", "-q", "-m", "Merge side into main", "side", commit_ts=ts_inside_24h - 600)

        # Commit 3 (inside 24h) — DELEGATED (`Delegate-Run: run-round-2`):
        # - Code: src/app.rs (+3, -1), vendor/keep.rs (+2), src/generated/keep.rs (+1) -> total code +6 / -1
        # - Excluded: vendor/drop.rs, src/generated/auto.rs, ui/__snapshots__/a.snap, foo.snap, api.golden.md
        # - Test: tests/unit.rs (+4), tests/README.md (+2, test NOT docs!), src/foo_test.rs (+1),
        #         web/a.test.tsx (+1), web/b.spec.ts (+1), scripts/test_ok.py (+1) -> total test +10 / -0
        # - Docs: README.md (+3), docs/guide.md (+2) -> total docs +5 / -0
        _commit_files(
            repo_a,
            {
                "src/app.rs": "fn a() {}\nfn c() {}\nfn d() {}\nfn e() {}\n",
                "vendor/keep.rs": "k1\nk2\n",
                "vendor/drop.rs": "d1\nd2\nd3\n",
                "src/generated/keep.rs": "gk1\n",
                "src/generated/auto.rs": "ga1\nga2\nga3\n",
                "ui/__snapshots__/a.snap": "s1\ns2\n",
                "foo.snap": "s3\n",
                "docs/api.golden.md": "g1\ng2\n",
                "tests/unit.rs": "t1\nt2\nt3\nt4\n",
                "tests/README.md": "tm1\ntm2\n",
                "src/foo_test.rs": "tr1\n",
                "web/a.test.tsx": "tt1\n",
                "web/b.spec.ts": "ts1\n",
                "scripts/test_ok.py": "tp1\n",
                "README.md": "d1\nd2\nd3\n",
                "docs/guide.md": "d4\nd5\n",
            },
            "feat: delegated delivery\n\nDelegate-Run: run-round-2\n",
            ts_inside_24h,
        )

        # Wire up reply chain: run-round-1 (archived, 10 days ago, Pi model!) -> run-round-2 (active, agy model!).
        # Round 1 tokens (outside 7d window — MUST NOT be truncated!): 300_000 (input 100k + output 50k + cacheRead 150k)
        # Round 2 tokens (agy, in agy-usage.jsonl): 200_000 (input 50k + output 50k + cacheRead 100k)
        # Total chain tokens = 500_000 = 0.5M.
        # Delegated churn in 24h = (6+1) + 10 + 5 = 22 lines -> 22 / 0.5 = 44 lines per million tokens!
        rdir = repo_a / ".local/run/delegate"
        r2_dir = rdir / "run-round-2"
        r2_dir.mkdir(parents=True, exist_ok=True)
        (r2_dir / "meta.json").write_text(
            json.dumps({"run": "run-round-2", "parent": "run-round-1", "model": "agy/gemini-4-neon"}),
            encoding="utf-8",
        )
        # Round 1 archived under <cache>/archive/run-round-1/record/
        arch_rec = self.cache_dir / "archive" / "run-round-1" / "record"
        arch_rec.mkdir(parents=True, exist_ok=True)
        (arch_rec / "meta.json").write_text(
            json.dumps({"run": "run-round-1", "model": "pi/sonnet", "startedAt": "2026-11-10T00:00:00Z"}),
            encoding="utf-8",
        )
        (arch_rec / "summary.json").write_text(
            json.dumps(
                {
                    "finishedAt": "2026-11-10T00:05:00Z",
                    "tokensDelta": {"input": 100_000, "output": 50_000, "cacheRead": 150_000},
                }
            ),
            encoding="utf-8",
        )
        # Write round 2 into agy-usage.jsonl
        (self.state_dir / "agy-usage.jsonl").write_text(
            json.dumps(
                {
                    "run": "run-round-2",
                    "epoch": ts_inside_24h,
                    "isAgy": True,
                    "model": "agy/gemini-4-neon",
                    "role": "author",
                    "tokens": {"input": 50_000, "output": 50_000, "cacheRead": 100_000},
                }
            )
            + "\n",
            encoding="utf-8",
        )

        self._register_repos([repo_a])

        res_json = self.cli("lines", "--json", cwd=self.work)
        self.assertEqual(res_json.returncode, 0, res_json.stderr)
        data = json.loads(res_json.stdout)

        self.assertEqual(len(data["windows"]), 2)
        w24, w7d = data["windows"][0], data["windows"][1]
        self.assertEqual(w24["key"], "24h")
        self.assertEqual(w7d["key"], "7d")

        # 24h checks (only Commit 3; Commit 1 is 24h+120s ago, Commit 2 is 3d ago, Merge commit excluded)
        self.assertEqual(w24["byCategory"]["code"]["total"], {"added": 6, "deleted": 1, "churn": 7})
        self.assertEqual(w24["byCategory"]["test"]["total"], {"added": 10, "deleted": 0, "churn": 10})
        self.assertEqual(w24["byCategory"]["docs"]["total"], {"added": 5, "deleted": 0, "churn": 5})
        self.assertEqual(w24["total"], {"added": 21, "deleted": 1, "churn": 22})
        self.assertEqual(w24["byAttribution"]["delegated"]["churn"], 22)
        self.assertEqual(w24["byAttribution"]["other"]["churn"], 0)
        self.assertAlmostEqual(w24["delegatedSharePct"], 100.0, places=1)
        self.assertEqual(set(w24["chainRuns"]), {"run-round-1", "run-round-2"})
        self.assertEqual(w24["tokens"]["total"], 500_000)
        self.assertAlmostEqual(w24["linesPerMillionTokens"], 44.0, places=1)

        # 7d checks (Commit 1: +5 code; Commit 2: +2 code from whole-dir rename; Commit 3: +6/-1 code, +10 test, +5 docs)
        self.assertEqual(w7d["byCategory"]["code"]["total"], {"added": 13, "deleted": 1, "churn": 14})
        self.assertEqual(w7d["byCategory"]["test"]["total"], {"added": 10, "deleted": 0, "churn": 10})
        self.assertEqual(w7d["byCategory"]["docs"]["total"], {"added": 5, "deleted": 0, "churn": 5})
        self.assertEqual(w7d["total"], {"added": 28, "deleted": 1, "churn": 29})
        self.assertEqual(w7d["byAttribution"]["delegated"]["churn"], 22)
        self.assertEqual(w7d["byAttribution"]["other"]["churn"], 7)
        self.assertAlmostEqual(w7d["delegatedSharePct"], round(22.0 * 100.0 / 29.0, 2), places=1)
        self.assertEqual(w7d["tokens"]["total"], 500_000)
        self.assertAlmostEqual(w7d["linesPerMillionTokens"], 44.0, places=1)
        self.assertEqual(len(data["daily7d"]), 7)
        self.assertEqual(data["errors"], [])

        # Human output check (verbatim header, U+2212 minus, formatted rows)
        res_human = self.cli("lines", cwd=self.work)
        self.assertEqual(res_human.returncode, 0, res_human.stderr)
        out = res_human.stdout
        self.assertTrue(out.startswith("代码改动（各仓库默认分支，不含合并提交与生成文件）\n"), out)
        self.assertIn("24 小时", out)
        self.assertIn("7 天", out)
        self.assertIn("+21 / \u22121", out)
        self.assertIn("+28 / \u22121", out)
        self.assertIn("44 行", out)

        # Single-window `--since 48h` shows 1 column
        res_since = self.cli("lines", "--since", "48h", "--json", cwd=self.work)
        self.assertEqual(res_since.returncode, 0, res_since.stderr)
        d48 = json.loads(res_since.stdout)
        self.assertEqual(len(d48["windows"]), 1)
        self.assertEqual(d48["windows"][0]["key"], "48h")
        # Includes Commit 1 (+5) and Commit 3 (+21/-1), excludes Commit 2 (3d ago)
        self.assertEqual(d48["windows"][0]["total"], {"added": 26, "deleted": 1, "churn": 27})

    def test_by_repo_sorting_unmatched_tokens_and_single_repo_failure(self):
        now = int(time.time())
        ts_old = now - 10 * 86400
        ts_recent = now - 1800

        repo_small = _init_repo(self.work / "repo-small", ts_old)
        _commit_files(
            repo_small,
            {"a.rs": "1\n2\n"},
            "feat: small\n\nDelegate-Run: unknown-run-without-usage\n",
            ts_recent,
        )

        repo_big = _init_repo(self.work / "repo-big", ts_old)
        _commit_files(
            repo_big,
            {"b.rs": "".join(f"line {i}\n" for i in range(1200))},
            "feat: big",
            ts_recent,
        )

        # Non-git broken repo directory (still has .local/run/delegate so discover_snapshot_repos sees it).
        repo_broken = self.work / "repo-broken"
        (repo_broken / ".local/run/delegate").mkdir(parents=True, exist_ok=True)

        self._register_repos([repo_small, repo_big, repo_broken])

        res = self.cli("lines", "--by", "repo", cwd=self.work)
        self.assertEqual(res.returncode, 0, res.stderr)
        out = res.stdout
        # Thousand comma separator for 1,200+ lines
        self.assertIn("+1,200 / \u22120", out)
        # Missing usage shows em-dash '—'
        self.assertIn("每百万 token", out)
        self.assertIn("—", out)
        # --by repo rows appear after 合计 and before 委派占比, sorted descending by 7d churn
        pos_total = out.index("合计")
        pos_big = out.index("[repo-big]")
        pos_small = out.index("[repo-small]")
        pos_share = out.index("委派占比")
        self.assertLess(pos_total, pos_big)
        self.assertLess(pos_big, pos_small)
        self.assertLess(pos_small, pos_share)
        # Single broken repo diagnostic line at end
        self.assertIn("（repo-broken 读取失败：", out)

        # JSON also records the error while keeping good repos intact
        jres = self.cli("lines", "--json", cwd=self.work)
        self.assertEqual(jres.returncode, 0, jres.stderr)
        jdata = json.loads(jres.stdout)
        self.assertEqual(len(jdata["errors"]), 1)
        self.assertEqual(jdata["errors"][0]["repo"], "repo-broken")
        w7_repos = [r["repo"] for r in jdata["windows"][1]["byRepo"]]
        self.assertEqual(w7_repos, ["repo-big", "repo-small"])
        self.assertIsNone(jdata["windows"][0]["linesPerMillionTokens"])

    def test_invalid_line_stats_config_fails_closed_naming_field(self):
        now = int(time.time())
        repo = _init_repo(self.work / "repo-badcfg", now - 3600)
        (repo / ".delegate.json").write_text(
            json.dumps({"lineStats": {"exclude": [123]}}),
            encoding="utf-8",
        )
        self._register_repos([repo])

        res = self.cli("lines", cwd=self.work)
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("lineStats.exclude[0]", res.stderr)

        # Also rejected by `delegate config`
        cfg_res = self.cli("config", cwd=repo)
        self.assertNotEqual(cfg_res.returncode, 0)
        self.assertIn("lineStats.exclude[0]", cfg_res.stderr)

    def test_cache_reused_when_head_unchanged(self):
        now = int(time.time())
        repo = _init_repo(self.work / "repo-cache", now - 10 * 86400)
        _commit_files(repo, {"x.rs": "a\nb\nc\n"}, "feat: cached", now - 600)
        self._register_repos([repo])

        r1 = self.cli("lines", "--json", cwd=self.work)
        self.assertEqual(r1.returncode, 0, r1.stderr)
        cache_file = self.state_dir / "line-stats-cache.json"
        self.assertTrue(cache_file.is_file(), "cache file should be written to state dir")
        cache_doc = json.loads(cache_file.read_text(encoding="utf-8"))
        self.assertEqual(len(cache_doc["entries"]), 1)

        # Replace git on PATH with a stub that fails if invoked — HEAD unchanged must hit cache without calling git!
        stub_dir = self.work / "no-git-bin"
        stub_dir.mkdir(exist_ok=True)
        fake_git = stub_dir / "git"
        fake_git.write_text("#!/bin/sh\necho 'git must not be called on cache hit' >&2\nexit 99\n", encoding="utf-8")
        fake_git.chmod(0o755)
        old_path = self.env["PATH"]
        try:
            self.env["PATH"] = f"{stub_dir}:{old_path}"
            r2 = self.cli("lines", "--json", cwd=self.work)
            self.assertEqual(r2.returncode, 0, r2.stderr)
            d2 = json.loads(r2.stdout)
            self.assertEqual(d2["windows"][0]["total"], {"added": 3, "deleted": 0, "churn": 3})
        finally:
            self.env["PATH"] = old_path

    def test_apply_commit_writes_delegate_run_trailer(self):
        repo = self.repo({"seed.txt": "seed\n"})
        pre = (
            "printf 'line1\\nline2\\n' > feature.rs && "
            "git add feature.rs && "
            "git -c user.name=t -c user.email=t@t commit -qm 'feat: add feature'"
        )
        state = self.run_write(repo, pre, "trailer-test", "--accept", "true")
        run_id = state["run"]

        ap = self.cli("apply", "--commit", run_id, cwd=repo, human=True)
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)

        msg = _git(repo, "log", "-1", "--format=%B")
        self.assertIn(f"Delegate-Run: {run_id}", msg)

        # Verify `delegate lines --json` counts this commit as delegated!
        self._register_repos([repo])
        lres = self.cli("lines", "--json", cwd=repo)
        self.assertEqual(lres.returncode, 0, lres.stderr)
        ldata = json.loads(lres.stdout)
        w24 = ldata["windows"][0]
        self.assertIn(run_id, w24["delegatedRuns"])
        self.assertEqual(w24["byAttribution"]["delegated"]["added"], 2)


if __name__ == "__main__":
    unittest.main()
