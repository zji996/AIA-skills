# delegate 宿主接入协议（protocol 1）

> 给 harness 作者看：一个宿主（Claude Code、Codex、Pi 或自研 harness）要把 delegate 当作它的子代理层，需要提供什么、可以依赖什么。
> 本文是 [delegate-spec.md](delegate-spec.md) 中**对宿主可见部分**的摘要与稳定承诺，细节以 spec 为准。spec 其余部分（run 目录布局、锁、快照算法）是实现细节，宿主不应依赖。

## 1. 分工

delegate 负责一切与宿主无关的事：隔离（worktree、快照）、验收命令、整机容量与重任务排队、升档、合并（`apply`）、清理、结论格式。宿主不内置子代理逻辑，只做一层很薄的适配：

| 宿主必须提供 | 用途 |
|---|---|
| 执行 shell 命令并取得 stdout、stderr 与退出码 | 所有交互都是命令行 |
| **后台执行并在结束时通知**；没有时，单次调用要能跑满 5 分钟 | `wait`/`run`/`reply --wait` 会阻塞到同事结束。有通知就放后台；没有就用 `wait --max 4m` 分段，退出码 75 表示仍在运行 |
| 可选：**流式通知**（命令每输出一行就通知一次） | 有就用 `wait --stream`，每个同事完工各得一次通知；没有就用 `wait --any` 逐个收取，见 §5 |
| **会话标识**：导出到它执行的命令的环境变量 | 无参 `wait` 只收本会话派出的任务，见 §3 |
| 让模型读到技能说明（`skills/delegate/SKILL.md`）与 `D=<技能目录>/bin/delegate` | 模型据此派发与读结论 |

宿主**不需要**：理解 run 目录、解析事件流、管理 worktree、调度并发。

## 2. 宿主可以依赖的输出（必须保持）

- **状态行 / 结论行**：单行 JSON，以 `{"run"` 开头，UTF-8。字段顺序和空白不属于契约，按 JSON 解析。
- **稳定字段**：`run`、`name`、`state`、`agent`、`mode`、`caller`、`next`、`dir`、`error`、`accept`、`changes`、`escalatedFrom`。其他字段可能增加，宿主须忽略不认识的字段。
- **state**：`running` / `waiting` / `delivered` / `answered` / `rejected` / `malformed` / `failed` / `timeout` / `killed` / `crashed` / `stopped`（spec §4）。
- **退出码**：`0` delivered/answered 或命令成功；`1` 其他结局；`2` 用法错误或被拒绝（容量满、嵌套委派等）；`75` `--max` 到期仍在运行。
- **诊断**：写 stderr，以 `delegate: ` 开头；只供人和模型读，宿主不解析。
- **`next`**：可直接复制执行的下一条命令；没有 `next` 表示答复本身就是交付物。
- **逐个收取**（5.11 起，可选）：`wait --stream` 每个 run 结束输出一行状态行（`report` 字段是读答复的命令），全部结束后退出；`wait --any` 在任一 run 结束时返回，只输出已结束的，重复同一命令收下一个。`sourceDrift` 字段提示 worktree 快照之后源工作区的变化与重叠文件，帮助在 `apply`、`apply --merge` 与 `reply --sync` 之间选择。
- **5.12 可选字段**：`finishedAt` 为已写入的终态 UTC 时间，旧记录/推断 crashed 省略；`completionTiming` 是首次检查边界的 `already-finished` / `finished-during-wait`，保持收取顺序，一次可交付多个旧结果。`pendingChanges` 是累计待合入量（`changes` 仍是本轮差异）；`protectViolationReasons` 是可选保护原因映射，原违规路径数组不变。
- **apply 结论**：文件清单后追加 `run`、`operation: "apply"`、`apply{ok,dryRun}`，可选 `numberedPrefixConflicts[{directory,prefix,paths}]`（仅警告、不改退出码），及 `acceptValidityScope: "repository-snapshot"`、`acceptValidityReason`、可选 `acceptStillValid`。仅本轮成功且未改树的验收与最终源全树相同才为 true；失败或已知树不同为 false，dry-run、旧记录、快照失败、无验收/不完整证据省略布尔值。忽略文件、环境、数据库与 Git 历史不在范围内；后续改动与 reply 不得沿用此前 true。任务 state 不变，protocol 保持 1。

新增字段、新增 state 以外的变化（删除或改名上述字段、改变退出码含义）要升 protocol 版本。

## 3. 会话标识（caller）

