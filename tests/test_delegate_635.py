"""6.35.0 regressions & concurrency stress suite:
1. Background `_merge-worker` + persistent integration worktree under `<cache>` (reusing `target/`
   & `node_modules/`), `premerge.json`, `refs/delegate/premerge/<run>`, L0/L1 display transitions
   (`预合并中` -> `快进 ✓（基于 <short>）`), and fast-forward `apply` (<2s, skips re-verify,
   preserves uncommitted semantics, plus `--commit` compatibility).
2. Tiered `acceptRules` execution on integration tree when HEAD advances (runs ONLY matched layers;
   falls back to `accept` when no `acceptRules`); `delegate sign` triggers `_merge-worker` for
   `controllerReviewPaths` runs.
3. Conflict & failed premerge: emits `premerge_conflict` / `premerge_failed` with conflict files
   and log pointer, displays `预合并冲突：<文件>` / `预合并失败`, does NOT auto-reply/rework.
4. HEAD advance & dirty overlap fallback:
   - HEAD advances -> `status` shows `快进 ✗ 主干已前进` (zero-lock) and wakes `_merge-worker` to
     re-premerge on new HEAD.
   - `apply` after HEAD advance falls back to 3-way merge with reason `主干已前进 N 个提交，走三方合并`.
   - Dirty overlap on `changedPaths` falls back to 3-way merge; unrelated dirty file still fast-forwards!
5. Four required concurrency & fail-closed stress cases:
   - Worker running while controller commits on source repo.
   - Two runs delivered simultaneously (single-instance `LOCK_NB` + wake handoff).
   - `apply` runs while worker is actively premerging (never blocks on worker, never corrupts source).
   - Worker killed with `SIGKILL` mid-run (source untouched, `apply` falls back cleanly, `status`
     detects dead worker and recovers to `快进 ✓`).
"""
import json
import os
import shlex
import shutil
import signal
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate635Tests(unittest.TestCase):
    def setUp(self):
        fixtures.DelegateTests.setUp(self)
        self.env.pop("DELEGATE_PREMERGE", None)

    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until
    wait_json = fixtures.DelegateTests.wait_json
    wait_gone = fixtures.DelegateTests.wait_gone

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def cache_dir(self) -> Path:
        return Path(self.env["XDG_CACHE_HOME"]) / "delegate"

    def git(self, repo: Path, *args: str) -> str:
        res = subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            text=True,
            check=True,
        )
        return res.stdout.strip()

    def read_events(self) -> list[dict]:
        ev_file = self.state_dir() / "events.jsonl"
        if not ev_file.is_file():
            return []
        return [json.loads(ln) for ln in ev_file.read_text().splitlines() if ln.strip()]

    def wait_premerge_status(self, run_dir: Path, expected: set[str], timeout: float = 15.0) -> dict:
        pm_file = run_dir / "premerge.json"

        def check():
            if not pm_file.is_file():
                return False
            try:
                doc = json.loads(pm_file.read_text())
            except ValueError:
                return False
            return doc.get("status") in expected

        self.until(check, f"premerge.json status in {expected}", timeout=timeout)
        return json.loads(pm_file.read_text())

    def write_user_config(self, doc: dict) -> None:
        cfg_dir = Path(self.env["XDG_CONFIG_HOME"]) / "delegate"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "config.json").write_text(json.dumps(doc))
        for k in ("DELEGATE_SCOUT_MODEL", "DELEGATE_REVIEW_MODEL", "DELEGATE_WRITE_MODEL"):
            self.env.pop(k, None)

    def install_scripted_pi(self, body_py: str) -> Path:
        log_file = self.work / "pi-calls.jsonl"
        self.env["SCRIPTED_PI_LOG"] = str(log_file)
        pi = self.bin / "pi"
        pi.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys, time\n"
            "from pathlib import Path\n"
            "args = sys.argv[1:]\n"
            "if args == ['--version']:\n"
            "    print('fake-pi 1.0'); sys.exit(0)\n"
            "if len(args) >= 2 and args[1] == '--list-models':\n"
            "    print('provider model context max-out thinking images')\n"
            "    print('p write-m 1M 1K yes yes')\n"
            "    print('p review-m 1M 1K yes yes')\n"
            "    sys.exit(0)\n"
            "stdin_text = sys.stdin.read()\n"
            "sdir = args[args.index('--session-dir') + 1] if '--session-dir' in args else ''\n"
            "sid = args[args.index('--session-id') + 1] if '--session-id' in args else ''\n"
            "if sdir:\n"
            "    Path(sdir).mkdir(parents=True, exist_ok=True)\n"
            "    (Path(sdir) / f'1_{sid or os.getpid()}.jsonl').write_text('{\"type\":\"session\"}\\n')\n"
            "lp = Path(os.environ['SCRIPTED_PI_LOG'])\n"
            "call_idx = len(lp.read_text().splitlines()) + 1 if lp.is_file() else 1\n"
            "with lp.open('a') as fh:\n"
            "    fh.write(json.dumps({'call': call_idx, 'cwd': os.getcwd()}) + '\\n')\n"
            "def emit_answer(text):\n"
            "    print(json.dumps({'type':'message_end','message':{'role':'assistant','stopReason':'stop','model':'p/m','usage':{'input':10,'output':5,'cacheRead':0},'content':[{'type':'text','text':text}]}}), flush=True)\n"
            "    print(json.dumps({'type':'agent_settled'}), flush=True)\n"
            + body_py
            + "\n"
        )
        pi.chmod(0o755)
        self.write_user_config({
            "models": {"w": "p/write-m", "r": "p/review-m"},
            "tiers": {"scout": "r", "review": "r", "write": "w"},
        })
        return log_file

    # ------------------------------------------------------------------
    # 1. Delivery -> `预合并中` -> `快进 ✓（基于 <short>）` -> fast-forward apply (<2s)
    # ------------------------------------------------------------------
    def test_delivery_premerge_transition_and_fast_forward_apply_under_2s(self):
        verify_marker = self.work / "verify-ran"
        pm_gate = self.work / "ff-premerge.gate"
        self.addCleanup(lambda: pm_gate.touch())
        repo = self.repo({
            ".delegate.json": json.dumps({
                "accept": "true",
                "applyVerify": f"touch {verify_marker} && exit 1",
            }),
            "src/a.txt": "v0\n",
            "src/del.txt": "to-delete\n",
        })
        head0 = self.git(repo, "rev-parse", "HEAD")
        short0 = head0[:7]

        # Hold worker on an explicit gate so we deterministically observe `预合并中`!
        self.env["DELEGATE_TEST_PREMERGE_GATE"] = str(pm_gate)
        self.fake_pi(
            [fixtures.answer("updated a.txt and removed del.txt"), fixtures.SETTLED],
            pre="echo v1 > src/a.txt; rm -f src/del.txt; echo new > src/new.txt",
        )
        st = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "ff-happy", "edit files")
        )
        self.assertEqual(st["state"], "delivered")
        run_id = st["run"]
        run_dir = Path(st["dir"])

        # While worker is paused at the gate: L0 deterministically shows `预合并中`!
        self.wait_premerge_status(run_dir, {"running"})
        l0_during = self.cli("status", run_id, human=True, cwd=repo).stdout.splitlines()[0]
        self.assertIn("预合并中", l0_during, l0_during)

        # Release gate and wait for `_merge-worker` to finish -> `premerge.json` is ready + ref pinned!
        pm_gate.touch()
        pm = self.wait_premerge_status(run_dir, {"ready"})
        self.assertEqual(pm["baseHead"], head0)
        self.assertTrue(pm["mergedTree"])
        self.assertEqual(sorted(pm["changedPaths"]), ["src/a.txt", "src/del.txt", "src/new.txt"])
        self.assertTrue(pm["acceptOk"])
        self.assertIn("elapsedSeconds", pm)
        self.assertIn("ranLayers", pm)

        # Verify ref `refs/delegate/premerge/<run>` is pinned in source repo (NOT inside source workdir)
        pinned = self.git(repo, "rev-parse", f"refs/delegate/premerge/{run_id}")
        self.assertEqual(pinned, pm["mergedTree"])

        # Integration worktree lives under `<cache>/premerge/...`, never inside source repo!
        premerge_wts = list((self.cache_dir() / "premerge").glob("*/worktree"))
        self.assertEqual(len(premerge_wts), 1)
        self.assertFalse((repo / ".local/premerge").exists())

        # L0 & L1 now show `快进 ✓（基于 <short>）`!
        l0_ready = self.cli("status", run_id, human=True, cwd=repo).stdout.splitlines()[0]
        self.assertIn(f"快进 ✓（基于 {short0}）", l0_ready, l0_ready)
        l1_card = self.cli("card", run_id, human=True, cwd=repo).stdout
        self.assertIn(f"快进 ✓（基于 {short0}）", l1_card, l1_card)

        # Fast-forward apply hits the premerged tree, keeps changes uncommitted, and skips failing applyVerify!
        self.env.pop("DELEGATE_TEST_PREMERGE_GATE", None)
        ap = self.cli("apply", "--json", run_id, cwd=repo)
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)
        ap_doc = self.outcome(ap)
        self.assertTrue(ap_doc["fastForward"]["hit"], ap_doc)
        self.assertEqual(ap_doc["fastForward"]["baseHead"], head0)
        self.assertTrue(ap_doc["acceptStillValid"], ap_doc)
        self.assertFalse(verify_marker.exists(), "fast-forward apply must skip re-running verify")

        # Changes landed in source working tree, uncommitted (git index untouched)!
        self.assertEqual((repo / "src/a.txt").read_text(), "v1\n")
        self.assertFalse((repo / "src/del.txt").exists())
        self.assertEqual((repo / "src/new.txt").read_text(), "new\n")
        self.assertEqual(self.git(repo, "rev-parse", "HEAD"), head0)
        staged = self.git(repo, "diff", "--cached", "--name-only")
        self.assertEqual(staged, "", f"fast-forward must leave changes uncommitted, staged={staged!r}")

    # ------------------------------------------------------------------
    # 2. Tiered acceptRules on integration tree + target/node_modules reuse + sign trigger
    # ------------------------------------------------------------------
    def test_accept_rules_cache_reuse_and_sign_triggers_premerge(self):
        layer_log = self.work / "premerge-layers.log"
        repo = self.repo({
            ".gitignore": "target/\nnode_modules/\n",
            ".delegate.json": json.dumps({
                "accept": f"echo full >> {layer_log}",
                "acceptRules": [
                    {
                        "name": "web",
                        "tier": "fast",
                        "paths": ["apps/web/**"],
                        "command": f"echo web >> {layer_log}",
                    },
                    {
                        "name": "rust-slow",
                        "tier": "slow",
                        "paths": ["crates/**"],
                        "command": f"echo rust-slow >> {layer_log}",
                    },
                ],
                "review": True,
                "controllerReviewPaths": ["apps/web/policy.ts"],
            }),
            "apps/web/policy.ts": "export const P = 1;\n",
            "crates/core/src/lib.rs": "pub fn v() -> i32 { 1 }\n",
            "docs/other.md": "v0\n",
        })

        self.install_scripted_pi(
            "if call_idx == 1:\n"
            "    Path('apps/web/policy.ts').write_text('export const P = 2;\\n')\n"
            "    emit_answer('updated policy')\n"
            "else:\n"
            "    emit_answer('PASS\\nClean policy update.')\n"
        )
        st = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "signed-web", "update policy")
        )
        run_id = st["run"]
        run_dir = Path(st["dir"])
        self.assertEqual(st["state"], "delivered")
        self.assertEqual(st["review"]["verdict"], "NEEDS_CONTROLLER")

        # Before `delegate sign`, `_merge-worker` MUST NOT premerge (unsigned controllerReviewPaths).
        fixtures.settle_supervisors(self.work / "runs")
        self.assertFalse((run_dir / "premerge.json").exists())
        l0_unsigned = self.cli("status", run_id, human=True, cwd=repo).stdout.splitlines()[0]
        self.assertIn("快进 —", l0_unsigned)

        # Advance main on an unrelated file so premerge MUST actually run `acceptRules` on integration tree!
        (repo / "docs/other.md").write_text("v1\n")
        self.git(repo, "add", "docs/other.md")
        self.git(repo, "commit", "-qm", "unrelated main commit")
        head1 = self.git(repo, "rev-parse", "HEAD")

        # Reset layer_log so we observe ONLY what `_merge-worker` runs!
        layer_log.write_text("")

        # `delegate sign` succeeds -> triggers `_merge-worker`!
        sign_res = self.cli("sign", run_id, cwd=repo)
        self.assertEqual(sign_res.returncode, 0, sign_res.stdout + sign_res.stderr)
        pm = self.wait_premerge_status(run_dir, {"ready"})
        self.assertEqual(pm["baseHead"], head1)
        self.assertFalse(pm.get("reusedAccept"), "HEAD moved so premerge must execute matched layers")
        self.assertEqual(pm["ranLayers"], ["web"])
        self.assertEqual(layer_log.read_text().splitlines(), ["web"])

        # Plant cache artifacts inside persistent integration worktree's `target/` and `node_modules/`.
        int_wt = next((self.cache_dir() / "premerge").glob("*/worktree"))
        (int_wt / "target").mkdir(parents=True, exist_ok=True)
        (int_wt / "target/cached.rlib").write_text("warm-rust-cache")
        (int_wt / "node_modules").mkdir(parents=True, exist_ok=True)
        (int_wt / "node_modules/pkg.txt").write_text("warm-node-cache")

        # Advance main again -> `status` detects `快进 ✗ 主干已前进` and wakes worker -> caches survive!
        (repo / "docs/other.md").write_text("v2\n")
        self.git(repo, "add", "docs/other.md")
        self.git(repo, "commit", "-qm", "second unrelated commit")
        head2 = self.git(repo, "rev-parse", "HEAD")

        l0_stale = self.cli("status", run_id, human=True, cwd=repo).stdout.splitlines()[0]
        self.assertIn("快进 ✗ 主干已前进", l0_stale, l0_stale)

        self.wait_json(
            run_dir / "premerge.json",
            lambda d: d.get("baseHead") == head2 and d.get("status") == "ready",
            "worker re-premerges onto head2 after status wakeup",
            timeout=15.0,
        )
        self.assertEqual((int_wt / "target/cached.rlib").read_text(), "warm-rust-cache")
        self.assertEqual((int_wt / "node_modules/pkg.txt").read_text(), "warm-node-cache")

    # ------------------------------------------------------------------
    # 3. Premerge conflict & premerge acceptance failure (events, display, no auto-rework)
    # ------------------------------------------------------------------
    def test_premerge_conflict_and_failed_accept_emit_events_without_auto_rework(self):
        repo = self.repo({
            ".delegate.json": json.dumps({"accept": "grep -q 'OK' check.txt"}),
            "shared.txt": "line1\n",
            "check.txt": "OK\n",
        })
        self.fake_pi(
            [fixtures.answer("edited shared"), fixtures.SETTLED],
            pre="echo 'colleague-change' > shared.txt",
        )
        st = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "conf-run", "edit shared")
        )
        run_id = st["run"]
        run_dir = Path(st["dir"])
        self.wait_premerge_status(run_dir, {"ready"})

        # Main edits the SAME line of shared.txt -> triggers premerge conflict!
        (repo / "shared.txt").write_text("main-conflicting-change\n")
        self.git(repo, "add", "shared.txt")
        self.git(repo, "commit", "-qm", "conflict on shared.txt")
        head_c = self.git(repo, "rev-parse", "HEAD")

        # `status` wakes worker -> worker hits 3-way conflict
        _ = self.cli("status", run_id, human=True, cwd=repo)
        pm_doc = self.wait_json(
            run_dir / "premerge.json",
            lambda d: d.get("baseHead") == head_c and d.get("status") == "conflict",
            "premerge hits conflict",
            timeout=15.0,
        )
        self.assertIn("shared.txt", pm_doc["conflicts"])
        self.assertTrue(Path(pm_doc["log"]).is_file())

        l0_conf = self.cli("status", run_id, human=True, cwd=repo).stdout.splitlines()[0]
        self.assertIn("预合并冲突：shared.txt", l0_conf, l0_conf)
        l1_conf = self.cli("card", run_id, human=True, cwd=repo).stdout
        self.assertIn("预合并冲突：shared.txt", l1_conf, l1_conf)

        evs = self.read_events()
        conf_evs = [e for e in evs if e["run"] == run_id and e["kind"] == "premerge_conflict"]
        self.assertEqual(len(conf_evs), 1, evs)
        self.assertIn("shared.txt", conf_evs[0]["payload"]["conflicts"])
        self.assertTrue(conf_evs[0]["payload"].get("log"))
        # Must NOT auto-rework/reply!
        runs_dirs = [p for p in (self.work / "runs").iterdir() if p.is_dir()]
        self.assertEqual(len(runs_dirs), 1, "conflict must NOT auto-spawn a reply run")

        # Now test `premerge_failed`: revert shared.txt conflict, break check.txt on main!
        (repo / "shared.txt").write_text("line1\n")
        (repo / "check.txt").write_text("FAIL\n")
        self.git(repo, "add", "shared.txt", "check.txt")
        self.git(repo, "commit", "-qm", "break accept on main")
        head_f = self.git(repo, "rev-parse", "HEAD")

        _ = self.cli("status", run_id, human=True, cwd=repo)
        pm_fail = self.wait_json(
            run_dir / "premerge.json",
            lambda d: d.get("baseHead") == head_f and d.get("status") == "failed",
            "premerge hits accept failure",
            timeout=15.0,
        )
        self.assertFalse(pm_fail["acceptOk"])
        self.assertTrue(Path(pm_fail["log"]).is_file())
        fail_evs = [e for e in self.read_events() if e["run"] == run_id and e["kind"] == "premerge_failed"]
        self.assertEqual(len(fail_evs), 1)
        self.assertTrue(fail_evs[0]["payload"].get("log"))

    # ------------------------------------------------------------------
    # 4. Fallback reason on HEAD advance + dirty overlap vs unrelated dirty file
    # ------------------------------------------------------------------
    def test_apply_fallback_reason_on_head_advance_and_dirty_overlap(self):
        repo = self.repo({
            ".delegate.json": json.dumps({"accept": "true"}),
            "a.txt": "v0\n",
            "b.txt": "v0\n",
            "unrelated.txt": "v0\n",
        })
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo v1 > a.txt")
        st1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "r-head", "edit a"))
        self.wait_premerge_status(Path(st1["dir"]), {"ready"})

        # Advance main by 2 commits -> apply directly (without waiting for re-premerge) -> 3-way fallback + exact reason!
        (repo / "b.txt").write_text("c1\n")
        self.git(repo, "add", "b.txt")
        self.git(repo, "commit", "-qm", "commit 1")
        (repo / "b.txt").write_text("c2\n")
        self.git(repo, "add", "b.txt")
        self.git(repo, "commit", "-qm", "commit 2")

        ap1 = self.cli("apply", st1["run"], human=True, cwd=repo)
        self.assertEqual(ap1.returncode, 0, ap1.stdout + ap1.stderr)
        self.assertIn("主干已前进 2 个提交，走三方合并", ap1.stdout, ap1.stdout)
        self.assertEqual((repo / "a.txt").read_text(), "v1\n")
        self.assertEqual((repo / "b.txt").read_text(), "c2\n")
        self.git(repo, "add", "a.txt")
        self.git(repo, "commit", "-qm", "land r-head")

        # Test dirty overlap vs unrelated dirty file!
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo 'line1\nv2' > a.txt")
        st2 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "r-dirty", "edit a"))
        self.wait_premerge_status(Path(st2["dir"]), {"ready"})

        # Unrelated dirty file does NOT block fast-forward!
        (repo / "unrelated.txt").write_text("local-wip\n")
        # Dirty on `a.txt` (overlapping `changedPaths`) DOES force 3-way fallback!
        (repo / "a.txt").write_text("v1\nlocal-extra\n")
        ap2 = self.cli("apply", "--json", st2["run"], cwd=repo)
        # Either conflicts or cleanly 3-way merges, either way MUST NOT fast-forward!
        ap2_doc = self.outcome(ap2)
        self.assertFalse(ap2_doc["fastForward"]["hit"], ap2_doc)
        self.assertEqual(ap2_doc["fastForward"]["missCode"], "dirty_overlap")
        self.assertIn("未提交修改", ap2_doc["fastForward"]["reason"])
        # Restore a.txt clean, keep unrelated.txt dirty -> MUST hit fast-forward!
        self.git(repo, "checkout", "--", "a.txt")
        ap2_ok = self.cli("apply", "--json", st2["run"], cwd=repo)
        self.assertEqual(ap2_ok.returncode, 0, ap2_ok.stdout + ap2_ok.stderr)
        ap2_ok_doc = self.outcome(ap2_ok)
        self.assertTrue(ap2_ok_doc["fastForward"]["hit"], ap2_ok_doc)
        self.assertEqual((repo / "a.txt").read_text(), "line1\nv2\n")
        self.assertEqual((repo / "unrelated.txt").read_text(), "local-wip\n")

    # ------------------------------------------------------------------
    # 5A. Stress 1: Worker running while controller commits on source repo
    # ------------------------------------------------------------------
    def test_stress_race1_controller_commits_while_worker_running(self):
        repo = self.repo({
            ".delegate.json": json.dumps({"accept": "true"}),
            "a.txt": "v0\n",
            "main.txt": "m0\n",
        })
        pm_gate = self.work / "race1-premerge.gate"
        self.addCleanup(lambda: pm_gate.touch())
        self.env["DELEGATE_TEST_PREMERGE_GATE"] = str(pm_gate)
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo a1 > a.txt")
        st = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "race-commit", "edit a"))
        run_dir = Path(st["dir"])

        # Wait until worker is actively paused at the gate.
        self.wait_premerge_status(run_dir, {"running"})

        # Controller commits while worker is mid-flight, then releases gate!
        (repo / "main.txt").write_text("m1\n")
        self.git(repo, "add", "main.txt")
        self.git(repo, "commit", "-qm", "controller commit mid-premerge")
        head1 = self.git(repo, "rev-parse", "HEAD")
        pm_gate.touch()

        # Worker must detect HEAD moved and re-premerge onto head1!
        self.wait_json(
            run_dir / "premerge.json",
            lambda d: d.get("baseHead") == head1 and d.get("status") == "ready",
            "worker re-premerges onto head1 after mid-flight commit",
            timeout=15.0,
        )
        # Source working tree was NEVER touched by worker!
        self.assertEqual(self.git(repo, "status", "--porcelain"), "")
        self.assertEqual((repo / "main.txt").read_text(), "m1\n")
        self.assertEqual((repo / "a.txt").read_text(), "v0\n")

        # Fast-forward apply succeeds cleanly on head1!
        self.env.pop("DELEGATE_TEST_PREMERGE_GATE", None)
        ap = self.cli("apply", "--json", st["run"], cwd=repo)
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)
        self.assertTrue(self.outcome(ap)["fastForward"]["hit"])
        self.assertEqual((repo / "a.txt").read_text(), "a1\n")
        self.assertEqual((repo / "main.txt").read_text(), "m1\n")

    # ------------------------------------------------------------------
    # 5B. Stress 2: Two runs delivered simultaneously (LOCK_NB + wake handoff)
    # ------------------------------------------------------------------
    def test_stress_race2_two_runs_delivered_simultaneously(self):
        repo = self.repo({
            ".delegate.json": json.dumps({"accept": "true"}),
            "f1.txt": "base1\n",
            "f2.txt": "base2\n",
        })
        self.env["DELEGATE_TEST_PREMERGE_DELAY"] = "0.25"
        # Start two runs concurrently with different file edits via env-driven fake pi.
        pi = self.bin / "pi"
        pi.write_text(
            f"#!{sys.executable}\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "args = sys.argv[1:]\n"
            "if args == ['--version']: print('fake 1'); sys.exit(0)\n"
            "if len(args) >= 2 and args[1] == '--list-models':\n"
            "    print('provider model context max-out thinking images')\n"
            "    print('p m 1M 1K yes yes'); sys.exit(0)\n"
            "prompt = sys.stdin.read()\n"
            "if 'EDIT_F1' in prompt:\n"
            "    Path('f1.txt').write_text('new1\\n')\n"
            "else:\n"
            "    Path('f2.txt').write_text('new2\\n')\n"
            "print(json.dumps({'type':'message_end','message':{'role':'assistant','stopReason':'stop','model':'p/m','usage':{'input':1,'output':1,'cacheRead':0},'content':[{'type':'text','text':'done'}]}}), flush=True)\n"
            "print(json.dumps({'type':'agent_settled'}), flush=True)\n"
        )
        pi.chmod(0o755)

        r1 = self.outcome(self.cli("start", "--worktree", "--workdir", repo, "--name", "sim-1", "EDIT_F1"))
        r2 = self.outcome(self.cli("start", "--worktree", "--workdir", repo, "--name", "sim-2", "EDIT_F2"))
        w = self.cli("wait", "--all", "--no-result", r1["run"], r2["run"], cwd=repo)
        self.assertEqual(w.returncode, 0, w.stderr)

        # BOTH runs must reach `ready` (zero lost wakeups under LOCK_NB contention)!
        pm1 = self.wait_premerge_status(Path(r1["dir"]), {"ready"}, timeout=15.0)
        pm2 = self.wait_premerge_status(Path(r2["dir"]), {"ready"}, timeout=15.0)
        self.assertEqual(pm1["changedPaths"], ["f1.txt"])
        self.assertEqual(pm2["changedPaths"], ["f2.txt"])

        # Apply both sequentially (disjoint files -> both fast-forward cleanly!)
        self.env.pop("DELEGATE_TEST_PREMERGE_DELAY", None)
        ap1 = self.cli("apply", "--json", r1["run"], cwd=repo)
        ap2 = self.cli("apply", "--json", r2["run"], cwd=repo)
        self.assertEqual(ap1.returncode, 0, ap1.stdout + ap1.stderr)
        self.assertEqual(ap2.returncode, 0, ap2.stdout + ap2.stderr)
        self.assertTrue(self.outcome(ap1)["fastForward"]["hit"])
        self.assertTrue(self.outcome(ap2)["fastForward"]["hit"])
        self.assertEqual((repo / "f1.txt").read_text(), "new1\n")
        self.assertEqual((repo / "f2.txt").read_text(), "new2\n")

    # ------------------------------------------------------------------
    # 5C. Stress 3: `apply` runs while worker is actively premerging
    # ------------------------------------------------------------------
    def test_stress_race3_apply_while_worker_running_never_blocks_or_corrupts(self):
        repo = self.repo({
            ".delegate.json": json.dumps({"accept": "true"}),
            "x.txt": "v0\n",
        })
        pm_gate = self.work / "race3-premerge.gate"
        self.addCleanup(lambda: pm_gate.touch())
        self.env["DELEGATE_TEST_PREMERGE_GATE"] = str(pm_gate)
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo x1 > x.txt")
        st = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "race-apply", "edit x"))
        run_dir = Path(st["dir"])

        # Confirm worker is actively paused at the gate right now.
        pm_running = self.wait_premerge_status(run_dir, {"running"})
        worker_pid = pm_running.get("workerPid", 0)
        self.assertGreater(worker_pid, 0)

        # Immediately run `apply` while `pm_gate` is still untouched!
        # Must NOT wait for the gated worker; must yield/fall back cleanly to 3-way merge.
        ap = self.cli("apply", "--json", st["run"], cwd=repo, timeout=15)
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)
        ap_doc = self.outcome(ap)
        self.assertFalse(ap_doc["fastForward"]["hit"])
        self.assertEqual(ap_doc["fastForward"]["missCode"], "premerge_running")
        self.assertEqual((repo / "x.txt").read_text(), "x1\n")

        # Modify x.txt in source after apply to ensure yielding worker never overwrites source!
        (repo / "x.txt").write_text("x1-controller-edit\n")
        # Worker must yield and exit cleanly without `pm_gate` ever being touched.
        self.wait_gone(worker_pid, "worker exits cleanly", timeout=10.0)
        self.assertEqual((repo / "x.txt").read_text(), "x1-controller-edit\n")

    # ------------------------------------------------------------------
    # 5D. Stress 4: Worker killed with SIGKILL mid-run -> fail-closed + self-heal
    # ------------------------------------------------------------------
    def test_stress_race4_worker_sigkill_fail_closed_and_recovery(self):
        repo = self.repo({
            ".delegate.json": json.dumps({"accept": "true"}),
            "k1.txt": "v0\n",
            "k2.txt": "v0\n",
        })
        pm_gate1 = self.work / "race4-premerge1.gate"
        pm_gate2 = self.work / "race4-premerge2.gate"
        self.addCleanup(lambda: (pm_gate1.touch(), pm_gate2.touch()))
        self.env["DELEGATE_TEST_PREMERGE_GATE"] = str(pm_gate1)
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo k1-new > k1.txt")
        st1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "kill-1", "edit k1"))
        run1_dir = Path(st1["dir"])

        pm1 = self.wait_premerge_status(run1_dir, {"running"})
        wpid = int(pm1["workerPid"])
        self.assertGreater(wpid, 0)

        # SIGKILL worker while paused mid-run!
        os.kill(wpid, signal.SIGKILL)
        self.wait_gone(wpid, "killed worker gone", timeout=5.0)

        # Source working tree must be completely intact!
        self.assertEqual(self.git(repo, "status", "--porcelain"), "")
        self.assertEqual((repo / "k1.txt").read_text(), "v0\n")
        # `premerge.json` must remain valid JSON (atomic write).
        self.assertIsInstance(json.loads((run1_dir / "premerge.json").read_text()), dict)

        # Case A: `apply` right after SIGKILL falls back cleanly to 3-way merge!
        self.env.pop("DELEGATE_TEST_PREMERGE_GATE", None)
        ap1 = self.cli("apply", "--json", st1["run"], cwd=repo)
        self.assertEqual(ap1.returncode, 0, ap1.stdout + ap1.stderr)
        self.assertFalse(self.outcome(ap1)["fastForward"]["hit"])
        self.assertEqual((repo / "k1.txt").read_text(), "k1-new\n")
        self.git(repo, "add", "k1.txt")
        self.git(repo, "commit", "-qm", "commit k1")

        # Case B: Second run SIGKILLed, then `status` detects dead workerPid and restarts worker -> `快进 ✓`!
        self.env["DELEGATE_TEST_PREMERGE_GATE"] = str(pm_gate2)
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo k2-new > k2.txt")
        st2 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "kill-2", "edit k2"))
        run2_dir = Path(st2["dir"])
        pm2 = self.wait_premerge_status(run2_dir, {"running"})
        wpid2 = int(pm2["workerPid"])
        os.kill(wpid2, signal.SIGKILL)
        self.wait_gone(wpid2, "killed worker2 gone", timeout=5.0)

        # Remove gate; calling `status` detects dead workerPid and wakes fresh `_merge-worker`!
        self.env.pop("DELEGATE_TEST_PREMERGE_GATE", None)
        _ = self.cli("status", st2["run"], human=True, cwd=repo)
        self.wait_premerge_status(run2_dir, {"ready"}, timeout=15.0)
        ap2 = self.cli("apply", "--json", st2["run"], cwd=repo)
        self.assertEqual(ap2.returncode, 0, ap2.stdout + ap2.stderr)
        self.assertTrue(self.outcome(ap2)["fastForward"]["hit"])
        self.assertEqual((repo / "k2.txt").read_text(), "k2-new\n")

    # ------------------------------------------------------------------
    # 6. Fast-forward apply MUST run the exact same accept_validity checks
    #    as non-fast-forward apply, and concurrent worker shells must never
    #    SIGTERM `apply`'s generate/verify shells.
    # ------------------------------------------------------------------
    def _make_repo(self, name: str, files: dict[str, str]) -> Path:
        repo = self.work / name
        repo.mkdir(parents=True, exist_ok=True)
        git = ["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t"]
        subprocess.run(git[:3] + ["init", "-q"], check=True)
        for rel, text in files.items():
            (repo / rel).parent.mkdir(parents=True, exist_ok=True)
            (repo / rel).write_text(text)
        subprocess.run(git + ["add", "-A"], check=True)
        subprocess.run(git + ["commit", "-qm", "init"], check=True)
        return repo

    @staticmethod
    def _validity_sig(doc: dict) -> tuple:
        return (
            "acceptStillValid" in doc,
            doc.get("acceptStillValid"),
            doc.get("acceptValidityScope"),
            doc.get("acceptValidityReason"),
        )

    def _run_validity_matrix(self, prefix: str, premerge_ready: bool) -> dict[str, tuple]:
        if premerge_ready:
            self.env.pop("DELEGATE_PREMERGE", None)
        else:
            self.env["DELEGATE_PREMERGE"] = "0"

        results: dict[str, tuple] = {}

        # Case 1: clean valid reuse, legacy snapshot, large untracked, write-tree failure, dry-run
        repo = self._make_repo(f"{prefix}-basic", {"a.txt": "old\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo new > a.txt")
        st = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "--name", f"{prefix}-basic", "task")
        )
        run_dir = Path(st["dir"])
        if premerge_ready:
            self.wait_premerge_status(run_dir, {"ready"})

        def reset_clean():
            self.git(repo, "checkout", "--", "a.txt")
            for marker in (".applied", ".sync-base"):
                p = run_dir / marker
                if p.exists():
                    p.unlink()

        # 1a. dry-run
        dry_doc = self.outcome(self.cli("apply", "--json", "--keep", "--dry-run", st["run"]))
        self.assertEqual(dry_doc["fastForward"]["hit"], premerge_ready, dry_doc)
        results["dry_run"] = self._validity_sig(dry_doc)

        # 1b. clean valid apply
        ok_doc = self.outcome(self.cli("apply", "--json", "--keep", st["run"]))
        self.assertEqual(ok_doc["fastForward"]["hit"], premerge_ready, ok_doc)
        self.assertEqual(ok_doc["verify"], {"skipped": "acceptStillValid"})
        results["clean_valid"] = self._validity_sig(ok_doc)

        # 1c. legacy / incomplete acceptance snapshot
        reset_clean()
        summary = json.loads((run_dir / "summary.json").read_text())
        legacy = dict(summary)
        legacy["accept"] = {"ok": True, "exitCode": 0, "command": "true"}
        (run_dir / "summary.json").write_text(json.dumps(legacy))
        leg_doc = self.outcome(self.cli("apply", "--json", "--keep", st["run"]))
        self.assertEqual(leg_doc["fastForward"]["hit"], premerge_ready, leg_doc)
        results["legacy_snapshot"] = self._validity_sig(leg_doc)
        (run_dir / "summary.json").write_text(json.dumps(summary))

        # 1d. large untracked file in source
        reset_clean()
        (repo / "large-untracked").write_bytes(b"x" * 2097153)
        large_doc = self.outcome(self.cli("apply", "--json", "--keep", st["run"]))
        self.assertEqual(large_doc["fastForward"]["hit"], premerge_ready, large_doc)
        results["large_untracked"] = self._validity_sig(large_doc)
        (repo / "large-untracked").unlink()

        # 1e. source snapshot failure (`git write-tree` fails)
        reset_clean()
        wrapper = self.bin / "git"
        real_git = shutil.which("git", path=os.environ["PATH"])
        wrapper.write_text(
            f'#!/bin/sh\ncase "$*" in *"write-tree"*) exit 1;; esac\nexec {shlex.quote(real_git)} "$@"\n'
        )
        wrapper.chmod(0o755)
        try:
            fail_doc = self.outcome(self.cli("apply", "--json", "--keep", st["run"]))
            self.assertEqual(fail_doc["fastForward"]["hit"], premerge_ready, fail_doc)
            results["snapshot_failed"] = self._validity_sig(fail_doc)
        finally:
            wrapper.unlink()

        # Case 2: generated regeneration changes result relative to accepted tree
        gen_repo = self._make_repo(
            f"{prefix}-gen",
            {
                ".delegate.json": json.dumps(
                    {"generated": {"paths": ["gen/"], "command": "cp a.txt gen/out.txt"}}
                ),
                "a.txt": "old\n",
                "gen/out.txt": "old\n",
            },
        )
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre="echo next > a.txt; echo different > gen/out.txt",
        )
        st_gen = self.outcome(
            self.cli("run", "--worktree", "--workdir", gen_repo, "--accept", "true", "--name", f"{prefix}-gen", "task")
        )
        if premerge_ready:
            self.wait_premerge_status(Path(st_gen["dir"]), {"ready"})
        gen_doc = self.outcome(self.cli("apply", "--json", "--keep", st_gen["run"]))
        self.assertEqual(gen_doc["fastForward"]["hit"], premerge_ready, gen_doc)
        results["generated_changed"] = self._validity_sig(gen_doc)

        # Case 3: acceptance command mutated worktree
        mut_repo = self._make_repo(f"{prefix}-mut", {"a.txt": "old\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo again > a.txt")
        st_mut = self.outcome(
            self.cli(
                "run", "--worktree", "--workdir", mut_repo,
                "--accept", "echo accept-edit >> a.txt",
                "--name", f"{prefix}-mut", "task",
            )
        )
        if premerge_ready:
            self.wait_premerge_status(Path(st_mut["dir"]), {"ready", "failed"})
        mut_doc = self.outcome(self.cli("apply", "--json", "--keep", st_mut["run"]))
        results["accept_mutated"] = self._validity_sig(mut_doc)

        return results

    def test_apply_accept_validity_matches_when_premerge_ready(self):
        ready = self._run_validity_matrix("ready", premerge_ready=True)
        disabled = self._run_validity_matrix("disabled", premerge_ready=False)

        self.assertEqual(ready, disabled)
        self.assertFalse(ready["dry_run"][0])
        self.assertIn("dry run", ready["dry_run"][3])
        self.assertEqual(ready["clean_valid"][:2], (True, True))
        self.assertFalse(ready["legacy_snapshot"][0])
        self.assertIn("incomplete acceptance", ready["legacy_snapshot"][3])
        self.assertFalse(ready["large_untracked"][0])
        self.assertIn("large untracked", ready["large_untracked"][3])
        self.assertFalse(ready["snapshot_failed"][0])
        self.assertIn("snapshot failed", ready["snapshot_failed"][3])
        self.assertEqual(ready["generated_changed"][:2], (True, False))
        self.assertEqual(ready["accept_mutated"][:2], (True, False))
        self.assertIn("acceptance changed", ready["accept_mutated"][3])

    def test_apply_accept_validity_matches_when_premerge_not_ready(self):
        # 1. Premerge actively running in parallel with `apply` during `generated` regeneration:
        #    `_merge-worker`'s shell cleanup MUST NOT SIGTERM `apply`'s generate command, and
        #    the validity conclusion must match the premerge-ready path identically.
        repo_live = self._make_repo(
            "notready-gen-live",
            {
                ".delegate.json": json.dumps(
                    {"generated": {"paths": ["gen/"], "command": "sleep 0.15; cp a.txt gen/out.txt"}}
                ),
                "a.txt": "old\n",
                "gen/out.txt": "old\n",
            },
        )
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre="echo new > a.txt; cp a.txt gen/out.txt",
        )
        self.env.pop("DELEGATE_PREMERGE", None)
        pm_gate = self.work / "notready-live-premerge.gate"
        self.addCleanup(lambda: pm_gate.touch())
        self.env["DELEGATE_TEST_PREMERGE_GATE"] = str(pm_gate)
        st_live = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo_live, "--accept", "true", "--name", "nr-live", "task")
        )
        pm_running = self.wait_premerge_status(Path(st_live["dir"]), {"running"})
        self.env.pop("DELEGATE_TEST_PREMERGE_GATE", None)
        ap_live = self.cli("apply", "--json", "--keep", st_live["run"], cwd=repo_live)
        self.assertEqual(ap_live.returncode, 0, ap_live.stdout + ap_live.stderr)
        live_doc = self.outcome(ap_live)
        self.assertTrue(live_doc["apply"]["ok"], live_doc)
        self.assertFalse(live_doc["fastForward"]["hit"], live_doc)
        self.assertEqual(live_doc["fastForward"]["missCode"], "premerge_running", live_doc)
        self.assertIs(live_doc["acceptStillValid"], True, live_doc)
        self.assertFalse((Path(st_live["dir"]) / ".generate-pending").exists())
        self.wait_gone(int(pm_running["workerPid"]), "yielding premerge worker exited", timeout=10.0)

        # 2. Same scenario after premerge finishes -> fast-forward hit, identical validity conclusion!
        repo_ready = self._make_repo(
            "notready-gen-ready",
            {
                ".delegate.json": json.dumps(
                    {"generated": {"paths": ["gen/"], "command": "cp a.txt gen/out.txt"}}
                ),
                "a.txt": "old\n",
                "gen/out.txt": "old\n",
            },
        )
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre="echo new > a.txt; cp a.txt gen/out.txt",
        )
        st_ready = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo_ready, "--accept", "true", "--name", "nr-ready", "task")
        )
        self.wait_premerge_status(Path(st_ready["dir"]), {"ready"})
        ap_ready = self.cli("apply", "--json", "--keep", st_ready["run"], cwd=repo_ready)
        self.assertEqual(ap_ready.returncode, 0, ap_ready.stdout + ap_ready.stderr)
        ready_doc = self.outcome(ap_ready)
        self.assertTrue(ready_doc["fastForward"]["hit"], ready_doc)
        self.assertEqual(self._validity_sig(live_doc), self._validity_sig(ready_doc))

    def test_apply_lock_blocks_gc_worker_during_verify_and_preserves_applied(self):
        """Deterministic regression for W213: while `apply` is paused inside post-merge
        `verify`, running `_gc-worker` must NOT kill `verify`, remove `verify.log`/run files,
        write `.applied-cleaned`, or overwrite `.applied` with `appliedBy: detected`; once
        `apply` completes and releases `apply.lock`, reclamation succeeds normally.
        """
        gate_armed = self.work / "verify.armed"
        verify_started = self.work / "verify.started"
        verify_release = self.work / "verify.release"
        self.addCleanup(lambda: verify_release.touch())

        verify_cmd = (
            f"if [ -f {shlex.quote(str(gate_armed))} ]; then "
            f"touch {shlex.quote(str(verify_started))}; "
            f"while [ ! -f {shlex.quote(str(verify_release))} ]; do sleep 0.05; done; "
            f"fi"
        )
        repo = self.repo({
            ".delegate.json": json.dumps({"accept": verify_cmd}),
            "f1.txt": "base1\n",
            "other.txt": "orig\n",
        })
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED], pre="echo new1 > f1.txt")
        st = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "gc-race", "edit f1")
        )
        run_dir = Path(st["dir"])
        self.wait_premerge_status(run_dir, {"ready"}, timeout=15.0)

        # Make an unrelated dirty edit so `apply` hits fast-forward on f1.txt, sees
        # `acceptStillValid: false`, and enters post-merge `verify` (armed by `gate_armed`).
        (repo / "other.txt").write_text("dirty-unrelated\n")
        gate_armed.touch()

        proc = subprocess.Popen(
            [str(fixtures.DELEGATE), "apply", "--json", "--keep", st["run"]],
            cwd=repo,
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            self.until(
                lambda: verify_started.is_file(),
                "apply entered post-merge verify",
                timeout=15.0,
            )
            self.assertIsNone(proc.poll(), "apply must still be running inside verify")
            self.assertTrue((run_dir / "apply.lock").is_file())
            applied_before = json.loads((run_dir / ".applied").read_text())
            self.assertNotEqual(applied_before.get("appliedBy"), "detected")
            self.assertTrue(applied_before.get("fastForward"))

            # Run `_gc-worker` synchronously while `apply` holds `<run>/apply.lock` inside `verify`.
            gc_during = self.cli("_gc-worker", cwd=repo)
            self.assertEqual(gc_during.returncode, 0, gc_during.stdout + gc_during.stderr)

            # `_gc-worker` must NOT touch the in-flight run, its `.applied`, or kill `verify`!
            self.assertIsNone(proc.poll(), "_gc-worker must not kill apply/verify")
            self.assertTrue(run_dir.is_dir())
            self.assertFalse((run_dir / ".applied-cleaned").exists())
            self.assertTrue((run_dir / "verify.log").is_file())
            applied_during = json.loads((run_dir / ".applied").read_text())
            self.assertEqual(applied_during, applied_before)

            # Release `verify` -> `apply` completes cleanly with `verify.ok == True`!
            verify_release.touch()
            out, err = proc.communicate(timeout=15.0)
            self.assertEqual(proc.returncode, 0, out + err)
            doc = json.loads([ln for ln in out.splitlines() if ln.strip().startswith("{")][-1])
            self.assertTrue(doc["fastForward"]["hit"], doc)
            self.assertIs(doc["acceptStillValid"], False, doc)
            self.assertTrue(doc["verify"]["ok"], doc)
        finally:
            verify_release.touch()
            if proc.poll() is None:
                proc.kill()
                proc.wait(timeout=5.0)

        # After `apply` exits and releases `apply.lock`, `_gc-worker` reclaims normally
        # and keeps `apply.lock` intact on disk.
        gc_after = self.cli("_gc-worker", cwd=repo)
        self.assertEqual(gc_after.returncode, 0, gc_after.stdout + gc_after.stderr)
        self.assertTrue((run_dir / ".applied-cleaned").is_file())
        self.assertTrue((run_dir / "apply.lock").is_file())


if __name__ == "__main__":
    unittest.main()
