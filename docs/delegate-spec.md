# delegate 规格（v4.5）

> 本文是 `skills/delegate` 的**实现契约**：命令行、输出、run 目录、锁与状态机。它是 Rust 重写与 harness 原生接入的依据。
> 本文不在技能目录内，技能加载时不会读取；模型使用技能只需 `SKILL.md`。行为以本文为准，实现与本文不一致时按缺陷处理。
> 一致性验收：`tests/test_delegate.py` 以子进程黑盒方式驱动 CLI，任何实现都应通过（`DELEGATE_BIN` 指向被测可执行文件，见 §13）。

关键词：**必须**＝契约的一部分，改变即不兼容；**应**＝推荐行为，可在不破坏调用方的前提下调整。

---

## 1. 角色与术语

| 术语 | 含义 |
|---|---|
| 主控（caller） | 调用 CLI 的一方：人、主模型或另一个同事 |
| 同事（agent） | 被委派执行任务的 CLI：`pi` 或 `codex` |
| run | 一次委派的一轮执行，一个目录；`reply` 产生新的 run |
| 对话（conversation） | 由 `parent` 链起来的一串 run，以第一个 run 命名 |
| supervisor | 每个 run 一个的后台进程：启动同事、重跑、验收、写结论 |
| lane | 整机重任务队列，§8 |
| 快照（snapshot） | 工作区在某一时刻的 git tree 对象，§6 |

## 2. 命令行

入口是单个可执行文件：Python 实现 `skills/delegate/scripts/delegate.py`，Rust 实现 `crates/delegate`（`target/release/delegate`）。所有输出 JSON 的行都是单行、UTF-8、非 ASCII 字符不转义；**空白与字段顺序不属于契约**（两个实现不同），状态行以 `{"run"` 开头。读取方——包括读取 run 目录中 JSON 文件的 `agent-handoff`——必须按 JSON 解析或容忍任意空白，不得依赖文本格式。诊断信息写 stderr，以 `delegate: ` 开头。

### 2.1 退出码（必须）

| 码 | 含义 |
|---|---|
| 0 | 结局为 `delivered` / `answered`；或非等待类命令成功 |
| 1 | 其他结局；`apply` 有冲突或写了冲突标记；`result` 无答复 |
| 2 | 用法错误或被拒绝（参数非法、并发满、内存不足、嵌套委派、写入互斥） |
| 75 | `--max` 到期时仍有任务在运行 |
| 其他 | `lane` 原样返回命令的退出码；命令被信号 N 结束或 lane 自身收到信号 N 时为 128+N |

### 2.2 子命令

| 命令 | 参数 | 行为 |
|---|---|---|
| `start` | 启动选项（§2.3）＋任务说明 | 创建 run 并启动 supervisor，立即输出一行状态（§3.1）；stderr 提示收取命令 |
| `run` | 启动选项＋`--max`/`--progress`/`--full` | `start` 后等待，按 §3.2 输出 |
| `reply <run> [消息]` | `--fresh` `--accept` `--hide-accept` `--accept-timeout` `--timeout` `--image` `--name` `--prompt(-file)` ＋等待选项 | 续接对话（§7） |
| `wait [<run>...\|--all]` | `--max` `--no-result` `--full` `--progress` | 等待并输出；无参数（或 `--all`）取所有运行中或结果未读取的 run，没有时提示并以 0 退出 |
| `status [<run>...]`（别名 `list`） | | 每个 run 一行状态；无参数列出全部 |
| `result [<run>] [--path]` | | 输出完整答复（或其路径）；非运行中时标记已读取 |
| `diff [<run>] [--stat] [--total] [路径...]` | | `git diff` 该 run 前后快照；`--total` 自对话起点；终端下带颜色；对象被清理时回退输出 `changes.patch` |
| `apply [<run>] [--dry-run] [--merge]` | | §6.4 |
| `lane [--label 文字] [--] [命令...]` | | §8；无命令时列出队列，每行 `running\|queued  <since>  <label>` |
| `stop <run>...` | | §9.4 |
| `clean <run>...\|--finished [--force]` | | §10 |

