# delegate

## 一站式合入提交 `delegate land` 与 `apply`/`land` 首末行契约

- 一条命令完成合入、验收、按全链文件集提交与清理：`delegate land <run> [-m "<标题>" | --message "<标题>" | -F <文件>] [--no-verify]` 复用 `apply` 管道合入源工作区，按 `.delegate.json` `mergeAccept` 优先（回退 run `accept`，忽略 `applyVerify: false`；`acceptStillValid` 且未换命令时写「沿用交付时验收」；`--no-verify` 显式跳过并写明）跑验收，仅暂存与 `delegate files` 同口径的全链文件集（含删除）并自动附加 `Delegate-Run:` trailer，成功后自动清理 worktree；任一步失败在人读 stdout 首行与末行固定输出 `LAND FAILED（<阶段>） <run>`（`apply` 同理 `APPLY OK|FAILED`），保留已写入与暂存并给续做命令；`apply --commit` 同步支持 `-m`/`--message`/`-F`（空格形式）及全链暂存（提交失败仍取消暂存）。One-step `delegate land` (apply -> mergeAccept/accept verify -> full-chain staging -> commit with `Delegate-Run:` trailer -> auto-clean worktree), fixed `APPLY|LAND OK|FAILED` first/last stdout line contract, and `-m`/`--message`/`-F` support on both `land` and `apply --commit`.

## 依赖图导入、价值配额补位与停止线统计

- 批量导入任务 DAG 并按价值配额无模型补位：`work add/wait --class feat|unblock|infra|docs` 标价值类别（缺类别不参与补位且 `work` 行尾标「缺类别」）；`delegate work import-dag <FILE> [--dry-run]` 读 JSON 批量建工作项（`after` 转 `work_closed:`、`files` 存路径前缀并附 `--auto` 参数，重 key/成环/brief 缺失整批拒绝）；`.delegate.json` 配 `fill: {"target": 6, "maxInfraDocsShare": 0.3}` 与 `hotspots` 在 `reconcile` 按类别/出度/就绪时间补位（只数写链、跳过热点冲突、记 `work_dispatched` `reason: "fill"`）；`delegate fill stats [--since 24h]` 统计冲突率/返工率/未合入峰值/lane 排队占比/每 10 交付 agy 周额度与只读占比并给出停止线提醒（不自动降档）。Batch-import JSON task DAGs with `work import-dag`, value-class auto-fill dispatch with `fill`/`hotspots` (counting write chains only), and advisory stop-line metrics via `delegate fill stats`.
- 额度预报软门控影子模式与对账：自动补位（`fill`/`work --auto`）与 DAG 续派（`queue`）派发 `agy` 写任务前只读本地缓存（>20 分钟视为未知）判断目标池预计剩余时间是否装得下「在跑写数 + 1」× 近 14 天写任务中位数耗时，装不下时不拦截仅记 `quota_would_hold` 事件（手动 `start` 不记不拦不打印）；用 `delegate quota-shadow [--since 72h] [--window MINS] [--json]` 对账影子事件与真实 429 `RESOURCE_EXHAUSTED` 失败，输出召回率与误报率。Shadow-mode quota headroom check on auto-fill and DAG queue dispatches (cache-only, >20m stale treated as unknown, emitting `quota_would_hold` without blocking, manual `start` untouched) plus `delegate quota-shadow` recall/false-positive reconciliation against real 429 quota failures.

## 审查模型可配置、按类别选 thinking、格式补问与补充审查

- 按配置或单次参数选定审查模型并容错回退：在 `.delegate.json` 或用户配置设置 `review.model` / `review.thinking`（或传 `start --review-model <别名>`，优先级 CLI > 仓库 > 用户 > `review` 档位）；不可用时回退 `review` 档并在审查卡写 `审查改用 <别名>（原因）`，审查卡与 `status` 显示实际模型。Configure reviewer model/thinking with 4-layer precedence, non-silent fallback to the review tier on unavailability, and actual model display in card and status.
- 按任务类别定默认思考档、按 `classModels`/`designPaths` 路由并查看质量 × 价位适配卡：`models.<alias>` 可配可选相对价位档 `cost`（`low|main|high`，旧字符串写法兼容、非法值严格校验报错）；`delegate fit` 与 `delegate tiers` 在每个别名旁展示价位（未设显示 `—`），`fit --json` 带 `cost`，`start`/`reply` 启动回显在模型旁附 `〔<cost>〕`（只显示不自动换模型、不拦截）；`delegate start --class build|hard|review|design` 在未显式给 `--thinking` 时分别默认 `medium`/`high`/`medium`/`high` 并回显 `thinking <档>〔类别 <类>〕`，显式 `--thinking` 优先；用户配置 `classModels`（如 `{"design":"carbon"}`）在未传 `--model` 时为对应类别选模型（未配置退回 tier 默认并提示一行，`--model` 永远优先），未写 `--class` 但说明路径落在 `.delegate.json` `designPaths` 下时自动推断 `design` 并在回执标「推断」（仅按路径，不按关键词）。Model alias `cost` tiers (`low|main|high`) displayed in `fit`/`tiers`/`fit --json` and `start`/`reply` echo without auto-switching or blocking, plus four-class `--class` default thinking (`build`/`review` -> `medium`, `hard`/`design` -> `high`), optional `classModels` per-class default model routing with fallback notice (`--model` always wins), and path-only `.delegate.json` `designPaths` inference.
- 用历史已知结论回放评估候选审查模型：`delegate fit bench review <别名> [--since 30d] [-n 10] [--estimate] [--yes] [--json]` 从本机 history 选最多 n 条有确定结论的写入任务（漏检与检出尽量各半，优先近期、不同仓库），按 `--in <run>` 同口径从归档重建只读快照与 diff 只读重审，输出命中率、漏检数、误报数与总用量并按 `fit set review` 同存储写入 ≤80 字符结论与 `sample`；`--estimate` 仅按原审查用量估算且不起模型调用，缺 `--yes` 仅打印估算不派发，不可重建 run 跳过并计数不中断。Replay historical write runs with known review outcomes (`missed`/`caught`) via `delegate fit bench review` on archived read-only snapshots, requiring `--yes` after cost estimation (or `--estimate` dry-run), skipping unreconstructable runs, and saving the summary into `fit.json`.
- 审查员格式出错同会话补问与 `review.extra` 补充审查：审查员首行缺 `PASS`/`REWORK`/`NEEDS_CONTROLLER` 时在同会话尾部补问 1 次，仍不合格才判 `formatError` 并注明「补问 1 次仍无结论」；配置 `review.extra` 仅在命中 `controllerReviewPaths` 或 `risk:high` 且 `agy-quota.json` Claude/GPT 池余量高于阈值时并行跑第二意见（低于阈值跳过并写明余量，意见不一致时标「意见不一致」并升 `NEEDS_CONTROLLER`）。Single same-session format-error follow-up and trigger/quota-gated supplementary review (`review.extra`) escalating disagreements to `NEEDS_CONTROLLER`.

## agy 按账号与时间段用量统计与余量快照推算

- 换号后查各账号近 24 小时或 7 天用了多少 token：`delegate usage --since 24h --by account` / `--since 7d --by day|run|repo --json`，作者与审查员轮次分列，审查员用量单独落在 `review/round-N/usage.json` 并汇总进 `summary.json`（只记活跃 `agy` 槽位 ID、不记邮箱），运行中拿到累计用量实时更新，拿不到单列「运行中未计」。Summarize local `agy` token usage by account slot, day, run, or repo with author vs reviewer split and uncounted running runs listed separately.
- TUI 额度页与同周期耗尽预测：`delegate tui` 额度页展示各账号近 5h/7d 本地 Token 用量与 `/usage` 剩余额度，基于同一 `resetAt` 周期的 `/usage` 剩余 `%` 历史（不依赖本地 token 标定）结合在跑并发给出近窗斜率区间预测与数据不足/空闲门控，结果不超过 `resetAt` 且仅针对当前选用账号与池告警（`auto` 同池有绿档可接管时静默）；`delegate agy account note <id> --weekly 95.41 --five-hour 85.10` 仅登记手动备注快照。Local 5h/7d token usage charts in TUI Quota tab plus same-cycle `/usage`-only depletion interval forecasting with insufficient-data/idle gates, resetAt capping, and selected-pool-only alerts.

