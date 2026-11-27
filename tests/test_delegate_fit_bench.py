"""End-to-end tests for `delegate fit bench review`:
- Selects historical write runs with known review outcomes (missed / caught, balanced)
- `--estimate` estimates tokens & duration from original review usage with zero model calls
- Missing `--yes` prints estimate and refuses to dispatch (exit 2, zero model calls)
- `--yes` reconstructs pre-rework snapshots for auto-reworked caught runs (verified by inspecting
  worktree file contents, not just prompts), fails closed when legacy archives lack round-1
  reviewedTree & total.diff, skips unreconstructable runs, computes hit rate / missed / errors /
  total usage, records summary into fit.json when valid > 0, and never records `avoid` when all
  calls fail with errors.
"""
import json
import unittest
from pathlib import Path

import test_delegate as fixtures


class DelegateFitBenchTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    cli = fixtures.DelegateTests.cli
    repo = fixtures.DelegateTests.repo

    def write_user_config(self, doc: dict) -> None:
        cfg_dir = Path(self.env["XDG_CONFIG_HOME"]) / "delegate"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "config.json").write_text(json.dumps(doc))
        for k in (
            "DELEGATE_SCOUT_MODEL",
            "DELEGATE_REVIEW_MODEL",
            "DELEGATE_WRITE_MODEL",
            "DELEGATE_THINKING",
        ):
            self.env.pop(k, None)

    def make_repo(self) -> Path:
        return self.repo(
            {
                "README.md": "hello\n",
                ".delegate.json": json.dumps(
                    {
                        "scoutFirst": False,
                        "review": {"tier": "review"},
                        "accept": "true",
                    }
                ),
            }
        )

    def install_scripted_agy(self) -> Path:
        log_file = self.work / "agy-calls.jsonl"
        state_file = self.work / "agy-rework-seen.json"
        agy = self.work / "bin" / "agy"
        agy.parent.mkdir(parents=True, exist_ok=True)
        script = f"""#!/usr/bin/env python3
import json, os, sys
from pathlib import Path

argv = sys.argv[1:]
if '--version' in argv:
    print('1.5.15')
    sys.exit(0)

model = ''
conv_in = ''
for i, a in enumerate(argv):
    if a == '--model' and i + 1 < len(argv):
        model = argv[i + 1]
    elif a == '--conversation' and i + 1 < len(argv):
        conv_in = argv[i + 1]

raw_in = sys.stdin.readline()
msg = json.loads(raw_in.strip().splitlines()[-1]) if raw_in.strip() else {{}}
content = msg.get('message', {{}}).get('content', '')

log_path = Path({str(log_file)!r})
calls = log_path.read_text().splitlines() if log_path.exists() else []
call_idx = len(calls) + 1
with log_path.open('a') as f:
    f.write(json.dumps({{'idx': call_idx, 'model': model, 'conv_in': conv_in, 'cwd': os.getcwd(), 'content': content}}) + '\\n')

conv_id = conv_in or f'conv-{{model}}-{{call_idx}}'

def emit_ok(text):
    print(json.dumps({{'event': 'init', 'conversation_id': conv_id}}), flush=True)
    print(json.dumps({{'event': 'step_update', 'step_update': {{'step_index': 1, 'step_type': 'agent_response', 'state': 'DONE', 'text_delta': text}}}}), flush=True)
    print(json.dumps({{'event': 'result', 'result': {{'conversation_id': conv_id, 'status': 'SUCCESS', 'response': text, 'usage': {{'input_tokens': 40, 'output_tokens': 15, 'cache_read_tokens': 5}}}}}}), flush=True)

def emit_err(err_msg):
    print(json.dumps({{'event': 'init', 'conversation_id': conv_id}}), flush=True)
    print(json.dumps({{'event': 'result', 'result': {{'conversation_id': conv_id, 'status': 'ERROR', 'error': err_msg, 'response': ''}}}}), flush=True)
    sys.stderr.write(err_msg + '\\n')
    sys.exit(1)

state_path = Path({str(state_file)!r})
seen = json.loads(state_path.read_text()) if state_path.exists() else {{}}

if model == 'write-m':
    marker = 'default'
    for tag in ('missed-1', 'missed-2', 'missed-broken', 'caught-1', 'caught-2'):
        if tag in content:
            marker = tag
            break
    target = Path(f'{{marker}}.txt')
    prev = target.read_text() if target.exists() else ''
    target.write_text(prev + f'edit-{{call_idx}}\\n')
    emit_ok(f'done {{marker}}')
elif model == 'orig-rev-m':
    tag = None
    for t in ('caught-1', 'caught-2'):
        if t in content:
            tag = t
            break
    if tag and not seen.get(tag):
        seen[tag] = True
        state_path.write_text(json.dumps(seen))
        emit_ok(f'REWORK\\nMissing edge case check in {{tag}}.')
    else:
        emit_ok('PASS\\nLooks good.')
elif model == 'cand-good-m':
    # Inspect actual reconstructed worktree contents:
    # For caught-1 / caught-2, the pre-rework snapshot must have exactly 1 line (before round-2 fix).
    # If it mistakenly sees the 2-line post-rework version, it returns PASS so the test catches the bug.
    for t in ('caught-1', 'caught-2'):
        p = Path(f'{{t}}.txt')
        if p.exists():
            lines = [ln for ln in p.read_text().splitlines() if ln.strip()]
            if len(lines) == 1:
                emit_ok(f'REWORK\\nCaught pre-rework defect in {{t}}.txt (1 line).')
            else:
                emit_ok(f'PASS\\n{{t}}.txt already fixed ({{len(lines)}} lines).')
            sys.exit(0)
    if Path('missed-1.txt').exists():
        emit_ok('REWORK\\nCaught defect in missed-1.txt.')
    elif Path('missed-2.txt').exists():
        emit_ok('PASS\\nMissed defect in missed-2.txt.')
    else:
        emit_ok('PASS\\nNo target file found in reconstructed worktree.')
elif model == 'cand-bad-m':
    emit_ok('PASS\\nEverything looks fine.')
elif model == 'cand-err-m':
    emit_err('RESOURCE_EXHAUSTED: simulated quota failure')
else:
    emit_ok('PASS\\nDefault.')
"""
        agy.write_text(script)
        agy.chmod(0o755)
        self.env["PATH"] = f"{agy.parent}:{self.env.get('PATH', '')}"
        self.write_user_config(
            {
                "models": {
                    "writer": {"model": "agy/write-m", "cost": "main"},
                    "orig-rev": {"model": "agy/orig-rev-m", "cost": "main"},
                    "cand-good": {"model": "agy/cand-good-m", "cost": "low"},
                    "cand-bad": {"model": "agy/cand-bad-m", "cost": "low"},
                    "cand-err": {"model": "agy/cand-err-m", "cost": "low"},
                },
                "tiers": {
                    "scout": "orig-rev",
                    "review": "orig-rev",
                    "write": "writer",
                },
            }
        )
        return log_file

    def _parse_run_id(self, stdout: str) -> str:
        for line in stdout.splitlines():
            line = line.strip()
            if not line.startswith("{"):
                continue
            doc = json.loads(line)
            if isinstance(doc, dict) and doc.get("run"):
                return doc["run"]
        raise AssertionError(f"no run id found in stdout:\n{stdout}")

    def _run_write(self, repo: Path, name: str, prompt: str) -> str:
        res = self.cli("run", "--worktree", "--no-scout", "--name", name, "--json", prompt, cwd=repo)
        self.assertEqual(res.returncode, 0, f"run {name} failed: {res.stderr}\n{res.stdout}")
        return self._parse_run_id(res.stdout)

    def _reply_write(self, repo: Path, run_id: str, prompt: str) -> str:
        res = self.cli("reply", run_id, "--no-scout", "--no-sync", "--json", prompt, cwd=repo)
        self.assertEqual(res.returncode, 0, f"reply {run_id} failed: {res.stderr}\n{res.stdout}")
        child_id = self._parse_run_id(res.stdout)
        wait = self.cli("wait", child_id, cwd=repo)
        self.assertEqual(wait.returncode, 0, f"wait {child_id} failed: {wait.stderr}\n{wait.stdout}")
        return child_id

    def test_fit_bench_review_estimate_gate_replay_skip_and_record(self):
        repo = self.make_repo()
        log_file = self.install_scripted_agy()

        # 1. Create 2 missed runs (orig reviewer PASS -> controller replies to rework).
        r_missed_1 = self._run_write(repo, "missed-1", "implement missed-1 feature")
        self._reply_write(repo, r_missed_1, "fix bug in missed-1")

        r_missed_2 = self._run_write(repo, "missed-2", "implement missed-2 feature")
        self._reply_write(repo, r_missed_2, "fix bug in missed-2")

        # 2. Create 2 caught runs (orig reviewer REWORK in round 1 -> author fixes -> PASS in round 2).
        r_caught_1 = self._run_write(repo, "caught-1", "implement caught-1 feature")
        r_caught_2 = self._run_write(repo, "caught-2", "implement caught-2 feature")

        # 3. Create 1 additional missed run whose archive we will corrupt so it cannot be reconstructed.
        r_broken = self._run_write(repo, "missed-broken", "implement missed-broken feature")
        self._reply_write(repo, r_broken, "fix bug in missed-broken")

        # Archive & clean all finished runs so bench reconstructs from archive snapshots.
        clean = self.cli("clean", "--finished", "--force", "--sync", cwd=repo)
        self.assertEqual(clean.returncode, 0, clean.stderr)

        arch_root = Path(self.env["XDG_CACHE_HOME"]) / "delegate" / "archive"

        # Verify archive preserved REWORK round total.diff and review.json recorded round reviewedTree.
        caught_1_rec = arch_root / r_caught_1 / "record"
        self.assertTrue((caught_1_rec / "review" / "round-1" / "total.diff").is_file())
        caught_1_rev = json.loads((caught_1_rec / "review.json").read_text())
        self.assertTrue(caught_1_rev["rounds"][0].get("reviewedTree"))
        self.assertTrue(caught_1_rev["rounds"][0].get("reviewedBaseTree"))

        # For caught-1, remove reviewedTree from rounds[0] so bench exercises Path 2
        # (applying archived `review/round-1/total.diff` onto base).
        caught_1_rev["rounds"][0].pop("reviewedTree", None)
        (caught_1_rec / "review.json").write_text(json.dumps(caught_1_rev))

        # Corrupt the archived snapshot of r_broken so reconstruct_in_snapshot fails.
        arch_record = arch_root / r_broken / "record"
        self.assertTrue(arch_record.is_dir(), f"expected archive record at {arch_record}")
        for fname in ("changes.patch", "applied.patch", "changes.json", ".applied"):
            p = arch_record / fname
            if p.exists():
                p.unlink()
        meta_path = arch_record / "meta.json"
        meta_doc = json.loads(meta_path.read_text())
        meta_doc["base"] = {}
        meta_doc["worktree"] = {"source": "/nonexistent/deleted-repo", "path": "/nonexistent/wt"}
        meta_path.write_text(json.dumps(meta_doc))

        calls_before = len(log_file.read_text().splitlines())

        # A. Unknown alias fails with exit 2.
        bad_alias = self.cli("fit", "bench", "review", "no-such-alias", "--estimate")
        self.assertEqual(bad_alias.returncode, 2)
        self.assertIn("unknown alias", bad_alias.stderr)

        # B. `--estimate` estimates tokens & duration with ZERO model calls.
        est_res = self.cli("fit", "bench", "review", "cand-good", "-n", "10", "--estimate", "--json")
        self.assertEqual(est_res.returncode, 0, est_res.stderr)
        est = json.loads(est_res.stdout)
        self.assertTrue(est["estimateOnly"])
        self.assertFalse(est["dispatched"])
        self.assertFalse(est["requiresYes"])
        self.assertEqual(est["selectedCount"], 5)
        self.assertEqual(est["selectedMissed"], 3)
        self.assertEqual(est["selectedCaught"], 2)
        self.assertGreater(est["estimate"]["tokens"]["total"], 0)
        self.assertEqual(len(log_file.read_text().splitlines()), calls_before)

        # Balanced selection with -n 2 picks 1 missed and 1 caught.
        est_bal = self.cli("fit", "bench", "review", "cand-good", "-n", "2", "--estimate", "--json")
        self.assertEqual(est_bal.returncode, 0, est_bal.stderr)
        bal = json.loads(est_bal.stdout)
        self.assertEqual(bal["selectedCount"], 2)
        self.assertEqual(bal["selectedMissed"], 1)
        self.assertEqual(bal["selectedCaught"], 1)

        # C. Missing `--yes` prints estimate, returns exit 2, and dispatches ZERO model calls.
        no_yes = self.cli("fit", "bench", "review", "cand-good", "-n", "10", "--json")
        self.assertEqual(no_yes.returncode, 2, no_yes.stderr)
        no_yes_doc = json.loads(no_yes.stdout)
        self.assertFalse(no_yes_doc["estimateOnly"])
        self.assertFalse(no_yes_doc["dispatched"])
        self.assertTrue(no_yes_doc["requiresYes"])
        self.assertGreater(no_yes_doc["estimate"]["tokens"]["total"], 0)
        self.assertEqual(len(log_file.read_text().splitlines()), calls_before)

        # Human output without --yes also mentions --yes.
        no_yes_human = self.cli("fit", "bench", "review", "cand-good", "-n", "10")
        self.assertEqual(no_yes_human.returncode, 2)
        self.assertIn("--yes", no_yes_human.stdout)
        self.assertEqual(len(log_file.read_text().splitlines()), calls_before)

        # D. Confirmed `--yes` replays the 4 valid runs (with pre-rework 1-line caught snapshots),
        #    skips the 1 broken run, and records fit.json.
        run_res = self.cli("fit", "bench", "review", "cand-good", "-n", "10", "--yes", "--json")
        self.assertEqual(run_res.returncode, 0, run_res.stderr)
        out = json.loads(run_res.stdout)
        self.assertTrue(out["dispatched"])
        self.assertEqual(out["selectedCount"], 5)
        self.assertEqual(out["evaluated"], 4)
        self.assertEqual(out["skipped"], 1)
        self.assertEqual(out["hits"], 3, json.dumps(out, ensure_ascii=False, indent=2))
        self.assertEqual(out["missed"], 1)
        self.assertEqual(out["falsePositives"], 0)
        self.assertEqual(out["errors"], 0)
        self.assertAlmostEqual(out["hitRate"], 0.75)
        self.assertGreater(out["usage"]["tokens"]["total"], 0)
        self.assertTrue(out["fitRecorded"])
        self.assertEqual(out["fitVerdict"], "ok")
        self.assertLessEqual(len(out["fitNote"]), 80)

        skipped_items = [r for r in out["runs"] if r["status"] == "skipped"]
        self.assertEqual(len(skipped_items), 1)
        self.assertEqual(skipped_items[0]["run"], r_broken)
        self.assertTrue(skipped_items[0]["skipReason"])

        # Exactly 4 new candidate model calls were made.
        all_calls = [json.loads(line) for line in log_file.read_text().splitlines()]
        new_calls = all_calls[calls_before:]
        self.assertEqual(len(new_calls), 4)
        self.assertTrue(all(c["model"] == "cand-good-m" for c in new_calls))

        # Verify fit.json via `delegate fit --json`.
        fit_card = json.loads(self.cli("fit", "--json").stdout)
        entry = fit_card["review"]["cand-good"]
        self.assertEqual(entry["verdict"], "ok")
        self.assertEqual(entry["note"], out["fitNote"])
        self.assertEqual(entry["sample"], out["fitSample"])

        # E. All-error candidate (`cand-err`): evaluated == 0, errors == 4, hitRate is None,
        #    MUST NOT record `avoid` in fit.json!
        err_res = self.cli("fit", "bench", "review", "cand-err", "-n", "10", "--yes", "--json")
        self.assertEqual(err_res.returncode, 0, err_res.stderr)
        err_out = json.loads(err_res.stdout)
        self.assertEqual(err_out["evaluated"], 0)
        self.assertEqual(err_out["errors"], 4)
        self.assertEqual(err_out["skipped"], 1)
        self.assertIsNone(err_out["hitRate"])
        self.assertFalse(err_out["fitRecorded"])
        fit_card_after_err = json.loads(self.cli("fit", "--json").stdout)
        self.assertNotIn("cand-err", fit_card_after_err.get("review", {}))

        # F. Legacy auto-reworked caught archive missing both round-1 reviewedTree & total.diff
        #    MUST fail closed and be skipped (never falling back to post-rework changes.patch).
        caught_2_rec = arch_root / r_caught_2 / "record"
        caught_2_rev = json.loads((caught_2_rec / "review.json").read_text())
        caught_2_rev["rounds"][0].pop("reviewedTree", None)
        caught_2_rev["rounds"][0].pop("reviewedBaseTree", None)
        (caught_2_rec / "review.json").write_text(json.dumps(caught_2_rev))
        td2 = caught_2_rec / "review" / "round-1" / "total.diff"
        if td2.exists():
            td2.unlink()

        bad_res = self.cli("fit", "bench", "review", "cand-bad", "-n", "10", "--yes", "--json")
        self.assertEqual(bad_res.returncode, 0, bad_res.stderr)
        bad_out = json.loads(bad_res.stdout)
        self.assertEqual(bad_out["evaluated"], 3, json.dumps(bad_out, ensure_ascii=False, indent=2))
        self.assertEqual(bad_out["skipped"], 2)
        self.assertEqual(bad_out["hits"], 0)
        self.assertEqual(bad_out["missed"], 3)
        self.assertEqual(bad_out["fitVerdict"], "avoid")
        skipped_bad_ids = {r["run"] for r in bad_out["runs"] if r["status"] == "skipped"}
        self.assertEqual(skipped_bad_ids, {r_broken, r_caught_2})

        fit_card2 = json.loads(self.cli("fit", "--json").stdout)
        self.assertEqual(fit_card2["review"]["cand-bad"]["verdict"], "avoid")
        self.assertLessEqual(len(fit_card2["review"]["cand-bad"]["note"]), 80)

        # Ensure the 4 valid runs in step D were all included.
        eval_ids = {r["run"] for r in out["runs"] if r["status"] == "evaluated"}
        self.assertEqual(eval_ids, {r_missed_1, r_missed_2, r_caught_1, r_caught_2})


if __name__ == "__main__":
    unittest.main()
