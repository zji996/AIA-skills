"""6.24.0 regressions:
- concise one-line `status`, detailed `status <run>`, `--json` omitting `files` unless `--files`
- net diff relative to merge-base (`netBase`) after rebase/merge in worktree (no false protectViolation)
- `delegate handoff` Markdown summary with netBase, TODO(master), migrations, and overlaps
"""
import json
import shlex
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate624Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def git(self, repo, *args):
        return subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()

    def test_status_default_one_line_and_json_files_flag(self):
        repo = self.repo({
            "apps/web/App.tsx": "old\n",
            "base.txt": "v0\n",
            ".delegate.json": json.dumps({"defaults": {"protect": ["apps/"]}}),
        })
        pre = (
            "echo c > base.txt; git add base.txt; git commit -qm step1; "
            "for i in $(seq 1 12); do echo x > extra_${i}.txt; done"
        )
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre=pre)
        started = self.cli(
            "run", "--worktree", "--workdir", repo, "--name", "slim",
            "请参考 apps/web/App.tsx 实现新功能",
        )
        run = self.outcome(started)
        run_id = run["run"]

        # Create source workspace drift after run started? Simulate recorded sourceWorkspaceChanges
        # by adding a dirty file before a second run or checking recorded fields directly.
        summary_path = Path(run["dir"]) / "summary.json"
        summary = json.loads(summary_path.read_text())
        summary["sourceWorkspaceChanges"] = ["src/dirty_one.rs", "src/dirty_two.rs"]
        summary_path.write_text(json.dumps(summary))

        # 1) `delegate status` (list mode): counts header + strictly one line for this run, counts only, no path lists.
        list_out = self.cli("status", human=True)
        self.assertEqual(list_out.returncode, 0, list_out.stderr)
        lines = [ln for ln in list_out.stdout.splitlines() if ln.strip()]
        self.assertIn("运行中 0 · 待合入 1", lines[0])
        run_lines = [ln for ln in lines[1:] if not ln.startswith("另有 ") and not ln.startswith("归档占用 ")]
        self.assertEqual(len(run_lines), 1, list_out.stdout)
        line = run_lines[0]
        self.assertIn("commits differ 12 个文件", line)
        self.assertIn("正文提及保护路径 1 处", line)
        self.assertIn("源仓库工作区有新改动：2 个文件", line)
        for hidden in ("extra_1.txt", "extra_12.txt", "apps/web/App.tsx", "dirty_one.rs", "dirty_two.rs"):
            self.assertNotIn(hidden, line, f"list line should not inline {hidden}: {line}")

        # 2) `delegate status <run>`: one-line summary + indented path details.
        detail_out = self.cli("status", run_id, human=True)
        self.assertEqual(detail_out.returncode, 0, detail_out.stderr)
        self.assertIn("commits differ from the final tree:", detail_out.stdout)
        self.assertIn("extra_1.txt", detail_out.stdout)
        self.assertIn("正文提及保护路径：apps/web/App.tsx", detail_out.stdout)
        self.assertIn("--allow apps/web/App.tsx", detail_out.stdout)
        self.assertIn("源仓库工作区有新改动：2 个文件（src/dirty_one.rs, src/dirty_two.rs）", detail_out.stdout)

        # 3) `delegate status --json` omits `files` by default; `--files` includes it.
        default_json = self.outcome(self.cli("status", run_id))
        self.assertNotIn("files", default_json)
        self.assertEqual(default_json["changes"]["files"], 13)

        with_files = self.outcome(self.cli("status", run_id, "--files"))
        self.assertIn("files", with_files)
        self.assertEqual(len(with_files["files"]), 13)
        self.assertIn("extra_1.txt", with_files["files"])

    def test_rebased_worktree_net_diff_avoids_false_protect_violation(self):
        repo = self.repo({
            "src/app.txt": "v0\n",
            "docs/decision/0001.md": "decision v0\n",
            ".delegate.json": json.dumps({"defaults": {"protect": ["docs/decision/"]}}),
        })
        gate = self.work / "rebase-gate"
        branch = self.git(repo, "rev-parse", "--abbrev-ref", "HEAD")
        # Wait for main to advance (modifying protected docs/decision/0001.md), then commit own change and rebase.
        pre = (
            f"while [ ! -f {shlex.quote(str(gate))} ]; do sleep 0.05; done; "
            "echo feat > src/feature.txt; git add src/feature.txt; git commit -qm 'add feature'; "
            f"git rebase {shlex.quote(branch)}"
        )
        self.fake_pi([fixtures.answer("rebased and done"), fixtures.SETTLED], pre=pre)
        started = self.outcome(
            self.cli("start", "--worktree", "--workdir", repo, "--name", "rebased", "--accept", "true", "add feature")
        )
        run_id = started["run"]
        wt = Path(started["worktree"])
        self.until(lambda: wt.is_dir(), "worktree created")

        # Main advances with a change to a PROTECTED path and another unrelated file.
        (repo / "docs/decision/0001.md").write_text("decision v1 from main\n")
        (repo / "src/main_only.txt").write_text("main change\n")
        self.git(repo, "add", "docs/decision/0001.md", "src/main_only.txt")
        self.git(repo, "commit", "-qm", "main updates protected doc")
        new_main_sha = self.git(repo, "rev-parse", "HEAD")

        gate.touch()
        waited = self.cli("wait", run_id)
        self.assertEqual(waited.returncode, 0, waited.stdout + waited.stderr)
        done = self.outcome(waited)

        # Must NOT report protectViolation; net diff contains ONLY colleague's own file.
        self.assertEqual(done["state"], "delivered", done)
        self.assertNotIn("protectViolation", done)
        self.assertEqual(done["netBase"], new_main_sha)
        self.assertEqual(done["changes"]["files"], 1)
        self.assertEqual(done["pendingChanges"]["files"], 1)
        self.assertEqual(done["files"], ["src/feature.txt"])

        meta = json.loads((Path(done["dir"]) / "meta.json").read_text())
        self.assertEqual(meta["netBase"], new_main_sha)

        patch_text = (Path(done["dir"]) / "changes.patch").read_text()
        self.assertIn("src/feature.txt", patch_text)
        self.assertNotIn("docs/decision/0001.md", patch_text)
        self.assertNotIn("src/main_only.txt", patch_text)

        # diff and diff --total use netBase (only src/feature.txt).
        for diff_args in (("diff", run_id), ("diff", "--total", run_id)):
            diff_res = self.cli(*diff_args)
            self.assertEqual(diff_res.returncode, 0, diff_res.stderr)
            self.assertIn("src/feature.txt", diff_res.stdout)
            self.assertNotIn("docs/decision/0001.md", diff_res.stdout)
            self.assertNotIn("src/main_only.txt", diff_res.stdout)

        # Main advances a protected doc AGAIN before apply; apply still lands cleanly using netBase.
        (repo / "docs/decision/0001.md").write_text("decision v2 from main\n")
        self.git(repo, "add", "docs/decision/0001.md")
        self.git(repo, "commit", "-qm", "main updates protected doc again")

        applied = self.cli("apply", "--json", run_id)
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertEqual((repo / "src/feature.txt").read_text(), "feat\n")
        self.assertEqual((repo / "docs/decision/0001.md").read_text(), "decision v2 from main\n")

    def test_merged_worktree_uses_merge_base_and_still_catches_real_protect_violation(self):
        repo = self.repo({
            "src/app.txt": "v0\n",
            "docs/decision/0001.md": "decision v0\n",
            ".delegate.json": json.dumps({"defaults": {"protect": ["docs/decision/"]}}),
        })
        branch = self.git(repo, "rev-parse", "--abbrev-ref", "HEAD")

        # Merge case: colleague merges main (which touched protected path) and edits only own file -> allowed.
        gate1 = self.work / "merge-gate"
        pre_merge = (
            f"while [ ! -f {shlex.quote(str(gate1))} ]; do sleep 0.05; done; "
            "echo merged-feat > src/merged.txt; git add src/merged.txt; git commit -qm 'feat'; "
            f"git merge --no-edit {shlex.quote(branch)}"
        )
        self.fake_pi([fixtures.answer("merged"), fixtures.SETTLED], pre=pre_merge)
        started = self.outcome(self.cli("start", "--worktree", "--workdir", repo, "--name", "merged", "m"))
        self.until(lambda: Path(started["worktree"]).is_dir(), "wt")
        (repo / "docs/decision/0001.md").write_text("decision v1\n")
        self.git(repo, "add", "docs/decision/0001.md")
        self.git(repo, "commit", "-qm", "main doc")
        main_sha = self.git(repo, "rev-parse", "HEAD")
        gate1.touch()
        done = self.outcome(self.cli("wait", started["run"]))
        self.assertEqual(done["state"], "answered", done)
        self.assertNotIn("protectViolation", done)
        self.assertEqual(done["netBase"], main_sha)
        self.assertEqual(done["files"], ["src/merged.txt"])

        # Violation case: colleague rebases onto main AND modifies the protected file itself -> still rejected!
        gate2 = self.work / "viol-gate"
        pre_viol = (
            f"while [ ! -f {shlex.quote(str(gate2))} ]; do sleep 0.05; done; "
            f"git rebase {shlex.quote(branch)}; "
            "echo rogue >> docs/decision/0001.md"
        )
        self.fake_pi([fixtures.answer("oops"), fixtures.SETTLED], pre=pre_viol)
        started2 = self.outcome(self.cli("start", "--worktree", "--workdir", repo, "--name", "viol", "v"))
        self.until(lambda: Path(started2["worktree"]).is_dir(), "wt2")
        (repo / "docs/decision/0001.md").write_text("decision v2\n")
        self.git(repo, "add", "docs/decision/0001.md")
        self.git(repo, "commit", "-qm", "main doc 2")
        gate2.touch()
        done2 = self.outcome(self.cli("wait", started2["run"]))
        self.assertEqual(done2["state"], "rejected", done2)
        self.assertEqual(done2["protectViolation"], ["docs/decision/0001.md"])

    def test_handoff_writes_markdown_with_netbase_todos_migrations_and_overlaps(self):
        repo = self.repo({
            "src/shared.rs": "// TODO(master): existing context\nfn keep() {}\n// TODO(master): to delete\n",
            "src/other.rs": "fn other() {}\n",
        })
        base_sha = self.git(repo, "rev-parse", "--short=7", "HEAD")

        # Run A (unmerged): edits src/shared.rs (removes 1 old TODO, adds 2 new TODO(master)) + migration
        pre_a = (
            "printf '// TODO(master): existing context\\nfn keep() {}\\n"
            "// TODO(master): wire auth\\n// TODO(master): check rate limit\\n' > src/shared.rs; "
            "mkdir -p migrations; printf 'CREATE TABLE users (id INT);\\n' > migrations/0149_add_users.sql"
        )
        self.fake_pi([fixtures.answer("run a done"), fixtures.SETTLED], pre=pre_a)
        run_a = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "run-a", "--accept", "true", "task a")
        )

        # Advance main before Run B starts so Run B has a distinct netBase short sha.
        (repo / "src/other.rs").write_text("fn other() { /* v2 */ }\n")
        self.git(repo, "add", "src/other.rs")
        self.git(repo, "commit", "-qm", "advance main")
        sha_b = self.git(repo, "rev-parse", "--short=7", "HEAD")

        # Run B (unmerged): also edits src/shared.rs (overlap!) + nested migration
        pre_b = (
            "printf '// TODO(master): existing context\\nfn keep() { /* b */ }\\n// TODO(master): to delete\\n' > src/shared.rs; "
            "mkdir -p apps/api/migrations; printf 'CREATE TABLE roles (id INT);\\n' > apps/api/migrations/0150_add_roles.sql"
        )
        self.fake_pi([fixtures.answer("run b done"), fixtures.SETTLED], pre=pre_b)
        run_b = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "run-b", "--accept", "true", "task b")
        )

        # Run C (applied/merged): must be excluded from HANDOFF.md
        self.fake_pi([fixtures.answer("run c done"), fixtures.SETTLED], pre="echo c > src/done.rs")
        run_c = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "run-c", "--accept", "true", "task c")
        )
        self.assertEqual(self.cli("apply", "--json", run_c["run"]).returncode, 0)

        res = self.cli("handoff", cwd=repo, human=True)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)

        handoff_file = Path(self.env["DELEGATE_RUNS"]) / "HANDOFF.md"
        self.assertTrue(handoff_file.is_file(), res.stdout)
        self.assertIn(str(handoff_file), res.stdout)

        md = handoff_file.read_text()
        self.assertIn("run-a", md)
        self.assertIn("run-b", md)
        self.assertNotIn("run-c", md)
        self.assertIn(f"`{base_sha}`", md)
        self.assertIn(f"`{sha_b}`", md)
        self.assertIn("0149_add_users.sql", md)
        self.assertIn("0150_add_roles.sql", md)
        self.assertIn(f"delegate apply {run_a['run']}", md)
        self.assertIn(f"delegate apply {run_b['run']}", md)

        # Verify exact TODO(master) counts in table rows (run-a has 2 added, run-b has 0 added).
        row_a = next(line for line in md.splitlines() if line.startswith("| run-a |"))
        row_b = next(line for line in md.splitlines() if line.startswith("| run-b |"))
        self.assertEqual(row_a.split("|")[5].strip(), "2", row_a)
        self.assertEqual(row_b.split("|")[5].strip(), "0", row_b)

        # Overlap section lists src/shared.rs touched by both run-a and run-b.
        self.assertIn("## 重叠修改文件", md)
        overlap_section = md.split("## 重叠修改文件", 1)[1]
        self.assertIn("`src/shared.rs`: run-a, run-b", overlap_section)

    def test_reply_rebase_onto_new_main_updates_net_base_and_avoids_protect_violation(self):
        repo = self.repo({
            "src/app.txt": "v0\n",
            "docs/decision/0001.md": "decision v0\n",
            ".delegate.json": json.dumps({"defaults": {"protect": ["docs/decision/"]}}),
        })
        branch = self.git(repo, "rev-parse", "--abbrev-ref", "HEAD")

        # Round 1: colleague commits own change on initial base.
        self.fake_pi(
            [fixtures.answer("round 1"), fixtures.SETTLED],
            pre="echo r1 > src/mine.txt; git add src/mine.txt; git commit -qm 'r1'",
        )
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "reply-rebase", "--accept", "true", "first")
        )
        self.assertEqual(r1["state"], "delivered")

        # Main moves forward, modifying a protected path.
        (repo / "docs/decision/0001.md").write_text("decision v1 from main\n")
        self.git(repo, "add", "docs/decision/0001.md")
        self.git(repo, "commit", "-qm", "main protected update")
        new_main_sha = self.git(repo, "rev-parse", "HEAD")

        # Round 2 (reply): colleague rebases onto main and adds a second commit.
        self.fake_pi(
            [fixtures.answer("round 2"), fixtures.SETTLED],
            pre=f"git rebase {shlex.quote(branch)} && echo r2 >> src/mine.txt && git add src/mine.txt && git commit -qm 'r2'",
        )
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "rebase onto main and refine"))
        self.assertEqual(r2["state"], "delivered", r2)
        self.assertNotIn("protectViolation", r2)
        self.assertEqual(r2["netBase"], new_main_sha)
        self.assertEqual(r2["changes"]["files"], 1)
        self.assertEqual(r2["pendingChanges"]["files"], 1)
        self.assertEqual(r2["files"], ["src/mine.txt"])

        total_diff = self.cli("diff", "--total", r2["run"])
        self.assertEqual(total_diff.returncode, 0, total_diff.stderr)
        self.assertIn("src/mine.txt", total_diff.stdout)
        self.assertNotIn("docs/decision/0001.md", total_diff.stdout)

        applied = self.cli("apply", "--json", r2["run"])
        self.assertEqual(applied.returncode, 0, applied.stdout + applied.stderr)
        self.assertEqual((repo / "src/mine.txt").read_text(), "r1\nr2\n")
        self.assertEqual((repo / "docs/decision/0001.md").read_text(), "decision v1 from main\n")


if __name__ == "__main__":
    unittest.main()