## 6.38 agy 本地多账号管理与迁移

- 管理多个 `agy` 账号替代 `agy-swap`：`delegate agy account add <id>`（别名 `delegate agy login <id>`）在隔离临时 HOME 跑交互登录，检测到令牌落盘后在终端提示按两次 Ctrl+D 退出保存，按存在将凭据文件（最少需要 `antigravity-cli` 令牌，`0700`/`0600`）收入 `~/.config/delegate/agy-accounts/<id>/`，不影响当前活跃账号并提示其他机器跑 `lan-agent-sync`；`list`/`status` 展示活跃槽位、最后使用时间与在跑 `agy` 进程，永不打印令牌、不调外部 Google 额度接口、不做自动轮换。Local manual `agy` multi-account slots with 0700/0600 permissions, isolated temp-HOME login (`agy login` alias, Ctrl+D save hint), no auto-rotation, and no external quota polling.
- 手动切换与防竞态：`delegate agy account use <id>` 持排他锁，先将当前活跃账号被 `agy` 刷新过的最新凭据回存到原槽位再原子替换并清理残留 `credential-user-scope`；同 HOME 存在在跑 `agy` 进程时拒绝切换并列出 PID 与 delegate run。File-locked manual account switch saving back refreshed tokens first and refusing while `agy` processes run in the same HOME.
- 迁移与 TUI：`delegate agy account import-agy-swap --dry-run` 预览从 `agy-swap` vault/accounts 导入的槽位且不打印令牌，`restore-entry --dry-run` 预览备份并恢复 `~/.local/bin/agy` 为直连 `~/.local/libexec/agy` 的最小包装或符号链接；`delegate tui` 新增「6账号」页（`6`/`A` 切页、活跃标记、`Enter`/`u` 就地确认切换）。Dry-run/execute `agy-swap` import, entry-point restoration, and TUI Accounts tab with in-place switch confirmation.

## 6.37 返工/续做各轮答复保留与审查卡索引（W051）

- 审-改自动返工（或 `reply` 续做链）保留各轮原文：触发 `REWORK` 前将上一轮答复持久化为 `round-N-result.md`（不覆盖旧结论与实测数字，`result.md` 始终镜像最新一轮；归档 `archive/<run>/record/` 与轻量保留均完整保存并纳入证据引用扫描）；`delegate card <run>` 第 1 段在多轮时确定性列出每轮原文路径与行/字数（单轮不变）；`delegate result <run>` 支持 `--round N`（越界给 `1..=N` 范围）、`--all`、`--rounds`（不标已读）、`--path` 与 `--json` 查看任意轮或全部轮次，包括已归档的父 run。Preserve per-round results (`round-N-result.md`) across review-rework and reply chains (including archived ancestors), link every round's original path in review card section 1, and inspect rounds via `delegate result --round/--all/--rounds/--json`.

## 6.36 Stop 钩子合一、工作表与审查卡误报修复

- 统一 Stop 钩子（零锁、零 git）：`hooks/claude-code-stop.py` 合并 waiter/unmerged/事件游标拦截且只读 `state/index.json` 与快照，识别 PATH 裸 `delegate wait` 与带路径写法，跳过进行中的 `apply`/合入员（含 `_merge-worker`）/门禁（`lane`）与零改动交付；延后走 `delegate work defer --reason … --until …` 并删掉钩子侧 `deferred.tsv`；每次拦截写 `<state>/stop-hook.jsonl`，`delegate hook stats` 按周统计触发与处理去向；`scripts/install-claude-hooks.sh` 仅装单一 Stop 钩子并移除旧脚本配置。Unified zero-lock/zero-git Stop hook with bare/path waiter matching, in-flight apply/merge/lane & zero-change exemptions, work-table deferrals, stop-hook.jsonl logging, and weekly `delegate hook stats`.
- 工作表误报修复（W044）：已 clean/归档的 run 不再影响工作项状态（带可判定前置条件的项如实保持 `blocked`），零改动交付在工作表里显示为「已收取」而非 `attention · 待合 +0f`。Cleaned/archived runs no longer drive work item state and zero-change deliveries display as 已收取.
- 工作表去掉 `--wait-manual`、所有者事项必填与 `reply --minor` 无默认时限：`work add`/`wait` 不再接受 `--wait-manual`（旧 `manual:` 前置读 `work.jsonl` 时一次性迁入 `followup` 并写回与记 `work_migrated` 事件），`work wait` 支持 `--needs-owner nod|act|card` 与补齐 `Qxx` 四字段；`--needs-owner` 及非 `direction` `--owner-kind` 必须同时提供 `--ask`（非标题问句）/`--recommendation`/`--if-undecided`/`--cost`，`work show`/TUI Q 卡新增「在等它的工作」，既有缺字段 Q 项在 `work --owner` 和 TUI 行尾标「缺建议」；`reply --minor` 去掉默认 10 分钟硬超时（显式 `--timeout` 仍生效）。Remove `--wait-manual` with one-time followup migration, require full Q-item fields on `--needs-owner`/`--owner-kind` (with `work wait <Qxx>` backfill, waiting-work section on Q cards, and `缺建议` tail tags), and remove the default 10m timeout on `reply --minor`.
- 审查卡误报修复：lockfile 仅本包自身版本号变化不报依赖风险；跳过标记按语言与语法识别（`sys.exit` 不误命 `xit`）；`[&str; N]` 数组长度、`7.min(len)` 切片截取与普通字段 `unwrap_or` 不误报配置/常量风险。Review card false-positive fixes for lockfile self-version bumps, language-aware skip markers, and config/constant heuristics.

## 6.35 后台预合并与快进 apply

- 写入任务交付（或命中 `controllerReviewPaths` 后完成 `delegate sign`）：后台按仓库单实例拉起 `_merge-worker`（`LOCK_NB`），在 `<cache>` 持久化集成 worktree（复用 `target/` 与 `node_modules/`）做源 HEAD + 净 diff 三方合并并只跑命中的 `acceptRules` 层（未配则跑 `accept`），钉 `refs/delegate/premerge/<run>` 并写 `premerge.json`；L0/L1 与 TUI run 行、工作表「此刻在干什么」实时展示 `预合并中` → `快进 ✓（基于 <short>）` / `快进 ✗ 主干已前进` / `预合并冲突：<文件>`。Background per-repo `_merge-worker` premerge with persistent cache worktree, pinned premerge ref, and live L0/L1/TUI fast-forward states.
- 主控审完审查卡执行 `delegate apply`：在 `apply.lock` 内命中源 `HEAD == baseHead` 且 `changedPaths` 无脏改时 2 秒内直接写出 `mergedTree` 改动（保持未提交语义）并跳过复验；主干前进或有重叠脏改时失败关闭回退三方合并并输出一行原因（如「主干已前进 2 个提交，走三方合并」），零锁 `status` 发现主干前进后自动唤醒 worker 重新预合并。Fail-closed fast-forward apply (<2s, uncommitted semantics, skips re-verify) with explicit fallback reason and zero-lock stale re-premerge wakeup.
- 预合并冲突/验收失败与并发竞态：写 `premerge_conflict` / `premerge_failed` 事件带冲突文件与日志指针，绝不自动续做或 `autoApply`；worker 运行中主控提交、双 run 同时交付、预合并期间 `apply`、worker 被 `SIGKILL` 均保持源工作区完好。Conflict/failure events with log pointers without auto-rework, and fail-closed concurrency/SIGKILL safety.

## 6.31 统一工作表与适时提示