缺少同事 CLI 时退出码 2：Codex 提示安装并登录；Pi 提示 `sh <仓库>/third_party/pi-kit/install.sh --additive`，其中 `<仓库>` 为从可执行文件真实路径逐级向上、首个包含该安装脚本的目录，找不到时给出远程安装命令。

`<run>` 解析顺序（必须）：含 `meta.json` 的目录路径 → `last`（最近启动的）→ 完整 id → 唯一子串；匹配数不为 1 即退出码 2。

### 2.3 启动选项

| 选项 | 默认 | 说明 |
|---|---|---|
| 任务说明 | — | 位置参数拼接、`--prompt`、或 `--prompt-file`（`-` 为 stdin）；为空即退出码 2 |
| `--tier cheap\|strong` | 只读 `cheap`，写入 `strong` | 按档位选同事，映射见 §12；与 `--agent` 互斥（退出码 2） |
| `--agent pi\|codex` | — | 直接指定同事，不属于任何档位，不升档 |
| `--name` | 说明首行 | 用于 run id 与显示；run id 取其 `[A-Za-z0-9._-]` 片段，最长 40 |
| `--workdir` | 当前目录 | 必须存在 |
| `--image <路径>` | — | 可重复；解析为绝对路径 |
| `--read-only` | 否 | §5 |
| `--in-place` | 否 | 仅与 `--read-only` 同用，与 `--worktree` 互斥 |
| `--worktree` | 否 | 需要 git 仓库 |
| `--accept <命令>` | — | shell 命令 |
| `--hide-accept` | 否 | 不在说明中附验收命令 |
| `--accept-timeout` | `10m` | 自取得 lane 名额起计 |
| `--timeout` | pi `15m`，codex `30m` | 每次尝试；§9.2 |
| `--retries N` | 1 | 0–3，答复畸形时的重跑次数 |
| `--provider` `--model` `--thinking` | — | 透传给同事 CLI |
| `--allow-parallel-writes` | 否 | 跳过写入互斥 |

时长格式：`^\d+(\.\d+)?[smhd]?$`，无单位为秒，必须 > 0。

## 3. 输出

### 3.1 状态行

`start`、`status`、`wait`/`run` 的结论都是一行 JSON 对象。字段（必须保持名称与语义；未出现的字段表示无值）：

| 字段 | 出现条件 | 含义 |
|---|---|---|
| `run` `name` `state` `agent` `mode` `dir` | 总是 | `mode` 为 `write` 或 `read-only`；`state` 见 §4 |
| `parent` | reply | 上一轮 run id |
| `worktree` | 在 worktree 中运行 | worktree 路径 |
| `elapsedSeconds` `turns` | 总是 | 运行中为实时值 |
| `last` `idleSeconds` | 运行中 | 最近一个动作及其距今秒数 |
| `attempts` `model` `tokens{input,output,cacheRead}` | 已结束 | |
| `files` | 有改动 | 相对路径列表 |
| `changes{files,added,deleted,after}` | 快照成功 | `after` 为结束快照 tree |
| `accept{command,ok,exitCode,tail?,queuedSeconds?}` | 执行过验收 | `tail` 为失败输出末 1500 字符 |
| `readOnlyViolation` | 只读 run 在自己的 worktree 中改了文件 | 文件列表 |
| `workspaceChanged` | `--in-place` 只读 run 期间工作区有变化 | 文件列表；无法归属 |
| `tier` | 按档位选的同事 | `cheap` / `strong`；升档后为 `strong` |
| `escalatedFrom` | 升过档 | 原先的同事 |
| `queuedSeconds` | 同事在 lane 中排队 ≥1 秒 | |
| `graceSeconds` | 同事用了超时宽限 | 超出 `--timeout` 的秒数 |
| `warning` `error` | | 人读文本 |
| `result` `resultChars` | 有非空答复 | `result.md` 路径与字符数 |
| `next` | 有建议动作 | 下一步（含可复制的命令），§3.3 |

### 3.2 结论块（`run` / `wait` / `reply`）

