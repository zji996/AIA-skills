# delegate 规格（v5.12.0）

> 本文是 `skills/delegate` 的**实现契约**：命令行、输出、run 目录、锁与状态机。它是 Rust 重写与 harness 原生接入的依据。
> 本文不在技能目录内，技能加载时不会读取；模型使用技能只需 `SKILL.md`。行为以本文为准，实现与本文不一致时按缺陷处理。
> 一致性验收：`tests/test_delegate.py` 以子进程黑盒方式驱动 CLI，任何实现都应通过（`DELEGATE_BIN` 指向被测可执行文件，默认 `skills/delegate/bin/delegate`，见 §13）。

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

入口是单个可执行文件：`crates/delegate` 构建的静态二进制，安装在 `skills/delegate/bin/delegate`（最后一个 Python 实现见 tag `delegate-py-4.5.0`）。所有输出 JSON 的行都是单行、UTF-8、非 ASCII 字符不转义；**空白与字段顺序不属于契约**（两个实现不同），状态行以 `{"run"` 开头。读取方——包括读取 run 目录中 JSON 文件的 `agent-handoff`——必须按 JSON 解析或容忍任意空白，不得依赖文本格式。诊断信息写 stderr，以 `delegate: ` 开头。

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
| `reply <run> [消息]` | `--fresh` `--sync` `--accept` `--hide-accept` `--accept-timeout` `--timeout` `--image` `--name` `--prompt(-file)`；`--wait` 时可用等待选项 | 续接对话并立即返回启动状态；`--wait` 等结论（§7） |
| `wait [<run>...\|--all]` | `--max` `--no-result` `--full` `--progress` `--any` `--stream` | 等待并输出；无参数按派发会话收取（§9.5），`--all` 取本仓库所有运行中或结果未读取的 run；没有时以 0 退出。`--any` 与 `--stream` 见 §9.5 |
| `status [<run>...]`（别名 `list`） | | 每个 run 一行状态；无参数列出全部 |
| `result [<run>] [--path]` | | 输出完整答复（或其路径）；非运行中时标记已读取 |
| `diff [<run>] [--stat] [--total] [路径...]` | | `git diff` 该 run 前后快照；`--total` 自对话起点；终端下带颜色；对象被清理时回退输出 `changes.patch` |
| `apply [<run>] [--dry-run] [--merge]` | | §6.4 |
| `lane [--label 文字] [--] [命令...]` | | §8；无命令时列出队列，每行 `running\|queued  <since>  <label>` |
| `stop <run>...` | | §9.4 |
| `clean <run>...\|--finished [--force]` | | §10 |
| `protocol` | | 输出一行 JSON：`protocol`（宿主接入协议版本，当前 1）、`version`、`caller{id,source}`（未知时为 `null`）、`agents[]{name,tiers,available,bin,version,shadowed}`；`tiers` 是按当前配置（`DELEGATE_{CHEAP,STRONG}_AGENT`）该同事担任默认的档位列表，可为空，`bin` 为 PATH 上首个可执行文件的真实路径（不可用时为 `null`），`shadowed` 为 PATH 中更靠后、真实路径不同的同名可执行文件，按其在 PATH 中的路径列出（snap 等启动器都解析到同一文件，解析后看不出是谁）；退出码 0。供宿主适配器探测，见 [delegate-protocol.md](delegate-protocol.md) |

缺少同事 CLI 时退出码 2：Codex 提示安装并登录；Pi 提示 `sh <仓库>/third_party/pi-kit/install.sh --additive`，其中 `<仓库>` 为从可执行文件真实路径逐级向上、首个包含该安装脚本的目录，找不到时给出远程安装命令。

`<run>` 解析顺序（必须）：含 `meta.json` 的目录路径 → `last`（最近启动的）→ 完整 id → 唯一完全匹配的名称（`--name`）→ 唯一 id 子串；匹配数不为 1 即退出码 2。

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
| `--accept <命令>` | `.delegate.json` 顶层 `accept`（仅写入） | shell 命令；显式参数覆盖默认 |
| `--no-accept` | 否 | 禁用仓库默认验收；reply 中取消继承的验收 |
| `--hide-accept` | 否 | 不在说明中附验收命令 |
| `--accept-timeout` | `10m` | 自取得 lane 名额起计 |
| `--timeout` | pi `25m`，codex `50m` | 每次尝试；§9.2 |
| `--retries N` | 1 | 0–3，答复畸形时的重跑次数 |
| `--provider` `--model` `--thinking` | — | 透传给同事 CLI |
| `--allow-parallel-writes` | 否 | 跳过写入互斥 |
| `--after <run>` | — | 立即创建 run（state `waiting`），上游以 delivered/answered 结束后才执行；其他结局使本 run 以 `skipped` 结束，`error` 写明上游名称与结局，exit_code 为 1 |
| `--in <run>` | — | 仅与 `--read-only` 同用；在上游 worktree 当前状态的快照中运行（§6.3）；未给 `--after` 时上游必须已结束 |
| `--protect <路径>` | — | 可重复；路径相对仓库根，末尾 `/` 表示目录前缀，否则精确匹配文件；需要 git 仓库。reply 继承上一轮设置，不可覆盖 |
| `--protect-reason <路径> <原因>` | — | 可重复，两个独立参数，同时保护路径并附原因；同一归一化路径的完全重复去重、不同原因或空白原因拒绝。reply 不可覆盖 |