- 全机统一工作表与 7 态状态机：`delegate work` 展示第 0 层计数与 6 列紧凑表（`id`/`repo`/`title`/`state`/`focus`/`next`，单行 ≤100 字，自动把未绑定活跃/待处理 run 折叠合表为 `R:<id>`，默认折叠 `blocked`/`deferred`）；`delegate work show <ID>` 展第 2 层卡片与宿主写出的第 3 层原文指针；`work close --evidence`、`work drop --reason`、`work defer --reason --until` 显式管理闭环与暂缓，预留 `artifacts[]` 与 `followups[]`。Unified machine-wide work table (`<state>/work.jsonl`) with 7 states, 6-column table merging ephemeral runs, Layer-2 card with Host-written pointers, and explicit close/drop/defer.
- `wait_on` 前置与 `--after-applied`：`work add`/`work wait`（及 `queue add`）支持 `run_applied:<run>`（`--after-applied`）、`run_delivered:<run>`、`work_closed:<Wxx>`、只读扫描 `docs/owner/README.md` 的 `owner:<Qxx>`（绝不反写）、`after_time:<ISO>` 与 `--auto` 自动派发；`start --work <Wxx>` 绑定工作项并由 `reply` 自动继承 `workId`。Structured `wait_on` evaluation including `--after-applied`, read-only `owner:Qxx` scanning, `--auto` dispatch, and `start --work` / `reply` inheritance.
- 零前缀适时提示与 Stop hook 分级：`wait`/`wait --since`/`apply`/`merge`/`clean` 仅在命令 stdout 尾部追加 1–4 行带原文指针的上下文提示（上游合入立即解锁并打印可执行 `start --work` 命令；`apply` 后有 `done_when`/`artifacts`/未完成待办留在 `verify`；`clean` 仅印 1 行保留确认不阻断）；`claude-code-stop.py` 分级只拦当下阻断项（未读告警事件含 `work_ready`/`work_defer_expired`/`work_stale`、提问、无 waiter、未暂缓未合入），不因 `blocked`/`deferred`/未超期 `verify` 反复拦。Zero-prefix command-tail timely hints with Host-written pointers and graded Stop hook blocking.
- 平滑迁移与 `import-md`：首次访问自动迁入旧 `<runs>/notes.json`（到 `verify`）与未过期 `.local/delegate/deferred.tsv`（到 `deferred`），`delegate note` 保留为 `work.jsonl` 之上的兼容薄壳；`delegate work import-md <file> --dry-run` 解析 Markdown 待办并比对 `history.jsonl` 标出疑似已合入/重复项，带源文件行号指针供人工确认。Automatic idempotent migration of `notes.json`/`deferred.tsv`, thin-shell `delegate note`, and `work import-md --dry-run` with history cross-checking.
- 审查员只读守卫忽略依赖与构建产物：只读审查员跑 `pnpm`/`cargo`/`pytest` 产生的未跟踪 `node_modules`、`target`、`.venv`、`__pycache__`、`dist`、`test-results` 及 `.gitignore` 忽略路径不再触发 `readOnlyViolation` 误升级；改动 git 跟踪文件或写入未忽略新文件仍失败关闭为 `NEEDS_CONTROLLER`。Read-only reviewer guard excludes untracked dependency/build dirs and `.gitignore`-ignored paths while still failing closed on tracked or unignored writes.

## 6.30 合入员与发散侦察（Radar）

- 串行合入员与 `merge-report.md`：`delegate merge --ready` 或 `delegate merge <run>...` 基于源 HEAD 在独立集成 worktree 串行 3-way 合入，命中 `generated.inputs` 重跑生成，逐个经 lane 跑 `accept` 并于末尾跑 `mergeAccept`（默认等于 `accept`），原样沿用同事提交标题附 `Delegate-Run: <run>` trailer；遇冲突、测试变红、命中受保护路径或未签字/失效的 `controllerReviewPaths` 原子回退到上一个全绿提交并标 `SKIPPED` 后继续；主控用 `delegate apply --keep-commits <merge-run>` 拿回提交链自行 push。Serial integration merger in an isolated worktree with rollback-and-skip, per-run accept + mergeAccept, and verbatim commits with Delegate-Run trailers.
- `autoMerge` 自主合入白名单：`.delegate.json` 配置 `autoMerge`（支持 `!` 排除与 `add:` 纯新增，如 `docs/**`、`!docs/decision/**`、`add:**/tests/**`、`apps/web/src/styles/**`）；全部改动落在白名单且审查 `PASS` 的 run 标「可自主合入」，其余标「待主控点头」。Auto-merge glob whitelist with negation and add-only modifiers gated on review PASS.
- 发散侦察（Radar）与候选板：`delegate radar` 顺序运行 `.delegate.json` `radar.collect` 并计算指纹，未变时零 token 退出并记 `radar_unchanged`；变化时派只读 scout（禁写文件、禁派活），Host 校验每条候选的证据锚点（`文件:行`/commit/输出行号，无锚丢弃）并按指纹去重写 `<state>/radar/<repo>/board.md`；主控用 `delegate radar pick <id>` 生成任务说明草稿不自动派活，`delegate radar schedule` 输出 00:30–04:30 每 2 小时定时安装说明。Deterministic collect-and-fingerprint Radar scout with zero-token unchanged exit, Host-validated evidence anchors, deduplicated board, and draft-only pick.
- 事件流与 TUI：`merge_started`/`merge_item`/`merge_done`/`radar_unchanged`/`radar_done` 接入 `events.jsonl`；`delegate tui` 新增「3合入」与「4候选」标签页（`1-4`/`e`/`m`/`b` 切页）。Merge/radar events in the event stream and new Merges/Candidates tabs in delegate-tui.
- 编译并发共享令牌池与动态公平分配：可用线程取 `available_parallelism()`、cgroup v2 `cpu.max`（向上取整）与 `nproc` 最小值，`budget = max(1, 线程 - 2)`，`maxHeavy` 默认 `max(2, budget / 12)`（内存/IO 保护），CPU 由 `<state>/jobserver.fifo`（`budget - maxHeavy` 令牌）与 `lane` 进程内代理线程按资历权重（`1, 1/2, 1/4…`，每条保底 2）动态公平分配（注入 `MAKEFLAGS`/`CARGO_MAKEFLAGS`/`CARGO_BUILD_JOBS`，用不满的份额让给他人，结束升满预算，被杀自动校正回收；`heavyJobs` 显式覆盖；远端不做代理照旧相除注入 `CARGO_BUILD_JOBS`），`delegate lane` 与 TUI 底栏显示池子与各命令份额（如 `令牌 88/88 空闲 31 · #1 61/62 · #2 31/31`）。Shared machine jobserver FIFO with in-process per-command dynamic seniority-weighted fair-share proxy, surplus yielding, kill recovery, explicit heavyJobs override, and live pool/share display in lane and TUI.

## 6.29 审-改闭环、`controllerReviewPaths` 与签字哈希

- 自动审-改闭环：仓库 `.delegate.json` 开启 `review: {"tier": "review"}` 后，写入任务验收通过自动派新上下文只读审查员（不能写文件、不能派活），输入任务说明、`diff --total` 与审查要点（默认 `references/review-rubric.md`，可覆盖）；首行严格 `PASS`/`REWORK`/`NEEDS_CONTROLLER`（格式不对失败关闭为 `NEEDS_CONTROLLER`），写进 `review.json` 与 `summary.review` 并联动 DAG 续派队列。Automatic read-only review loop with controlled first-line verdicts and fail-closed format validation.
- 返工原文透传与增量续审：判定 `REWORK` 时 Host 把审查结论文件原样退回作者（不经模型转述），作者交付后审查员续接自身会话且只看本轮增量 diff；不设轮数上限，第 3 轮起发 `needs_attention` 事件提醒主控并照常继续不强杀。Verbatim rework handback, incremental re-review, and unlimited rounds with round-3+ needs_attention alert.
- `controllerReviewPaths` 与 `delegate sign`：本链改到通配符命中的路径（如 `SKILL.md`、提示词文件）时，无论审查结论为何均升级为 `NEEDS_CONTROLLER`，交付附命中路径 diff 全文与 SHA-256 签字哈希；主控执行 `delegate sign <run>` 落盘 `sign.json`，受控路径 diff 改动任意一字即令 `delegate sign --check <run>` 失效；不改 `apply` 语义。Controller review escalation for sensitive paths with SHA-256 diff sign-off invalidated on any single-character edit.
- 事件流、Stop hook 与 TUI：`review_started`/`review_verdict`/`rework_sent`/`needs_attention` 接入 `events.jsonl`、`events --rebuild` 与 Stop hook（仅对 `needs_attention` 与 `NEEDS_CONTROLLER` 拦截未读）；`status` 与 `delegate tui` 实时显示「审查中 / PASS / REWORK 第N轮 / NEEDS_CONTROLLER」与签字状态。Review lifecycle events, Stop hook unread alerting, and status/TUI display.