`start` / `run` / `reply` 把 caller 写进 run 的 `meta.json`（`caller` 与 `callerSource`）；无参 `wait` 只收 caller 相同的 run，其他会话的任务只在 stderr 提示一行。依次取第一个非空的环境变量：

| 变量 | 由谁设置 |
|---|---|
| `DELEGATE_CALLER` | 任何宿主或用户显式指定；**新宿主优先用它** |
| `CLAUDE_CODE_SESSION_ID` | Claude Code 给 Bash 工具的命令导出 |
| `CODEX_THREAD_ID` | Codex 给它执行的命令导出 |
| `PI_SESSION_ID` | Pi 的 bash 工具默认导出 |

都没有时 caller 为 `null`，无参 `wait` 收取本仓库全部任务（旧行为）。宿主只需保证同一会话内值稳定、不同会话不同；格式不限。

## 4. 能力探测

`delegate protocol` 输出一行 JSON，供宿主适配器在启动时检查兼容性、给用户做诊断：

```json
{"protocol":1,"version":"5.12.0","caller":{"id":"…","source":"CLAUDE_CODE_SESSION_ID"},
 "agents":[{"name":"pi","tiers":["cheap"],"available":true,"bin":"/…/pi","version":"0.87.1","shadowed":[]},
           {"name":"codex","tiers":["strong"],"available":true,"bin":"/…/codex","version":"codex-cli 0.158.0",
            "shadowed":["/snap/bin/codex"]}]}
```

- `protocol` 不同于宿主预期时，适配器应提示升级而不是继续调用。
- `agents[].bin` 是 PATH 上实际会执行的文件；`shadowed` 按 PATH 中的原路径列出更靠后、被它遮住的同名可执行文件（例如旧的 snap 版本），用于排查“同事用错了版本”。
- `caller` 为 `null` 说明宿主没有导出会话标识。

## 5. 适配一个新宿主（清单）

1. **会话标识**：给执行的命令导出 `DELEGATE_CALLER=<会话 id>`（或复用 §3 已支持的变量）。用 `delegate protocol` 确认 `caller.source`。
2. **加载技能**：把 `skills/delegate` 安装进宿主的技能目录（`scripts/install.sh` 会按 `metadata.exclude-agents` 跳过不该装的宿主），让模型能读到 SKILL.md。
3. **后台等待**：有“后台执行并通知”能力的宿主，要保证阻塞命令进后台。可以在工具调用前加拦截（Claude Code 的实现见 `skills/delegate/hooks/claude-code-background.py`：前台的 `wait`/`run`/`reply --wait` 被拒绝并提示改用后台）；也可以只在宿主的系统提示里说明。没有这种能力的宿主不需要拦截，SKILL.md 已教模型用 `--max`。
   **逐个收取**按宿主能力选一种，delegate 本身不依赖任何宿主：
   - 能把后台命令的每行输出变成通知（Claude Code 的 Monitor）：`$D wait --stream`，一个同事完工就通知一次，读完那一行再按 `report` / `next` 处理，不影响其他同事。
   - 只有“后台命令结束时通知”（Claude Code 的 `run_in_background`、其他宿主的后台任务）：后台跑 `$D wait --any`，每次通知处理完已结束的，再放一个同样的命令。
   - 都没有（普通脚本、只能同步调用的宿主）：循环 `$D wait --any --max 4m`，退出码 75 表示这段时间内没人完工，stderr 为 `no active or undelivered runs` 时收完。
   收到结束通知先读 `completionTiming`，它可能在交付首次检查时已结束的旧结果；缺 `finishedAt` 不应推算完成时刻。慢 `apply` 也可后台执行，stderr 会实时提示检查、生成排队与日志位置，宿主不要解析这些人读提示。同仓库串行 apply；读末尾结论决定验收复用及编号预警的后续处理。
4. **不要嵌套**：宿主自身被 delegate 派出时（环境里有 `DELEGATE_AGENT`），不要再向模型提供 delegate；delegate 也会以退出码 2 拒绝嵌套派发。
5. **验证**：在新宿主里跑一次 `$D run --read-only --name smoke "回答 ok"`，确认结论行的 `caller` 正确、无参 `wait` 只收本会话的任务。

## 6. 被调用方（同事）

同事 CLI 与宿主是两个独立的轴：宿主换了不影响同事，反之亦然。同事的差异（命令行、事件流解析、会话续接、只读方式、默认超时、是否占强档名额）集中在 `crates/delegate/src/agents.rs` 的适配表里；接入新同事只需加一项表和它的事件解析函数，宿主侧无需改动。
