# 容量与自动排队

整机 `maxActive` 默认 32，仓库 `repoMaxActive` 默认 0（不限），二者是防失控上限；实际限流靠按模型并发、`maxHeavy` 与内存水位。仓库键按规范化 git common dir（主仓库与其 worktree 共用），非 git 目录按规范化路径。仓库计数仍来自整机 slot，不另建池；旧 slot 只有 run 目录时从 meta 反查仓库，新 slot 可附仓库键，读 slot 失败沿用原处理。容量不足照常返回 run id，supervisor 自动排队；reply 优先于 start，同级 FIFO，模型或仓库满的任务不阻挡其他可运行任务。排队不占 active 或新 worktree 池位，放行时才分配工作树；`--after` 先等上游成功才入容量队。`stop` 取消，之后 `clean` 可清理。

`status` 显示「排队中 N 分钟 · 原因 · 前面还有 k 个」；JSON 为 `state: "waiting"`、`queue: {reason,ahead,since}`（since 为 UTC 入队时间），不含运行 elapsed。原因分总路数、仓库路数、模型并发（别名与上限）、可用内存。`wait`/`wait --any` 将它当作未完成，等待上限沿用原规则；容量队本身没有超时。supervisor 消失或机器重启后与现有 deferred 一样显示 `crashed`，不会自动恢复；主控确认后重新派发或清理。


模型规则与 `--overflow next|queue` 见 [模型路由](model-routing.md)。同一完整模型在多档声明上限时取最小值，固定 `--model`、环境指定模型和续做也受限；别名或时段不绕过上限。

## 放行

全机启动锁内复查全部容量并登记放行，避免多个等待者抢同一名额。排在前面的任务若其模型或仓库满了，就暂时跳过；尚可运行的前序任务先虚拟保留名额，确保同级 FIFO 与 reply 优先。`ahead` 是所有更高优先级及同级较早入队的任务数，不表示它们会阻塞本任务。容量队每 100ms 检查一次，也能在没有任何 active 任务时等内存水位恢复；heavy lane 的锁等待机制不变。