## 6.28 确定性事件流、transcript 判活、Stop Hook 竞态锁、卡住只提醒与 DAG 续派

- 全机事件流与游标等待：每次起、排队、拿/放 lane、交付、提问、验收、停止、apply、clean、卡住提醒与续派推进都追加 `<state>/events.jsonl`（单调 `seq`，写入失败仅记诊断不阻断 run）；`delegate events [--since SEQ] [--follow] [--rebuild] [--json]` 可回放、实时跟踪或从 run meta 重建，`delegate wait --since SEQ` 只返回 `seq > SEQ` 的事件并输出新游标。Deterministic append-only event stream, rebuild from run metas, and cursor-based wait.
- Stop Hook 竞态锁：主控在 Claude Code 准备结束回合时，`hooks/claude-code-stop.py` 核对 `cursors.json` 之后是否存在未读的交付/提问/卡住提醒/续派暂停事件；有则拦截并返回「有新事件，先看 delegate events --since N（未读 K 条：…）。」，待 `wait`/`result` 推进游标或 `stop_hook_active` 为 true 后放行。Stop hook race lock preventing premature turn exit on unread colleague events.
- 判活看 transcript 尾部：`delegate status` 与 `delegate tui` 读取同事 transcript/事件/错误日志尾部，区分等待中、在思考、在跑命令（带命令名与已跑时长）、配额或报错，并展示「正在做什么」。Activity and liveness classification from colleague transcript tail.
- 卡住只提醒不强杀、验收默认不设超时与机械重复自检：`agents.rs` 遇到 idle/command 停滞或连续重复同一命令+同类结果（ADR 0120，默认阈值 12 次）不再 `kill_group`，验收也不再默认 10 分钟硬超时（仅显式 `--accept-timeout`/`.delegate.json` `acceptTimeout` 生效，验收长时间无输出发 `stallKind: "accept"` 带已跑时长与最后一行输出且不杀进程），统一写同事不可见的 `stall_warning` 供主控和 TUI 查看，仅主控显式 `stop` 才杀进程；旧 6.27 `stalled` 记录照常可读。Warn-only stall, unlimited default acceptance with silent-output stall warning, and mechanical repetition self-check without auto-kill.
- 确定性 DAG 续派与 TUI 事件页：`delegate queue add <name> --after <run|name> --prompt-file …` 建立有序队列，前一步交付、验收通过且（若有 6.29 审查结果）审查 `PASS` 时零模型调用自动派下一步，失败/提问/`NEEDS_CONTROLLER` 则暂停并写 `queue_paused` 事件；`delegate queue list` 与 `delegate tui`（含新增「事件」标签页）均实时展示。Zero-LLM deterministic DAG continuation queue and TUI Events tab.

## 6.27 续做链净改动口径、主干漂移单列与默认基线同步

- pool slot 续做链进行中，主干推进若干提交（含受保护路径）：`status` 只报本链自身改动数、`diff --total` 不含主干变更、不误报 `protectViolation`、不产生无关 `commitsDiffer`；`status <run>` / `wait` 单列一行「主干已前进 N 个提交 / M 个文件，与本链改动重叠 K 个」，仅重叠非零时加 `!!!` 醒目提示。Chain-net diff accounting in pooled replies with separate drift line highlighted only on overlap.
- 写入任务续做（默认开启基线同步）：无冲突时自动把本链净改动合到当前源快照、重放同事提交、保留 pool slot `target/` 增量缓存，并在续做提示附「基线已同步到主干 `<短 hash>`」；遇冲突则原子保持旧基线，在 stderr 列出重叠文件并提示 `--no-sync` / `--sync=agent`。Default reply baseline sync preserving incremental build cache with conflict fallback.

## 6.26 apply 透明化与 start 不阻塞

- `apply` 写完文件后生成器失败（或冲突、cherry-pick 中断）：`apply.ok` 为 false 时 JSON 与人读输出均写明 `failedStep`、`failureDetail` 与已写入源工作区的 `writtenFiles`；Markdown 围栏代码示例与 Setext 标题 `=======` 不再误判冲突标记。Actionable apply failure diagnostics with failedStep, writtenFiles, and Markdown conflict-marker accuracy.
- 仓库配置 `applyVerify: false` 且主干前进后执行 `apply`：末行说明「主干已前进，验收快照失效；本仓库配置合入后不自动验收，批量门禁时跑：…」，不再误导为「验收无效」；真正失败仍醒目输出。Factual wording for applyVerify=false while keeping real failures prominent.
- 不带 `--keep-commits` 合入同事的多条提交：`apply` 提示「改动未提交：N 个文件」并列出同事原提交标题供复用；加 `--commit` 仅暂存本次写入路径并压成一条提交，暂存区脏或三方合并含本地未提交改动时失败关闭。Uncommitted apply reminder with commit titles and fail-closed --commit squash option.
- `apply` 正在跑耗时生成器时前台执行 `delegate start`：立即拍照返回不被阻塞；若撞上 `apply` 短暂写盘锁，stderr 打一行等待进度并在写盘结束立即继续。Non-blocking start during apply regeneration with brief write-lock progress notice.

## 6.25 委派留痕、分层可查与查重

- 280+ 历史 run 的仓库执行 `status`：首行计数（运行中/待合入/待回答/排队），只列待处理项，末行「另有 N 条已结束：delegate history」；读过 `result` 或 `apply` 过不再淹没列表，`--all` 保持全量。Actionable default status with counts and history pointer.
- 查以前派过什么任务：`delegate history 头像 --since 14d` 按老到新每条一行指向 `delegate show <run>`，支持中文单字+二元组匹配；`delegate show <run>` 展示提示词、确定性结论段、验收行、现查 apply 后同文件 `git log`、续做链与 `result`/`patch`/归档原文入口，归档 LRU 过期后说明「原文已过期，仅剩索引」。Progressive disclosure history index and Layer-2 show.
- 派新任务时同一件事换个说法：`start`/`run` 与在跑及近 14 天 history 做词重叠比对，stderr 提示最多 3 条「相似：… · delegate show <run>」，只提示不拦截。Advisory pre-dispatch deduplication.
- 自动委派看板与无硬上限归档：每次 start/排队转运行/finish/reply/result/apply/clean/stop/crash 自动原子重写仓库配置的 `board`（默认 `history` 旁 `BOARD.md`），`delegate board` 输出同份内容，顶行含整机在途与各节计数；归档不设总量硬上限，仅文件系统临满时淘汰最久未读 `events.jsonl.gz`，核心文件与 `history.jsonl` 永不删。Auto-maintained delegation board and capless archive with critical-FS trace trimming.

## 6.19 Remote 构建