时长格式：`^\d+(\.\d+)?[smhd]?$`，无单位为秒，必须 > 0。

## 3. 输出

### 3.1 状态行

`start`、`status`、`wait`/`run` 的结论都是一行 JSON 对象。字段（必须保持名称与语义；未出现的字段表示无值）：

| 字段 | 出现条件 | 含义 |
|---|---|---|
| `run` `name` `state` `agent` `mode` `dir` | 总是 | `mode` 为 `write` 或 `read-only`；`state` 见 §4 |
| `finishedAt` | 已写终态的新 run | UTC 结束时刻，紧邻 `state` 输出；旧 summary 缺失或运行时推断 `crashed` 时省略，不能以启动/读取时间代替 |
| `completionTiming` | `wait --any/--stream` 的结束结论 | `already-finished`：首次状态检查时已结束；`finished-during-wait`：等待期间完成（stream 后加入的任务也用此值）。不是 shell 调用开始时刻边界，不改变收取顺序 |
| `agentBin` | 已解析同事程序 | 当前同事实际执行文件的绝对路径；升档后更新 |
| `parent` | reply | 上一轮 run id |
| `after` | 使用 `--after` | 上游 run id |
| `worktree` | 在 worktree 中运行 | worktree 路径 |
| `ageSeconds` | meta 有 `startedEpoch` | 从启动到当前的非负整秒；`status` 每行均按此计算 |
| `elapsedSeconds` `turns` | 总是 | 运行中为实时值 |
| `last` `idleSeconds` | 运行中 | 最近一个动作及其距今秒数 |
| `attempts` `model` `tokens{input,output,cacheRead}` | 已结束 | |
| `files` | 有改动 | 相对路径列表 |
| `changes{files,added,deleted,after}` | 快照成功 | `after` 为结束快照 tree |
| `pendingChanges{files,added,deleted}` | worktree 写入、累计 diff 成功 | 自最近 apply/sync 基准（无则 chainBase）至本轮记录的累计待合入量；`changes` 仍是本轮语义，零改动 reply 仍据此给 diff --total / apply 建议 |
| `shape{dirs,largest,config,removed,*More?}` | 写入任务、快照有改动 | `dirs` 按前两级目录汇总增删行；`largest` 为改后文本文件总行数；`config` 为依赖清单、锁文件、构建与 CI 配置路径；`removed` 为删除路径。各列表默认最多 5 项，`DELEGATE_SHAPE_LIMIT` 可调（1–20），`dirsMore` 等字段为未显示项数。`changes` 仍表示整体总数，完整逐文件 diff 见 `changes.json` |
| `accept{command,ok,exitCode,tail?,queuedSeconds?,tree?,treeAfter?,snapshotComplete?,snapshotReason?}` | 执行过验收 | `tail` 为失败输出末 1500 字符；取得 lane 后、执行前记录 tree，命令及进程清理后记录 treeAfter，完整证据条件见 §6.4 |
| `cleanup{terminated,ports,commands}` | 结束时清理过后台进程 | 结束时仍存活且被终止的进程数、监听 TCP 端口及带 PID 的截短命令；PID 去重 |
| `warnings` | worktree 的 link/copy 源缺失或为空 | 警告字符串数组；`wait` 也显示 |
| `readOnlyViolation` | 只读 run 在自己的 worktree 中改了文件 | 文件列表 |
| `workspaceChanged` | `--in-place` 只读 run 期间工作区有变化 | 文件列表；无法归属 |
| `protectViolation` | 改动命中受保护路径 | 排序后的仓库相对路径列表 |
| `protectViolationReasons` | 违规且设置了原因 | 命中的保护规则路径到原因的映射，不改变 protectViolation 字符串数组 |
| `tier` | 按档位选的同事 | `cheap` / `strong`；升档后为 `strong` |
| `escalatedFrom` | 升过档 | 原先的同事 |
| `queuedSeconds` | 同事在 lane 中排队 ≥1 秒 | |
| `graceSeconds` | 同事用了超时宽限 | 超出 `--timeout` 的秒数 |
| `warning` `error` | | 人读文本 |
| `result` `resultChars` | 有非空答复 | `result.md` 路径与字符数 |
| `sourceDrift{files,overlap,overlapMore?}` | 结论块或 `wait --stream` 中已结束、未 apply、有改动的 worktree 写入 run，且源工作区自快照（或上次 apply / `--sync`）以来有变化 | `files` 为源工作区变化的文件数；`overlap` 为其中同事也改过的文件（最多 10 项）。`status` 不计算，保持轻量 |
| `report` | `wait --stream` 行、有答复且未送达 | 读答复的命令 `wait <run>` |
| `next` | 有建议动作 | 下一步（含可复制的命令），§3.3 |

`apply` 保留逐文件清单，在末尾追加一行以 `{"run"` 开头的 JSON：`run`、`operation: "apply"`、`apply{ok,dryRun}`、可选 `acceptStillValid`、`acceptValidityScope: "repository-snapshot"`、`acceptValidityReason`、可选 `numberedPrefixConflicts[{directory,prefix,paths}]`。不修改任务 state 或将应用成功变为 delivered。

### 3.2 结论块（`run` / `wait` / `reply`）

对每个 run 依次输出：状态行；已结束的再输出改动清单与答复：