对每个 run 依次输出：状态行；已结束的再输出改动清单与答复：

```
{"run": ...}

===== changes: <run> (<N> files, +A -D[; worktree <path>]) =====
 M path  +1 -0
 A big.bin  large file
 M vendor  submodule contents
===== result: <run> (<字符数> chars) =====
<答复；超过 DELEGATE_RESULT_CHARS 只显示末尾，并注明全文路径>
===== end: <run> =====
```

- 改动清单最多列 40 项；写入 run 无改动时输出 `===== changes: <run>: none =====`；快照失败时输出 `unknown`；非 git 的写入 run 输出 `not tracked`。
- 只要打印了答复（或 run 无答复），即创建 `.delivered`。`--no-result` 只输出状态行，不标记已读取。
- 仍在运行的 run 只输出状态行；此时 stderr 提示再次 `wait`，退出码 75。

### 3.3 `next`（应）

| 情形 | 建议 |
|---|---|
| 运行中 | `wait <run>` |
| 完成、写入、原地、有改动 | 查看 `diff <run>`（改动已在工作区） |
| 完成、写入、worktree、有改动、未 apply | 查看 `diff <run> --total`，再 `apply <run>` |
| 完成、只读或无改动或已 apply | 无 |
| `rejected` | 读 `accept.tail`；`reply <run> '<要修的>'` 或接手 |
| `malformed` / `failed` / `timeout` / `killed` / `crashed` | 换同事 / 读 error / 拆小 / 接手 |

## 4. 状态机

```
starting ──supervisor 写 pid──▶ running ──▶ delivered | answered | rejected | malformed
    │                              │          | failed | timeout | killed | stopped
    └─15 s 仍无 supervisor──▶ crashed ◀─supervisor 消失且无 exit_code
```

| state | 判定 |
|---|---|
| `starting` | 无 `exit_code`、无 `pid`，启动不足 15 秒 |
| `running` | 无 `exit_code`，`pid` 对应进程存活且命令行含 `_supervise` |
| `crashed` | 无 `exit_code`，且上述均不满足 |
| 结束态 | 有 `exit_code`，取 `summary.json` 的 `state` |

### 4.1 选同事与升档（必须）

- 未给 `--agent` 时按档位选：`cheap` → `DELEGATE_CHEAP_AGENT`（默认 `pi`），`strong` → `DELEGATE_STRONG_AGENT`（默认 `codex`），值必须是 `pi`/`codex`。隐式的 `cheap` 档同事未安装、而强档已安装时，改用强档并在 stderr 说明。`meta.json` 与状态行记 `tier`（直接指定时无此字段）。
- **升档**：`tier == "cheap"` 的 run 结局为 `malformed`/`failed`/`timeout`/`rejected`、未被 stop、且（只读，或快照成功且没有任何改动）时，若强档已安装并在整机容量内（在 `<state>/.start.lock` 下检查），在同一 run 中换强档再跑一轮：事件 `escalate{from,to,after}`，`meta.json` 的 `agent` 改为强档、`tier` 改为 `strong`、`fork` 清空、记 `escalatedFrom`；只读 run 升到 Codex 时在 `prompt.md` 末尾补只读约定（§7.3）。新一轮沿用重跑次数，`attempts` 接续计数；快照、只读核对与验收按新一轮重新进行，结论带 `escalatedFrom`。容量不足时记事件 `escalate_skipped`，不升档。每个 run 至多升档一次。
- reply 沿用上一轮的 `agent` 与 `tier`。

### 4.2 判定

同事一次尝试的判定（必须）：被 stop → `stopped`；超时 → `timeout`；非零退出 → 退出码为 -9/137 时 `killed`，否则 `failed`；最后一轮 `stopReason == "stop"` 且已 settled 时，答复为空或末尾是泄漏的工具调用（正则 `\bcall:[\w.-]+(?::[\w-]+)?\{`，且以 `}` 结尾）→ `malformed`，否则 `ok`；其余 → `failed`。`malformed` 最多重跑 `--retries` 次。

