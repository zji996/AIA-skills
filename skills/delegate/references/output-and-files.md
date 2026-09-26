# 命令、run 目录与文件

## 命令与选项

| 命令 | 作用 |
|---|---|
| `start [选项] [任务说明]` | 后台启动，立即返回一行状态 |
| `run [选项] [--max <时长>]` | 启动并等到结论 |
| `reply <run> [消息] [--fresh] [--accept <命令>] [--image] [--timeout] [--max]` | 接着上一轮的会话追问并等到结论（`--fresh` 则开新会话，消息须自足，验收命令照常附上）：同一同事、workdir、worktree 与只读模式；`<run>` 指对话中任一轮，自动接在最新一轮后。验收命令默认沿用，换了才写进消息，`--accept ''` 取消；取消或隐藏已变更的命令时会告诉同事旧标准不再适用 |
| `diff [<run>] [--stat] [--total] [路径...]` | 以 `git diff` 输出该轮的改动；`--total` 为整段对话；终端下带颜色 |
| `apply [<run>] [--dry-run] [--merge]` | 把 `--worktree` 的现状（含最后一轮之后在 worktree 里的手工修改）相对对话起点的全部改动合并回原工作区（只写文件，不碰 index）：你没动过的文件直接写入（含权限位），双方都改过的文本做三方合并，大文件从 worktree 复制；经符号链接目录、文件与目录互换、二进制与符号链接冲突一律算冲突；有合并不了的冲突时什么都不写，`--merge` 则写入其余文件并在冲突处留冲突标记（其余冲突跳过）。没有跳过项时，共用该 worktree 的所有 run（含畸形的旁支）标记 `.applied` |
| `wait [<run>...\|--all] [--max <时长>] [--no-result] [--full] [--progress]` | 等待并输出结论；不指定任务时（或 `--all`）等所有仍在运行和尚未读取结果的任务 |
| `status [<run>...]` | 每个任务一行 JSON；运行中带 `last` 与 `idleSeconds` |
| `result [<run>] [--path]` | 输出完整答复 |
| `stop <run>...` | 终止任务及其进程组 |
| `clean <run>...\|--finished [--force]` | 删除已结束的任务；`--finished` 默认保留结果未读取的 |
| `lane [--label <文字>] [--] <命令>` | 在整机重任务队列里执行命令（一个参数按 shell 命令执行），退出码原样返回；不带命令时列出正在跑与排队的项 |

启动选项：`--tier cheap|strong`（默认只读 cheap、写入 strong；便宜档失败且未改动时自动升档一次）、`--agent pi|codex`（直接指定，与 `--tier` 互斥，不升档）、`--image <路径>`（可重复）、`--accept <命令>`、`--hide-accept`、`--accept-timeout`（默认 10m）、`--read-only`、`--in-place`（只读任务读实时工作区而非快照）、`--workdir`、`--timeout`（每次尝试，pi 默认 15m，codex 默认 30m）、`--retries`（答复畸形时重跑次数，默认 1）、`--model`/`--thinking`/`--provider`（不指定时用各 CLI 自己的默认设置；Codex 的 `--thinking` 对应推理强度）、`--allow-parallel-writes`、`--worktree`。`<run>` 可以是完整 id、唯一片段、`last` 或 run 目录。

## 重任务队列（lane）

整机一条先进先出队列，同时放行 `DELEGATE_MAX_HEAVY` 个（默认 1，`0` 不限）：验收命令、worktree `setup`、同事与主控用 `lane` 跑的检查都在这里排队。每个排队者在 `${XDG_STATE_HOME:-~/.local/state}/delegate/lane/` 下有一张按到达时间命名的票，持有其排他 flock 直到结束；等待者阻塞在前一张票的锁上，由内核在其结束或进程死亡时唤醒，不轮询，崩溃不留死锁。已在队列内的命令（带 `DELEGATE_LANE_HELD`）再调用 `lane` 直接执行，不会等自己。

排队时间不计时：验收的 `--accept-timeout` 与 `setup` 的超时从拿到名额开始算；同事自己用 `lane` 排队的时间记在 run 目录的 `lane-wait`，从它的 `--timeout` 中扣除。验收与 setup 命令在独立进程组中运行，超时或结束后整组清理，不留后台残留。

同事的超时：到 `--timeout`（不含排队）时若有命令正在执行，或 120 秒内有事件，继续运行，最多到 `--timeout` 的 `1 + DELEGATE_TIMEOUT_GRACE/100` 倍（默认 1.5 倍）；结论里 `graceSeconds` 记下超出的秒数。

## 改动清单

在 git 仓库中，启动时和同事结束后（验收命令之前）各把整个工作区记为一个 tree 对象：借用真实 index 的副本执行 `git add -A` 与 `write-tree`，真实 index、分支和 stash 都不动；被忽略的文件不计，大于 `DELEGATE_SNAPSHOT_MAX_BYTES`（默认 2 MiB）的未跟踪文件只比较大小与修改时间。两次快照之差就是 `files`、`changes`（文件数与 +/- 行数）、`changes.json` 与 `changes.patch`，因此 shell 或脚本改的文件也会列出，改了又改回的不列，运行前已有的脏改动不算。原地运行时，别人在同一时间对仓库的改动也会被计入；需要干净归属时用 `--worktree`。子模块只作为一个条目出现：其检出中的已跟踪改动、未跟踪文件或提交变化都记为 `submodule contents`。快照失败时（例如 git 出错），结论带 `warning`，改动清单显示 unknown，只读任务也因此无法核验。非 git 目录只能根据编辑事件列出 `files`。

## worktree

