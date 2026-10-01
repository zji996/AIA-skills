# 命令、run 目录与文件

## 命令与选项

| 命令 | 作用 |
|---|---|
| `start [选项] [任务说明]` | 后台启动，立即返回一行状态 |
| `run [选项] [--max <时长>]` | 启动并等到结论 |
| `reply <run> [消息] [--wait] [--fresh] [--sync]` | 接着最新一轮会话在后台启动并立即返回；`--wait` 等到结论（此时可用 `--max`/`--progress`/`--full`）。`--fresh` 开新会话，消息须自足；`--sync` 先同步主控后来改动，冲突时直接拒绝。验收与证据命令默认沿用，可分别覆盖或关闭；取消或隐藏已变更的验收命令时会告诉同事旧标准不再适用 |
| `diff [<run>] [--stat] [--total] [路径...]` | 以 `git diff` 输出该轮的改动；`--total` 为整段对话；终端下带颜色 |
| `apply [<run>] [--dry-run] [--merge]` | 把 `--worktree` 的现状（含最后一轮之后在 worktree 里的手工修改）相对对话起点的全部改动合并回原工作区（只写文件，不碰 index）：你没动过的文件直接写入（含权限位），双方都改过的文本做三方合并，大文件从 worktree 复制；经符号链接目录、文件与目录互换、二进制与符号链接冲突一律算冲突；有合并不了的冲突时什么都不写，`--merge` 则写入其余文件并在冲突处留冲突标记（其余冲突跳过）。没有跳过项时，共用该 worktree 的所有 run（含畸形的旁支）标记 `.applied` |
| `wait [<run>...\|--all] [--max <时长>] [--no-result] [--full] [--progress]` | 等待并输出结论；不指定任务时（或 `--all`）等所有仍在运行和尚未读取结果的任务 |
| `status [<run>...] [--json]` | 默认每任务一条自然语言短行；`--json` 保留完整原字段 |
| `result [<run>] [--path]` | 输出完整答复 |
| `stop <run>...` | 终止任务及其 scope、进程组 |
| `clean <run>...\|--finished [--force]` | 删除已结束的任务；`--finished` 默认保留结果未读取的 |
| `lane [--label <文字>] [--] <命令>` | 在整机重任务队列里执行命令（一个参数按 shell 命令执行），退出码原样返回；不带命令时列出正在跑与排队的项 |

启动选项：`--tier cheap|strong`（默认只读 cheap、写入 strong；便宜档失败且未改动时自动升档一次）、`--agent pi|codex`（直接指定，与 `--tier` 互斥，不升档）、`--image <路径>`（可重复）、`--accept <命令>` / `--no-accept`（覆盖或关闭仓库默认验收）、`--hide-accept`、`--accept-timeout`（默认 10m）、`--read-only`、`--in-place`（只读任务读实时工作区而非快照）、`--workdir`、`--timeout`（每次尝试，pi 默认 25m，codex 默认 50m）、`--retries`（答复畸形时重跑次数，默认 1）、`--model`/`--thinking`/`--provider`（不指定时用各 CLI 自己的默认设置；Codex 的 `--thinking` 对应推理强度）、`--allow-parallel-writes`、`--worktree`、`--after <run>`（等上游以 delivered/answered 结束再执行，否则 `skipped`；等待期间 state 为 `waiting`、不占名额）、`--in <run>`（只读，在上游 worktree 的快照里审它的改动）、`--protect <路径>`（可重复；末尾 `/` 为目录；被改动即判 `rejected` 并带 `protectViolation`，不跑验收；reply 沿用）。`<run>` 可以是完整 id、唯一片段、`last` 或 run 目录。

`start`/`run`/`reply` 另支持 `--evidence <命令>`、`--no-evidence` 与 `--evidence-timeout`（默认 30m）。reply 沿用上一轮证据命令与超时，可覆盖或关闭。`--agent` 固定同事时仍显示适配表默认档位（Pi cheap、Codex strong），JSON `agentPinned: true`；旧 meta 缺字段时按同一规则推断显示，不改写旧 meta，不自动升档。

## 证据命令

证据用于真实模型评测等慢且不确定的检查。验收通过后执行；未设验收时在正常答复后执行；验收失败或 protect 违规时跳过。在同事工作目录（隔离任务为其 worktree）通过 lane 排队，用验收相同的 env、原 PATH、独立进程组及 cgroup 回收，deny shim 不影响证据。拿到 lane 名额才开始计算 `--evidence-timeout`。

