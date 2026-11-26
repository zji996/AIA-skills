"""End-to-end tests for two-step scout-first default on write tasks (`delegate start --worktree`).

Covers:
- Default scout-first flow (read-only scout via `tiers.scout` -> deterministic L0/L1 split & anchor check ->
  `scout-map.md` persistence -> fresh writer session with verbatim appendix + L0 + map path, no symlink/--share)
- Three exceptions (`--no-scout`, `.delegate.json` `"scoutFirst": false`, lightweight brief) + counter-cases
- Read-only / `--tier scout|review` / `reply` never scout
- Dropped maps: missing L0, bad anchors > 20%, and scout run failure -> writer still runs without map,
  `scout_map_dropped` recorded in card & events
- L0 section 3 mismatch -> enters `question` state (not rework) with reply hint; `delegate reply` launches
  fresh writer with controller answer appended at end of writer prompt
- Fake `agy`: writer is a brand-new conversation (no `--conversation` continuing scout), `preEditSteps` counting
- `delegate lines`: includes `scoutRun` tokens in write task's per-million-token metric
- `queue add --no-scout` passthrough
"""
import json
import os
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


VALID_MAP = """\
## L0
### 1. 落点
1. `src/alpha.rs` :: `alpha_fn` (L1-L3) — 主实现落点
2. `src/beta.rs` :: `beta_fn` (L1-L3) — 调用方落点

### 2. 跨文件关系
`beta_fn` 调用 `alpha_fn`；测试命令 `true`。

### 3. 与说明不符之处
无

### 4. 未核实项
无

## L1
完整调用表：`src/alpha.rs :: alpha_fn (L1-L3)` -> `src/beta.rs :: beta_fn (L1-L3)`。
"""

MISMATCH_MAP = """\
## L0
### 1. 落点
1. `src/alpha.rs` :: `alpha_fn` (L1-L3) — 实际函数名是 `alpha_fn`

### 2. 跨文件关系
无特殊依赖。

### 3. 与说明不符之处
1. 说明里提到 `old_alpha`，但代码里实际叫 `alpha_fn` 且签名不同。

### 4. 未核实项
无

## L1
详见 `src/alpha.rs :: alpha_fn (L1-L3)`。
"""

BAD_ANCHOR_MAP = """\
## L0
### 1. 落点
1. `src/alpha.rs` :: `alpha_fn` (L1-L3) — 正确落点
2. `src/beta.rs` :: `beta_fn` (L1-L3) — 正确落点
3. `src/alpha.rs` :: `ghost_symbol` (L1-L2) — 错误符号
4. `src/missing.rs` :: `absent_fn` (L1-L2) — 不存在文件

### 2. 跨文件关系
无。

### 3. 与说明不符之处
无

### 4. 未核实项
无

## L1
details
"""


def _pi_tool_start(tool: str, args: dict) -> dict:
    return {"type": "tool_execution_start", "toolName": tool, "args": args}


def _pi_answer_with_usage(text: str, inp: int = 1000, out: int = 500, cache_read: int = 0) -> dict:
    return {
        "type": "message_end",
        "message": {
            "role": "assistant",
            "stopReason": "stop",
            "model": "fake-model",
            "content": [{"type": "text", "text": text}],
            "usage": {"input": inp, "output": out, "cacheRead": cache_read, "cacheWrite": 0},
        },
    }


