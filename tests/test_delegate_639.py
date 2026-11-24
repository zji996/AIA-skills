"""6.39.0 regressions (delegate audit fixes P0-1, P0-2, P0-3, P1-5, P1-6, P2-7, P2-8, P2-10):
- P0-1: `controllerReviewPaths` only escalates when verdict != REWORK (no longer hijacks automatic
  rework as in `star-selfheal`); review card renders `originalVerdict` + `round-N-verdict.md` pointer.
- P0-2: Stop hook checks `<run>/waiter.lock` (via `/proc/locks`), scopes cross-repo checks to this
  session's `caller` or the current repo, and recognizes bare `delegate wait` / `wait --any`
  (regression reproducing the 11-23 15:42-15:57 `norvane` 102-block incident).
- P0-3: `apply` cancels same-run `_merge-worker` and `premerge` checks `.applied`/`apply.lock` before
  every phase; `apply` isolates `CARGO_TARGET_DIR` when `<source>/target/.cargo-lock` is held;
  `apply` records `startedAt`/`finishedAt`/`elapsedMs` in JSON, `events.jsonl`, and `hook-actions.jsonl`.
- P1-5: Acceptance failure attribution cross-references `accept.log` error paths against this run's
  changes and labels cards with `报错文件不在本 run 改动中（疑似主干已有失败）` + log line pointer + next steps.
- P1-6: `wait --since` fails fast on nonexistent runs with similar-prefix suggestions; filtered mode
  checks only target run activity.
- P2-7: `apply` conflict output and JSON `next` suggest `delegate reply --sync=agent <run> "rebase 到最新主干并解决冲突（<files>）"`.
- P2-8: Card first line displays short base SHA (`基线 <short>`) and marks `已 rebase：<old>→<new>` when advanced.
- P2-10: `source_guard` allows `2>&1` / `>&2` / `2>&-` fd redirects instead of refusing `<repo>/1`.
"""
import fcntl
import json
import os
import shlex
import signal
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures

ROOT = Path(__file__).resolve().parent.parent
STOP_HOOK = ROOT / "skills/delegate/hooks/claude-code-stop.py"


