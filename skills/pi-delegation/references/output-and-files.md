# 输出与过程文件

## 读懂输出

- **控制台进度**：仅显示写入、失败、重试、压缩及每 20 次只读/命令动作的计数；最终答复和总结必显示。单条读取、搜索和成功命令留在 `events.jsonl`，不占主控上下文。
- **状态行**：`state`（running/ok/failed/timeout/killed/stopped/crashed）、`files`（写过的文件）、`turns`、`bashRuns`、`failedBashRuns`、`tokens`、`result` 与 `resultChars`。运行中还有 `last`（最近动作）和 `idleSeconds`；`last` 为 `thinking` 或 `answering` 时表示模型正在长时间生成，不是卡死。
- **最终答复**：直接运行 `pi-json-stream.sh` 时超过 2400 字符只显示末尾，完整内容保留在 `events.jsonl`；通过 `pi-delegate.sh` 运行时，超过 6000 字符只显示末尾，完整内容见 `result.md` 或加 `--full`。后者阈值可用 `PI_DELEGATE_RESULT_CHARS` 调整。

## 过程文件位置

- 记录跟随主控：放在**执行命令时当前目录所在的 git 根**下的 `.local/run/pi/<run_id>/`，不在仓库中则放在当前目录的 `.local/run/pi/`，与 `--workdir` 无关。主控在同一项目的任意子目录都能用 id 或 `last` 找到记录；换到别的项目时要传 run 目录路径，或统一设置 `PI_DELEGATE_RUNS`。
- 根目录首次创建时写入只含 `*` 的 `.gitignore`，不会污染 `git status`；run 目录权限为 `700`。
- 主要文件：`prompt.md`（提示词副本）、`events.jsonl`（完整事件）、`result.md`（完整答复）、`summary.json`、`stderr.log`。Pi 修改的文件在 `--workdir` 中。
- 每次 `start` 会删除结束超过 7 天且结果已报告的记录；用 `PI_DELEGATE_KEEP_DAYS` 调整，设为 `0` 关闭。

## 底层执行器

`scripts/pi-json-stream.sh [--provider] [--model] [--thinking] [--read-only] [--timeout 15m] [--workdir <dir>] <prompt-file> <events-log>` 是前台同步版本，`pi-delegate.sh` 在后台调用它。它把 Pi 的 JSON 流过滤成事件日志并追加 `summary`，超时返回 124，被杀返回 137；只有 Pi 正常退出、最后一轮成功且答复非空、发出 `settled` 时才返回 0。

它在启动 Pi 时导出 `PI_DELEGATE_ACTIVE=1`；检测到该变量时，`pi-delegate.sh start/run` 与 `pi-json-stream.sh` 都会以退出码 2 拒绝，防止被委派的 Pi 再次委派自己。
