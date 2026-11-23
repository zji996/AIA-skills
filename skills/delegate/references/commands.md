# 命令与证据

## 命令与选项

| 命令 | 作用 |
|---|---|
| `start [选项] [任务说明]` | 后台启动，立即返回一行状态 |
| `run [选项] [--max <时长>]` | 启动并等到结论 |
| `reply <run> [消息] [--wait] [--fresh] [--sync\|--sync=agent\|--no-sync] [--allow <path>]` | 续接最新一轮会话后台启动；`--wait` 等到结论。写入 worktree 续做默认三方同步到当前源快照（保池槽 `target/`，冲突时保旧基线并在 stderr 列重叠文件）；`--no-sync` 跳过、`--sync` 遇冲突拒收、`--sync=agent` 留文本标记交同事先解（未清标记拒收，二进制/结构冲突仍拒收）。`--allow` 在保护范围内按链开放路径；`--fresh` 开新会话。验收/证据默认沿用，可覆盖或关闭 |
| `patch <run> [-- <path>…]` | 打印保留的完整补丁，可按精确路径/目录前缀过滤，供 `git apply --3way` 恢复 |
| `diff [<run>] [--stat] [--total] [路径...]` | 以 `git diff` 输出该轮的改动；`--total` 为整段对话；终端下带颜色 |
| `apply [<run> …] [--dry-run] [--merge] [--verify\|--no-verify] [--keep] [--keep-commits\|--commit] [--closes <Wxx>] [--json]` | 按序将 worktree 改动合入源工作区，默认不提交并列同事标题，遇错即停报 `failedStep`/`writtenFiles`；`acceptStillValid` 不为 true 时末次合入后经 lane 重跑验收（`applyVerify: false` 关闭）。`--keep-commits` 按序 cherry-pick；`--commit` 仅暂存本批路径压成单提交；`--closes <Wxx>` 合入并显式关闭工作项；尾部提示解锁下游与 `verify` 留存原因，成功后按 `worktree.cleanOnApply` 归档删 worktree |
| `busy <资源名>` | 有实际运行的匹配命令时逐行列 run、命令和持续秒数，退出 0；空闲无输出、退出 1 |
| `wait [<run>...\|--all] [--since <SEQ>] [--max <时长>] [--no-result] [--full] [--progress]` | 无参和带名字的 `wait <run>...` 默认同 `--any`：返回上次 wait 之后所有进入终态或提问态的 run（首个给全文，其余给一行摘要加 `result` 命令）；`--since <SEQ>` 仅返回 `seq > SEQ` 的事件并输出新游标，同时更新 `cursors.json`；`--all` / `--until-all` 等全部（单次最多 50 分钟）。人读末行给本会话剩余数和原样重跑命令 |
| `work [list\|show\|add\|wait\|close\|drop\|defer\|import-md] ...` | 统一工作表（7 态/6 列/卡片、`--after-applied`、`owner:Qxx`、`--auto`、`defer --reason --until`），见 [工作表](worktable.md) |
| `hook stats [--weeks N] [--json]` | 按周统计 Stop 钩子触发与去向（wait/apply/defer/忽略） |
| `events [--since <SEQ>] [--follow] [--rebuild] [--json]` | 读/跟随全机只追加事件流 `<state_dir>/events.jsonl`（单调 `seq`，含起/排队/lane/验收/交付/提问/停/apply/clean/卡警/续派/`review_*`/`merge_*`/`radar_*`/`work_*`）；`--rebuild` 从 run 重建；写失败仅记诊断 |
| `queue <add\|list\|remove\|cancel> ...` | 确定性 DAG 续派队列：支持 `--after`（上游交付通过且审查 `PASS`）与 `--after-applied`（上游合入后），零模型调用自动起下一步（`queue_dispatched`），异常暂停（`queue_paused`） |
| `sign [--check] [--json] [<run>]` | 记录或校验 `controllerReviewPaths` diff 的 SHA-256 签字（`sign.json`）；改一字即失效（`--check` 有效退出 0，未签/失效退出 1），供合入员验签，不改 `apply` 语义 |
| `merge [--ready\|<run>...] [--name <名>] [--json]` | 独立集成 worktree 串行 3-way 合入（重生、逐个 `accept`、末尾 `mergeAccept`，按 `autoMerge` 出 `merge-report.md` 供 `apply --keep-commits` 落回）；`merge list [--all]` 查批次，见 [合入员与 Radar](merge-and-radar.md) |
| `radar [run\|list\|board\|pick\|schedule]` | 跑 `radar.collect` 算指纹，变化时派只读 scout 去重写 `<state>/radar/<repo>/board.md`；`pick <id>` 出草稿，`schedule` 给定时说明，见 [合入员与 Radar](merge-and-radar.md) |
| `status [<run>...] [--all] [--json] [--files]` | 首行计数并只列待处理任务与未完成待办（零锁零副作用直读 `<state>/index.json`；续做链折末环，详情看 `status <run>`，`--all` 全量，`--json --files` 带文件数组），末行指向 `history` |
| `snapshot --json [--repo <路径>...] [--events-limit N]` | 零锁、零子进程一次输出全机 runs、work、events、lane、queue、merges 与 candidates 快照（供 TUI 与工具使用） |
| `note [<run> ["<文字>"\|--done [<序号>\|--all]]] [--all] [--json]` | `work.jsonl` 薄壳：追加、查看或完成 run/续做链待办，自动迁入旧 `notes.json`，`clean` 后保留 |
| `handoff [--out <路径>]` | 生成未清理未合入任务与待办的一页交接索引（默认 `.local/run/delegate/HANDOFF.md`），含净改动、新增 `TODO(master)`、迁移文件与重叠文件 |
| `history [关键词…] [--since <时长>] [--state <状态>] [--limit N] [--json]` | 查本仓库永久追加索引 `history.jsonl`（老的在上新的在下，每条一行指向 `show`）；关键词匹配名字、提示词与结论，中文支持单字加二元组 |
| `show [<run>] [--json]` | 第二层单 run 摘要：提示词前几行、确定性结论段、验收行、合入后源仓库同文件提交（现查 `git log`）、续做链与归档路径，末行指向 `result`/`patch`/归档原文；clean 后从归档读，归档过期说明「原文已过期，仅剩索引」 |
| `board [--path]` | 刷新并打印本工作区的自动委派看板（默认放在 history 目录旁 `BOARD.md`，可用仓库 `.delegate.json` 的 `board` 配置路径）；每次状态变化自动原子重写，顶行含整机在途与各节计数，写失败只记诊断不阻断委派 |
| `result [<run>] [--path]` | 输出完整答复 |
| `stop <run>...` | 终止任务及其 scope、进程组 |
| `clean <run>...\|--finished [--force] [--sync]` | 删除已结束的任务（默认原子移入 trash 并由后台 `_gc-worker` 异步回收，加 `--sync` 同步回收；清理前自动检测是否已合入 HEAD 以解锁 `run_applied` 下游）；`--finished` 默认保留结果未读取的 |
| `lane [--label <文字>] [--measure] [--] <命令>` | 在整机重任务队列里执行命令；`--measure` 使用测量预约核并串行排队（一个参数按 shell 命令执行），退出码原样返回；不带命令时列出正在跑与排队的项 |