证据命令默认未设置。结果写入 meta 与 JSON 结论的可选 `evidence{exit,timedOut,seconds,tail,log}`：退出码、是否超时、执行秒数、日志末尾和完整日志位置。短行加“证据 通过/失败/超时”。任何证据失败只记诊断，永不改变任务 state、升档判断或命令退出码；它也不替代 `acceptStillValid` 的验收快照依据。

## 重任务队列（lane）

`status`/`wait` 默认只展示决策所需的状态、同事与档位、耗时、文件数与增删行、前三个目录、最近命令（运行中，最多 100 字符）、验收与下一步，文件路径相对仓库；`wait` 仍随后附答复。脚本用 `--json` 读取原有完整结构，`start`/`run`/`reply`/`stop` 也支持此选项；`result`、`diff`、`apply` 输出不变。JSON 的运行目录、worktree、结果位置仍保留可直接访问的绝对路径。

每任务有 `waiter.lock`，持有排他 flock 并记录等待者 PID；重复 `wait` 跳过已有活等待者覆盖的任务，全被覆盖时退出 `76`，通知仍给原等待者。`--any`、`--stream`（含后续加入的任务）、`--machine` 同样处理；等待者退出或死亡时内核释放锁，旧 PID 不阻止后续等待。

整机一条先进先出队列，同时放行 `DELEGATE_MAX_HEAVY` 个（默认 2，`0` 不限）：验收、证据、worktree `setup`、同事与主控用 `lane` 跑的检查都在这里排队。每个排队者在 `${XDG_STATE_HOME:-~/.local/state}/delegate/lane/` 下有一张按到达时间命名的票，持有其排他 flock 直到结束；等待者阻塞在前一张票的锁上，由内核在其结束或进程死亡时唤醒，不轮询，崩溃不留死锁。已在队列内的命令（带 `DELEGATE_LANE_HELD`）再调用 `lane` 直接执行，不会等自己。

排队时间不计时：验收的 `--accept-timeout` 与 `setup` 的超时从拿到名额开始算；同事自己用 `lane` 排队的时间记在 run 目录的 `lane-wait`，从它的 `--timeout` 中扣除。同事、每条 setup 与验收命令在独立进程组中运行；用户 systemd 可用时各自再进入独立 scope，结束或超时时按 cgroup 回收其派生进程（包括 `env -i`、`setsid` 后的进程）。无用户实例时自动沿用进程组与环境标记清理；`DELEGATE_CGROUP=0` 或 `PI_DELEGATE_CGROUP=0` 强制关闭 scope。supervisor 与主控的 `wait`、排队中的其他任务不进入这些 scope。

同事的超时：到 `--timeout`（不含排队）时若有命令正在执行，或 120 秒内有事件，继续运行，最多到 `--timeout` 的 `1 + DELEGATE_TIMEOUT_GRACE/100` 倍（默认 1.1 倍）；结论里 `graceSeconds` 记下超出的秒数。

## 用户与仓库配置

用户配置 `${XDG_CONFIG_HOME:-~/.config}/delegate/config.json` 支持 `agentDeny`、`env`、`maxRework`、默认 `accept`/`evidence`、`maxActive`、`maxCodex`、`maxHeavy`、`repoMaxActive`、`repoMaxCodex`。仓库根 `.delegate.json` 使用相同字段并保存仓库事实。