```
{"run": ...}

===== changes: <run> (<N> files, +A -D[; worktree <path>]) =====
 M path  +1 -0
 A big.bin  large file
 M vendor  submodule contents
===== shape =====
 areas: apps/api +12 -3
 largest after: apps/api/server.rs (240 lines)
 config: Cargo.toml
 removed: apps/api/old.rs
===== result: <run> (<字符数> chars) =====
<答复；超过 DELEGATE_RESULT_CHARS 显示开头约 2/3 与结尾约 1/3，并注明省略字数及全文路径>
===== end: <run> =====
```

- 改动清单最多列 40 项；有 `shape` 时在清单后输出简短 shape 小节；写入 run 无改动时输出 `===== changes: <run>: none =====`；快照失败时输出 `unknown`；非 git 的写入 run 输出 `not tracked`。
- 只要打印了答复（或 run 无答复），即创建 `.delivered`。`--no-result` 只输出状态行，不标记已读取。
- 仍在运行的 run 只输出状态行；此时 stderr 提示再次 `wait`，退出码 75。

### 3.3 `next`（应）

| 情形 | 建议 |
|---|---|
| 运行中 | `wait <run>` |
| 完成、写入、原地、有改动 | 查看 `diff <run>`（改动已在工作区） |
| 完成、写入、worktree、有改动、未 apply | 查看 `diff <run> --total`，再 `apply <run>` |
| 同上且 `sourceDrift.overlap` 非空 | 写明重叠文件数；`apply`（冲突即停，`--merge` 写冲突标记），或先 `reply <run> --sync` 让同事在最新源上收尾 |
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

### 4.1.1 编排（`--after`、`--in`，必须）

- 带 `--after` 的 supervisor 阻塞在上游的 `supervisor.lock` 上（上游无生命周期锁时按 `DELEGATE_POLL` 兜底），等待期间不启动同事。上游成功后，任务说明末尾按说明语言追加上游名称、结局、`result.md` 路径，以及存在时的 `changes.patch` 与 worktree 路径。
- `waiting` 是未结束状态，`skipped` 是结束状态。waiting run 不占整机并发名额、也不参加写入互斥检查；离开等待时在 `<state>/.start.lock` 下重新检查容量，满额时等到任一运行中的 run 结束再检查。
- `--in` 在被审查 run 结束后，对其 worktree 当前状态做快照（排除上游的 `snapshotExclude`，即 link/copy 路径，它们保留起点 tree 中的原始条目）并新建独立 worktree：HEAD 设为上游 worktree 的 HEAD，index 保持快照，使 `git diff HEAD` 显示上游的改动；workdir 映射到新 worktree 的对应目录，copy/link/setup 照常。只读违规只记在本 run，绝不改动上游 worktree。
- 这不是嵌套：每一步都由主控声明，结果都回到主控。

### 4.2 判定

同事一次尝试的判定（必须）：被 stop → `stopped`；超时 → `timeout`；非零退出 → 退出码为 -9/137 时 `killed`，否则 `failed`；最后一轮 `stopReason == "stop"` 且已 settled 时，答复为空或末尾是泄漏的工具调用（正则 `\bcall:[\w.-]+(?::[\w-]+)?\{`，且以 `}` 结尾）→ `malformed`，否则 `ok`；其余 → `failed`。`malformed` 最多重跑 `--retries` 次。

`ok` 之后：`--accept` 存在时执行验收，退出码 0 → `delivered`，否则 `rejected`；无验收 → `answered`。**只读 run 改了文件不改变 state**（§5）。`exit_code` 文件内容为 `0`（delivered/answered）或 `1`。

## 5. 只读

- **能隔离时**（在自己的 worktree 中，git 仓库的默认情况）：Pi 与 Codex 同等对待——全部工具（可运行 `git log`/`git diff` 与测试），任务说明末尾附只读约定（§7.3），结束后按快照核对。隔离与核对已兜底，工具限制不再必要。
- **无法隔离时**（非 git 目录或 `--in-place`）：Pi 以 `--tools read,grep,find,ls` 启动（没有写工具，也没有 shell），不附约定；Codex 仍靠约定与（git 中的）事后核对。
- Codex：以 full access 启动；任务说明末尾附只读约定（§7.3），结束后按快照核对。
- 在 git 仓库中，只读 run **默认在 worktree 中运行**（§6.3），`--in-place` 除外。只读 worktree 的 HEAD 移到主控的 HEAD，**index 保持快照内容**（`git reset --soft`）：主控的未提交改动（含子模块指针）以已暂存的形式在 `git diff HEAD` / `git status` 中可见，又不影响结束时的快照比对。只读 worktree 与写入 worktree 一样执行 `setup`。
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

子模块指纹保留原有 SHA-1 摘要，并记录检出的 `HEAD` 与是否干净。**仅在 worktree run 中**，起始快照没有该子模块的指纹、结束时子模块干净且 `HEAD` 等于结束快照 tree 中的 gitlink，视为仅初始化（同事为跑测试补齐了子模块）：不计入 `changes`/`files`，`apply` 不处理它。子模块内已跟踪或未跟踪文件有改动、或检出提交不同，照常计为改动。

