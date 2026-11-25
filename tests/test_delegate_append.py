import json
import shlex
import unittest
from pathlib import Path

import test_delegate as fixtures


APPEND_HEADER = (
    "追加问题（主控在你作答期间补充）：把它并进你的完整答复，最终答复要覆盖原任务和全部追加"
)


class DelegateAppendTests(unittest.TestCase):
    setUp = fixtures.DelegateTests.setUp
    fake_pi = fixtures.DelegateTests.fake_pi
    cli = fixtures.DelegateTests.cli
    outcome = fixtures.DelegateTests.outcome
    repo = fixtures.DelegateTests.repo
    until = fixtures.DelegateTests.until
    user_config = fixtures.DelegateTests.user_config

    def _install_fake_agy(self, script_body: str) -> Path:
        self.fake_pi([fixtures.answer("ok"), fixtures.SETTLED])
        fake_agy = self.bin / "agy"
        fake_agy.write_text("#!/usr/bin/env python3\n" + script_body, encoding="utf-8")
        fake_agy.chmod(0o755)
        self.env["DELEGATE_AGY_BIN"] = str(fake_agy)
        self.user_config(
            {
                "models": {
                    **fixtures.REGISTERED_MODELS,
                    "agyappend": "agy/gemini-4-neon",
                }
            }
        )
        return fake_agy

    def test_agy_running_reply_appends_merges_two_and_runs_accept_once(self):
        repo = self.repo({"a.txt": "v0\n"})
        turn1_ready = self.work / "turn1-ready"
        turn1_go = self.work / "turn1-go"
        stdin_log = self.work / "agy-stdin-lines.jsonl"
        accept_log = self.work / "accept-count.log"

        self._install_fake_agy(
            f"""
import json, os, select, sys, time
from pathlib import Path

if "--version" in sys.argv[1:]:
    print("1.6.0")
    sys.exit(0)

turn1_ready = Path({str(turn1_ready)!r})
turn1_go = Path({str(turn1_go)!r})
stdin_log = Path({str(stdin_log)!r})
conv_id = "conv-append-multiturn"

print(json.dumps({{"event": "init", "conversation_id": conv_id}}), flush=True)

turn = 0
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    turn += 1
    with stdin_log.open("a", encoding="utf-8") as f:
        f.write(line + "\\n")
    if turn == 1:
        turn1_ready.touch()
        while not turn1_go.exists():
            time.sleep(0.02)
        Path("a.txt").write_text("round1\\n", encoding="utf-8")
        print(json.dumps({{
            "event": "step_update",
            "step_update": {{"step_index": 1, "step_type": "agent_response", "state": "DONE", "text_delta": "第一轮答复"}},
        }}), flush=True)
        print(json.dumps({{
            "event": "result",
            "result": {{"conversation_id": conv_id, "status": "SUCCESS", "response": "第一轮答复", "usage": {{"input_tokens": 100, "output_tokens": 20, "cache_read_tokens": 10}}}},
        }}), flush=True)
    elif turn == 2:
        Path("a.txt").write_text("round2\\n", encoding="utf-8")
        print(json.dumps({{
            "event": "result",
            "result": {{"conversation_id": conv_id, "status": "SUCCESS", "response": "第二轮完整答复（含全部追加）", "usage": {{"input_tokens": 180, "output_tokens": 55, "cache_read_tokens": 90}}}},
        }}), flush=True)
"""
        )

        accept_cmd = f"echo accept >> {shlex.quote(str(accept_log))}"
        started = self.outcome(
            self.cli(
                "start",
                "--worktree",
                "--workdir",
                repo,
                "--model",
                "agyappend",
                "--name",
                "design",
                "--accept",
                accept_cmd,
                "原始设计任务",
            )
        )
        run_id = started["run"]
        run_dir = Path(started["dir"])
        self.addCleanup(lambda: (turn1_go.touch(), self.cli("stop", run_id)))

        self.until(turn1_ready.exists, "agy turn 1 started")

        # 1) Default reply on running run appends directly (no --when-done required).
        r1 = self.cli("reply", run_id, "补充问题A", human=True)
        self.assertEqual(r1.returncode, 0, r1.stdout + r1.stderr)
        self.assertEqual(
            r1.stdout.strip(),
            "已追加到 design：当前轮结束后在同一会话接着答（第 1 条追加）",
        )

        # 2) Second append via --when-done alias queues into the same mailbox.
        r2 = self.cli("reply", "--when-done", run_id, "补充问题B", human=True)
        self.assertEqual(r2.returncode, 0, r2.stdout + r2.stderr)
        self.assertEqual(
            r2.stdout.strip(),
            "已追加到 design：当前轮结束后在同一会话接着答（第 2 条追加）",
        )

        # Status shows total=2, pending=2 before turn boundary.
        st_human = self.cli("status", run_id, human=True)
        self.assertEqual(st_human.returncode, 0, st_human.stderr)
        self.assertIn("已追加 2 条，待投递 2 条", st_human.stdout)
        st_json = self.outcome(self.cli("status", run_id))
        self.assertEqual(st_json["appends"]["total"], 2)
        self.assertEqual(st_json["appends"]["pending"], 2)
        self.assertEqual(st_json["appends"]["delivered"], 0)

        # Release turn 1 -> supervisor merges both appends into ONE stdin line for turn 2,
        # then closes stdin when turn 2 ends with empty mailbox.
        turn1_go.touch()
        waited = self.cli("wait", "--all", run_id)
        self.assertEqual(waited.returncode, 0, waited.stdout + waited.stderr)
        final_state = self.outcome(waited)
        self.assertEqual(final_state["state"], "delivered")

        # Final result.md is round 2's answer.
        self.assertEqual(
            (run_dir / "result.md").read_text(encoding="utf-8").strip(),
            "第二轮完整答复（含全部追加）",
        )

        # Per-round answers are preserved and inspectable via `result --round`.
        res_r1 = self.cli("result", run_id, "--round", "1", human=True)
        self.assertEqual(res_r1.returncode, 0, res_r1.stderr)
        self.assertEqual(res_r1.stdout.strip(), "第一轮答复")
        res_r2 = self.cli("result", run_id, "--round", "2", human=True)
        self.assertEqual(res_r2.returncode, 0, res_r2.stderr)
        self.assertEqual(res_r2.stdout.strip(), "第二轮完整答复（含全部追加）")
        res_rounds = self.cli("result", run_id, "--rounds", human=True)
        self.assertEqual(res_rounds.returncode, 0, res_rounds.stderr)
        self.assertIn("初稿", res_rounds.stdout)
        self.assertIn("追加", res_rounds.stdout)

        # Both appends were merged and delivered ONCE as stdin line 2.
        lines = [json.loads(x) for x in stdin_log.read_text(encoding="utf-8").splitlines()]
        self.assertEqual(len(lines), 2, f"expected 2 stdin turns, got {len(lines)}")
        merged_content = lines[1]["message"]["content"]
        self.assertTrue(merged_content.startswith(APPEND_HEADER), merged_content)
        self.assertIn("1. 补充问题A", merged_content)
        self.assertIn("2. 补充问题B", merged_content)
        self.assertLess(
            merged_content.index("1. 补充问题A"),
            merged_content.index("2. 补充问题B"),
        )

        # Accept ran ONLY ONCE (after round 2), and no child run was spawned.
        self.assertEqual(accept_log.read_text(encoding="utf-8").splitlines(), ["accept"])
        self.assertFalse((run_dir / "reply-delivered.json").exists())
        self.assertFalse((run_dir / "pending-reply.json").exists())

        # status and card both display 「已追加 2 条，待投递 0 条」.
        self.assertIn("已追加 2 条，待投递 0 条", self.cli("status", run_id, human=True).stdout)
        card = self.cli("card", run_id, human=True)
        self.assertEqual(card.returncode, 0, card.stderr)
        self.assertIn("已追加 2 条，待投递 0 条", card.stdout)

    def test_agy_append_after_stdin_closed_falls_back_to_new_round(self):
        repo = self.repo({"a.txt": "v0\n"})
        stdin_closed = self.work / "agy-stdin-closed"
        exit_gate = self.work / "agy-exit-gate"
        child_record = self.work / "agy-child-record.json"

        self._install_fake_agy(
            f"""
import json, sys, time
from pathlib import Path

if "--version" in sys.argv[1:]:
    print("1.6.0")
    sys.exit(0)

stdin_closed = Path({str(stdin_closed)!r})
exit_gate = Path({str(exit_gate)!r})
child_record = Path({str(child_record)!r})
conv_id = "conv-race-fallback"

is_continued = "--conversation" in sys.argv[1:]
print(json.dumps({{"event": "init", "conversation_id": conv_id}}), flush=True)

received = []
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    received.append(json.loads(line))
    if not is_continued:
        print(json.dumps({{
            "event": "result",
            "result": {{"conversation_id": conv_id, "status": "SUCCESS", "response": "第一轮收尾答复", "usage": {{"input_tokens": 50, "output_tokens": 10, "cache_read_tokens": 0}}}},
        }}), flush=True)
    else:
        child_record.write_text(json.dumps({{"argv": sys.argv[1:], "messages": received}}), encoding="utf-8")
        print(json.dumps({{
            "event": "result",
            "result": {{"conversation_id": conv_id, "status": "SUCCESS", "response": "兜底新round答复", "usage": {{"input_tokens": 80, "output_tokens": 20, "cache_read_tokens": 40}}}},
        }}), flush=True)

if not is_continued:
    # Stdin has now hit EOF (closed by supervisor); pause before process exit so the test can
    # deterministically race a late `reply` after stdin closure.
    stdin_closed.touch()
    while not exit_gate.exists():
        time.sleep(0.02)
"""
        )

        started = self.outcome(
            self.cli(
                "start",
                "--read-only",
                "--workdir",
                repo,
                "--model",
                "agyappend",
                "--name",
                "race",
                "原始任务",
            )
        )
        run_id = started["run"]
        run_dir = Path(started["dir"])
        self.addCleanup(lambda: (exit_gate.touch(), self.cli("stop", run_id)))

        # Wait until supervisor has closed stdin (empty mailbox on turn 1), while agy is still exiting.
        self.until(stdin_closed.exists, "agy observed stdin EOF")

        # Late reply arrives AFTER stdin closed -> must fall back to post-settle pending_reply.
        late1 = self.cli("reply", run_id, "收尾后追加一", human=True)
        self.assertEqual(late1.returncode, 0, late1.stdout + late1.stderr)
        self.assertIn("已追加到 race", late1.stdout)
        self.assertIn("已退回结束后同会话开新 round", late1.stdout)
        self.assertIn("第 1 条追加", late1.stdout)

        late2 = self.cli("reply", run_id, "收尾后追加二", human=True)
        self.assertEqual(late2.returncode, 0, late2.stdout + late2.stderr)
        self.assertIn("已退回结束后同会话开新 round", late2.stdout)
        self.assertIn("第 2 条追加", late2.stdout)

        self.assertIn("已追加 2 条，待投递 2 条", self.cli("status", run_id, human=True).stdout)

        # Let parent agy exit -> parent settles -> `_reply-pending` opens ONE new round on same conversation.
        exit_gate.touch()
        receipt = run_dir / "reply-delivered.json"
        self.until(receipt.exists, "fallback pending reply launched child run")
        child_dir = Path(json.loads(receipt.read_text(encoding="utf-8"))["run"])
        waited_child = self.cli("wait", "--all", str(child_dir))
        self.assertEqual(waited_child.returncode, 0, waited_child.stdout + waited_child.stderr)

        self.assertEqual(
            (run_dir / "result.md").read_text(encoding="utf-8").strip(),
            "第一轮收尾答复",
        )
        self.assertEqual(
            (child_dir / "result.md").read_text(encoding="utf-8").strip(),
            "兜底新round答复",
        )
        rec = json.loads(child_record.read_text(encoding="utf-8"))
        self.assertIn("--conversation", rec["argv"])
        self.assertIn("conv-race-fallback", rec["argv"])
        child_text = rec["messages"][0]["message"]["content"]
        self.assertIn("1. 收尾后追加一", child_text)
        self.assertIn("2. 收尾后追加二", child_text)
        self.assertFalse((run_dir / "pending-reply.json").exists())
        self.assertIn("已追加 2 条，待投递 0 条", self.cli("status", run_id, human=True).stdout)

    def test_stop_clears_pending_appends_mailbox(self):
        repo = self.repo({"a.txt": "v0\n"})
        turn1_ready = self.work / "stop-turn1-ready"
        turn1_go = self.work / "stop-turn1-go"

        self._install_fake_agy(
            f"""
import json, sys, time
from pathlib import Path

if "--version" in sys.argv[1:]:
    print("1.6.0")
    sys.exit(0)

turn1_ready = Path({str(turn1_ready)!r})
turn1_go = Path({str(turn1_go)!r})
print(json.dumps({{"event": "init", "conversation_id": "conv-stop"}}), flush=True)
for raw in sys.stdin:
    if not raw.strip():
        continue
    turn1_ready.touch()
    while not turn1_go.exists():
        time.sleep(0.02)
"""
        )

        started = self.outcome(
            self.cli("start", "--read-only", "--workdir", repo, "--model", "agyappend", "--name", "stoppable", "任务")
        )
        run_id = started["run"]
        run_dir = Path(started["dir"])
        self.addCleanup(lambda: (turn1_go.touch(), self.cli("stop", run_id)))

        self.until(turn1_ready.exists, "agy running")
        self.assertEqual(self.cli("reply", run_id, "待取消一").returncode, 0)
        self.assertEqual(self.cli("reply", run_id, "待取消二").returncode, 0)
        self.assertIn("已追加 2 条，待投递 2 条", self.cli("status", run_id, human=True).stdout)

        stopped = self.cli("stop", run_id)
        self.assertEqual(stopped.returncode, 0, stopped.stderr)

        st = self.outcome(self.cli("status", run_id))
        self.assertEqual(st["state"], "stopped")
        self.assertNotIn("pendingReply", st)
        self.assertNotIn("appends", st)
        self.assertFalse((run_dir / "pending-reply.json").exists())
        self.assertFalse((run_dir / "reply-delivered.json").exists())
        mailbox = json.loads((run_dir / "appends.json").read_text(encoding="utf-8"))
        self.assertTrue(mailbox["closed"])
        self.assertEqual(len(mailbox["items"]), 2)
        for item in mailbox["items"]:
            self.assertIsNone(item["deliveredAt"])
            self.assertIsNotNone(item["cancelledAt"])

    def test_pi_running_reply_appends_at_turn_boundary_same_run(self):
        repo = self.repo({"a.txt": "v0\n"})
        ready = self.work / "pi-ready"
        gate = self.work / "pi-gate"
        accept_log = self.work / "pi-accept-count.log"
        self.fake_pi(
            [fixtures.answer("Pi 第一轮答复"), fixtures.SETTLED],
            pre=f"echo r1 > a.txt; touch {shlex.quote(str(ready))}; while [ ! -f {shlex.quote(str(gate))} ]; do sleep .02; done",
        )
        accept_cmd = f"echo accept >> {shlex.quote(str(accept_log))}"
        started = self.outcome(
            self.cli(
                "start",
                "--worktree",
                "--workdir",
                repo,
                "--name",
                "piapp",
                "--accept",
                accept_cmd,
                "原始 Pi 任务",
            )
        )
        run_id = started["run"]
        run_dir = Path(started["dir"])
        self.addCleanup(lambda: (gate.touch(), self.cli("stop", run_id)))

        self.until(ready.exists, "pi turn 1 running")
        r1 = self.cli("reply", run_id, "Pi 追加一", human=True)
        self.assertEqual(r1.returncode, 0, r1.stderr)
        self.assertEqual(
            r1.stdout.strip(),
            "已追加到 piapp：当前轮结束后在同一会话接着答（第 1 条追加）",
        )
        r2 = self.cli("reply", run_id, "Pi 追加二", human=True)
        self.assertEqual(r2.returncode, 0, r2.stderr)
        self.assertIn("第 2 条追加", r2.stdout)

        # Swap fake_pi for turn 2 before releasing turn 1 gate.
        self.fake_pi([fixtures.answer("Pi 第二轮合并答复"), fixtures.SETTLED], pre="echo r2 > a.txt")
        gate.touch()

        waited = self.cli("wait", "--all", run_id)
        self.assertEqual(waited.returncode, 0, waited.stdout + waited.stderr)
        self.assertEqual(self.outcome(waited)["state"], "delivered")
        self.assertEqual(
            (run_dir / "result.md").read_text(encoding="utf-8").strip(),
            "Pi 第二轮合并答复",
        )
        self.assertEqual(
            self.cli("result", run_id, "--round", "1", human=True).stdout.strip(),
            "Pi 第一轮答复",
        )
        self.assertEqual(
            self.cli("result", run_id, "--round", "2", human=True).stdout.strip(),
            "Pi 第二轮合并答复",
        )
        merged_prompt = (run_dir / "append-round-2-prompt.md").read_text(encoding="utf-8")
        self.assertTrue(merged_prompt.startswith(APPEND_HEADER), merged_prompt)
        self.assertIn("1. Pi 追加一", merged_prompt)
        self.assertIn("2. Pi 追加二", merged_prompt)
        self.assertEqual(accept_log.read_text(encoding="utf-8").splitlines(), ["accept"])
        self.assertIn("已追加 2 条，待投递 0 条", self.cli("status", run_id, human=True).stdout)
        self.assertIn("已追加 2 条，待投递 0 条", self.cli("card", run_id, human=True).stdout)

    def _install_fake_pi_rpc(self, script_body: str) -> Path:
        fake_pi = self.bin / "pi"
        fake_pi.write_text("#!/usr/bin/env python3\n" + script_body, encoding="utf-8")
        fake_pi.chmod(0o755)
        self.env["DELEGATE_PI_BIN"] = str(fake_pi)
        return fake_pi

    def test_pi_rpc_steer_injects_mid_turn_and_closes_cleanly(self):
        repo = self.repo({"a.txt": "v0\n"})
        tool_running = self.work / "pi-rpc-tool-running"
        rpc_log = self.work / "pi-rpc-received.jsonl"
        accept_log = self.work / "pi-rpc-accept.log"

        self._install_fake_pi_rpc(
            f"""
import json, os, sys
from pathlib import Path

args = sys.argv[1:]
if args == ["--version"]:
    print("rpi 0.20.0 (fake-rpc)")
    sys.exit(0)
if len(args) >= 2 and args[1] == "--list-models":
    print("provider model context max-out thinking images")
    print("p m 1M 1K yes yes")
    print("p scout-m 64k 1K no no")
    print("p review-m 1M 1K yes yes")
    print("p write-m 1M 1K yes yes")
    sys.exit(0)

is_probe = "--no-session" in args
tool_running = Path({str(tool_running)!r})
rpc_log = Path({str(rpc_log)!r})

# Create session file if --session-dir given
sdir, sid = "", ""
for i, a in enumerate(args):
    if a == "--session-dir" and i + 1 < len(args):
        sdir = args[i + 1]
    elif a == "--session-id" and i + 1 < len(args):
        sid = args[i + 1]
if sdir and not is_probe:
    Path(sdir).mkdir(parents=True, exist_ok=True)
    (Path(sdir) / f"t_{{sid or os.getpid()}}.jsonl").write_text("{{}}\\n", encoding="utf-8")

print(json.dumps({{
    "type": "ready",
    "protocol": "rpi",
    "supportedCommands": ["prompt", "queue_message", "get_queue", "clear_queue", "stop"],
}}), flush=True)

queued_items = []
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    cmd = json.loads(line)
    ctype = cmd.get("type")
    cid = cmd.get("id")
    if not is_probe:
        with rpc_log.open("a", encoding="utf-8") as f:
            f.write(json.dumps(cmd, ensure_ascii=False) + "\\n")
    if ctype == "prompt":
        print(json.dumps({{"type": "response", "command": "prompt", "success": True, "id": cid, "data": {{"accepted": True}}}}), flush=True)
        print(json.dumps({{"type": "agent_start"}}), flush=True)
        print(json.dumps({{"type": "tool_execution_start", "toolName": "bash", "args": {{"command": "cargo check"}}}}), flush=True)
        tool_running.touch()
    elif ctype == "queue_message":
        q = "steer" if cmd.get("deliverAs") == "steer" else "followUp"
        eid = f"entry-{{cid}}"
        print(json.dumps({{
            "type": "response",
            "command": "queue_message",
            "success": True,
            "id": cid,
            "data": {{"accepted": True, "entryId": eid, "queue": q}},
        }}), flush=True)
        if not is_probe and q == "steer":
            # Current tool finishes -> steered message is consumed by this turn -> single unified answer & agent_settled.
            print(json.dumps({{"type": "tool_execution_end", "toolName": "bash", "isError": False}}), flush=True)
            Path("a.txt").write_text("steered-edit\\n", encoding="utf-8")
            print(json.dumps({{
                "type": "message_end",
                "message": {{
                    "role": "assistant",
                    "stopReason": "stop",
                    "provider": "p",
                    "model": "write-m",
                    "usage": {{"input": 100, "output": 40, "cacheRead": 20}},
                    "content": [{{"type": "text", "text": "已通过 steer 并入追加后的完整答复"}}],
                }},
            }}), flush=True)
            print(json.dumps({{"type": "agent_settled", "outcome": "completed"}}), flush=True)
    elif ctype == "get_queue":
        print(json.dumps({{
            "type": "response",
            "command": "get_queue",
            "success": True,
            "id": cid,
            "data": {{"items": queued_items, "count": len(queued_items)}},
        }}), flush=True)
    elif ctype == "clear_queue":
        cleared = len(queued_items)
        queued_items = []
        print(json.dumps({{
            "type": "response",
            "command": "clear_queue",
            "success": True,
            "id": cid,
            "data": {{"cleared": cleared}},
        }}), flush=True)
    elif ctype == "stop":
        print(json.dumps({{"type": "response", "command": "stop", "success": True, "id": cid, "data": {{"stopped": True}}}}), flush=True)
        sys.exit(0)
"""
        )

        accept_cmd = f"echo accept >> {shlex.quote(str(accept_log))}"
        started = self.outcome(
            self.cli(
                "start",
                "--worktree",
                "--workdir",
                repo,
                "--name",
                "rpi-steer",
                "--accept",
                accept_cmd,
                "原始 Pi RPC 任务",
            )
        )
        run_id = started["run"]
        run_dir = Path(started["dir"])
        self.addCleanup(lambda: self.cli("stop", run_id))

        self.until(tool_running.exists, "pi RPC entered mid-turn tool execution")
        self.assertTrue((run_dir / ".pi-rpc-steer").is_file())

        # Send reply mid-turn -> immediately pushed via RPC `queue_message(deliverAs: "steer")`.
        rep = self.cli("reply", run_id, "轮中实时 steer 追加", human=True)
        self.assertEqual(rep.returncode, 0, rep.stdout + rep.stderr)
        self.assertIn("已追加到 rpi-steer", rep.stdout)
        self.assertIn("steer", rep.stdout)
        self.assertIn("第 1 条追加", rep.stdout)

        waited = self.cli("wait", "--all", run_id)
        self.assertEqual(waited.returncode, 0, waited.stdout + waited.stderr)
        self.assertEqual(self.outcome(waited)["state"], "delivered")

        cmds = [json.loads(x) for x in rpc_log.read_text(encoding="utf-8").splitlines()]
        steer_cmds = [c for c in cmds if c.get("type") == "queue_message"]
        self.assertEqual(len(steer_cmds), 1, f"expected 1 steer command, got {cmds}")
        self.assertEqual(steer_cmds[0].get("deliverAs"), "steer")
        self.assertTrue(steer_cmds[0]["message"].startswith(APPEND_HEADER))
        self.assertIn("1. 轮中实时 steer 追加", steer_cmds[0]["message"])
        self.assertEqual(cmds[-1].get("type"), "stop")

        self.assertEqual(
            (run_dir / "result.md").read_text(encoding="utf-8").strip(),
            "已通过 steer 并入追加后的完整答复",
        )
        self.assertEqual(accept_log.read_text(encoding="utf-8").splitlines(), ["accept"])
        self.assertFalse((run_dir / "reply-delivered.json").exists())
        self.assertFalse((run_dir / "pending-reply.json").exists())
        self.assertIn("已追加 1 条，待投递 0 条", self.cli("status", run_id, human=True).stdout)
        self.assertIn("已追加 1 条，待投递 0 条", self.cli("card", run_id, human=True).stdout)

    def test_pi_rpc_fallback_after_stop_merges_multiple_fallback_options(self):
        repo = self.repo({"a.txt": "v0\n"})
        stop_received = self.work / "pi-rpc-stop-received"
        exit_gate = self.work / "pi-rpc-exit-gate"
        child_argv_file = self.work / "pi-rpc-child-argv.json"

        self._install_fake_pi_rpc(
            f"""
import json, os, sys, time
from pathlib import Path

args = sys.argv[1:]
if args == ["--version"]:
    print("rpi 0.20.0 (fake-rpc)")
    sys.exit(0)
if len(args) >= 2 and args[1] == "--list-models":
    print("provider model context max-out thinking images")
    print("p m 1M 1K yes yes")
    print("p scout-m 64k 1K no no")
    print("p review-m 1M 1K yes yes")
    print("p write-m 1M 1K yes yes")
    sys.exit(0)

is_probe = "--no-session" in args
stop_received = Path({str(stop_received)!r})
exit_gate = Path({str(exit_gate)!r})
child_argv_file = Path({str(child_argv_file)!r})

sdir, sid = "", ""
for i, a in enumerate(args):
    if a == "--session-dir" and i + 1 < len(args):
        sdir = args[i + 1]
    elif a == "--session-id" and i + 1 < len(args):
        sid = args[i + 1]
if sdir and not is_probe:
    Path(sdir).mkdir(parents=True, exist_ok=True)
    (Path(sdir) / f"t_{{sid or os.getpid()}}.jsonl").write_text("{{}}\\n", encoding="utf-8")

is_child = stop_received.exists() and not is_probe

print(json.dumps({{
    "type": "ready",
    "protocol": "rpi",
    "supportedCommands": ["prompt", "queue_message", "get_queue", "clear_queue", "stop"],
}}), flush=True)

for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    cmd = json.loads(line)
    ctype = cmd.get("type")
    cid = cmd.get("id")
    if ctype == "prompt":
        print(json.dumps({{"type": "response", "command": "prompt", "success": True, "id": cid, "data": {{"accepted": True}}}}), flush=True)
        if is_child:
            child_argv_file.write_text(json.dumps({{"args": args, "prompt": cmd.get("message", "")}}, ensure_ascii=False), encoding="utf-8")
            ans = "子轮兜底答复"
        else:
            ans = "父轮答复"
        print(json.dumps({{
            "type": "message_end",
            "message": {{
                "role": "assistant",
                "stopReason": "stop",
                "provider": "p",
                "model": "review-m",
                "usage": {{"input": 10, "output": 5, "cacheRead": 0}},
                "content": [{{"type": "text", "text": ans}}],
            }},
        }}), flush=True)
        print(json.dumps({{"type": "agent_settled", "outcome": "completed"}}), flush=True)
    elif ctype == "queue_message":
        q = "steer" if cmd.get("deliverAs") == "steer" else "followUp"
        print(json.dumps({{"type": "response", "command": "queue_message", "success": True, "id": cid, "data": {{"accepted": True, "entryId": f"e-{{cid}}", "queue": q}}}}), flush=True)
    elif ctype == "get_queue":
        print(json.dumps({{"type": "response", "command": "get_queue", "success": True, "id": cid, "data": {{"items": [], "count": 0}}}}), flush=True)
    elif ctype == "clear_queue":
        print(json.dumps({{"type": "response", "command": "clear_queue", "success": True, "id": cid, "data": {{"cleared": 0}}}}), flush=True)
    elif ctype == "stop":
        print(json.dumps({{"type": "response", "command": "stop", "success": True, "id": cid, "data": {{"stopped": True}}}}), flush=True)
        if not is_probe and not is_child:
            stop_received.touch()
            while not exit_gate.exists():
                time.sleep(0.02)
        sys.exit(0)
"""
        )

        started = self.outcome(
            self.cli("start", "--read-only", "--workdir", repo, "--name", "rpi-race", "原始任务")
        )
        run_id = started["run"]
        run_dir = Path(started["dir"])
        self.addCleanup(lambda: (exit_gate.touch(), self.cli("stop", run_id)))

        self.until(stop_received.exists, "pi RPC received stop after closing in-run channel")

        # First late fallback carries --fresh; second does not. Merged child must keep --fresh!
        r1 = self.cli("reply", "--fresh", run_id, "收尾后第一条", human=True)
        self.assertEqual(r1.returncode, 0, r1.stdout + r1.stderr)
        self.assertIn("已退回结束后同会话开新 round", r1.stdout)
        r2 = self.cli("reply", run_id, "收尾后第二条", human=True)
        self.assertEqual(r2.returncode, 0, r2.stdout + r2.stderr)
        self.assertIn("第 2 条追加", r2.stdout)

        exit_gate.touch()
        receipt = run_dir / "reply-delivered.json"
        self.until(receipt.exists, "fallback child launched")
        child_dir = Path(json.loads(receipt.read_text(encoding="utf-8"))["run"])
        waited_child = self.cli("wait", "--all", str(child_dir))
        self.assertEqual(waited_child.returncode, 0, waited_child.stdout + waited_child.stderr)

        child_rec = json.loads(child_argv_file.read_text(encoding="utf-8"))
        # --fresh means child uses --session-id instead of --fork!
        self.assertNotIn("--fork", child_rec["args"])
        self.assertIn("--session-id", child_rec["args"])
        self.assertIn("1. 收尾后第一条", child_rec["prompt"])
        self.assertIn("2. 收尾后第二条", child_rec["prompt"])

    def test_pi_rpc_late_steer_unconsumed_at_settled_runs_next_prompt_round(self):
        repo = self.repo({"a.txt": "v0\n"})
        turn1_active = self.work / "pi-rpc-late-turn1-active"
        rpc_log = self.work / "pi-rpc-late-received.jsonl"
        accept_log = self.work / "pi-rpc-late-accept.log"

        self._install_fake_pi_rpc(
            f"""
import json, os, sys
from pathlib import Path

args = sys.argv[1:]
if args == ["--version"]:
    print("rpi 0.20.0 (fake-rpc)")
    sys.exit(0)
if len(args) >= 2 and args[1] == "--list-models":
    print("provider model context max-out thinking images")
    print("p m 1M 1K yes yes")
    print("p scout-m 64k 1K no no")
    print("p review-m 1M 1K yes yes")
    print("p write-m 1M 1K yes yes")
    sys.exit(0)

is_probe = "--no-session" in args
turn1_active = Path({str(turn1_active)!r})
rpc_log = Path({str(rpc_log)!r})

sdir, sid = "", ""
for i, a in enumerate(args):
    if a == "--session-dir" and i + 1 < len(args):
        sdir = args[i + 1]
    elif a == "--session-id" and i + 1 < len(args):
        sid = args[i + 1]
if sdir and not is_probe:
    Path(sdir).mkdir(parents=True, exist_ok=True)
    (Path(sdir) / f"t_{{sid or os.getpid()}}.jsonl").write_text("{{}}\\n", encoding="utf-8")

print(json.dumps({{
    "type": "ready",
    "protocol": "rpi",
    "supportedCommands": ["prompt", "queue_message", "get_queue", "clear_queue", "stop"],
}}), flush=True)

prompt_count = 0
queued_items = []
for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    cmd = json.loads(line)
    ctype = cmd.get("type")
    cid = cmd.get("id")
    if not is_probe:
        with rpc_log.open("a", encoding="utf-8") as f:
            f.write(json.dumps(cmd, ensure_ascii=False) + "\\n")
    if ctype == "prompt":
        prompt_count += 1
        print(json.dumps({{"type": "response", "command": "prompt", "success": True, "id": cid, "data": {{"accepted": True}}}}), flush=True)
        if prompt_count == 1:
            turn1_active.touch()
        else:
            # Round 2 runs after get_queue + clear_queue recovered the unconsumed late steer.
            Path("a.txt").write_text("round2-recovered\\n", encoding="utf-8")
            print(json.dumps({{
                "type": "message_end",
                "message": {{
                    "role": "assistant",
                    "stopReason": "stop",
                    "provider": "p",
                    "model": "write-m",
                    "usage": {{"input": 120, "output": 50, "cacheRead": 30}},
                    "content": [{{"type": "text", "text": "第二轮补答（已覆盖迟到steer）"}}],
                }},
            }}), flush=True)
            print(json.dumps({{"type": "agent_settled", "outcome": "completed"}}), flush=True)
    elif ctype == "queue_message":
        q = "steer" if cmd.get("deliverAs") == "steer" else "followUp"
        eid = f"entry-{{cid}}"
        if not is_probe:
            # Simulate the race: turn 1 finishes and settles right as `queue_message` arrives,
            # leaving the steered message accepted in `queued` state WITHOUT starting a new turn.
            Path("a.txt").write_text("round1-before-steer\\n", encoding="utf-8")
            print(json.dumps({{
                "type": "message_end",
                "message": {{
                    "role": "assistant",
                    "stopReason": "stop",
                    "provider": "p",
                    "model": "write-m",
                    "usage": {{"input": 80, "output": 20, "cacheRead": 0}},
                    "content": [{{"type": "text", "text": "第一轮答复（未消费迟到steer）"}}],
                }},
            }}), flush=True)
            print(json.dumps({{"type": "agent_settled", "outcome": "completed"}}), flush=True)
            queued_items.append({{
                "entryId": eid,
                "queue": q,
                "status": "queued",
                "text": cmd.get("message", ""),
            }})
        print(json.dumps({{
            "type": "response",
            "command": "queue_message",
            "success": True,
            "id": cid,
            "data": {{"accepted": True, "entryId": eid, "queue": q}},
        }}), flush=True)
    elif ctype == "get_queue":
        print(json.dumps({{
            "type": "response",
            "command": "get_queue",
            "success": True,
            "id": cid,
            "data": {{"items": queued_items, "count": len(queued_items)}},
        }}), flush=True)
    elif ctype == "clear_queue":
        cleared = len(queued_items)
        queued_items = []
        print(json.dumps({{
            "type": "response",
            "command": "clear_queue",
            "success": True,
            "id": cid,
            "data": {{"cleared": cleared}},
        }}), flush=True)
    elif ctype == "stop":
        print(json.dumps({{"type": "response", "command": "stop", "success": True, "id": cid, "data": {{"stopped": True}}}}), flush=True)
        sys.exit(0)
"""
        )

        accept_cmd = f"echo accept >> {shlex.quote(str(accept_log))}"
        started = self.outcome(
            self.cli(
                "start",
                "--worktree",
                "--workdir",
                repo,
                "--name",
                "rpi-late-steer",
                "--accept",
                accept_cmd,
                "原始任务",
            )
        )
        run_id = started["run"]
        run_dir = Path(started["dir"])
        self.addCleanup(lambda: self.cli("stop", run_id))

        self.until(turn1_active.exists, "pi RPC turn 1 active")
        rep = self.cli("reply", run_id, "迟到的 steer 问题", human=True)
        self.assertEqual(rep.returncode, 0, rep.stdout + rep.stderr)
        self.assertIn("已追加到 rpi-late-steer", rep.stdout)

        waited = self.cli("wait", "--all", run_id)
        self.assertEqual(waited.returncode, 0, waited.stdout + waited.stderr)
        self.assertEqual(self.outcome(waited)["state"], "delivered")

        cmds = [json.loads(x) for x in rpc_log.read_text(encoding="utf-8").splitlines()]
        ctypes = [c.get("type") for c in cmds]
        self.assertIn("queue_message", ctypes)
        self.assertIn("get_queue", ctypes)
        self.assertIn("clear_queue", ctypes)
        prompts = [c for c in cmds if c.get("type") == "prompt"]
        self.assertEqual(len(prompts), 2, f"expected 2 prompts (initial + recovered steer), got {cmds}")
        self.assertTrue(prompts[1]["message"].startswith(APPEND_HEADER))
        self.assertIn("1. 迟到的 steer 问题", prompts[1]["message"])
        self.assertEqual(ctypes[-1], "stop")

        # Final answer is round 2; round 1 preserved via `result --round 1`.
        self.assertEqual(
            (run_dir / "result.md").read_text(encoding="utf-8").strip(),
            "第二轮补答（已覆盖迟到steer）",
        )
        self.assertEqual(
            self.cli("result", run_id, "--round", "1", human=True).stdout.strip(),
            "第一轮答复（未消费迟到steer）",
        )
        self.assertEqual(
            self.cli("result", run_id, "--round", "2", human=True).stdout.strip(),
            "第二轮补答（已覆盖迟到steer）",
        )
        self.assertEqual(accept_log.read_text(encoding="utf-8").splitlines(), ["accept"])
        self.assertFalse((run_dir / "pending-reply.json").exists())
        self.assertIn("已追加 1 条，待投递 0 条", self.cli("status", run_id, human=True).stdout)
        self.assertIn("已追加 1 条，待投递 0 条", self.cli("card", run_id, human=True).stdout)


if __name__ == "__main__":
    unittest.main()