- 本机 Agent 中途 cargo fmt/check：默认远端独立 lane、回传改动/删除和原始退出码，诊断路径可点开本机；measure 留本机、agentDeny 先拒绝，resources 不登记本机。Remote heavy-command routing and sync.
- 仓库写 remote.host 即使关闭 remote 仍拒绝；用户别名 ci、仓库逐字段覆盖或 false、DELEGATE_REMOTE=0、config/help 显示有效值。Private host configuration.
- SSH/rsync 离线退出 75，绝不调用本机 cargo；验收显示基础设施失败，预算为零也能续做。Fail closed, no rework charge.
- stop/timeout 终止远端组及子进程；非池槽 clean 删除、池槽温热缓存保留；lane/status 列持有者和排队，排队计入 lane 等待；setup/evidence/verify 同样转发，remote check 一行连通结论。Cancellation and visibility.

## 6.18 证据与最终树

- 测量答复引用 `.local/report` 和原始轨迹后 clean：默认归档非构建忽略输出，status/result 指向 archive；target/node_modules/cache 不收，明确引用例外，200 MiB 超限列跳过大文件，`--no-archive` 放弃；按最近使用/年龄淘汰。Evidence survives clean.
- 源工作区并行 commit，status/wait 的所有 Git 命令仍带 `GIT_OPTIONAL_LOCKS=0`，不打开可选索引锁。No optional source index locks.
- 同事已提交实现却留函数/新文件未提交：一行状态和 apply 显示 “commits differ from the final tree” 与文件列表；默认 apply 取最终树，`--keep-commits` 拒绝不完整提交。Final tree versus commits.
- 普通 `reply --sync` 冲突不改树；`--sync=agent` 留文本标记，说明首行列文件要求先解，标记残留即使 accept true 仍拒收，普通续答继承检查；子模块指针（gitlink）冲突默认取主干，生成文件（`.delegate.json` `generated.paths`）冲突不写标记并提示运行生成命令重生，其余二进制/结构冲突失败不部分同步。Agent resolves sync conflicts.

## 6.17 长会话控制摩擦

- 仓库默认保护 apps/，本轮授权前端：`start --allow apps/web/` 回显开放、提示仍保护其他路径，修改 web 不拒收，reply/fresh 继承；仓库配置不变。Per-run protection lift.
- 同事仍在跑但主控已有补充：直接 `reply impl "…"`（或 `--prompt-file -`，`--when-done` 为兼容别名）即追加到只追加信箱，支持多条按序编号合并；支持 RPC `steer` 的 Pi 在当前工具调用结束后立即注入，`agy`（管道 stdin）及无 `steer` 的 Pi 在当前轮结束后同会话合并接着答，收尾后才到的追加自动退回结束后开新 round，`status`/`card`/TUI 显示「已追加 N 条，待投递 M 条」，`stop` 取消待投递，各轮答复可 `result --round` 查看。Live append mailbox (steer / turn-boundary / fallback).
- 停掉尚未开始的 after 任务与另一路正在跑的任务同时 wait：any 不收停止/跳过，只收自主结束，all 仍等全部离开活跃态。Ignore controller stop/skip in any-wait.
- 池槽 apply 与无改动 reply 的重复 apply/merge：槽目录保留、不误报移除失败，相关轮次显示已合入，clean 不报 never applied。Applied round bookkeeping.
- 编译中锁住源观察锁与容量锁：status 仍及时返回缓存，显示 busy、不重新快照/diff 已结束任务。Cached nonblocking status.
- Bash 在源目录 cat/rg/sed 查询放行，cd 后写文件或直接重定向到源树拒绝一行并指出 worktree；源 .local 日志允许；cp/rsync/install/ln 从源树到 worktree 放行，反向、源树内复制与目标目录命中源树拒绝，单行指出写入路径。变量/脚本/非 Bash 的检测边界必须说明，不声称权限隔离。Source checkout write accident guard.

## 6.16 进程边界与选模适配

- 并行同事收尾 `pkill -f sleep` / `killall sleep`：只结束本次 run 所有 scope 的匹配进程，运行外同名进程仍在；`pgrep` 仅返回本运行 pid，只读 `--shell` 同样。无 systemd/cgroup 则非零并提示 `kill <pid>`，零匹配保留退出码 1。Scoped process matching fails closed without cgroups.
- 别名对象配置默认 medium、顶层 high：直接选别名或 tier 引用均用 medium，CLI 可覆盖，启动只在一处显示档与来源；强模型按规格实现先 medium，安全/并发/根因用 high。Model alias thinking defaults and precedence.
- 首次同类实现任务（含照主控定稿做界面）给两个别名同一规格对比，以 `fit set build … --sample spec.md` 记一句结论；第二个 first 自动降级旧 first，81 字 note 拒绝；`fit` 三类卡（build/hard/review）列未评，新增别名照 sample 重评；`--class scout`/`design` 退出 2 并提示改用三类。`start --class` 提示 avoid、首选、未评，不阻断；不带 class 不变。Fixed three-class model suitability card and advisory routing.

## 6.15 并行合入不丢改动

- 九路写入先合一路留下标记，再合重叠路径：apply/dry-run 整体拒绝，列文件与原 run；无交集照常，status 在解决前显示「已合入·有冲突待解决」。Reject nested conflict markers before writing any target file.
- 合入后发现单个文件要重来：`patch <run> -- <path>` 输出保留补丁供 `git apply --3way`，原 run 记录仍在、池槽已释放。
- 提示含源目录绝对路径：write 提示提醒仅在 cwd 写，启动给 note；自己的树零改动而源有新差异时显示「写到了源仓库」，其他路 delegate apply 不算误写。
- `wait --all r1 r2` 中 r1 已交付、r2 排队：仍等 r2；超时列 r2，默认 wait 的首行解释旧交付。
- protect 与 generated 路径重叠但仅改文档：收尾与 apply 都不跑无关生成器，保留 not regenerated 诊断。
- 强模型只读查日志 / SELECT：`--tier review --model <别名> --shell` 在快照执行，文件变化判「只读任务产生了写入」，任何只读任务禁止 apply。

## 6.14 容量排队与模型预算

- 整机或仓库到上限、内存不足：`start` 成功返回 run id，`status` 显示排队分钟数、原因与前方人数，JSON 含 `queue`；不用手动重派。Queue automatically at capacity; wait for queued tasks normally.
- 便宜模型 maxActive 满了，没同意改用更贵模型：默认等原模型；只有明确接受下一候选时才用 `--overflow next` 或条目 `overflow: "next"`，`--overflow queue` 可覆盖配置。Pinned --model obeys the same resolved-model cap.
- 先排的新任务与后排的 reply 抢名额：reply 先，同级 FIFO；模型 A 满不挡模型 B，排队不占工作树池位，`stop` 可取消、`clean` 可清理。

## 6.9 派发前探连通

- 某个自部署节点今晚回 502，换档前先看哪几档不通：`delegate probe` 走 Pi 本身并行测各档模型，每行 ok/FAIL 与耗时，失败附 Pi 报错第一行；末行给出不带 `--tier` 时 start 会选的模型。Probe tier models through Pi before dispatching.
- 只想确认某个模型现在能不能用：`delegate probe --model provider/id[:thinking]`，参数错误退出 2。
- 峰时换档的规则模型现在会不会被选中：`delegate probe --tier review`，行里带 hours 命中与 maxActive 当前占用；窗口外的模型通了也不算推断用途首选失败。

## 5.28 测量预约与验收冲突

- `measure`/`measureCpus` 配置和 `lane --measure` 让压测、计时、编译耗时命令按预约核 FIFO 排队；排队时间不计 stall/timeout，命令可读取 `DELEGATE_MEASURE_CPUS` 与 `DELEGATE_MEASURE_CORES`。
- 验收或说明正文命中 `agentDeny` 时，start/reply 在 stderr 警告并把命令列为主控负责，启动仍成功。

## 5.27 stall 与 check-in

- 未给 `--timeout` 的任务不会继承 Pi 默认上限；idle 30m、command 90m stall 可由环境或配置缩短，停滞状态保留改动且 reply 不耗返工。
- Claude Code 无 `--max` 的 wait 在 55m 输出进度 glance 并退出 75；`wait --stream` 按同一间隔输出 glance 后继续，行尾给出原命令供重跑。

## 5.26 真实主控使用回归

