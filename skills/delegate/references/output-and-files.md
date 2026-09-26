# 命令、run 目录与文件

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

启动选项：`--agent pi|codex`、`--image <路径>`（可重复）、`--accept <命令>`、`--hide-accept`、`--accept-timeout`（默认 10m）、`--read-only`、`--workdir`、`--timeout`（每次尝试，pi 默认 15m，codex 默认 30m）、`--retries`（答复畸形时重跑次数，默认 1）、`--model`/`--thinking`/`--provider`（不指定时用各 CLI 自己的默认设置；Codex 的 `--thinking` 对应推理强度）、`--allow-parallel-writes`。`<run>` 可以是完整 id、唯一片段、`last` 或 run 目录。

图片以绝对路径交给同事：Pi 作为 `@<路径>` 附件，Codex 作为 `--image`。

## 位置

- 默认放在**执行命令时当前目录所在的 git 根**下的 `.local/run/pi/<run_id>/`（沿用 4.0 前的目录名，`agent-handoff` 依赖它）；不在仓库中则放在当前目录的 `.local/run/pi/`，与 `--workdir` 无关。主控在同一项目的任意子目录都能用 id 或 `last` 找到记录；换项目时传 run 目录路径，或统一设置 `DELEGATE_RUNS`。
- 根目录首次创建时写入只含 `*` 的 `.gitignore`，不污染 `git status`；run 目录权限为 `700`。
- 整机并发登记放在 `${XDG_STATE_HOME:-~/.local/state}/delegate/`：每个运行中的任务一个 `*.slot` 文件，内容是其 run 目录；任务结束或目录被删后，下一次 `start` 自动清掉对应登记。这个位置刻意不提供 `DELEGATE_*` 覆盖，被委派的同事无法另起一个计数池。

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

每次 `start` 会删除结束超过 7 天且结果已读取的记录；`DELEGATE_KEEP_DAYS` 调整天数，设为 `0` 关闭。

## 环境变量

均先读 `DELEGATE_<名称>`，未设置时回退到 4.0 前的 `PI_DELEGATE_<名称>`。

| 变量 | 作用 |
|---|---|
| `DELEGATE_RUNS` | run 根目录 |
| `DELEGATE_MAX_ACTIVE` | 整机同时运行的任务上限，默认 6；`0` 不限 |
| `DELEGATE_MAX_CODEX` | 其中 Codex 任务上限，默认 3；`0` 不限 |
| `DELEGATE_RESULT_CHARS` | 答复超过该长度只显示末尾，默认 6000 |
| `DELEGATE_KEEP_DAYS` | 自动清理天数，默认 7 |
| `DELEGATE_POLL` | 等待时的检查间隔秒数，默认 1 |
| `DELEGATE_AGENT` | 由脚本导出给同事（`pi`/`codex`），用于执行委派层级 |
| `DELEGATE_PARENT_RUN` | 由脚本导出给同事，值为其所在 run；它委派的写入任务不受这个 run 的写入互斥限制 |
| `PI_DELEGATE_ACTIVE` | 旧版标记，仍导出给 Pi；存在时视为 Pi 调用者 |
