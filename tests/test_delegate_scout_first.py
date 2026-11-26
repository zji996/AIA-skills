"""End-to-end tests for two-step scout-first default on write tasks (`delegate start --worktree`).

Covers:
- Default scout-first flow (read-only scout via `tiers.scout` with prose handoff prompt ->
  loose path/line scanning & git blob SHA recording on scout's own snapshot tree ->
  `scout-map.md` archival -> fresh writer session with prose note embedded directly, no symlink/--share)
- Four real historical discard fixtures (`dm-gc`, `agy-auto-switch`, `dev-self-heal`, `ops-auth-provider`)
  are accepted without dropping and their paths are extracted
- Race fix: committing +50 lines into `source` during scout execution does NOT drop the map,
  pins blobs to scout's snapshot tree, refreshes writer `netBase`, and appends the factual drift note
- Missing paths produce a factual footnote instead of dropping the map
- Unique drop conditions (scout failure/empty; or >=5 repo-style paths with 0 existing when brief names
  an existing file) record concrete `dropReason` & `dropDetails` in meta, summary, card, and events
- Natural-language scout question enters `scoutQuestion` (not rework) with reply hint; `delegate reply`
  launches fresh writer with controller answer appended and drift checked against updated baseline
- Explicit opt-outs (`--no-scout`, `.delegate.json` `"scoutFirst": false`) skip scout, while short/targeted write briefs still scout by default
- Read-only / `--tier scout|review` / `reply` never scout
- Fake `agy`: writer is a brand-new conversation (no `--conversation` continuing scout), `preEditSteps` counting
- `delegate lines`: includes `scoutRun` tokens in write task's per-million-token metric
- `queue add --no-scout` passthrough
"""
import json
import subprocess
import unittest
from pathlib import Path

import test_delegate as fixtures


FIXTURES_DIR = Path(__file__).resolve().parent / "fixtures" / "scout"

PROSE_MAP = """\
### 改动位置
主要改动在 [src/alpha.rs](src/alpha.rs#L1-L3) 的 `alpha_fn`（第 1-3 行）以及 `src/beta.rs`（L1–L3）的 `beta_fn`。

### 上下游与验证
`beta_fn` 调用 `alpha_fn`；最快验证命令是 `true`。
"""

NATURAL_QUESTION_MAP = """\
开工前请主控确认：说明里提到 `old_alpha`，但代码里实际只有 [src/alpha.rs](src/alpha.rs#L1-L3) 的 `alpha_fn` 且签名不同。我倾向直接改 `alpha_fn`，请确认是否按此推进？

---

### 已查清的事实
- `src/alpha.rs` (L1-3)：当前函数叫 `alpha_fn() -> i32`。
- `src/beta.rs` (L1-3)：调用方在 `beta_fn`。
"""

