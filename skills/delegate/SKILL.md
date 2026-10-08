---
name: delegate
description: 把可独立验收的任务交给同事 Agent 在后台并行完成，你只收结果并把关。遇到这些情况时主动使用，不必等用户开口：要读很多文件但只需结论；改动前想要第二意见或独立审查；有两个以上互不依赖、能用命令验收的子任务；需要有人看截图或设计图给意见。同事分两档：便宜档 Pi（Gemini，读材料、摘要、文案、看图）与强档 Codex（GPT，写代码、严密审查），只读默认便宜档、写入默认强档，便宜档失败自动升档。脚本代跑验收命令，只给一行结论与下一步，过程不进主控上下文；整机与仓库容量、重检查有上限。Use to delegate or parallelize work proactively, and to get a second opinion, code review or image review from Gemini (Pi) or GPT (Codex), judged by results.
license: MIT
compatibility: Linux x86_64 或 aarch64；入口是安装时下载的静态二进制 bin/delegate，不需要 Python；需要所选同事的 CLI：pi 或 codex。
metadata:
  version: "5.25.1"
  binary: delegate
  exclude-agents: pi
---

# Delegate（按结果委派）

你是主控：把边界清楚的任务交给同事，自己继续推进，回来只看结果。**是否采纳、如何整合始终由你决定。** 同事不能再委派，所有结果都回到你这里。

## 三步用法

```bash
D=<本技能目录>/bin/delegate
$D start --read-only "审查 apps/api 的错误处理，只列真实缺陷，写明文件:行号"   # 1. 放出（可连放多个）
$D wait                                                                    # 2. 等：放后台，下一个结束就返回
#                                                                          # 3. 按结论行的“下一步”处理
```

- **放出**：`start` 回显任务名、同事与档位，以及本次自动生效的东西（仓库固定说明几行、默认 worktree / 保护路径 / 附加验收及其来源）。名字省略时取说明的第一个分句；之后用它指代整段对话。
- **等**：`wait` 放后台（Claude Code 的 Bash 用 `run_in_background: true`），不用 `status` 轮询。它交付下一个结束的任务就返回，末行写明本会话还剩几个、原样再跑哪条命令——照做即可，直到末行说没有在跑的任务。
- **处理**：每个任务一条短行（状态、改动、验收、下一步），随后是答复。只读任务去代码核实关键结论；写入任务看 `$D diff <name>`，满意后 `$D apply <name>`——它合入、清理该任务，并在末行说明验收是否仍有效、无效时要在主干重跑哪些命令。

## 场景速查

| 想要 | 命令 |
|---|---|
| 读材料、只要结论 | `$D start --read-only "…"`；要限制篇幅加 `--max-answer 800`（明显超长时自动让同事压缩一次） |
| 独立审查、第二意见 | `$D start --read-only --tier strong "…"`（多路并行时各起一个，按子系统拆开） |
| 看截图或设计图 | `$D start --read-only --image shot.png "…"` |
| 改代码，测试通过才算完 | `$D start --worktree --accept-also "make test" "…"`；不许同事碰的路径加 `--protect tests/` |
| 接着上一轮追问或返工 | `$D reply <name> "…"`（同一会话、同一 worktree）；开新会话加 `--fresh`；你之后改了代码想让它看到加 `--sync` |
| 先 A 后 B | `$D start --after <A> …`；审 A 的结果加 `--in <A> --read-only` |
| 自己跑重检查 | `$D lane make check`（与同事验收、证据共用队列） |
| 同时在几个仓库派了任务 | 后台 `$D wait --machine`，整机的都会收到 |
| 答复被截断 / 看改了什么 / 停掉 / 清理 | `$D result <name>` / `$D diff <name>` / `$D stop <name>` / `$D clean --finished` |

多行说明用 `--prompt-file -` 加 heredoc。更多示例与任务说明模板见 [references/recipes.md](references/recipes.md)。

## 让仓库替你记住

每次都要写的东西放进配置，不要手抄进说明：仓库根 `.delegate.json`（或用户级 `config.json`）的 `standing` 是自动附在每份任务说明末尾的固定规矩（可分 `all` / `write` / `readOnly`），`agentDeny` 拦下的命令及其替代提示会一并告诉同事；`defaults` 给 `worktree`、`protect`、`acceptAlso`、`timeout`、`evidence` 默认值，命令行显式选项覆盖，`--in-place` 退回原地写入。`resultChars`（默认 20000）是答复显示上限。字段与合并规则见 [references/configuration.md](references/configuration.md)。

## 选档位

| `--tier` | 默认给 | 适合 | 不适合 |
|---|---|---|---|
| `cheap`（Pi） | 只读任务 | 广度侦察（在代码里找事实、列清单）、界面与前端实现和审查、文案、看图；重要审查时作为另一路视角 | 需要严密判断与风险评估的逻辑（量化边界、并发、事务、迁移） |
| `strong`（Codex） | 写入任务 | 后端与协议代码、疑难 bug、严密审查与风险评估、核实便宜档的结论 | 界面视觉与交互打磨；只图省事的批量杂活 |

便宜档答复畸形、出错、超时或验收失败且未改文件时，自动强档重跑一次。`--agent pi|codex` 固定同事，不升档。

## 写好任务说明

同事不知道你的会话，说明需要自足：**目标**（要什么结果）、**边界**（能改哪、不能改哪）、**完成标准**（验收命令，或答复要包含什么）、**已知事实与决定**（别让它重新猜）。新增旁路能力（投影、索引、记忆、缓存、通知）时再写一句**失败语义**：哪些失败必须中止主路径，哪些只记诊断、稍后重试；不写时强档倾向于处处失败关闭。给模型或用户读的文字（提示词、文案）自己写好交给同事接线，不要让它代拟。

## 把关时记住

- **已交付**只说明验收通过，改法仍要看 `diff`；**已答复**没有验收，关键结论去代码核实；其余状态按短行的下一步处理。
- 同一仓库的写入任务串行 `apply`；合入一批后自己跑全量检查。同事只跑相关检查，被 `agentDeny` 拦下的全量检查归你。
- 写入任务加 `--worktree`（或配置 `defaults.worktree`）就在独立 worktree 里做，可以并行；原地写入同一目录同时只能有一个。
- 超时的写入任务改动保留，先 `reply` 续做；任务太大就拆小，不要为求快压短 `--timeout`。
- 机器与仓库有并发上限，命中时先 `wait` 收一批；上限由用户设定，不要自行调整。

状态含义、退出码、JSON 字段、等待的细节与完整边界见 [references/reading-results.md](references/reading-results.md)；续接与返工、两档配合、并行粒度见 [references/practice.md](references/practice.md)；长时间自主推进一批工作见 [references/long-run.md](references/long-run.md)；命令与全部选项见 `$D --help`，run 目录与文件见 [references/output-and-files.md](references/output-and-files.md)。
