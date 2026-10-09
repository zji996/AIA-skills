# 读结论、状态与边界

入口只保留日常用法；这里是把关时按需查的细节。相关：[命令与证据](commands.md)、[配置](configuration.md)、[run 目录与文件](output-and-files.md)、[等待与后台回收](runtime.md)。

## 结论、状态与退出码

`status`/`wait` 短行给名字、状态、同事/档位、耗时、改动、最近命令、验收、证据与下一步；答复附后。完整诊断取 `--json`、`diff`、`result`。

| 状态（JSON state） | 含义 |
|---|---|
| 已交付（`delivered`） | 验收通过；改法仍要看 `diff` |
| 已答复（`answered`） | 未设验收；关键结论去代码核实 |
| 未通过（`rejected`） | 验收失败或保护违规，`--json` 查看 `accept.tail`/`protectViolation` |
| `malformed` / `failed` / `timeout` / `killed` / `crashed` / `stopped` | 答复畸形 / 出错 / 超时 / 进程异常 / 被终止 |

state 只描述答复。只读任务写文件仍是 `answered`，短行提示，JSON 带 `readOnlyViolation`（隔离未合并）或 `workspaceChanged`（原地无法归属）。改动来自前后快照：含 shell 改动，排除原有脏改动与验收副产物。退出码：`0` 成功，`1` 其他结局，`2` 用法错误/拒绝，`75` 仍运行，`76` 已有等待者。

`busy <资源>` 是独立查询：占用时退出 0 并打印占用者，空闲退出 1 且不输出；它不改变任务验收判定。`acceptBlind` 命中只提示验收盲区，不改变 `delivered`。

`agentDeny` 默认按 argv 前缀拦截，`exact: true` 仅拦完全相等的参数；同事及其 `lane` 命中退出 `77` 并给替代提示，结论带 `denied`。同事跑相关检查，主控合入一批后跑全量；验收、证据、setup、apply 生成与复验、主控 lane 用原 PATH。

`--evidence <命令>` 收集模型评测等旁路证据，验收通过后（无验收则答复后）经 lane 运行；`--evidence-timeout` 默认 30m，排队不计时。失败/超时只记 `evidence`，永不改 state 或退出码；验收失败或保护违规跳过。reply 继承，`--no-evidence` 关闭。手动合入后，`status`/`wait`/`clean` 检查源工作树是否包含全部最终内容、删除与执行位；全含则显示“已合入（主干已含改动）”，JSON `appliedBy: "detected"`，可清理；部分包含或读取失败仍提示 apply。

通用 deny/env/返工预算/默认 accept 与 evidence/容量写到 `${XDG_CONFIG_HOME}/delegate/config.json`（未设变量时使用系统约定的用户配置目录），仓库 `.delegate.json` 合并覆盖。`configSources` 记来源；配置错误指出文件与字段，退出 2。合并与撤销见 [references/configuration.md](configuration.md)。

`--json` 保留原完整结构：`shape` 含目录增删行、改后最大文件、配置与删除路径；`*More` 是省略数量，逐文件在 `changes.json`。`changes` 是本轮，`pendingChanges` 是累计待合入量，零改动 reply 仍可能需 `diff --total`/`apply`。

慢 `apply` 可后台执行；失败后重试生成。JSON 的 `acceptStillValid: true` 证明验收成功且未改树、最终源树相同，可按 `repository-snapshot` 复用；`false` 重验，缺字段看原因。托管 copy/link、未初始化且未改动的 gitlink 内容列在 `excluded`；指针、其他索引标记及脏子模块仍核验。忽略文件、环境、数据库、Git 历史在范围外，改动/reply 后重判。

## 等待的细节

- 每任务只允许一个等待者；已有活等待者时跳过，全被覆盖退出 `76`，通知给原等待者。上下文压缩后旧等待者可能仍占着，此时用 `status` 看结局、`result <name>` 读答复，不必再等。
- 无参 `wait` 只收本会话，无会话标识时收全部；`--all` 收本仓库全部，`--machine` 收整机；`--until-all` 等到范围内全部结束才返回。
- 宿主能把后台进程的每行输出变成事件时（Claude Code 的 Monitor）可用 `wait --stream`。宿主没有完成唤醒（如 Codex）时不会自动收到结果：在自己的步骤之间用 `wait --max 4m` 主动收取，返回 `75` 后再等。Claude Code 的 `hooks/claude-code-background.py` 可拒绝前台等待。
- 脚本加 `--json`：`completionTiming` 区分旧结果与等待中新完成，`sourceDrift.overlap` 提示源改动重叠。
- 只看过截断答复的任务 `clean --finished` 会保留，读过全文（`result <name>`）或加 `--force` 才删。

## 保护路径与生成物

`--protect`/`--protect-reason` 与 `generated.paths` 重叠时只允许生成命令改动，收尾复跑核对，产出不一致报“生成物被手改”。同事需要改受保护路径时须停下报告，不得搬逻辑或削弱测试绕路。

## 边界

1. **容量**：整机默认任务 12/Codex 6、重命令 2；每仓库任务 8/Codex 4（git worktree 算同仓库）。可用内存低于 4 GB 拒绝启动；命中上限说明层级、计数并列该层任务，先 `wait` 收一批；`--after` 等容量时两层都检查。验收、证据、setup、`lane` 排队不计时。上限由用户设定，同事不要自行调整。
2. **只读**：git 仓库里默认读启动时的工作区快照（独立 worktree，含未提交改动），你可以同时改代码；要读实时工作区加 `--in-place`。在快照里两位同事都有全部工具（能看 git 历史、跑测试），只读靠约定与事后核对，写了也只留在它自己的 worktree；非 git 目录或 `--in-place` 时 Pi 只剩读文件、搜索和列目录。
3. **写入**：原地写入同一目录同时只能有一个，且你同时改的文件会算进它的改动；要并行或不想被打扰就加 `--worktree`。同源已有写入任务在运行时，不带 `--worktree` 的写入任务会自动改用 worktree，把源工作区留给你 apply；`apply` 时如果有原地写入任务正在改源工作区，会警告。
4. **worktree 依赖与验收**：`worktree.copy` 复制忽略材料，或用 `link`/`setup`（`writeSetup` 仅写入）；缺源/空源带 `warnings`。`env` 注入同事、验收、证据和 setup；`accept`/`evidence` 是写入默认，只读不用；CLI 覆盖或关闭。`applyVerify` 合并后主干复验。别 link `node_modules`/`.venv`。
5. **超时**：`--timeout` 默认 Pi 25 分钟、Codex 50 分钟（不含排队），到时若仍在执行命令再宽限最多 10%，最坏约 55 分钟，落在主控 1 小时提示缓存内。超时的写入任务改动保留，先 `reply` 续做；任务太大就拆小。默认值有意偏宽：被截断后的返工（续接、重读上下文、重跑验收）比多等几分钟更贵，不要为求快把 `--timeout` 压短。

命令与全部选项见 `$D --help`；run 目录、文件、环境变量与清理策略见 [references/output-and-files.md](output-and-files.md)。