**受保护路径**：`--protect` / `--protect-reason` 使用相同归一化规则，路径排序去重后记入 `meta.json` 的 `protect` 字符串数组，原因另存 `protectReasons` 映射，任务说明末尾逐项附原因与约定（§7.3）。结束快照后若改动命中受保护路径，state 为 `rejected`，带 `protectViolation` 与可选的 `protectViolationReasons`，`error` 与 `next` 提示受保护路径被改动，不执行 `--accept`。这样的便宜档 run 有改动，不升档；只读核对照常记录。语义绕路只能靠模型遵守约定，脚本只核对路径。

起始快照在创建 run 时取，结束快照在同事结束后、验收之前取（验收副产物不计）。改动 = 两个 tree 的 `diff --numstat/--name-status --no-renames`，加上 `large` 与子模块指纹的差异（子模块列为 `submodule contents`）。写入 `changes.json`（`base`、`after`、`top`、`afterLarge`、`changes[{path,status A|M|D,added,deleted,large?,submodule?}]`）与 `changes.patch`（`git diff --binary`）。非 git 目录退回到编辑事件中的路径。

### 6.3 worktree

- 路径：`${XDG_CACHE_HOME:-~/.cache}/delegate/worktrees/<仓库名>-<run id>`。
- 起点：`commit-tree <起始快照> [-p HEAD]`（作者 `delegate <delegate@localhost>`），`git worktree add --detach`。
- `--workdir` 为子目录时，同事在 worktree 的对应子目录工作。
- 准备：`copy`（递归复制仓库根相对的文件或目录，包括被 git 忽略的路径，如 `.local/scan`）、`link`（符号链接，替换空目录，适合子模块）、`setup`（在 lane 中依次执行，§8）、`writeSetup`（仅写入任务，在 `setup` 之后执行，适合只有构建和测试才需要的重环境）。`copy`/`link` 源不存在时跳过并向 stderr 提示；其余失败 → `failed`，`error` 说明。
- 一个对话共享一个 worktree；`clean` 删除最后一个引用它的 run 时，在 worktree 自身 `git-common-dir` 所属的仓库执行 `git worktree remove --force`（失败则删目录后 `worktree prune`）；不依赖记录的来源路径，`--in` 的上游可能已先被清理。

### 6.3.1 并行写入提示（5.13）

`start`/`run` 创建写入任务后，对同一源仓库（worktree 的 `source` 或原地 `top`）上仍在运行的其他写入任务各输出一行 stderr：其当前 worktree 相对 `chainBase` 的改动文件数与前 5 个路径。本任务说明原文包含其中某个路径（完整相对路径，或长度 ≥ 8、含 `.` 且非 `mod.rs`/`index.ts` 等通用名的文件名）时该行以 `overlap:` 开头并建议 `--after` 或 `--protect`，否则以 `note:` 开头。只提示，不拒绝、不改退出码；只读任务不提示。

### 6.4 apply（必须）

把对话起点快照（`chainBase`）到 **worktree 当前状态**（重新快照；worktree 已删除时用最后一轮记录）的改动合并回源工作区，不动 index：

| 源文件现状 | 动作 |
|---|---|
| 已等于目标 | 跳过 |
| 等于起点 | 直接写入（含权限位与符号链接）或删除 |
| 三方都是文本 | `git merge-file`；无冲突写入，有冲突时默认中止，`--merge` 写冲突标记 |
| 二进制、符号链接、类型变化、删改冲突、经符号链接目录 | 冲突 |
| 大文件 | 从 worktree 复制；源中已存在且不同即冲突 |

有冲突且无 `--merge` 时**什么都不写**，退出码 1。

**分轮合并**：完整合入的 `apply` 在各 run 的 `.applied` 中记录本次合入的 worktree tree、大文件指纹与内容摘要；之后的 reply 在 `meta.json` 的 `appliedBase` 中继承它。再次 `apply` 以最近一次完整合入的状态为基准，没有记录时用 `chainBase`。写了冲突标记也推进基准并记为已合并（仍退出 1）；有跳过项或 `--dry-run` 时不推进。`diff --total` 仍展示整段对话；没有待合入的改动且没有待生成标记时输出 `no changes to apply`，退出码 0。

删除、复制、写入或重新生成期间任何文件操作失败，`apply` 在 stderr 报告文件路径和原因，以非 0 退出，不写新的 `.applied` 或 `.sync-base`。此前已成功写入的文件保留，worktree 保留；下一次 `apply` 仍从最近一次完整成功的基准重试。冲突且未指定 `--merge` 时保持上述预检语义，源工作区不写入。

**生成文件**：`.delegate.json` 的 `"generated": {"paths": [...], "command": "..."}` 声明的路径（语义同 `--protect`）不做三方合并、也不覆盖；改动清单与 diff 仍如实列出。合并写入了文件后，通过 lane 在源仓库根以 `sh -c` 运行 `command` 重新生成，输出写入 run 目录的 `generate.log`；命令使用 §9.1 的限时执行和进程组回收机制，超时由 `DELEGATE_GENERATE_TIMEOUT` 控制（默认 10m）。`--dry-run` 只报告将会重新生成；命令失败或超时时退出码 1、显示日志末尾，已合并的文件保留，lane 名额释放。完全成功时对话内所有 run 及共用该 worktree 的旁支 run 写 `.applied`。只读 run 与原地 run 拒绝 `apply`（退出码 2）。

5.12 起，入口、排队回调、执行开始与结果写 stderr 并 flush：检查合并、生成器排队人数及前序名称、正在生成（日志绝对路径、执行超时，排队不计入）、完成/失败。慢 apply 可以后台执行。写普通文件前记录 `.generate-pending`，只有生成成功才清除；同 worktree 的后续 apply/reply 即使零文件动作也重试生成，失败不推进基准。待生成而配置被删除时拒绝假成功。