- 标量：仓库 > 用户 > 内置默认；容量的 `DELEGATE_MAX_ACTIVE` / `DELEGATE_MAX_CODEX` / `DELEGATE_MAX_HEAVY` / `DELEGATE_REPO_MAX_ACTIVE` / `DELEGATE_REPO_MAX_CODEX` 最高，仍兼容 `PI_DELEGATE_*`，`0` 不限。整机默认 12 / 6 / 2，每仓库默认 8 / 4。写入默认 `accept`/`evidence` 在仓库未提供字段时用用户值，对应 CLI 覆盖或关闭；只读不用两项默认。
- `env` 按键合并，仓库优先；`agentDeny` 按 `(argv, exact)` 去重，省略 `exact` 等于 `false`；仓库同键 hint 覆盖用户值，保留原顺序。仓库 `{"argv":["cargo","xtask","check"],"exact":true,"allow":true}` 撤销完全相同 argv 且 exact 相同的一条用户规则，无需 hint；不撤销不同 exact 或其他前缀规则，用户配置不能声明 `allow: true`。
- `worktree`、`generated`、`applyVerify` 只认仓库：用户级出现时忽略，`start`（含 `run`）在 stderr 汇总提示一次。
- 两份文件独立校验，覆盖不能隐藏错误；错误指出文件与字段，启动退出 2。JSON 语法错误指出文件与行列。meta、summary 与 `--json` 记 `configSources: ["user","repo"]`（启动时只有存在的配置文件，没有时 `[]`）。reply 沿用启动时的 env、deny、验收与证据，返工预算和容量按当前两级配置读取；来源包含继承配置与当前配置两部分。

用户级示例：

```json
{"maxActive":12,"maxCodex":6,"maxHeavy":2,"repoMaxActive":8,"repoMaxCodex":4,"maxRework":1,
 "env":{"CUDA_VISIBLE_DEVICES":""},
 "agentDeny":[{"argv":["cargo","xtask","check"],"hint":"主控合入后跑全量；同事改跑 cargo test -p <crate>"}]}
```

## 同事全量检查拦截（agentDeny）

仓库根 `.delegate.json` 可配置：

```json
{"agentDeny": [
  {"argv": ["cargo", "xtask", "check"], "hint": "全量检查由主控合入后跑；改跑 cargo test -p <crate> 与 cargo clippy -p <crate>"},
  {"argv": ["cargo", "xtask", "infra-test"], "exact": true, "hint": "请带过滤参数只测相关用例"},
  {"argv": ["pnpm", "check"], "hint": "全量检查由主控合入后跑；改跑 pnpm --filter <package> test"}
]}
```

默认按完整 argv 的逐项、大小写敏感前缀匹配；尾部可以有更多参数。`exact: true` 必须 argv 完全相等且无多余参数：上述 `cargo xtask infra-test` 命中，`cargo xtask infra-test --filter db` 放行。`cargo xtask check --all` 命中前缀规则，`cargo --locked xtask check` 不命中，不忽略或重排 `+toolchain`、`--locked` 等全局参数。每条规则的 `argv` 非空，首项是程序名（字母、数字、`._-`，不能是路径或 `.`/`..`），其他项为字符串，`hint` 非空，`exact` 可选且为布尔值；字符串不能含 NUL。同程序多条规则按配置顺序匹配首条。

启动时在 run 的 `agent-shims/` 为每个程序生成 shell shim，按原 PATH（含顶层 env.PATH）与同事工作目录解析真实程序的绝对路径，保留 cargo/rustup 等多调用符号链接的名称；真实程序缺失时，未命中的调用退出 127。只在同事进程的 PATH 最前面插入该目录，reply 继承规则。命中打印 hint 到 stderr，退出 77；未命中直接 exec，参数、stdin/stdout 与退出码透传。同事的 `lane` 继承 shim PATH，即使自己设置 `DELEGATE_ALLOW_HEAVY=1` 或 `DELEGATE_LANE_HELD=1` 仍会拒绝。`denied.log` 累计本轮各尝试的次数，结论 `denied: N`，短行显示“拦下 N 次全量检查”；未配置或空列表不生成 shim，不增加字段。

验收、证据、setup、收尾生成核对、apply 的生成与 applyVerify 都由未注入 shim 的执行环境运行，主控自己的 lane 同理。没有环境放行开关或可交给同事的令牌。威胁模型是防误用，不是防恶意：同 UID 进程可以改 PATH、用绝对路径或改 shim/记录，这不是安全沙箱。

## 改动清单

在 git 仓库中，启动时和同事结束后（验收命令之前）各把整个工作区记为一个 tree 对象：借用真实 index 的副本执行 `git add -A` 与 `write-tree`，真实 index、分支和 stash 都不动；被忽略的文件不计，大于 `DELEGATE_SNAPSHOT_MAX_BYTES`（默认 2 MiB）的未跟踪文件只比较大小与修改时间。两次快照之差就是 `files`、`changes`（文件数与 +/- 行数）、`changes.json` 与 `changes.patch`，因此 shell 或脚本改的文件也会列出，改了又改回的不列，运行前已有的脏改动不算。原地运行时，别人在同一时间对仓库的改动也会被计入；需要干净归属时用 `--worktree`。子模块只作为一个条目出现：其检出中的已跟踪改动、未跟踪文件或提交变化都记为 `submodule contents`。快照失败时（例如 git 出错），结论带 `warning`，改动清单显示 unknown，只读任务也因此无法核验。非 git 目录只能根据编辑事件列出 `files`。

