---
name: delegate
description: 把可独立验收的任务交给同事 Agent 在后台并行完成，你只收结果并把关。遇到这些情况时主动使用，不必等用户开口：要读很多文件但只需结论；改动前想要第二意见或独立审查；有两个以上互不依赖、能用命令验收的子任务；需要有人看截图或设计图给意见。同事都是 Pi，按用途 scout（只读侦察）、review（只读严密审查）、write（写代码带验收）派发；也可用 models 别名指定模型，thinking 独立选。脚本代跑验收命令，只给一行结论与下一步，过程不进主控上下文；容量满会排队，重检查也限流。Use to delegate or parallelize work proactively to Pi colleagues, and to get a second opinion, code review or image review from another model, judged by results.
license: MIT
compatibility: Linux x86_64 或 aarch64；入口是安装时下载的静态二进制 bin/delegate，不需要 Python；需要 pi CLI（pi-kit 的 rpi 通道下 `pi` 即 rpi）。
metadata:
  version: "6.14.0"
  binary: delegate
  exclude-agents: pi
---

# Delegate（按结果委派）

你是主控：把边界清楚的任务交给同事（Pi），自己继续推进，回来看结果、决定是否采纳。同事不能再委派。

## 用法

```bash
D=<本技能目录>/bin/delegate
$D start --read-only "审查 apps/api 的错误处理，列真实缺陷并写文件:行号"   # 放出，可连放多个
$D wait                                                                  # 放后台；下一个结束就返回，末行给出继续等的命令
```

总路数、仓库路数、模型并发或内存不足时自动排队、不换模型；`status` 看排队原因与前方人数，`--json` 看 `queue`。只有愿意模型满时改用下一候选才加 `--overflow next`。

每个任务回一条短行（状态、改动、验收、下一步）和答复。写入任务看 `$D diff <name>`，满意后 `$D apply <name>`。

| 想要 | 命令 |
|---|---|
| 读材料、审查、第二意见 | `$D start --read-only "…"`；限篇幅加 `--max-answer 800` |
| 看截图或设计图 | `$D start --read-only --image shot.png "…"` |
| 改代码，测试通过才算完 | `$D start --worktree --accept "make test" "…"`；不许碰的路径加 `--protect tests/` |
| 追问或返工 | `$D reply <name> "…"`；开新会话加 `--fresh`，带上你后来的改动加 `--sync` |
| 先 A 后 B | `$D start --after <A> …`；写入 B 从 A 交付的改动起步，`apply A B` 按序合入 |
| 自己跑重检查、测性能 | `$D lane make check`；测量用 `lane --measure` |
| 节点不稳、换档前先测连通 | `$D probe`：走 Pi 本身（不是裸 HTTP）测各档模型，不建任务、不占名额 |
| 看全文 / 改动 / 停掉 / 清理 | `$D result` / `$D diff` / `$D stop` / `$D clean --finished` |

多行说明用 `--prompt-file -` 加 heredoc；示例见 [references/recipes.md](references/recipes.md)，全部选项见 `$D --help`。

## 档位

三个独立维度：`--tier scout|review|write` 选用途（scout 侦察/摘要/找文件/文案，review 严密审查/根因/第二意见，write 改代码带验收）；`--model <别名>` 从配置 `models` 指定模型，与 `--tier` 互斥；`--thinking off|minimal|low|medium|high|xhigh` 选思维长度，不再写进模型名。省略用途与模型时，写入推断 write、只读推断 review，回显提醒且状态标「推断」；scout/review 不接写入。图片只检查最终模型的 Pi images 声明，不改选模型。上游瞬时失败先同会话续做，仍失败且未改文件才换本档下一个模型；固定模型不换。首次使用用 `$D tiers --alias name=provider/id --scout name --review name --write name --thinking high` 登记并配齐三档；只选已登记的模型，细节见 [references/configuration.md](references/configuration.md)。

## 任务说明

同事看不到你的会话：写清目标、能改的范围、完成标准（最好是验收命令，复杂的写成脚本）和已知事实。仓库每次都要说的话放进 `.delegate.json` 的 `standing`，见 [references/configuration.md](references/configuration.md)。

## 收结果

- 已交付表示验收通过，改法仍要看 diff；已答复没有验收，关键结论自己核实。
- 状态为「提问」时同事卡在需要你定的点上：`$D reply <name> "<决定>"` 回答，不算返工；只有改过文件后再要求改才算。
- 同一仓库的写入一次 `apply A B …` 按序合入；验收没法沿用的任务在合并后的主干重跑一次各自的验收，全量检查仍由你按批跑。

状态、退出码与等待细节见 [references/reading-results.md](references/reading-results.md)；续接、并行与长时间推进见 [references/practice.md](references/practice.md)、[references/long-run.md](references/long-run.md)；run 目录见 [references/output-and-files.md](references/output-and-files.md)。
