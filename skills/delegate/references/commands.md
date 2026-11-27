# 命令与证据

## 命令与选项

| 命令 | 作用 |
|---|---|
| `start [选项] [任务说明]` | 后台启动，立即返回一行状态；派发前预检路径与符号印 stderr，`--no-brief-check` 可关；`--worktree` 写任务默认先派只读探路出地图（`--no-scout`、`.delegate.json` `"scoutFirst": false` 例外），`queue add` 透传 `--no-scout` |
| `run [选项] [--max <时长>]` | 启动并等到结论 |
| `reply <run> [消息] [--wait] [--fresh] [--sync\|--sync=agent\|--no-sync] [--allow <path>] [--when-done]` | 续接最新一轮会话后台启动（运行中默认直接追加：Pi 支持 RPC `steer` 时轮中即注入，否则同 `agy` 本轮末同会话合并接着答，收尾后退回新 round，`status`/`card`/TUI 显「已追加 N 条，待投递 M 条」，`stop` 取消待投递，`--when-done` 兼容）；`--wait` 等到结论。写入 worktree 续做默认三方同步到当前源快照（保池槽 `target/`，冲突时保旧基线并在 stderr 列重叠文件）；`--no-sync` 跳过、`--sync` 遇冲突拒收、`--sync=agent` 留文本标记交同事先解（未清标记拒收；子模块 gitlink 冲突默认取主干、`.delegate.json` 生成文件冲突提示重生，其余二进制/结构冲突仍拒收）。`--allow` 在保护范围内按链开放路径；`--fresh` 开新会话。验收/证据默认沿用，可覆盖或关闭 |
| `card <run>` | L1 一屏审查卡（摘要与各轮原文、风险项、测试与验收、预合并状态、折叠改动）；L2 风险 hunk 用 `diff <run> --risky`，L3 完整改动用 `diff <run>`，详见 [审查卡与预合并](review-card.md) |
| `patch <run> [-- <path>…]` | 打印保留的完整补丁，可按精确路径/目录前缀过滤，供 `git apply --3way` 恢复 |
| `diff [<run>] [--stat] [--total] [路径...]` | 以 `git diff` 输出该轮的改动；`--total` 为整段对话；终端下带颜色 |
| `files [<run>] [--json]` | 每行输出一个本 run 改动的仓库相对路径（归档后仍可用），便于精确 `git add` |
| `land [<run> …] [-m "<标题>" \| --message "<标题>" \| -F <文件>] [--no-verify\|--verify] [--merge] [--allow-dirty-overlap] [--resume-regenerate] [--dry-run] [--keep] [--keep-open] [--closes <Wxx>] [--json]` | 一站式合入主路径：复用 `apply` 管道合入源工作区（含 `--merge` 与重叠拒绝）→ 在源工作区跑 `.delegate.json` `mergeAccept`（无则跑 run `accept`，忽略 `applyVerify: false`；`acceptStillValid` 且未换命令时跳过并写「沿用交付时验收」；`--no-verify` 显式跳过并写明）→ 仅暂存全链文件集（与 `files` 同口径，含删除）→ 提交（省略 `-m/-F` 沿用同事标题，总附 `Delegate-Run:` trailer，用户 git 配置与钩子照常；`-m`/`--message`/`-F` 仅支持空格传值）→ 成功后自动归档清 worktree。人读 stdout 首行与末行固定 `LAND OK <run>` / `LAND FAILED（<阶段>） <run>`，任一步失败保留已写入与暂存区并给出续做命令 |
| `apply [<run> …] [--dry-run] [--merge] [--allow-dirty-overlap] [--resume-regenerate] [--verify\|--no-verify] [--keep] [--keep-open] [--keep-commits\|--commit [-m "<标题>" \| --message "<标题>" \| -F <文件>]] [--closes <Wxx>] [--json]` | 按序将 worktree 改动合入源工作区（只写入不提交），先检目标文件是否已有未提交改动（忽略本 run 自写相同内容），有重叠默认拒绝并给先提交或 `--allow-dirty-overlap` 两选项；人读 stdout 首行与末行固定 `APPLY OK <run>` / `APPLY FAILED（<阶段>） <run>`，默认不提交并列同事标题，成功倒数第二行印 `delegate files <run>`，遇错即停报 `failedStep`/`writtenFiles`；`acceptStillValid` 不为 true 时末次合入后经 lane 重跑验收（`applyVerify: false` 关闭）。`--keep-commits` 按序 cherry-pick；`--commit` 按全链文件集暂存并压成单提交（支持 `-m`/`--message`/`-F` 空格传值，提交失败时维持取消暂存）；合入默认自动关闭关联工作项（`--keep-open` 强制留 `verify`，`--closes <Wxx>` 显式关闭）；尾部提示解锁下游与 `verify` 留存原因，成功后按 `worktree.cleanOnApply` 归档删 worktree |
| `busy <资源名>` | 有实际运行的匹配命令时逐行列 run、命令和持续秒数，退出 0；空闲无输出、退出 1 |
| `wait [<run>...\|--all] [--since <SEQ>] [--max <时长>] [--no-result] [--full] [--progress]` | 无参和带名字的 `wait <run>...` 默认同 `--any`：返回上次 wait 之后所有进入终态或提问态的 run（首个给全文，其余给一行摘要加 `result` 命令）；`--since <SEQ>` 仅返回 `seq > SEQ` 的事件并输出新游标，同时更新 `cursors.json`；`--all` / `--until-all` 等全部（单次最多 50 分钟）。人读末行给本会话剩余数和原样重跑命令 |
| `work [list\|show\|add\|wait\|close\|drop\|defer\|import-md\|import-dag] ...` | 统一工作表（7 态/6 列/卡片、`--class feat\|unblock\|infra\|docs`、`import-dag` 批量导依赖图、`--after-applied`、`owner:Qxx`、`--auto`、`defer`），见 [工作表](worktable.md) |
| `fill stats [--since 24h] [--repo <路径>] [--all] [--json]` | 按当前 `fill.target` 统计 premerge 冲突率、返工率、未合入峰值、lane 排队占比、每 10 次交付 agy 周额度、只读 run 数与 agy 占比，并提示停止线（仅提醒不自动降档） |
| `hook check [--format claude\|agy\|json] [--cwd <目录>]` | 零锁零 git Stop 钩子内核：单件渐进披露、卡链分与跨仓 30m 穿透、Claude/`agy` 双格式适配 |
| `hook pending [--json] [--cwd <目录>]` | 按优先级每项一行列出全部待办类别、状态与下一步命令（第 2 层视图，只读不写冷却） |
| `hook stats [--weeks N] [--json]` | 按周统计 Stop 钩子触发与去向（wait/apply/defer/忽略） |
| `events [--since <SEQ>] [--follow] [--rebuild] [--json]` | 读/跟随全机事件流 `events.jsonl`；非 `--follow` 人读推进 `cursors.json`（`--json` 不推）；`--rebuild` 从 run 重建 |
| `queue <add\|list\|remove\|cancel> ...` | 确定性 DAG 续派队列：支持 `--after`（上游交付通过且审查 `PASS`）与 `--after-applied`（上游合入后），零模型调用自动起下一步（`queue_dispatched`），异常暂停（`queue_paused`） |
| `sign [--check] [--json] [<run>]` | 记录或校验 `controllerReviewPaths` diff 的 SHA-256 签字（`sign.json`）；改一字即失效（`--check` 有效退出 0，未签/失效退出 1），供合入员验签，不改 `apply` 语义 |
| `merge [--ready\|<run>...] [--name <名>] [--json]` | 独立集成 worktree 串行 3-way 合入（重生、逐个 `accept`、末尾 `mergeAccept`，按 `autoMerge` 出 `merge-report.md` 供 `apply --keep-commits` 落回）；`merge list [--all]` 查批次，见 [合入员与 Radar](merge-and-radar.md) |
| `radar [run\|list\|board\|pick\|schedule]` | 跑 `radar.collect` 算指纹，变化时派只读 scout 去重写 `<state>/radar/<repo>/board.md`；`pick <id>` 出草稿，`schedule` 给定时说明，见 [合入员与 Radar](merge-and-radar.md) |
| `status [<run>...] [--all] [--json] [--files]` | 跨本机已登记仓库（`repos.json` 与活跃 `*.slot`，非当前仓标 `[<repo>]`；远端执行标 `远端:@<host>`/命令/时长，不跨机聚合远端机器本地独立 run）首行计数并列待处理任务与未完成待办（零锁直读 `index.json`；人读默认与 `--all` 均将 reply 链折为末环一行「`<名>（第 N 轮）`」，`--json` 保留每轮，`--json --files` 带文件数组），`status <run>` 展开各轮原文与远端输出尾部，末行指向 `history` |
| `snapshot --json [--repo <路径>...] [--events-limit N]` | 零锁、零子进程一次输出全机 runs、work、events、lane、queue、merges 与 candidates 快照（供 TUI 与工具使用） |
| `note [<run> ["<文字>"\|--done [<序号>\|--all]]] [--all] [--json]` | `work.jsonl` 薄壳：追加、查看或完成 run/续做链待办，自动迁入旧 `notes.json`，`clean` 后保留 |
| `handoff [--out <路径>]` | 生成未清理未合入任务与待办的一页交接索引（默认 `.local/run/delegate/HANDOFF.md`），含净改动、新增 `TODO(master)`、迁移文件与重叠文件 |
| `adopt <run>…` | `/clear` 后接手整链 caller（已归档时支持从归档续接） |
| `history [关键词…] [--since <时长>] [--state <状态>] [--limit N] [--json]` | 查本仓库永久追加索引 `history.jsonl`（老的在上新的在下，每条一行指向 `show`）；关键词匹配名字、提示词与结论，中文支持单字加二元组 |
| `show [<run>] [--json]` | 第二层单 run 摘要：提示词前几行、确定性结论段、验收行、合入后源仓库同文件提交（现查 `git log`）、续做链与归档路径，末行指向 `result`/`patch`/归档原文；clean 后从归档读，归档过期说明「原文已过期，仅剩索引」 |
| `board [--path]` | 刷新并打印本工作区自动委派看板（默认 `BOARD.md`，`.delegate.json` `board` 可配）；状态变化自动原子重写，写失败只记诊断 |
| `result [<run>] [--round N\|--all\|--rounds] [--path] [--no-mark] [--json]` | 输出答复；`--round N`/`--all`/`--rounds` 查单轮、全部或索引 |
| `stop <run>...` | 终止任务及其 scope、进程组 |
| `clean <run>...\|--finished [--force] [--sync]` | 删除已结束任务（默认原子移入 trash 异步回收，`--sync` 同步；清理前先归档全文与补丁，并检测是否已进 HEAD 以解锁 `run_applied`）；`.truncated` 不阻止清理，`--finished` 默认保留未合入工作树与未回报结果 |
| `probe` | 走 Pi/agy 本身测各档已登记模型连通性，不建任务、不占名额 |
| `tui` | 终端看板（`0`/`O` 总览图表、`6`/`A` 账号与真实额度、`Q` 刷新、`Enter`/`u` 切换） |
| `eta [<run>\|<Wxx>\|--work\|--queue]` | 查进度预估与停表归责（`status`/`work`/`queue`/`wait` 已直显），见 [容量与工期](capacity.md) |
| `disk [status\|prune] [--dry-run] [--json]` | 查看主盘/溢出盘水位、溢出槽数与超 24h 占槽提醒（`status`），或按三信号在用判活执行零代价清理、整盘水位全局 LRU 与冷槽溢出/回迁并写 `<state>/disk-health.json`（`prune`，见 [磁盘健康](disk-health.md)） |
| `lane [--label <文字>] [--measure] [--] <命令>` | 整机重任务队列执行命令，结束时在 `lane_released` 记录子进程树 `cpuUserSecs`/`cpuSysSecs`/`maxRssKb`（`events` 与 `--measure` 摘要显示 CPU user/sys、墙钟与 CPU/墙钟 比）；`--measure` 预约核串行排队，退出码原样返回；无参列占用 |
| `agy account <list\|status\|add\|use\|note\|remove\|import-agy-swap\|restore-entry>` / `agy login [<id>]` | 手工管理本地 `agy` 账号槽位与余量快照（`agy login` 为 `account add` 别名，不自动轮换、不调额度接口），见 [agy 账号](agy-accounts.md) |
| `usage [--since 24h\|7d] [--by account\|day\|run\|repo] [--account ID] [--repo PATH] [--all] [--json]` | 按账号/日/run/仓库汇总本地 `agy` 用量（作者与审查员分列，运行中未计单列） |
| `lines [--since <时长>] [--by repo] [--json]` | 统计各已登记仓库默认分支当前总行数与近 24h/7d 增减行数（代码/测试/文档、各仓明细与缓存日走势、委派占比与每百万 token 产出，按 HEAD 缓存，支持 `.delegate.json` `lineStats.exclude`） |
| `escapes [--since 30d] [--repo PATH] [--json]` | 按 `Delegate-Run:` 与 `Fixes-Run:` trailer 统计合入后漏网缺陷（按审查模型、写手模型与类别汇总合入数、漏出数与比例，支持已归档 run，未知 run 单列计数） |
| `speed [--since 7d\|14d\|all] [--repo PATH] [--json]` | 从 `events.jsonl` 按仓库校准工期参数（`t_chain`/`t_crit`/`P_eff`/`T_merge`、返工率、只读链），写 `<state>/speed/<repo-slug>.jsonl` 并标 `±20%` 漂移，见 [容量与工期](capacity.md) |

