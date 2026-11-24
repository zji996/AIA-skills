"""delegate ETA · Chain 1 integration tests:
1. `started` event carries `class`, `thinking`, and `workId` (direct start, reply inheritance,
   and deferred start after capacity queue wait via `start_gated`).
2. `accept` event carries `elapsedSeconds`, `queuedSeconds`, and `layerSeconds` (both single
   `--accept` and tiered `acceptRules`).
3. `premerge` success emits `premerge_ready` with `elapsedSeconds`, `reusedAccept`, `baseHead`,
   `baseShort`, and `ranLayers` (both reused-accept fast path and baseline-moved rerun path).
4. `delegate speed` atomically writes `<state>/eta-model.json` and cleanly rebuilds it after deletion.
"""
import json
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateEtaChain1Tests(unittest.TestCase):
    def setUp(self):
        fixtures.DelegateTests.setUp(self)

    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until
    wait_until = fixtures.DelegateTests.wait_until
    wait_json = fixtures.DelegateTests.wait_json
    wait_premerge = fixtures.DelegateTests.wait_premerge
    start_gated = fixtures.DelegateTests.start_gated
    hold_lane_gated = fixtures.DelegateTests.hold_lane_gated
    user_config = fixtures.DelegateTests.user_config

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def read_events(self) -> list[dict]:
        ev_file = self.state_dir() / "events.jsonl"
        if not ev_file.is_file():
            return []
        out = []
        for ln in ev_file.read_text(encoding="utf-8").splitlines():
            ln = ln.strip()
            if ln:
                out.append(json.loads(ln))
        return out

    def git(self, repo: Path, *args: str) -> str:
        res = subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            text=True,
            check=True,
        )
        return res.stdout.strip()

    def make_repo(self, extra_files: dict[str, str] | None = None) -> Path:
        files = {"notes.txt": "initial\n"}
        if extra_files:
            files.update(extra_files)
        return self.repo(files)

    def add_work_item(self, repo: Path, title: str = "eta work item") -> str:
        res = self.cli("work", "add", "--json", title, cwd=repo)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        doc = json.loads(res.stdout)
        return doc.get("id") or doc["item"]["id"]

    def test_started_event_includes_class_thinking_work_id_and_reply_inherits(self):
        repo = self.make_repo()
        wid = self.add_work_item(repo, "hard task for eta")
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])

        # Explicitly override `--class hard` default thinking (`high`) with `--thinking low`.
        res = self.cli(
            "run",
            "--read-only",
            "--workdir",
            repo,
            "--class",
            "hard",
            "--thinking",
            "low",
            "--work",
            wid,
            "--name",
            "eta-direct",
            "inspect",
            cwd=repo,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        first = self.outcome(res)
        first_id = first["run"]

        started = [e for e in self.read_events() if e["kind"] == "started" and e["run"] == first_id]
        self.assertEqual(len(started), 1, started)
        payload = started[0]["payload"]
        self.assertEqual(payload.get("class"), "hard")
        self.assertEqual(payload.get("thinking"), "low")
        self.assertEqual(payload.get("workId"), wid)

        # Reply without repeating --class / --thinking / --work must inherit class & workId
        # onto meta and `started` WITHOUT triggering `--class hard`'s default thinking (`high`).
        rep = self.cli("reply", first_id, "follow up", cwd=repo)
        self.assertEqual(rep.returncode, 0, rep.stdout + rep.stderr)
        self.assertIn("原会话", rep.stderr)
        self.assertNotIn("类别 hard", rep.stderr)
        second = self.outcome(rep)
        second_id = second["run"]
        second_meta = json.loads((Path(second["dir"]) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(second_meta.get("class"), "hard")
        self.assertEqual(second_meta.get("thinking"), "low")

        rep_started = [e for e in self.read_events() if e["kind"] == "started" and e["run"] == second_id]
        self.assertEqual(len(rep_started), 1, rep_started)
        rep_payload = rep_started[0]["payload"]
        self.assertEqual(rep_payload.get("class"), "hard")
        self.assertEqual(rep_payload.get("thinking"), "low")
        self.assertEqual(rep_payload.get("workId"), wid)
        self.assertEqual(rep_payload.get("parent"), first_id)

    def test_queued_started_event_preserves_class_thinking_and_work_id(self):
        repo = self.make_repo()
        wid = self.add_work_item(repo, "queued eta item")
        self.env["DELEGATE_MIN_AVAILABLE_MB"] = "0"
        self.env["DELEGATE_MAX_ACTIVE"] = "1"

        first_state, first_gate = self.start_gated(
            "--read-only",
            "--workdir",
            repo,
            "--name",
            "blocker",
            "hold slot",
            cwd=repo,
        )

        # Queue second run while slot is occupied; reuse the same gated fake_pi.
        q_res = self.cli(
            "start",
            "--read-only",
            "--workdir",
            repo,
            "--class",
            "build",
            "--thinking",
            "medium",
            "--work",
            wid,
            "--name",
            "queued-job",
            "waiting task",
            cwd=repo,
        )
        self.assertEqual(q_res.returncode, 0, q_res.stdout + q_res.stderr)
        q_state = self.outcome(q_res)
        q_id = q_state["run"]
        q_dir = Path(q_state["dir"])
        self.addCleanup(
            lambda: (
                (q_dir / ".fake-gate").touch() if q_dir.exists() else None,
                self.cli("stop", q_state["dir"]) if q_dir.exists() else None,
            )
        )

        # Confirm it was queued first, then release blocker so supervisor promotes it.
        self.wait_until(
            lambda: any(e["kind"] == "queued" and e["run"] == q_id for e in self.read_events()),
            "queued event for deferred run",
        )
        first_gate.touch()

        # Wait for queued run to emit `started` with `fromQueue=True`.
        def find_queued_started():
            matches = [e for e in self.read_events() if e["kind"] == "started" and e["run"] == q_id]
            return matches[0] if matches else None

        ev = self.wait_until(find_queued_started, "started event after queue release", timeout=15)
        (q_dir / ".fake-gate").touch()
        self.wait_until(
            lambda: any(e["kind"] == "delivered" and e["run"] == q_id for e in self.read_events()),
            "delivered event for queued run",
            timeout=15,
        )
        self.wait_until(
            lambda: any(
                e["kind"] == "delivered" and e["run"] == first_state["run"]
                for e in self.read_events()
            ),
            "delivered event for blocker run",
            timeout=15,
        )

        pl = ev["payload"]
        self.assertTrue(pl.get("fromQueue"))
        self.assertEqual(pl.get("class"), "build")
        self.assertEqual(pl.get("thinking"), "medium")
        self.assertEqual(pl.get("workId"), wid)

    def test_accept_event_records_elapsed_queued_and_layer_seconds_single_accept(self):
        repo = self.make_repo()
        self.fake_pi(
            [fixtures.answer("edited"), fixtures.SETTLED],
            pre="echo changed >> notes.txt",
        )

        res = self.cli(
            "run",
            "--worktree",
            "--workdir",
            repo,
            "--name",
            "single-accept",
            "--accept",
            "test -f notes.txt",
            "edit notes",
            cwd=repo,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        run_id = self.outcome(res)["run"]

        acc_evs = [e for e in self.read_events() if e["kind"] == "accept" and e["run"] == run_id]
        self.assertEqual(len(acc_evs), 1, acc_evs)
        pl = acc_evs[0]["payload"]
        self.assertTrue(pl.get("ok"))
        self.assertIsInstance(pl.get("elapsedSeconds"), (int, float))
        self.assertGreaterEqual(pl["elapsedSeconds"], 0.0)
        self.assertIsInstance(pl.get("queuedSeconds"), (int, float))
        self.assertGreaterEqual(pl["queuedSeconds"], 0.0)
        self.assertIsInstance(pl.get("layerSeconds"), dict)
        self.assertIn("accept", pl["layerSeconds"])
        self.assertIsInstance(pl["layerSeconds"]["accept"], (int, float))

    def test_accept_event_records_layer_seconds_for_tiered_rules(self):
        repo = self.make_repo()
        (repo / ".delegate.json").write_text(
            json.dumps(
                {
                    "acceptRules": [
                        {
                            "name": "fast-lint",
                            "tier": "fast",
                            "lane": False,
                            "paths": ["notes.txt"],
                            "command": "test -f notes.txt",
                        },
                        {
                            "name": "slow-check",
                            "tier": "slow",
                            "lane": True,
                            "paths": ["notes.txt"],
                            "command": "grep -q tiered notes.txt",
                        },
                    ]
                }
            ),
            encoding="utf-8",
        )
        self.git(repo, "add", ".delegate.json")
        self.git(repo, "commit", "-m", "add tiered accept rules")

        self.fake_pi(
            [fixtures.answer("tiered edit"), fixtures.SETTLED],
            pre="echo tiered >> notes.txt",
        )
        res = self.cli(
            "run",
            "--worktree",
            "--workdir",
            repo,
            "--name",
            "tiered-accept",
            "edit notes",
            cwd=repo,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        run_id = self.outcome(res)["run"]

        acc_evs = [e for e in self.read_events() if e["kind"] == "accept" and e["run"] == run_id]
        self.assertEqual(len(acc_evs), 1, acc_evs)
        pl = acc_evs[0]["payload"]
        self.assertTrue(pl.get("ok"))
        self.assertTrue(pl.get("tiered"))
        self.assertFalse(pl.get("failClosed"))
        self.assertIsInstance(pl.get("elapsedSeconds"), (int, float))
        self.assertIsInstance(pl.get("queuedSeconds"), (int, float))
        layer_secs = pl.get("layerSeconds")
        self.assertIsInstance(layer_secs, dict)
        self.assertIn("fast-lint", layer_secs)
        self.assertIn("slow-check", layer_secs)

    def test_premerge_ready_event_emitted_for_reused_and_rerun(self):
        self.env.pop("DELEGATE_PREMERGE", None)
        repo = self.make_repo()
        self.fake_pi(
            [fixtures.answer("premerge edit"), fixtures.SETTLED],
            pre="echo feature > feature.txt",
        )

        res = self.cli(
            "run",
            "--worktree",
            "--workdir",
            repo,
            "--name",
            "pm-ready-test",
            "--accept",
            "test -f feature.txt",
            "add feature",
            cwd=repo,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        state = self.outcome(res)
        run_id = state["run"]
        run_dir = Path(state["dir"])

        # 1) Initial premerge reuses verified acceptance -> emits `premerge_ready` with reusedAccept=True.
        pm1 = self.wait_premerge(run_dir, expected=("ready",), timeout=15.0)
        self.assertTrue(pm1.get("reusedAccept"), pm1)

        def ready_events():
            return [e for e in self.read_events() if e["kind"] == "premerge_ready" and e["run"] == run_id]

        evs1 = self.wait_until(lambda: ready_events() or None, "first premerge_ready event")
        self.assertEqual(len(evs1), 1, evs1)
        p1 = evs1[0]["payload"]
        self.assertTrue(p1.get("reusedAccept"))
        self.assertEqual(p1.get("baseHead"), pm1["baseHead"])
        self.assertEqual(p1.get("baseShort"), pm1["baseHead"][:7])
        self.assertIsInstance(p1.get("elapsedSeconds"), (int, float))
        self.assertIsInstance(p1.get("ranLayers"), list)

        # 2) Advance source HEAD with an unrelated commit and wake `_merge-worker` via `status`
        # -> re-premerges on new HEAD with reusedAccept=False and emits a second `premerge_ready`.
        (repo / "unrelated.txt").write_text("mainline advance\n", encoding="utf-8")
        self.git(repo, "add", "unrelated.txt")
        self.git(repo, "commit", "-m", "advance mainline")
        new_head = self.git(repo, "rev-parse", "HEAD")

        st = self.cli("status", state["dir"], cwd=repo)
        self.assertEqual(st.returncode, 0, st.stdout + st.stderr)

        pm2 = self.wait_json(
            run_dir / "premerge.json",
            lambda d: d.get("status") == "ready" and d.get("baseHead") == new_head,
            "re-premerge ready on new HEAD",
            timeout=15.0,
        )
        self.assertFalse(pm2.get("reusedAccept"), pm2)

        evs2 = self.wait_until(
            lambda: (lambda cur: cur if len(cur) >= 2 else None)(ready_events()),
            "second premerge_ready event after HEAD advance",
            timeout=15.0,
        )
        p2 = evs2[-1]["payload"]
        self.assertFalse(p2.get("reusedAccept"))
        self.assertEqual(p2.get("baseHead"), new_head)
        self.assertIsInstance(p2.get("elapsedSeconds"), (int, float))

    def test_speed_command_writes_and_rebuilds_eta_model_json(self):
        repo = self.make_repo()
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre="echo ok >> notes.txt",
        )
        res = self.cli(
            "run",
            "--worktree",
            "--workdir",
            repo,
            "--class",
            "build",
            "--accept",
            "test -f notes.txt",
            "--name",
            "speed-sample",
            "build sample",
            cwd=repo,
        )
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        run_id = self.outcome(res)["run"]
        ap = self.cli("apply", run_id, cwd=repo)
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)

        sp = self.cli("speed", "--json", cwd=repo)
        self.assertEqual(sp.returncode, 0, sp.stdout + sp.stderr)

        model_path = self.state_dir() / "eta-model.json"
        self.assertTrue(model_path.is_file(), "delegate speed should write eta-model.json")
        doc = json.loads(model_path.read_text(encoding="utf-8"))
        self.assertEqual(doc.get("version"), 1)
        self.assertGreater(doc.get("eventsSeq", 0), 0)
        self.assertIn("global", doc)
        self.assertIn("priors", doc)
        self.assertEqual(doc["priors"]["formula"]["tChainP50Sec"], 63.0 * 60.0)
        self.assertEqual(doc["priors"]["formula"]["pEffMean"], 2.0)

        repo_key = str(repo.resolve())
        self.assertIn(repo_key, doc["repos"])
        wb = doc["repos"][repo_key]["byModeClass"]["write:build"]
        self.assertEqual(wb["appliedCount"], 1)
        self.assertEqual(wb["stages"]["accept"]["count"], 1)
        self.assertEqual(wb["stages"]["agentWork"]["count"], 1)

        # Derived file is deletable and cleanly rebuildable on next `delegate speed`.
        model_path.unlink()
        self.assertFalse(model_path.exists())
        sp2 = self.cli("speed", cwd=repo)
        self.assertEqual(sp2.returncode, 0, sp2.stdout + sp2.stderr)
        self.assertTrue(model_path.is_file())
        doc2 = json.loads(model_path.read_text(encoding="utf-8"))
        self.assertEqual(doc2["version"], 1)
        self.assertEqual(doc2["eventsSeq"], doc["eventsSeq"])


if __name__ == "__main__":
    unittest.main()
