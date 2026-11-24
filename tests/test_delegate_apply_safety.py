"""Regression tests for 2026-11-24 apply safety, `/clear` adopt, and premerge cache self-healing:
1. `apply` exit codes: conflicts and unresolved conflict markers exit 3 (including `apply --commit`
   and `apply --merge`), other failures (dirty-overlap, regeneration, commit, verify) exit 1,
   never exiting 0 when `!!! apply 失败` or conflict markers are emitted; `merge` conflict exits 3.
2. `delegate adopt <run>...`: rewrites `caller` across every round of the reply chain, updates
   `index.json` and linked open work items, and emits an `adopted` event; Stop hook hints
   `delegate adopt <run>` when the original caller is dead/superseded in the same terminal or cwd
   (reusing W058 liveness checks without regressing W058's 6h cross-session isolation).
3. Premerge cache self-healing: when premerge `generate` or acceptance fails with a linker error
   (`undefined symbol` or `linking with ... failed`), premerge removes affected crate entries in
   its own `target/<profile>/{incremental,deps,.fingerprint}` (never touching any worktree target),
   emits `premerge_cache_healed`, and retries once; a second failure reports `premerge_failed`.
"""
import json
import os
import shlex
import subprocess
import sys
import time
import unittest
from pathlib import Path

import test_delegate as fixtures

ROOT = Path(__file__).resolve().parent.parent
STOP_HOOK = ROOT / "skills/delegate/hooks/claude-code-stop.py"


def iso_ago(hours: float) -> str:
    ts = int(time.time() - hours * 3600)
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts))


class DelegateApplySafetyTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def cache_dir(self) -> Path:
        return Path(self.env["XDG_CACHE_HOME"]) / "delegate"

    def clean_caller_env(self, extra: dict | None = None) -> dict[str, str]:
        env = {k: v for k, v in self.env.items() if k not in fixtures.CALLER_ENV}
        if extra:
            env.update(extra)
        return env

    def run_delegate(
        self, args: list[str], cwd: Path, env: dict[str, str]
    ) -> subprocess.CompletedProcess:
        return subprocess.run(
            [str(fixtures.DELEGATE), *args],
            cwd=cwd,
            capture_output=True,
            text=True,
            env=env,
            timeout=45,
        )

    def git(self, repo: Path, *args: str) -> str:
        res = subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            text=True,
            check=True,
        )
        return res.stdout.strip()

    def read_events(self) -> list[dict]:
        p = self.state_dir() / "events.jsonl"
        if not p.is_file():
            return []
        return [json.loads(line) for line in p.read_text(encoding="utf-8").splitlines() if line.strip()]

    def run_stop_hook(
        self,
        cwd: Path,
        env: dict[str, str],
        session_id: str | None = None,
        extra_env: dict | None = None,
    ) -> dict | None:
        hook_env = {
            **env,
            "DELEGATE_BIN": str(fixtures.DELEGATE),
            "DELEGATE_STOP_HOOK_WAITERS": "",
            "DELEGATE_STOP_HOOK_PROCS": "",
            "DELEGATE_STOP_HOOK_CURRENT_ONLY": "1",
        }
        if extra_env:
            hook_env.update(extra_env)
        payload: dict = {"cwd": str(cwd), "stop_hook_active": False}
        if session_id is not None:
            payload["session_id"] = session_id
        res = subprocess.run(
            [sys.executable, str(STOP_HOOK)],
            input=json.dumps(payload),
            capture_output=True,
            text=True,
            env=hook_env,
            timeout=15,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        out = res.stdout.strip()
        return json.loads(out) if out else None

    def get_dead_pid(self) -> int:
        proc = subprocess.Popen(
            [sys.executable, "-c", "pass"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        proc.wait(timeout=5)
        return proc.pid

    def read_proc_start_ticks(self, pid: int) -> int:
        txt = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8", errors="ignore")
        fields = txt[txt.rfind(")") + 2 :].split()
        return int(fields[19])

    # ------------------------------------------------------------------
    # 1. Apply / merge exit codes: conflict -> 3, other failure -> 1
    # ------------------------------------------------------------------
    def test_1_apply_conflict_commit_exits_3_and_blocks_shell_and_chain(self):
        repo = self.repo({"shared.txt": "base\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo colleague > shared.txt")
        run = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "c1", "--accept", "true", "edit")
        )
        # Committed conflicting edit on main.
        (repo / "shared.txt").write_text("main-change\n", encoding="utf-8")
        self.git(repo, "add", "shared.txt")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "original subject")
        head_before = self.git(repo, "rev-parse", "HEAD")

        # Chaining `&& git commit --amend` MUST NOT execute because `apply --commit` exits 3.
        cmd = (
            f"{shlex.quote(str(fixtures.DELEGATE))} apply {shlex.quote(run['run'])} --commit "
            f"&& git -C {shlex.quote(str(repo))} commit --amend -m 'SHOULD-NOT-RUN'"
        )
        res = subprocess.run(
            ["bash", "-lc", cmd],
            cwd=repo,
            capture_output=True,
            text=True,
            env=self.env,
            timeout=30,
        )
        self.assertEqual(res.returncode, 3, res.stdout + res.stderr)
        self.assertIn("!!! apply 失败（阶段：文件冲突 / conflicts", res.stdout)
        self.assertEqual(self.git(repo, "rev-parse", "HEAD"), head_before)
        self.assertEqual(self.git(repo, "log", "-1", "--pretty=%s"), "original subject")

        # `apply --merge` writes conflict markers and ALSO exits 3.
        merged = self.cli("apply", "--merge", "--keep", run["run"], cwd=repo, human=True)
        self.assertEqual(merged.returncode, 3, merged.stdout + merged.stderr)
        self.assertIn("<<<<<<< current", (repo / "shared.txt").read_text(encoding="utf-8"))

        # Subsequent apply onto unresolved conflict markers exits 3 as well.
        self.fake_pi([fixtures.answer("done2"), fixtures.SETTLED], pre="echo second > shared.txt")
        run2 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "c2", "--accept", "true", "edit2")
        )
        blocked = self.cli("apply", run2["run"], cwd=repo, human=True)
        self.assertEqual(blocked.returncode, 3, blocked.stdout + blocked.stderr)
        self.assertIn("冲突标记未解决", blocked.stderr)

    def test_1b_apply_non_conflict_failure_exits_1_and_merge_conflict_exits_3(self):
        # Dirty-overlap (non-conflict failure) exits 1.
        repo = self.repo({"a.txt": "line1\nline2\nline3\nline4\nline5\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="sed -i s/line1/ONE/ a.txt")
        r_dirty = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "dirty1", "--accept", "true", "edit")
        )
        (repo / "a.txt").write_text("line1\nline2\nline3\nline4\nFIVE\n", encoding="utf-8")
        res_dirty = self.cli("apply", r_dirty["run"], cwd=repo, human=True)
        self.assertEqual(res_dirty.returncode, 1, res_dirty.stdout + res_dirty.stderr)
        self.assertIn("!!! apply 失败", res_dirty.stdout)

        # Clean working tree, commit a conflicting change, then test `delegate merge` conflict exit 3.
        (repo / "a.txt").write_text("CONFLICT\nline2\nline3\nline4\nline5\n", encoding="utf-8")
        self.git(repo, "add", "a.txt")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "conflict head")
        res_merge = self.cli("merge", r_dirty["run"], "--json", cwd=repo)
        self.assertEqual(res_merge.returncode, 3, res_merge.stdout + res_merge.stderr)
        rep = json.loads(res_merge.stdout)
        self.assertEqual(rep["mergedCount"], 0)
        self.assertEqual(rep["skippedCount"], 1)

    # ------------------------------------------------------------------
    # 2. `delegate adopt <run>...` & Stop hook `/clear` hint
    # ------------------------------------------------------------------
    def test_2_adopt_rewrites_entire_reply_chain_and_emits_event(self):
        repo = self.repo({"a.txt": "v0\n"})
        old_sid = "sess-before-clear"
        new_sid = "sess-after-clear"
        old_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": old_sid})
        new_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": new_sid})

        # Create a work item and a 3-round reply chain under `old_sid`.
        w_add = self.run_delegate(["work", "add", "追踪项", "--repo", str(repo), "--json"], repo, old_env)
        self.assertEqual(w_add.returncode, 0, w_add.stderr)
        wid = json.loads(w_add.stdout)["id"]

        self.fake_pi([fixtures.answer("round 1"), fixtures.SETTLED], pre="echo r1 > a.txt")
        r1 = self.outcome(
            self.run_delegate(
                ["run", "--json", "--worktree", "--workdir", str(repo), "--work", wid, "--name", "chain-a", "--accept", "true", "round 1"],
                repo,
                old_env,
            )
        )
        self.fake_pi([fixtures.answer("round 2"), fixtures.SETTLED], pre="echo r2 > a.txt")
        r2 = self.outcome(
            self.run_delegate(["reply", "--json", "--wait", r1["run"], "round 2"], repo, old_env)
        )
        self.fake_pi([fixtures.answer("round 3"), fixtures.SETTLED], pre="echo r3 > a.txt")
        r3 = self.outcome(
            self.run_delegate(
                ["reply", "--over-limit", "test chain", "--json", "--wait", r2["run"], "round 3"],
                repo,
                old_env,
            )
        )

        # Adopt from the MIDDLE round (`r2`) under `new_sid` — all 3 rounds MUST be rewritten!
        adopted = self.run_delegate(["adopt", "--json", r2["run"]], repo, new_env)
        self.assertEqual(adopted.returncode, 0, adopted.stdout + adopted.stderr)
        payload = json.loads(adopted.stdout.strip())
        self.assertEqual(payload["fromCaller"], old_sid)
        self.assertEqual(payload["toCaller"], new_sid)
        self.assertEqual(payload["rounds"], [r1["run"], r2["run"], r3["run"]])

        for r in (r1, r2, r3):
            meta = json.loads((Path(r["dir"]) / "meta.json").read_text(encoding="utf-8"))
            self.assertEqual(meta["caller"], new_sid, r["run"])

        # Linked work item's caller is also updated.
        w_show = json.loads(
            self.run_delegate(["work", "show", wid, "--json"], repo, new_env).stdout
        )
        self.assertEqual(w_show.get("caller"), new_sid)

        # `adopted` event recorded in events.jsonl.
        evs = [e for e in self.read_events() if e.get("kind") == "adopted"]
        self.assertEqual(len(evs), 1, evs)
        self.assertEqual(evs[0]["run"], r3["run"])
        self.assertEqual(evs[0]["payload"]["fromCaller"], old_sid)
        self.assertEqual(evs[0]["payload"]["toCaller"], new_sid)
        self.assertEqual(evs[0]["payload"]["rounds"], [r1["run"], r2["run"], r3["run"]])

        # Adopting without any caller env fails non-zero.
        no_caller_env = self.clean_caller_env()
        fail = self.run_delegate(["adopt", r3["run"]], repo, no_caller_env)
        self.assertNotEqual(fail.returncode, 0)

    def test_2b_stop_hook_hints_adopt_for_dead_caller_in_same_cwd_or_terminal_or_clear(self):
        repo = self.repo({"a.txt": "v0\n"})
        old_sid = "sess-old-clear"
        new_sid = "sess-new-clear"
        old_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": old_sid})
        new_env = self.clean_caller_env({"CLAUDE_CODE_SESSION_ID": new_sid})

        self.fake_pi([fixtures.answer("delivered work"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r = self.outcome(
            self.run_delegate(
                ["start", "--json", "--worktree", "--workdir", str(repo), "--name", "clear-run", "--accept", "true", "edit"],
                repo,
                old_env,
            )
        )
        self.until(lambda: (Path(r["dir"]) / "exit_code").exists(), "clear-run finished")

        sdir = self.state_dir()
        sdir.mkdir(parents=True, exist_ok=True)
        dead_pid = self.get_dead_pid()

        # Case A: Dead caller in SAME cwd (only 5 minutes ago, << 6h) -> Stop hook prompts `delegate adopt <run>`!
        (sdir / "sessions.json").write_text(
            json.dumps(
                {
                    old_sid: {
                        "pid": dead_pid,
                        "cwd": str(repo.resolve()),
                        "tty": "tmux:%1",
                        "lastSeenAt": iso_ago(0.08),
                    }
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        blk_cwd = self.run_stop_hook(
            repo,
            new_env,
            session_id=new_sid,
            extra_env={"TMUX_PANE": "%99"},
        )
        self.assertIsNotNone(blk_cwd)
        self.assertEqual(blk_cwd["decision"], "block")
        self.assertIn(f"delegate adopt {r['run']}", blk_cwd["reason"])

        # Case B: Dead caller in DIFFERENT cwd AND DIFFERENT terminal (< 6h) -> NOT blocked (preserves W058)!
        other_dir = self.work / "somewhere-else"
        other_dir.mkdir(exist_ok=True)
        (sdir / "sessions.json").write_text(
            json.dumps(
                {
                    old_sid: {
                        "pid": dead_pid,
                        "cwd": str(other_dir.resolve()),
                        "tty": "tmux:%1",
                        "lastSeenAt": iso_ago(0.08),
                    }
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        self.assertIsNone(
            self.run_stop_hook(
                repo,
                new_env,
                session_id=new_sid,
                extra_env={"TMUX_PANE": "%99"},
            )
        )

        # Case C: Dead caller in DIFFERENT cwd, SAME terminal (`TMUX_PANE=%1`) -> prompts `delegate adopt <run>`!
        blk_tty = self.run_stop_hook(
            repo,
            new_env,
            session_id=new_sid,
            extra_env={"TMUX_PANE": "%1"},
        )
        self.assertIsNotNone(blk_tty)
        self.assertIn(f"delegate adopt {r['run']}", blk_tty["reason"])

        # Case D: `/clear` inside the SAME live harness process (same PID + startTicks as current session)!
        live_proc = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        self.addCleanup(
            lambda: (live_proc.kill(), live_proc.wait(timeout=5)) if live_proc.poll() is None else None
        )
        ticks = self.read_proc_start_ticks(live_proc.pid)
        (sdir / "sessions.json").write_text(
            json.dumps(
                {
                    old_sid: {
                        "pid": live_proc.pid,
                        "startTicks": ticks,
                        "cwd": str(repo.resolve()),
                        "lastSeenAt": iso_ago(0.05),
                    },
                    new_sid: {
                        "pid": live_proc.pid,
                        "startTicks": ticks,
                        "cwd": str(repo.resolve()),
                        "lastSeenAt": iso_ago(0.01),
                    },
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        blk_clear = self.run_stop_hook(repo, new_env, session_id=new_sid)
        self.assertIsNotNone(blk_clear)
        self.assertIn(f"delegate adopt {r['run']}", blk_clear["reason"])

        # After running `delegate adopt <run>`, adopt hint is gone (now owned by `new_sid` directly).
        self.assertEqual(self.run_delegate(["adopt", r["run"]], repo, new_env).returncode, 0)
        blk_after = self.run_stop_hook(repo, new_env, session_id=new_sid)
        self.assertIsNotNone(blk_after)  # still unmerged, owned by new_sid now
        self.assertNotIn("delegate adopt", blk_after["reason"])

    # ------------------------------------------------------------------
    # 3. Premerge target cache self-healing (accept + generate + 2nd fail)
    # ------------------------------------------------------------------
    def _premerge_wt_dir(self, repo: Path) -> Path:
        premerge_root = self.cache_dir() / "premerge"

        def _find():
            if not premerge_root.is_dir():
                return False
            for d in premerge_root.iterdir():
                wt = d / "worktree"
                if wt.is_dir():
                    return wt
            return False

        return self.until(_find, f"premerge worktree created under {premerge_root}", timeout=20)

    def test_3_premerge_accept_linker_error_heals_cache_and_retries_once(self):
        self.env.pop("DELEGATE_PREMERGE", None)
        attempt_log = self.work / "pm-accept-attempts.log"
        # Accept script: only fails with linker error when `target/debug/incremental/xtask-deadbeef12` exists!
        # On the worker's run, we seed that corrupted directory in premerge's own target (and in the run's
        # own worktree target to prove worktree targets are NEVER touched).
        accept_script = (
            f'echo run >> {shlex.quote(str(attempt_log))}; '
            'if [ -d target/debug/incremental/xtask-deadbeef12 ]; then '
            '  printf "%s\\n" "mold: error: undefined symbol: core::ptr::drop_in_place<xtask::State>" >&2; '
            "  printf '%s\\n' 'error: could not compile `xtask` (bin \"xtask\")' >&2; "
            '  exit 1; '
            'fi; '
            'exit 0'
        )
        repo = self.repo(
            {
                ".gitignore": "target/\n",
                ".delegate.json": json.dumps({"accept": accept_script}),
                "src.txt": "v0\n",
                "main.txt": "m0\n",
            }
        )
        # First warm up premerge worker once so premerge worktree exists, then plant corrupted cache.
        self.fake_pi([fixtures.answer("warm"), fixtures.SETTLED], pre="echo w1 > src.txt")
        r_warm = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "warm", "--accept", accept_script, "warm")
        )
        pm_wt = self._premerge_wt_dir(repo)
        self.until(
            lambda: (Path(r_warm["dir"]) / "premerge.json").is_file()
            and json.loads((Path(r_warm["dir"]) / "premerge.json").read_text())["status"] == "ready",
            "warm premerge ready",
            timeout=25,
        )
        self.assertEqual(self.cli("apply", r_warm["run"], cwd=repo).returncode, 0)
        self.git(repo, "add", "src.txt")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "warm commit")

        # Now start a new run (passes in its own clean worktree, then advance main so premerge MUST re-run accept!).
        self.fake_pi([fixtures.answer("feat"), fixtures.SETTLED], pre="echo f2 > src.txt")
        r_feat = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "heal-ok", "--accept", accept_script, "feat")
        )
        run_dir = Path(r_feat["dir"])
        run_wt = Path(r_feat["worktree"])
        self.until(
            lambda: (run_dir / "premerge.json").is_file()
            and json.loads((run_dir / "premerge.json").read_text())["status"] == "ready",
            "initial reused premerge ready",
            timeout=25,
        )

        # Plant corrupted artifacts in premerge's OWN target AND in run's worktree target.
        pm_debug = pm_wt / "target/debug"
        (pm_debug / "incremental/xtask-deadbeef12").mkdir(parents=True, exist_ok=True)
        (pm_debug / "deps").mkdir(parents=True, exist_ok=True)
        (pm_debug / "deps/xtask-deadbeef12").write_text("bad\n", encoding="utf-8")
        (pm_debug / "deps/libserde-11112222.rlib").write_text("keep\n", encoding="utf-8")
        (pm_debug / ".fingerprint/xtask-deadbeef12").mkdir(parents=True, exist_ok=True)

        wt_sentinel = run_wt / "target/debug/incremental/xtask-deadbeef12"
        wt_sentinel.mkdir(parents=True, exist_ok=True)

        attempt_log.unlink(missing_ok=True)

        # Advance main on an unrelated file -> invalidates premerge -> wakes worker to re-run accept!
        (repo / "main.txt").write_text("m1\n", encoding="utf-8")
        self.git(repo, "add", "main.txt")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "advance main")
        new_head = self.git(repo, "rev-parse", "HEAD")

        # Wake worker via `status`.
        self.cli("status", r_feat["run"], cwd=repo)
        self.until(
            lambda: (run_dir / "premerge.json").is_file()
            and json.loads((run_dir / "premerge.json").read_text()).get("baseHead") == new_head
            and json.loads((run_dir / "premerge.json").read_text()).get("status") == "ready",
            "healed premerge ready on new head",
            timeout=25,
        )

        # Verify: retried once (2 attempts total), `premerge_cache_healed` emitted, NO `premerge_failed`,
        # unrelated dep preserved, and worktree target untouched!
        attempts = [ln for ln in attempt_log.read_text(encoding="utf-8").splitlines() if ln.strip()]
        self.assertEqual(len(attempts), 2, attempts)
        evs = [e for e in self.read_events() if e.get("run") == r_feat["run"]]
        healed_evs = [e for e in evs if e["kind"] == "premerge_cache_healed"]
        failed_evs = [e for e in evs if e["kind"] == "premerge_failed"]
        self.assertEqual(len(healed_evs), 1, evs)
        self.assertEqual(healed_evs[0]["payload"]["step"], "accept")
        self.assertIn("xtask", healed_evs[0]["payload"]["crates"])
        self.assertEqual(failed_evs, [], evs)
        self.assertFalse((pm_debug / "incremental/xtask-deadbeef12").exists())
        self.assertFalse((pm_debug / "deps/xtask-deadbeef12").exists())
        self.assertFalse((pm_debug / ".fingerprint/xtask-deadbeef12").exists())
        self.assertTrue((pm_debug / "deps/libserde-11112222.rlib").is_file())
        self.assertTrue(wt_sentinel.is_dir(), "worktree target must NEVER be touched")

    def test_3b_premerge_second_linker_failure_reports_premerge_failed(self):
        self.env.pop("DELEGATE_PREMERGE", None)
        counter = self.work / "pm-fail-count.log"
        # Succeeds when `main.txt` is `m0` (initial run), ALWAYS fails with `linking with ... failed` on `m1`!
        accept_script = (
            'if grep -q m0 main.txt; then exit 0; fi; '
            f'echo fail >> {shlex.quote(str(counter))}; '
            "printf '%s\\n' 'error: linking with `mold` failed: exit status: 1' >&2; "
            'exit 1'
        )
        repo = self.repo(
            {
                ".gitignore": "target/\n",
                ".delegate.json": json.dumps({"accept": accept_script}),
                "src.txt": "v0\n",
                "main.txt": "m0\n",
            }
        )
        self.fake_pi([fixtures.answer("feat"), fixtures.SETTLED], pre="echo f1 > src.txt")
        r = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "heal-fail", "--accept", accept_script, "feat")
        )
        run_dir = Path(r["dir"])
        pm_wt = self._premerge_wt_dir(repo)
        self.until(
            lambda: (run_dir / "premerge.json").is_file()
            and json.loads((run_dir / "premerge.json").read_text())["status"] == "ready",
            "initial premerge ready",
            timeout=25,
        )
        # Seed profile dirs in premerge target (without crate-named entries -> exercises profile fallback!).
        pm_debug = pm_wt / "target/debug"
        (pm_debug / "incremental/old").mkdir(parents=True, exist_ok=True)
        (pm_debug / "deps").mkdir(parents=True, exist_ok=True)
        (pm_debug / "deps/stale.o").write_text("x\n", encoding="utf-8")
        (pm_debug / ".fingerprint/old").mkdir(parents=True, exist_ok=True)

        # Advance main so accept now hits persistent linker error.
        (repo / "main.txt").write_text("m1\n", encoding="utf-8")
        self.git(repo, "add", "main.txt")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "break link")
        new_head = self.git(repo, "rev-parse", "HEAD")

        self.cli("status", r["run"], cwd=repo)
        self.until(
            lambda: (run_dir / "premerge.json").is_file()
            and json.loads((run_dir / "premerge.json").read_text()).get("baseHead") == new_head
            and json.loads((run_dir / "premerge.json").read_text()).get("status") == "failed",
            "second failure reports premerge_failed",
            timeout=25,
        )

        # Exactly 2 attempts (1 initial + 1 retry), 1 `premerge_cache_healed`, 1 `premerge_failed`.
        attempts = [ln for ln in counter.read_text(encoding="utf-8").splitlines() if ln.strip()]
        self.assertEqual(len(attempts), 2, attempts)
        evs = [e for e in self.read_events() if e.get("run") == r["run"]]
        self.assertEqual(len([e for e in evs if e["kind"] == "premerge_cache_healed"]), 1, evs)
        self.assertEqual(len([e for e in evs if e["kind"] == "premerge_failed"]), 1, evs)
        self.assertFalse((pm_debug / "incremental").exists())
        self.assertFalse((pm_debug / "deps").exists())
        self.assertFalse((pm_debug / ".fingerprint").exists())

    def test_3c_premerge_generate_linker_error_heals_cache_and_succeeds(self):
        self.env.pop("DELEGATE_PREMERGE", None)
        gen_counter = self.work / "pm-gen-count.log"
        gen_script = (
            f'echo gen >> {shlex.quote(str(gen_counter))}; '
            'if [ -d target/debug/incremental/xtask-bad12345 ]; then '
            '  printf "%s\\n" "mold: error: undefined symbol: drop_glue<xtask::Gen>" >&2; '
            "  printf '%s\\n' 'error: could not compile `xtask`' >&2; "
            '  exit 1; '
            'fi; '
            'cp schema.in gen/out.txt'
        )
        cfg = {
            "accept": "true",
            "generated": {
                "inputs": ["schema.in"],
                "paths": ["gen/out.txt"],
                "command": gen_script,
            },
        }
        repo = self.repo(
            {
                ".gitignore": "target/\n",
                ".delegate.json": json.dumps(cfg),
                "schema.in": "v0\n",
                "gen/out.txt": "v0\n",
                "other.txt": "o0\n",
            }
        )
        # Warm up premerge worktree first with a non-generated change.
        self.fake_pi([fixtures.answer("warm"), fixtures.SETTLED], pre="echo o1 > other.txt")
        r_warm = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "gen-warm", "--accept", "true", "warm")
        )
        pm_wt = self._premerge_wt_dir(repo)
        self.until(
            lambda: (Path(r_warm["dir"]) / "premerge.json").is_file()
            and json.loads((Path(r_warm["dir"]) / "premerge.json").read_text())["status"] == "ready",
            "warm premerge ready",
            timeout=25,
        )
        self.assertEqual(self.cli("apply", r_warm["run"], cwd=repo).returncode, 0)
        self.git(repo, "add", "other.txt")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "warm")

        # Plant corrupted premerge target cache BEFORE starting the run that touches `schema.in`!
        pm_debug = pm_wt / "target/debug"
        (pm_debug / "incremental/xtask-bad12345").mkdir(parents=True, exist_ok=True)
        (pm_debug / "deps").mkdir(parents=True, exist_ok=True)
        (pm_debug / "deps/xtask-bad12345").write_text("bad\n", encoding="utf-8")
        (pm_debug / ".fingerprint/xtask-bad12345").mkdir(parents=True, exist_ok=True)
        gen_counter.unlink(missing_ok=True)

        self.fake_pi(
            [fixtures.answer("gen edit"), fixtures.SETTLED],
            pre="echo v1 > schema.in && cp schema.in gen/out.txt",
        )
        r_gen = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--name", "gen-heal", "--accept", "true", "edit schema")
        )
        run_dir = Path(r_gen["dir"])
        self.until(
            lambda: (run_dir / "premerge.json").is_file()
            and json.loads((run_dir / "premerge.json").read_text())["status"] == "ready",
            "generate cache-healed premerge ready",
            timeout=25,
        )

        attempts = [ln for ln in gen_counter.read_text(encoding="utf-8").splitlines() if ln.strip()]
        self.assertEqual(len(attempts), 2, attempts)
        evs = [e for e in self.read_events() if e.get("run") == r_gen["run"]]
        healed = [e for e in evs if e["kind"] == "premerge_cache_healed"]
        self.assertEqual(len(healed), 1, evs)
        self.assertEqual(healed[0]["payload"]["step"], "generate")
        self.assertEqual([e for e in evs if e["kind"] == "premerge_failed"], [], evs)


if __name__ == "__main__":
    unittest.main()
