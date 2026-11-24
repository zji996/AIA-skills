"""End-to-end behavioral regressions for the 2026-11-25 delegate friction fixes (items 1-6 + bonus):
1. Mainline-attributed acceptance failure -> `blocked_mainline` ("主干阻塞", not `rejected`/rework),
   records blocking files & blobs, and auto-syncs + re-runs ONLY failed layers (zero LLM calls)
   once source HEAD changes those blocking files (still failing -> `rejected`); emits
   `accept_blocked_mainline` / `accept_rerun`.
   Bonus: rework round reused layers render as `沿用第 N 轮通过` (not `跳过`).
2. `premerge_conflict` deduplication by `(run, conflicts, conflictSourceBlobs)` — unrelated source
   HEAD advances do not re-premerge or emit duplicate events/unread Stop hook blocks; changes to
   conflicted files on mainline re-premerge and report.
3. `delegate mark-applied <run> [--commit <sha>] [--check-files]` marks manually committed runs as
   applied; Stop hook auto-treats a run as applied when its net changes appear verbatim in source HEAD.
4. `delegate work defer <run>` on a multi-run work item only defers that run's unmerged reminder
   without changing the work item state; only `delegate work defer Wxx` defers the work item.
5. `numbers_taken` / occupied migration numbers only count net-new migrations relative to `netBase`
   and exclude migrations already on mainline or synced from mainline.
6. `delegate work close <Wxx> "<evidence>"` accepts a trailing positional argument as `--evidence`.
"""

import json
import os
import subprocess
import sys
import unittest
from pathlib import Path

import test_delegate as fixtures

ROOT = Path(__file__).resolve().parent.parent
STOP_HOOK = ROOT / "skills/delegate/hooks/claude-code-stop.py"