写入任务有快照改动时，`--json` 的 `shape` 补充改动形状：`dirs` 按路径前两级目录汇总增删行；`largest` 列出改后总行数最多的文本文件；`config` 列出被改动的依赖清单、锁文件、构建与 CI 配置；`removed` 列出删除文件。列表默认各最多 5 项，`DELEGATE_SHAPE_LIMIT` 可设为 1–20；超出时 `dirsMore`、`largestMore`、`configMore`、`removedMore` 记录未显示数量。`changes` 仍是整体文件数与增删行数，完整逐文件信息在 `changes.json`。JSON 模式沿用改动清单与 `shape` 小节；只读或无改动任务省略。

## worktree

`--worktree` 在 `${XDG_CACHE_HOME:-~/.cache}/delegate/worktrees/<仓库名>-<run>` 建立 detached worktree（放在仓库外，免得测试、lint、文件监听扫到），检出的是启动时快照的提交（`commit-tree`，父提交为 HEAD，只被该 worktree 引用），所以同事看到的正是你当前的工作区（含未提交与未忽略的未跟踪文件），`git diff HEAD` 只显示它自己的改动；没有提交的新仓库也可以用。`--workdir` 为子目录时，同事在 worktree 的对应子目录工作。supervisor 在同事启动前按仓库根 `.delegate.json` 准备 worktree；其 `generated` 配置用于 `apply` 后重建生成文件（见 `docs/delegate-spec.md` §6.4）：

```json
{"accept": "make check", "generated": {"paths": ["src/generated/"], "command": "make generate"},
 "worktree": {"copy": [".env", ".local/scan"], "link": ["models/weights"],
              "setup": ["pnpm install --offline --frozen-lockfile"], "writeSetup": ["uv sync --frozen --offline"]}}
```

- `copy`：仓库根相对路径；被 git 忽略的文件或目录（如 `.local/scan`）也会递归复制，改动不影响原仓库。源不存在或为空时在 stderr 和 `warnings` 提示。
- `link`：大而只读的被忽略目录，建符号链接，写入会落到原仓库。
- `setup`：依次在 worktree 根执行，输出写入 `setup.log`，任一失败即判 `failed`（`DELEGATE_SETUP_TIMEOUT`，默认 10m）。依赖用包管理器从本机缓存重建：pnpm 与 uv 以硬链接安装，几乎不额外占盘，但文件多时仍慢（含 torch 的数 GB venv 约 1 分钟）；不要 link `node_modules`、`.venv`，其中的可编辑安装指向原仓库源码。
- `writeSetup`：同 `setup`，只在写入任务里、`setup` 之后执行；只读任务跳过。
- 这三类路径不计入改动。子模块在新 worktree 里是空目录：只读使用时写进 `link`（会替换空目录），需要独立修改时在 `setup` 里初始化。
- 顶层 `accept` 是写入任务的默认验收命令；`--accept` 覆盖，`--no-accept` 关闭，只读任务不使用。
- `generated.paths` 的匹配语义同 `--protect`；改动清单与 diff 仍列出这些文件，`apply` 跳过其合并，在合并其他文件后由 lane 在源仓库根执行 `sh -c` 的 `command`，日志写入 `generate.log`；`--dry-run` 只报告动作。生成命令失败时退出 1，已合并文件保留（§6.4）。
- `--protect`/`--protect-reason` 与生成路径重叠时，启动 stderr 提示生成命令例外并附在同事的任务说明里；该命令与路径保存到 meta，reply/fresh 沿用。收尾在 lane 中于 worktree 根复跑，比较生成路径的内容、权限和符号链接（含忽略文件）；一致则允许，漂移报 `protectViolation` 与“生成物被手改”，生成失败判 rejected，日志在 `protect-generate.log`。
- 验收证据将托管 copy/link 路径明确排除，`accept.excluded` 与 apply 结论的 `excluded` 列出仓库相对路径；这些路径上的 skip-worktree 不算缺口。未托管 gitlink 的目录缺失或为空、且指针等于 HEAD 时，其未初始化内容也列为范围外，但指针仍保留在证据树中。源子模块初始化后重新核验：干净且同指针可复用，脏内容、索引标记、非空未初始化目录、变更的未初始化指针或读取失败仍判证据不完整。

