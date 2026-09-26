---
name: delegate
description: 把可独立验收的任务交给同事 Agent 在后台并行完成，你只收结果并把关。遇到这些情况时主动使用，不必等用户开口：要读很多文件但只需结论；改动前想要第二意见或独立审查；有两个以上互不依赖、能用命令验收的子任务；需要有人看截图或设计图给意见。同事：Pi（Gemini：说人话、便宜快速、够用，后端逻辑偏弱）与 Codex（GPT：谨慎、逻辑强、较慢较贵），两者都能读图。脚本代跑验收命令，只给一行结论，过程不进主控上下文；整机并发有上限。Use to delegate or parallelize work proactively, and to get a second opinion, code review or image review from Gemini (Pi) or GPT (Codex), judged by results.
license: MIT
compatibility: Linux；需要 python3（3.9+，仅标准库），以及所选同事的 CLI：pi 或 codex。
metadata:
  version: "4.0.0"
  exclude-agents: pi
---

# Delegate（按结果委派）

你是主控：把边界清楚的任务交给同事，自己继续推进，回来只看结果。同事交回的是草稿和执行结果，**是否采纳、如何整合始终由你决定**。

`scripts/delegate.py` 负责启动同事、等待结束、在工作目录里运行你给的验收命令，最后输出一行 JSON 结论和同事的答复；过程细节留在 run 目录。

## 选择同事

| `--agent` | 性格 | 交给它 | 别交给它 |
|---|---|---|---|
| `pi`（默认，Gemini） | 说人话、便宜快速、够用；后端逻辑偏弱 | 并行调研与总结、机械改动、前端与文案草稿、看截图或 UI 给意见、多路第二意见 | 并发、事务、数据迁移、协议状态机这类要严密推理的后端逻辑 |
| `codex`（GPT） | 行为谨慎、逻辑能力强；较慢较贵 | 后端逻辑与疑难 bug、迁移和并发审查、需要严格独立视角的评审、Pi 做不好的写入任务 | 只图省事的批量杂活（交给 Pi 更划算） |

两者都能读图：`--image <路径>` 可重复。Pi 失败一次通常不值得重跑同一任务：换 Codex 或自己接手。同事不知道你的会话上下文：任务说明要自足，写清目标、边界和怎么算完成。

## 什么时候委派

- **原子化**：几句话能交代清楚，不依赖只有你知道的隐含背景。
- **可验收**：写入任务能用一条命令判断成败（测试、lint、脚本退出码）；只读任务的交付物就是答复。
- **可并行**：互不依赖的任务同时放出；写入任务各用自己的 `--workdir`，或各自的 git worktree。

改动只有几行、或下一步决策强依赖尚未整理的上下文时，直接自己做。

## 用法

```bash
D=<本技能目录>/scripts/delegate.py

# 写入：给验收命令；脚本在同事结束后于 workdir 执行，退出码 0 即交付
$D run --agent codex --name fix-lock --workdir apps/api --accept "go test ./..." --prompt-file - <<'EOF'
修复任务队列在并发取消时重复释放锁的问题，保持现有接口不变。
EOF

# 只读：并行放出，统一收取
$D start --read-only --name ui-review --image shot.png "看这张截图，列出小屏布局最影响使用的三处问题"
$D start --read-only --agent codex --name review-db "审查 src/db/ 的事务边界，只列真实缺陷"
$D wait --all
```

- **等待方式按你所在的环境选**：能在后台运行命令并在结束时收到通知的环境，直接后台运行 `run`，无需轮询；单次工具调用有时长上限的环境，给 `run`/`wait` 加 `--max 4m`，到时返回 75，稍后再 `wait`。`--max` 只结束本次等待，同事仍在后台运行；不再需要时用 `stop`。
- 有 `--accept` 时，脚本会把验收命令作为“完成标准”附在任务说明末尾；需要盲验时加 `--hide-accept`。`prompt.md` 保存同事实际收到的全文。
- 多行任务说明用 `--prompt-file -` 加 heredoc。建议总加 `--name`。

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

`delivered` 只说明验收命令通过，不代表改法合适：采纳前看 diff。退出码：`0` delivered/answered，`1` 其他结局，`2` 用法错误或被拒绝（含并发已满、违反层级），`75` 仍在运行。

## 边界

1. **并发上限**：按整台机器计数（跨项目，含同事再委派的子任务），默认同时最多 6 个，其中 Codex 最多 3 个；超出时拒绝并列出正在运行的任务。先 `wait` 收一批结果再派。上限由用户通过 `DELEGATE_MAX_ACTIVE` / `DELEGATE_MAX_CODEX` 设定（`0` 为不限），同事不要自行调整；委派记录写在 git 忽略的 `.local/run/`，不算只读任务的改动。
2. **委派层级**：主控可委派给 `pi` 和 `codex`；Codex 可把子任务再交给 `pi`，不能交给 `codex`；Pi 不能再委派（本技能也不会安装到 Pi 扫描的目录）。
3. **权限与只读**：Codex 固定以 `--dangerously-bypass-approvals-and-sandbox`（full access）启动，不依赖各机器 `~/.codex/config.toml`；写入靠 git 回退兜底，采纳前照常看 diff。Pi 的只读模式移除写工具；Codex 的只读由任务说明写明边界，结束后用 git 核对工作目录，有改动即判 `failed`。非 git 目录无法核对，这时只读任务优先交给 Pi。
4. **写入互斥**：同一 workdir 同时只允许一个写入任务（委派者自己的 run 除外）。确需并行时各用不同 `--workdir`，或加 `--allow-parallel-writes` 并确保文件不重叠。
5. **环境**：缺少 `pi` 时脚本给出随仓库附带的 pi-kit 安装命令；缺少 `codex` 时提示安装并登录。经非交互 SSH 调用时这些 CLI 往往只在登录 shell 的 `PATH` 里。

命令（`start`/`run`/`wait`/`status`/`result`/`stop`/`clean`）与全部选项见 `$D --help`；run 目录、文件、环境变量与清理策略见 [references/output-and-files.md](references/output-and-files.md)。