`ok` 之后：`--accept` 存在时执行验收，退出码 0 → `delivered`，否则 `rejected`；无验收 → `answered`。**只读 run 改了文件不改变 state**（§5）。`exit_code` 文件内容为 `0`（delivered/answered）或 `1`。

## 5. 只读

- Pi：以 `--tools read,grep,find,ls` 启动，没有写工具，也没有 shell。
- Codex：以 full access 启动；任务说明末尾附只读约定（§7.3），结束后按快照核对。
- 在 git 仓库中，只读 run **默认在 worktree 中运行**（§6.3），`--in-place` 除外。只读 worktree 的 HEAD 重置为主控的 HEAD（mixed），未提交改动在 `git diff HEAD` / `git status` 中可见。只读 Pi 的 worktree 不执行 `setup`。
- 核对：结束快照与起始快照有差异时，worktree 中记 `readOnlyViolation`（改动留在 worktree，不可 `apply`），原地记 `workspaceChanged`；两者都附 `warning`，state 仍为 `answered`/`delivered`。
- 非 git 目录无法隔离与核对。

## 6. 快照、改动与 worktree

### 6.1 快照（必须）

在 git 顶层 `top` 上：把真实 index 以**保留 mtime** 的方式复制为临时 index（保留 mtime 才能让 git 识别 racy-clean 条目），用 `GIT_INDEX_FILE` 指向它执行 `git add -A -- . <排除>` 与 `git write-tree`。真实 index、refs、stash 不受影响。

- 被忽略的文件不计。
- 大于 `DELEGATE_SNAPSHOT_MAX_BYTES`（默认 2 MiB）的未跟踪文件不入对象库，记为 `large{path: [size, mtime_ns]}`。
- `.delegate.json` 的 `copy`/`link` 路径排除在外。
- 子模块记指纹：`HEAD`、`diff --binary HEAD`、未跟踪文件列表及其 size:mtime 的 SHA-1。
- 结果：`{"tree", "large", "submodules"}`。失败时结论带 `warning`，改动为 unknown。

### 6.2 改动

起始快照在创建 run 时取，结束快照在同事结束后、验收之前取（验收副产物不计）。改动 = 两个 tree 的 `diff --numstat/--name-status --no-renames`，加上 `large` 与子模块指纹的差异（子模块列为 `submodule contents`）。写入 `changes.json`（`base`、`after`、`top`、`afterLarge`、`changes[{path,status A|M|D,added,deleted,large?,submodule?}]`）与 `changes.patch`（`git diff --binary`）。非 git 目录退回到编辑事件中的路径。

### 6.3 worktree

- 路径：`${XDG_CACHE_HOME:-~/.cache}/delegate/worktrees/<仓库名>-<run id>`。
- 起点：`commit-tree <起始快照> [-p HEAD]`（作者 `delegate <delegate@localhost>`），`git worktree add --detach`。
- `--workdir` 为子目录时，同事在 worktree 的对应子目录工作。
- 准备：`copy`（复制）、`link`（符号链接，替换空目录，适合子模块）、`setup`（在 lane 中依次执行，§8）。任一步失败 → `failed`，`error` 说明。
- 一个对话共享一个 worktree；`clean` 删除最后一个引用它的 run 时 `git worktree remove --force`。

### 6.4 apply（必须）

把对话起点快照（`chainBase`）到 **worktree 当前状态**（重新快照；worktree 已删除时用最后一轮记录）的改动合并回源工作区，不动 index：

| 源文件现状 | 动作 |
|---|---|
| 已等于目标 | 跳过 |
| 等于起点 | 直接写入（含权限位与符号链接）或删除 |
| 三方都是文本 | `git merge-file`；无冲突写入，有冲突时默认中止，`--merge` 写冲突标记 |
| 二进制、符号链接、类型变化、删改冲突、经符号链接目录 | 冲突 |
| 大文件 | 从 worktree 复制；源中已存在且不同即冲突 |

有冲突且无 `--merge` 时**什么都不写**，退出码 1。完全成功时对话内所有 run 及共用该 worktree 的旁支 run 写 `.applied`。只读 run 与原地 run 拒绝 `apply`（退出码 2）。