`--read-only` 在 git 仓库里默认也用这样的 worktree（`--in-place` 除外），只作为供阅读的快照：主控同时的编辑既不影响它读到的内容，也不会被算成它的改动；它违规写入的文件留在 worktree 里，记为 `readOnlyViolation`，不能 `apply`。两位同事在其中都有全部工具，`setup` 照常执行（它们可能跑测试）；只有无法隔离时（非 git 或 `--in-place`），只读 Pi 才只保留读文件、搜索、列目录。

worktree 由对话共享，`clean` 删除最后一个使用它的 run 时执行 `git worktree remove`，过期清理同理；`agent-handoff` 会列出尚未 `apply` 的写入型 worktree。

写入任务未 apply 时，`status`/`wait`/`clean` 仅检查累计改动清单中的路径，按类型、大小和哈希比较源工作树与同事结束时的最终内容，执行位也须一致，删除路径须在源中不存在；不要求提交进 HEAD。全部包含时写 `.applied` 并记 `appliedBy: "detected"`，短行“已合入（主干已含改动）”，`next` 改为清理建议，`clean --finished` 按已合入处理。仅部分包含或读取失败时视为未检测到，沿用原 apply 建议；检测不代表验收已在源中复跑。

同仓库串行 `apply`；慢合并可后台执行。stderr 实时报告检查、生成器排队（人数与前序任务）、生成日志绝对路径和执行超时；排队不算生成超时。生成失败留 `.generate-pending`，再次 apply 即使普通文件已合入也重跑生成。编号前缀冲突只警告，不改号或退出码；末尾 `operation: apply` JSON 带 `numberedPrefixConflicts` 和验收复用的原因。`acceptStillValid` 只在验收前后历史全树与最终源全树有完整相同证据时为 true，不完整时省略；忽略文件、环境、数据库、Git 历史另行判断，后续改动/reply 不沿用旧 true。`.delegate.json` 的 `applyVerify`（`true` 用顶层 `accept`，或直接写命令）开启合并后验收：`acceptStillValid` 不为 true 时经 lane 在源工作目录运行，结论带 `verify`，失败退出 1（合并已写入）；`--verify` 强制、`--no-verify` 跳过。

## 会话

每轮的会话都属于自己的 run：Pi 保存在 run 目录的 `session/` 下，Codex 使用自己的会话存储，`summary.json` 的 `session` 记下会话 id。`reply` 在每次尝试时从上一轮的会话**分叉**（Pi `--fork` 上一轮会话文件的副本，Codex `exec fork`），上一轮的会话从不被改动：答复畸形重跑时从同一处重新开始，清理早先的轮次也不影响后续追问。结局为 `malformed` 的轮次不算对话的延续，之后的 `reply` 与 `apply` 都从它的上一轮接着。4.1 之前的 run 使用 `--no-session`，不能 `reply`。

写入任务上一轮为 `timeout` 时，下一次 reply 记 `meta.continuation: "timeout"`、`rework.kind: "continuation"`，不消耗 maxRework，超限也允许续做，改动超过 minor 的 60 行仍不补计。连续超时可以连续续做；rejected 等结局后仍按原返工规则。超时结论的 `next` 是可直接复制的续做命令，无会话 id 或 Pi 会话文件缺失时自动带 `--fresh`，自然语言结论注明“不计返工次数”。

图片以绝对路径交给同事：Pi 作为 `@<路径>` 附件，Codex 作为 `--image`。

## 位置

- 默认放在**执行命令时当前目录所在的 git 根**下的 `.local/run/delegate/<run_id>/`；不在仓库中则放在当前目录的 `.local/run/delegate/`，与 `--workdir` 无关。按名字/ID 查找时也读取旧 `.local/run/pi/`，不迁移旧记录；显式 `DELEGATE_RUNS` / `PI_DELEGATE_RUNS` 保持单根目录。换项目时传 run 目录路径。
- 根目录首次创建时写入只含 `*` 的 `.gitignore`，不污染 `git status`；run 目录权限为 `700`。
- 整机并发登记放在 `${XDG_STATE_HOME:-~/.local/state}/delegate/`：每个运行中的任务一个 `*.slot` 文件，内容是其 run 目录；任务结束或目录被删后，下一次 `start` 自动清掉对应登记。这个位置刻意不提供 `DELEGATE_*` 覆盖，被委派的同事无法另起一个计数池。