- apply 后 `reply`：轻量记录仍能读原说明与答复，自动从当前 HEAD 新建写入 worktree；`--fresh` 只用新消息。Continue an applied run with its original context.
- 同事 worktree 有两个提交：apply 总是逐个列短哈希、标题和文件数；`--keep-commits` 与 `defaults.keepCommits` 按序 cherry-pick，冲突停下。
- `--max-answer 1800` 压缩失败：交付显示不超过 1800 字答复，末行有原文路径、字数和失败原因，`result` 可读全文。
- `start --share .local/reference --share /tmp/reference`：repo 内同位置建链接，外部只附可读输入行；reply 沿用，目标已存在时报错。
- `resources.pg.commands` 命中同事、验收或 lane 的 `cargo xtask infra-test`：执行中 `busy pg` 退出 0 并列占用，结束/进程消失后退出 1；status 加“占用 pg”。
- `acceptBlind` 命中 Dockerfile、compose 文件：已交付状态不变，短行、JSON 与 apply 末行标出最多五个盲区路径。
- Claude Code 钩子拒绝 `delegate wait &` 与分析脚本混发：给出原 wait 子命令及两条命令的拆分步骤；纯 wait 原提示不变。
- 通过符号链接调用 delegate：wait 的“原样再跑”与结论 `next` 采用该链接的绝对路径，不解析到安装目录。

## 5.25 少写少选

- 主控在三份任务说明里手抄同一段仓库规矩：仓库 `.delegate.json` 设 `standing` 后，start 回显“附加 N 行（repo）”，`prompt.md` 末尾含该文字与生效的 deny 命令及 hint；普通 reply 不重复，`--fresh` 重附。Standing instructions from config are appended to every task prompt and echoed at start.
- 主控连放三个任务后只跑 `delegate wait`：交付第一个结束的并返回，末行写“还剩 2 个…再跑 delegate wait”；全部收完末行写没有在跑的任务。`--until-all` 才等到全部结束。
- 侦察答复 6817 字：默认上限 20000 时全文显示；上限 6000 时 6817 ≤ 7500 仍全文显示；`--max-answer 900` 且答复超过 1350 字时同一会话追问压缩一次，`result.md` 为压缩后答复，原文在 `result-original.md`，不耗返工预算，失败保留原答复。
- 仓库设 `defaults: {"worktree": true, "protect": ["docs/decision/"], "acceptAlso": ["make lint"]}`：不带选项的写入任务进 worktree、带保护与附加验收，启动回显各项来源；`--in-place` 退回原地；CLI 的 `--protect` 覆盖整项。未配置时行为与 5.24 相同。
- `apply` 成功后该任务的 worktree 已释放、run 轻量记录和补丁保留（`--keep` 另保留 worktree），末行写明验收是否仍有效及需在主干重跑的命令；清理失败只提示、退出码仍为 0。
- 任务结束时构建拉起的 sccache 服务端不被终止、不出现“终止了后台进程”的提示；其他被终止的进程提示里带可执行名。

## 5.24 源仓库构建目录提示

- Rust 源仓库 target 涨到 155 GiB，worktree 汇总覆盖不到：写入收尾后在 20 秒内量源根 target，status/wait 用最近缓存提示总量、各路径大小和先量再删的参考，绝不自动删除。Warn about source repository build disk usage using the latest cached measurement, without deleting build artifacts.
- 用户设 `sourceBuild.paths: ["build"]`，仓库设 `["target","cache"]` 时替换；`[]` 关闭，绝对/越界/外部符号链接报配置文件和字段、退出 2。阈值默认 60 GiB，`DELEGATE_SOURCE_BUILD_WARN_GIB=0` 关闭；两种磁盘 JSON 行可并存，du 超时或缓存失败不改验收结论。

## 5.23 清理容器与磁盘提示

- 十几个已结束的 Rust 写入任务手动改写后合入，worktree 的 target 共占 126 GiB：结束时只测一次并记录 `worktreeBytes`，status/wait 按本仓库汇总，达到 20 GiB 时提示总量、最大三项与 `clean <name> --force`；`DELEGATE_WORKTREE_WARN_GIB=0` 关闭。Warn about retained worktree disk usage without repeatedly running du.
- clean 删除 worktree 前回收 working_dir label 等于或位于该路径下的 compose 容器，前缀相同的兄弟目录不匹配；目录已删仍按记录路径回收，另扫 delegate worktree 根下的旧孤儿。Docker 不存在、无权限或超时只记一行提示；JSON 报告容器数量，匿名卷删除、具名卷保留。Reclaim compose containers and orphaned worktrees safely without requiring Docker.
- 改写后手动合入并提交：立即 `clean <run> --force`，检测只认完全包含；等门禁直接后台跑命令本身或 lane，不写会匹配自身 shell 的 `until ! pgrep -f` 轮询。

## 5.22 追加任务验收

- 仓库默认 accept 是 fmt 与 clippy，任务还需相关测试：用 `--accept-also 'cargo test -p api'` 保留默认门禁；可重复，按参数顺序以 `&&` 追加，没有基础命令时独立运行。Append task tests while preserving configured acceptance gates.
- 显式 `--accept` 替换非空且不同的默认命令：start 显示原默认与 `--accept-also` 提示；相同命令不提示，meta 与 summary 记录最终合成命令。
- `--no-accept` 与 `--accept-also` 同用是用法错误（2）；reply 的追加跟在上一轮最终命令后，显式 `--accept` 则先替换继承命令。

## 5.21.1 长时间自主推进与经验分层

- 主控持续推进一批已授权的任务：按需读 long-run.md，用忽略目录里的“进行中 / 待做 / 待确认”待办接续，每轮取最上面一项；写入一律 worktree，apply 后按任务提交。Use for an authorized long-running batch with a resumable backlog.
- 上下文压缩或 Monitor 到期：核对原等待者，保留有效后台 wait；退出 76 只表示已有等待者，不代表完成，Monitor 过期后重挂。
- 同事只跑定向验收，主控每批全量门禁；墙钟断言单独重跑，基准与门禁数据库隔离；只读审计留请求、id、代码行，优先修阻塞再复走。
- 连续两轮无有效进展、只剩待确认或门禁失败无法定位：更新待办并汇报验证与待确认；未授权不 push、不发布。
- 新经验先改脚本或默认值；入口只留 Agent 必须知道的规则，长流程进 references，已解决的实战条目压成版本表一行。入口 4000、参考 3000、实战 8000 token 超限时本仓库门禁失败。

## 5.18 证据、精确拦截与两层容量

- 修复已通过确定门禁，还想跑真实模型评测：`--accept` 决定交付，`--evidence` 收评测；默认 30m 且 lane 排队不计时，证据失败/超时只记 evidence，不改 state 或退出码。Evidence commands collect diagnostics without rejecting valid changes.
- 用户或仓库默认 evidence 自动用于写入，read-only 不继承默认；reply 沿用上一轮命令和超时，`--no-evidence` 关闭。验收失败或 protect 违规跳过证据，原 PATH 不受 agentDeny shim 影响。
- 只禁不带过滤参数的 `cargo xtask infra-test`：agentDeny 设 `exact: true`，额外参数放行；用户/仓库去重与 allow 撤销按 `(argv, exact)` 匹配。
- 主控已用 git 手动合入完整改动（尚未 commit）：status/wait/clean 比较源工作树最终内容、删除与执行位，全部包含显示“已合入（主干已含改动）”、appliedBy detected，clean --finished 可清；部分合入或读取失败仍建议 apply。Detect manual integration from working tree contents.
- 显式 `--model` 固定模型：短行显示模型名，JSON agentPinned true，不自动升档；`--agent` 已删除并退出 2；旧 Codex 记录 reply 时由 Pi 开新会话续做。
- 多项目同时委派：整机默认任务 12、重命令 2，每仓库默认 repoMaxActive 8；git worktree 与主仓库同计，非 git 按规范化路径；命中上限说明层级、计数并只列该层任务。
- Claude Code Monitor 能将后台逐行输出变事件：优先后台 wait --stream；只有结束通知则重复后台 wait --any。新字段可选，protocol 保持 1。

