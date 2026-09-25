# run 目录与文件

## 位置

- 默认放在**执行命令时当前目录所在的 git 根**下的 `.local/run/pi/<run_id>/`；不在仓库中则放在当前目录的 `.local/run/pi/`，与 `--workdir` 无关。主控在同一项目的任意子目录都能用 id 或 `last` 找到记录；换项目时传 run 目录路径，或统一设置 `PI_DELEGATE_RUNS`。
- 根目录首次创建时写入只含 `*` 的 `.gitignore`，不污染 `git status`；run 目录权限为 `700`。

## 文件

| 文件 | 内容 |
|---|---|
| `meta.json` | 启动参数、同事（`agent`）、workdir、模式、验收命令、启动时间 |
| `prompt.md` | 同事实际收到的任务说明；末尾可能附完成标准（`--accept`）与只读边界（Codex 只读任务） |
| `events.jsonl` | 过滤后的全过程：读取、命令、编辑路径、错误、每轮模型与用量、重跑；不含编辑全文 |
| `result.md` | 最后一轮的完整答复 |
| `summary.json` | 结论：`state`、`attempts`、`files`、`accept`、`readOnlyViolation`、`tokens`、`error` |
| `accept.log` | 验收命令的完整输出与退出码 |
| `stderr.log` | 同事 CLI 的标准错误 |
| `exit_code` | 结束标记：`0` 为 delivered/answered，`1` 为其他结局；运行中不存在 |
| `.delivered` | 结果已被 `run`/`wait`/`result` 读取过 |

`agent-handoff` 的快照脚本依赖 `meta.json`、`exit_code` 与 `.delivered` 判断未完成或未读取的委派；修改这三者的名字或含义时要同步修改它。

## 清理

每次 `start` 会删除结束超过 7 天且结果已读取的记录；`PI_DELEGATE_KEEP_DAYS` 调整天数，设为 `0` 关闭。

## 环境变量

| 变量 | 作用 |
|---|---|
| `PI_DELEGATE_RUNS` | run 根目录 |
| `PI_DELEGATE_RESULT_CHARS` | 答复超过该长度只显示末尾，默认 6000 |
| `PI_DELEGATE_KEEP_DAYS` | 自动清理天数，默认 7 |
| `PI_DELEGATE_POLL` | 等待时的检查间隔秒数，默认 1 |
| `PI_DELEGATE_AGENT` | 由脚本导出给同事（`pi`/`codex`），用于执行委派层级 |
| `PI_DELEGATE_PARENT_RUN` | 由脚本导出给同事，值为其所在 run；它委派的写入任务不受这个 run 的写入互斥限制 |
| `PI_DELEGATE_ACTIVE` | 旧版标记，仍导出给 Pi；存在时视为 Pi 调用者 |