class DelegateFriction1125Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until
    start_gated = fixtures.DelegateTests.start_gated

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def git(self, repo: Path, *args: str) -> str:
        res = subprocess.run(
            ["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t", *args],
            capture_output=True,
            text=True,
            check=True,
        )
        return res.stdout.strip()

    def make_repo(self, name: str, files: dict[str, str]) -> Path:
        repo = self.work / name
        repo.mkdir(parents=True, exist_ok=True)
        self.git(repo, "init", "-q")
        for rel, text in files.items():
            p = repo / rel
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text, encoding="utf-8")
        self.git(repo, "add", "-A")
        self.git(repo, "commit", "-qm", "init")
        return repo

    def read_events(self) -> list[dict]:
        p = self.state_dir() / "events.jsonl"
        if not p.is_file():
            return []
        return [json.loads(ln) for ln in p.read_text(encoding="utf-8").splitlines() if ln.strip()]

    def run_stop_hook(self, repo: Path, session_id: str | None = None) -> dict | None:
        env = {
            **self.env,
            "DELEGATE_BIN": str(fixtures.DELEGATE),
            "DELEGATE_STOP_HOOK_CURRENT_ONLY": "1",
        }
        if session_id:
            env["CLAUDE_CODE_SESSION_ID"] = session_id
        res = subprocess.run(
            [sys.executable, str(STOP_HOOK)],
            input=json.dumps({"cwd": str(repo), "stop_hook_active": False}),
            env=env,
            capture_output=True,
            text=True,
            timeout=10,
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        out = res.stdout.strip()
        return json.loads(out) if out else None

    def consume_events(self, repo: Path) -> None:
        self.cli("events", "--since", "0", cwd=repo, human=True)

    def install_scripted_pi(self, script_body: str) -> Path:
        log_file = self.work / "scripted-pi-1125.jsonl"
        if log_file.exists():
            log_file.unlink()
        self.env["SCRIPTED_PI_LOG"] = str(log_file)
        pi = self.bin / "pi"
        pi.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "args = sys.argv[1:]\n"
            "if args == ['--version']:\n"
            "    print('fake-pi 1.0')\n"
            "    sys.exit(0)\n"
            "if len(args) >= 2 and args[1] == '--list-models':\n"
            "    print('provider model context max-out thinking images')\n"
            "    print('p m 1M 1K yes yes')\n"
            "    print('p review-m 1M 1K yes yes')\n"
            "    print('p write-m 1M 1K yes yes')\n"
            "    sys.exit(0)\n"
            "stdin_text = sys.stdin.read()\n"
            "sdir = ''\n"
            "sid = ''\n"
            "for i, a in enumerate(args):\n"
            "    if a == '--session-dir' and i + 1 < len(args): sdir = args[i + 1]\n"
            "    elif a == '--session-id' and i + 1 < len(args): sid = args[i + 1]\n"
            "if sdir:\n"
            "    Path(sdir).mkdir(parents=True, exist_ok=True)\n"
            "    (Path(sdir) / f't_{sid or os.getpid()}.jsonl').write_text('{}\\n')\n"
            "log_path = Path(os.environ['SCRIPTED_PI_LOG'])\n"
            "prev = log_path.read_text().splitlines() if log_path.is_file() else []\n"
            "call_idx = len(prev) + 1\n"
            "with log_path.open('a') as fh:\n"
            "    fh.write(json.dumps({'call': call_idx, 'args': args, 'stdin': stdin_text}, ensure_ascii=False) + '\\n')\n"
            "def emit_answer(text):\n"
            "    print(json.dumps({'type': 'message_end', 'message': {'role': 'assistant', 'stopReason': 'stop', 'model': 'fake-model', 'usage': {'input': 10, 'output': 5, 'cacheRead': 0}, 'content': [{'type': 'text', 'text': text}]}}))\n"
            "    print(json.dumps({'type': 'agent_settled'}))\n"
            + script_body
            + "\n",
            encoding="utf-8",
        )
        pi.chmod(0o755)
        return log_file

    # ------------------------------------------------------------------
    # Item 1: Mainline-blocked acceptance -> blocked_mainline + auto sync & rerun failed layer only
    # ------------------------------------------------------------------
    def test_1_mainline_blocked_auto_sync_and_rerun_failed_layer_only(self):
        fast_log = self.work / "fast-web.runs"
        audit_log = self.work / "size-audit.runs"
        repo = self.make_repo(
            "repo-mainline-block",
            {
                "apps/ui.ts": "export const ui = 0;\n",
                "crates/api/handlers.rs": "TOO_BIG\n",
                "README.md": "v0\n",
                ".delegate.json": json.dumps(
                    {
                        "acceptRules": [
                            {
                                "name": "fast-web",
                                "tier": "fast",
                                "paths": ["apps/**"],
                                "command": f"echo hit >> {fast_log}",
                            },
                            {
                                "name": "size-audit",
                                "tier": "slow",
                                "paths": ["apps/**", "crates/**"],
                                "command": (
                                    f"echo hit >> {audit_log}; "
                                    "if grep -q '^OK$' crates/api/handlers.rs; then exit 0; "
                                    "else echo 'FAIL: size-audit crates/api/handlers.rs:1004 exceeds 1000 lines'; exit 1; fi"
                                ),
                            },
                        ]
                    }
                ),
            },
        )
        pi_log = self.install_scripted_pi(
            "Path('apps/ui.ts').write_text('export const ui = 1;\\n')\n"
            "emit_answer('updated ui')\n"
        )

        # 1) Initial run touches ONLY apps/ui.ts; fails size-audit on mainline handlers.rs -> blocked_mainline.
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "free-gesture-1", "update ui")
        )
        run_id = r1["run"]
        self.assertEqual(r1["state"], "blocked_mainline", r1)
        self.assertEqual(len(pi_log.read_text().splitlines()), 1)
        self.assertEqual(len(fast_log.read_text().splitlines()), 1)
        self.assertEqual(len(audit_log.read_text().splitlines()), 1)

        mb = r1.get("mainlineBlock") or {}
        self.assertEqual(mb.get("blockingFiles"), ["crates/api/handlers.rs"])
        self.assertEqual(mb.get("failedLayers"), ["size-audit"])

        # Human status says "主干阻塞" (not "未通过").
        st_human = self.cli("status", run_id, cwd=repo, human=True).stdout
        self.assertIn("主干阻塞", st_human)
        self.assertNotIn("未通过", st_human)

        # Event stream emitted `accept_blocked_mainline` (not `rejected`).
        evs = [e for e in self.read_events() if e.get("run") == run_id]
        kinds = [e["kind"] for e in evs]
        self.assertIn("accept_blocked_mainline", kinds)
        self.assertNotIn("rejected", kinds)

        # Stop hook does NOT block on `blocked_mainline`.
        self.assertIsNone(self.run_stop_hook(repo))

        # 2) Unrelated mainline commit (README.md) does NOT trigger rerun.
        (repo / "README.md").write_text("v1\n", encoding="utf-8")
        self.git(repo, "add", "README.md")
        self.git(repo, "commit", "-qm", "docs: unrelated")
        unrelated_head = self.git(repo, "rev-parse", "HEAD")
        self.cli("status", run_id, cwd=repo)
        self.until(
            lambda: (
                json.loads(self.cli("status", run_id, cwd=repo).stdout)
                .get("mainlineBlock", {})
                .get("checkedHead")
                == unrelated_head
            ),
            "worker checks unrelated head without rerunning",
            timeout=10,
        )
        self.assertEqual(len(audit_log.read_text().splitlines()), 1)
        self.assertEqual(json.loads(self.cli("status", run_id, cwd=repo).stdout)["state"], "blocked_mainline")

        # 3) Mainline fixes handlers.rs -> auto-sync & re-run ONLY failed layer `size-audit` (zero LLM calls).
        (repo / "crates/api/handlers.rs").write_text("OK\n", encoding="utf-8")
        self.git(repo, "add", "crates/api/handlers.rs")
        self.git(repo, "commit", "-qm", "fix: split handlers.rs under limit")
        self.cli("status", run_id, cwd=repo)
        self.until(
            lambda: json.loads(self.cli("status", run_id, cwd=repo).stdout).get("state") == "delivered",
            "blocked_mainline run auto-reruns and transitions to delivered",
            timeout=15,
        )

        # Verify zero extra model calls and ONLY `size-audit` re-ran (fast-web stayed at 1).
        self.assertEqual(len(pi_log.read_text().splitlines()), 1, "auto-rerun must make zero LLM calls")
        self.assertEqual(len(fast_log.read_text().splitlines()), 1, "already-passed fast layer must not re-run")
        self.assertEqual(len(audit_log.read_text().splitlines()), 2, "failed layer must re-run once")

        evs_after = [e for e in self.read_events() if e.get("run") == run_id]
        rerun_evs = [e for e in evs_after if e["kind"] == "accept_rerun"]
        self.assertEqual(len(rerun_evs), 1, evs_after)
        self.assertTrue(rerun_evs[0]["payload"]["ok"])
        self.assertEqual(rerun_evs[0]["payload"]["ranLayers"], ["size-audit"])
        self.assertIn("delivered", [e["kind"] for e in evs_after])

    def test_1b_mainline_blocked_still_fails_after_rerun_hands_to_controller(self):
        repo = self.make_repo(
            "repo-mainline-still-fail",
            {
                "apps/ui.ts": "export const ui = 0;\n",
                "crates/api/handlers.rs": "TOO_BIG_V1\n",
            },
        )
        self.fake_pi([fixtures.answer("ui edit"), fixtures.SETTLED], pre="echo 'export const ui = 1;' > apps/ui.ts")
        r1 = self.outcome(
            self.cli(
                "run",
                "--worktree",
                "--workdir",
                repo,
                "--name",
                "still-fail",
                "--accept",
                "if grep -q '^OK$' crates/api/handlers.rs; then exit 0; else echo 'FAIL: size-audit crates/api/handlers.rs:1004'; exit 1; fi",
                "edit ui",
            )
        )
        run_id = r1["run"]
        self.assertEqual(r1["state"], "blocked_mainline", r1)

        # Blocking file changes on mainline, but STILL fails acceptance -> becomes `rejected` for controller.
        (repo / "crates/api/handlers.rs").write_text("TOO_BIG_V2\n", encoding="utf-8")
        self.git(repo, "add", "crates/api/handlers.rs")
        self.git(repo, "commit", "-qm", "wip: handlers still too big")
        self.cli("status", run_id, cwd=repo)
        self.until(
            lambda: json.loads(self.cli("status", run_id, cwd=repo).stdout).get("state") == "rejected",
            "still-failing rerun transitions to rejected",
            timeout=15,
        )
        evs = [e for e in self.read_events() if e.get("run") == run_id]
        rerun_evs = [e for e in evs if e["kind"] == "accept_rerun"]
        self.assertEqual(len(rerun_evs), 1, evs)
        self.assertFalse(rerun_evs[0]["payload"]["ok"])
        self.assertIn("rejected", [e["kind"] for e in evs])

    # ------------------------------------------------------------------
    # Bonus observation: Rework round reused layers display "沿用第 N 轮通过" (not "跳过")
    # ------------------------------------------------------------------
    def test_1c_rework_card_shows_reused_from_round_label(self):
        repo = self.make_repo(
            "repo-rework-reuse",
            {
                "apps/web/a.ts": "export const a = 0;\n",
                "crates/core/b.rs": "pub fn b() -> i32 { 0 }\n",
                ".delegate.json": json.dumps(
                    {
                        "review": {"tier": "review"},
                        "acceptRules": [
                            {"name": "web", "tier": "fast", "paths": ["apps/**"], "command": "true"},
                            {"name": "rust", "tier": "fast", "paths": ["crates/**"], "command": "true"},
                        ],
                    }
                ),
            },
        )
        self.install_scripted_pi(
            "if call_idx == 1:\n"
            "    Path('apps/web/a.ts').write_text('export const a = 1;\\n')\n"
            "    Path('crates/core/b.rs').write_text('pub fn b() -> i32 { 1 }\\n')\n"
            "    emit_answer('round 1 done')\n"
            "elif call_idx == 2:\n"
            "    emit_answer('REWORK\\nPlease change b() to return 2.\\n\\n## 主控重点看\\n无')\n"
            "elif call_idx == 3:\n"
            "    Path('crates/core/b.rs').write_text('pub fn b() -> i32 { 2 }\\n')\n"
            "    emit_answer('round 2 fixed b.rs only')\n"
            "else:\n"
            "    emit_answer('PASS\\nLooks good.\\n\\n## 主控重点看\\n无')\n"
        )
        r = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "rework-reuse", "edit web and rust")
        )
        self.assertEqual(r["state"], "delivered", r)
        card_human = self.cli("card", r["run"], cwd=repo, human=True).stdout
        self.assertIn("[沿用第 1 轮通过] web", card_human)
        self.assertNotIn("[跳过] web", card_human)

    # ------------------------------------------------------------------
    # Item 2: Premerge conflict dedup by (run, conflict files, source blobs)
    # ------------------------------------------------------------------
    def test_2_premerge_conflict_dedup_until_source_blob_changes(self):
        self.env.pop("DELEGATE_PREMERGE", None)
        repo = self.make_repo(
            "repo-pm-dedup",
            {
                "shared.txt": "line1\nline2\nline3\n",
                "other.txt": "v0\n",
            },
        )
        self.fake_pi(
            [fixtures.answer("edited shared"), fixtures.SETTLED],
            pre="printf 'from-run\\nline2\\nline3\\n' > shared.txt",
        )
        r = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "free-gesture-2", "--accept", "true", "edit shared")
        )
        run_id = r["run"]
        self.until(
            lambda: json.loads(self.cli("status", run_id, cwd=repo).stdout).get("premerge", {}).get("status") == "ready",
            "initial premerge ready",
            timeout=15,
        )

        # Introduce conflicting edit to shared.txt on mainline -> triggers 1st premerge_conflict.
        (repo / "shared.txt").write_text("from-main-v1\nline2\nline3\n", encoding="utf-8")
        self.git(repo, "add", "shared.txt")
        self.git(repo, "commit", "-qm", "mainline conflict v1")
        h_conf1 = self.git(repo, "rev-parse", "HEAD")
        self.cli("status", run_id, cwd=repo)
        self.until(
            lambda: (
                (pm := json.loads(self.cli("status", run_id, cwd=repo).stdout).get("premerge", {}))
                and pm.get("status") == "conflict"
                and pm.get("baseHead") == h_conf1
            ),
            "first premerge conflict recorded",
            timeout=15,
        )
        conf_evs = [e for e in self.read_events() if e.get("run") == run_id and e.get("kind") == "premerge_conflict"]
        self.assertEqual(len(conf_evs), 1, conf_evs)

        # Consume cursor and defer unmerged reminder so only new unread events could block Stop hook.
        self.consume_events(repo)
        self.assertEqual(
            self.cli("work", "defer", run_id, "--reason", "等主干稳定", "--until", "4h", cwd=repo, human=True).returncode,
            0,
        )
        self.assertIsNone(self.run_stop_hook(repo))

        # Advance mainline 3 times on UNRELATED other.txt -> MUST NOT re-emit premerge_conflict!
        for n in range(1, 4):
            (repo / "other.txt").write_text(f"v{n}\n", encoding="utf-8")
            self.git(repo, "add", "other.txt")
            self.git(repo, "commit", "-qm", f"unrelated {n}")
            h_cur = self.git(repo, "rev-parse", "HEAD")
            self.cli("status", run_id, cwd=repo)
            self.until(
                lambda h=h_cur: (
                    (pm := json.loads(self.cli("status", run_id, cwd=repo).stdout).get("premerge", {}))
                    and pm.get("status") == "conflict"
                    and pm.get("baseHead") == h
                ),
                f"conflict baseHead bumped silently to unrelated head #{n}",
                timeout=15,
            )

        conf_evs_after_unrelated = [
            e for e in self.read_events() if e.get("run") == run_id and e.get("kind") == "premerge_conflict"
        ]
        self.assertEqual(len(conf_evs_after_unrelated), 1, "unrelated commits must not duplicate premerge_conflict")
        self.assertIsNone(self.run_stop_hook(repo), "no unread premerge_conflict after unrelated commits")

        # Now modify the conflicted file `shared.txt` on mainline (new blob!) -> MUST re-merge & emit 2nd event!
        (repo / "shared.txt").write_text("from-main-v2\nline2\nline3\n", encoding="utf-8")
        self.git(repo, "add", "shared.txt")
        self.git(repo, "commit", "-qm", "mainline conflict v2")
        h_conf2 = self.git(repo, "rev-parse", "HEAD")
        self.cli("status", run_id, cwd=repo)
        self.until(
            lambda: (
                len([e for e in self.read_events() if e.get("run") == run_id and e.get("kind") == "premerge_conflict"]) == 2
                and json.loads(self.cli("status", run_id, cwd=repo).stdout).get("premerge", {}).get("baseHead") == h_conf2
            ),
            "second premerge_conflict emitted after conflict file blob changed",
            timeout=15,
        )

    # ------------------------------------------------------------------
    # Item 3: `delegate mark-applied` + Stop hook auto-detection of verbatim source HEAD
    # ------------------------------------------------------------------
    def test_3_mark_applied_and_stop_hook_verbatim_head_detection(self):
        repo = self.make_repo("repo-manual-apply", {"a.txt": "v0\n", "b.txt": "v0\n", "c.txt": "v0\n"})

        # Part A: `delegate mark-applied <run> [--commit <sha>] [--check-files]`
        self.fake_pi(
            [fixtures.answer("edited a and b"), fixtures.SETTLED],
            pre="echo from-r1-a > a.txt; echo from-r1-b > b.txt",
        )
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "manual-r1", "--accept", "true", "edit a and b")
        )
        self.consume_events(repo)
        blk = self.run_stop_hook(repo)
        self.assertIsNotNone(blk)
        self.assertIn("manual-r1", blk["reason"])

        # Commit only a.txt first (partial commit, with manually resolved content).
        (repo / "a.txt").write_text("manually-resolved-a\n", encoding="utf-8")
        self.git(repo, "add", "a.txt")
        self.git(repo, "commit", "-qm", "partial manual merge")
        partial_sha = self.git(repo, "rev-parse", "HEAD")

        # Invalid commit fails.
        bad = self.cli("mark-applied", r1["run"], "--commit", "deadbeefdeadbeef", cwd=repo, human=True)
        self.assertNotEqual(bad.returncode, 0)

        # `--check-files` fails when b.txt is missing from partial commit.
        miss = self.cli("mark-applied", r1["run"], "--commit", partial_sha, "--check-files", cwd=repo, human=True)
        self.assertNotEqual(miss.returncode, 0)
        self.assertIn("b.txt", miss.stderr)

        # Commit both a.txt and b.txt in a squash commit -> `--check-files` succeeds!
        (repo / "a.txt").write_text("manually-resolved-a2\n", encoding="utf-8")
        (repo / "b.txt").write_text("manually-resolved-b\n", encoding="utf-8")
        self.git(repo, "add", "a.txt", "b.txt")
        self.git(repo, "commit", "-qm", "full manual merge")
        full_sha = self.git(repo, "rev-parse", "HEAD")

        ok = self.cli("mark-applied", r1["run"], "--commit", full_sha, "--check-files", "--json", cwd=repo, human=True)
        self.assertEqual(ok.returncode, 0, ok.stdout + ok.stderr)
        ok_doc = json.loads(ok.stdout)
        self.assertEqual(ok_doc["appliedBy"], "manual")
        self.assertEqual(ok_doc["commit"], full_sha)
        self.assertEqual(ok_doc["missingFiles"], [])

        st1 = json.loads(self.cli("status", r1["run"], cwd=repo).stdout)
        self.assertTrue(st1.get("applied"), st1)
        self.assertEqual(st1.get("appliedBy"), "manual")
        self.assertIsNone(self.run_stop_hook(repo))

        # Part B: Stop hook auto-treats a run as applied when net changes appear verbatim in source HEAD.
        self.fake_pi(
            [fixtures.answer("edited c"), fixtures.SETTLED],
            pre="echo verbatim-from-r2 > c.txt",
        )
        r2 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "verbatim-r2", "--accept", "true", "edit c")
        )
        # Before committing verbatim content: Stop hook blocks (even after consuming events cursor).
        self.consume_events(repo)
        blk2 = self.run_stop_hook(repo)
        self.assertIsNotNone(blk2)
        self.assertIn("verbatim-r2", blk2["reason"])

        # Commit exact verbatim content of c.txt directly in source HEAD (no `apply` or `mark-applied`).
        (repo / "c.txt").write_text("verbatim-from-r2\n", encoding="utf-8")
        self.git(repo, "add", "c.txt")
        self.git(repo, "commit", "-qm", "manual verbatim commit of r2")

        # Stop hook now passes automatically!
        self.assertIsNone(self.run_stop_hook(repo), "Stop hook should auto-treat verbatim HEAD match as applied")

    # ------------------------------------------------------------------
    # Item 4: Per-run defer does NOT defer multi-run work item or sibling runs
    # ------------------------------------------------------------------
    def test_4_work_defer_by_run_scopes_to_run_not_work_item(self):
        repo = self.make_repo("repo-multi-run-defer", {"storage.rs": "v0\n", "routes.rs": "v0\n"})
        add_res = self.cli("work", "add", "W076 组织存储与路由", "--repo", repo, "--json", human=True)
        self.assertEqual(add_res.returncode, 0, add_res.stdout + add_res.stderr)
        wid = json.loads(add_res.stdout)["id"]

        self.fake_pi([fixtures.answer("storage done"), fixtures.SETTLED], pre="echo s1 > storage.rs")
        r_storage = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--work", wid, "--name", "org-step3-storage", "--accept", "true", "storage")
        )
        self.fake_pi([fixtures.answer("routes done"), fixtures.SETTLED], pre="echo r1 > routes.rs")
        r_routes = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--work", wid, "--name", "org-step3-routes", "--accept", "true", "routes")
        )

        self.consume_events(repo)
        blk_both = self.run_stop_hook(repo)
        self.assertIsNotNone(blk_both)
        self.assertIn("org-step3-storage", blk_both["reason"])
        self.assertIn("org-step3-routes", blk_both["reason"])

        # Defer ONLY org-step3-storage by run ID -> work item state stays non-deferred, sibling still blocks!
        def_run = self.cli(
            "work", "defer", r_storage["run"], "--reason", "等 schema 确认", "--until", "4h", "--json", cwd=repo, human=True
        )
        self.assertEqual(def_run.returncode, 0, def_run.stdout + def_run.stderr)
        def_doc = json.loads(def_run.stdout)
        self.assertNotEqual(def_doc["state"], "deferred", "per-run defer must not set work item to deferred")
        self.assertTrue(def_doc.get("runOnly"))
        self.assertIn(r_storage["run"], def_doc.get("runDefers", {}))

        blk_after = self.run_stop_hook(repo)
        self.assertIsNotNone(blk_after, "sibling run org-step3-routes must still block Stop hook")
        self.assertNotIn("org-step3-storage", blk_after["reason"])
        self.assertIn("org-step3-routes", blk_after["reason"])

        # Defer by work item ID `Wxx` -> defers the entire work item (and suppresses both runs).
        def_work = self.cli(
            "work", "defer", wid, "--reason", "整项延后", "--until", "4h", "--json", cwd=repo, human=True
        )
        self.assertEqual(def_work.returncode, 0, def_work.stdout + def_work.stderr)
        self.assertEqual(json.loads(def_work.stdout)["state"], "deferred")
        self.assertIsNone(self.run_stop_hook(repo))

    # ------------------------------------------------------------------
    # Item 5: Occupied migration numbers count ONLY net additions relative to netBase
    # ------------------------------------------------------------------
    def test_5_numbers_taken_excludes_mainline_and_synced_migrations(self):
        repo = self.make_repo(
            "repo-mig-taken",
            {
                "migrations/202611240001_init.sql": "SELECT 1;\n",
                "app.rs": "fn main() {}\n",
            },
        )
        # Start run 1 (`free-gesture-1`) which adds its own migration 202611250001.
        self.fake_pi(
            [fixtures.answer("r1 done"), fixtures.SETTLED],
            pre="echo 'CREATE TABLE g();' > migrations/202611250001_free_gesture.sql",
        )
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "free-gesture-1", "--accept", "true", "add gesture mig")
        )

        # Now mainline commits 202611241442_organization_storage_usage.sql AND leaves 202611241443 uncommitted in source.
        (repo / "migrations/202611241442_organization_storage_usage.sql").write_text("CREATE TABLE org_usage();\n", encoding="utf-8")
        self.git(repo, "add", "migrations/202611241442_organization_storage_usage.sql")
        self.git(repo, "commit", "-qm", "feat: org storage usage migration")
        (repo / "migrations/202611241443_source_uncommitted.sql").write_text("CREATE TABLE wip();\n", encoding="utf-8")

        # Sync `free-gesture-1` with source via `reply --sync=auto` so its worktree now contains BOTH
        # mainline 202611241442 AND synced uncommitted 202611241443!
        self.fake_pi([fixtures.answer("r1 synced"), fixtures.SETTLED])
        r1_reply = self.outcome(
            self.cli("reply", "--sync", r1["run"], "sync mainline")
        )
        r1_synced = self.outcome(self.cli("wait", "--no-result", r1_reply["run"]))
        self.assertEqual(r1_synced["state"], "delivered", r1_synced)

        # Start a new run `next-task` and inspect its prompt.md / prepare-notices.txt for `已占用编号`.
        self.fake_pi([fixtures.answer("next done"), fixtures.SETTLED])
        r2 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "next-task", "--accept", "true", "next work")
        )
        r2_dir = Path(r2["dir"])
        prompt_text = (r2_dir / "prompt.md").read_text(encoding="utf-8")
        notice_file = r2_dir / "prepare-notices.txt"
        notice_text = notice_file.read_text(encoding="utf-8") if notice_file.is_file() else ""
        combined = prompt_text + "\n" + notice_text

        # Mainline committed migration 202611241442 must NOT be listed at all.
        self.assertNotIn("202611241442", combined, f"mainline migration falsely reported: {combined}")
        # Source uncommitted migration 202611241443 may ONLY be attributed to `source checkout`, NEVER to `free-gesture-1`.
        self.assertIn("202611241443 (source checkout)", combined, combined)
        self.assertNotIn("202611241443 (free-gesture-1)", combined, combined)
        self.assertNotIn("202611241443 (reply to free-gesture-1)", combined, combined)
        # free-gesture-1's genuine net-new migration 202611250001 MUST be attributed to free-gesture-1's chain.
        self.assertRegex(combined, r"202611250001 \((?:reply to )?free-gesture-1\)", combined)

    # ------------------------------------------------------------------
    # Item 6: `work close <Wxx> "<evidence>"` positional evidence shorthand
    # ------------------------------------------------------------------
    def test_6_work_close_accepts_positional_evidence(self):
        repo = self.make_repo("repo-close-pos", {"a.txt": "v0\n"})
        w1 = json.loads(self.cli("work", "add", "位置证据测试", "--repo", repo, "--json", human=True).stdout)
        wid = w1["id"]

        res = self.cli("work", "close", wid, "commit abc1234, 验证已通过", "--json", cwd=repo, human=True)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        closed = json.loads(res.stdout)
        self.assertEqual(closed["state"], "done")
        self.assertEqual(closed["closeEvidence"], "commit abc1234, 验证已通过")

        # Passing both `--evidence` and an extra positional argument still fails cleanly.
        w2 = json.loads(self.cli("work", "add", "多余位置参数测试", "--repo", repo, "--json", human=True).stdout)
        bad = self.cli("work", "close", w2["id"], "--evidence", "ev1", "extra-arg", cwd=repo, human=True)
        self.assertNotEqual(bad.returncode, 0)
        self.assertIn("unexpected positional argument", bad.stderr)

    # ------------------------------------------------------------------
    # Rework fix 1: `blocked_mainline` does NOT spend or hit `maxRework` on `reply`
    # ------------------------------------------------------------------
    def test_1d_blocked_mainline_reply_does_not_spend_rework_budget(self):
        repo = self.make_repo(
            "repo-mainline-rework-budget",
            {
                "apps/ui.ts": "export const ui = 0;\n",
                "crates/api/handlers.rs": "OK\n",
                ".delegate.json": json.dumps({"maxRework": 1}),
            },
        )
        accept_cmd = (
            "if grep -q '^OK$' crates/api/handlers.rs; then exit 0; "
            "else echo 'FAIL: size-audit crates/api/handlers.rs:1004 exceeds 1000 lines'; exit 1; fi"
        )

        # Round 1: edits apps/ui.ts and passes -> delivered (rework used: 0/1).
        self.fake_pi(
            [fixtures.answer("round 1"), fixtures.SETTLED],
            pre="echo 'export const ui = 1;' > apps/ui.ts",
        )
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "budget-run", "--accept", accept_cmd, "r1")
        )
        self.assertEqual(r1["state"], "delivered", r1)

        # Round 2 (real rework): spends the 1/1 rework budget and passes -> delivered (rework used: 1/1).
        self.fake_pi(
            [fixtures.answer("round 2"), fixtures.SETTLED],
            pre="echo 'export const ui = 2;' > apps/ui.ts",
        )
        r2_start = self.outcome(self.cli("reply", "--no-sync", r1["run"], "first real rework"))
        r2 = self.outcome(self.cli("wait", "--no-result", r2_start["run"]))
        self.assertEqual(r2["state"], "delivered", r2)
        r2_meta = json.loads((Path(r2["dir"]) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(r2_meta["rework"]["used"], 1, r2_meta["rework"])

        # Mainline regresses handlers.rs; Round 3 (`--minor` short fix) syncs broken mainline -> blocked_mainline.
        (repo / "crates/api/handlers.rs").write_text("TOO_BIG\n", encoding="utf-8")
        self.git(repo, "add", "crates/api/handlers.rs")
        self.git(repo, "commit", "-qm", "mainline regression")

        self.fake_pi(
            [fixtures.answer("round 3 minor"), fixtures.SETTLED],
            pre="echo 'export const ui = 3;' > apps/ui.ts",
        )
        r3_start = self.outcome(self.cli("reply", "--sync", "--minor", r2["run"], "minor tweak"))
        r3 = self.outcome(self.cli("wait", "--no-result", r3_start["run"]))
        self.assertEqual(r3["state"], "blocked_mainline", r3)

        # Mainline fixes handlers.rs; controller runs `delegate reply <r3> '重跑'` (no --minor / --over-limit)
        # even though maxRework 1/1 is already exhausted -> MUST be allowed as continuation (used stays 1)!
        (repo / "crates/api/handlers.rs").write_text("OK\n", encoding="utf-8")
        self.git(repo, "add", "crates/api/handlers.rs")
        self.git(repo, "commit", "-qm", "mainline fix")

        self.fake_pi([fixtures.answer("round 4 rerun"), fixtures.SETTLED])
        r4_cmd = self.cli("reply", "--sync", r3["run"], "重跑")
        self.assertEqual(r4_cmd.returncode, 0, r4_cmd.stdout + r4_cmd.stderr)
        r4_start = self.outcome(r4_cmd)
        r4 = self.outcome(self.cli("wait", "--no-result", r4_start["run"]))
        self.assertEqual(r4["state"], "delivered", r4)
        r4_meta = json.loads((Path(r4["dir"]) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(r4_meta["rework"]["kind"], "continuation", r4_meta["rework"])
        self.assertEqual(r4_meta["rework"]["used"], 1, "blocked_mainline reply must not increment rework used")

        # Also verify a fresh run hitting blocked_mainline on Round 1 keeps rework.used == 0 on reply.
        (repo / "crates/api/handlers.rs").write_text("TOO_BIG_2\n", encoding="utf-8")
        self.git(repo, "add", "crates/api/handlers.rs")
        self.git(repo, "commit", "-qm", "mainline regression 2")
        self.fake_pi(
            [fixtures.answer("fresh r1"), fixtures.SETTLED],
            pre="echo 'export const ui = 10;' > apps/ui.ts",
        )
        f1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "fresh-blocked", "--accept", accept_cmd, "f1")
        )
        self.assertEqual(f1["state"], "blocked_mainline", f1)
        (repo / "crates/api/handlers.rs").write_text("OK\n", encoding="utf-8")
        self.git(repo, "add", "crates/api/handlers.rs")
        self.git(repo, "commit", "-qm", "mainline fix 2")
        self.fake_pi([fixtures.answer("fresh r2"), fixtures.SETTLED])
        f2_start = self.outcome(self.cli("reply", "--sync", f1["run"], "重跑"))
        f2 = self.outcome(self.cli("wait", "--no-result", f2_start["run"]))
        self.assertEqual(f2["state"], "delivered", f2)
        f2_meta = json.loads((Path(f2["dir"]) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(f2_meta["rework"]["kind"], "continuation", f2_meta["rework"])
        self.assertEqual(f2_meta["rework"]["used"], 0, f2_meta["rework"])

    # ------------------------------------------------------------------
    # Rework fix 2: `mark-applied` handles 2-parent merge commits, advances to chain tail, rejects active
    # ------------------------------------------------------------------
    def test_3b_mark_applied_merge_commit_reply_chain_tail_and_rejects_active(self):
        repo = self.make_repo(
            "repo-mark-applied-merge",
            {"a.txt": "v0\n", "b.txt": "v0\n", "main.txt": "v0\n"},
        )

        # Build a 2-round reply chain: r1 edits a.txt, r2 (reply) edits b.txt.
        self.fake_pi([fixtures.answer("r1"), fixtures.SETTLED], pre="echo r1-a > a.txt")
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "chain-merge", "--accept", "true", "r1")
        )
        self.fake_pi([fixtures.answer("r2"), fixtures.SETTLED], pre="echo r2-b > b.txt")
        r2_start = self.outcome(self.cli("reply", "--no-sync", r1["run"], "also edit b"))
        r2 = self.outcome(self.cli("wait", "--no-result", r2_start["run"]))
        self.assertEqual(r2["state"], "delivered", r2)

        # Create a real 2-parent merge commit on the default branch containing both a.txt and b.txt.
        default_branch = self.git(repo, "rev-parse", "--abbrev-ref", "HEAD")
        self.git(repo, "checkout", "-qb", "manual-merge-src")
        (repo / "a.txt").write_text("merged-a\n", encoding="utf-8")
        (repo / "b.txt").write_text("merged-b\n", encoding="utf-8")
        self.git(repo, "add", "a.txt", "b.txt")
        self.git(repo, "commit", "-qm", "feat: manual changes on branch")
        self.git(repo, "checkout", "-q", default_branch)
        (repo / "main.txt").write_text("main-v1\n", encoding="utf-8")
        self.git(repo, "add", "main.txt")
        self.git(repo, "commit", "-qm", "main advance")
        self.git(repo, "merge", "--no-ff", "-m", "merge manual changes", "manual-merge-src")
        merge_sha = self.git(repo, "rev-parse", "HEAD")
        parents = self.git(repo, "rev-list", "--parents", "-n", "1", merge_sha).split()
        self.assertEqual(len(parents), 3, f"expected 2-parent merge commit, got {parents}")

        # Call `mark-applied` using the ROOT run ID `r1`: must advance to chain tail `r2`, mark BOTH
        # `r2` and `r1`, and pass `--check-files` against the 2-parent merge commit!
        res = self.cli(
            "mark-applied", r1["run"], "--commit", merge_sha, "--check-files", "--json", cwd=repo, human=True
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        doc = json.loads(res.stdout)
        self.assertEqual(doc["run"], r2["run"], doc)
        self.assertEqual(doc["missingFiles"], [], doc)
        self.assertIn(r2["run"], doc["markedRuns"])
        self.assertIn(r1["run"], doc["markedRuns"])
        self.assertTrue(json.loads(self.cli("status", r2["run"], cwd=repo).stdout).get("applied"))
        self.assertTrue(json.loads(self.cli("status", r1["run"], cwd=repo).stdout).get("applied"))

        # Active (still running) run must be rejected by `mark-applied`.
        active_start, gate_path = self.start_gated(
            "--worktree", "--workdir", repo, "--name", "still-active", "--accept", "true", "wait on gate",
            attempts=([fixtures.answer("done"), fixtures.SETTLED],),
        )
        try:
            bad_active = self.cli("mark-applied", active_start["run"], cwd=repo, human=True)
            self.assertNotEqual(bad_active.returncode, 0)
            self.assertIn("is still running", bad_active.stderr)
        finally:
            gate_path.touch()
            self.cli("wait", "--no-result", active_start["run"])

    # ------------------------------------------------------------------
    # Item 7: `apply` (fast-forward, 3-way merge, --merge, --commit) preserves file mode
    # ------------------------------------------------------------------
    def _git_tree_mode(self, repo: Path, rel: str) -> str:
        line = self.git(repo, "ls-tree", "HEAD", "--", rel)
        return line.split()[0] if line else ""

    def _is_exec(self, path: Path) -> bool:
        return bool(os.stat(path).st_mode & 0o111)

    def test_7_apply_preserves_file_mode_across_fast_forward_three_way_and_merge(self):
        base_script = "#!/bin/sh\nline2\nline3\nline4\nline5\nline6\nline7\nline8\nline9\necho tail\n"

        # --- Path 1: Fast-forward (`premerge` -> `apply --commit`) ---
        self.env.pop("DELEGATE_PREMERGE", None)
        repo_ff = self.make_repo(
            "repo-mode-ff",
            {
                "manage.sh": base_script,
                "src_exec.sh": "#!/bin/sh\necho s0\n",
                "tool.sh": "#!/bin/sh\necho t0\n",
                "drop_exec.sh": "#!/bin/sh\necho d0\n",
            },
        )
        (repo_ff / "manage.sh").chmod(0o755)
        (repo_ff / "drop_exec.sh").chmod(0o755)
        self.git(repo_ff, "add", "manage.sh", "drop_exec.sh")
        self.git(repo_ff, "commit", "-qm", "chmod +x manage.sh and drop_exec.sh")

        self.fake_pi(
            [fixtures.answer("ff edits"), fixtures.SETTLED],
            pre=(
                "sed -i 's/echo tail/echo tail-run/' manage.sh; "
                "echo 'echo s-run' >> src_exec.sh; "
                "echo 'echo t-run' >> tool.sh && chmod +x tool.sh; "
                "echo 'echo d-run' >> drop_exec.sh && chmod -x drop_exec.sh"
            ),
        )
        r_ff = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo_ff, "--name", "mode-ff", "--accept", "true", "ff")
        )
        self.assertEqual(r_ff["state"], "delivered", r_ff)

        # Advance mainline: non-conflicting edit on manage.sh (stays 100755) + chmod +x src_exec.sh (100644->100755).
        (repo_ff / "manage.sh").write_text(
            base_script.replace("line2", "line2-main"), encoding="utf-8"
        )
        (repo_ff / "manage.sh").chmod(0o755)
        (repo_ff / "src_exec.sh").chmod(0o755)
        self.git(repo_ff, "add", "manage.sh", "src_exec.sh")
        self.git(repo_ff, "commit", "-qm", "mainline edits manage.sh and chmod +x src_exec.sh")
        h_ff = self.git(repo_ff, "rev-parse", "HEAD")

        # Wake _merge-worker and wait until fast-forward premerge is ready on h_ff.
        self.cli("status", r_ff["run"], cwd=repo_ff)
        self.until(
            lambda: (
                (pm := json.loads(self.cli("status", r_ff["run"], cwd=repo_ff).stdout).get("premerge", {}))
                and pm.get("status") == "ready"
                and pm.get("baseHead") == h_ff
            ),
            "fast-forward premerge ready on updated mainline",
            timeout=15,
        )

        ap_ff = self.cli("apply", "--commit", r_ff["run"], cwd=repo_ff, human=True)
        self.assertEqual(ap_ff.returncode, 0, ap_ff.stdout + ap_ff.stderr)
        self.assertTrue(self._is_exec(repo_ff / "manage.sh"), "ff: manage.sh lost +x")
        self.assertEqual(self._git_tree_mode(repo_ff, "manage.sh"), "100755")
        self.assertTrue(self._is_exec(repo_ff / "src_exec.sh"), "ff: source +x not preserved")
        self.assertEqual(self._git_tree_mode(repo_ff, "src_exec.sh"), "100755")
        self.assertTrue(self._is_exec(repo_ff / "tool.sh"), "ff: run +x not applied")
        self.assertEqual(self._git_tree_mode(repo_ff, "tool.sh"), "100755")
        self.assertFalse(self._is_exec(repo_ff / "drop_exec.sh"), "ff: run -x not applied")
        self.assertEqual(self._git_tree_mode(repo_ff, "drop_exec.sh"), "100644")

        # --- Path 2: Direct 3-way merge (`DELEGATE_PREMERGE=0`, `apply --commit`) ---
        self.env["DELEGATE_PREMERGE"] = "0"
        repo_3w = self.make_repo(
            "repo-mode-3way",
            {
                "manage.sh": base_script,
                "src_exec.sh": "#!/bin/sh\necho s0\n",
                "run_exec.sh": "#!/bin/sh\necho r0\n",
            },
        )
        (repo_3w / "manage.sh").chmod(0o755)
        self.git(repo_3w, "add", "manage.sh")
        self.git(repo_3w, "commit", "-qm", "chmod +x manage.sh")

        self.fake_pi(
            [fixtures.answer("3way edits"), fixtures.SETTLED],
            pre=(
                "sed -i 's/echo tail/echo tail-run/' manage.sh; "
                "echo 'echo s-run' >> src_exec.sh; "
                "echo 'echo r-run' >> run_exec.sh && chmod +x run_exec.sh"
            ),
        )
        r_3w = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo_3w, "--name", "mode-3way", "--accept", "true", "3way")
        )
        self.assertEqual(r_3w["state"], "delivered", r_3w)

        (repo_3w / "manage.sh").write_text(
            base_script.replace("line2", "line2-main"), encoding="utf-8"
        )
        (repo_3w / "manage.sh").chmod(0o755)
        (repo_3w / "src_exec.sh").chmod(0o755)
        self.git(repo_3w, "add", "manage.sh", "src_exec.sh")
        self.git(repo_3w, "commit", "-qm", "mainline edits manage.sh and chmod +x src_exec.sh")

        ap_3w = self.cli("apply", "--commit", r_3w["run"], cwd=repo_3w, human=True)
        self.assertEqual(ap_3w.returncode, 0, ap_3w.stdout + ap_3w.stderr)
        self.assertTrue(self._is_exec(repo_3w / "manage.sh"), "3way: manage.sh lost +x")
        self.assertEqual(self._git_tree_mode(repo_3w, "manage.sh"), "100755")
        self.assertTrue(self._is_exec(repo_3w / "src_exec.sh"), "3way: source +x not preserved")
        self.assertEqual(self._git_tree_mode(repo_3w, "src_exec.sh"), "100755")
        self.assertTrue(self._is_exec(repo_3w / "run_exec.sh"), "3way: run +x not applied")
        self.assertEqual(self._git_tree_mode(repo_3w, "run_exec.sh"), "100755")

        # --- Path 3: Conflict merge (`apply --merge` writing conflict markers) ---
        repo_cm = self.make_repo(
            "repo-mode-merge-conflict",
            {
                "manage.sh": "#!/bin/sh\necho v0\n",
                "drop_conflict.sh": "#!/bin/sh\necho v0\n",
            },
        )
        (repo_cm / "manage.sh").chmod(0o755)
        (repo_cm / "drop_conflict.sh").chmod(0o755)
        self.git(repo_cm, "add", "manage.sh", "drop_conflict.sh")
        self.git(repo_cm, "commit", "-qm", "chmod +x scripts")

        self.fake_pi(
            [fixtures.answer("conflict edits"), fixtures.SETTLED],
            pre=(
                "printf '#!/bin/sh\\necho from-run\\n' > manage.sh; "
                "printf '#!/bin/sh\\necho from-run\\n' > drop_conflict.sh && chmod -x drop_conflict.sh"
            ),
        )
        r_cm = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo_cm, "--name", "mode-merge", "--accept", "true", "cm")
        )
        self.assertEqual(r_cm["state"], "delivered", r_cm)

        (repo_cm / "manage.sh").write_text("#!/bin/sh\necho from-main\n", encoding="utf-8")
        (repo_cm / "manage.sh").chmod(0o755)
        (repo_cm / "drop_conflict.sh").write_text("#!/bin/sh\necho from-main\n", encoding="utf-8")
        (repo_cm / "drop_conflict.sh").chmod(0o755)
        self.git(repo_cm, "add", "manage.sh", "drop_conflict.sh")
        self.git(repo_cm, "commit", "-qm", "mainline conflicting edits")

        ap_cm = self.cli("apply", "--merge", r_cm["run"], cwd=repo_cm, human=True)
        self.assertEqual(ap_cm.returncode, 3, ap_cm.stdout + ap_cm.stderr)
        self.assertIn("<<<<<<<", (repo_cm / "manage.sh").read_text(encoding="utf-8"))
        self.assertTrue(
            self._is_exec(repo_cm / "manage.sh"),
            "apply --merge dropped +x on manage.sh when run did not change mode",
        )
        self.assertIn("<<<<<<<", (repo_cm / "drop_conflict.sh").read_text(encoding="utf-8"))
        self.assertFalse(
            self._is_exec(repo_cm / "drop_conflict.sh"),
            "apply --merge ignored explicit run -x on drop_conflict.sh",
        )


if __name__ == "__main__":
    unittest.main()