## 5.17 用户配置与仓库事实

- 多个仓库共用禁止同事全量检查的规则：把 agentDeny、通用 env 与 maxRework 写入用户级 delegate/config.json，仓库只补自己的字段。Shared user config applies across repositories.
- 某仓库需要同事运行用户级禁止的一条命令：仓库 agentDeny 用完全相同 argv 和 allow: true 撤销，其他用户规则仍生效。
- 用户 env 与仓库 env 同键：仓库覆盖，其他键合并；仓库未声明 accept 时用用户默认，显式 --accept / --no-accept 优先。
- 新机器首次派任务未配置三个用途档或 models 别名表：start 退出 2 并列出 Pi 可用模型；主控照用户指定或自行选好后运行 `delegate tiers --alias sol=provider/code --scout sol --review sol --write sol --thinking high`，并告诉用户选了什么，再重新派发。
- 换机器时设置 maxActive/maxHeavy 与 tiers 模型：用户配置提供容量，仓库可覆盖，DELEGATE_MAX_* 最高；同事不得自行放宽容量。
- 用户配置误写 worktree、generated 或 applyVerify：start 在 stderr 提示一次并忽略，仓库事实留在 .delegate.json；错误字段和损坏 JSON 必须指出文件并退出 2。
- 查看规则来自哪里：status --json 与 summary 的 configSources 区分 user/repo，自然语言结论保持简短。

## 5.16 相关检查与超时续做

- 三路同事都打算跑 `cargo xtask check`：配置 `agentDeny` 的 argv 前缀与相关 crate 检查提示，直接调用和 lane 调用均退出 77；主控合入一批后通过自己的 lane 统一跑全量。Agent full-suite checks are denied; the caller runs the batch check.
- 同事设 `DELEGATE_ALLOW_HEAVY=1` 再跑全量：仍拒绝，结论 denied 增加；脚本的 --accept、setup、apply 生成与 applyVerify 保持原 PATH，照常执行。
- `cargo test -p api` 没命中 deny：argv、退出码与 stdin/stdout 原样透传；`cargo --locked xtask check` 不匹配 `cargo xtask check`，配置按需要覆盖该精确前缀。
- 写入任务 timeout 但返工次数用完：复制 next 的 reply 续做，meta.continuation 为 timeout，不耗 maxRework；连续 timeout 连续续做，后续 rejected 再 reply 仍遵守预算。Timeout continuation is not rework.
- 没有 agentDeny 的仓库：沿用原行为，不生成 shim 或 denied 字段。

## 5.15 精简输出与边界

- 默认收结论只读短行与答复：名字、状态、档位、耗时、文件数与增删行、前三目录、验收、下一步；需要完整字段时显式 `status --json`/`wait --json`，不要展开全过程。
- `.delegate.json` 将只读 gitlink `third_party/pi` 放进 worktree.link：验收证据的 `excluded` 列出该路径，skip-worktree 不再导致 incomplete；手工给普通文件加标记仍不可复用。
- `third_party/pi-kit` 未初始化、目录空且指针未改：内容列为 `excluded`，不判 incomplete；指针继续参与树比较。源子模块随后初始化并改脏时，必须重新判不完整，不能沿用旧内容排除。
- DTO 改动需要重生成，但 `gen/` 受保护：按启动提示允许运行 `generated.command`，收尾复跑核对；产出漂移时看 `protectViolation` 的“生成物被手改”。
- 同一任务已有后台 wait：第二个 wait 立即退出 76，通知仍给原 pid；`--any`/`--stream`/`--machine` 跳过已覆盖任务，等待者死亡后可以重新接收。

## 5.12 收取与合并触发

- 后台 `wait --any` 通知到了，但可能是早先结束的审查：读 `completionTiming` 与 `finishedAt`，继续收尚未完成的任务。
- 合并看起来卡住，生成器排在全量检查后面：后台 `apply`，看 stderr 的排队人数、任务名、生成日志与超时；失败后再次 apply，即使普通文件已全部合入也要重试生成。
- 两路各新增同目录 `0032_*.sql`：串行 apply，读 `numberedPrefixConflicts`，人工审查编号，dry-run 也应预警。
- 恢复逻辑必须留在另一任务负责的 `model_loop.rs`：用 `--protect-reason model_loop.rs '另一任务负责；恢复逻辑必须留在这里'`，要求遇到必要修改停下报告。
- 首轮改动还没 apply，reply 只跑验收：用 `pendingChanges` 与 `next` 查看累计 `diff --total` 后合入。
- apply 后能否复用验收：只在 `acceptStillValid: true` 的 `repository-snapshot` 范围内判断；无字段看原因，主控改了无关源码、验收改树、生成结果不同均不当作有效，环境与忽略文件仍由主控把关。

## 应触发

- 同一个仓库里上次会话还有未收的任务；这次派出的两项完成后只用无参 `wait` 收本会话的结果，并提示旧任务。
- 让 Pi 并行审查一下 `src/api/` 和 `src/db/` 的错误处理，你继续做前端。
- 这个缓存方案帮我找个便宜模型问问第二意见。
- 这个动画页面我来做浏览器检查，你让 Pi 先审一下交互与小屏布局，给出最重要的三条意见。
- Delegate the migration of these three config files to a background agent and review the diff afterwards.
- 让 Pi 只读审查存储布局，控制台别刷读取步骤，只给我最终结论和错误。
- 把这个 bug 交给 Pi 修，跑 `npm test` 通过才算完成，我先去做别的。
- 让强档模型独立审一下这个迁移方案，和便宜档的意见对照看看。
- （主控自发）我继续改代码，同时让同事分运行时、API/调度、前端三路只读审查这批提交。
- 这十几篇文档让便宜的模型各自概括一下要点，我汇总。
- 这个修复交给强一点的模型做，测试过了再给我看 diff。
- 这是移动端设置页的截图，找个同事看看布局哪里别扭。
- 这个锁释放的并发 bug 交给强档同事修，`go test ./...` 通过为准。
- （主控自发）要审 api、worker、web 三块的错误处理，各自独立，分给同事并行做。
- 让同事在单独的 worktree 里改上传接口，别碰我现在的工作区，改完给我看改了哪些文件再合并。
- 刚才那个任务让 Pi 接着把测试补上，就在它原来的会话里继续。
- 把 `.local/scan` 里准备好的忽略材料复制进同事的 worktree，让它接着审查；我先去忙，回头再收 reply 结果。
- 继续上轮任务，并把我后来补的测试同步给同事；这次等它答完再返回。
- 让同事在 worktree 里完成接口改动，结论里要看目录分布、最大文件和配置文件变化，再决定是否合并。
- 让两个同事并行改同仓库不同目录，用 `--protect` 划分文件；按仓库 `.delegate.json` 的默认验收收结果。
- 给同事审查一个目录内最多五个高风险点，不要漫无边际扫描全仓库。

## 不应触发

- 把这个函数的变量名改成驼峰。（委派与验收的开销高于直接修改）
- 帮我配置 Pi 的默认模型。（是 Pi 配置问题，不是委派任务）

- 只读任务没写用途：推断 review、stderr 明确提醒且 status 标「推断」；显式 `--tier scout` 写入退出 2。Infer purpose with a visible notice, not silent routing.
- 用 `--model sol --thinking low` 或 reply 换模型：别名解析完整名，thinking 独立回显；模型后缀和 --tier/--model 同用报错。
- rpi 列出了没凭据的内置模型：不能因此选到它，只从 models 登记表匹配；未登记提示 --alias，歧义列已登记候选。
- 图片带给 images=no 的模型：不改选模型，退出 2，给出 models.json input 加 image 或 --model 换模型两条出路。

## 6.21.8 验收时限配置与源仓库越界归属

