"""Long controller session regressions: evidence, index locks, commits, conflict handoff."""
import json
import shlex
import shutil
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate618Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def run_tree(self, repo, pre, answer="done", *options):
        self.fake_pi([fixtures.answer(answer), fixtures.SETTLED], pre=pre)
        result = self.cli("run", "--worktree", "--workdir", repo, *options, "task")
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return self.outcome(result)

    def clean_outcome(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return json.loads(result.stdout.splitlines()[0])

    def test_clean_archives_ignored_and_result_references_and_routes_history(self):
        repo = self.repo({"a.txt": "old\n", ".gitignore": ".local/\ntarget/\nnode_modules/\n.cache/\n"})
        state = self.run_tree(repo, "mkdir -p .local/raw target node_modules .cache; "
                              "echo report > .local/report; echo trace > .local/raw/trace; "
                              "ln -s report .local/report-link; ln -s /dev/null .local/external; "
                              "echo explicit > target/mentioned; echo build > target/omit; "
                              "echo dependency > node_modules/omit; echo cache > .cache/omit; "
                              "echo untracked > evidence.txt",
                              "Report: `.local/report`, `.local/raw/`, `target/mentioned`, `evidence.txt`, `a.txt`.")
        cleaned = self.clean_outcome(self.cli("clean", "--json", state["run"]))
        archive = Path(cleaned["archive"])
        self.assertFalse(Path(state["worktree"]).exists())
        self.assertFalse(Path(state["dir"]).exists())
        self.assertEqual(sorted(str(p.relative_to(archive / "files")) for p in (archive / "files").rglob("*") if p.is_file()),
                         [".local/raw/trace", ".local/report", ".local/report-link", "a.txt", "evidence.txt", "target/mentioned"])
        self.assertEqual(self.outcome(self.cli("status", state["run"]))["archive"], str(archive))
        result = self.cli("result", state["run"])
        self.assertIn(".local/report", result.stdout)
        self.assertIn(str(archive), result.stderr)
        self.assertIn(str(archive), self.cli("status", state["run"], human=True).stdout)

    def test_archive_preserves_all_evidence_without_size_cap_and_status_warns_on_total_size(self):
        self.env["DELEGATE_ARCHIVE_MAX_BYTES"] = "10"
        repo = self.repo({"a.txt": "old\n", ".gitignore": ".local/\n"})
        state = self.run_tree(repo, "mkdir .local; printf 123456 > .local/ref; "
                              "printf 123456789 > .local/large; printf 1234 > .local/small", ".local/ref")
        result = self.cli("clean", "--json", state["run"])
        archive = Path(self.clean_outcome(result)["archive"])
        manifest = json.loads((archive / "manifest.json").read_text())
        self.assertEqual(manifest["files"], [".local/ref", ".local/small", ".local/large"])
        self.assertEqual(manifest["bytes"], 19)
        self.assertEqual(manifest["skipped"], [])
        self.assertNotIn("size cap", result.stderr)

        # Status surfaces an archive size anomaly warning when total archive size exceeds threshold.
        self.env["DELEGATE_ARCHIVE_WARN_BYTES"] = "10"
        st_human = self.cli("status", human=True)
        self.assertEqual(st_human.returncode, 0, st_human.stderr)
        self.assertIn("归档总量异常偏大", st_human.stdout)
        st_json = self.cli("status")
        summary_line = [json.loads(l) for l in st_json.stdout.splitlines() if "statusSummary" in l][-1]["statusSummary"]
        self.assertTrue(summary_line["archiveSizeWarning"])
        self.assertEqual(summary_line["archiveWarnBytes"], 10)

    def test_clean_no_archive_and_archive_copy_error_preserves_tree(self):
        repo = self.repo({"a.txt": "old\n", ".gitignore": ".local/\n"})
        state = self.run_tree(repo, "mkdir .local; echo proof > .local/report")
        root = self.work / "cache/delegate/archive"
        root.parent.mkdir(parents=True, exist_ok=True)
        root.write_text("block directory creation")
        failed = self.cli("clean", state["run"])
        self.assertEqual(failed.returncode, 2, failed.stdout + failed.stderr)
        self.assertTrue((Path(state["worktree"]) / ".local/report").exists())
        result = self.cli("clean", "--no-archive", state["run"])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertFalse(Path(state["worktree"]).exists())
        self.assertNotIn("archive:", result.stdout)

    def test_archives_prune_lru_and_age(self):
        # 6.24+: no total-bytes hard cap; normal clean keeps all archives.
        # Only when archive filesystem hits critical free-space watermark are LRU events/files trimmed,
        # while prompt/result/meta/summary/patch are always preserved.
        repo = self.repo({"a.txt": "old\n", ".gitignore": ".local/\n"})
        archives = []
        for i in range(3):
            state = self.run_tree(repo, "mkdir .local; printf 123456 > .local/proof")
            arch = Path(self.clean_outcome(self.cli("clean", "--json", state["run"]))["archive"])
            archives.append(arch)
        # Normal operation keeps every archive intact.
        for arch in archives:
            self.assertTrue((arch / "record/meta.json").is_file())
            self.assertTrue((arch / "record/events.jsonl.gz").is_file())
            self.assertTrue((arch / "files/.local/proof").is_file())

        # Refresh LRU on archives[0], make archives[1] oldest, then trigger critical FS watermark.
        (archives[1] / ".access").write_text("1")
        (archives[2] / ".access").write_text("2")
        self.cli("status", json.loads((archives[0] / "record/meta.json").read_text())["run"])
        ev_size = (archives[1] / "record/events.jsonl.gz").stat().st_size
        self.env["DELEGATE_FS_CRITICAL_FREE_BYTES"] = str(ev_size + 100)
        self.env["DELEGATE_FS_CRITICAL_FREE_PCT"] = "0"
        self.env["DELEGATE_ARCHIVE_FS_AVAIL_BYTES"] = "100"
        state = self.run_tree(repo, "mkdir .local; printf 123456 > .local/proof")
        self.cli("clean", state["run"])
        # Oldest unread (archives[1]) had its events.jsonl.gz trimmed, while archives[0] kept it;
        # core files (prompt/result/meta/summary/patch) survived on every archive.
        self.assertFalse((archives[1] / "record/events.jsonl.gz").exists())
        self.assertTrue((archives[0] / "record/events.jsonl.gz").is_file())
        for name in ("prompt.md", "result.md", "meta.json", "summary.json"):
            self.assertTrue((archives[1] / "record" / name).is_file(), f"must never delete {name}")

    def test_source_git_commands_disable_optional_locks_even_during_status(self):
        real_git = shutil.which("git")
        log = self.work / "git-env.log"
        stub = self.bin / "git"
        stub.write_text("#!/bin/sh\nprintf '%s %s\\n' \"$GIT_OPTIONAL_LOCKS\" \"$*\" >> "
                        + shlex.quote(str(log)) + "\nexec " + shlex.quote(real_git) + ' "$@"\n')
        stub.chmod(0o755)
        repo = self.repo({"a.txt": "old\n"})
        state = self.run_tree(repo, "echo changed > a.txt")
        log.write_text("")
        self.cli("status", state["run"])
        self.cli("wait", state["run"], "--no-result")
        self.cli("diff", state["run"])
        self.cli("apply", "--dry-run", state["run"])
        source_calls = [line for line in log.read_text().splitlines() if str(repo) in line]
        self.assertTrue(source_calls)
        self.assertTrue(all(line.startswith("0 ") for line in source_calls), source_calls)
        self.assertFalse((repo / ".git/index.lock").exists())

    def test_dirty_commit_warning_and_default_apply_retains_final_tree(self):
        repo = self.repo({"a.txt": "old\n"})
        state = self.run_tree(repo, "echo committed > a.txt; git add a.txt; git commit -qm implementation; "
                              "echo final >> a.txt; echo helper > helper.txt")
        self.assertEqual(state["commitsDiffer"], ["a.txt", "helper.txt"])
        status = self.cli("status", state["run"], human=True)
        self.assertIn("commits differ from the final tree: a.txt, helper.txt", status.stdout)
        refused = self.cli("apply", "--keep-commits", state["run"])
        self.assertEqual(refused.returncode, 2)
        self.assertEqual((repo / "a.txt").read_text(), "old\n")
        applied = self.cli("apply", "--json", state["run"])
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertEqual(self.outcome(applied)["commitsDiffer"], ["a.txt", "helper.txt"])
        self.assertIn("commits differ from the final tree", applied.stderr)
        self.assertEqual((repo / "a.txt").read_text(), "committed\nfinal\n")
        self.assertTrue((repo / "helper.txt").exists())

    def test_sync_agent_hands_off_markers_rejects_and_inherits_until_resolved(self):
        repo = self.repo({"a.txt": "base\n"})
        state = self.run_tree(repo, "echo colleague > a.txt")
        (repo / "a.txt").write_text("caller\n")
        default = self.cli("reply", state["run"], "--sync", "fix")
        self.assertEqual(default.returncode, 2)
        self.assertEqual((Path(state["worktree"]) / "a.txt").read_text(), "colleague\n")
        self.fake_pi([fixtures.answer("forgot to resolve"), fixtures.SETTLED])
        result = self.cli("reply", "--wait", state["run"], "--sync=agent", "--accept", "true", "fix")
        rejected = self.outcome(result)
        self.assertEqual(rejected["state"], "rejected")
        self.assertFalse(rejected["accept"]["ok"])
        self.assertEqual(rejected["syncConflictMarkers"], ["a.txt"])
        prompt = (Path(rejected["dir"]) / "prompt.md").read_text()
        self.assertTrue(prompt.startswith("Resolve these sync conflicts FIRST"), prompt)
        self.assertIn("a.txt", prompt.splitlines()[0])
        self.assertIn("<<<<<<< worktree", (Path(state["worktree"]) / "a.txt").read_text())
        self.fake_pi([fixtures.answer("only divider remains"), fixtures.SETTLED], pre="printf '=======\\n' > a.txt")
        again = self.outcome(self.cli("reply", "--wait", rejected["run"], "fix"))
        self.assertEqual(again["state"], "rejected")
        self.fake_pi([fixtures.answer("resolved"), fixtures.SETTLED], pre="printf 'resolved\\n' > a.txt")
        resolved = self.outcome(self.cli("reply", "--wait", "--over-limit", "conflict resolution", again["run"], "resolve"))
        self.assertEqual(resolved["state"], "delivered")
        self.assertNotIn("syncConflictMarkers", resolved)

    def test_apply_cleanup_archives_ignored_evidence_too(self):
        repo = self.repo({"a.txt": "old\n", ".gitignore": ".local/\n"})
        state = self.run_tree(repo, "mkdir .local; echo proof > .local/report; echo new > a.txt", ".local/report")
        result = self.cli("apply", state["run"])
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        archived = self.outcome(self.cli("status", state["run"]))
        self.assertEqual((Path(archived["archive"]) / "files/.local/report").read_text(), "proof\n")
        self.assertFalse(Path(state["worktree"]).exists())

    def test_sync_agent_add_add_delete_modify_and_multiple_conflicts(self):
        middle = "".join(f"line {i}\n" for i in range(20))
        base = "base top\n" + middle + "base end\n"
        repo = self.repo({"a.txt": base, "deleted.txt": "base\n"})
        state = self.run_tree(repo, "sed -i 's/base/agent/g' a.txt deleted.txt; echo agent > added.txt")
        (repo / "a.txt").write_text(base.replace("base", "caller"))
        (repo / "deleted.txt").unlink()
        (repo / "added.txt").write_text("caller\n")
        self.fake_pi([fixtures.answer("resolve later"), fixtures.SETTLED])
        result = self.cli("reply", "--wait", "--sync=agent", state["run"], "resolve")
        rejected = self.outcome(result)
        self.assertEqual(rejected["state"], "rejected")
        self.assertEqual(rejected["syncConflictMarkers"], ["a.txt", "added.txt", "deleted.txt"])
        self.assertEqual((Path(state["worktree"]) / "a.txt").read_text().count("<<<<<<<"), 2)

    def test_sync_agent_binary_conflict_still_refuses_atomically(self):
        repo = self.repo({"a.txt": "base\n", "binary": "base\n"})
        state = self.run_tree(repo, "echo agent > a.txt; printf 'agent\\0bytes' > binary")
        (repo / "a.txt").write_text("caller\n")
        (repo / "binary").write_bytes(b"caller\0bytes")
        (repo / "new.txt").write_text("caller-only\n")
        result = self.cli("reply", "--sync=agent", state["run"], "resolve")
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("binary", result.stderr)
        self.assertEqual((Path(state["worktree"]) / "a.txt").read_text(), "agent\n")
        self.assertFalse((Path(state["worktree"]) / "new.txt").exists())

    def test_archive_excludes_reconstructible_dirs_even_when_dir_mentioned(self):
        repo = self.repo({
            "a.txt": "old\n",
            ".gitignore": ".venv/\ntarget/\nnode_modules/\ndist/\n__pycache__/\n.pytest_cache/\n",
        })
        pre = (
            "mkdir -p .venv/lib/site-packages backend/.venv/lib target/debug node_modules/pkg "
            "dist __pycache__ .pytest_cache/v/cache backend/notes; "
            "echo cfg > .venv/pyvenv.cfg; "
            "echo huge > .venv/lib/site-packages/lib.so; "
            "echo huge2 > backend/.venv/lib/pkg.py; "
            "echo explicit > target/mentioned; "
            "echo bin > target/debug/app; "
            "echo dep > node_modules/pkg/index.js; "
            "echo bundle > dist/bundle.js; "
            "echo pyc > __pycache__/a.pyc; "
            "echo cache > .pytest_cache/v/cache/lastfailed; "
            "echo note > backend/notes/summary.txt"
        )
        answer = (
            "Created `.venv/`, ran `backend/`, rebuilt `target/`, `node_modules/`, `dist/`, "
            "`__pycache__/`, `.pytest_cache/`, and verified `.venv/pyvenv.cfg` and `target/mentioned`."
        )
        state = self.run_tree(repo, pre, answer)
        cleaned = self.clean_outcome(self.cli("clean", "--json", state["run"]))
        archive = Path(cleaned["archive"])
        archived_files = sorted(
            str(p.relative_to(archive / "files"))
            for p in (archive / "files").rglob("*")
            if p.is_file()
        )
        # Only explicitly named individual files inside reconstructible dirs are kept,
        # plus non-reconstructible files under mentioned normal dirs (`backend/notes/summary.txt`).
        self.assertEqual(
            archived_files,
            [".venv/pyvenv.cfg", "backend/notes/summary.txt", "target/mentioned"],
        )

    def test_clean_rescan_archive_recomputes_manifests_and_usage_and_default_warn_is_30gib(self):
        repo = self.repo({"a.txt": "old\n", ".gitignore": ".local/\n"})
        st1 = self.run_tree(repo, "mkdir -p .local/big; printf '1234567890' > .local/keep; printf 'aaaaaaaaaaaaaaaaaaaa' > .local/big/drop", ".local/keep")
        arch1 = Path(self.clean_outcome(self.cli("clean", "--json", st1["run"]))["archive"])
        st2 = self.run_tree(repo, "mkdir -p .local; printf 'bbbbbbbbbb' > .local/two", ".local/two")
        arch2 = Path(self.clean_outcome(self.cli("clean", "--json", st2["run"]))["archive"])

        # Default archive warning threshold is 30 GiB when env overrides are absent.
        self.env.pop("DELEGATE_ARCHIVE_WARN_GIB", None)
        self.env.pop("DELEGATE_ARCHIVE_WARN_BYTES", None)
        st_json = self.cli("status")
        summary = [json.loads(l) for l in st_json.stdout.splitlines() if "statusSummary" in l][-1]["statusSummary"]
        self.assertEqual(summary["archiveWarnBytes"], 30 * 1024 * 1024 * 1024)
        self.assertEqual(summary["archiveCount"], 2)

        usage_path = self.work / "cache/delegate/archive/.usage.json"
        before_usage = json.loads(usage_path.read_text())
        self.assertEqual(before_usage["count"], 2)

        # Manually delete files inside arch1 and delete arch2 completely, leaving manifests & .usage.json stale.
        shutil.rmtree(arch1 / "files/.local/big")
        shutil.rmtree(arch2)
        stale_manifest = json.loads((arch1 / "manifest.json").read_text())
        self.assertIn(".local/big/drop", stale_manifest["files"])
        self.assertEqual(stale_manifest["bytes"], 30)

        # Rescan recomputes manifest.json, record/archive.json, and .usage.json.
        res = self.cli("clean", "--rescan-archive", "--json")
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        payload = json.loads(res.stdout.splitlines()[0])["archiveRescan"]
        self.assertEqual(payload["count"], 1)
        self.assertEqual(payload["updatedManifests"], 1)
        self.assertEqual(payload["previousCount"], 2)
        self.assertEqual(payload["previousBytes"], before_usage["bytes"])
        self.assertLess(payload["bytes"], before_usage["bytes"])

        fresh_manifest = json.loads((arch1 / "manifest.json").read_text())
        self.assertEqual(fresh_manifest["files"], [".local/keep"])
        self.assertEqual(fresh_manifest["bytes"], 10)
        rec_manifest = json.loads((arch1 / "record/archive.json").read_text())
        self.assertEqual(rec_manifest["files"], [".local/keep"])
        self.assertEqual(rec_manifest["bytes"], 10)

        after_usage = json.loads(usage_path.read_text())
        self.assertEqual(after_usage["count"], 1)
        self.assertEqual(after_usage["bytes"], fresh_manifest["totalBytes"])

        # Second rescan is idempotent (0 manifests rewritten).
        res2 = self.cli("clean", "--rescan-archive", "--json")
        self.assertEqual(res2.returncode, 0, res2.stdout + res2.stderr)
        payload2 = json.loads(res2.stdout.splitlines()[0])["archiveRescan"]
        self.assertEqual(payload2["updatedManifests"], 0)
        self.assertEqual(payload2["bytes"], after_usage["bytes"])

        # Human output works too.
        res_human = self.cli("clean", "--rescan-archive", human=True)
        self.assertEqual(res_human.returncode, 0, res_human.stdout + res_human.stderr)
        self.assertIn("archive rescan: 1 份", res_human.stdout)