class Delegate639Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
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

    def install_scripted_pi(self, script_body: str) -> Path:
        log_file = self.work / "scripted-pi-6362.jsonl"
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
    # P0-1: controllerReviewPaths must NOT hijack REWORK; card shows originalVerdict + verdictFile
    # ------------------------------------------------------------------
    def test_p0_1_controller_review_paths_does_not_hijack_rework_and_card_shows_original_verdict(self):
        repo = self.make_repo(
            "repo-star-selfheal",
            {
                "prompts/system.md": "v0\n",
                ".delegate.json": json.dumps(
                    {
                        "review": {"tier": "review"},
                        "controllerReviewPaths": ["prompts/**"],
                    }
                ),
            },
        )
        self.install_scripted_pi(
            "if call_idx == 1:\n"
            "    Path('prompts/system.md').write_text('v1-buggy\\n')\n"
            "    emit_answer('round 1 initial change')\n"
            "elif call_idx == 2:\n"
            "    emit_answer('REWORK\\n\\n发现阻塞缺陷：缺少兜底处理。')\n"
            "elif call_idx == 3:\n"
            "    Path('prompts/system.md').write_text('v2-fixed\\n')\n"
            "    emit_answer('round 2 fixed blocking bug')\n"
            "else:\n"
            "    emit_answer('PASS\\n\\n阻塞缺陷已修复，可交主控签字。')\n"
        )
        st = self.outcome(
            self.cli(
                "run",
                "--workdir",
                repo,
                "--worktree",
                "--name",
                "star-selfheal",
                "--accept",
                "true",
                "update selfheal prompt",
            )
        )
        self.assertEqual(st["state"], "delivered", st)
        run_dir = Path(st["dir"])
        rev = json.loads((run_dir / "review.json").read_text(encoding="utf-8"))
        # Must have entered automatic rework on round 1 instead of being hijacked!
        self.assertEqual(rev["round"], 2, rev)
        self.assertEqual(rev["reworkRounds"], 1, rev)
        self.assertEqual(rev["verdict"], "NEEDS_CONTROLLER", rev)
        self.assertEqual(rev["originalVerdict"], "PASS", rev)
        self.assertEqual(rev["upgradedBy"], "controllerReviewPaths", rev)
        self.assertTrue(rev["verdictFile"].endswith("round-2-verdict.md"), rev)
        self.assertEqual(rev["rounds"][0]["verdict"], "REWORK", rev["rounds"])
        self.assertNotIn("upgradedBy", rev["rounds"][0])
        self.assertTrue((run_dir / "review/round-1-verdict.md").is_file())
        self.assertTrue((run_dir / "review/round-2-verdict.md").is_file())

        # Card shows originalVerdict, points back to round-2-verdict.md, AND preserves W051 round result pointers + base SHA.
        card_json = json.loads(self.cli("card", st["run"], "--json", cwd=repo, human=True).stdout)
        self.assertEqual(card_json["review"]["originalVerdict"], "PASS", card_json["review"])
        self.assertEqual(len(card_json["author"]["rounds"]), 2, card_json["author"])
        card_human = self.cli("card", st["run"], cwd=repo, human=True).stdout
        self.assertIn("基线 ", card_human.splitlines()[0])
        self.assertIn("原判 PASS", card_human)
        self.assertIn("controllerReviewPaths", card_human)
        self.assertIn("round-2-verdict.md", card_human)
        self.assertIn("各轮原文：", card_human)
        self.assertIn("round-1-result.md", card_human)
        self.assertIn("round-2-result.md", card_human)

        # Legacy record with originalVerdict=REWORK (pre-fix star-selfheal) also surfaces clearly in card.
        rev["originalVerdict"] = "REWORK"
        rev["verdictFile"] = str(run_dir / "review/round-1-verdict.md")
        (run_dir / "review.json").write_text(json.dumps(rev), encoding="utf-8")
        legacy_card = self.cli("card", st["run"], cwd=repo, human=True).stdout
        self.assertIn("原判 REWORK", legacy_card)
        self.assertIn("round-1-verdict.md", legacy_card)

    # ------------------------------------------------------------------
    # P0-2: Stop hook waiter.lock + caller scope + bare `wait` / `wait --any`
    #       (reproducing 11-23 15:42-15:57 norvane 102-block incident)
    # ------------------------------------------------------------------
    def test_p0_2_stop_hook_waiter_lock_caller_scope_and_norvane_incident_regression(self):
        norvane = self.make_repo("norvane", {"a.txt": "v0\n"})
        other_repo = self.make_repo("other-workspace", {"b.txt": "v0\n"})
        no_runs_env = {k: v for k, v in self.env.items() if k != "DELEGATE_RUNS"}
        norvane_env = {**no_runs_env, "CLAUDE_CODE_SESSION_ID": "sess-norvane"}
        other_env = {**no_runs_env, "CLAUDE_CODE_SESSION_ID": "sess-other"}

        def run_del(cwd: Path, env: dict, *args: str):
            return subprocess.run(
                [str(fixtures.DELEGATE), *args],
                cwd=cwd,
                capture_output=True,
                text=True,
                env=env,
                timeout=30,
            )

        def run_hook(cwd: Path, env: dict, session_id: str, extra_env: dict | None = None):
            henv = {
                **env,
                "DELEGATE_BIN": str(fixtures.DELEGATE),
                "DELEGATE_STOP_HOOK_WAITERS": "",
                "DELEGATE_STOP_HOOK_PROCS": "",
            }
            if extra_env:
                henv.update(extra_env)
            res = subprocess.run(
                [sys.executable, str(STOP_HOOK)],
                input=json.dumps({"cwd": str(cwd), "session_id": session_id, "stop_hook_active": False}),
                capture_output=True,
                text=True,
                env=henv,
                timeout=10,
            )
            self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
            out = res.stdout.strip()
            return json.loads(out) if out else None

        gate = self.work / "norvane-live-gate"
        self.fake_pi(
            [fixtures.answer("live done"), fixtures.SETTLED],
            pre=f"while [ ! -f {shlex.quote(str(gate))} ]; do sleep 0.05; done",
        )
        started = []
        for nm in ("body-stream", "star-selfheal", "body-motion-svc"):
            r = self.outcome(run_del(norvane, norvane_env, "start", "--json", "--read-only", "--name", nm, "work"))
            started.append(r)

        def cleanup_live():
            gate.touch()
            for r in started:
                run_del(norvane, norvane_env, "stop", r["run"])

        self.addCleanup(cleanup_live)

        # 1) Regression for 11-23 15:42-15:57: another session in `other_repo` MUST NOT be blocked by norvane's 3 runs!
        self.assertIsNone(run_hook(other_repo, other_env, "sess-other"))

        # 2) Same session (`sess-norvane`) in `other_repo` with NO waiter SHOULD be blocked (it owns those runs).
        blk_same_caller = run_hook(other_repo, norvane_env, "sess-norvane")
        self.assertIsNotNone(blk_same_caller)
        self.assertEqual(blk_same_caller["decision"], "block")
        self.assertIn("【norvane】", blk_same_caller["reason"])

        # 3) Start a real `delegate wait` in `norvane` -> holds `<run>/waiter.lock`!
        waiter_proc = subprocess.Popen(
            [str(fixtures.DELEGATE), "wait"],
            cwd=norvane,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=norvane_env,
        )
        self.addCleanup(lambda: (waiter_proc.terminate(), waiter_proc.wait(timeout=5)) if waiter_proc.poll() is None else None)
        self.until(
            lambda: all((Path(r["dir"]) / "waiter.lock").is_file() for r in started),
            "waiter.lock created for all 3 norvane runs",
            timeout=10.0,
        )

        # With real waiter.lock held (and DELEGATE_STOP_HOOK_WAITERS="" / PROCS="" so ONLY waiter.lock is checked!):
        # BOTH `norvane` AND `other_repo` (same caller!) must PASS!
        self.assertIsNone(run_hook(norvane, norvane_env, "sess-norvane"))
        self.assertIsNone(run_hook(other_repo, norvane_env, "sess-norvane"))

        # Stop the real waiter; verify `norvane` blocks again without a waiter.
        waiter_proc.terminate()
        waiter_proc.wait(timeout=5)
        blk_cur = run_hook(norvane, norvane_env, "sess-norvane")
        self.assertIsNotNone(blk_cur)
        self.assertIn("body-stream", blk_cur["reason"])

        # 4) Bare `delegate wait` and `delegate wait --any` must both be recognized!
        for wcmd in ("delegate wait", "delegate wait --any", f"{fixtures.DELEGATE} wait --any"):
            self.assertIsNone(
                run_hook(norvane, norvane_env, "sess-norvane", extra_env={"DELEGATE_STOP_HOOK_WAITERS": wcmd}),
                f"expected {wcmd!r} to cover current repo runs",
            )

    # ------------------------------------------------------------------
    # P0-3: apply/premerge no double-run + target avoidance + apply elapsedMs
    # ------------------------------------------------------------------
    def test_p0_3_apply_premerge_no_double_run_target_isolation_and_timing(self):
        gen_count_file = self.work / "gen-invocations.log"
        pm_started_marker = self.work / "premerge-gen-started"
        pm_gate = self.work / "premerge-gen-gate"
        pm_finished_marker = self.work / "premerge-gen-finished"
        self.addCleanup(lambda: pm_gate.touch())
        repo = self.make_repo(
            "repo-apply-premerge",
            {
                ".delegate.json": json.dumps(
                    {
                        "generated": {
                            "paths": ["gen/"],
                            "inputs": ["src/"],
                            "command": (
                                "case \"$PWD\" in "
                                f"*/premerge/*) touch {shlex.quote(str(pm_started_marker))}; "
                                f"while [ ! -f {shlex.quote(str(pm_gate))} ]; do sleep .05; done; "
                                f"touch {shlex.quote(str(pm_finished_marker))} ;; "
                                f"*) printf '%s|%s\\n' \"$PWD\" \"${{CARGO_TARGET_DIR:-}}\" >> {shlex.quote(str(gen_count_file))} ;; "
                                "esac; "
                                "mkdir -p gen && cp src/a.txt gen/out.txt"
                            ),
                        }
                    }
                ),
                "src/a.txt": "v0\n",
                "gen/out.txt": "v0\n",
            },
        )
        # Create stop-hook.jsonl so `hook::record_action_timed` logs to `hook-actions.jsonl`.
        sdir = self.state_dir()
        sdir.mkdir(parents=True, exist_ok=True)
        (sdir / "stop-hook.jsonl").write_text('{"ts":"2026-11-24T00:00:00Z","hook":"unmerged","runs":"x"}\n')

        # Hold a flock on `<repo>/target/.cargo-lock` to simulate concurrent post-commit / dev_fresh build!
        cargo_lock = repo / "target/.cargo-lock"
        cargo_lock.parent.mkdir(parents=True, exist_ok=True)
        lock_fh = cargo_lock.open("w")
        fcntl.flock(lock_fh.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.addCleanup(lambda: (fcntl.flock(lock_fh.fileno(), fcntl.LOCK_UN), lock_fh.close()))

        self.fake_pi([fixtures.answer("edited a"), fixtures.SETTLED], pre="echo v1 > src/a.txt; cp src/a.txt gen/out.txt")
        self.env.pop("DELEGATE_PREMERGE", None)
        st = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "--name", "pm-race", "edit a")
        )
        run_dir = Path(st["dir"])

        # Wait until `_merge-worker` is actively executing `premerge-generate` (mid-flight!).
        self.until(pm_started_marker.is_file, "premerge-generate started", timeout=10.0)
        pm_doc = self.wait_json(
            run_dir / "premerge.json",
            lambda d: d.get("status") == "running",
            "premerge worker entered premerge-generate",
            timeout=10.0,
        )
        worker_pid = pm_doc["workerPid"]

        # Apply immediately while worker is paused at `pm_gate` AND `<repo>/target/.cargo-lock` is held!
        ap = self.cli("apply", "--json", "--keep", st["run"], cwd=repo, timeout=15)
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)
        doc = self.outcome(ap)
        self.assertTrue(doc["apply"]["ok"], doc)
        timing = doc.get("applyTiming") or {}
        self.assertTrue(timing.get("premergeCancelled"), doc)
        self.assertTrue(timing.get("targetContended"), doc)
        self.assertTrue(timing.get("regenerateTargetIsolated"), doc)
        self.assertIn("apply-target", timing.get("regenerateTargetDir", ""))
        self.assertIsInstance(doc.get("elapsedMs"), int, doc)
        self.assertTrue(doc.get("startedAt"), doc)
        self.assertTrue(doc.get("finishedAt"), doc)
        self.assertIsInstance(timing.get("generateMs"), int, doc)

        # Worker must exit promptly and MUST NOT complete its cancelled `premerge-generate`!
        self.wait_gone(worker_pid, "cancelled premerge worker exited", timeout=5.0)
        self.assertFalse(pm_finished_marker.exists(), "cancelled premerge-generate must not run to completion")
        gen_lines = [ln for ln in gen_count_file.read_text().splitlines() if ln.strip()]
        self.assertEqual(len(gen_lines), 1, f"expected apply regenerate once, got: {gen_lines}")
        self.assertIn("apply-target", gen_lines[0], f"expected isolated CARGO_TARGET_DIR, got: {gen_lines[0]}")

        # Verify `events.jsonl` `applied` event and `hook-actions.jsonl` record `startedAt`/`finishedAt`/`elapsedMs`.
        evs = [json.loads(ln) for ln in (sdir / "events.jsonl").read_text().splitlines() if ln.strip()]
        applied_evs = [e for e in evs if e.get("kind") == "applied" and e.get("run") == st["run"]]
        self.assertTrue(applied_evs, evs)
        ap_pl = applied_evs[-1]["payload"]
        self.assertIsInstance(ap_pl.get("elapsedMs"), int, ap_pl)
        self.assertTrue(ap_pl.get("startedAt") and ap_pl.get("finishedAt"), ap_pl)

        actions = [json.loads(ln) for ln in (sdir / "hook-actions.jsonl").read_text().splitlines() if ln.strip()]
        apply_actions = [a for a in actions if a.get("action") == "apply" and st["run"] in a.get("runs", [])]
        self.assertTrue(apply_actions, actions)
        self.assertIsInstance(apply_actions[-1].get("elapsedMs"), int, apply_actions[-1])
        self.assertTrue(apply_actions[-1].get("startedAt") and apply_actions[-1].get("finishedAt"), apply_actions[-1])

    # ------------------------------------------------------------------
    # P1-5: Accept failure attribution distinguishes mainline failure vs own regression
    # ------------------------------------------------------------------
    def test_p1_5_accept_failure_attribution_distinguishes_mainline_vs_own_regression(self):
        repo = self.make_repo(
            "repo-accept-attr",
            {
                "apps/web/src/styles/agent-birth.css": ".a { color: red; }\n",
                "apps/web/src/scene/camera.ts": "export const cam = 1;\n",
            },
        )
        # Case A: Error in untouched CSS file -> suspected mainline failure!
        self.fake_pi([fixtures.answer("tuned camera"), fixtures.SETTLED], pre="echo 'export const cam = 2;' > apps/web/src/scene/camera.ts")
        r_main = self.outcome(
            self.cli(
                "run",
                "--worktree",
                "--workdir",
                repo,
                "--name",
                "camera-fix",
                "--accept",
                "echo 'apps/web/src/styles/agent-birth.css:14:3: Unknown word'; exit 1",
                "fix camera",
            )
        )
        self.assertEqual(r_main["state"], "rejected", r_main)
        card_a_json = json.loads(self.cli("card", r_main["run"], "--json", cwd=repo, human=True).stdout)
        attr_a = card_a_json["accept"].get("failureAttribution")
        self.assertIsNotNone(attr_a, card_a_json["accept"])
        self.assertTrue(attr_a["suspectedMainline"], attr_a)
        self.assertEqual(attr_a["outsideRunFiles"][0]["path"], "apps/web/src/styles/agent-birth.css")
        card_a_human = self.cli("card", r_main["run"], cwd=repo, human=True).stdout
        self.assertIn("报错文件不在本 run 改动中（疑似主干已有失败）", card_a_human)
        self.assertIn("apps/web/src/styles/agent-birth.css（日志：", card_a_human)
        self.assertIn("sed -n", card_a_human)
        self.assertIn("delegate lane", card_a_human)

        # Case B: Error in the run's own modified file -> NOT suspected mainline failure!
        self.fake_pi([fixtures.answer("broke camera"), fixtures.SETTLED], pre="echo 'export const cam = 3;' > apps/web/src/scene/camera.ts")
        r_own = self.outcome(
            self.cli(
                "run",
                "--worktree",
                "--workdir",
                repo,
                "--name",
                "camera-broken",
                "--accept",
                "echo 'apps/web/src/scene/camera.ts:1:1: error TS2345'; exit 1",
                "break camera",
            )
        )
        self.assertEqual(r_own["state"], "rejected", r_own)
        card_b_json = json.loads(self.cli("card", r_own["run"], "--json", cwd=repo, human=True).stdout)
        self.assertFalse(card_b_json["accept"]["failureAttribution"]["suspectedMainline"])
        card_b_human = self.cli("card", r_own["run"], cwd=repo, human=True).stdout
        self.assertNotIn("疑似主干已有失败", card_b_human)

    # ------------------------------------------------------------------
    # P1-6: wait --since fails fast on nonexistent run with suggestions & checks target activity only
    # ------------------------------------------------------------------
    def test_p1_6_wait_nonexistent_run_fails_fast_with_suggestions_and_filter_checks_target_only(self):
        repo = self.make_repo("repo-wait-filter", {"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("room done"), fixtures.SETTLED])
        r_room = self.outcome(self.cli("run", "--read-only", "--workdir", repo, "--name", "room-v1", "check room"))

        # Start an unrelated long-running task so machine has an active run.
        gate = self.work / "wait-filter-gate"
        self.fake_pi(
            [fixtures.answer("other done"), fixtures.SETTLED],
            pre=f"while [ ! -f {shlex.quote(str(gate))} ]; do sleep 0.05; done",
        )
        r_live = self.outcome(self.cli("start", "--read-only", "--workdir", repo, "--name", "other-live", "hold"))
        self.addCleanup(lambda: (gate.touch(), self.cli("stop", r_live["run"], cwd=repo)))

        # 1) Nonexistent run with similar slug -> immediate non-zero error with suggestion!
        t0 = time.perf_counter()
        bad = self.cli("wait", "--since", "0", "20261124-025333-room-v1", cwd=repo, human=True)
        self.assertNotEqual(bad.returncode, 0, bad.stdout + bad.stderr)
        self.assertLess(time.perf_counter() - t0, 3.0)
        self.assertIn("相近 run：", bad.stderr)
        self.assertIn(r_room["run"], bad.stderr)

        # 2) Filtered `wait --since <latest> <finished_run>` MUST return immediately despite `other-live` running!
        evs = [
            json.loads(ln)
            for ln in self.cli("events", "--json", cwd=repo, human=True).stdout.splitlines()
            if ln.strip()
        ]
        max_seq = max((e["seq"] for e in evs), default=0)
        t1 = time.perf_counter()
        fin = self.cli("wait", "--since", str(max_seq), r_room["run"], cwd=repo, human=True)
        self.assertEqual(fin.returncode, 0, fin.stdout + fin.stderr)
        self.assertLess(time.perf_counter() - t1, 2.0, "filtered wait hung on unrelated active run")

    # ------------------------------------------------------------------
    # P2-7: apply conflict suggests `delegate reply --sync=agent <run> "rebase 到最新主干并解决冲突（<files>）"`
    # ------------------------------------------------------------------
    def test_p2_7_apply_conflict_suggests_reply_sync_agent_rebase(self):
        repo = self.make_repo("repo-conflict", {"shared.txt": "line0\n"})
        self.fake_pi([fixtures.answer("edited shared"), fixtures.SETTLED], pre="echo colleague > shared.txt")
        r = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "conf-run", "--accept", "true", "edit shared")
        )
        # Concurrent committed edit on source checkout creates a real conflict.
        (repo / "shared.txt").write_text("controller-change\n", encoding="utf-8")
        self.git(repo, "add", "shared.txt")
        self.git(repo, "commit", "-qm", "controller-change")
        expected_cmd = f'delegate reply --sync=agent {r["run"]} "rebase 到最新主干并解决冲突（shared.txt）"'

        res_human = self.cli("apply", r["run"], cwd=repo, human=True)
        self.assertNotEqual(res_human.returncode, 0)
        self.assertIn(expected_cmd, res_human.stderr)
        self.assertIn(expected_cmd, res_human.stdout)

        res_json = self.cli("apply", "--json", r["run"], cwd=repo)
        self.assertNotEqual(res_json.returncode, 0)
        payload = self.outcome(res_json)
        self.assertEqual(payload["apply"]["failedStep"], "conflicts")
        self.assertEqual(payload["next"], expected_cmd)
        self.assertIn(expected_cmd, payload["apply"]["next"])

    # ------------------------------------------------------------------
    # P2-8: Card first line shows short base SHA and marks `已 rebase：<old>→<new>`
    # ------------------------------------------------------------------
    def test_p2_8_card_first_line_shows_base_sha_and_rebase_transition(self):
        repo = self.make_repo("repo-rebase-card", {"feat.txt": "v0\n", "main.txt": "m0\n"})
        head0 = self.git(repo, "rev-parse", "HEAD")
        short0 = head0[:7]

        self.fake_pi(
            [fixtures.answer("feat v1"), fixtures.SETTLED],
            pre="echo v1 > feat.txt && git -c user.name=t -c user.email=t@t add feat.txt && git -c user.name=t -c user.email=t@t commit -qm 'feat v1'",
        )
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "cam-fix", "--accept", "true", "round 1")
        )
        card1_first = self.cli("card", r1["run"], cwd=repo, human=True).stdout.splitlines()[0]
        self.assertIn(f"基线 {short0}", card1_first)
        self.assertNotIn("已 rebase", card1_first)

        # Advance source mainline on unrelated file and reply (auto-sync rebases worktree onto head1).
        (repo / "main.txt").write_text("m1\n", encoding="utf-8")
        self.git(repo, "add", "main.txt")
        self.git(repo, "commit", "-qm", "mainline advance")
        head1 = self.git(repo, "rev-parse", "HEAD")
        short1 = head1[:7]
        self.assertNotEqual(short0, short1)

        self.fake_pi([fixtures.answer("rebased onto latest main"), fixtures.SETTLED])
        r2 = self.outcome(self.cli("reply", r1["run"], "verify after rebase", cwd=repo))
        self.assertEqual(self.cli("wait", r2["run"], cwd=repo).returncode, 0)
        card2_json = json.loads(self.cli("card", r2["run"], "--json", cwd=repo, human=True).stdout)
        self.assertTrue(card2_json["base"]["rebased"], card2_json["base"])
        self.assertEqual(card2_json["base"]["short"], short1)
        self.assertEqual(card2_json["base"]["previousShort"], short0)
        card2_first = self.cli("card", r2["run"], cwd=repo, human=True).stdout.splitlines()[0]
        self.assertIn(f"基线 {short1}（已 rebase：{short0}→{short1}）", card2_first)

    # ------------------------------------------------------------------
    # P2-10: source_guard allows `2>&1` instead of refusing `<repo>/1`
    # ------------------------------------------------------------------
    def test_p2_10_source_guard_allows_2_to_1_redirect_in_source_checkout(self):
        repo = self.make_repo("repo-guard", {"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "guard-check", "--accept", "true", "edit")
        )
        for ok_cmd in ("ls 2>&1", "git status 2>&1", "cat a.txt 2>&1 >/dev/null", "echo hi >&2", "ls 2>&-"):
            res = self.cli("_source-check", r["dir"], ok_cmd, str(repo), cwd=repo, human=True)
            self.assertEqual(res.returncode, 0, f"{ok_cmd}: {res.stdout + res.stderr}")
        for bad_cmd in ("echo hi > out.txt", "echo hi &> out.txt", "echo hi 2>&1 > out.txt"):
            res = self.cli("_source-check", r["dir"], bad_cmd, str(repo), cwd=repo, human=True)
            self.assertEqual(res.returncode, 77, f"{bad_cmd}: {res.stdout + res.stderr}")


if __name__ == "__main__":
    unittest.main()
