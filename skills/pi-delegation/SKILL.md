---
name: pi-delegation
description: 把可独立验收的原子任务外包给同事 Agent 在后台完成，只收结果：Pi（如 Gemini，便宜快速）或 Codex（GPT）写代码或测试、只读评审、第二意见、并行调研。脚本代为运行验收命令并给出一行结论，中间过程不进入主控上下文；采纳与否由主控决定。Use to delegate tasks, run parallel reviews or get a second opinion via Pi or Codex, judged by results.
license: MIT
compatibility: Linux；需要 python3（3.9+，仅标准库），以及所选同事的 CLI：pi 或 codex。
metadata:
  version: "3.2.0"
  exclude-agents: pi
---

# Pi Delegation（按结果委派）

你是主控：把边界清楚的任务交给同事，自己继续推进，回来只看结果。同事交回的是草稿和执行结果，**是否采纳、如何整合始终由你决定**。

`scripts/pi_delegate.py` 负责启动同事、等待结束、在工作目录里运行你给的验收命令，最后输出一行 JSON 结论和同事的答复；过程细节留在 run 目录。

## 选择同事

| `--agent` | 适合 | 特点 |
|---|---|---|
| `pi`（默认） | 起草、机械改动、并行调研、多路第二意见 | 调用 Pi 当前默认模型（通常是 Gemini Flash），便宜快速；只读模式会移除写工具 |
| `codex` | 更难的写入任务、需要严格独立视角的审查 | 调用 Codex CLI（GPT），使用其原生工具链；只读由说明加事后核对保证（见下） |

同事不是你本人：不知道你的会话上下文。任务说明要自足，写清目标、边界和怎么算完成。

## 适合外包的任务

- **原子化**：几句话能交代清楚，不依赖只有你知道的隐含背景。
- **可验收**：写入任务能用一条命令判断成败（测试、lint、脚本退出码）；只读任务的交付物就是答复。
- **可并行**：互不依赖的任务同时放出；写入任务各用自己的 `--workdir`，或各自的 git worktree。

改动只有几行、或下一步决策强依赖尚未整理的上下文时，直接自己做。

## 用法

```bash
D=<本技能目录>/scripts/pi_delegate.py

# 写入：给验收命令；脚本在同事结束后于 workdir 执行，退出码 0 即交付
$D run --agent codex --name fix-parser --workdir packages/parser --accept "npm test" --prompt-file - <<'EOF'
修复 parse() 对空输入抛异常的问题，保持现有接口不变。
EOF

# 只读：并行放出，统一收取
$D start --read-only --name review-api "审查 src/api/ 的错误处理，列出真实缺陷"
$D start --read-only --agent codex --name review-db "审查 src/db/ 的事务边界"
$D wait --all
```

- **等待方式按你所在的环境选**：能在后台运行命令并在结束时收到通知的环境，直接后台运行 `run`，无需轮询；单次工具调用有时长上限的环境，给 `run`/`wait` 加 `--max 4m`，到时返回 75，稍后再 `wait`。`--max` 只结束本次等待，同事仍在后台运行；不再需要时用 `stop`。
- 有 `--accept` 时，脚本会把验收命令作为“完成标准”附在任务说明末尾；需要盲验时加 `--hide-accept`。`prompt.md` 保存同事实际收到的全文。
- 多行任务说明用 `--prompt-file -` 加 heredoc。建议总加 `--name`。
- 写入任务想和自己的工作隔离时，先 `git worktree add` 一个分支给同事，交付后复核再合并。

## 读结论并把关

每个结束的任务输出一行 JSON，之后是答复（超过 6000 字只显示末尾，`--full` 或 `result` 看全文）：

| state | 含义 | 你要做的 |
|---|---|---|
| `delivered` | 已答复且验收命令通过 | 复核 `files` 的 diff 再采纳 |
| `answered` | 已答复，未设验收 | 按内容判断，关键结论去代码里核实 |
| `rejected` | 验收命令失败，`accept.tail` 有输出末尾 | 看原因，修补或自己接手 |
| `malformed` | 答复为空或是一段泄漏的工具调用，已自动重跑仍如此 | 换同事或自己做 |
| `failed` | 同事出错，或只读任务改了文件（`readOnlyViolation` 列出） | 看 `error`；改动需手动还原 |
| `timeout` / `killed` / `crashed` / `stopped` | 超时、进程异常或被终止 | 任务太大就拆小，否则自己接手 |

`delivered` 只说明验收命令通过，不代表改法合适：采纳前看 diff。`files` 合并同事的编辑记录与 workdir 的 git 变化（不含验收命令自身产生的文件）。退出码：`0` delivered/answered，`1` 其他结局，`2` 用法错误或被拒绝，`75` 仍在运行。

## 委派层级

- **你（主控）** 可以委派给 `pi` 和 `codex`。
- **Codex 同事** 可以把子任务再交给 `pi`，不能交给 `codex`；它委派的写入任务可以在它自己的 workdir 内进行。
- **Pi 同事** 不能再委派；本技能也不会安装到 Pi 扫描的目录。

脚本通过 `PI_DELEGATE_AGENT` / `PI_DELEGATE_PARENT_RUN` 环境变量识别调用者，违规时以退出码 2 拒绝。

## 命令与选项

| 命令 | 作用 |
|---|---|
| `start [选项] [任务说明]` | 后台启动，立即返回一行状态 |
| `run [选项] [--max <时长>]` | 启动并等到结论 |
| `wait [<run>...\|--all] [--max <时长>] [--no-result] [--full] [--progress]` | 等待并输出结论；`--all` 含仍在运行和尚未读取结果的任务 |
| `status [<run>...]` | 每个任务一行 JSON；运行中带 `last` 与 `idleSeconds` |
| `result [<run>] [--path]` | 输出完整答复 |
| `stop <run>...` | 终止任务及其进程组 |
| `clean <run>...\|--finished [--force]` | 删除已结束的任务；`--finished` 默认保留结果未读取的 |

启动选项：`--agent pi|codex`、`--accept <命令>`、`--hide-accept`、`--accept-timeout`（默认 10m）、`--read-only`、`--workdir`、`--timeout`（每次尝试，pi 默认 15m，codex 默认 30m）、`--retries`（答复畸形时重跑次数，默认 1）、`--model`/`--thinking`/`--provider`（不指定时用各 CLI 自己的默认设置；Codex 的 `--thinking` 对应推理强度）、`--allow-parallel-writes`。`<run>` 可以是完整 id、唯一片段、`last` 或 run 目录。

**只读的保证方式**：Pi 的只读模式移除写工具。Codex 在部分系统上无法启用沙箱（例如 Ubuntu 24.04 默认的 AppArmor 限制会让 bwrap 失败），所以脚本在任务说明里写明只读边界，结束后用 git 核对工作目录，有改动即判为 `failed`。非 git 目录无法核对。

## 边界

1. **写入互斥**：同一运行记录根目录内，同一 workdir 同时只允许一个写入任务（委派者自己的 run 除外）；并发启动会排队检查。跨项目指向同一 workdir 时统一设置 `PI_DELEGATE_RUNS`。确需并行时各用不同 `--workdir`，或加 `--allow-parallel-writes` 并确保文件不重叠。
2. **环境**：缺少 `pi` 时脚本给出随仓库附带的 pi-kit 安装命令（`bootstrap.sh --with-pi` 同样可装）；缺少 `codex` 时提示安装并登录 Codex CLI。经非交互 SSH 调用时注意这些 CLI 往往只在登录 shell 的 `PATH` 里。

run 目录位置、文件与清理策略见 [references/output-and-files.md](references/output-and-files.md)。
