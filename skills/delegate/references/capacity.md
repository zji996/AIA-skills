# 容量与自动排队

整机 `maxActive` 默认 32，仓库 `repoMaxActive` 默认 0（不限），二者是防失控上限；实际限流靠按模型并发、`maxHeavy` 与内存水位。仓库键按规范化 git common dir（主仓库与其 worktree 共用），非 git 目录按规范化路径。仓库计数仍来自整机 slot，不另建池；旧 slot 只有 run 目录时从 meta 反查仓库，新 slot 可附仓库键，读 slot 失败沿用原处理。容量不足照常返回 run id，supervisor 自动排队；reply 优先于 start，同级 FIFO，模型或仓库满的任务不阻挡其他可运行任务。排队不占 active 或新 worktree 池位，放行时才分配工作树。`stop` 取消，之后 `clean` 可清理。

`status` 显示「排队中 N 分钟 · 原因 · 前面还有 k 个」；JSON 为 `state: "waiting"`、`queue: {reason,ahead,since}`（since 为 UTC 入队时间），不含运行 elapsed。原因分总路数、仓库路数、模型并发（别名与上限）、可用内存。`wait`/`wait --any` 将它当作未完成，等待上限沿用原规则；容量队本身没有超时。supervisor 消失或机器重启后与现有 deferred 一样显示 `crashed`，不会自动恢复；主控确认后重新派发或清理。


模型规则与 `--overflow next|queue` 见 [模型路由](model-routing.md)。同一完整模型在多档声明上限时取最小值，固定 `--model`、环境指定模型和续做也受限；别名或时段不绕过上限。

## 放行

全机启动锁内复查全部容量并登记放行，避免多个等待者抢同一名额。排在前面的任务若其模型或仓库满了，就暂时跳过；尚可运行的前序任务先虚拟保留名额，确保同级 FIFO 与 reply 优先。`ahead` 是所有更高优先级及同级较早入队的任务数，不表示它们会阻塞本任务。容量队每 100ms 检查一次，也能在没有任何 active 任务时等内存水位恢复；heavy lane 的锁等待机制不变。

## 工期参数校准（speed）

用写任务链数 `N` 与关键路径深度 `D` 估墙钟，不再按人类工作日估算：

```text
T_dev = max(D · t_crit, N · t_chain / P_eff) + T_merge
```

运行 `delegate speed [--since 7d|14d|all] [--repo PATH] [--json]`，从全机事件流 `<state>/events.jsonl` 按仓库计算各参数（建议每周校准一次）：

- **写任务链（`t_chain`）**：`started` 且 `mode == "write"` 的 run 按 `payload.parent` 递归归到根任务；统计从首次 `started` 到首次 `applied` 的墙钟分布（`p50` / `p75` / `mean` 与合入条数），公式中的 `t_chain` 取 `p50`。
- **关键路径单链耗时（`t_crit`）**：非一次过写链（链内 `started` 的 run 数 `> 1` 或出现过 `rework_sent`）的 `p50` / `p75`；一次过写链的 `p50` 单独列出。
- **返工率（分母为已合入写链）**：自动打回比例（含 `rework_sent`）、总非一次过比例（run 数 `> 1` 或含 `rework_sent`）、3 轮以上占比（总轮数 = 链内 `started` run 数 + `rework_sent` 次数 `>= 3`）。
- **有效并发（`P_eff`）**：按分钟统计至少有 1 条写链在跑的活跃分钟内并发均值（同时输出 `p50` / `p75`），以及每活跃小时合入写链数；每条链的活跃区间取链内各 run 从 `started` 到终态（`delivered` / `rejected` / `failed` / `stopped` / `question`，无终态回退 `applied`）的并集，不把交付后等待合入的空闲时段计入并发。
- **合入摩擦（`T_merge`）**：`premerge_conflict` / `premerge_failed` 写链占比（分母为窗口内全部写链），以及每条已合入写链最后一次 `delivered` / `review_verdict` 到首次 `applied` 的等待耗时 `p75`。
- **只读链**：`mode != "write"` 的任务链按首次 `started` 到末次终态计算 `p50` / `p75`，单独列示。
- **自动扩窗**：请求窗口短于 14 天（默认 `7d`）且该仓库已合入写链不足 30 条时，自动扩至 `14d` 并在表尾说明。
- **历史记录与漂移**：每次运行将快照追加到 `<state>/speed/<repo-slug>.jsonl`，并与上一条记录对比 `t_chain`、`t_crit`、`P_eff`、`T_merge` 的相对变化，超过 `±20%` 标出 `⚠`。

## 无模型补位派发与热点

- 仓库级 `.delegate.json` 配 `fill: {"target": 6, "maxInfraDocsShare": 0.3}`（缺省不启用）与 `hotspots: ["README.md", ...]`（常冲突单文件；norvane 先放 `xtask/size-baseline.txt`，aia-skills 先放 `README.md`）。
- 工作表 `reconcile` 时，若该仓库在跑写链数 `< target`（只数写任务；只读、调研类 run 不计入也不设上限），从「就绪 + 有 brief + 有类别 + 有 `autoDispatch` 参数」中挑 1 项派出：先按类别 `feat > unblock > infra > docs`，同类按出度（有多少未关闭项在等它）从大到小，再按就绪时间从早到晚。
- `infra` + `docs` 在跑数加本次派发后不超过 `maxInfraDocsShare` 配额（`target=6` 时最多 2 条）；候选 `files` 前缀与在跑项同撞某个 `hotspots` 路径时跳过（其余路径不互斥）。每次补位记 `work_dispatched`（带 `reason: "fill"`、`class` 与 `activeWriteRuns`）。

## 放开的节奏（不自动升降档）

- `target` 从 6 起（只数写链）。满 20 次写入交付后运行 `delegate fill stats [--since 24h]` 检查停止线：premerge 冲突率 `>10%`、返工率 `>28.3%`、已交付未合入峰值 `>5`、lane 排队占比 `>15%`（并核对每 10 次交付 agy 周额度与只读 run 占比）。
- 没越线就试 `8`；越线就先补 `hotspots`，仍越线再退到 `5`。越线仅提醒（ADR 0120），不自动升降档，均由主控手动改 `.delegate.json`。
