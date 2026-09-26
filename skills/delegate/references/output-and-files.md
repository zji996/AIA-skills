# 命令、run 目录与文件

## 命令与选项

| 命令 | 作用 |
|---|---|
| `start [选项] [任务说明]` | 后台启动，立即返回一行状态 |
| `run [选项] [--max <时长>]` | 启动并等到结论 |
| `reply <run> [消息] [--accept <命令>] [--image] [--timeout] [--max]` | 在同一会话里追问并等到结论：同一同事、workdir、worktree 与只读模式；`<run>` 指对话中任一轮，自动接在最新一轮后。验收命令默认沿用，换了才写进消息，`--accept ''` 取消 |
| `diff [<run>] [--stat] [--total] [路径...]` | 以 `git diff` 输出该轮的改动；`--total` 为整段对话；终端下带颜色 |
| `apply [<run>] [--dry-run] [--merge]` | 把 `--worktree` 对话的全部改动合并回原工作区（只写文件，不碰 index）：你没动过的文件直接写入（含权限位），双方都改过的文本做三方合并，大文件从 worktree 复制；经符号链接目录、文件与目录互换、二进制与符号链接冲突一律算冲突；有合并不了的冲突时什么都不写，`--merge` 则写入其余文件并在冲突处留冲突标记（其余冲突跳过）。没有跳过项时整段对话标记 `.applied` |
| `wait [<run>...\|--all] [--max <时长>] [--no-result] [--full] [--progress]` | 等待并输出结论；`--all` 含仍在运行和尚未读取结果的任务 |
| `status [<run>...]` | 每个任务一行 JSON；运行中带 `last` 与 `idleSeconds` |
| `result [<run>] [--path]` | 输出完整答复 |
| `stop <run>...` | 终止任务及其进程组 |
| `clean <run>...\|--finished [--force]` | 删除已结束的任务；`--finished` 默认保留结果未读取的 |

启动选项：`--agent pi|codex`、`--image <路径>`（可重复）、`--accept <命令>`、`--hide-accept`、`--accept-timeout`（默认 10m）、`--read-only`、`--workdir`、`--timeout`（每次尝试，pi 默认 15m，codex 默认 30m）、`--retries`（答复畸形时重跑次数，默认 1）、`--model`/`--thinking`/`--provider`（不指定时用各 CLI 自己的默认设置；Codex 的 `--thinking` 对应推理强度）、`--allow-parallel-writes`、`--worktree`。`<run>` 可以是完整 id、唯一片段、`last` 或 run 目录。

## 改动清单

在 git 仓库中，启动时和同事结束后（验收命令之前）各把整个工作区记为一个 tree 对象：借用真实 index 的副本执行 `git add -A` 与 `write-tree`，真实 index、分支和 stash 都不动；被忽略的文件不计，大于 `DELEGATE_SNAPSHOT_MAX_BYTES`（默认 2 MiB）的未跟踪文件只比较大小与修改时间。两次快照之差就是 `files`、`changes`（文件数与 +/- 行数）、`changes.json` 与 `changes.patch`，因此 shell 或脚本改的文件也会列出，改了又改回的不列，运行前已有的脏改动不算。原地运行时，别人在同一时间对仓库的改动也会被计入；需要干净归属时用 `--worktree`。非 git 目录只能根据编辑事件列出 `files`。

## worktree

`--worktree` 在 `${XDG_CACHE_HOME:-~/.cache}/delegate/worktrees/<仓库名>-<run>` 建立 detached worktree（放在仓库外，免得测试、lint、文件监听扫到），先检出 HEAD，再把启动时的快照写入，所以同事看到的正是你当前的工作区（含未提交与未忽略的未跟踪文件）。`--workdir` 为子目录时，同事在 worktree 的对应子目录工作。supervisor 在同事启动前按仓库根 `.delegate.json` 准备 worktree：

```json
{"worktree": {"copy": [".env"], "link": ["models/weights"],
              "setup": ["pnpm install --offline --frozen-lockfile", "uv sync --frozen --offline"]}}
```

- `copy`：小的被忽略文件或目录，复制过去，改动不影响原仓库。
- `link`：大而只读的被忽略目录，建符号链接，写入会落到原仓库。
- `setup`：依次在 worktree 根执行，输出写入 `setup.log`，任一失败即判 `failed`（`DELEGATE_SETUP_TIMEOUT`，默认 10m）。依赖用包管理器从本机缓存重建：pnpm 与 uv 以硬链接安装，几 GB 的环境也只需一两秒；不要 link `node_modules`、`.venv`，其中的可编辑安装指向原仓库源码。
- 这三类路径不计入改动。子模块不会自动初始化，需要时写进 `setup`。

worktree 由对话共享，`clean` 删除最后一个使用它的 run 时执行 `git worktree remove`；`agent-handoff` 会列出尚未 `apply` 的 worktree。

## 会话

Pi 的会话保存在 run 目录的 `session/` 下（每次尝试一个 `--session-id`；`reply` 先复制上一轮的会话，清理早先的轮次不影响续接），Codex 使用自己的会话存储，`summary.json` 的 `session` 记下最后的会话 id；`reply` 以 `pi --session-id` / `codex exec resume` 续接。4.1 之前的 run 使用 `--no-session`，不能 `reply`。

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
| `summary.json` | 结论：`state`、`attempts`、`files`、`changes`、`accept`、`readOnlyViolation`、`tokens`、`session`、`error` |
| `changes.json` / `changes.patch` | 前后快照的 tree、逐文件状态与行数；可直接 `git apply` 的补丁 |
| `setup.log` | `--worktree` 的 `setup` 命令输出 |
| `session/` | Pi 的会话记录，供 `reply` 续接 |
| `.applied` | `--worktree` 的改动已由 `apply` 合并 |
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