## 7. 会话与任务说明

### 7.1 同事调用（必须）

Pi：
```
pi (--session-id <uuid> | --fork <上一轮会话副本>) --session-dir <run>/session --mode json
   [--provider P] [--model M] [--thinking T] [--tools read,grep,find,ls] -p [@<图片>...]   # stdin: prompt.md
```
Codex：
```
codex exec [fork <会话 id>] --json --skip-git-repo-check [-C <workdir>] --dangerously-bypass-approvals-and-sandbox
   [-m M] [-c model_reasoning_effort="T"] [-c model_provider="P"] [--image=<图片>...] -     # stdin: prompt.md
```
进程工作目录为 workdir，独立进程组。环境：去掉 §12 的保护变量，叠加 `.delegate.json` 的 `env`，再设 `DELEGATE_AGENT`、`DELEGATE_RUN_DIR`（Pi 另设 `PI_DELEGATE_ACTIVE=1`）。

### 7.2 事件

同事的 JSON 流被过滤为 `events.jsonl` 的紧凑事件，每条带 `attempt` 与 `at`（UTC，`%Y-%m-%dT%H:%M:%SZ`）：`session{id}`、`bash{cmd}`、`bash_done{ok}`、`read|edit|write{path}`、`tool{tool,arg}`、`tool_error{tool,detail}`、`turn{provider,model,stopReason,usage}`、`result{text}`（Pi）、`message{text}`（Codex）、`turn_error{detail}`、`warning{detail}`、`retry`、`rerun`、`compaction_start|end`、`settled`、`queue{ahead}`（验收排队）。不记录编辑全文。

### 7.3 附加约定（必须保持语义）

任务说明为中文（含 CJK 字符）时用中文附言，否则英文。以 `\n\n---\n` 与原文分隔：
- 验收命令（未隐藏时）：说明"结束后委派方在工作目录运行此命令，退出码 0 视为完成"，附代码块，并提示自检时用 `<入口> lane <命令>` 排队、排队时间不计时。
- Codex 只读：不得创建、修改、删除文件；改动会被报告且不被采纳；委派记录不算改动。
- reply 更改或取消验收命令时，说明旧标准不再适用。

`prompt.md` 保存同事实际收到的全文。

### 7.4 reply（必须）

接在对话**最新一轮**之后（结局为 `malformed` 的轮次跳过）；同一 agent、workdir、worktree、mode、env、provider/model/thinking、retries。每次尝试都从上一轮会话**分叉**（Pi 用会话文件副本 `--fork`，Codex `exec fork`），上一轮会话永不改动。`--fresh` 开新会话但留在同一对话与 worktree。同一轮已有进行中的 reply 时拒绝。

## 8. lane：整机重任务队列（必须）

- 同时放行 `DELEGATE_MAX_HEAVY`（默认 1，0 不限）个；先到先得。
- 进入者：验收命令、worktree `setup`、`lane` 子命令（同事自检与主控检查）。
- 票据：`<state>/lane/<time_ns 20 位>-<pid>.ticket`，内容 `{pid,label,since,epoch}`。在 `<state>/lane/.lock` 的排他锁下创建，保证到达顺序；以临时名创建、加排他 flock、再 rename，持有者在等待与执行期间一直持锁。
- 存活 = 其 flock 被持有（`LOCK_SH|LOCK_NB` 失败）。无人持锁的票据视为死亡，任何人看到即删除。
- 等待：位置 < 上限即放行；否则对"位于 位置−上限 的票据"加共享 flock 阻塞，醒来后重新计算。**不得轮询。**
- 取消：等待中的 supervisor 收到 stop 信号时放弃排队；信号先于阻塞到达也不得漏掉（先置等待标记，再检查停止标志，再阻塞）。取得名额后若已被 stop，不得执行验收。
- 重入：名额内的命令带 `DELEGATE_LANE_HELD=1`，其中再调用 lane 直接执行。该变量不得传给 supervisor、同事或新的 run。
- 排队不计时：验收与 setup 的超时从取得名额起算；同事经 `lane` 排队时，在 `<run>/lane-waiting-<pid>`（持锁标记）与 `<run>/lane-wait`（每行一次秒数）中记账，从其 `--timeout` 中扣除。
- `lane` 子命令：命令在独立进程组中运行；收到 SIGTERM/SIGINT/SIGHUP 时先结束整个命令组（3 秒后 SIGKILL），再以 128+信号退出；不得在信号处理函数中执行不可重入的等待。

