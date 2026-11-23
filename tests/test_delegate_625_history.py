"""6.24 regressions: actionable default status, permanent history.jsonl index,
Layer-2 `show` (live, archived, expired, post-apply git log), pre-dispatch dedup,
archive retention/LRU preserving history, and backfill of legacy runs."""
import gzip
import json
import os
import shlex
import subprocess
import time
import unittest
from pathlib import Path

import test_delegate as fixtures


class Delegate625HistoryTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo

    def git(self, repo, *args):
        return subprocess.run(
            ["git", "-C", str(repo), "-c", "user.name=t", "-c", "user.email=t@t", *args],
            capture_output=True, text=True, check=True,
        ).stdout.strip()

    def history_files(self):
        root = self.work / "state/delegate/history"
        if not root.is_dir():
            return []
        return sorted(root.rglob("history.jsonl"))

    def test_status_default_filters_actionable_and_tracks_result_and_apply(self):
        repo = self.repo({"a.txt": "base\n", "b.txt": "base\n"})
        gate = self.work / "status-gate"
        # 1. Completed read-only run via `run` -> already read -> hidden from default status.
        self.fake_pi([fixtures.answer("already read answer"), fixtures.SETTLED])
        done_ro = self.outcome(self.cli("run", "--read-only", "--workdir", repo, "--name", "done-ro", "check docs"))
        meta_ro = json.loads((Path(done_ro["dir"]) / "meta.json").read_text())
        self.assertIn("resultReadAt", meta_ro)

        # 2. Background read-only run via `start` -> unread -> shown until `result`.
        unread_ro = self.outcome(self.cli("start", "--read-only", "--workdir", repo, "--name", "unread-ro", "scan logs"))
        self.cli("wait", "--no-result", unread_ro["run"])

        # 3. Delivered write run -> pending apply -> shown until `apply`.
        self.fake_pi([fixtures.answer("updated a.txt"), fixtures.SETTLED], pre="echo v2 > a.txt")
        pending_apply = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "--name", "pending-apply", "edit a")
        )

        # 4. Questioning run -> pending question -> shown until `reply`.
        self.fake_pi([fixtures.answer("需要主控决定\n选方案 A 还是 B？"), fixtures.SETTLED])
        question_run = self.outcome(self.cli("run", "--read-only", "--workdir", repo, "--name", "ask-q", "decide"))

        # 5. Rejected run -> unhandled -> shown until `result` or `reply`.
        self.fake_pi([fixtures.answer("tried fixing"), fixtures.SETTLED], pre="echo bad > b.txt")
        rejected_run = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--accept", "false", "--name", "rej-one", "fix b")
        )

        # 6. Active running + queued run (maxActive=1).
        self.fake_pi(
            [fixtures.answer("slow done"), fixtures.SETTLED],
            pre=f'while [ ! -f {shlex.quote(str(gate))} ]; do sleep .05; done',
        )
        self.env["DELEGATE_MAX_ACTIVE"] = "1"
        live = self.outcome(self.cli("start", "--read-only", "--workdir", repo, "--name", "live-one", "long task"))
        queued = self.outcome(self.cli("start", "--read-only", "--workdir", repo, "--name", "queued-one", "second task"))
        self.addCleanup(lambda: (gate.touch(), self.cli("stop", live["run"], queued["run"])))
        self.env.pop("DELEGATE_MAX_ACTIVE", None)

        human = self.cli("status", human=True)
        self.assertEqual(human.returncode, 0, human.stderr)
        lines = human.stdout.splitlines()
        self.assertIn("运行中 1 · 待合入 1 · 待回答 1 · 排队 1", lines[0])
        self.assertIn("待读 1", lines[0])
        self.assertIn("未通过 1", lines[0])
        for name in ("live-one", "queued-one", "ask-q", "pending-apply", "unread-ro", "rej-one"):
            self.assertIn(name, human.stdout)
        self.assertNotIn("done-ro", human.stdout)
        self.assertIn("另有 1 条已结束：delegate history", lines[-1])

        # `--all` keeps showing every run, including done-ro.
        all_out = self.cli("status", "--all", human=True)
        self.assertIn("done-ro", all_out.stdout)
        self.assertNotIn("另有", all_out.stdout)

        # Reading `result` handles unread-ro and rej-one and records resultReadAt in meta.json.
        self.assertEqual(self.cli("result", unread_ro["run"]).returncode, 0)
        self.assertIn("resultReadAt", json.loads((Path(unread_ro["dir"]) / "meta.json").read_text()))
        self.assertEqual(self.cli("result", rejected_run["run"]).returncode, 0)
        self.assertIn("resultReadAt", json.loads((Path(rejected_run["dir"]) / "meta.json").read_text()))

        # Applying pending-apply records appliedAt & appliedHead (archived on auto-clean) and hides it from status.
        self.assertEqual(self.cli("apply", pending_apply["run"]).returncode, 0)
        live_meta = Path(pending_apply["dir"]) / "meta.json"
        arch_meta = self.work / "cache/delegate/archive" / pending_apply["run"] / "record" / "meta.json"
        applied_meta = json.loads((live_meta if live_meta.is_file() else arch_meta).read_text())
        self.assertTrue(applied_meta.get("appliedAt"))
        self.assertEqual(applied_meta.get("appliedHead"), self.git(repo, "rev-parse", "HEAD"))

        # Replying to ask-q clears it from 待回答.
        self.fake_pi([fixtures.answer("noted A"), fixtures.SETTLED])
        self.assertEqual(self.cli("reply", "--wait", question_run["run"], "选 A").returncode, 0)

        after = self.cli("status", human=True)
        self.assertIn("运行中 1 · 待合入 0 · 待回答 0 · 排队 1", after.stdout.splitlines()[0])
        for name in ("unread-ro", "rej-one", "pending-apply", "ask-q"):
            self.assertNotIn(name, after.stdout)
        self.assertIn("另有 6 条已结束：delegate history", after.stdout)

        gate.touch()
        self.cli("wait", live["run"], queued["run"])

    def test_history_append_only_chinese_search_filters_and_2kb_cap(self):
        repo = self.repo({"apps/api/avatar.rs": "old\n", "apps/web/order.ts": "old\n"})
        long_prompt = "修复用户头像上传接口返回 500 的问题，补充大小限制和转码逻辑。" + ("检查边界条件与错误码。" * 40)
        long_answer = "## 结论\n已修复头像上传 500，限制单文件 5MB 并统一转为 WebP。\n\n## 详细修改\n" + ("细项说明。" * 200)
        self.fake_pi([fixtures.answer(long_answer), fixtures.SETTLED], pre="echo fixed > apps/api/avatar.rs")
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "--name", "修复头像上传", long_prompt)
        )
        self.assertEqual(self.cli("apply", r1["run"]).returncode, 0)

        self.fake_pi([fixtures.answer("订单 CSV 导出接口已完成并补全分页测试。"), fixtures.SETTLED],
                     pre="echo csv > apps/web/order.ts")
        r2 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--accept", "false", "--name", "订单导出",
                     "新增订单 CSV 导出接口并支持分页")
        )
        self.assertEqual(self.cli("clean", "--force", r1["run"]).returncode, 0)

        hist_files = self.history_files()
        self.assertEqual(len(hist_files), 1)
        raw_lines = [l for l in hist_files[0].read_text(encoding="utf-8").splitlines() if l.strip()]
        # Append-only: finish(r1), apply(r1), finish(r2), clean(r1).
        events = [json.loads(l)["event"] for l in raw_lines]
        self.assertIn("finish", events)
        self.assertIn("apply", events)
        self.assertIn("clean", events)
        # Every line strictly under 2 KiB even with long Chinese text.
        for line in raw_lines:
            self.assertLess(len(line.encode("utf-8")), 2048, f"history line too big: {len(line.encode('utf-8'))}")

        # `delegate history` lists oldest first, one line per run.
        hist_all = self.cli("history", human=True)
        self.assertEqual(hist_all.returncode, 0, hist_all.stderr)
        out_lines = hist_all.stdout.splitlines()
        self.assertEqual(len(out_lines), 2)
        self.assertIn("修复头像上传", out_lines[0])
        self.assertIn("已交付/已合入", out_lines[0])
        self.assertIn(f"delegate show {r1['run']}", out_lines[0])
        self.assertIn("订单导出", out_lines[1])
        self.assertIn("未通过", out_lines[1])

        # Chinese keyword search (exact + non-contiguous bigram).
        q_avatar = self.cli("history", "头像", human=True)
        self.assertEqual(len(q_avatar.stdout.splitlines()), 1)
        self.assertIn("修复头像上传", q_avatar.stdout)

        q_bigram = self.cli("history", "头像转码", human=True)
        self.assertEqual(len(q_bigram.stdout.splitlines()), 1)
        self.assertIn("修复头像上传", q_bigram.stdout)

        # `--state` and `--since` filters.
        q_applied = self.cli("history", "--state", "applied", "--json")
        applied_objs = [json.loads(l) for l in q_applied.stdout.splitlines() if l.strip()]
        self.assertEqual([o["run"] for o in applied_objs], [r1["run"]])
        self.assertTrue(applied_objs[0]["cleaned"])
        self.assertIn("apps/api", applied_objs[0]["filesByDir"])

        q_rej = self.cli("history", "--state", "rejected", "--since", "14d", "--json")
        rej_objs = [json.loads(l) for l in q_rej.stdout.splitlines() if l.strip()]
        self.assertEqual([o["run"] for o in rej_objs], [r2["run"]])

    def test_show_live_archived_subsequent_commits_and_chain(self):
        repo = self.repo({"src/avatar.rs": "v0\n"})
        self.fake_pi([fixtures.answer("## 结论\n初版完成头像校验。"), fixtures.SETTLED],
                     pre="echo v1 > src/avatar.rs")
        r1 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "--name", "avatar-fix",
                     "修复头像上传校验逻辑\n补充单元测试")
        )
        self.fake_pi([fixtures.answer("## 结论\n已按评审意见补上 WebP 转换。"), fixtures.SETTLED],
                     pre="echo v2 > src/avatar.rs")
        r2 = self.outcome(self.cli("reply", "--wait", r1["run"], "顺手加上 WebP 转换"))
        self.assertEqual(self.cli("apply", r2["run"]).returncode, 0)

        # Commit the applied change and a subsequent touch to the same file.
        self.git(repo, "add", "-A")
        self.git(repo, "commit", "-qm", "land avatar fix")
        (repo / "src/avatar.rs").write_text("v3\n")
        self.git(repo, "add", "-A")
        self.git(repo, "commit", "-qm", "tune avatar limit")

        # Show while uncleaned (live run dir).
        show_live = self.cli("show", r2["run"], human=True)
        self.assertEqual(show_live.returncode, 0, show_live.stderr)
        self.assertIn("顺手加上 WebP 转换", show_live.stdout)
        self.assertIn("已按评审意见补上 WebP 转换", show_live.stdout)
        self.assertIn("验收：验收通过", show_live.stdout)
        self.assertIn("land avatar fix", show_live.stdout)
        self.assertIn("tune avatar limit", show_live.stdout)
        self.assertIn("续做链：", show_live.stdout)
        self.assertIn(r1["run"], show_live.stdout)
        self.assertIn(f"[{r2['run']}]", show_live.stdout)
        self.assertIn(f"delegate result {r2['run']}", show_live.stdout)
        self.assertIn(f"delegate patch {r2['run']}", show_live.stdout)

        # Clean both runs -> archive keeps prompt, result, meta, patch, events.jsonl.gz; drops shims/session/worktree.
        self.assertEqual(self.cli("clean", "--force", r1["run"], r2["run"]).returncode, 0)
        self.assertFalse(Path(r2["dir"]).exists())
        arch = self.work / "cache/delegate/archive" / r2["run"]
        record = arch / "record"
        for kept in ("prompt.md", "result.md", "meta.json", "applied.patch", "events.jsonl.gz"):
            self.assertTrue((record / kept).is_file(), f"missing {kept} in archive")
        with gzip.open(record / "events.jsonl.gz", "rt", encoding="utf-8") as fh:
            self.assertTrue(fh.read().strip())
        for dropped in ("worktree", "agent-shims", "session", "fork"):
            self.assertFalse((record / dropped).exists(), f"{dropped} must not be archived")

        # Show AFTER clean reads from archive and still shows everything + subsequent commits.
        show_arch = self.cli("show", r2["run"], human=True)
        self.assertEqual(show_arch.returncode, 0, show_arch.stderr)
        self.assertIn("已按评审意见补上 WebP 转换", show_arch.stdout)
        self.assertIn("tune avatar limit", show_arch.stdout)
        self.assertIn(str(arch), show_arch.stdout)
        self.assertEqual(self.cli("patch", r2["run"]).returncode, 0)

    def test_dedup_positive_and_negative_against_active_and_14d_history(self):
        repo = self.repo({"a.txt": "1\n"})
        gate = self.work / "dedup-gate"
        # Finished recent task (within 14d).
        self.fake_pi([fixtures.answer("done"), fixtures.SETTLED])
        prev = self.outcome(
            self.cli("run", "--read-only", "--workdir", repo, "--name", "修复头像上传",
                     "修复用户头像上传接口返回 500 的问题，检查文件大小校验和格式转换")
        )
        # Old finished task (>14d ago) should NOT trigger dedup.
        old = self.outcome(
            self.cli("run", "--read-only", "--workdir", repo, "--name", "旧登录超时",
                     "排查旧版单点登录会话超时掉线的问题并修复 cookie 刷新")
        )
        hist_file = self.history_files()[0]
        old_ts = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(time.time() - 20 * 86400))
        patched = []
        for line in hist_file.read_text(encoding="utf-8").splitlines():
            obj = json.loads(line)
            if obj.get("run") == old["run"]:
                obj["startedAt"] = old_ts
                obj["finishedAt"] = old_ts
                obj["at"] = old_ts
            patched.append(json.dumps(obj, ensure_ascii=False))
        hist_file.write_text("\n".join(patched) + "\n", encoding="utf-8")

        # Active running task about avatar cropping.
        self.fake_pi(
            [fixtures.answer("working"), fixtures.SETTLED],
            pre=f'while [ ! -f {shlex.quote(str(gate))} ]; do sleep .05; done',
        )
        active = self.outcome(
            self.cli("start", "--read-only", "--workdir", repo, "--name", "用户头像裁剪",
                     "给前端个人资料页加上用户头像上传和裁剪功能")
        )
        self.addCleanup(lambda: (gate.touch(), self.cli("stop", active["run"])))
        fixtures.DelegateTests.until(
            self, lambda: (Path(active["dir"]) / "agent.pid").is_file(), "active agent started"
        )

        # Positive: paraphrased Chinese task ("同一任务换个说法") hits both recent & active, non-blocking (exit 0).
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
        dup = self.cli(
            "start", "--read-only", "--workdir", repo, "--name", "头像接口500",
            "排查上传用户头像时后端报 500 错误，看看大小限制与图片转码逻辑",
        )
        self.assertEqual(dup.returncode, 0, dup.stderr)
        similar_lines = [l for l in dup.stderr.splitlines() if l.startswith("相似：")]
        self.assertTrue(similar_lines, f"expected dedup hint in stderr:\n{dup.stderr}")
        self.assertLessEqual(len(similar_lines), 3)
        self.assertTrue(any(prev["run"] in l and f"delegate show {prev['run']}" in l for l in similar_lines),
                        similar_lines)

        # Positive against active running task.
        dup_active = self.cli(
            "start", "--read-only", "--workdir", repo, "--name", "前端头像裁剪",
            "实现个人主页的用户头像裁剪与上传",
        )
        self.assertEqual(dup_active.returncode, 0, dup_active.stderr)
        self.assertTrue(any(active["run"] in l and "运行中" in l for l in dup_active.stderr.splitlines()),
                        dup_active.stderr)

        # Negative: unrelated task -> no `相似：` line.
        unrelated = self.cli(
            "start", "--read-only", "--workdir", repo, "--name", "订单CSV导出",
            "给订单导出增加 CSV 下载接口，支持按日期范围筛选和分页查询",
        )
        self.assertEqual(unrelated.returncode, 0, unrelated.stderr)
        self.assertFalse(any(l.startswith("相似：") for l in unrelated.stderr.splitlines()), unrelated.stderr)

        # Negative: >14d old task -> no `相似：` line.
        old_dup = self.cli(
            "start", "--read-only", "--workdir", repo, "--name", "旧登录超时",
            "排查旧版单点登录会话超时掉线的问题并修复 cookie 刷新",
        )
        self.assertEqual(old_dup.returncode, 0, old_dup.stderr)
        self.assertFalse(any(l.startswith("相似：") and old["run"] in l for l in old_dup.stderr.splitlines()), old_dup.stderr)

        gate.touch()
        self.cli("wait", "--all")

    def test_archive_no_hard_cap_and_critical_fs_trims_only_events_preserving_core_and_history(self):
        repo = self.repo({"a.txt": "1\n"})
        self.fake_pi([fixtures.answer("任务结论：已验证缓存一致性。"), fixtures.SETTLED],
                     pre="echo changed > a.txt")
        runs = []
        archives = []
        for i in range(3):
            st = self.outcome(
                self.cli("run", "--worktree", "--workdir", repo, "--accept", "true",
                         "--name", f"arch-{i}", f"排查缓存一致性第 {i} 轮")
            )
            runs.append(st["run"])
            cleaned = json.loads(self.cli("clean", "--force", "--json", st["run"]).stdout.splitlines()[0])
            archives.append(Path(cleaned["archive"]))

        # No total-bytes hard cap: all 3 archives remain intact, and status/board show archive usage.
        for arch in archives:
            self.assertTrue((arch / "record/events.jsonl.gz").is_file())
            self.assertTrue((arch / "record/changes.patch").is_file())
        st_human = self.cli("status", human=True)
        self.assertIn("归档占用", st_human.stdout)
        self.assertIn("3 份", st_human.stdout)
        board_out = self.cli("board", human=True)
        self.assertIn("归档占用：", board_out.stdout)

        # Under critical FS free-space watermark, only least-recently-read events.jsonl.gz is trimmed;
        # prompt, result, meta, summary, patch, and history.jsonl are NEVER deleted.
        (archives[0] / ".access").write_text("1")
        (archives[1] / ".access").write_text("2")
        ev_size = (archives[0] / "record/events.jsonl.gz").stat().st_size
        self.env["DELEGATE_FS_CRITICAL_FREE_BYTES"] = str(ev_size + 100)
        self.env["DELEGATE_FS_CRITICAL_FREE_PCT"] = "0"
        self.env["DELEGATE_ARCHIVE_FS_AVAIL_BYTES"] = "100"
        st3 = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--accept", "true",
                     "--name", "arch-3", "排查缓存一致性第 3 轮")
        )
        clean3 = self.cli("clean", "--force", "--json", st3["run"])
        self.assertIn("critically low", clean3.stderr)
        self.assertFalse((archives[0] / "record/events.jsonl.gz").exists())
        self.assertTrue((archives[1] / "record/events.jsonl.gz").is_file())
        for kept in ("prompt.md", "result.md", "meta.json", "summary.json", "changes.patch"):
            self.assertTrue((archives[0] / "record" / kept).is_file(), f"missing {kept}")

        # history.jsonl still has all runs.
        hist_out = self.cli("history", "--json")
        hist_ids = [json.loads(l)["run"] for l in hist_out.stdout.splitlines() if l.strip()]
        for rid in runs + [st3["run"]]:
            self.assertIn(rid, hist_ids)

        # If an archive dir is manually missing on disk, `show` still explains original expired.
        import shutil
        shutil.rmtree(archives[0])
        show_expired = self.cli("show", runs[0], human=True)
        self.assertEqual(show_expired.returncode, 0, show_expired.stderr)
        self.assertIn("原文已过期，仅剩索引", show_expired.stdout)
        self.assertIn("已验证缓存一致性", show_expired.stdout)
        show_json = json.loads(self.cli("show", "--json", runs[0]).stdout)
        self.assertTrue(show_json["originalExpired"])

    def test_board_updates_on_every_lifecycle_transition_and_respects_repo_config(self):
        import signal
        import threading

        repo = self.repo({"a.txt": "base\n", "b.txt": "base\n"})
        # Invalid board config rejected with exit 2.
        (repo / ".delegate.json").write_text(json.dumps({"board": ""}))
        self.assertEqual(self.cli("config", cwd=repo).returncode, 2)

        # Configure repo-relative board path `.local/delegate/board.md`.
        (repo / ".delegate.json").write_text(json.dumps({"board": ".local/delegate/board.md"}))
        board_file = repo / ".local/delegate/board.md"

        # 1. Start + queued -> running transition.
        gate1 = self.work / "board-gate-1"
        gate2 = self.work / "board-gate-2"
        self.fake_pi(
            [fixtures.answer("first finished"), fixtures.SETTLED],
            pre=(
                f'case "$*" in '
                f'*run-one*) while [ ! -f {shlex.quote(str(gate1))} ]; do sleep .05; done;; '
                f'*run-two*) while [ ! -f {shlex.quote(str(gate2))} ]; do sleep .05; done;; '
                f'esac'
            ),
        )
        self.env["DELEGATE_MAX_ACTIVE"] = "1"
        r_live = self.outcome(self.cli("start", "--read-only", "--workdir", repo, "--name", "run-one", "run-one task", cwd=repo))
        r_queued = self.outcome(self.cli("start", "--read-only", "--workdir", repo, "--name", "run-two", "run-two task", cwd=repo))
        self.addCleanup(lambda: (gate1.touch(), gate2.touch(), self.cli("stop", r_live["run"], r_queued["run"], cwd=repo)))
        self.env.pop("DELEGATE_MAX_ACTIVE", None)

        deadline = time.time() + 10
        b1 = ""
        while time.time() < deadline:
            if board_file.is_file():
                b1 = board_file.read_text(encoding="utf-8")
                if "在跑 1（排队 1）" in b1.splitlines()[0] and "run-two" in b1:
                    break
            time.sleep(0.05)
        self.assertTrue(board_file.is_file(), "board file should be created automatically on start")
        self.assertIn("在跑 1（排队 1）", b1.splitlines()[0])
        self.assertIn("整机在途 2", b1.splitlines()[0])
        self.assertIn("## 在跑", b1)
        self.assertIn("run-one", b1)
        self.assertIn("run-two", b1)
        self.assertIn(f"delegate show {r_live['run']}", b1)

        # Release run-one -> run-one finishes (unread -> 待回答), run-two transitions queued -> running!
        gate1.touch()
        self.cli("wait", "--no-result", r_live["run"], cwd=repo)
        deadline = time.time() + 10
        while time.time() < deadline:
            b2 = board_file.read_text(encoding="utf-8")
            if "在跑 1（排队 0）" in b2.splitlines()[0] and "## 待回答" in b2:
                break
            time.sleep(0.1)
        self.assertIn("在跑 1（排队 0）", b2.splitlines()[0], b2)
        self.assertIn("## 待回答", b2)
        self.assertIn("run-one", b2)

        # `result` clears run-one from 待回答 on the board.
        self.assertEqual(self.cli("result", r_live["run"], cwd=repo).returncode, 0)
        b3 = board_file.read_text(encoding="utf-8")
        self.assertIn("待回答 0", b3.splitlines()[0], b3)

        # `stop` transitions run-two out of 在跑 into 失败或中断.
        self.assertEqual(self.cli("stop", r_queued["run"], cwd=repo).returncode, 0)
        b4 = board_file.read_text(encoding="utf-8")
        self.assertIn("在跑 0（排队 0）", b4.splitlines()[0], b4)
        self.assertIn("## 失败或中断", b4)
        self.assertIn("run-two", b4)

        # `clean` removes stopped run-two from 失败或中断.
        self.assertEqual(self.cli("clean", "--force", r_queued["run"], cwd=repo).returncode, 0)
        b5 = board_file.read_text(encoding="utf-8")
        self.assertIn("失败或中断 0", b5.splitlines()[0], b5)
        self.assertNotIn("run-two", b5)

        # Write run -> 待审合 -> apply -> 24 小时内已合入 (with short HEAD).
        self.fake_pi([fixtures.answer("## 结论\n已修改核心逻辑并补测。"), fixtures.SETTLED],
                     pre="echo updated > a.txt")
        r_write = self.outcome(
            self.cli("run", "--worktree", "--workdir", repo, "--accept", "true", "--name", "write-job", "改 a.txt", cwd=repo)
        )
        b6 = board_file.read_text(encoding="utf-8")
        self.assertIn("待审合 1", b6.splitlines()[0], b6)
        self.assertIn("## 待审合", b6)
        self.assertIn("write-job", b6)
        self.assertIn("已修改核心逻辑并补测", b6)
        self.assertIn("验收通过", b6)

        self.assertEqual(self.cli("apply", r_write["run"], cwd=repo).returncode, 0)
        short_head = self.git(repo, "rev-parse", "HEAD")[:7]
        b7 = board_file.read_text(encoding="utf-8")
        self.assertIn("待审合 0", b7.splitlines()[0], b7)
        self.assertIn("24 小时内已合入 1", b7.splitlines()[0], b7)
        self.assertIn("## 24 小时内已合入", b7)
        self.assertIn(f"HEAD {short_head}", b7)

        # Question -> 待回答 -> reply clears parent!
        self.fake_pi([fixtures.answer("需要主控决定\n选方案 1 还是方案 2？"), fixtures.SETTLED])
        r_q = self.outcome(self.cli("run", "--read-only", "--workdir", repo, "--name", "ask-choice", "decide", cwd=repo))
        b8 = board_file.read_text(encoding="utf-8")
        self.assertIn("待回答 1", b8.splitlines()[0], b8)
        self.assertIn("提问", b8)
        self.fake_pi([fixtures.answer("已按方案 1 处理。"), fixtures.SETTLED])
        self.assertEqual(self.cli("reply", "--wait", r_q["run"], "选方案 1", cwd=repo).returncode, 0)
        b9 = board_file.read_text(encoding="utf-8")
        self.assertIn("待回答 0", b9.splitlines()[0], b9)

        # Crash (kill supervisor with SIGKILL) -> `delegate board` detects crashed in 失败或中断.
        gate3 = self.work / "board-gate-3"
        self.fake_pi([fixtures.answer("never"), fixtures.SETTLED],
                     pre=f'while [ ! -f {shlex.quote(str(gate3))} ]; do sleep .05; done')
        r_crash = self.outcome(self.cli("start", "--read-only", "--workdir", repo, "--name", "crash-job", "crash test", cwd=repo))
        self.addCleanup(lambda: (gate3.touch(), self.cli("stop", r_crash["run"], cwd=repo)))
        sup_pid = int((Path(r_crash["dir"]) / "pid").read_text().strip())
        os.kill(sup_pid, signal.SIGKILL)
        for _ in range(40):
            if not Path(f"/proc/{sup_pid}").exists():
                break
            time.sleep(0.05)
        board_cmd = self.cli("board", human=True, cwd=repo)
        self.assertEqual(board_cmd.returncode, 0, board_cmd.stderr)
        self.assertIn("## 失败或中断", board_cmd.stdout)
        self.assertIn("crash-job", board_cmd.stdout)
        self.assertIn("进程异常", board_cmd.stdout)
        self.assertEqual(board_cmd.stdout, board_file.read_text(encoding="utf-8"))

        # Concurrent `delegate board` writes never yield a partial/corrupt file.
        errors = []
        stop_reader = threading.Event()
        def reader():
            while not stop_reader.is_set():
                try:
                    txt = board_file.read_text(encoding="utf-8")
                    if not (txt.startswith("在跑 ") and "delegate history\n" in txt):
                        errors.append(f"torn read: {txt!r}")
                except FileNotFoundError:
                    errors.append("board disappeared during atomic rename")
        t_read = threading.Thread(target=reader)
        t_read.start()
        procs = [
            subprocess.Popen(
                [str(fixtures.DELEGATE), "board"],
                cwd=repo, env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
            )
            for _ in range(6)
        ]
        for p in procs:
            p.communicate(timeout=15)
            self.assertEqual(p.returncode, 0)
        stop_reader.set()
        t_read.join()
        self.assertEqual(errors, [])

        # Unwritable board path only prints diagnostic; does not fail delegation.
        blocker = repo / ".local/board-blocker"
        blocker.parent.mkdir(parents=True, exist_ok=True)
        blocker.write_text("file blocking dir")
        (repo / ".delegate.json").write_text(json.dumps({"board": ".local/board-blocker/board.md"}))
        self.fake_pi([fixtures.answer("still ok"), fixtures.SETTLED])
        res = self.cli("run", "--read-only", "--workdir", repo, "--name", "degrade-ok", "check", cwd=repo)
        self.assertEqual(res.returncode, 0, res.stdout + res.stderr)
        self.assertIn("board update failed", res.stderr)
        gate3.touch()

    def test_backfill_existing_runs_and_archives_without_migrating_dirs(self):
        runs_root = self.work / "runs"
        runs_root.mkdir(parents=True, exist_ok=True)
        # Synthetic pre-6.24 uncleaned run.
        legacy_live = runs_root / "20261110-100000-legacy-live"
        legacy_live.mkdir()
        (legacy_live / "meta.json").write_text(json.dumps({
            "run": "20261110-100000-legacy-live",
            "dir": str(legacy_live),
            "name": "旧版头像检查",
            "mode": "read-only",
            "agent": "pi",
            "tier": "review",
            "model": "p/m",
            "startedAt": "2026-11-10T10:00:00Z",
            "startedEpoch": 1794304800,
            "startedNs": 1794304800000000000,
        }))
        (legacy_live / "summary.json").write_text(json.dumps({
            "state": "answered",
            "finishedAt": "2026-11-10T10:01:00Z",
            "elapsedSeconds": 60,
        }))
        (legacy_live / "prompt.md").write_text("检查旧版头像上传权限\n")
        (legacy_live / "result.md").write_text("## 结论\n权限校验完整。\n")
        (legacy_live / "exit_code").write_text("0\n")
        (legacy_live / ".delivered").write_text("")

        # Synthetic pre-6.24 cleaned archive record.
        arch_dir = self.work / "cache/delegate/archive/20261111-110000-legacy-arch"
        rec_dir = arch_dir / "record"
        rec_dir.mkdir(parents=True)
        manifest = {"path": str(arch_dir), "bytes": 0, "files": [], "skipped": [], "createdAt": "2026-11-11T11:02:00Z"}
        (arch_dir / "manifest.json").write_text(json.dumps(manifest))
        (rec_dir / "archive.json").write_text(json.dumps(manifest))
        (rec_dir / "meta.json").write_text(json.dumps({
            "run": "20261111-110000-legacy-arch",
            "dir": str(runs_root / "20261111-110000-legacy-arch"),
            "name": "旧版归档任务",
            "mode": "read-only",
            "agent": "pi",
            "model": "p/m",
            "startedAt": "2026-11-11T11:00:00Z",
            "startedEpoch": 1794394800,
            "startedNs": 1794394800000000000,
        }))
        (rec_dir / "summary.json").write_text(json.dumps({
            "state": "answered",
            "finishedAt": "2026-11-11T11:02:00Z",
            "elapsedSeconds": 120,
        }))
        (rec_dir / "result.md").write_text("旧版归档的结论内容。\n")
        (rec_dir / "exit_code").write_text("0\n")

        # First command triggers backfill without moving directories.
        hist = self.cli("history", human=True)
        self.assertEqual(hist.returncode, 0, hist.stderr)
        self.assertIn("旧版头像检查", hist.stdout)
        self.assertIn("旧版归档任务", hist.stdout)
        self.assertTrue(legacy_live.is_dir())
        self.assertTrue(rec_dir.is_dir())

        show_arch = self.cli("show", "20261111-110000-legacy-arch", human=True)
        self.assertEqual(show_arch.returncode, 0, show_arch.stderr)
        self.assertIn("旧版归档的结论内容", show_arch.stdout)

    def test_280_plus_history_repo_status_concise_and_search(self):
        runs_root = self.work / "runs"
        runs_root.mkdir(parents=True, exist_ok=True)
        base_ts = int(time.time()) - 30 * 86400
        for i in range(285):
            ts = base_ts + i * 3600
            iso_s = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts))
            iso_f = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(ts + 45))
            rid = f"20261101-{i:06d}-task-{i}"
            d = runs_root / rid
            d.mkdir()
            is_avatar = i in (42, 180, 270)
            name = f"修复头像服务-{i}" if is_avatar else f"常规子任务-{i}"
            prompt = f"修复用户头像上传与裁剪异常 #{i}" if is_avatar else f"处理常规后台模块 #{i}"
            ans = f"## 结论\n头像处理链路 #{i} 已修复。" if is_avatar else f"## 结论\n模块 #{i} 已完成。"
            (d / "meta.json").write_text(json.dumps({
                "run": rid, "dir": str(d), "name": name, "mode": "read-only",
                "agent": "pi", "tier": "review", "model": "p/sol", "modelName": "sol",
                "startedAt": iso_s, "startedEpoch": ts, "startedNs": ts * 10**9,
            }))
            (d / "summary.json").write_text(json.dumps({
                "state": "answered", "finishedAt": iso_f, "elapsedSeconds": 45,
            }))
            (d / "prompt.md").write_text(prompt + "\n")
            (d / "result.md").write_text(ans + "\n")
            (d / "exit_code").write_text("0\n")
            (d / ".delivered").write_text("")

        # Default status on a 285-run repo is only 2 lines (count header + history pointer)!
        st = self.cli("status", human=True)
        self.assertEqual(st.returncode, 0, st.stderr)
        lines = st.stdout.splitlines()
        self.assertEqual(len(lines), 2, st.stdout)
        self.assertIn("运行中 0 · 待合入 0 · 待回答 0 · 排队 0", lines[0])
        self.assertIn("另有 285 条已结束：delegate history", lines[1])

        # Querying `history 头像` returns exactly the 3 avatar runs, oldest first.
        q = self.cli("history", "头像", human=True)
        self.assertEqual(q.returncode, 0, q.stderr)
        q_lines = q.stdout.splitlines()
        self.assertEqual(len(q_lines), 3, q.stdout)
        self.assertIn("修复头像服务-42", q_lines[0])
        self.assertIn("修复头像服务-180", q_lines[1])
        self.assertIn("修复头像服务-270", q_lines[2])

    def test_two_workspaces_keep_separate_boards_with_machine_wide_in_flight_count(self):
        repo_a = self.work / "repo-a"
        repo_b = self.work / "repo-b"
        for repo, name in ((repo_a, "a"), (repo_b, "b")):
            repo.mkdir()
            self.git(repo, "init", "-q")
            (repo / f"{name}.txt").write_text("1\n")
            self.git(repo, "add", "-A")
            self.git(repo, "commit", "-qm", "init")
        gate = self.work / "two-ws-gate"
        self.fake_pi(
            [fixtures.answer("done"), fixtures.SETTLED],
            pre=f'while [ ! -f {shlex.quote(str(gate))} ]; do sleep .05; done',
        )
        # Drop explicit DELEGATE_RUNS so each repo uses its own `.local/run/delegate` and history/BOARD.md.
        env_no_runs = {k: v for k, v in self.env.items() if k != "DELEGATE_RUNS"}
        def run_in(repo, *args):
            return subprocess.run(
                [str(fixtures.DELEGATE), *args],
                cwd=repo, env=env_no_runs, capture_output=True, text=True, timeout=30,
            )
        r_a = json.loads(run_in(repo_a, "start", "--json", "--read-only", "--name", "job-a", "work in a").stdout)
        r_b = json.loads(run_in(repo_b, "start", "--json", "--read-only", "--name", "job-b", "work in b").stdout)
        self.addCleanup(lambda: (
            gate.touch(),
            run_in(repo_a, "stop", r_a["run"]),
            run_in(repo_b, "stop", r_b["run"]),
        ))
        path_a = Path(run_in(repo_a, "board", "--path").stdout.strip())
        path_b = Path(run_in(repo_b, "board", "--path").stdout.strip())
        self.assertNotEqual(path_a, path_b)
        self.assertEqual(path_a.name, "BOARD.md")
        self.assertEqual(path_b.name, "BOARD.md")
        board_a = run_in(repo_a, "board").stdout
        board_b = run_in(repo_b, "board").stdout
        self.assertIn("在跑 1（排队 0）· 整机在途 2", board_a.splitlines()[0], board_a)
        self.assertIn("job-a", board_a)
        self.assertNotIn("job-b", board_a)
        self.assertIn("在跑 1（排队 0）· 整机在途 2", board_b.splitlines()[0], board_b)
        self.assertIn("job-b", board_b)
        self.assertNotIn("job-a", board_b)
        gate.touch()
        run_in(repo_a, "wait", r_a["run"])
        run_in(repo_b, "wait", r_b["run"])


if __name__ == "__main__":
    unittest.main()
