"""Regressions for apply regeneration P9:
1. Regeneration (and apply's post-merge verify) have no default 600s hard timeout;
   silent commands emit `stall_warning` (event + status line) without being killed.
2. When `<source>/target/.cargo-lock` is held by another process, regeneration queues
   for the main-repo target by default instead of compiling in a cold isolated directory;
   only explicit config (`generated.isolateTarget` / `DELEGATE_APPLY_ISOLATE_TARGET`) isolates.
3. Failed regeneration `ok: false` output suggests both `delegate apply --resume-regenerate <run>`
   and `delegate mark-applied <run>`; `--resume-regenerate` finishes regeneration + `.applied`,
   and `mark-applied` clears `.generate-pending` and unlocks downstream `--after-applied` work items.
"""
import fcntl
import json
import os
import shlex
import subprocess
import threading
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateApplyRegenTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def read_events(self) -> list[dict]:
        p = self.state_dir() / "events.jsonl"
        if not p.is_file():
            return []
        return [json.loads(line) for line in p.read_text().splitlines() if line.strip()]

    def git(self, repo: Path, *args: str) -> str:
        res = subprocess.run(
            ["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t", *args],
            capture_output=True,
            text=True,
            check=True,
        )
        return res.stdout.strip()

    def test_slow_regeneration_warns_without_killing_and_shows_in_status(self):
        gate = self.work / "slow-gen-gate"
        self.addCleanup(lambda: gate.touch())
        gen_cmd = (
            "echo 'Compiling generated bindings v0.1.0'; "
            f"while [ ! -f {shlex.quote(str(gate))} ]; do sleep 0.05; done; "
            "cp src.txt gen/out.txt"
        )
        repo = self.repo({
            "src.txt": "v0\n",
            "gen/out.txt": "v0\n",
            ".delegate.json": json.dumps({"generated": {"paths": ["gen/"], "command": gen_cmd}}),
        })
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo v1 > src.txt; cp src.txt gen/out.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "slow-gen", "update src"))
        run_dir = Path(state["dir"])

        # Short stall threshold stand-in for >600s silence; no DELEGATE_GENERATE_TIMEOUT set.
        env = dict(self.env)
        env.pop("DELEGATE_GENERATE_TIMEOUT", None)
        env.pop("PI_DELEGATE_GENERATE_TIMEOUT", None)
        env["DELEGATE_STALL_GENERATE"] = "0.5s"

        proc = subprocess.Popen(
            [str(fixtures.DELEGATE), "apply", "--json", "--keep", state["run"]],
            cwd=repo,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(lambda: (gate.touch(), proc.wait(timeout=10) if proc.poll() is None else None))

        def has_generate_stall_warning():
            st = self.outcome(self.cli("status", state["run"]))
            sw = st.get("stallWarning")
            return isinstance(sw, dict) and sw.get("kind") == "generate"

        self.until(has_generate_stall_warning, "generate stall_warning in live status", timeout=15)
        self.assertIsNone(proc.poll(), "slow regeneration must NOT be killed on stall warning")

        live = self.outcome(self.cli("status", state["run"]))
        self.assertEqual(live["stallWarning"]["kind"], "generate")
        self.assertIn("Compiling generated bindings v0.1.0", live["stallWarning"].get("lastLine", ""))

        live_human = self.cli("status", state["run"], human=True)
        self.assertIn("卡住提醒", live_human.stdout)

        # Wait briefly past another tick to confirm it stays alive, then release gate.
        time.sleep(0.3)  # sleep-ok: verify process stays alive after stall_warning without being killed
        self.assertIsNone(proc.poll(), "slow regeneration must remain alive until completion")
        gate.touch()
        out, err = proc.communicate(timeout=15)
        self.assertEqual(proc.returncode, 0, out + err)
        doc = json.loads(out.strip().splitlines()[-1])
        self.assertTrue(doc["apply"]["ok"], doc)
        self.assertTrue((run_dir / ".applied").exists())
        self.assertFalse((run_dir / ".generate-pending").exists())
        self.assertFalse((run_dir / "stall-warning.json").exists())
        self.assertEqual((repo / "gen/out.txt").read_text(), "v1\n")

        evs = self.read_events()
        gen_warns = [
            e for e in evs
            if e["kind"] == "stall_warning"
            and e["run"] == state["run"]
            and e["payload"].get("stallKind") == "generate"
        ]
        self.assertGreaterEqual(len(gen_warns), 1, evs)
        self.assertIn("Compiling generated bindings v0.1.0", gen_warns[0]["payload"].get("lastLine", ""))

    def test_contended_main_target_waits_by_default_and_isolates_only_when_configured(self):
        target_log = self.work / "target-dir.log"
        gen_cmd = (
            f"printf '%s\\n' \"${{CARGO_TARGET_DIR:-MAIN}}\" >> {shlex.quote(str(target_log))}; "
            "cp src.txt gen/out.txt"
        )
        repo = self.repo({
            "src.txt": "v0\n",
            "gen/out.txt": "v0\n",
            ".delegate.json": json.dumps({"generated": {"paths": ["gen/"], "command": gen_cmd}}),
        })
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo v1 > src.txt; cp src.txt gen/out.txt")
        state = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "wait-target", "update src"))

        cargo_lock = repo / "target/.cargo-lock"
        cargo_lock.parent.mkdir(parents=True, exist_ok=True)
        lock_fh = cargo_lock.open("w")
        fcntl.flock(lock_fh.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        lock_released = threading.Event()

        def release_lock():
            if not lock_released.is_set():
                try:
                    fcntl.flock(lock_fh.fileno(), fcntl.LOCK_UN)
                finally:
                    lock_fh.close()
                    lock_released.set()

        self.addCleanup(release_lock)

        env = dict(self.env)
        env.pop("DELEGATE_APPLY_ISOLATE_TARGET", None)
        env.pop("CARGO_TARGET_DIR", None)
        proc = subprocess.Popen(
            [str(fixtures.DELEGATE), "apply", "--json", "--keep", state["run"]],
            cwd=repo,
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.addCleanup(lambda: (release_lock(), proc.wait(timeout=10) if proc.poll() is None else None))

        # Hold the target lock for 0.4s: apply must queue/wait instead of switching to a cold dir.
        time.sleep(0.4)  # sleep-ok: hold target/.cargo-lock to assert apply waits instead of isolating
        self.assertIsNone(proc.poll(), "apply must wait while main repo target/.cargo-lock is held")
        release_lock()

        out, err = proc.communicate(timeout=15)
        self.assertEqual(proc.returncode, 0, out + err)
        doc = json.loads(out.strip().splitlines()[-1])
        self.assertTrue(doc["apply"]["ok"], doc)
        timing = doc.get("applyTiming") or {}
        self.assertTrue(timing.get("targetContended"), doc)
        self.assertGreaterEqual(timing.get("targetWaitMs", 0), 200, doc)
        self.assertFalse(timing.get("regenerateTargetIsolated", False), doc)
        lines = [ln.strip() for ln in target_log.read_text().splitlines() if ln.strip()]
        self.assertEqual(lines, ["MAIN"], f"expected main target dir, got: {lines}")

        # Explicit `.delegate.json` generated.isolateTarget=true opts into isolated target dir.
        (repo / ".delegate.json").write_text(json.dumps({
            "generated": {"paths": ["gen/"], "command": gen_cmd, "isolateTarget": True}
        }))
        self.git(repo, "add", "-A")
        self.git(repo, "commit", "-m", "enable isolateTarget")
        self.fake_pi([fixtures.answer("done2"), fixtures.SETTLED], pre="echo v2 > src.txt; cp src.txt gen/out.txt")
        state2 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "iso-target", "update src 2"))

        lock_fh2 = cargo_lock.open("w")
        fcntl.flock(lock_fh2.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.addCleanup(lambda: (fcntl.flock(lock_fh2.fileno(), fcntl.LOCK_UN), lock_fh2.close()))

        ap2 = self.cli("apply", "--json", "--keep", state2["run"], cwd=repo, timeout=15)
        self.assertEqual(ap2.returncode, 0, ap2.stdout + ap2.stderr)
        doc2 = self.outcome(ap2)
        timing2 = doc2.get("applyTiming") or {}
        self.assertTrue(timing2.get("regenerateTargetIsolated"), doc2)
        self.assertIn("apply-target", timing2.get("regenerateTargetDir", ""))
        lines2 = [ln.strip() for ln in target_log.read_text().splitlines() if ln.strip()]
        self.assertIn("apply-target", lines2[-1])

    def test_failed_regeneration_suggests_resume_and_mark_applied_and_unlocks_downstream(self):
        repo = self.repo({
            "src.txt": "v0\n",
            "gen/out.txt": "v0\n",
            ".delegate.json": json.dumps({"generated": {"paths": ["gen/"], "command": "echo broken >&2; exit 2"}}),
        })
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo v1 > src.txt; cp src.txt gen/out.txt")
        r1 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "resume-case", "edit src"))
        r1_dir = Path(r1["dir"])

        # Path A: failure suggests --resume-regenerate and mark-applied; --resume-regenerate recovers cleanly.
        failed = self.cli("apply", "--json", "--keep", r1["run"], cwd=repo)
        self.assertEqual(failed.returncode, 1, failed.stdout + failed.stderr)
        doc = self.outcome(failed)
        self.assertFalse(doc["apply"]["ok"], doc)
        self.assertEqual(doc["apply"]["failedStep"], "regeneration")
        next_cmds = doc["apply"].get("next") or []
        self.assertTrue(any(f"--resume-regenerate {r1['run']}" in c for c in next_cmds), next_cmds)
        self.assertTrue(any(f"mark-applied {r1['run']}" in c for c in next_cmds), next_cmds)
        self.assertFalse((r1_dir / ".applied").exists())
        self.assertTrue((r1_dir / ".generate-pending").exists())
        self.assertEqual((repo / "src.txt").read_text(), "v1\n")

        (repo / ".delegate.json").write_text(json.dumps({
            "generated": {"paths": ["gen/"], "command": "cp src.txt gen/out.txt"}
        }))
        resumed = self.cli("apply", "--resume-regenerate", "--json", "--keep", r1["run"], cwd=repo)
        self.assertEqual(resumed.returncode, 0, resumed.stdout + resumed.stderr)
        resumed_doc = self.outcome(resumed)
        self.assertTrue(resumed_doc["apply"]["ok"], resumed_doc)
        self.assertTrue((r1_dir / ".applied").exists())
        self.assertFalse((r1_dir / ".generate-pending").exists())
        self.assertEqual((repo / "gen/out.txt").read_text(), "v1\n")

        # Path B: manual regeneration + commit + mark-applied unlocks downstream `--after-applied` work item.
        self.git(repo, "add", "-A")
        self.git(repo, "commit", "-m", "land r1")
        (repo / ".delegate.json").write_text(json.dumps({
            "generated": {"paths": ["gen/"], "command": "echo broken2 >&2; exit 2"}
        }))
        self.git(repo, "add", ".delegate.json")
        self.git(repo, "commit", "-m", "break generator")

        self.fake_pi([fixtures.answer("done2"), fixtures.SETTLED], pre="echo v2 > src.txt; cp src.txt gen/out.txt")
        r2 = self.outcome(self.cli("run", "--worktree", "--workdir", repo, "--name", "manual-case", "edit src 2"))
        r2_dir = Path(r2["dir"])

        added_res = self.cli(
            "work", "add", "downstream task", "--class", "feat",
            "--after-applied", r2["run"], "--json", cwd=repo,
        )
        self.assertEqual(added_res.returncode, 0, added_res.stdout + added_res.stderr)
        added = json.loads(added_res.stdout)
        wid = added["id"]
        self.assertEqual(added["state"], "blocked", added)

        failed2 = self.cli("apply", "--keep", r2["run"], cwd=repo, human=True)
        self.assertEqual(failed2.returncode, 1, failed2.stdout + failed2.stderr)
        self.assertIn(f"delegate apply --resume-regenerate {r2['run']}", failed2.stdout)
        self.assertIn(f"delegate mark-applied {r2['run']}", failed2.stdout)
        self.assertTrue((r2_dir / ".generate-pending").exists())

        # Controller manually generates, commits, and marks applied.
        (repo / "gen/out.txt").write_text((repo / "src.txt").read_text())
        self.git(repo, "add", "src.txt", "gen/out.txt")
        self.git(repo, "commit", "-m", "manual regen and land r2")

        marked = self.cli("mark-applied", "--json", r2["run"], cwd=repo)
        self.assertEqual(marked.returncode, 0, marked.stdout + marked.stderr)
        self.assertTrue((r2_dir / ".applied").exists())
        self.assertFalse((r2_dir / ".generate-pending").exists())

        shown_res = self.cli("work", "show", wid, "--json", cwd=repo)
        self.assertEqual(shown_res.returncode, 0, shown_res.stdout + shown_res.stderr)
        shown = json.loads(shown_res.stdout)
        self.assertEqual(shown["state"], "ready", shown)


if __name__ == "__main__":
    unittest.main()