启动选项：`--tier scout|review|write`（用途，不写时只读推断 review、写入推断 write，明确提醒并标记；scout/review 自动只读；三档须配齐）、`--image <路径>`（可重复）、`--accept <命令>` / `--no-accept`（覆盖或关闭仓库默认验收）、`--hide-accept`、`--accept-timeout`（可选；默认 `.delegate.json` 的 `acceptTimeout`，未设不限时；无输出仅发 `stall_warning`；`reply` 继承）、`--read-only`、`--shell`（仅 git 快照内只读调查，写文件拒收）、`--in-place`（只读读实时工作区；写入覆盖 worktree 默认并原地写入）、`--workdir`、`--timeout`（每次尝试，可选；没有默认值）、`--retries`（答复畸形时重跑次数，默认 1）、`--model <别名或已登记完整名>`（可与 --tier 同用，只在 models 别名表内精确/唯一子串匹配，固定模型不自动换）、`--thinking off|minimal|low|medium|high|xhigh`（独立长度，模型名不带后缀；reply 也可换模型、档位或长度）、`--allow-parallel-writes`、`--worktree`、`--in <run>`（只读，在已完成任务 worktree 的快照里审它的改动）、`--protect <路径>`（可重复；末尾 `/` 为目录；被改动即判 `rejected` 并带 `protectViolation`，不跑验收；reply 沿用；正文出现未放开的保护路径时启动回显与 status/wait 均提示补 `--allow`，不拦截）。`--name` 省略时取说明首分句生成最多 24 字的可读名称，唯一 ID 规则不变；reply 轮次命名不变。`<run>` 可以是完整 id、唯一片段、`last` 或 run 目录。

`start`/`run`/`reply` 另支持 `--evidence <命令>`、`--no-evidence` 与 `--evidence-timeout`（默认 30m，reply 沿用）。`--model` 固定模型时 JSON `agentPinned: true`，不自动升档；旧 meta 缺字段推断显示不改写。6.0 起移除 `--agent`。

`start`/`run`/`reply` 支持 `--max-answer N`（正整数，reply 继承）：超出 1.5 倍时续接压缩一次；失败时只显示前后截断的 N 字及原文路径、字数、原因，`result` 仍读全文。原文另存 `result-original.md`，不耗返工预算。`start --share PATH` 可重复，repo 内在 worktree 同位置建链接，repo 外只写入任务说明，reply 继承。固定说明与默认参数见 [配置](configuration.md)。

## 证据命令

证据用于真实模型评测等慢且不确定的检查。验收通过后执行；未设验收时在正常答复后执行；验收失败或 protect 违规时跳过。在同事工作目录（隔离任务为其 worktree）通过 lane 排队，用验收相同的 env、原 PATH、独立进程组及 cgroup 回收，deny shim 不影响证据。拿到 lane 名额才开始计算 `--evidence-timeout`。

`start`/`run`/`reply` 可重复使用 `--accept-also <命令>`，按顺序以 `&&` 追加到生效验收；无基础命令则独立运行，与 `--no-accept` 冲突退出 2。

证据命令默认未设置。结果写入 meta 与 JSON 结论的可选 `evidence{exit,timedOut,seconds,tail,log}`：退出码、是否超时、执行秒数、日志末尾和完整日志位置。短行加“证据 通过/失败/超时”。任何证据失败只记诊断，永不改变任务 state、升档判断或命令退出码；它也不替代 `acceptStillValid` 的验收快照依据。

等待检查线：Claude Code 在未给 `--max` 时默认 50m 后打印每个运行任务的进度 glance 并退出 75；`--stream` 每 50m 打印一次并继续。`checkIn`、`DELEGATE_CHECK_IN` 可改时长，`0` 关闭。