**编号预检**：写文件前，仅对本次基准的新增、实际计划写入文件，检查 ASCII 数字前缀加 `_`；按父目录＋原样数字前缀分组。比较源中已跟踪/未跟踪的同目录文件和本批其他新增文件，扣除计划删除，忽略同完整路径、修改项和生成路径，不跟随符号链接目录；不同目录互不冲突，`32` 与 `0032` 不合并。冲突写 stderr 警告并输出 `numberedPrefixConflicts`，dry-run 也报告；不改号、不改退出码，不扫描其他未合入 worktree。通用命名可能不是迁移编号，由主控判断；无源仓库应用锁，同仓库必须串行 apply，防止预检竞态。

**验收复用**：取得 lane 后、验收前及命令/进程清理后保存完整仓库快照证据（不沿用验收前 changes.after 或读取当前 worktree 代替历史）。apply 结束时对源工作区全树重新快照，含未提交改动与生成文件；HEAD 相同不足以证明有效。仅本轮 accept.ok 为 true、历史快照完整且 tree == treeAfter == 最终源 tree 时为 true。已知验收失败、验收改树或源 tree 不同为 false。dry-run、无验收、旧记录/缺字段、快照失败或证据不全时省略布尔值并说明原因；大型未跟踪文件、脏/缺失子模块属 tree 外输入，不能用 size/mtime 证明有效。索引 assume-unchanged / skip-worktree 标志使证据不完整；干净子模块递归核对快照和索引标志。忽略文件、环境、数据库、Git 历史仍由主控判断。apply 不重跑验收，结果仅表示此次 `repository-snapshot` 范围，后续源码改动或 reply 必须重新判断，不向 reply 链永久传播。

**合并后验收**（5.13）：`.delegate.json` 的 `applyVerify` 为 `true`（用顶层 `accept`，缺省时用本任务的 accept）或命令字符串时，合入成功且非 dry-run 的 `apply` 在 `acceptStillValid` 不为 true 时，经 lane 在源工作目录（`sourceWorkdir`）以 `sh -c` 运行该命令，限时同任务 `acceptTimeoutSeconds`，输出写 `verify.log`。`apply --verify` 强制运行（即使 `acceptStillValid: true`），`--no-verify` 跳过，二者互斥（退出码 2）。结论追加 `verify{command,ok,exitCode,log,tail?,next?}`，或 `verify{skipped}`（`acceptStillValid`、无命令）。验收失败时合并已在源树中、基准照常推进，apply 退出码 1。`applyVerify` 为其他类型时报错。

## 7. 会话与任务说明

### 7.1 同事调用（必须）

Pi：
```
pi (--session-id <uuid> | --fork <上一轮会话副本>) --session-dir <run>/session --mode json
   [--provider P] [--model M] [--thinking T] [--tools read,grep,find,ls（仅无法隔离的只读）] -p [@<图片>...]   # stdin: prompt.md
```
Codex：
```
codex exec [fork <会话 id>] --json --skip-git-repo-check [-C <workdir>] --dangerously-bypass-approvals-and-sandbox
   [-m M] [-c model_reasoning_effort="T"] [-c model_provider="P"] [--image=<图片>...] -     # stdin: prompt.md
```
进程工作目录为 workdir，独立进程组。用户 systemd 可用时，同事、每条 setup 与验收命令分别在 `systemd-run --user --scope --quiet --collect` 创建的 scope 内运行；supervisor 不在 scope 内。启动时探测一次，失败或 `DELEGATE_CGROUP=0`（也接受 `PI_DELEGATE_CGROUP=0`）则沿用进程组及环境标记回收。环境：去掉 §12 的保护变量，叠加 `.delegate.json` 的 `env`，再设 `DELEGATE_AGENT`、`DELEGATE_RUN_DIR`（Pi 另设 `PI_DELEGATE_ACTIVE=1`）。

### 7.2 事件

同事的 JSON 流被过滤为 `events.jsonl` 的紧凑事件，每条带 `attempt` 与 `at`（UTC，`%Y-%m-%dT%H:%M:%SZ`）：`session{id}`、`bash{cmd}`、`bash_done{ok}`、`read|edit|write{path}`、`tool{tool,arg}`、`tool_error{tool,detail}`、`turn{provider,model,stopReason,usage}`、`result{text}`（Pi）、`message{text}`（Codex）、`turn_error{detail}`、`warning{detail}`、`retry`、`rerun`、`compaction_start|end`、`settled`、`queue{ahead}`（验收排队）。不记录编辑全文。

### 7.3 附加约定（必须保持语义）

任务说明为中文（含 CJK 字符）时用中文附言，否则英文。以 `\n\n---\n` 与原文分隔：
- 验收命令（未隐藏时）：说明"结束后委派方在工作目录运行此命令，退出码 0 视为完成"，附代码块，并提示自检时用 `<入口> lane <命令>` 排队、排队时间不计时。
- 只读约定（Codex 的一切只读 run，以及在 worktree 中的只读 Pi）：不得创建、修改、删除文件；改动会被报告且不被采纳；委派记录不算改动。升档时若 `prompt.md` 尚无这段约定才追加，不得重复。
- reply 更改或取消验收命令时，说明旧标准不再适用。
- 受保护路径：逐项列路径与可选原因；中文约定为“若正确完成任务必须修改受保护路径，停止该实现路线，报告路径、必要修改和原因，等待主控处理；不得为避开保护而迁移、复制逻辑或削弱测试。”英文约定为“If completing the task correctly requires changing a protected path, stop that implementation route, report the path, necessary changes and reason, and wait for the delegator. Do not move or copy logic or weaken tests to bypass protection.”

