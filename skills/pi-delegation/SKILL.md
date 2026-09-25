---
name: pi-delegation
description: 把可独立验收的原子任务外包给 Pi 在后台并行完成，只收结果：写代码或测试、只读评审、第二意见、同时问多个问题。脚本代为运行验收命令并给出一行结论，中间过程不进入主控上下文。Use to delegate tasks, run parallel reviews or get a second opinion via Pi, judged by results.
license: MIT
compatibility: Linux；需要 pi 与 python3（3.9+，仅标准库）。
metadata:
  version: "3.0.0"
  exclude-agents: pi
---

# Pi Delegation (Pi 后台协作助手)

像交代给同事一样委派：说清要什么、怎么算做完，然后去忙别的，回来只看结果。`scripts/pi_delegate.py` 在后台运行 Pi，结束后由脚本自己执行你给的验收命令，输出一行结论和 Pi 的答复；中间每一步读了什么、跑了什么都留在 run 目录，不进入你的上下文。

它的价值在于省主控上下文、可以并行，以及得到另一个模型的独立视角。

## 适合外包的任务

- **原子化**：几句话能交代清楚，不依赖只有本会话才知道的隐含背景。
- **可验收**：写入任务能用一条命令判断成败（测试、lint、脚本退出码）；只读任务的交付物就是答复本身。
- **可并行**：互不依赖的调研、评审或改动可以同时放出，写入任务各用自己的 `--workdir`。

改动只有几行、或下一步决策强依赖中间细节时，自己做更快。

## 用法

```bash
D=<本技能目录>/scripts/pi_delegate.py

# 写入任务：给出验收命令，脚本在 Pi 结束后于 workdir 中执行，退出码 0 即交付
$D run --name fix-parser --workdir packages/parser --accept "npm test" --prompt-file - <<'EOF'
修复 parse() 对空输入抛异常的问题，保持现有接口不变。
EOF

# 只读任务：并行放出，统一收取
$D start --read-only --name review-api "审查 src/api/ 的错误处理，列出真实缺陷"
$D start --read-only --name review-db  "审查 src/db/ 的事务边界"
$D wait --all
```

- `run` 一直等到结束。在支持后台命令并会通知完成的环境（如 Claude Code）里，用后台方式启动 `run`，完成时直接收到结论，无需轮询。
- 工具有单次时长上限的环境，给 `run`/`wait` 加 `--max 4m`；到时仍在运行就返回 75，稍后再 `wait`。
- 多行提示词用 `--prompt-file -` 加 heredoc，脚本保存为 run 目录里的 `prompt.md`。建议总加 `--name`。

## 读结论

每个结束的任务输出一行 JSON，之后是答复（超过 6000 字只显示末尾，`--full` 或 `result` 看全文）：

| state | 含义 | 你要做的 |
|---|---|---|
| `delivered` | Pi 已答复且验收命令通过 | 看一眼 `files`，采纳 |
| `answered` | Pi 已答复，未设验收 | 按答复内容判断 |
| `rejected` | 验收命令失败，`accept.tail` 有输出末尾 | 看失败原因，决定修补或自己接手 |
| `malformed` | 答复为空或是一段泄漏的工具调用，已自动重跑仍如此 | 换个模型或自己做 |
| `failed` / `timeout` / `killed` / `crashed` | Pi 自身出错、超时或进程异常，`error` 有摘要 | 任务太大就拆小；否则自己接手 |
| `stopped` | 被 `stop` 终止 | — |

`files` 同时来自 Pi 的编辑记录和 workdir 的 git 状态变化（不含验收命令自身产生的文件）。退出码：`0` delivered/answered，`1` 其他已结束状态，`2` 用法错误或被拒绝，`75` 仍在运行。

## 命令与选项

| 命令 | 作用 |
|---|---|
| `start [选项] [提示词]` | 后台启动，立即返回一行状态 |
| `run [选项] [--max <时长>]` | 启动并等到结论 |
| `wait [<run>...\|--all] [--max <时长>] [--no-result] [--full] [--progress]` | 等待并输出结论；`--all` 含仍在运行和尚未读取结果的任务 |
| `status [<run>...]` | 每个任务一行 JSON；运行中带 `last` 与 `idleSeconds` |
| `result [<run>] [--path]` | 输出完整答复 |
| `stop <run>...` | 终止任务及其 Pi 进程组 |
| `clean <run>...\|--finished [--force]` | 删除已结束的任务；`--finished` 默认保留结果未读取的 |

启动选项：`--accept <命令>`、`--accept-timeout`（默认 10m）、`--read-only`（只开放 read/grep/find/ls，不是系统沙盒）、`--workdir`、`--timeout`（每次 Pi 运行，默认 15m）、`--retries`（答复畸形时的重跑次数，默认 1）、`--provider`/`--model`/`--thinking`（未指定时用 Pi 的默认设置）、`--allow-parallel-writes`。`<run>` 可以是完整 id、唯一片段、`last` 或 run 目录。`--progress` 会额外打印写入、错误与重试，默认不显示过程。

run 目录位置、文件与清理策略见 [references/output-and-files.md](references/output-and-files.md)。

## 边界

1. **写入互斥**：同一 workdir 同时只允许一个写入任务；确需并行时各用不同 `--workdir`，或加 `--allow-parallel-writes` 并确保文件不重叠。
2. **不可嵌套**：被委派的 Pi 里调用本脚本会被拒绝；本技能也不会安装到 Pi 扫描的技能目录。
3. **环境**：缺少 `pi` 时脚本给出随仓库附带的 pi-kit 安装命令（`bootstrap.sh --with-pi` 同样可装）。
