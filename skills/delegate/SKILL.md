---
name: delegate
description: 把可独立验收的任务交给同事 Agent 在后台并行完成，你只收结果并把关。遇到这些情况时主动使用，不必等用户开口：要读很多文件但只需结论；改动前想要第二意见或独立审查；有两个以上互不依赖、能用命令验收的子任务；需要有人看截图或设计图给意见。同事是 Pi 或 Antigravity CLI（agy，登记为 agy/<模型> 的别名走它），按用途 scout（只读侦察）、review（只读严密审查）、write（写代码带验收）派发；也可用 models 别名指定模型、默认 thinking 与价位 cost；fit 适配卡记录三类任务的质量与价位，新模型按同题重评。脚本代跑验收命令，只给一行结论与下一步，过程不进主控上下文；容量满会排队，重检查也限流。Use to delegate or parallelize work proactively to Pi or agy (Antigravity CLI) colleagues, and to get a second opinion, code review or image review from another model, judged by results.
license: MIT
compatibility: Linux x86_64 或 aarch64；入口是安装时下载的静态二进制 bin/delegate，不需要 Python；需要 pi CLI（pi-kit 的 rpi 通道下 `pi` 即 rpi）。
metadata:
  version: "6.57.0"
  binary: delegate
  extra-binaries: delegate-tui
  exclude-agents: pi
---

# Delegate（按结果委派）

你是主控：把边界清楚的任务交给同事（Pi 或 agy），自己继续推进，回来看结果、决定是否采纳。同事不能再委派。

## 用法

```bash
D=<本技能目录>/bin/delegate

# 1. 只读侦察 / 审查（看图加 --image shot.png）
$D start --read-only "说明目标与需要核实的结论"

# 2. 改代码（隔离 worktree + 验收命令）
$D start --worktree --accept "测试命令" "说明目标、改动范围与已知事实"

# 3. 放后台等结果（agy 用 run_command 设 WaitMsBeforeAsync=500；幂等可重入，勿手写 while/sleep 轮询）
$D wait

# 4. 追问 / 回答提问 / 返工（运行中直接追加，已结束自动续接）
$D reply <run> "决定或修改意见"

# 5. 看改动并一站式合入提交（合入 → 验收 → 按全链文件集带 trailer 提交 → 自动清 worktree）
$D card <run>                                 # 完整 diff 用 $D diff <run>
$D land <run> -m "提交说明"                    # 后续命令用 && 不要用 ;
```

## 规则

1. 默认档位、思考深度、只读探路、基线同步与合入后清理都已自动选好，常规任务**不要传额外选项**；多行说明用 `--prompt-file -` 加 heredoc。
2. 同事看不到你的上下文：写清目标、范围、验收命令和已知事实；看到 `提问` 状态用 `reply` 回答（不算返工）。
3. `land`/`apply` 最后一行是结论，`FAILED（未合入）` 时按提示处理，不要在旧代码上跑测试；不要给 `land`/`apply` 套短 `timeout`。

> 快照对象钉在 `refs/delegate/<run-id>/*`（`clean` 时移除），普通 `git gc` 安全；切勿执行 `git prune` 或 `git gc --prune=now|all`（见 `gc.log` 删该文件即可）。

## 按需参考

- 命令全表、选项与示例：[命令与选项](references/commands.md)、[示例](references/recipes.md)、[收结果与状态](references/reading-results.md)、[目录与产物](references/output-and-files.md)（全部选项另见 `$D --help`）
- 档位、模型适配卡与配置：[模型路由与适配卡](references/model-routing.md)、[配置](references/configuration.md)、[运行与远端](references/runtime.md)、[agy 账号与用量](references/agy-accounts.md)
- 审查、合入、工作表与清理：[审查卡、预合并与 `Fixes-Run` 漏网记账](references/review-card.md)、[审查要点](references/review-rubric.md)、[并行合入](references/parallel-safety.md)、[合入员与 Radar](references/merge-and-radar.md)、[工作表](references/worktable.md)、[清理与回收](references/cleanup.md)、[磁盘健康](references/disk-health.md)、[容量与工期](references/capacity.md)、[续接与长跑](references/practice.md)、[长时间推进](references/long-run.md)
