---
name: pi-delegation
description: 通过 Pi CLI 在后台并行调度外部模型。当有可独立验收的编码子任务想交出去、需要只读代码评审、想要第二意见或同时问多个问题时使用；提供 start/wait/status/result/stop 异步管理、写入互斥与完整答复留存。Use to delegate tasks, run parallel reviews or get a second opinion via Pi.
license: MIT
compatibility: Linux；需要 pi、jq、setsid、GNU timeout。
metadata:
  version: "2.0.0"
  exclude-agents: pi
---

# Pi Delegation (Pi 后台协作助手)

`scripts/pi-delegate.sh` 在后台启动 Pi，每次运行对应一个 run 目录；主控 Agent 通过多次短时 `wait` 跟进，不受自身工具的单次时长上限影响。Pi 的模型通常便宜且快，适合同时放出多个：写代码、做评审、问意见。

## 何时委派

- **值得委派**：目标能独立验收（有明确的文件或测试结果）、背景几句话能交代清楚、多个任务可以并行，或者需要不同模型的独立视角。
- **自己做更快**：改动只有几行、需要大量本会话才有的隐含上下文、或者下一步决策强依赖结果细节。
- 委派出去的写入任务仍由主控负责验收，所以只委派你愿意逐行复核的范围。

## 快速上手

```bash
D=<本技能目录>/scripts/pi-delegate.sh

# 咨询或评审：一条命令等到结果（超过 --max 仍未完成则返回 75，改用 wait 继续）
$D run --read-only --name consult "对这个缓存方案给出三个主要风险：..."

# 并行：同时发出多个任务，统一收取
$D start --read-only --name review-api "审查 src/api/ 的错误处理..."
$D start --name impl --workdir packages/web --prompt-file - <<'EOF'   # 写入任务，多行提示词走 stdin
任务：...
边界：...
EOF
$D wait --all            # 退出码 75 表示仍有任务在跑，重复调用即可
```

多行提示词用 `--prompt-file -` 加 heredoc，脚本会保存为 run 目录里的 `prompt.md`，无需自己建临时文件。建议总是加 `--name`，便于辨认和引用。

## 命令

| 命令 | 作用 |
|---|---|
| `start [选项] [提示词]` | 后台启动，立即输出一行状态 JSON |
| `run [选项] [--max 240s]` | `start` 后接 `wait` |
| `wait [<run>...\|--all] [--max 240s]` | 输出新增进度，结束时输出状态和最终答复；`--max 0` 只查看不等待 |
| `status [<run>...]` | 每个任务一行 JSON，默认列出全部 |
| `result [<run>] [--path]` | 输出完整答复 |
| `stop <run>...` | 终止任务及其整个进程会话 |
| `clean <run>...\|--finished [--force]` | 删除已结束的 run 目录；`--finished` 会保留结果未报告过的任务 |

`<run>` 可以是完整 id、唯一片段、`last` 或 run 目录路径。启动选项：`--read-only`（只开放 read/grep/find/ls，不是系统沙盒）、`--provider`/`--model`/`--thinking`、`--timeout`（默认 15m）、`--workdir`、`--allow-parallel-writes`。退出码：`0` 成功，`1` 失败或被停止，`2` 用法错误或被拦截，`75` 仍在运行。

状态字段、进度行含义、run 目录位置与清理策略见 [references/output-and-files.md](references/output-and-files.md)。

## 协作要点

1. **写入互斥**：同一工作目录同时只允许一个写入任务，因为两个 Agent 同时改同一棵树几乎必然冲突。确需并行写入时给每个任务不同的 `--workdir`，或加 `--allow-parallel-writes` 并确保文件不重叠。
2. **主控复核**：`ok` 只表示 Pi 正常结束并给出了答复，不代表改对了。写入任务结束后用 `git diff` 和关键验证命令自己确认。
3. **失败处理**：`timeout`、`stopped`、`crashed` 时先看已生成的文件再决定是否重跑；反复超时说明任务太大，按可验收的文件或功能拆分。
4. **不可嵌套**：被委派的 Pi 里调用本脚本会被拒绝；本技能也不会安装到 Pi 会扫描的技能目录。
5. **收尾**：结果采纳后执行 `clean --finished`；需要留档的结论先整理进仓库文档。

## 委派提示词模板

```text
先读取并遵守 AGENTS.md 及与本任务直接相关的设计文档。
任务：<具体、可独立验收的目标>。
边界：<明确非目标与不应改动的文件>。
成功标准：<功能行为与输出指标>。
验证命令：<一两个必要的测试或 lint 命令；失败时定位后再重试>。
交付形式：只报告产物位置、实际通过的验证和剩余风险；不复述指令或执行过程。
```

咨询类问题直接写清背景、约束和期望的回答形式即可；需要看代码时加 `--read-only` 并指明文件。