class DelegateScoutFirstTests(unittest.TestCase):
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    fake_pi = fixtures.DelegateTests.fake_pi
    repo = fixtures.DelegateTests.repo
    user_config = fixtures.DelegateTests.user_config

    def setUp(self):
        fixtures.DelegateTests.setUp(self)
        # Exercise the real default (scout-first ON).
        self.env.pop("DELEGATE_SCOUT_FIRST", None)
        models = dict(fixtures.REGISTERED_MODELS)
        models["scout"] = {"model": "p/scout-m", "thinking": "medium"}
        models["review"] = {"model": "p/review-m", "thinking": "medium"}
        models["write"] = {"model": "p/write-m", "thinking": "high"}
        self.user_config({
            "models": models,
            "tiers": {
                "scout": "scout",
                "review": "review",
                "write": "write",
            },
        })
        for k in ("DELEGATE_SCOUT_MODEL", "DELEGATE_REVIEW_MODEL", "DELEGATE_WRITE_MODEL"):
            self.env.pop(k, None)

    def _base_repo(self, extra: dict | None = None) -> str:
        files = {
            "src/alpha.rs": "pub fn alpha_fn() -> i32 {\n    1\n}\n",
            "src/beta.rs": "pub fn beta_fn() -> i32 {\n    2\n}\n",
            "src/gamma.rs": "pub fn gamma_fn() -> i32 {\n    3\n}\n",
            "src/delta.rs": "pub fn delta_fn() -> i32 {\n    4\n}\n",
        }
        if extra:
            files.update(extra)
        return self.repo(files)

    def _run_dir(self, run_id: str) -> Path:
        return self.work / "runs" / run_id

    def _global_events(self) -> list[dict]:
        ev_path = self.work / "state" / "delegate" / "events.jsonl"
        if not ev_path.is_file():
            return []
        return [json.loads(line) for line in ev_path.read_text(encoding="utf-8").splitlines() if line.strip()]

    def test_default_scout_then_write_full_flow_and_accounting(self):
        # Attempt 1 = scout run (read-only), Attempt 2 = writer run (fresh session with 3 pre-edit steps)
        self.fake_pi(
            [_pi_answer_with_usage(VALID_MAP, inp=2000, out=1000), fixtures.SETTLED],
            [
                _pi_tool_start("read", {"path": "src/alpha.rs"}),
                _pi_tool_start("bash", {"command": "echo check"}),
                _pi_tool_start("grep", {"pattern": "alpha_fn", "path": "src"}),
                _pi_tool_start("edit", {"path": "src/alpha.rs"}),
                _pi_tool_start("read", {"path": "src/beta.rs"}),
                _pi_answer_with_usage("writer delivered", inp=4000, out=2000),
                fixtures.SETTLED,
            ],
            pre='if [ -f "$DELEGATE_RUN_DIR/scout-map.md" ]; then echo "// edited" >> src/alpha.rs; fi',
        )
        repo = self._base_repo()
        subprocess.run(["git", "-C", repo, "config", "user.name", "Test"], check=True)
        subprocess.run(["git", "-C", repo, "config", "user.email", "test@example.com"], check=True)
        # Brief has 0 explicit paths -> NOT lightweight -> triggers scout-first by default.
        brief = "重构 alpha 与 beta 的实现并返回新值"
        res = self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", brief)
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("探路：先探路（", res.stderr)

        started = json.loads(res.stdout.splitlines()[0])
        write_run = started["run"]
        scout_run = started.get("scoutRun", "")
        self.assertTrue(scout_run, f"expected scoutRun in start status: {started}")

        w = self.cli("wait", write_run)
        self.assertEqual(w.returncode, 0, w.stderr)
        summary = json.loads((self._run_dir(write_run) / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(summary["state"], "delivered")
        self.assertEqual(summary["scoutRun"], scout_run)
        self.assertEqual(summary["scoutMapStatus"], "accepted")
        self.assertEqual(summary["scoutBadAnchors"], 0)
        self.assertEqual(summary["scoutAnchorsTotal"], 2)
        self.assertEqual(summary["preEditSteps"], 3)
        self.assertEqual(summary["scoutTokens"]["input"], 2000)
        self.assertEqual(summary["scoutTokens"]["output"], 1000)

        # Scout run used tiers.scout (model p/scout-m, thinking medium, read-only) with verbatim prompt template
        scout_meta = json.loads((self._run_dir(scout_run) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(scout_meta["mode"], "read-only")
        self.assertEqual(scout_meta["tier"], "scout")
        self.assertEqual(scout_meta["model"], "p/scout-m")
        self.assertEqual(scout_meta["thinking"], "medium")
        scout_prompt = (self._run_dir(scout_run) / "prompt.md").read_text(encoding="utf-8")
        self.assertIn("# 只读探路：为下面这项改动画一张地图", scout_prompt)
        self.assertIn(brief, scout_prompt)

        # Writer run: scout-map.md persisted, NOT in share, NO symlink in worktree; prompt has verbatim appendix + L0 + abs path
        map_file = self._run_dir(write_run) / "scout-map.md"
        self.assertTrue(map_file.is_file())
        self.assertIn("## L1", map_file.read_text(encoding="utf-8"))
        write_meta = json.loads((self._run_dir(write_run) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(write_meta.get("share", []), [])
        self.assertIsNone(write_meta.get("fork"))
        self.assertIn("src/alpha.rs", write_meta["scout"]["blobs"])
        self.assertEqual(len(write_meta["scout"]["blobs"]["src/alpha.rs"]), 40)

        writer_prompt = (self._run_dir(write_run) / "prompt.md").read_text(encoding="utf-8")
        self.assertIn(brief, writer_prompt)
        self.assertIn("## 已核实的落点（探路地图 L0）\n以下是只读探路对现状的核实结果（与上文冲突时以上文为准）：", writer_prompt)
        self.assertIn("`src/alpha.rs` :: `alpha_fn` (L1-L3)", writer_prompt)
        expected_map_ref = f"完整地图（L1：调用表、同类实现、测试夹具写法、看过但无关的文件）在 `{map_file.resolve()}`，需要时按节查。"
        self.assertIn(expected_map_ref, writer_prompt)

        # Card records scout fields & preEditSteps
        card = json.loads((self._run_dir(write_run) / "card.json").read_text(encoding="utf-8"))
        self.assertEqual(card["scoutRun"], scout_run)
        self.assertEqual(card["scout"]["run"], scout_run)
        self.assertEqual(card["scout"]["mapStatus"], "accepted")
        self.assertEqual(card["scout"]["badAnchors"], 0)
        self.assertEqual(card["scout"]["tokens"]["input"], 2000)
        self.assertEqual(card["preEditSteps"], 3)

        # Pi was called twice (scout + writer), writer started a fresh session (no --fork / --session <file>, distinct --session-id)
        pi_log = (self.work / "pi.log").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(pi_log), 2)
        self.assertNotIn(" --session ", pi_log[1])
        self.assertNotIn(" --fork ", pi_log[1])
        self.assertIn(" --session-id ", pi_log[0])
        self.assertIn(" --session-id ", pi_log[1])
        sid0 = pi_log[0].split(" --session-id ", 1)[1].split()[0]
        sid1 = pi_log[1].split(" --session-id ", 1)[1].split()[0]
        self.assertNotEqual(sid0, sid1)

    def test_three_exceptions_and_non_write_skip_scout(self):
        repo = self._base_repo()
        subprocess.run(["git", "-C", repo, "config", "user.name", "Test"], check=True)
        subprocess.run(["git", "-C", repo, "config", "user.email", "test@example.com"], check=True)

        # 1. --no-scout
        self.fake_pi([fixtures.answer("direct 1"), fixtures.SETTLED], pre='echo "// 1" >> src/alpha.rs')
        res = self.cli("start", "--worktree", "--no-scout", "--workdir", repo, "--accept", "true", "改动无路径任务")
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("探路：跳过（--no-scout）", res.stderr)
        st1 = json.loads(res.stdout.splitlines()[0])
        self.assertNotIn("scoutRun", st1)
        self.assertEqual(self.cli("wait", st1["run"]).returncode, 0)

        # 2. Lightweight brief (1-3 existing paths, no symbol hints, <= 2000 chars)
        self.fake_pi([fixtures.answer("direct 2"), fixtures.SETTLED], pre='echo "// 2" >> src/alpha.rs')
        res = self.cli(
            "start", "--worktree", "--workdir", repo, "--accept", "true",
            "只改 `src/alpha.rs` 与 `src/beta.rs` 里的 `alpha_fn` 返回值",
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("探路：跳过（极轻任务）", res.stderr)
        st2 = json.loads(res.stdout.splitlines()[0])
        self.assertNotIn("scoutRun", st2)
        self.assertEqual(self.cli("wait", st2["run"]).returncode, 0)

        # Counter-case: > 3 paths (4 existing paths) is NOT lightweight -> must scout!
        self.fake_pi(
            [fixtures.answer(VALID_MAP), fixtures.SETTLED],
            [fixtures.answer("wrote 4"), fixtures.SETTLED],
            pre='if [ -f "$DELEGATE_RUN_DIR/scout-map.md" ]; then echo "// 4" >> src/alpha.rs; fi',
        )
        res = self.cli(
            "start", "--worktree", "--workdir", repo, "--accept", "true",
            "同时改 `src/alpha.rs`、`src/beta.rs`、`src/gamma.rs`、`src/delta.rs`",
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("探路：先探路（", res.stderr)
        st_4 = json.loads(res.stdout.splitlines()[0])
        self.assertTrue(st_4.get("scoutRun"))
        self.assertEqual(self.cli("wait", st_4["run"]).returncode, 0)

        # Counter-case: symbol hint (missing symbol) is NOT lightweight -> must scout!
        self.fake_pi(
            [fixtures.answer(VALID_MAP), fixtures.SETTLED],
            [fixtures.answer("wrote sym"), fixtures.SETTLED],
            pre='if [ -f "$DELEGATE_RUN_DIR/scout-map.md" ]; then echo "// s" >> src/alpha.rs; fi',
        )
        res = self.cli(
            "start", "--worktree", "--workdir", repo, "--accept", "true",
            "改 `src/alpha.rs` 里的 `nonexistent_sym_xyz`",
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("探路：先探路（", res.stderr)
        st_sym = json.loads(res.stdout.splitlines()[0])
        self.assertTrue(st_sym.get("scoutRun"))
        self.assertEqual(self.cli("wait", st_sym["run"]).returncode, 0)

        # Counter-case: > 2000 chars is NOT lightweight -> must scout!
        self.fake_pi(
            [fixtures.answer(VALID_MAP), fixtures.SETTLED],
            [fixtures.answer("wrote long"), fixtures.SETTLED],
            pre='if [ -f "$DELEGATE_RUN_DIR/scout-map.md" ]; then echo "// l" >> src/alpha.rs; fi',
        )
        long_brief = "改 `src/alpha.rs` " + ("说明内容" * 520)
        self.assertGreater(len(long_brief), 2000)
        res = self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", long_brief)
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("探路：先探路（", res.stderr)
        st_long = json.loads(res.stdout.splitlines()[0])
        self.assertTrue(st_long.get("scoutRun"))
        self.assertEqual(self.cli("wait", st_long["run"]).returncode, 0)

        # 3. .delegate.json "scoutFirst": false
        Path(repo, ".delegate.json").write_text(json.dumps({"scoutFirst": False}), encoding="utf-8")
        subprocess.run(["git", "-C", repo, "add", ".delegate.json"], check=True)
        subprocess.run(["git", "-C", repo, "commit", "-m", "disable scoutFirst"], check=True, stdout=subprocess.DEVNULL)
        self.fake_pi([fixtures.answer("direct 3"), fixtures.SETTLED], pre='echo "// 3" >> src/alpha.rs')
        res = self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", "无路径也跳过")
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("探路：跳过（scoutFirst: false）", res.stderr)
        st3 = json.loads(res.stdout.splitlines()[0])
        self.assertNotIn("scoutRun", st3)
        self.assertEqual(self.cli("wait", st3["run"]).returncode, 0)

        # Read-only & --tier scout & --tier review never scout
        Path(repo, ".delegate.json").write_text(json.dumps({"scoutFirst": True}), encoding="utf-8")
        subprocess.run(["git", "-C", repo, "add", ".delegate.json"], check=True)
        subprocess.run(["git", "-C", repo, "commit", "-m", "enable scoutFirst"], check=True, stdout=subprocess.DEVNULL)
        for extra_flags in (["--read-only"], ["--tier", "scout"], ["--tier", "review"]):
            self.fake_pi([fixtures.answer("ro"), fixtures.SETTLED])
            res = self.cli("start", *extra_flags, "--workdir", repo, "只读调查")
            self.assertEqual(res.returncode, 0, res.stderr)
            self.assertNotIn("探路：先探路", res.stderr)
            st_ro = json.loads(res.stdout.splitlines()[0])
            self.assertNotIn("scoutRun", st_ro)
            self.assertEqual(self.cli("wait", st_ro["run"]).returncode, 0)

    def _reset_pi(self, *attempts, **kwargs):
        (self.work / "pi.log").unlink(missing_ok=True)
        self.fake_pi(*attempts, **kwargs)

    def test_drop_map_on_missing_l0_and_bad_anchors_writer_still_runs(self):
        repo = self._base_repo()
        subprocess.run(["git", "-C", repo, "config", "user.name", "Test"], check=True)
        subprocess.run(["git", "-C", repo, "config", "user.email", "test@example.com"], check=True)

        # Case A: missing ## L0 -> drop map, writer still runs without map, scout_map_dropped recorded
        self._reset_pi(
            [fixtures.answer("没有按模板给标题，只写了一段散文"), fixtures.SETTLED],
            [fixtures.answer("writer ok without map"), fixtures.SETTLED],
            pre='if [ -f "$DELEGATE_RUN_DIR/raw-brief.md" ]; then echo "// a" >> src/alpha.rs; fi',
        )
        res = self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", "实现新模块 A")
        self.assertEqual(res.returncode, 0, res.stderr)
        run_a = json.loads(res.stdout.splitlines()[0])["run"]
        self.assertEqual(self.cli("wait", run_a).returncode, 0)

        sum_a = json.loads((self._run_dir(run_a) / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(sum_a["state"], "delivered")
        self.assertEqual(sum_a["scoutMapStatus"], "dropped")
        self.assertIn("缺 L0", sum_a["scoutMapDropReason"])
        prompt_a = (self._run_dir(run_a) / "prompt.md").read_text(encoding="utf-8")
        self.assertNotIn("已核实的落点（探路地图 L0）", prompt_a)
        card_a = json.loads((self._run_dir(run_a) / "card.json").read_text(encoding="utf-8"))
        self.assertEqual(card_a["scout"]["mapStatus"], "dropped")
        self.assertEqual(card_a["scout"]["diagnostic"], "scout_map_dropped")
        dropped_evs_a = [e for e in self._global_events() if e.get("kind") == "scout_map_dropped" and e.get("run") == run_a]
        self.assertEqual(len(dropped_evs_a), 1)
        self.assertEqual(dropped_evs_a[0]["payload"]["code"], "missing_l0")

        # Case B: bad anchors > 20% (2/4 = 50%) -> drop map, writer still runs without map
        self._reset_pi(
            [fixtures.answer(BAD_ANCHOR_MAP), fixtures.SETTLED],
            [fixtures.answer("writer ok bad anchors"), fixtures.SETTLED],
            pre='if [ -f "$DELEGATE_RUN_DIR/raw-brief.md" ]; then echo "// b" >> src/beta.rs; fi',
        )
        res = self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", "实现新模块 B")
        self.assertEqual(res.returncode, 0, res.stderr)
        run_b = json.loads(res.stdout.splitlines()[0])["run"]
        self.assertEqual(self.cli("wait", run_b).returncode, 0)

        sum_b = json.loads((self._run_dir(run_b) / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(sum_b["state"], "delivered")
        self.assertEqual(sum_b["scoutMapStatus"], "dropped")
        self.assertEqual(sum_b["scoutBadAnchors"], 2)
        self.assertEqual(sum_b["scoutAnchorsTotal"], 4)
        prompt_b = (self._run_dir(run_b) / "prompt.md").read_text(encoding="utf-8")
        self.assertNotIn("已核实的落点（探路地图 L0）", prompt_b)
        card_b = json.loads((self._run_dir(run_b) / "card.json").read_text(encoding="utf-8"))
        self.assertEqual(card_b["scout"]["diagnostic"], "scout_map_dropped")
        self.assertEqual(card_b["scout"]["badAnchors"], 2)
        dropped_evs_b = [e for e in self._global_events() if e.get("kind") == "scout_map_dropped" and e.get("run") == run_b]
        self.assertEqual(len(dropped_evs_b), 1)
        self.assertEqual(dropped_evs_b[0]["payload"]["code"], "bad_anchors")

        # Case C: scout run fails -> treated as dropped map, writer still runs
        self._reset_pi(
            [{"type": "error", "error": "boom"}],
            [fixtures.answer("writer ok after scout fail"), fixtures.SETTLED],
            pre='if [ -f "$DELEGATE_RUN_DIR/raw-brief.md" ]; then echo "// c" >> src/gamma.rs; fi',
        )
        res = self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", "实现新模块 C")
        self.assertEqual(res.returncode, 0, res.stderr)
        run_c = json.loads(res.stdout.splitlines()[0])["run"]
        self.assertEqual(self.cli("wait", run_c).returncode, 0)
        sum_c = json.loads((self._run_dir(run_c) / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(sum_c["state"], "delivered")
        self.assertEqual(sum_c["scoutMapStatus"], "dropped")

    def test_mismatch_enters_question_and_reply_starts_fresh_writer_with_answer(self):
        repo = self._base_repo()
        subprocess.run(["git", "-C", repo, "config", "user.name", "Test"], check=True)
        subprocess.run(["git", "-C", repo, "config", "user.email", "test@example.com"], check=True)
        # Only 1 attempt initially (scout); writer must NOT run until controller replies!
        self.fake_pi(
            [fixtures.answer(MISMATCH_MAP), fixtures.SETTLED],
            [fixtures.answer("writer after reply"), fixtures.SETTLED],
            pre='if [ "$DELEGATE_MODE" = "write" ]; then echo "// replied" >> src/alpha.rs; fi',
        )
        orig_brief = "修改 `old_alpha` 逻辑以支持新特性"
        res = self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", orig_brief)
        self.assertEqual(res.returncode, 0, res.stderr)
        run1 = json.loads(res.stdout.splitlines()[0])["run"]
        w1 = self.cli("wait", run1)
        self.assertEqual(w1.returncode, 0, w1.stderr)

        sum1 = json.loads((self._run_dir(run1) / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(sum1["state"], "answered")
        self.assertTrue(sum1.get("question"))
        self.assertTrue(sum1.get("scoutQuestion"))
        result1 = (self._run_dir(run1) / "result.md").read_text(encoding="utf-8")
        self.assertIn("说明里提到 `old_alpha`，但代码里实际叫 `alpha_fn`", result1)
        self.assertIn(f'回复后起写手：delegate reply {run1} "<修正或决定>"', result1)
        # Verify Pi was invoked ONLY ONCE (for scout; writer was not started)
        pi_calls_before = (self.work / "pi.log").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(pi_calls_before), 1)

        # Controller replies with a short decision (much shorter than orig_brief) -> does NOT count as rework,
        # starts fresh writer with controller answer at end, and userPromptLen covers the full composed brief.
        ctrl_decision = "直接改 alpha_fn。"
        rep = self.cli("reply", run1, ctrl_decision)
        self.assertEqual(rep.returncode, 0, rep.stderr)
        self.assertNotIn("探路：先探路", rep.stderr)
        run2 = json.loads(rep.stdout.splitlines()[0])["run"]
        w2 = self.cli("wait", run2)
        self.assertEqual(w2.returncode, 0, w2.stderr)

        sum2 = json.loads((self._run_dir(run2) / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(sum2["state"], "delivered")
        meta2 = json.loads((self._run_dir(run2) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta2.get("rework", {}).get("kind"), "answer")
        self.assertIsNone(meta2.get("fork"))
        self.assertEqual(sum2["scoutRun"], sum1["scoutRun"])

        prompt2 = (self._run_dir(run2) / "prompt.md").read_text(encoding="utf-8")
        self.assertIn(orig_brief, prompt2)
        self.assertIn("## 已核实的落点（探路地图 L0）", prompt2)
        self.assertIn("## 主控对探路发现的答复", prompt2)
        self.assertIn(ctrl_decision, prompt2)
        self.assertLess(prompt2.index("## 已核实的落点（探路地图 L0）"), prompt2.index("## 主控对探路发现的答复"))
        self.assertTrue((self._run_dir(run2) / "scout-map.md").is_file())

        # userPromptLen must cover the full composed brief including controller reply (not just ctrl_decision.len())
        u_len = meta2["userPromptLen"]
        prompt2_bytes = prompt2.encode("utf-8")
        self.assertGreater(u_len, len(ctrl_decision.encode("utf-8")))
        extracted_prefix = prompt2_bytes[:u_len].decode("utf-8")
        self.assertIn(orig_brief, extracted_prefix)
        self.assertIn("## 主控对探路发现的答复", extracted_prefix)
        self.assertIn(ctrl_decision, extracted_prefix)
        self.assertNotIn("\n---\n", extracted_prefix)

    def test_prompt_file_with_trailing_newlines_preserves_separator_after_map_injection(self):
        repo = self._base_repo()
        self._reset_pi(
            [fixtures.answer(VALID_MAP), fixtures.SETTLED],
            [fixtures.answer("writer delivered multiline"), fixtures.SETTLED],
            pre='if [ -f "$DELEGATE_RUN_DIR/scout-map.md" ]; then echo "// nl" >> src/alpha.rs; fi',
        )
        prompt_file = self.work / "multiline-brief.md"
        raw_text = "# 多行任务说明\n重构 alpha 与 beta 并保持接口兼容\n\n\n"
        prompt_file.write_text(raw_text, encoding="utf-8")
        res = self.cli(
            "start", "--worktree", "--workdir", repo, "--accept", "true",
            "--prompt-file", str(prompt_file),
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        run_id = json.loads(res.stdout.splitlines()[0])["run"]
        w = self.cli("wait", run_id)
        self.assertEqual(w.returncode, 0, w.stderr)

        prompt_md = (self._run_dir(run_id) / "prompt.md").read_text(encoding="utf-8")
        map_file = (self._run_dir(run_id) / "scout-map.md").resolve()
        expected_tail = (
            f"完整地图（L1：调用表、同类实现、测试夹具写法、看过但无关的文件）在 `{map_file}`，需要时按节查。\n\n---\n"
        )
        self.assertIn(expected_tail, prompt_md)
        self.assertNotIn("需要时按节查。---", prompt_md)
        self.assertNotIn("需要时按节查。\n---\n", prompt_md)
        self.assertNotIn("保持接口兼容-", prompt_md)
        self.assertTrue(prompt_md.startswith("# 多行任务说明\n重构 alpha 与 beta 并保持接口兼容\n\n## 已核实的落点（探路地图 L0）"))

    def test_fake_agy_fresh_session_and_pre_edit_steps_and_lines_tokens(self):
        # Configure fake agy for both scout and write tiers to prove writer is a brand-new session
        # (no --conversation resuming scout's conversation id) and delegate lines counts scout tokens.
        fake_home = self.work / "agy-home"
        fake_home.mkdir(mode=0o700, exist_ok=True)
        self.env["HOME"] = str(fake_home)
        self.env["XDG_CONFIG_HOME"] = str(fake_home / ".config")
        self.env.pop("DELEGATE_AGY_BIN", None)
        calls_log = self.work / "agy-calls.jsonl"
        counter_file = self.work / "agy-counter.txt"

        cfg_dir = fake_home / ".config" / "delegate"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        (cfg_dir / "config.json").write_text(
            json.dumps({
                "models": {
                    "neon": {"model": "agy/gemini-4-neon", "thinking": "high"},
                    "neon-med": {"model": "agy/gemini-4-neon", "thinking": "medium"},
                },
                "tiers": {
                    "scout": "neon-med",
                    "review": "neon-med",
                    "write": "neon",
                },
            }),
            encoding="utf-8",
        )

        agy_bin = self.bin / "agy"
        agy_bin.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "from pathlib import Path\n"
            "argv = sys.argv[1:]\n"
            "if '--version' in argv or '--help' in argv:\n"
            "    print('1.6.0')\n"
            "    sys.exit(0)\n"
            "if not os.environ.get('DELEGATE_RUN_DIR'):\n"
            "    sys.exit(0)\n"
            f"counter = Path({str(counter_file)!r})\n"
            "idx = int(counter.read_text()) + 1 if counter.exists() else 1\n"
            "counter.write_text(str(idx))\n"
            "raw_in = sys.stdin.readline()\n"
            "conv_arg = ''\n"
            "effort_arg = ''\n"
            "for i, a in enumerate(argv):\n"
            "    if a == '--conversation' and i + 1 < len(argv):\n"
            "        conv_arg = argv[i + 1]\n"
            "    if a == '--effort' and i + 1 < len(argv):\n"
            "        effort_arg = argv[i + 1]\n"
            f"with open({str(calls_log)!r}, 'a', encoding='utf-8') as f:\n"
            "    f.write(json.dumps({'idx': idx, 'conv': conv_arg, 'effort': effort_arg, 'run': os.environ['DELEGATE_RUN_DIR']}) + '\\n')\n"
            "if idx == 1:\n"
            "    # Scout session\n"
            "    print(json.dumps({'event': 'init', 'conversation_id': 'agy-scout-conv-1'}), flush=True)\n"
            f"    print(json.dumps({{'event': 'step_update', 'step_update': {{'step_index': 1, 'step_type': 'agent_response', 'state': 'DONE', 'text_delta': {VALID_MAP!r}}}}}), flush=True)\n"
            f"    print(json.dumps({{'event': 'result', 'result': {{'conversation_id': 'agy-scout-conv-1', 'status': 'SUCCESS', 'response': {VALID_MAP!r}, 'usage': {{'input_tokens': 300000, 'output_tokens': 200000, 'cache_read_tokens': 0}}}}}}), flush=True)\n"
            "else:\n"
            "    # Writer session: 2 pre-edit tool calls (read_file + run_command), then edit_file\n"
            "    print(json.dumps({'event': 'init', 'conversation_id': 'agy-writer-conv-2'}), flush=True)\n"
            "    print(json.dumps({'event': 'step_update', 'step_update': {'step_index': 1, 'step_type': 'tool', 'state': 'DONE', 'tool_name': 'read_file', 'tool_info': {'parameters': {'Path': 'src/alpha.rs'}}}}), flush=True)\n"
            "    print(json.dumps({'event': 'step_update', 'step_update': {'step_index': 2, 'step_type': 'tool', 'state': 'DONE', 'tool_name': 'run_command', 'tool_info': {'parameters': {'CommandLine': 'echo hi'}}}}), flush=True)\n"
            "    print(json.dumps({'event': 'step_update', 'step_update': {'step_index': 3, 'step_type': 'tool', 'state': 'DONE', 'tool_name': 'edit_file', 'tool_info': {'parameters': {'Path': 'src/alpha.rs'}}}}), flush=True)\n"
            "    with open('src/alpha.rs', 'a', encoding='utf-8') as fh:\n"
            "        fh.write('// line1\\n// line2\\n// line3\\n// line4\\n')\n"
            "    print(json.dumps({'event': 'step_update', 'step_update': {'step_index': 4, 'step_type': 'agent_response', 'state': 'DONE', 'text_delta': 'agy writer done'}}), flush=True)\n"
            "    print(json.dumps({'event': 'result', 'result': {'conversation_id': 'agy-writer-conv-2', 'status': 'SUCCESS', 'response': 'agy writer done', 'usage': {'input_tokens': 400000, 'output_tokens': 100000, 'cache_read_tokens': 0}}}), flush=True)\n",
            encoding="utf-8",
        )
        agy_bin.chmod(0o755)

        repo = self._base_repo()
        subprocess.run(["git", "-C", repo, "config", "user.name", "Test"], check=True)
        subprocess.run(["git", "-C", repo, "config", "user.email", "test@example.com"], check=True)
        res = self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", "用 agy 改造 alpha")
        self.assertEqual(res.returncode, 0, res.stderr)
        st = json.loads(res.stdout.splitlines()[0])
        write_run = st["run"]
        scout_run = st["scoutRun"]
        w = self.cli("wait", write_run)
        self.assertEqual(w.returncode, 0, w.stderr)

        # Verify agy invocations: scout got effort=medium, writer got effort=high and empty --conversation (fresh session!)
        calls = [json.loads(line) for line in calls_log.read_text(encoding="utf-8").splitlines() if line.strip()]
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[0]["effort"], "medium")
        self.assertEqual(calls[1]["effort"], "high")
        self.assertEqual(calls[1]["conv"], "", "writer must start a fresh agy session, not continue scout")

        sum_w = json.loads((self._run_dir(write_run) / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(sum_w["state"], "delivered")
        self.assertEqual(sum_w["preEditSteps"], 2)
        self.assertEqual(sum_w["scoutRun"], scout_run)

        # Apply --commit so commit carries `Delegate-Run: <write_run>` trailer, then verify `delegate lines`
        ap = self.cli("apply", "--commit", write_run, cwd=repo, human=True)
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)
        repos_file = self.work / "state" / "delegate" / "repos.json"
        repos_file.parent.mkdir(parents=True, exist_ok=True)
        repos_file.write_text(json.dumps({str(Path(repo).resolve()): {"runs": str(self.work / "runs")}}), encoding="utf-8")
        ln = self.cli("lines", "--since", "24h", "--json", cwd=repo)
        self.assertEqual(ln.returncode, 0, ln.stderr)
        doc = json.loads(ln.stdout)
        win = doc["windows"][0]
        # Scout had 500k tokens + Writer had 500k tokens = 1,000,000 total tokens!
        self.assertEqual(win["tokens"]["total"], 1_000_000, f"expected scout+writer=1M tokens: {win}")
        self.assertIn(scout_run, win["chainRuns"])
        self.assertIn(write_run, win["chainRuns"])
        self.assertIsNotNone(win["linesPerMillionTokens"])
        self.assertAlmostEqual(win["linesPerMillionTokens"], 4.0, places=2)

    def test_queue_add_passes_through_no_scout(self):
        repo = self._base_repo()
        subprocess.run(["git", "-C", repo, "config", "user.name", "Test"], check=True)
        subprocess.run(["git", "-C", repo, "config", "user.email", "test@example.com"], check=True)
        self.fake_pi([fixtures.answer("upstream done"), fixtures.SETTLED], pre='echo "// u" >> src/beta.rs')
        # Upstream run with --no-scout so it finishes in 1 call
        r_up = self.cli("start", "--worktree", "--no-scout", "--workdir", repo, "--accept", "true", "上游任务")
        self.assertEqual(r_up.returncode, 0, r_up.stderr)
        up_run = json.loads(r_up.stdout.splitlines()[0])["run"]
        self.assertEqual(self.cli("wait", up_run).returncode, 0)

        prompt_file = self.work / "queued-prompt.md"
        prompt_file.write_text("下游任务无路径说明", encoding="utf-8")
        qa = self.cli(
            "queue", "add",
            "q-no-scout",
            "--after-applied", up_run,
            "--prompt-file", str(prompt_file),
            "--worktree", "--no-scout", "--workdir", repo, "--accept", "true",
            "--json",
        )
        self.assertEqual(qa.returncode, 0, qa.stderr)
        qdoc = json.loads(qa.stdout)
        self.assertIn("--no-scout", qdoc["startArgs"])


if __name__ == "__main__":
    unittest.main()
