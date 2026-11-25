"""6.33.0 regressions (W024 low-risk quick wins):
1. P0-3: agy message uses factual reactive-wakeup wording (no foreground polling directive).
2. P1-1: review.json / summary.json / index.json compression-as-index (untruncated files on disk,
   compact counts/samples/pointers in JSON, controllerReview drops inline diff, legacy compat).
3. P1-2: archive/.usage.json atomic cache (read-only fallback without write) & index-backed status.
4. P1-3: agy reviewer ro-workspace moved to <cache>/delegate/review-ro/<run> (searchable by rg,
   cleaned up after review).
5. P1-4: fail-closed auto-clean of applied chain (predecessors immediately, tail conditionally,
   uncommitted tail auto-cleaned by _gc-worker once committed in HEAD), async trash, diff/show
   fallback to archive.
6. P2-1: _gc-worker cold-start / corrupt / version-mismatch index rebuild & archive usage warmup;
   read-only status never writes index.
7. P2-2: agy multi-turn rework does not double-count cumulative tokens; reply records tokensDelta.
"""
import json
import os
import shlex
import shutil
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate633Tests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    fake_docker = fixtures.DelegateTests.fake_docker
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until

    def state_dir(self) -> Path:
        return Path(self.env["XDG_STATE_HOME"]) / "delegate"

    def cache_dir(self) -> Path:
        return Path(self.env["XDG_CACHE_HOME"]) / "delegate"

    def write_user_config(self, doc: dict) -> None:
        cfg_dir = Path(self.env["XDG_CONFIG_HOME"]) / "delegate"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "config.json").write_text(json.dumps(doc))
        for k in ("DELEGATE_SCOUT_MODEL", "DELEGATE_REVIEW_MODEL", "DELEGATE_WRITE_MODEL"):
            self.env.pop(k, None)

    def run_gc_worker_sync(self, repo: Path) -> None:
        """Wait for any background `_gc-worker` to release `.gc-worker.lock`, then run one synchronously."""
        import fcntl

        lock_path = self.state_dir() / ".gc-worker.lock"
        lock_path.parent.mkdir(parents=True, exist_ok=True)
        with open(lock_path, "a+") as f:
            fcntl.flock(f.fileno(), fcntl.LOCK_EX)
            fcntl.flock(f.fileno(), fcntl.LOCK_UN)
        res = self.cli("_gc-worker", cwd=repo)
        self.assertEqual(res.returncode, 0, res.stderr)

    def git(self, repo: Path, *args: str) -> str:
        res = subprocess.run(
            ["git", "-C", str(repo), *args],
            capture_output=True,
            text=True,
            check=True,
        )
        return res.stdout.strip()

    def install_scripted_pi(self, script_body: str) -> Path:
        counter_file = self.work / "pi-call-count.txt"
        pi = self.work / "bin" / "pi"
        pi.parent.mkdir(parents=True, exist_ok=True)
        pi.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "argv = sys.argv[1:]\n"
            "if '--version' in argv:\n"
            "    print('pi 1.0.0')\n"
            "    sys.exit(0)\n"
            "if '--list-models' in argv:\n"
            "    print('provider model context max-out thinking images')\n"
            "    print('test fake-strong 200K 16K yes yes')\n"
            "    print('test fake-review 200K 16K yes yes')\n"
            "    sys.exit(0)\n"
            f"counter = Path({str(counter_file)!r})\n"
            "call_idx = int(counter.read_text()) + 1 if counter.exists() else 1\n"
            "counter.write_text(str(call_idx))\n"
            "sdir = None\n"
            "for i, a in enumerate(argv):\n"
            "    if a == '--session-dir' and i + 1 < len(argv):\n"
            "        sdir = Path(argv[i + 1])\n"
            "if sdir:\n"
            "    sdir.mkdir(parents=True, exist_ok=True)\n"
            "    (sdir / f'sess-{call_idx}.jsonl').write_text(json.dumps({'type':'session','id':f'sess-{call_idx}'}) + '\\n')\n"
            "def emit_answer(text):\n"
            "    print(json.dumps({'type':'session','id':f'sess-{call_idx}'}), flush=True)\n"
            "    print(json.dumps({'type':'message_end','message':{'role':'assistant','provider':'test','model':'fake-strong','stopReason':'stop','usage':{'input':10,'output':5,'cacheRead':0},'content':[{'type':'text','text':text}]}}), flush=True)\n"
            "    print(json.dumps({'type':'agent_settled'}), flush=True)\n"
            + script_body
            + "\n"
        )
        pi.chmod(0o755)
        self.env["DELEGATE_PI_BIN"] = str(pi)
        self.write_user_config(
            {
                "models": {
                    "strong": "test/fake-strong",
                    "rev": "test/fake-review",
                },
                "tiers": {
                    "scout": "strong",
                    "review": "rev",
                    "write": "strong",
                },
            }
        )
        return pi

    def install_scripted_agy(self, script_body: str) -> Path:
        counter_file = self.work / "agy-call-count.txt"
        agy = self.work / "bin" / "agy"
        agy.parent.mkdir(parents=True, exist_ok=True)
        agy.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "argv = sys.argv[1:]\n"
            "if '--version' in argv:\n"
            "    print('1.5.15')\n"
            "    sys.exit(0)\n"
            f"counter = Path({str(counter_file)!r})\n"
            "call_idx = int(counter.read_text()) + 1 if counter.exists() else 1\n"
            "counter.write_text(str(call_idx))\n"
            "raw_in = sys.stdin.readline()\n"
            "msg = json.loads(raw_in.strip().splitlines()[-1]) if raw_in.strip() else {}\n"
            "content = msg.get('message', {}).get('content', '')\n"
            "conv_id = 'conv-fixed-1'\n"
            "for i, a in enumerate(argv):\n"
            "    if a == '--conversation' and i + 1 < len(argv):\n"
            "        conv_id = argv[i + 1]\n"
            "def emit_agy(text, inp=100, out=20, cache=1000, cid=None):\n"
            "    c = cid or conv_id\n"
            "    print(json.dumps({'event':'init','conversation_id':c}), flush=True)\n"
            "    print(json.dumps({'event':'step_update','step_update':{'step_index':1,'step_type':'agent_response','state':'DONE','text_delta':text}}), flush=True)\n"
            "    print(json.dumps({'event':'result','result':{'conversation_id':c,'status':'SUCCESS','response':text,'usage':{'input_tokens':inp,'output_tokens':out,'cache_read_tokens':cache}}}), flush=True)\n"
            + script_body
            + "\n"
        )
        agy.chmod(0o755)
        self.env["PATH"] = f"{agy.parent}:{self.env.get('PATH', '')}"
        self.write_user_config(
            {
                "models": {
                    "neon": "agy/gemini-4-neon",
                },
                "tiers": {
                    "scout": "neon",
                    "review": "neon",
                    "write": "neon",
                },
            }
        )
        return agy

    def test_p1_1_review_compression_as_index_and_legacy_compat(self):
        repo = self.repo(
            {
                "src/a.txt": "v0\n",
                "prompts/p.md": "p0\n",
                ".delegate.json": json.dumps(
                    {
                        "review": True,
                        "controllerReviewPaths": ["prompts/**"],
                    }
                ),
            }
        )
        # Reviewer writes 40 unignored files -> ro-violations must be stored untruncated on disk,
        # while review.json / summary.json / index.json stay compact (<2 KB).
        self.install_scripted_pi(
            "if call_idx == 1:\n"
            "    Path('src/a.txt').write_text('v1\\n')\n"
            "    Path('prompts/p.md').write_text('p1\\n' + 'z' * 8000 + '\\n')\n"
            "    emit_answer('done')\n"
            "else:\n"
            "    for i in range(40):\n"
            "        Path(f'rogue_{i:02d}.txt').write_text('bad')\n"
            "    emit_answer('PASS\\nreviewed')\n"
        )
        st = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "ro-bloat", "--accept", "true", "task")
        )
        run_dir = Path(st["dir"])
        review_json = json.loads((run_dir / "review.json").read_text())
        summary_json = json.loads((run_dir / "summary.json").read_text())

        # Full list saved untruncated to separate file
        ro_file = Path(review_json["roViolationFile"])
        self.assertTrue(ro_file.is_file())
        lines = ro_file.read_text().splitlines()
        self.assertEqual(len(lines), 40)
        self.assertIn("rogue_39.txt", lines)

        # JSON stays compact: counts + 5-item sample + file pointer, no inline controllerReview.diff
        self.assertEqual(review_json["roViolationCount"], 40)
        self.assertEqual(len(review_json["roViolationSample"]), 5)
        self.assertNotIn("readOnlyViolation", review_json)
        self.assertLessEqual(len(review_json["parseError"]), 243)
        self.assertNotIn("diff", review_json["controllerReview"])
        self.assertTrue(Path(review_json["controllerReview"]["diffFile"]).is_file())
        self.assertGreater(review_json["controllerReview"]["diffBytes"], 8000)

        # summary.json["review"] and index.json do NOT inline rounds[] or full diff
        self.assertNotIn("rounds", summary_json["review"])
        self.assertNotIn("diff", summary_json["review"]["controllerReview"])
        self.assertLess(len(json.dumps(summary_json["review"])), 2048)

        idx = json.loads((self.state_dir() / "index.json").read_text())
        idx_entry = idx["runs"][str(run_dir.resolve())]
        self.assertNotIn("rounds", idx_entry["status"]["review"])
        self.assertNotIn("diff", idx_entry["status"]["review"]["controllerReview"])
        self.assertLess(len(json.dumps(idx_entry["status"]["review"])), 2048)

        # Legacy 6.32 bloated review.json is still readable and compacted on status read
        bloated = {
            "status": "done",
            "verdict": "NEEDS_CONTROLLER",
            "round": 1,
            "reworkRounds": 0,
            "upgradedBy": "readOnlyViolation",
            "parseError": "E" * 5000,
            "readOnlyViolation": [f"old_{i}.txt" for i in range(100)],
            "controllerReview": {
                "paths": ["prompts/p.md"],
                "hash": review_json["controllerReview"]["hash"],
                "diffFile": review_json["controllerReview"]["diffFile"],
                "diff": "D" * 10000,
            },
            "rounds": [{"round": 1, "readOnlyViolation": [f"old_{i}.txt" for i in range(100)]}],
        }
        (run_dir / "review.json").write_text(json.dumps(bloated))
        legacy_st = self.outcome(self.cli("status", st["run"]))
        self.assertEqual(legacy_st["review"]["roViolationCount"], 100)
        self.assertEqual(len(legacy_st["review"]["roViolationSample"]), 5)
        self.assertNotIn("diff", legacy_st["review"]["controllerReview"])
        self.assertNotIn("rounds", legacy_st["review"])
        self.assertLessEqual(len(legacy_st["review"]["parseError"]), 243)

    def test_p0_3_p1_3_p2_2_agy_prompt_ro_workspace_rg_and_tokens_delta(self):
        repo = self.repo(
            {
                ".gitignore": ".local/\n",
                "src/findme.rs": "pub fn unique_needle_633() -> i32 { 7 }\n",
                ".delegate.json": json.dumps({"review": True}),
            }
        )
        probe_file = self.work / "agy-probe.json"
        rg_bin = shutil.which("rg")
        self.install_scripted_agy(
            f"import subprocess\n"
            f"probe = Path({str(probe_file)!r})\n"
            f"records = json.loads(probe.read_text()) if probe.exists() else []\n"
            f"cwd = os.getcwd()\n"
            f"rg_out = ''\n"
            f"if {rg_bin!r}:\n"
            f"    r = subprocess.run([{rg_bin!r}, '-l', 'unique_needle_633', '.'], capture_output=True, text=True)\n"
            f"    rg_out = r.stdout.strip()\n"
            f"records.append({{'call': call_idx, 'cwd': cwd, 'content': content, 'rg_out': rg_out}})\n"
            f"probe.write_text(json.dumps(records))\n"
            f"if call_idx == 1:\n"
            f"    Path('src/findme.rs').write_text('pub fn unique_needle_633() -> i32 {{ 8 }}\\n')\n"
            f"    emit_agy('turn 1', inp=100, out=20, cache=5000, cid='conv-author')\n"
            f"elif call_idx == 2:\n"
            f"    # Reviewer round 1 -> REWORK\n"
            f"    emit_agy('REWORK\\nchange to 9', inp=50, out=10, cache=200, cid='conv-rev')\n"
            f"elif call_idx == 3:\n"
            f"    # Author rework turn 2 -> cumulative tokens (NOT delta!)\n"
            f"    Path('src/findme.rs').write_text('pub fn unique_needle_633() -> i32 {{ 9 }}\\n')\n"
            f"    emit_agy('turn 2 fixed', inp=160, out=45, cache=8500, cid='conv-author')\n"
            f"elif call_idx == 4:\n"
            f"    # Reviewer round 2 -> PASS\n"
            f"    emit_agy('PASS\\nall good', inp=80, out=15, cache=400, cid='conv-rev')\n"
            f"else:\n"
            f"    # Reply child continuing same session -> cumulative tokens\n"
            f"    Path('src/findme.rs').write_text('pub fn unique_needle_633() -> i32 {{ 10 }}\\n')\n"
            f"    emit_agy('reply done', inp=210, out=65, cache=11000, cid='conv-author')\n"
        )
        r1 = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "agy-test", "--accept", "true", "update needle")
        )
        self.assertEqual(r1["review"]["verdict"], "PASS")
        self.assertEqual(r1["review"]["reworkRounds"], 1)

        # P2-2: agy rework took last turn's cumulative tokens (160/45/8500), NOT sum (260/65/13500)!
        self.assertEqual(r1["tokens"], {"input": 160, "output": 45, "cacheRead": 8500})
        self.assertNotIn("tokensDelta", r1)

        # P0-3 & P1-3 checks from probe
        records = json.loads(probe_file.read_text())
        self.assertGreaterEqual(len(records), 4)
        # P0-3: factual background-wakeup wording, no old foreground polling sentence
        self.assertIn("Long commands move to the background after 10 seconds", records[0]["content"])
        self.assertNotIn("Run commands in the foreground", records[0]["content"])

        # P1-3: reviewer ran in <cache>/delegate/review-ro/<run>, outside repo, rg found findme.rs,
        # and ro-workspace was removed after review finished.
        rev_cwd = Path(records[1]["cwd"])
        self.assertIn("review-ro", str(rev_cwd))
        self.assertFalse(str(rev_cwd).startswith(str(repo.resolve())))
        if rg_bin:
            self.assertIn("src/findme.rs", records[1]["rg_out"])
        self.assertFalse(rev_cwd.exists(), "ro-workspace should be cleaned up after review")

        # P2-2: reply child continuing same session records tokensDelta = child - parent last turn
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "set to 10"))
        self.assertEqual(r2["tokens"], {"input": 210, "output": 65, "cacheRead": 11000})
        self.assertEqual(r2["tokensDelta"], {"input": 50, "output": 20, "cacheRead": 2500})

    def test_p1_4_apply_auto_cleans_chain_and_diff_show_fallback_to_archive(self):
        repo = self.repo({"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("round 1"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r1 = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "chain-auto", "--accept", "true", "r1")
        )
        self.fake_pi([fixtures.answer("round 2"), fixtures.SETTLED], pre="echo v2 > a.txt")
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "r2"))

        # Apply with acceptStillValid=true -> both predecessor r1 and tail r2 auto-clean & archive!
        res = self.cli("apply", "--json", r2["run"])
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        self.assertEqual((repo / "a.txt").read_text(), "v2\n")
        self.assertFalse(Path(r1["dir"]).exists(), "predecessor r1 must be auto-cleaned")
        self.assertFalse(Path(r2["dir"]).exists(), "tail r2 (acceptStillValid) must be auto-cleaned")
        self.assertFalse(Path(r2["worktree"]).exists(), "worktree must be removed")

        # `diff` and `show` fall back cleanly to archived record!
        d_id = self.cli("diff", r2["run"], cwd=repo)
        self.assertEqual(d_id.returncode, 0, d_id.stderr)
        self.assertIn("+v2", d_id.stdout)
        self.assertIn("已清理归档", d_id.stderr)

        d_name = self.cli("diff", "chain-auto", "--stat", cwd=repo)
        self.assertEqual(d_name.returncode, 0, d_name.stderr)
        self.assertIn("a.txt", d_name.stdout)

        sh = self.cli("show", r2["run"], cwd=repo)
        self.assertEqual(sh.returncode, 0, sh.stderr)
        self.assertIn("round 2", sh.stdout)

    def test_p1_4_uncommitted_apply_verify_false_keeps_tail_until_committed_then_gc_cleans(self):
        repo = self.repo(
            {
                "a.txt": "v0\n",
                "other.txt": "x0\n",
                ".delegate.json": json.dumps({"applyVerify": False}),
            }
        )
        self.fake_pi([fixtures.answer("r1"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r1 = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "defer-clean", "--accept", "true", "r1")
        )
        self.fake_pi([fixtures.answer("r2"), fixtures.SETTLED], pre="echo v2 > a.txt")
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "r2"))

        # Advance source HEAD so acceptStillValid == false and applyVerify == false
        (repo / "other.txt").write_text("x1\n")
        self.git(repo, "add", "other.txt")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "advance head")

        res = self.cli("apply", "--json", r2["run"])
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        # Predecessor r1 is cleaned immediately; tail r2 stays lightweight `.applied-cleaned`!
        self.assertFalse(Path(r1["dir"]).exists(), "predecessor r1 should be auto-cleaned")
        self.assertTrue(Path(r2["dir"]).is_dir(), "uncommitted tail r2 should stay until committed")
        self.assertTrue((Path(r2["dir"]) / ".applied-cleaned").exists())
        self.assertFalse(Path(r2["worktree"]).exists(), "worktree staged to trash immediately")

        # Running _gc-worker before commit keeps r2 intact
        self.run_gc_worker_sync(repo)
        self.assertTrue(Path(r2["dir"]).is_dir())

        # Once controller commits a.txt into HEAD, _gc-worker auto-cleans r2!
        self.git(repo, "add", "a.txt")
        self.git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "land r2")
        self.run_gc_worker_sync(repo)
        self.assertFalse(Path(r2["dir"]).exists(), "_gc-worker should auto-clean committed tail r2")

    def test_p1_2_p2_1_archive_usage_cache_and_gc_worker_cold_start_index_rebuild(self):
        repo = self.repo({"a.txt": "v0\n"})
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED], pre="echo v1 > a.txt")
        r = self.outcome(
            self.cli("run", "--workdir", repo, "--worktree", "--name", "cold-idx", "--accept", "true", "t")
        )
        self.assertEqual(self.cli("apply", "--json", r["run"]).returncode, 0)
        # Let background _gc-worker spawned by apply finish before testing cold-start rebuild.
        self.run_gc_worker_sync(repo)

        usage_cache = self.cache_dir() / "archive" / ".usage.json"
        self.assertTrue(usage_cache.is_file(), "preserve must write archive/.usage.json")
        u_doc = json.loads(usage_cache.read_text())
        self.assertGreaterEqual(u_doc["count"], 1)
        self.assertGreater(u_doc["bytes"], 0)

        # Delete both index.json and .usage.json: read-only `status` must NOT create either!
        idx_file = self.state_dir() / "index.json"
        if idx_file.exists():
            idx_file.unlink()
        usage_cache.unlink()

        st_res = self.cli("status", "--json", cwd=repo)
        self.assertEqual(st_res.returncode, 0, st_res.stderr)
        self.assertFalse(idx_file.exists(), "read-only status must not write index.json")
        self.assertFalse(usage_cache.exists(), "read-only status must not write .usage.json")
        summary_line = json.loads(st_res.stdout.strip().splitlines()[-1])["statusSummary"]
        self.assertEqual(summary_line["archiveCount"], u_doc["count"])

        # `_gc-worker` rebuilds missing/corrupt/wrong-version index.json AND warms .usage.json!
        self.run_gc_worker_sync(repo)
        self.assertTrue(idx_file.is_file())
        self.assertTrue(usage_cache.is_file())

        # Corrupt index.json -> `_gc-worker` repairs it
        idx_file.write_text("{corrupt")
        self.run_gc_worker_sync(repo)
        repaired = json.loads(idx_file.read_text())
        self.assertEqual(repaired["schemaVersion"], 1)
        self.assertIsInstance(repaired["runs"], dict)

        # Wrong schemaVersion -> `_gc-worker` rebuilds it
        idx_file.write_text(json.dumps({"schemaVersion": 999, "runs": {}}))
        self.run_gc_worker_sync(repo)
        rebuilt = json.loads(idx_file.read_text())
        self.assertEqual(rebuilt["schemaVersion"], 1)


if __name__ == "__main__":
    unittest.main()
