# run 目录、会话与文件

按需阅读：[命令与证据](commands.md)、[等待与后台回收](runtime.md)、[快照与合并](worktrees.md)。

## 用户与仓库配置

配置合并、撤销与同事检查拦截见 [configuration.md](configuration.md)。

## 会话

每轮的会话都属于自己的 run：Pi 保存在 run 目录的 `session/` 下，Codex 使用自己的会话存储，`summary.json` 的 `session` 记下会话 id。`reply` 在每次尝试时从上一轮的会话**分叉**（Pi `--fork` 上一轮会话文件的副本，Codex `exec fork`），上一轮的会话从不被改动：答复畸形重跑时从同一处重新开始，清理早先的轮次也不影响后续追问。结局为 `malformed` 的轮次不算对话的延续，之后的 `reply` 与 `apply` 都从它的上一轮接着。4.1 之前的 run 使用 `--no-session`，不能 `reply`。

写入任务上一轮为 `timeout` 时，下一次 reply 记 `meta.continuation: "timeout"`、`rework.kind: "continuation"`，不消耗 maxRework，超限也允许续做，改动超过 minor 的 60 行仍不补计。连续超时可以连续续做；rejected 等结局后仍按原返工规则。超时结论的 `next` 是可直接复制的续做命令，无会话 id 或 Pi 会话文件缺失时自动带 `--fresh`，自然语言结论注明“不计返工次数”。

图片以绝对路径交给同事：Pi 作为 `@<路径>` 附件，Codex 作为 `--image`。

## 位置

- 默认放在**执行命令时当前目录所在的 git 根**下的 `.local/run/delegate/<run_id>/`；不在仓库中则放在当前目录的 `.local/run/delegate/`，与 `--workdir` 无关。按名字/ID 查找时也读取旧 `.local/run/pi/`，不迁移旧记录；显式 `DELEGATE_RUNS` / `PI_DELEGATE_RUNS` 保持单根目录。换项目时传 run 目录路径。
- 根目录首次创建时写入只含 `*` 的 `.gitignore`，不污染 `git status`；run 目录权限为 `700`。
- 整机并发登记放在 `${XDG_STATE_HOME}/delegate/`（未设变量时使用系统约定的用户状态目录）：每个运行中的任务一个 `*.slot` 文件，内容是其 run 目录；任务结束或目录被删后，下一次 `start` 自动清掉对应登记。这个位置刻意不提供 `DELEGATE_*` 覆盖，被委派的同事无法另起一个计数池。

容量同时检查整机资源与仓库审查带宽：整机 `maxActive`/`maxCodex` 默认 12/6，仓库 `repoMaxActive`/`repoMaxCodex` 默认 8/4。仓库键按规范化 git common dir（主仓库与其 worktree 共用），非 git 目录按规范化路径。仓库计数仍来自整机 slot，不另建池；旧 slot 只有 run 目录时从 meta 反查仓库，新 slot 可附仓库键，读 slot 失败沿用原处理。拒绝说明命中的层级、当前计数与上限，只列该层相关运行任务；`--after` 等待容量时两层都生效。

## 文件

| 文件 | 内容 |
|---|---|
| `meta.json` | 启动参数、同事（`agent`）、workdir、模式、验收命令、启动时间；可选 `agentPinned`、仓库键 `repoKey`、证据命令 `evidenceCommand`、超时 `evidenceTimeoutSeconds` 和结果 `evidence`；`protect` 字符串数组与可选 `protectReasons` 原因映射（reply/fresh 继承） |
| `prompt.md` | 同事实际收到的任务说明；末尾可能附完成标准（`--accept`）与只读边界（Codex 只读任务） |
| `events.jsonl` | 过滤后的全过程：读取、命令、编辑路径、错误、每轮模型与用量、重跑；不含编辑全文 |
| `result.md` | 最后一轮的完整答复 |
| `summary.json` | 结论：`state`、`attempts`、`files`、`changes`、`shape`、`accept`、可选 `evidence`、`cleanup`（已终止进程数、端口、命令、已停 user unit 与诊断）、`warnings`（空/缺失 worktree 源）、`readOnlyViolation` / `workspaceChanged`、`protectViolation`、`queuedSeconds`、`graceSeconds`、`warning`、`tokens`、`session`、`error`（`next` 由 `status` 现算） |
| `cleanup.json` / `cleanup.lock` | 多阶段后台进程与 user unit 回收记录 / 并发回收锁；systemctl 缺失、bus 不可用、无标记与回收失败只记诊断 |
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
| `DELEGATE_CGROUP` | `0` 关闭 delegate 自建用户 systemd scope；默认自动探测。项目脚本自行启动的 user unit 仍按任务归属回收 |
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