WRONG_REPO_FIVE_PATHS_MAP = """\
要改动以下五个文件：
- `other_repo/src/one.rs` (L1-10)
- `other_repo/src/two.rs` (L1-10)
- `other_repo/src/three.rs` (L1-10)
- `other_repo/src/four.rs` (L1-10)
- `other_repo/src/five.rs` (L1-10)
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

    def _named_repo(self, name: str, files: dict[str, str]) -> Path:
        repo = self.work / name
        repo.mkdir()
        git = ["git", "-C", str(repo), "-c", "user.name=Test", "-c", "user.email=test@example.com"]
        subprocess.run(git[:3] + ["init", "-q"], check=True)
        for rel, text in files.items():
            (repo / rel).parent.mkdir(parents=True, exist_ok=True)
            (repo / rel).write_text(text, encoding="utf-8")
        subprocess.run(git + ["add", "-A"], check=True)
        subprocess.run(git + ["commit", "-qm", "init"], check=True)
        return repo

    def _run_dir(self, run_id: str) -> Path:
        return self.work / "runs" / run_id

    def _global_events(self) -> list[dict]:
        ev_path = self.work / "state" / "delegate" / "events.jsonl"
        if not ev_path.is_file():
            return []
        return [json.loads(line) for line in ev_path.read_text(encoding="utf-8").splitlines() if line.strip()]

    def _reset_pi(self, *attempts, **kwargs):
        (self.work / "pi.log").unlink(missing_ok=True)
        self.fake_pi(*attempts, **kwargs)

    def test_default_scout_then_write_full_flow_and_accounting(self):
        # Attempt 1 = scout run (read-only), Attempt 2 = writer run (fresh session with 3 pre-edit steps)
        self.fake_pi(
            [_pi_answer_with_usage(PROSE_MAP, inp=2000, out=1000), fixtures.SETTLED],
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
        # Write tasks trigger scout-first by default.
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
        self.assertEqual(summary["preEditSteps"], 3)
        self.assertEqual(summary["scoutTokens"]["input"], 2000)
        self.assertEqual(summary["scoutTokens"]["output"], 1000)

        # Scout run used tiers.scout with verbatim Section 3.1 prose prompt template (no L0/L1, no `::`)
        scout_meta = json.loads((self._run_dir(scout_run) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(scout_meta["mode"], "read-only")
        self.assertEqual(scout_meta["tier"], "scout")
        self.assertEqual(scout_meta["model"], "p/scout-m")
        self.assertEqual(scout_meta["thinking"], "medium")
        scout_prompt = (self._run_dir(scout_run) / "prompt.md").read_text(encoding="utf-8")
        self.assertIn("# 只读探路：为接手改代码的同事写一份交接笔记", scout_prompt)
        self.assertIn("控制在 1500 字内", scout_prompt)
        self.assertNotIn("## L0", scout_prompt)
        self.assertNotIn("## L1", scout_prompt)
        self.assertIn(brief, scout_prompt)

        # Writer run: scout-map.md archived, NOT in share, NO symlink; prose note embedded directly in prompt.md
        map_file = self._run_dir(write_run) / "scout-map.md"
        self.assertTrue(map_file.is_file())
        write_meta = json.loads((self._run_dir(write_run) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(write_meta.get("share", []), [])
        self.assertIsNone(write_meta.get("fork"))
        self.assertIn("src/alpha.rs", write_meta["scout"]["blobs"])
        self.assertIn("src/beta.rs", write_meta["scout"]["blobs"])
        self.assertEqual(len(write_meta["scout"]["blobs"]["src/alpha.rs"]), 40)

        writer_prompt = (self._run_dir(write_run) / "prompt.md").read_text(encoding="utf-8")
        self.assertIn(brief, writer_prompt)
        self.assertIn("## 探路交接笔记\n以下是只读探路同事对现状的查证笔记（与上文任务说明冲突时以上文为准）：", writer_prompt)
        self.assertIn("`beta_fn` 调用 `alpha_fn`", writer_prompt)
        self.assertNotIn("完整地图（L1", writer_prompt)

        # Card records scout fields, single-listed scout tokens & preEditSteps
        card = json.loads((self._run_dir(write_run) / "card.json").read_text(encoding="utf-8"))
        self.assertEqual(card["scoutRun"], scout_run)
        self.assertEqual(card["scout"]["run"], scout_run)
        self.assertEqual(card["scout"]["mapStatus"], "accepted")
        self.assertEqual(card["scout"]["pathsTotal"], 2)
        self.assertEqual(card["scout"]["tokens"]["input"], 2000)
        self.assertEqual(card["preEditSteps"], 3)
        card_out = self.cli("card", write_run, human=True)
        self.assertEqual(card_out.returncode, 0, card_out.stderr)
        self.assertIn("探路用量", card_out.stdout)

        # Pi was called twice (scout + writer), writer started a fresh session
        pi_log = (self.work / "pi.log").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(pi_log), 2)
        self.assertNotIn(" --session ", pi_log[1])
        self.assertNotIn(" --fork ", pi_log[1])
        sid0 = pi_log[0].split(" --session-id ", 1)[1].split()[0]
        sid1 = pi_log[1].split(" --session-id ", 1)[1].split()[0]
        self.assertNotEqual(sid0, sid1)

    def test_four_real_discard_fixtures_not_dropped_and_paths_extracted(self):
        cases = [
            (
                "dm-gc.md",
                {
                    "crates/delegate/src/main.rs": "fn apply() {}\nfn clean() {}\n",
                    "crates/delegate/src/gc.rs": "fn worker_command() {}\n",
                    "crates/delegate/src/runs.rs": "fn prune() {}\n",
                    "crates/delegate/src/disk.rs": "fn probe_all_busy() {}\n",
                    "skills/delegate/SKILL.md": "# delegate\n",
                    "skills/delegate/references/commands.md": "# cmds\n",
                    "skills/delegate/references/model-routing.md": "# routing\n",
                    "CHANGELOG.md": "# Changelog\n",
                },
                ["crates/delegate/src/main.rs", "crates/delegate/src/gc.rs", "crates/delegate/src/disk.rs"],
                "tests/test_delegate_auto_gc.py",
            ),
            (
                "agy-auto-switch.md",
                {
                    "crates/delegate/src/config.rs": "fn validate() {}\n",
                    "crates/delegate/src/agy_accounts.rs": "fn refresh_all_quotas() {}\n",
                    "crates/delegate/src/supervise.rs": "fn compute_tokens() {}\n",
                    "crates/delegate-tui/src/main.rs": "fn main() {}\n",
                    "crates/delegate-tui/src/model.rs": "struct AgyAccountsState;\n",
                    "CHANGELOG.md": "# Changelog\n",
                },
                [
                    "crates/delegate/src/config.rs",
                    "crates/delegate/src/agy_accounts.rs",
                    "crates/delegate-tui/src/main.rs",
                ],
                None,
            ),
            (
                "dev-self-heal.md",
                {
                    "manage.sh": "#!/usr/bin/env bash\n",
                    "scripts/run-delegated.sh": "#!/usr/bin/env bash\n",
                    "apps/aiaxiom-server/src/server/threads/harness_change.rs": "fn start_watcher() {}\n",
                    "apps/web/src/pages/Auth.tsx": "export function Auth() {}\n",
                    "docs/reference/dev-maintenance.md": "# dev\n",
                },
                [
                    "manage.sh",
                    "scripts/run-delegated.sh",
                    "apps/aiaxiom-server/src/server/threads/harness_change.rs",
                    "apps/web/src/pages/Auth.tsx",
                ],
                None,
            ),
            (
                "ops-auth-provider.md",
                {
                    "apps/aiaxiom-server/src/server/ops.rs": "enum OpsAction {}\n",
                    "crates/control-plane/src/oauth/mod.rs": "struct ControlPlane;\n",
                    "scripts/alpha.sh": "#!/usr/bin/env bash\n",
                    "docs/reference/dev-maintenance.md": "# dev\n",
                },
                [
                    "apps/aiaxiom-server/src/server/ops.rs",
                    "crates/control-plane/src/oauth/mod.rs",
                    "scripts/alpha.sh",
                ],
                "apps/aiaxiom-server/src/server/ops/auth_provider.rs",
            ),
        ]

        for idx, (fixture_name, repo_files, expected_blobs, expected_missing) in enumerate(cases):
            with self.subTest(fixture=fixture_name):
                raw_text = (FIXTURES_DIR / fixture_name).read_text(encoding="utf-8")
                repo = self._named_repo(f"repo-fixture-{idx}", repo_files)
                subprocess.run(["git", "-C", str(repo), "config", "user.name", "Test"], check=True)
                subprocess.run(["git", "-C", str(repo), "config", "user.email", "test@example.com"], check=True)
                first_file = next(iter(repo_files.keys()))
                self._reset_pi(
                    [fixtures.answer(raw_text), fixtures.SETTLED],
                    [fixtures.answer(f"delivered {fixture_name}"), fixtures.SETTLED],
                    pre=f'if [ -f "$DELEGATE_RUN_DIR/scout-map.md" ]; then echo "// ok" >> {first_file}; fi',
                )
                res = self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", f"实现 {fixture_name}")
                self.assertEqual(res.returncode, 0, res.stderr)
                run_id = json.loads(res.stdout.splitlines()[0])["run"]
                w = self.cli("wait", run_id)
                self.assertEqual(w.returncode, 0, w.stderr)

                summary = json.loads((self._run_dir(run_id) / "summary.json").read_text(encoding="utf-8"))
                self.assertEqual(summary["state"], "delivered")
                self.assertEqual(summary["scoutMapStatus"], "accepted", f"{fixture_name} must not drop: {summary}")
                meta = json.loads((self._run_dir(run_id) / "meta.json").read_text(encoding="utf-8"))
                blobs = meta["scout"]["blobs"]
                for path in expected_blobs:
                    self.assertIn(path, blobs, f"{fixture_name} missing extracted blob {path}")
                writer_prompt = (self._run_dir(run_id) / "prompt.md").read_text(encoding="utf-8")
                self.assertIn("## 探路交接笔记", writer_prompt)
                if expected_missing:
                    self.assertIn(expected_missing, meta["scout"].get("missingPaths", []))
                    self.assertIn("注：探路提及的以下路径在基线中尚不存在（可能是待新建文件或路径笔误）：", writer_prompt)
                    self.assertIn(expected_missing, writer_prompt)

    def test_race_source_inserts_50_lines_during_scout_keeps_map_and_notes_drift(self):
        repo = self._base_repo()
        subprocess.run(["git", "-C", repo, "config", "user.name", "Test"], check=True)
        subprocess.run(["git", "-C", repo, "config", "user.email", "test@example.com"], check=True)

        # Compute pre-scout blob SHA of src/alpha.rs and pre-scout HEAD
        old_alpha_sha = subprocess.check_output(
            ["git", "-C", repo, "hash-object", "src/alpha.rs"], text=True
        ).strip()
        old_head = subprocess.check_output(
            ["git", "-C", repo, "rev-parse", "HEAD"], text=True
        ).strip()

        # During scout execution (no scout-map.md in scout run dir), insert 50 lines at the top of repo/src/alpha.rs and commit!
        insert_py = self.work / "insert_50.py"
        insert_py.write_text(
            "import subprocess\n"
            "from pathlib import Path\n"
            f"repo = Path({str(repo)!r})\n"
            "p = repo / 'src/alpha.rs'\n"
            "p.write_text(''.join(f'// inserted {i}\\n' for i in range(50)) + p.read_text(encoding='utf-8'), encoding='utf-8')\n"
            "subprocess.run(['git', '-C', str(repo), 'commit', '-am', 'insert 50 lines during scout'], check=True, stdout=subprocess.DEVNULL)\n",
            encoding="utf-8",
        )
        insert_script = (
            f'if [ ! -f "$DELEGATE_RUN_DIR/scout-map.md" ]; then '
            f'python3 "{insert_py}"; '
            f'else echo "// writer edit" >> src/beta.rs; fi'
        )
        self._reset_pi(
            [fixtures.answer(PROSE_MAP), fixtures.SETTLED],
            [fixtures.answer("writer handled drift"), fixtures.SETTLED],
            pre=insert_script,
        )

        res = self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", "并发推进时改造 alpha 和 beta")
        self.assertEqual(res.returncode, 0, res.stderr)
        run_id = json.loads(res.stdout.splitlines()[0])["run"]
        w = self.cli("wait", run_id)
        self.assertEqual(w.returncode, 0, w.stderr)

        new_head = subprocess.check_output(
            ["git", "-C", repo, "rev-parse", "HEAD"], text=True
        ).strip()
        self.assertNotEqual(old_head, new_head)

        summary = json.loads((self._run_dir(run_id) / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(summary["state"], "delivered")
        self.assertEqual(summary["scoutMapStatus"], "accepted")

        meta = json.loads((self._run_dir(run_id) / "meta.json").read_text(encoding="utf-8"))
        # Blob was pinned to scout's own snapshot tree (before the 50-line insertion), NOT live source!
        self.assertEqual(meta["scout"]["blobs"]["src/alpha.rs"], old_alpha_sha)
        # Writer detected drift on src/alpha.rs only (src/beta.rs was untouched)
        self.assertEqual(meta["scout"].get("changedPaths"), ["src/alpha.rs"])
        # Writer netBase was refreshed to the new HEAD when writer tree was snapped
        self.assertEqual(meta["netBase"], new_head)

        writer_prompt = (self._run_dir(run_id) / "prompt.md").read_text(encoding="utf-8")
        self.assertIn("注：探路后以下文件在当前基线中已变动，行号可能偏移：src/alpha.rs", writer_prompt)
        # Footnote must sit before the contract separator `---`
        self.assertLess(
            writer_prompt.index("注：探路后以下文件在当前基线中已变动，行号可能偏移：src/alpha.rs"),
            writer_prompt.index("\n---\n"),
        )

    def test_missing_paths_footnote_without_dropping_map(self):
        repo = self._base_repo()
        subprocess.run(["git", "-C", repo, "config", "user.name", "Test"], check=True)
        subprocess.run(["git", "-C", repo, "config", "user.email", "test@example.com"], check=True)

        map_with_new_files = (
            "改动入口在 `src/alpha.rs` 第 1-3 行，同时建议新建 `src/new_helper.rs` 和 "
            "`tests/test_new_feature.py` 补单测；`alpha.rs` 只是简写不该误报。"
        )
        self._reset_pi(
            [fixtures.answer(map_with_new_files), fixtures.SETTLED],
            [fixtures.answer("writer created helper"), fixtures.SETTLED],
            pre='if [ -f "$DELEGATE_RUN_DIR/scout-map.md" ]; then echo "// ok" >> src/alpha.rs; fi',
        )
        res = self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", "新增辅助模块与测试")
        self.assertEqual(res.returncode, 0, res.stderr)
        run_id = json.loads(res.stdout.splitlines()[0])["run"]
        w = self.cli("wait", run_id)
        self.assertEqual(w.returncode, 0, w.stderr)

        summary = json.loads((self._run_dir(run_id) / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(summary["state"], "delivered")
        self.assertEqual(summary["scoutMapStatus"], "accepted")
        meta = json.loads((self._run_dir(run_id) / "meta.json").read_text(encoding="utf-8"))
        self.assertEqual(meta["scout"]["missingPaths"], ["src/new_helper.rs", "tests/test_new_feature.py"])
        writer_prompt = (self._run_dir(run_id) / "prompt.md").read_text(encoding="utf-8")
        self.assertIn(
            "注：探路提及的以下路径在基线中尚不存在（可能是待新建文件或路径笔误）：src/new_helper.rs、tests/test_new_feature.py",
            writer_prompt,
        )

    def test_drop_only_on_wrong_repo_or_scout_failure_records_concrete_reason(self):
        repo = self._base_repo()
        subprocess.run(["git", "-C", repo, "config", "user.name", "Test"], check=True)
        subprocess.run(["git", "-C", repo, "config", "user.email", "test@example.com"], check=True)

        # Case A: plain prose without L0/L1 headings is ACCEPTED (no longer dropped!)
        self._reset_pi(
            [fixtures.answer("没有按旧模板给标题，只写了一段自然语言交接笔记，入口在 src/alpha.rs。"), fixtures.SETTLED],
            [fixtures.answer("writer ok with prose"), fixtures.SETTLED],
            pre='if [ -f "$DELEGATE_RUN_DIR/scout-map.md" ]; then echo "// a" >> src/alpha.rs; fi',
        )
        res = self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", "实现新模块 A")
        self.assertEqual(res.returncode, 0, res.stderr)
        run_a = json.loads(res.stdout.splitlines()[0])["run"]
        self.assertEqual(self.cli("wait", run_a).returncode, 0)
        sum_a = json.loads((self._run_dir(run_a) / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(sum_a["state"], "delivered")
        self.assertEqual(sum_a["scoutMapStatus"], "accepted")
        prompt_a = (self._run_dir(run_a) / "prompt.md").read_text(encoding="utf-8")
        self.assertIn("## 探路交接笔记", prompt_a)

        # Case B: >=5 repo-style paths with 0 existing AND brief names existing files -> DROPPED with concrete reason & details
        self._reset_pi(
            [fixtures.answer(WRONG_REPO_FIVE_PATHS_MAP), fixtures.SETTLED],
            [fixtures.answer("writer ok after wrong repo drop"), fixtures.SETTLED],
            pre='if [ -f "$DELEGATE_RUN_DIR/raw-brief.md" ]; then echo "// b" >> src/beta.rs; fi',
        )
        brief_with_existing = "同时改造 `src/alpha.rs`、`src/beta.rs`、`src/gamma.rs`、`src/delta.rs`"
        res = self.cli("start", "--worktree", "--workdir", repo, "--accept", "true", brief_with_existing)
        self.assertEqual(res.returncode, 0, res.stderr)
        run_b = json.loads(res.stdout.splitlines()[0])["run"]
        self.assertEqual(self.cli("wait", run_b).returncode, 0)

        sum_b = json.loads((self._run_dir(run_b) / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(sum_b["state"], "delivered")
        self.assertEqual(sum_b["scoutMapStatus"], "dropped")
        self.assertIn("5 个仓内样式路径", sum_b["scoutMapDropReason"])
        self.assertIn("other_repo/src/one.rs", sum_b["scoutMapDropDetails"])
        prompt_b = (self._run_dir(run_b) / "prompt.md").read_text(encoding="utf-8")
        self.assertNotIn("## 探路交接笔记", prompt_b)
        card_b = json.loads((self._run_dir(run_b) / "card.json").read_text(encoding="utf-8"))
        self.assertEqual(card_b["scout"]["diagnostic"], "scout_map_dropped")
        self.assertIn("5 个仓内样式路径", card_b["scout"]["dropReason"])
        self.assertEqual(len(card_b["scout"]["dropDetails"]), 5)
        card_b_md = self.cli("card", run_b, human=True)
        self.assertEqual(card_b_md.returncode, 0, card_b_md.stderr)
        self.assertIn("scout_map_dropped", card_b_md.stdout)
        self.assertIn("5 个仓内样式路径", card_b_md.stdout)
        dropped_evs_b = [e for e in self._global_events() if e.get("kind") == "scout_map_dropped" and e.get("run") == run_b]
        self.assertEqual(len(dropped_evs_b), 1)
        self.assertEqual(dropped_evs_b[0]["payload"]["code"], "wrong_repo_paths")
        self.assertEqual(len(dropped_evs_b[0]["payload"]["details"]), 5)

        # Case C: scout run fails -> treated as dropped map with concrete reason, writer still runs
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
        self.assertIn("探路失败", sum_c["scoutMapDropReason"])

    def test_natural_scout_question_enters_question_and_reply_starts_fresh_writer(self):
        repo = self._base_repo()
        subprocess.run(["git", "-C", repo, "config", "user.name", "Test"], check=True)
        subprocess.run(["git", "-C", repo, "config", "user.email", "test@example.com"], check=True)
        # Only 1 attempt initially (scout); writer must NOT run until controller replies!
        self.fake_pi(
            [fixtures.answer(NATURAL_QUESTION_MAP), fixtures.SETTLED],
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
        self.assertIn("开工前请主控确认", result1)
        self.assertIn(f'回复后起写手：delegate reply {run1} "<修正或决定>"', result1)
        # Verify Pi was invoked ONLY ONCE (for scout; writer was not started)
        pi_calls_before = (self.work / "pi.log").read_text(encoding="utf-8").splitlines()
        self.assertEqual(len(pi_calls_before), 1)

        # Modify src/alpha.rs in source BEFORE controller replies -> reply writer should detect drift & refresh netBase!
        alpha_p = Path(repo) / "src/alpha.rs"
        alpha_p.write_text("// updated while waiting for controller\n" + alpha_p.read_text(encoding="utf-8"), encoding="utf-8")
        subprocess.run(["git", "-C", repo, "commit", "-am", "drift before reply"], check=True, stdout=subprocess.DEVNULL)
        head_before_reply = subprocess.check_output(["git", "-C", repo, "rev-parse", "HEAD"], text=True).strip()

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
        self.assertEqual(meta2["netBase"], head_before_reply)
        self.assertEqual(meta2["scout"].get("changedPaths"), ["src/alpha.rs"])

        prompt2 = (self._run_dir(run2) / "prompt.md").read_text(encoding="utf-8")
        self.assertIn(orig_brief, prompt2)
        self.assertIn("## 探路交接笔记", prompt2)
        self.assertIn("注：探路后以下文件在当前基线中已变动，行号可能偏移：src/alpha.rs", prompt2)
        self.assertIn("## 主控对探路发现的答复", prompt2)
        self.assertIn(ctrl_decision, prompt2)
        # Drift note must land at the end of the scout note (after internal `---` and before controller reply!)
        self.assertLess(
            prompt2.index("调用方在 `beta_fn`"),
            prompt2.index("注：探路后以下文件在当前基线中已变动，行号可能偏移：src/alpha.rs"),
        )
        self.assertLess(
            prompt2.index("注：探路后以下文件在当前基线中已变动，行号可能偏移：src/alpha.rs"),
            prompt2.index("## 主控对探路发现的答复"),
        )
        self.assertTrue((self._run_dir(run2) / "scout-map.md").is_file())

        # userPromptLen must cover the full composed brief including drift note & controller reply
        u_len = meta2["userPromptLen"]
        prompt2_bytes = prompt2.encode("utf-8")
        extracted_prefix = prompt2_bytes[:u_len].decode("utf-8")
        self.assertIn(orig_brief, extracted_prefix)
        self.assertIn("## 主控对探路发现的答复", extracted_prefix)
        self.assertIn(ctrl_decision, extracted_prefix)
        self.assertFalse(extracted_prefix.rstrip().endswith("---"))

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

        # 2. Short, targeted brief (1-3 existing paths, no symbol hints, <= 2000 chars) STILL scouts by default
        self.fake_pi(
            [fixtures.answer(PROSE_MAP), fixtures.SETTLED],
            [fixtures.answer("wrote 2"), fixtures.SETTLED],
            pre='if [ -f "$DELEGATE_RUN_DIR/scout-map.md" ]; then echo "// 2" >> src/alpha.rs; fi',
        )
        res = self.cli(
            "start", "--worktree", "--workdir", repo, "--accept", "true",
            "只改 `src/alpha.rs` 与 `src/beta.rs` 里的 `alpha_fn` 返回值",
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("探路：先探路（", res.stderr)
        st2 = json.loads(res.stdout.splitlines()[0])
        self.assertTrue(st2.get("scoutRun"))
        self.assertEqual(self.cli("wait", st2["run"]).returncode, 0)

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

    def test_prompt_file_with_trailing_newlines_preserves_separator_after_map_injection(self):
        repo = self._base_repo()
        self._reset_pi(
            [fixtures.answer(PROSE_MAP), fixtures.SETTLED],
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
        self.assertIn("最快验证命令是 `true`。\n\n---\n", prompt_md)
        self.assertNotIn("保持接口兼容-", prompt_md)
        self.assertTrue(prompt_md.startswith("# 多行任务说明\n重构 alpha 与 beta 并保持接口兼容\n\n## 探路交接笔记"))

    def test_fake_agy_fresh_session_and_pre_edit_steps_and_lines_tokens(self):
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
            "    print(json.dumps({'event': 'init', 'conversation_id': 'agy-scout-conv-1'}), flush=True)\n"
            f"    print(json.dumps({{'event': 'step_update', 'step_update': {{'step_index': 1, 'step_type': 'agent_response', 'state': 'DONE', 'text_delta': {PROSE_MAP!r}}}}}), flush=True)\n"
            f"    print(json.dumps({{'event': 'result', 'result': {{'conversation_id': 'agy-scout-conv-1', 'status': 'SUCCESS', 'response': {PROSE_MAP!r}, 'usage': {{'input_tokens': 300000, 'output_tokens': 200000, 'cache_read_tokens': 0}}}}}}), flush=True)\n"
            "else:\n"
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

        calls = [json.loads(line) for line in calls_log.read_text(encoding="utf-8").splitlines() if line.strip()]
        self.assertEqual(len(calls), 2)
        self.assertEqual(calls[0]["effort"], "medium")
        self.assertEqual(calls[1]["effort"], "high")
        self.assertEqual(calls[1]["conv"], "", "writer must start a fresh agy session, not continue scout")

        sum_w = json.loads((self._run_dir(write_run) / "summary.json").read_text(encoding="utf-8"))
        self.assertEqual(sum_w["state"], "delivered")
        self.assertEqual(sum_w["preEditSteps"], 2)
        self.assertEqual(sum_w["scoutRun"], scout_run)

        ap = self.cli("apply", "--commit", write_run, cwd=repo, human=True)
        self.assertEqual(ap.returncode, 0, ap.stdout + ap.stderr)
        repos_file = self.work / "state" / "delegate" / "repos.json"
        repos_file.parent.mkdir(parents=True, exist_ok=True)
        repos_file.write_text(json.dumps({str(Path(repo).resolve()): {"runs": str(self.work / "runs")}}), encoding="utf-8")
        ln = self.cli("lines", "--since", "24h", "--json", cwd=repo)
        self.assertEqual(ln.returncode, 0, ln.stderr)
        doc = json.loads(ln.stdout)
        win = doc["windows"][0]
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