`prompt.md` 保存同事实际收到的全文。

### 7.4 reply（必须）

接在对话**最新一轮**之后（结局为 `malformed` 的轮次跳过）；可用 `--agent`/`--tier` 换一位同事（agent 改变即隐含 `--fresh`，新消息须自足），否则同一 agent、workdir、worktree、mode、env、provider/model/thinking、retries。每次尝试都从上一轮会话**分叉**（Pi 用会话文件副本 `--fork`，Codex `exec fork`），上一轮会话永不改动。`--fresh` 开新会话但留在同一对话与 worktree。同一轮已有进行中的 reply 时拒绝。

默认像 `start` 一样立即输出启动状态；`--wait` 像 `run` 一样收取结论与答复，`--max`/`--progress`/`--full` 仅可与 `--wait` 同用。`--sync` 在返回启动状态前完成，冲突时不启动。

`protect` 与可选 `protectReasons` 在普通 reply / fresh 中继承；`--protect` 与 `--protect-reason` 均禁止覆盖。旧 meta 无原因仍正常继承路径。

### 7.5 reply --sync（必须）

`reply <run> --sync` 仅适用于 worktree 对话。启动下一轮前，以对话起点快照（有最近一次完整 apply 或同步基准时用该基准）、源工作区当前快照与 worktree 当前状态做三方合并，只同步主控后来产生、尚未进入对话的改动。未改变指针的 gitlink 不视作冲突，即使源与 worktree 的子模块指纹不同；主控改变 gitlink 指针则冲突。任一文件冲突时不写入 worktree、不启动同事，退出码 2，stderr 列出冲突路径。成功时新一轮的起始快照取同步后的 worktree，同步文件不计入该轮同事的 `files`；run 目录的 `sync.json` 记录同步文件列表，任务说明末尾按说明语言附同步提示；`.sync-base` 记录后续 apply 使用的合并基准，同步不创建 `.applied`，完整 apply 后同时推进两者。原地对话使用 `--sync` 为用法错误。

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

`sh -c` 执行，独立进程组，stdin 为 `/dev/null`，stdout+stderr 写日志。环境：去掉保护变量、叠加 `env`、设 `DELEGATE_LANE_HELD=1` 与 `DELEGATE_RUN_DIR`。命令结束或超时后，**在回收 shell 之前**先读取各 scope 的 `ControlGroup`，递归收集 `cgroup.procs`，记录仍存活的同 UID 进程及其命令、监听端口；优先写 `cgroup.kill`，不可用则 `systemctl --user kill` 先发 SIGTERM、最多等待 2 秒后发 SIGKILL。随后结束整个进程组（SIGTERM，宽限后 SIGKILL，存活判断排除僵尸），并按同 UID 的 `/proc/<pid>/environ` 清理逃出进程组的进程。超时退出码记 124，日志追加 `[accept timed out]` 与 `[exit N]`。

### 9.2 同事超时（必须）

- 计时用单调时钟，扣除 lane 排队时间。
- 到 `--timeout` 时：若有命令正在执行（`bash` 多于 `bash_done`）或最近 120 秒内有事件，继续；最多到 `--timeout × (1 + DELEGATE_TIMEOUT_GRACE/100)`（默认 1.1 倍）。超出即结束同事进程组，判 `timeout`。
- 应只在判定可能变化的时刻醒来。

### 9.3 supervisor

由入口以隐藏子命令 `_supervise <run 目录>` 启动（`running` 的判定依赖命令行含 `_supervise`），脱离会话（`start_new_session`），不继承 `DELEGATE_LANE_HELD`；启动后先持有 `<run>/supervisor.lock` 的排他 flock（临时名加锁后 rename）直到进程退出，再写 `pid`。启动方最多等 5 秒 `pid` 出现；超时先终止整个 supervisor 进程组并回收子进程，再写 `crashed` 的 `summary.json` 与 `exit_code`，不得让迟到的 supervisor 改写 run。无论何种异常都必须写出 `summary.json` 与 `exit_code`。

### 9.4 stop

向 supervisor 发 SIGTERM；supervisor 回收该 run 的 scope、同事或验收的进程组，排队中则放弃排队。12 秒内无 `exit_code` 时由 `stop` 直接回收 scope 与残余进程组并写 `stopped`。supervisor 已死而同事仍在时，直接回收 scope 与同事进程组。回收不得触及 supervisor、主控的 `wait` 或其他排队任务。

### 9.5 等待（`wait`/`run`）