`--worktree` 在 `${XDG_CACHE_HOME:-~/.cache}/delegate/worktrees/<仓库名>-<run>` 建立 detached worktree（放在仓库外，免得测试、lint、文件监听扫到），检出的是启动时快照的提交（`commit-tree`，父提交为 HEAD，只被该 worktree 引用），所以同事看到的正是你当前的工作区（含未提交与未忽略的未跟踪文件），`git diff HEAD` 只显示它自己的改动；没有提交的新仓库也可以用。`--workdir` 为子目录时，同事在 worktree 的对应子目录工作。supervisor 在同事启动前按仓库根 `.delegate.json` 准备 worktree：

```json
{"worktree": {"copy": [".env"], "link": ["models/weights"],
              "setup": ["pnpm install --offline --frozen-lockfile", "uv sync --frozen --offline"]}}
```

- `copy`：小的被忽略文件或目录，复制过去，改动不影响原仓库。
- `link`：大而只读的被忽略目录，建符号链接，写入会落到原仓库。
- `setup`：依次在 worktree 根执行，输出写入 `setup.log`，任一失败即判 `failed`（`DELEGATE_SETUP_TIMEOUT`，默认 10m）。依赖用包管理器从本机缓存重建：pnpm 与 uv 以硬链接安装，几 GB 的环境也只需一两秒；不要 link `node_modules`、`.venv`，其中的可编辑安装指向原仓库源码。
- 这三类路径不计入改动。子模块在新 worktree 里是空目录：只读使用时写进 `link`（会替换空目录），需要独立修改时在 `setup` 里初始化。

`--read-only` 在 git 仓库里默认也用这样的 worktree（`--in-place` 除外），只作为供阅读的快照：主控同时的编辑既不影响它读到的内容，也不会被算成它的改动；它违规写入的文件留在 worktree 里，记为 `readOnlyViolation`，不能 `apply`。只读 Pi 没有 shell，不执行 `setup`；Codex 照常执行（它可能跑测试）。

worktree 由对话共享，`clean` 删除最后一个使用它的 run 时执行 `git worktree remove`，过期清理同理；`agent-handoff` 会列出尚未 `apply` 的写入型 worktree。

## 会话

每轮的会话都属于自己的 run：Pi 保存在 run 目录的 `session/` 下，Codex 使用自己的会话存储，`summary.json` 的 `session` 记下会话 id。`reply` 在每次尝试时从上一轮的会话**分叉**（Pi `--fork` 上一轮会话文件的副本，Codex `exec fork`），上一轮的会话从不被改动：答复畸形重跑时从同一处重新开始，清理早先的轮次也不影响后续追问。结局为 `malformed` 的轮次不算对话的延续，之后的 `reply` 与 `apply` 都从它的上一轮接着。4.1 之前的 run 使用 `--no-session`，不能 `reply`。

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
| `summary.json` | 结论：`state`、`attempts`、`files`、`changes`、`accept`、`readOnlyViolation` / `workspaceChanged`、`queuedSeconds`（同事在 lane 中排队的秒数）、`graceSeconds`、`warning`、`tokens`、`session`、`error`（`next` 由 `status` 按当前状态现算，不落盘） |
| `changes.json` / `changes.patch` | 前后快照的 tree、逐文件状态与行数；可直接 `git apply` 的补丁 |
| `setup.log` | `--worktree` 的 `setup` 命令输出 |
| `session/` / `fork/` | Pi 本轮的会话；`reply` 分叉所用的上一轮会话副本 |
| `.applied` | `--worktree` 的改动已由 `apply` 合并 |
| `accept.log` | 验收命令的完整输出与退出码；`summary.json` 的 `accept.queuedSeconds` 是它在 lane 中排队的秒数 |
| `supervisor.lock` | supervisor 在世期间持有的 flock，`wait` 阻塞在它上面 |
| `lane-wait` / `lane-waiting-*` | 同事在 lane 中已排队的秒数 / 正在排队的标记 |
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
| `DELEGATE_MAX_HEAVY` | lane 同时放行的重命令数，默认 1；`0` 不限 |
| `DELEGATE_MIN_AVAILABLE_MB` | 可用内存低于该值（MB）时拒绝启动，默认 4096；`0` 不检查 |
| `DELEGATE_TIMEOUT_GRACE` | 同事超时后仍在工作时的宽限百分比，默认 50 |
| `DELEGATE_RUN_DIR` / `DELEGATE_LANE_HELD` | 由脚本导出：同事所在 run 目录（用于扣除排队时间）/ 已在 lane 名额内 |
| `DELEGATE_RESULT_CHARS` | 答复超过该长度只显示末尾，默认 6000 |
| `DELEGATE_KEEP_DAYS` | 自动清理天数，默认 7 |
| `DELEGATE_POLL` | `wait --progress`、4.4 之前的 run 与刚启动的 supervisor 的检查间隔秒数，默认 1；其余等待不轮询 |
| `DELEGATE_CHEAP_AGENT` / `DELEGATE_STRONG_AGENT` | 档位对应的同事，默认 `pi` / `codex` |
| `DELEGATE_AGENT` | 由脚本导出给同事（`pi`/`codex`）；设有它的进程不能再委派 |
| `PI_DELEGATE_ACTIVE` | 旧版标记，仍导出给 Pi；存在时视为 Pi 调用者 |

## 代码结构

`bin/delegate` 是安装时按 `bin.sha256` 校验下载的静态二进制（Linux x86_64 / aarch64，musl），源码在仓库的 `crates/delegate/`，行为契约见仓库的 `docs/delegate-spec.md`。接入新的同事只需改 `agents.rs`（启动命令、续接方式、事件解析）。