启动选项：`--tier scout|review|write`（省写时只读推断 review、写入推断 write 并标记；scout/review 自动只读）、`--image <路径>`（可重复）、`--accept <命令>` / `--no-accept`、`--hide-accept`、`--accept-timeout`（默认 `.delegate.json` 的 `acceptTimeout`，未设不限时；`reply` 继承）、`--read-only`、`--shell`（仅快照内只读调查，写文件拒收）、`--in-place`、`--workdir`、`--timeout`（每次尝试，无默认）、`--retries`（畸形重跑次数，默认 1）、`--model <别名或完整名>`（在 models 内匹配，固定模型不自动换）、`--review-model <别名>`（覆盖本次自动审查模型，优先级高于仓库/用户配置与 review 档）、`--review-full`（强制本 run 各审查轮全量审查，跳过增量与同步轮直通）、`--class build|hard|review`（未显式给 `--thinking` 时决定默认思考档：build/review→medium、hard→high，并附适配卡提示）、`--thinking off|minimal|low|medium|high|xhigh`、`--allow-parallel-writes`、`--worktree`、`--in <run>`（只读审已完成任务快照）、`--protect <路径>`（可重复，末尾 `/` 为目录，改动即 `rejected`；正文提及未放开保护路径仅提示补 `--allow`）。`--name` 省略取首分句（最多 24 字）。`<run>` 可为完整 id、唯一片段、`last` 或目录。

`start`/`run`/`reply` 另支持 `--evidence <命令>`、`--no-evidence`、`--evidence-timeout`（默认 30m）、`--max-answer N`（超 1.5 倍续接压缩一次，原文存 `result-original.md`）、`--share PATH`（可重复，reply 继承）与 `--accept-also <命令>`（`&&` 追加，与 `--no-accept` 冲突退出 2）。固定说明与默认参数见 [配置](configuration.md)。

## 证据命令

证据用于慢且不确定的检查：验收通过（或未设验收时正常答复）后经 lane 在同事目录执行，失败仅记诊断、不改任务 state 或退出码，结果写入可选 `evidence{exit,timedOut,seconds,tail,log}`，也不替代 `acceptStillValid`。

等待检查线：未给 `--max` 时默认 50m 后打印运行进度并退出 75（`--stream` 继续）；`checkIn`/`DELEGATE_CHECK_IN` 可改时长，`0` 关闭。