容量同时检查整机资源与仓库审查带宽：整机 `maxActive`/`maxCodex` 默认 12/6，仓库 `repoMaxActive`/`repoMaxCodex` 默认 8/4。仓库键按规范化 git common dir（主仓库与其 worktree 共用），非 git 目录按规范化路径。仓库计数仍来自整机 slot，不另建池；旧 slot 只有 run 目录时从 meta 反查仓库，新 slot 可附仓库键，读 slot 失败沿用原处理。拒绝说明命中的层级、当前计数与上限，只列该层相关运行任务；`--after` 等待容量时两层都生效。

## 文件

| 文件 | 内容 |
|---|---|
| `meta.json` | 启动参数、同事（`agent`）、workdir、模式、验收命令、启动时间；可选 `agentPinned`、仓库键 `repoKey`、证据命令 `evidenceCommand`、超时 `evidenceTimeoutSeconds` 和结果 `evidence`；`protect` 字符串数组与可选 `protectReasons` 原因映射（reply/fresh 继承） |
| `prompt.md` | 同事实际收到的任务说明；末尾可能附完成标准（`--accept`）与只读边界（Codex 只读任务） |
| `events.jsonl` | 过滤后的全过程：读取、命令、编辑路径、错误、每轮模型与用量、重跑；不含编辑全文 |
| `result.md` | 最后一轮的完整答复 |
| `summary.json` | 结论：`state`、`attempts`、`files`、`changes`、`shape`、`accept`、可选 `evidence`、`cleanup`（已终止进程数、端口、命令）、`warnings`（空/缺失 worktree 源）、`readOnlyViolation` / `workspaceChanged`、`protectViolation`、`queuedSeconds`、`graceSeconds`、`warning`、`tokens`、`session`、`error`（`next` 由 `status` 现算） |
| `scopes` / `scopes.lock` | 本轮的 systemd scope 单元名 / 并发读写锁；仅在用户 systemd 可用时出现 |
| `changes.json` / `changes.patch` | 前后快照的 tree、逐文件状态与行数；可选 `finalEntries` 冻结结束时改动路径的最终指纹，供手动合入检测；可直接 `git apply` 的补丁 |
| `setup.log` | `--worktree` 的 `setup` 命令输出 |
| `verify.log` | `apply` 合并后验收（`applyVerify` / `--verify`）的输出 |
| `generate.log` | `apply` 执行 `.delegate.json` 的 `generated.command` 时的输出 |
| `.generate-pending` | 生成未成功，后续 apply 仍须重试；成功后清除 |
| `session/` / `fork/` | Pi 本轮的会话；`reply` 分叉所用的上一轮会话副本 |
| `.applied` | `--worktree` 改动已合入；`appliedBy: "detected"` 区分源工作树检测到的手动合入 |
| `accept.log` | 验收命令的完整输出与退出码；`summary.json` 的 `accept.queuedSeconds` 是它在 lane 中排队的秒数 |
| `evidence.log` | 证据命令完整输出；结果在 meta 和 JSON 的可选 `evidence` 中，不决定状态 |
| `supervisor.lock` | supervisor 在世期间持有的 flock，`wait` 阻塞在它上面 |
| `waiter.lock` | 收取进程持有的排他 flock，内容为 PID；进程死亡锁自动释放 |
| `protect-generate.log` | 收尾复跑受保护生成物命令的输出 |
| `agent-shims/` / `denied.log` | 同事专用程序 shim / 每次拒绝追加一行 `1`，结论统计为 `denied` |
| `lane-wait` / `lane-waiting-*` | 同事在 lane 中已排队的秒数 / 正在排队的标记 |
| `stderr.log` | 同事 CLI 的标准错误 |
| `exit_code` | 结束标记：`0` 为 delivered/answered，`1` 为其他结局；运行中不存在 |
| `.delivered` | 结果已被 `run`/`wait`/`result` 读取过 |

