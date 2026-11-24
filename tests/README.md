# 编写不依赖计时的测试

本仓测试以黑盒 CLI 子进程方式在多核并行与高负载宿主下运行，**严禁用墙钟时间代替确定性同步**。

## 四条铁律

1. **异步阻塞用门控（gate），不用 `sleep`**：让被测子进程在显式 gate 文件上等待并写出就绪标记，测试端确认就绪后再继续、最后 touch gate 放行，避免与启动/调度抖动竞速。
2. **非阻塞 / 隔离判不变量 + 探针，不用毫秒阈值**：测无锁读时用 gate 长期持有 `.state.lock` 并开启 `DELEGATE_MEASURE_STATE_LOCK=1`（或 Python `sys.addaudithook`），断言零 `STATE_LOCK_ATTEMPT` 且全部读完后持锁进程仍在位（`holder.poll() is None`）；测并发隔离断言被门控的 A 仍未退出而 B 已完成。
3. **等待状态用 `wait_until` 家族，不写裸 `sleep` 或一次性断言**：异步文件/事件/进程退出统一用单调时钟轮询帮手，严禁手写 `while time.time() < deadline` 或未防瞬态异常的 `until(...read_text())`。
4. **改异步测试前先压测**：本地连跑 5 遍确认零偶发失败，再跑完整门禁：
   ```sh
   python3 scripts/run-tests.py -k <用例名> --repeat 5
   scripts/verify.sh
   ```

## 共享帮手与探针位置

| 能力 | 位置 | 用途 |
|---|---|---|
| `wait_until` / `self.until` | `tests/test_delegate.py` (`wait_until`) | `time.monotonic()` 轮询，默认忽略 `FileNotFoundError`/`JSONDecodeError`/`ValueError` |
| `wait_text` / `wait_json` / `wait_gone` | `tests/test_delegate.py` | 容错等待文本、JSON 谓词、进程退出（含僵尸态 `Z`） |
| `fake_pi(..., gate=..., ready_file=...)` | `tests/test_delegate.py` (`DelegateTests.fake_pi`) | 原子替换 `bin/pi`（防 `O_TRUNC` 踩踏），内置就绪与门控循环 |
| `start_gated(...)` / `hold_lane_gated(...)` | `tests/test_delegate.py` | 启动门控异步 run / 占位重活队列，自动等就绪并在 cleanup 释放 |
| `settle_supervisors(runs_dir)` | `tests/test_delegate.py` | 确定性等待残留 supervisor 与后台锁退出 |
| `DELEGATE_MEASURE_STATE_LOCK=1` | `crates/delegate/src/locks.rs` | 在 stderr 输出 `STATE_LOCK_ATTEMPT` / `STATE_LOCK_HOLD_MS` 验证无锁读 |
| `DELEGATE_TEST_PREMERGE_GATE=<path>` | `crates/delegate/src/premerge.rs` | 仅 debug 构建生效，把 `_merge-worker` 确定性停在 `running` 态 |
| `_stress_serial = True` / `--repeat N` | `scripts/run-tests.py` | 含 `ThreadPoolExecutor` 或打标的压测自动拆出 8-worker 池串行跑 |

## 静态门禁（`scripts/check.sh`）

`scripts/check.sh` 的 AST 检查扫描 `tests/test_*.py` 与 `crates/*/tests/test_*.py`，拦截：
- 测试函数内裸 `time.sleep(...)`（确有必要如测超时窗口，行尾加 `# sleep-ok: <原因>`）
- 手写 `while ... time.time()` 轮询循环（豁免 `# poll-ok: <原因>`）
- `< 1.0s` 或 `< 1000ms` 的紧墙钟断言（豁免 `# wall-clock-ok: <原因>`）
- `until(...read_text())` 与测试内裸 `os.kill(pid, 0)`（改用 `wait_text`/`wait_json`/`wait_gone`，豁免 `# until-ok:` / `# kill-ok:`）