阻塞在各 run 的 `supervisor.lock` 共享锁上（每个一个线程），`--max` 到期即返回。`wait --machine` 在调用开始时从 `<state>` 的 slot 读取整机运行中或等待中的 run 目录并固定该列表（之后 slot 被清理不影响本次收取），跨仓库收取；不带 `--machine` 时只收取当前仓库的 run。无参 `wait` 在 caller 已知时只选 caller 相同且运行中或未送达的 run；其他运行中或未送达的 run（含没有 caller 的旧 run）不收取、不输出结论块，只在 stderr 用一行报告数量、最多三个名称及年龄，并提示 `wait --all` / `clean <run>`。本会话没有可收取的 run 而有其他 run 时，提示后以 0 退出；都没有时沿用 `no active or undelivered runs`。caller 未知时无参 `wait` 与旧行为相同，收取全部。`wait --all`、`wait <run...>` 和 `wait --machine` 不受 caller 过滤；退出码只由实际收取的 run 决定。仅在 `--progress`、supervisor 尚未加锁（刚启动）或 4.4 之前的 run 时按 `DELEGATE_POLL`（默认 1 秒）轮询。

逐个收取（5.11）：

- `wait --any`：选出的 run 中已结束的立即输出结论块（§3.2）；都未结束时等到任一结束。只输出已结束的 run，其余在 stderr 列出名称并提示再次 `wait --any`。显式列出的 run 中已送达的跳过，因此重复同一条命令即可依次收完；全部已送达时同无参 `wait` 输出 `no active or undelivered runs` 并以 0 退出。退出码由本次输出的 run 决定；`--max` 到期仍无结束时退出码 75。
- `wait --stream`：每个 run 结束时输出一行状态行（含 `sourceDrift`，有未读答复时带 `report`），不输出改动清单与答复、不标记 `.delivered`；全部输出后 stderr 写 `all <N> runs reported` 并退出。不带 run 参数时每 5 秒按同一规则（caller、`--all`、`--machine`）重新选取，把期间新派出的 run 也纳入；全部结束即退出，之后派出的 run 需要新的 `wait`。退出码：有非 delivered/answered 结局为 1，`--max` 到期仍有未结束的为 75，否则 0。
- `--any` 与 `--stream` 互斥（退出码 2）。两者都与宿主无关：`--stream` 适合能把命令的每行输出变成通知的宿主，`--any` 适合只有“后台命令结束时通知”或只能分段调用的宿主（配合 `--max`）。

5.12 起，两者按首次检查记录已结束集合，结论附 `completionTiming`（§3.1）；原收取顺序不变，多个旧结果一起交付，不按完成时间排序。后台命令结束通知可能是在交付旧结果；旧 summary 没有 `finishedAt` 时省略，不推算。

## 10. 并发、准入与清理

- 整机并发：`<state>/<sha1(run 路径)[:16]>.slot` 记录运行中的 run；`DELEGATE_MAX_ACTIVE`（默认 8）、`DELEGATE_MAX_CODEX`（默认 4），0 不限。超出即拒绝并列出运行中的任务。
- 内存准入：`/proc/meminfo` 的 `MemAvailable` 低于 `DELEGATE_MIN_AVAILABLE_MB`（默认 4096，0 不查）时拒绝。
- 启动在 `<state>/.start.lock` 与 `<runs>/.start.lock` 两把锁下进行，检查与登记不可交错。
- 写入互斥：同一 workdir 同时只允许一个原地写入 run（`--allow-parallel-writes` 与 worktree run 除外）。
- **只有一层委派**：调用者本身是同事（设有 `DELEGATE_AGENT`、`PI_DELEGATE_AGENT` 或 `PI_DELEGATE_ACTIVE`）时，`start`/`run`/`reply` 一律以退出码 2 拒绝；`lane` 等其他命令不受影响。所有结果都回到主控。
- 自动清理：每次启动删除结束超过 `DELEGATE_KEEP_DAYS`（默认 7，0 关闭）天、已读取（不含只显示过截断答复的）、且没有未 apply 写入 worktree 的 run。
- `clean`：跳过运行中的与同事进程仍存活的；`--finished` 默认保留未读取的、只显示过截断答复的（`wait` 截断打印时写 `.truncated`，完整打印或 `result` 读全文后删除），以及未完整 apply 的 worktree run；`--force` 可覆盖这些保留条件。显式列出的 run 照常删除；提示未 apply 的 worktree。

## 11. 文件与目录

run 根目录：`DELEGATE_RUNS`，否则为**调用时当前目录**所在 git 根下的 `.local/run/delegate/`（不在仓库中则为当前目录下）；按名字/ID 也查找旧 `.local/run/pi/`。首次创建时写入内容为 `*` 的 `.gitignore`；run 目录 `<YYYYmmdd-HHMMSS>-<slug>[-<4 hex>]`，权限 700。`<state>` 为 `${XDG_STATE_HOME:-~/.local/state}/delegate`，**不得**提供环境变量覆盖。