- 远程冷编译验收需要 40 分钟：`.delegate.json` 配 `"acceptTimeout": "40m"`，启动在「生效默认值及来源」回显来源 `repo`，`reply` 继承、`--accept-timeout` 覆盖。Repo-level acceptance timeout with CLI > repo > default precedence.
- 同事在 worktree 里只跑验收（工作树零改动），主控同时改源仓库文件：不判 `sourceWriteViolation`、正常验收交付，只报 `sourceWorkspaceChanges`；同事通过 `write`/`edit` 或 bash 绝对路径写了源文件时仍拒收，非 worktree 与 `node_modules` 软链检查不变。Worktree outside-write requires tool-call attribution to the colleague.

## 6.25 合入后待办、续做链折叠与跨仓库 Stop hook

- 合入前后要记上线替换/补数据/真机验证：`delegate note <run> "换线上服务"` 追加待办，`delegate note <run>` 列出，`delegate note <run> --done [<序号>|--all]` 标完成；挂在续做链任一环全链共享，`clean` 时保留在 `<runs>/notes.json` 直到 `--done`，`status`/`handoff` 持续展示。Post-merge todos attached to runs and reply chains, surviving `clean` until `--done`.
- 多轮 reply 续做链跑 `delegate status`：默认折叠成一行显示末环状态与轮次（如 `合入员（第 3 轮） · 运行中 …`），旧环提问/未通过不再刷屏；`--all` 与 `--json` 仍可看到每一环。Collapse reply chains in default human `status` while keeping every round in `--all` and `--json`.
- Claude Code 回合结束：`hooks/claude-code-stop.py` 跨本机所有有 delegate 记录的仓库检查提问、无 waiter 在跑、未合入（支持 `.local/delegate/deferred.tsv` 与 `DEFER_HOURS`）及未完成待办，写明仓库名并返回 `{"decision":"block","reason":...}`，`stop_hook_active` 为 true 时放行。Unified cross-repo Claude Code Stop hook covering questions, missing waiters, unmerged runs, and pending notes.
- 源仓库在 run 期间由主控提交改动（HEAD 前进，含手动脏改与部分暂存提交）：`source-watch` 排除已提交且工作区无新变的路径，`--read-only --shell` 同样按同事工具调用归属判定，不再误拒；systemd 254+/259 主机下 `systemd-run --scope` 自动附加 `--expand-environment=no`，`accept`/`setup`/`generate`/`measure` 中的 `$$` 与 `${VAR}` 不再被展开破坏。Exclude source HEAD commits from source-watch, attribute read-only shell source writes, and pass `--expand-environment=no` to `systemd-run --scope` when supported.

## 6.24 上下文瘦身、净基准与交接

- 多路未合入任务跑 `delegate status`：每 run 一行只留计数，`commitsDiffer`/保护提示/源改动路径清单移到 `status <run>` 详情，`--json` 默认不内联 `files`（加 `--files` 才带）。Compact one-line status and opt-in JSON files array.
- 同事在 worktree 里 rebase 或 merge 了改过保护路径的新主干：收尾不报 `protectViolation`，`changes`/`pendingChanges`/`changes.patch`/`diff`/`apply` 均按 merge-base（`meta.netBase`）只算同事自己的净改动。Net diff after rebase or merge avoids false protectViolation.
- 主控准备 `/clear`：`delegate handoff` 写出 `.local/run/delegate/HANDOFF.md` 并打印路径，列出未清理未合入任务的 `netBase`、净改动、`TODO(master)` 新增计数、`migrations/` 文件、下一步与重叠修改路径。One-page Markdown handoff for resuming after `/clear`.
- 主控前台执行 `until [ -f .local/run/delegate/.../exit_code ]; do sleep 5; done` 或 `while $D status …; do sleep …; done`：Claude Code hook 拒绝并提示改用后台 `delegate wait --any`，普通有限重试放行。Block foreground delegate polling loops while allowing bounded retries.

## 6.22 去串联与日常摩擦

- 去掉 `--after`：主控手动 `wait` 把关后再派下一步，`start --after` 退出 2，`--in RUN` 只读审已完成快照保留；6.21.x 遗留等待任务在 `status`/`wait` 标「需要重派」并给原样重派命令。Remove `--after` chaining and surface legacy waiting runs as needs-redispatch.
- 同事提问要不要放开受保护路径：`reply --allow PATH` 在原会话内放开并回显，后续续做继承，不丢上下文。Per-reply protection lift without losing session context.
- `start` 正文提到受保护前缀下的未放开路径：启动回显列路径和 `--allow` 参数，只提示不拦截，`status`/`wait` 带 `promptProtectedPaths` 标记。Advisory protected-path prompt check echoed at start, status and wait.
- 两次 `wait --any` 之间多个 run 进入终态或提问态：一次返回全部列出，首个给全文，其余一行摘要加 `result` 查看命令，不漏提问。Report all settled and questioning runs in one `wait --any`.
- 不带参 `status` 只列未结束、24 小时内结束未清理与本会话起的 run，更早未清理汇总一行（个数、空间、清理命令），`--all` 列全部。Default scoped status with older uncleaned summary and `--all`.
- `apply` 成功且归档写好后按 `worktree.cleanOnApply`（默认 true）自动删除 worktree 并保留归档，reply 续做链等整条链都合入后再清。Auto-clean worktree on apply controlled by `worktree.cleanOnApply`.
- 远程验收同步托管 `worktree.copy` 与 `worktree.link`（远端同路径直接链接，小目录同步），超限且远端缺失时验收自动回退本地并写明原因；SSH 断线仍失败关闭。Remote/local acceptance equivalence for managed copy and link paths.
- 源仓库执行 `git prune --expire=now` 后 `diff`/`apply` 仍可用：run 依赖对象钉在 `refs/delegate/<run-id>/*` 下，`clean` 时移除、`cleanOnApply` 仅清 worktree 时保留；旧 run 升级自动补写 ref，对象已丢时 `status`/`diff` 给 `git -C <worktree> diff HEAD`（未跟踪文件先 `add -N`）救回命令；hook 拦截危险 prune/gc/reflog/repack 并放行普通 `git gc`。Pin run Git objects under `refs/delegate/` and guard against immediate git prune.

## 6.20 使用摩擦回归 / Usage friction

- DELEGATE_REMOTE=0 启动后普通 reply、fresh 及续做保持本机，显式 1 恢复当前 remote 配置；explicit environment override.
- reply 正文含 `up -d --wait`、`delegate wait` 或 `--help` 不触发 hook；真正子命令 wait/run 或 reply --wait 仍拦前台，quoted message is data.
- apply 的远端槽锁排队不耗执行时限；传输未执行给重排/本地跑，程序自身 exit 75 仍判执行失败，queue versus execution.
- 提问回收未执行作业但保留队龄，回答后重跑同 label 优先于后来排队者、不能抢占已运行者；browser tests 仍共用 heavy lane，避免隐性超配。

## ETA 进度预估与停表归责

- 主控查看 `delegate status` / `status <run>`（含 `--json`）：活跃任务行尾追加单段交付预估（如 `预计 03:40 交付（p75 04:10）` 或 `等 lane(前2)·预计 …`），`status <run>` 展开一行阶段拆解与返工率；提问、待签字、预合并冲突、主干阻塞、疑似卡住或等内存时立即停表显示「卡在谁」，绝不因 ETA 超时停任务。
- 主控查看 `delegate work` / `work show <Wxx>` / `queue list` / `wait`：工作表 L0 首行追加关键路径研发墙钟与外部等待/所有者计数，L1 仍守 6 列与 100 字上限并在 `focus` 列内嵌 `~HH:MM` / `等Wxx(~HH:MM)` / `卡所有者Qxx`，`work show` 列出上游关键路径拆解，`queue list` 顶部显示 `DAG 关键路径`，`wait`/`wait --since` 尾部追加其余在跑波次与剩余链预估；`delegate speed --backtest` 输出 14 天严格因果滚动回测的 p75 覆盖率与 p50 偏差。