## 9. 进程、超时与停止

### 9.1 验收与 setup 命令（必须）

`sh -c` 执行，独立进程组，stdin 为 `/dev/null`，stdout+stderr 写日志。环境：去掉保护变量、叠加 `env`、设 `DELEGATE_LANE_HELD=1`。命令结束或超时后，**在回收 shell 之前**结束整个进程组（SIGTERM，宽限后 SIGKILL，存活判断排除僵尸），避免后台残留，且 PGID 不会被复用。超时退出码记 124，日志追加 `[accept timed out]` 与 `[exit N]`。

### 9.2 同事超时（必须）

- 计时用单调时钟，扣除 lane 排队时间。
- 到 `--timeout` 时：若有命令正在执行（`bash` 多于 `bash_done`）或最近 120 秒内有事件，继续；最多到 `--timeout × (1 + DELEGATE_TIMEOUT_GRACE/100)`（默认 1.5 倍）。超出即结束同事进程组，判 `timeout`。
- 应只在判定可能变化的时刻醒来。

### 9.3 supervisor

由入口以隐藏子命令 `_supervise <run 目录>` 启动（`running` 的判定依赖命令行含 `_supervise`），脱离会话（`start_new_session`），不继承 `DELEGATE_LANE_HELD`；启动后先持有 `<run>/supervisor.lock` 的排他 flock（临时名加锁后 rename）直到进程退出，再写 `pid`。启动方最多等 5 秒 `pid` 出现，否则写 `crashed`。无论何种异常都必须写出 `summary.json` 与 `exit_code`。

### 9.4 stop

向 supervisor 发 SIGTERM；supervisor 结束同事或验收的进程组，排队中则放弃排队。12 秒内无 `exit_code` 时由 `stop` 直接结束残余进程组并写 `stopped`。supervisor 已死而同事仍在时，直接结束同事进程组。

### 9.5 等待（`wait`/`run`）

阻塞在各 run 的 `supervisor.lock` 共享锁上（每个一个线程），`--max` 到期即返回。仅在 `--progress`、supervisor 尚未加锁（刚启动）或 4.4 之前的 run 时按 `DELEGATE_POLL`（默认 1 秒）轮询。

## 10. 并发、准入与清理

- 整机并发：`<state>/<sha1(run 路径)[:16]>.slot` 记录运行中的 run；`DELEGATE_MAX_ACTIVE`（默认 6）、`DELEGATE_MAX_CODEX`（默认 3），0 不限。超出即拒绝并列出运行中的任务。
- 内存准入：`/proc/meminfo` 的 `MemAvailable` 低于 `DELEGATE_MIN_AVAILABLE_MB`（默认 4096，0 不查）时拒绝。
- 启动在 `<state>/.start.lock` 与 `<runs>/.start.lock` 两把锁下进行，检查与登记不可交错。
- 写入互斥：同一 workdir 同时只允许一个原地写入 run（`--allow-parallel-writes` 与 worktree run 除外）。
- **只有一层委派**：调用者本身是同事（设有 `DELEGATE_AGENT`、`PI_DELEGATE_AGENT` 或 `PI_DELEGATE_ACTIVE`）时，`start`/`run`/`reply` 一律以退出码 2 拒绝；`lane` 等其他命令不受影响。所有结果都回到主控。
- 自动清理：每次启动删除结束超过 `DELEGATE_KEEP_DAYS`（默认 7，0 关闭）天、已读取、且没有未 apply 写入 worktree 的 run。
- `clean`：跳过运行中的与同事进程仍存活的；`--finished` 默认保留未读取的（`--force` 除外）；提示未 apply 的 worktree。

## 11. 文件与目录