| 文件 | 写入者 | 内容 |
|---|---|---|
| `meta.json` | 启动方（升档时 supervisor 更新） | run、dir、workdir、mode、agent、agentBin（实际解析的绝对执行路径）、agentVersion（该执行文件 `--version` 输出最后一行；取不到则省略）、tier、escalatedFrom、name、caller、callerSource、provider、model、thinking、timeout(Seconds)、accept、acceptTimeoutSeconds、retries、images、top、base、snapshotExclude、worktree{source,sourceWorkdir,config,path}、env、chainBase、sessionDir、parent、fork、startedAt/Epoch/Ns |
| `prompt.md` | 启动方 | 同事收到的全文 |
| `supervisor.lock` / `pid` / `agent.pid` | supervisor | 生命周期锁 / supervisor pid / 同事进程组 |
| `events.jsonl` `stderr.log` `supervisor.log` | supervisor | §7.2 / 同事 stderr / supervisor 输出 |
| `result.md` | supervisor | 最后一轮完整答复 |
| `summary.json` | supervisor | §3.1 中结束后的字段 |
| `finishedAt`（summary 字段） | 统一终态写入函数 | 正常结束、skipped、supervisor 错误、启动失败和 stop 强制结束时的 UTC 时间；summary 先写，exit_code 最后写；运行时推断 crashed 不伪造时间 |
| `protect` / `protectReasons`（meta 字段） | 启动方 | 保留保护路径字符串数组，可选原因映射，reply 继承 |
| `cleanup.json` | supervisor | 已清理进程的 PID、命令与监听 TCP 端口 |
| `scopes` / `scopes.lock` | supervisor | 本轮启动的唯一 systemd scope 单元名 / 并发读写锁 |
| `changes.json` `changes.patch` | supervisor | §6.2 |
| `accept.log` `setup.log` | supervisor | 命令、输出、`[exit N]` |
| `generate.log` / `.generate-pending` | apply | 生成日志 / 失败后仍须重试的标记（同 worktree 后续 apply 可接续） |
| `lane-wait` `lane-waiting-<pid>` | lane | §8 |
| `session/` `fork/` | 同事 / 启动方 | Pi 会话；reply 所用的上一轮会话副本 |
| `exit_code` | supervisor | 结束标记，**最后写** |
| `.delivered` `.applied` `.progress` | 读取方 | 已读取 / 已合并 / `--progress` 进度 |

`agent-handoff` 依赖 `meta.json`、`exit_code`、`.delivered`、`.applied` 与 `meta.json` 中的 `worktree.path`、`mode`；改变其含义须同步修改。

`caller` 是派发该轮 run 的主控会话标识，`start` / `run` / `reply`（包括 `--after` 的 waiting run）创建时写入；依次取非空 `DELEGATE_CALLER`、`CLAUDE_CODE_SESSION_ID`、`CODEX_THREAD_ID`、`PI_SESSION_ID`，均无则写 `null`；`callerSource` 记录取值来自哪个变量（caller 为 `null` 时也为 `null`）。宿主如何提供 caller 见 [delegate-protocol.md](delegate-protocol.md)。`session` 属于同事会话，不表示 caller。成功创建 run 后，若本仓库有已结束、未送达、caller 不等于当前 caller 的 run（当前 caller 未知时视全部旧 run 为其他），stderr 用一行报告数量、最多三个名称与年龄，并提示 `wait --all` / `clean <run>`；不影响 JSON stdout 与退出码。

## 12. 配置与环境变量

`.delegate.json`（仓库根）：

```json
{"env": {"CUDA_VISIBLE_DEVICES": ""},
 "worktree": {"copy": [".env", ".local/scan"], "link": ["third_party/sub"], "setup": ["pnpm install --offline --frozen-lockfile"], "writeSetup": ["uv sync --frozen --offline"]}}
```

`env` 为字符串到字符串的映射，注入同事、验收与 setup（原地与 worktree 均生效，reply 沿用）；`copy`/`link` 必须是仓库内相对路径，缺源跳过并在启动命令与 supervisor 的 stderr 提示。

环境变量均先读 `DELEGATE_<名>`，再读 `PI_DELEGATE_<名>`：`RUNS`、`CHEAP_AGENT`（pi）、`STRONG_AGENT`（codex）、`MAX_ACTIVE`、`MAX_CODEX`、`MAX_HEAVY`、`MIN_AVAILABLE_MB`、`TIMEOUT_GRACE`、`RESULT_CHARS`（6000）、`KEEP_DAYS`、`POLL`、`SNAPSHOT_MAX_BYTES`、`SETUP_TIMEOUT`（10m）、`GENERATE_TIMEOUT`（10m）。`DELEGATE_CALLER` 单独按上述优先级取值，不读取 `PI_DELEGATE_CALLER`。由实现导出、调用方不应设置的保护变量：`DELEGATE_AGENT`、`DELEGATE_RUN_DIR`、`DELEGATE_LANE_HELD`、`PI_DELEGATE_ACTIVE`、`PI_DELEGATE_AGENT`、`PI_DELEGATE_PARENT_RUN`。

仅测试构建（debug）可用且默认关闭的环境变量：`DELEGATE_TEST_SUPERVISOR_DELAY` 使 supervisor 在写 `pid` 前延迟指定时长，`DELEGATE_TEST_STARTUP_TIMEOUT` 缩短启动方等待 `pid` 的时限；用于黑盒验证超时回收，正式发布构建忽略它们。

## 13. 一致性验收

`tests/test_delegate.py` 是纯黑盒套件：通过伪造的 `pi`/`codex`（写在临时 `PATH` 中的 shell 脚本）以子进程驱动 CLI，只观察输出、退出码与 run 目录中的文件，覆盖本文全部必须项。被测入口由 `DELEGATE_BIN` 指定；未指定时套件先 `cargo build` `crates/delegate` 并测试产物，保证通过即覆盖当前源码；没有源码或 cargo 时才测已安装的 `skills/delegate/bin/delegate`，并在 stderr 提示：

```bash
DELEGATE_BIN=<被测可执行文件> python3 -m unittest tests.test_delegate
```

任何实现都必须在不修改该套件的前提下全部通过。
