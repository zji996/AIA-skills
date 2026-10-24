---
name: delegate
description: 把可独立验收的任务交给同事 Agent 在后台并行完成，你只收结果并把关。遇到这些情况时主动使用，不必等用户开口：要读很多文件但只需结论；改动前想要第二意见或独立审查；有两个以上互不依赖、能用命令验收的子任务；需要有人看截图或设计图给意见。同事都是 Pi，按档位选模型：默认档（未配则强档）接日常读写，强档写代码与严密审查，便宜档只读（侦察、摘要、文案、看图）；失败自动换强档模型重跑。脚本代跑验收命令，只给一行结论与下一步，过程不进主控上下文；整机与仓库容量、重检查有上限。Use to delegate or parallelize work proactively to Pi colleagues, and to get a second opinion, code review or image review from another model, judged by results.
license: MIT
compatibility: Linux x86_64 或 aarch64；入口是安装时下载的静态二进制 bin/delegate，不需要 Python；需要 pi CLI。
metadata:
  version: "6.4.0"
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

每个任务回一条短行（状态、改动、验收、下一步）和答复。写入任务看 `$D diff <name>`，满意后 `$D apply <name>`。

| 想要 | 命令 |
|---|---|
| 读材料、审查、第二意见 | `$D start --read-only "…"`；限篇幅加 `--max-answer 800` |
| 看截图或设计图 | `$D start --read-only --image shot.png "…"` |
| 改代码，测试通过才算完 | `$D start --worktree --accept "make test" "…"`；不许碰的路径加 `--protect tests/` |
| 追问或返工 | `$D reply <name> "…"`；开新会话加 `--fresh`，带上你后来的改动加 `--sync` |
| 先 A 后 B | `$D start --after <A> …` |
| 自己跑重检查、测性能 | `$D lane make check`；测量用 `lane --measure` |
| 看全文 / 改动 / 停掉 / 清理 | `$D result` / `$D diff` / `$D stop` / `$D clean --finished` |

多行说明用 `--prompt-file -` 加 heredoc；示例见 [references/recipes.md](references/recipes.md)，全部选项见 `$D --help`。

## 档位

同事都是 Pi，档位只选模型：不写 `--tier` 用 default（未配置则 strong）；`--tier strong` 用于写代码和严密审查；`--tier cheap` 只读，用于侦察、文案、看图。失败且没改文件时自动换强档重跑一次；`--model` 可直接指定模型。首次使用没配模型时，`start` 会列出可选模型和配置命令，照做即可（用户指定过就用用户的）。

## 任务说明

同事看不到你的会话：写清目标、能改的范围、完成标准（最好是验收命令，复杂的写成脚本）和已知事实。仓库每次都要说的话放进 `.delegate.json` 的 `standing`，见 [references/configuration.md](references/configuration.md)。

## 收结果

- 已交付表示验收通过，改法仍要看 diff；已答复没有验收，关键结论自己核实。
- 状态为「提问」时同事卡在需要你定的点上：`$D reply <name> "<决定>"` 回答，不算返工；只有改过文件后再要求改才算。
- 同一仓库的写入串行 `apply`，合入一批后自己跑全量检查。

状态、退出码与等待细节见 [references/reading-results.md](references/reading-results.md)；续接、并行与长时间推进见 [references/practice.md](references/practice.md)、[references/long-run.md](references/long-run.md)；run 目录见 [references/output-and-files.md](references/output-and-files.md)。