run 根目录：`DELEGATE_RUNS`，否则为**调用时当前目录**所在 git 根下的 `.local/run/pi/`（不在仓库中则为当前目录下），首次创建时写入内容为 `*` 的 `.gitignore`；run 目录 `<YYYYmmdd-HHMMSS>-<slug>[-<4 hex>]`，权限 700。`<state>` 为 `${XDG_STATE_HOME:-~/.local/state}/delegate`，**不得**提供环境变量覆盖。

| 文件 | 写入者 | 内容 |
|---|---|---|
| `meta.json` | 启动方（升档时 supervisor 更新） | run、dir、workdir、mode、agent、tier、escalatedFrom、name、provider、model、thinking、timeout(Seconds)、accept、acceptTimeoutSeconds、retries、images、top、base、snapshotExclude、worktree{source,sourceWorkdir,config,path}、env、chainBase、sessionDir、parent、fork、startedAt/Epoch/Ns |
| `prompt.md` | 启动方 | 同事收到的全文 |
| `supervisor.lock` / `pid` / `agent.pid` | supervisor | 生命周期锁 / supervisor pid / 同事进程组 |
| `events.jsonl` `stderr.log` `supervisor.log` | supervisor | §7.2 / 同事 stderr / supervisor 输出 |
| `result.md` | supervisor | 最后一轮完整答复 |
| `summary.json` | supervisor | §3.1 中结束后的字段 |
| `changes.json` `changes.patch` | supervisor | §6.2 |
| `accept.log` `setup.log` | supervisor | 命令、输出、`[exit N]` |
| `lane-wait` `lane-waiting-<pid>` | lane | §8 |
| `session/` `fork/` | 同事 / 启动方 | Pi 会话；reply 所用的上一轮会话副本 |
| `exit_code` | supervisor | 结束标记，**最后写** |
| `.delivered` `.applied` `.progress` | 读取方 | 已读取 / 已合并 / `--progress` 进度 |

`agent-handoff` 依赖 `meta.json`、`exit_code`、`.delivered`、`.applied` 与 `meta.json` 中的 `worktree.path`、`mode`；改变其含义须同步修改。

## 12. 配置与环境变量

`.delegate.json`（仓库根）：

```json
{"env": {"CUDA_VISIBLE_DEVICES": ""},
 "worktree": {"copy": [".env"], "link": ["third_party/sub"], "setup": ["pnpm install --offline --frozen-lockfile"]}}
```

`env` 为字符串到字符串的映射，注入同事、验收与 setup（原地与 worktree 均生效，reply 沿用）；`copy`/`link` 必须是仓库内相对路径。

环境变量均先读 `DELEGATE_<名>`，再读 `PI_DELEGATE_<名>`：`RUNS`、`CHEAP_AGENT`（pi）、`STRONG_AGENT`（codex）、`MAX_ACTIVE`、`MAX_CODEX`、`MAX_HEAVY`、`MIN_AVAILABLE_MB`、`TIMEOUT_GRACE`、`RESULT_CHARS`（6000）、`KEEP_DAYS`、`POLL`、`SNAPSHOT_MAX_BYTES`、`SETUP_TIMEOUT`（10m）。由实现导出、调用方不应设置的保护变量：`DELEGATE_AGENT`、`DELEGATE_RUN_DIR`、`DELEGATE_LANE_HELD`、`PI_DELEGATE_ACTIVE`、`PI_DELEGATE_AGENT`、`PI_DELEGATE_PARENT_RUN`。

## 13. 一致性验收

`tests/test_delegate.py` 是纯黑盒套件：通过伪造的 `pi`/`codex`（写在临时 `PATH` 中的 shell 脚本）以子进程驱动 CLI，只观察输出、退出码与 run 目录中的文件，覆盖本文全部必须项。被测入口由 `DELEGATE_BIN` 指定（默认 Python 实现 `skills/delegate/scripts/delegate.py`）：

```bash
DELEGATE_BIN=<被测可执行文件> python3 -m unittest tests.test_delegate
```

任何实现都必须在不修改该套件的前提下全部通过。