5.12 新 summary 带 `finishedAt`；旧记录缺失时不补造。`wait --any/--stream` 的 `completionTiming` 以首次检查分类，通知可能只交付旧结果。`changes` 是本轮差异，`pendingChanges` 是累计待合入量。`accept.tree` / `treeAfter` 保存验收前后历史 tree，`snapshotComplete` / `snapshotReason` 记录证据是否完整。受保护文件有原因时违规诊断另带 `protectViolationReasons`。

5.18 状态 JSON 可选 `applied: true` 与 `appliedBy: "detected"` / `"delegate"`，分别表示源工作树检测合入或通过 apply 合入。`evidence` 为旁路证据结果；旧记录缺少这些字段仍可读。

`agent-handoff` 的快照脚本依赖 `meta.json`、`exit_code` 与 `.delivered` 判断未完成或未读取的委派；修改这三者的名字或含义时要同步修改它。

## 清理

每次 `start` 会删除结束超过 7 天且结果已读取的记录（只显示过截断答复的不算已读取）；`DELEGATE_KEEP_DAYS` 调整天数，设为 `0` 关闭。

## 环境变量

均先读 `DELEGATE_<名称>`，未设置时回退到 4.0 前的 `PI_DELEGATE_<名称>`。

| 变量 | 作用 |
|---|---|
| `DELEGATE_RUNS` | run 根目录 |
| `DELEGATE_MAX_ACTIVE` | 整机同时运行的任务上限，默认 12；`0` 不限 |
| `DELEGATE_MAX_CODEX` | 整机 Codex 任务上限，默认 6；`0` 不限 |
| `DELEGATE_MAX_HEAVY` | lane 同时放行的重命令数，默认 2；`0` 不限 |
| `DELEGATE_REPO_MAX_ACTIVE` | 同仓库同时运行的任务上限，默认 8；`0` 不限 |
| `DELEGATE_REPO_MAX_CODEX` | 同仓库 Codex 任务上限，默认 4；`0` 不限 |
| `DELEGATE_MIN_AVAILABLE_MB` | 可用内存低于该值（MB）时拒绝启动，默认 4096；`0` 不检查 |
| `DELEGATE_CGROUP` | `0` 关闭用户 systemd scope 回收；默认自动探测 |
| `DELEGATE_TIMEOUT_GRACE` | 同事超时后仍在工作时的宽限百分比，默认 10 |
| `DELEGATE_RUN_DIR` / `DELEGATE_LANE_HELD` | 由脚本导出：同事所在 run 目录（用于扣除排队时间）/ 已在 lane 名额内 |
| `DELEGATE_RESULT_CHARS` | 答复超过该长度显示开头约 2/3 与结尾约 1/3，默认 6000 |
| `DELEGATE_KEEP_DAYS` | 自动清理天数，默认 7 |
| `DELEGATE_POLL` | `wait --progress`、4.4 之前的 run 与刚启动的 supervisor 的检查间隔秒数，默认 1；其余等待不轮询 |
| `DELEGATE_CHEAP_AGENT` / `DELEGATE_STRONG_AGENT` | 档位对应的同事，默认 `pi` / `codex` |
| `DELEGATE_AGENT` | 由脚本导出给同事（`pi`/`codex`）；设有它的进程不能再委派 |
| `PI_DELEGATE_ACTIVE` | 旧版标记，仍导出给 Pi；存在时视为 Pi 调用者 |
| `DELEGATE_CALLER` | 主控会话标识，无参 `wait` 据此只收本会话的任务；未设时依次取 `CLAUDE_CODE_SESSION_ID`、`CODEX_THREAD_ID`、`PI_SESSION_ID`。`$D protocol` 显示当前取到的值与来源 |

## 代码结构

`bin/delegate` 是安装时按 `bin.sha256` 校验下载的静态二进制（Linux x86_64 / aarch64，musl），源码在仓库的 `crates/delegate/`，行为契约见仓库的 `docs/delegate-spec.md`。接入新的同事只需在 `agents.rs` 的 `AGENTS` 适配表加一项（启动命令、续接方式、事件解析、默认超时、是否占强档名额）。接入新的主控宿主见仓库的 `docs/delegate-protocol.md`；`$D protocol` 输出协议版本、会话标识与各同事实际执行的文件、版本及被遮住的同名文件。
