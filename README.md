# AIA-skills

> **Atomic Agent Skills**  
> 面向高推理能力 AI Coding Agent 的原子化技能库：每个技能提供一项可执行能力和清晰的触发条件，而不是一套约束规则。

---

## 🌟 设计哲学：给能力，而不是给镣铐

1. **能力优先**：确定性的事（采集现场、调度进程、过滤事件流、审计文档）交给脚本，模型把精力留给判断。
2. **拒绝常识说教**：强模型不需要被告知怎么组织目录或为什么要跑测试；技能里只保留脚本无法保证、需要判断的点，并说明原因。
3. **可路由**：`description` 写清"做什么、什么时候用"，让 Agent 在需要时加载，其余时间不占上下文。
4. **高内聚、原子化**：每个技能解决一个独立问题，可自由组合。

---

## 📦 技能一览

| 技能名称 | 目录 | 能力与适用场景 |
| --- | --- | --- |
| **`repo-governance`** | `skills/repo-governance/` | **上下文治理与审计**：定义 `AGENTS.md`、`docs/current.md`、决策记录的信息分层；`audit-context.py` 按仓库配置估算 token 预算与上下文健康度，支持 `--report`、`--only`，并检查下一步堆积、`.local/` 未忽略、文档断链、`PROVISIONAL(Cnn)` 标记缺少待收敛清单条目等漂移问题；约定经验先改脚本、手册设预算并以替换代替追加；`adr-archive.py` 把并入主 ADR 的修订移进 `history/`、改状态并重算全仓相对链接。 |
| **`agent-handoff`** | `skills/agent-handoff/` | **会话交接**：`handoff-snapshot.sh` 自动采集分支、HEAD、未提交文件、最近提交、`.local/run/delegate/` 与旧 `.local/run/pi/` 的未读取委派任务和未合并 worktree，生成交接账本草稿。 |
| **`delegate`** | `skills/delegate/` | **同事 Agent 委派**：拆分任务、后台运行 Pi 或 agy 同事（scout/review/write 按用途，models 别名选模型并可带默认 thinking 与可选相对价位档 `cost`（`low|main|high`），按任务性质选思考档；用户级三类 `fit` 适配卡（build/hard/review）在表格、`fit --json` 与 `tiers` 展示别名价位（未设显示 `—`），`fit bench review <别名> [--since 30d] [-n 10] [--estimate] [--yes] [--json]` 用历史已知结论（漏检/检出）回放归档快照评估候选审查模型并落盘适配卡，`start`/`reply` 启动回显附 `〔cost〕`（仅显示不自动换模型不拦截），`start --class` 未传 `--thinking` 时按类别定默认档（build/review→medium、hard→high）并提示首选与未评；审查模型支持 `review.model`/`thinking` 与 `start --review-model` 四层优先级、不可用回退 review 档并在审查卡明示、自然开头宽松读结论（不补问，读不出时原样保留 `review.md` 落 `NEEDS_CONTROLLER` 并附事实提示），以及按触发与 Claude 池余量并行跑第二意见的 `review.extra`；仅调用已登记模型），按会话收取短结论与答复，完整字段按需 `--json`；全机只追加 `events.jsonl` 事件流（`events [--since|--follow|--rebuild|--json]`、`wait --since SEQ` 游标等待），Claude Code Stop hook 竞态锁在存在游标后未读交付/提问/卡住提醒/续派暂停事件时拦截；`status` 与 TUI 读 transcript/日志尾部判活（「等待中 / 在思考 / 在跑命令 / 配额或报错」）；idle/command 卡住与机械重复（ADR 0120）只写同事不可见的 `stall_warning` 不强杀，仅显式 `stop` 终止；`queue add/list/remove/cancel` 支持确定性 DAG 续派（上游交付+验收+可选 6.29 审查 PASS 时零模型调用自动起下一步，异常暂停并写事件）；每任务一个等待者，重复等待退出 76。`--accept-also` 追加任务测试并保留默认验收，覆盖默认时提示，`.delegate.json` 支持 `acceptTimeout` 并由 `reply` 继承。`agentDeny` 用前缀或精确 argv 拦截同事全量检查（含 lane），验收冲突只警告并交主控负责；`measure`/`measureCpus` 与 `lane --measure` 为压测、计时和编译耗时预约核，命令自动匹配且排队不计 stall/timeout。`evidence` 收集评测且不改交付状态；显式 timeout 或 stall 后 reply 续做不耗返工次数；Claude Code wait 每 50 分钟给进度 glance，控制器重跑打印命令。写入用 `--worktree` 隔离、`--protect-reason` 划分所有权，`start`/`reply` 均支持 `--allow` 按次放开保护并由续做继承，正文提及未放开的受保护路径时启动回显与 `status`/`wait` 均提示不拦截；运行中 `reply` 默认直接追加到只追加信箱（支持多条按序编号合并，支持 RPC `steer` 的 Pi 轮中即注入，`agy` 管道 stdin 与无 `steer` 的 Pi 在轮末同会话合并接着答，收尾后自动退回新 round，`status`/`card`/TUI 显「已追加 N 条，待投递 M 条」，`stop` 取消待投递，`--when-done` 为兼容别名），`wait --any` 忽略停止、每次报齐上次 wait 之后所有终态与提问（首个给全文，其余一行摘要加查看命令），生成路径收尾核对；托管 copy/link 与未初始化参考 gitlink 的内容列为 `excluded`。`status` 默认按本机登记仓库（`repos.json` 与活跃 `*.slot`）跨仓汇总首行计数与待处理任务（非当前仓标 `[<repo>]`，远端执行标 `远端:@<host>`、命令与已跑时长；续做链折叠成末环一行「`<名>（第 N 轮）`」，人读 `--all` 同样折叠为一行、`--json` 保留每轮，`status <run>` 展开各轮原文、路径与远端输出尾部，`--json` 加 `--files` 才带文件数组），末行指向 `history` 与归档占用（直接登录远端机器独立派发的 run 暂不跨机聚合）；`note` 在 run/续做链上挂合入后待办并在 `clean` 后保留直到 `--done`；worktree 里 rebase/merge 新主干后按 merge-base（`netBase`）算净改动与保护检查；`handoff` 生成 `.local/run/delegate/HANDOFF.md` 一页交接索引（含「合入后待办」）；`delegate hook check --format claude|agy|json`（`hooks/claude-code-stop.py` 薄壳）与 `hook pending` 提供零锁零 git 单件渐进 Stop 钩子、卡链 30m 跨仓穿透与全量待办视图，与 `delegate-tui` 共用「待处理」口径（已读只读与 `cleaned` 归默认折叠的「已收取」）；每仓库独立永久追加 `history.jsonl`（单条 <2 KB），`history` 支持中文单字+二元组检索，`show` 给单次摘要、合入后同文件提交与归档入口，`board` 及自动原子重写的看板文件（`.delegate.json` `board`，默认 `BOARD.md`）实时投影任务状态，`start`/`run` 派发前比对在跑与近 14 天历史在 stderr 提示相似任务（最多 3 条，不拦截）。`clean`/apply 释放前归档任务记录（提示词、答复、meta、patch、`events.jsonl.gz`）与证据，归档不设总量硬上限（仅文件系统临满时按最久未读淘汰事件日志，核心文件与 history 永不删），`apply` 成功后按 `worktree.cleanOnApply`（默认 true）自动清 worktree（续做链等整条链合入后再清）；合并前检查目标文件是否已有源工作区未提交改动（忽略本 run 自写相同内容），有重叠默认拒绝并给先提交或 `--allow-dirty-overlap` 两选项，新增 `files <run>`（归档后可用）并于 `apply` 成功打印；新增一站式 `land <run> [-m "…" | -F <文件>]`（合入→源工作区跑 `mergeAccept`/run `accept`，忽略 `applyVerify: false`，`acceptStillValid` 沿用交付验收，`--no-verify` 显式跳过→按 `files` 全链文件集含删除暂存→带 `Delegate-Run:` trailer 提交→自动清 worktree，失败保留已写入与暂存并给续做命令）；`apply` 默认落为未提交改动并提示「改动未提交：N 个文件」与同事原提交标题（`--commit` 同样按全链文件集暂存且支持 `-m`/`--message`/`-F`），`apply`/`land` 人读 stdout 首/末行固定 `APPLY|LAND OK|FAILED`，`ok: false` 明报 `failedStep` 与已落盘 `writtenFiles`，`applyVerify: false` 用事实措辞提示批量门禁命令，`apply` 仅在写盘短窗口持 `source-watch` 锁不阻塞 `start`；收尾与 apply 提醒提交外未提交文件，`reply --sync=agent` 把文本冲突交同事解、未解拒收。`apply` 拒绝叠冲突标记、保留可按路径取出的 `patch`，检测写出工作树；`wait --all` 等齐，`--shell` 支持快照内只读调查，`--in` 只读审查已完成工作树，tier/model 可同用；另支持生成重试、编号预警、`acceptStillValid` 与合并后复验；手动合入自动识别，`pendingChanges` 保留累计待合入量。写入收尾缓存 worktree 与源构建目录占用，status/wait 达阈值独立提示。容量满自动排队（reply 优先、同级 FIFO），模型满默认不换，`--overflow next` 才允许溢出下一候选；同事 `pkill`/`killall`/`pgrep` 限本次 run 全部 scope，无 cgroup 失败关闭（只读 shell 同样）；用户级 `remote` 将 cargo 等重命令 SSH/rsync 转到构建机，远端 lane 独立限流、同步托管 copy/link（超限缺失时验收回退本地并写明原因）、回传改动、取消回收、断线不回退；remote=off 续做继承，未执行验收可重排/本地跑；提问的 lane 作业重跑沿用原排队时间；`remote check` 测连通。Rust 管理整机与仓库防失控上限、模型并发、lane、后台进程回收，固定同事显示默认档位；`probe` 派发前用同一 Pi 可执行文件、环境与 provider 配置并行探测各档模型连通（不建任务、不占名额，默认档会选的模型不通退出 1），长答复全文落文件；工作项支持 `--class feat|unblock|infra|docs`（缺类别标「缺类别」）、`work import-dag` 批量建依赖图、`.delegate.json` `fill`/`hotspots` 无模型按价值配额补位（只数写链、避开热点冲突）及 `fill stats` 停止线统计（越线仅提醒不自动降档）；自动补位与 DAG 续派支持额度预报软门控影子模式（只读缓存且 >20m 视为未知，装不下时仅记 `quota_would_hold` 不拦截，手动 `start` 不记不拦）及 `quota-shadow [--since 72h] [--json]` 对账召回率与误报率；`speed [--backtest]` 与 `eta` 提供零 token、无常驻进程、零锁只读工期校准与实时进度预估（含停表归责「卡在谁」、lane/容量排队折算与工作表/续派 DAG 关键路径，直显于 `status`/`work`/`queue list`/`wait`）；[长时间自主推进](skills/delegate/references/long-run.md) 按需读取，实战经验按版本压缩沉淀。 |
| **`openai-image-gen`** | `skills/openai-image-gen/` | **图像生成落盘**：`image` 一次调用直接出成图（默认 GPT Image 2.6 sunburst · medium，flare 更快同价），网关回的格式不符时用 ffmpeg 转换；明显不对时 `edit` 改写明的细节或改提示词重出，上一版留在 `previous`；相同请求与没写改动的编辑不发请求；可用 `-r` 传角色设定图等参考图（最多 8 张），同一路径的重出与编辑自动沿用，文件变动会警告；每个输出文件限定出图 3、编辑 1 次，超出需用户同意；只返回一行 JSON；报错带错误码，内容规则拒绝要如实告知；附示意图提示词结构与审查要点。 |
| **`scroll-gesture`** | `skills/scroll-gesture/` | **滚动手势**：`input-probe.html` 在目标环境（Windows 鼠标、远程桌面、触屏）实测滚轮与触摸事件的频率、间隔、增量与鼠标格占比，一键复制 JSON；附过界翻页等自定义手势的判断点：只停原位/接缝/邻页、两段确认防误触、按力度（带衰减的阈值）而非距离确认、弹回等待长于远控间隔、动画可打断、鼠标格缓动。 |
| **`agent-interface-design`** | `skills/agent-interface-design/` | **模型界面设计原则**：设计或评审工具说明、工具结果与错误文字、Skill、系统段、读取方式时用：渐进式披露（分层且末行给下一层）、结果带下一步、给事实不规定顺序、前缀冻结只在尾部追加、失败分类与错误文字、顺着模型习惯、改文字先测（基线→复盘→同 seed 对照，基线已过不加规则）、压缩即索引（摘要旁由宿主写原文位置），附评审清单。 |
| **`iteration-speed`** | `skills/iteration-speed/` | **仓库迭代速度**：新机器/仓库初始化、构建测试门禁变慢、多 worktree/Agent 并行、验收分层与模块拆分前、估算规模与压力（小样本量单位成本再外推）、设代码量预算（清理后基线加增量）、对照性能（生产构建交替 A/B、比主线程 CPU 时间）或按工作量（数据库读行/页访问、指令数）结算门禁回归时使用；`iteration-speed detect` 只读探测生态与缓存配置，`time`/`history` 记录反馈时间，`gate` 监测门禁预算与耗时回退、汇总最慢 Cargo 测试，`step`/`steps` 逐步记录门禁耗时并在输入未变时复用通过结果，`rust-setup` 预览或备份后写入用户级 sccache/mold 配置；附 Rust、Node、Python、Go、Docker 的缓存边界与实践。 |

`delegate` 架构总览：[docs/assets/delegate-architecture.webp](docs/assets/delegate-architecture.webp)（提示词同目录）。

`delegate` 6.39.0：修复 8 项日常审计缺陷——`controllerReviewPaths` 不再劫持 `REWORK`（卡片显示 `originalVerdict` 与 `round-N-verdict.md` 指针）；Stop 钩子按 `waiter.lock` 判定并仅拦本会话 `caller` 或当前仓库的 run，识别无参 `delegate wait` / `wait --any`；`apply` 避让/取消同 run `premerge`、`regenerate` 遇主仓 `target/.cargo-lock` 占用自动切独立 `CARGO_TARGET_DIR` 并记录起止与 `elapsedMs`；验收失败比对 `accept.log` 报错路径与改动文件并在卡片标注疑似主干已有失败与日志行指针；`wait --since` 对不存在 run 立即报错并附相近建议、过滤模式仅查目标 run 活跃度；`apply` 冲突带 `reply --sync=agent` rebase 命令；卡片首行显示基线短 SHA 与 `已 rebase：<old>→<new>`；`source_guard` 放行 `2>&1`。

`delegate` 6.38.0：内置本地 `agy` 多账号管理与 TUI 账号页，替换第三方 `agy-swap`——新增 `delegate agy account list|status|add|use|remove|import-agy-swap|restore-entry`（凭据隔离存于 `~/.config/delegate/agy-accounts/<id>/`，目录 `0700`/文件 `0600`，永不打印令牌、不做自动轮换、不调外部额度接口）；`add` 在独立临时 HOME 跑交互登录不干扰活跃账号，`use` 全程排他锁并先回存刷新过的令牌再原子切换、有同 HOME 在跑 `agy` 进程时拒绝切换并列出 PID 与 run；`import-agy-swap [--dry-run]` 支持解码 `agy-swap` 的 `go-keyring-base64:` vault 导入槽位，`restore-entry [--dry-run]` 备份并恢复 `~/.local/bin/agy` 为直连 `~/.local/libexec/agy` 的最小包装或符号链接；`delegate-tui` 新增「6账号」标签页（`6`/`A` 切页、活跃标记、就地确认切换），详见 [agy 账号与迁移](skills/delegate/references/agy-accounts.md)。

`delegate` 6.37.0：按「压缩即索引」保留各轮答复（W051）——审-改自动返工前将上一轮答复持久化为 `round-N-result.md`（不覆盖旧结论与实测数字，`result.md` 仍镜像最新一轮；归档与轻量保留均完整保存）；审查卡第 1 段在存在多轮（审-改返工或 `reply` 续做链，含已归档父 run）时确定性列出每轮原文路径与行/字数；`delegate result <run>` 支持 `--round N`、`--all`、`--rounds`、`--path` 与 `--json` 查看任意轮或全部轮次。

`delegate` 6.36.1：修复 Stop 钩子事件游标无法消除及过期告警误拦——非 `--follow`、非 `--json` 的 `delegate events` 打印完后经短锁原子推进调用方（`DELEGATE_CALLER`/`CLAUDE_CODE_SESSION_ID` 等，未设时走 `default`）在 `cursors.json` 中的游标到最后打印的 `seq`（`--json`/`--follow` 不推进），照钩子提示执行 `delegate events --since N` 即可放行；Stop 钩子不再将已关闭/放弃/脱离 `ready` 的 `work_ready`、已闭环或重暂缓的 `work_defer_expired`/`work_stale`、已恢复的 `queue_paused` 与已 `apply`/`clean`/`reply`/结束/签字的 run 告警计作未读。

`delegate` 6.36.0：Stop 钩子合一（W041）+ 工作表（W044）与审查卡误报修复——`hooks/claude-code-stop.py` 合并原 waiter/unmerged/游标三处拦截，严格零锁、零 git 只读 `state/index.json` 与快照，认裸与带路径 `delegate wait`，跳过进行中的 `apply`/合入员/门禁与零改动交付；移除 `deferred.tsv` 改由 `delegate work defer --reason … --until …` 统一暂缓（升级迁移自动导入旧内容），每次拦截记 `<state>/stop-hook.jsonl` 并提供 `delegate hook stats` 按周统计触发与处理去向（挂了 wait / apply 了 / defer 了 / 被忽略了）；`scripts/install-claude-hooks.sh` 仅安装单一 Stop 钩子并移除旧脚本配置。工作表忽略已 clean/归档的 run，零改动交付显示「已收取」。审查卡风险规则排除 lockfile 仅本包版本号变化、按语言与语法识别跳过标记（不再将 `sys.exit` 误命 `xit`），并收紧配置/常量规则排除 `[&str; N]` 数组长度、`7.min(len)` 切片截取与普通字段 `unwrap_or` 回退。`delegate` 6.35.0：后台预合并（`_merge-worker`）+ 快进 `apply`（仍由主控审查合入、不做 `autoApply`）——每仓库单实例短命 `_merge-worker`（`LOCK_NB` + `.wake` 无丢唤醒）在 `delivered`（审查 `PASS`/未开审查且无未签字受控路径）、`delegate sign` 成功或主干前进后由零锁 `status`/`card` 读路径发现过期时拉起，在 `<cache>/premerge/<repo-key>/worktree` 持久化集成树（复用 `target/` 与 `node_modules/`，永不碰源工作区）做三方合并并仅跑命中的 `acceptRules` 层（未配跑 `accept`，同树直接复用），成功后钉 `refs/delegate/premerge/<run>` 并写 `premerge.json`，冲突/失败发 `premerge_conflict`/`premerge_failed` 事件带日志指针；`apply` 在 `apply.lock` 内校验 `HEAD == baseHead` 且 `changedPaths` 无脏改时直接快进写入未提交改动（<2s）并跳过复验，否则明报原因回退三方合并；L0/L1、`events`、Stop hook 及 `delegate-tui` run 行与工作表「此刻在干什么」实显 `快进 ✓（基于 <short>）` / `快进 ✗ 主干已前进` / `预合并中` / `预合并冲突：<文件>`。`delegate` 6.34.0：合入审查卡（L0–L3 渐进式披露）+ 分层验收（`acceptRules`）+ 慢层验收与只读审查并行——L0 在 `status`/`wait`/交付通知一行含验收分层、审查结论、改动规模、风险计数、`快进 —` 与 `delegate card <run>`；L1 `delegate card <run>` 一屏索引作者三行摘要+原文指针、审查员结论、内置 9 类规则+`riskRules`+审查员 `## 主控重点看` 去重风险项、测试增删改计数与验收分层、目录折叠其余改动与下一步；L2 `delegate diff <run> --risky` 仅输出风险 hunk 与上下文，L3 `diff` 保持现状。`acceptRules` 按改动路径分快/慢层，未命中或改构建配置 fail-closed 跑全量；快层通过后慢层与隔离只读审查员并行，任一失败互停（审查员被取消不记 `PASS`），返工仅重跑受影响层。`delegate` 6.33.0：W024 审计低风险快修——修订 `agy` 消息末尾为事实陈述（长命令 10 秒后转后台并由系统唤醒、无需 `sleep` 轮询）并修复 `agy` 多回合 token 双计、为同会话 `reply` 记录 `tokensDelta`；`review.json` / `summary.json` / `index.json` 压缩即索引（违规清单落独立文件、`controllerReview` 去内联 diff 留 `diffFile`/`diffBytes`、兼容旧记录）；非 `--all` `status` 走索引、`archive::usage()` 读 `.usage.json` 缓存、读路径 `sign_status` 零 git 子进程、`human_line` 免重读 `meta.json`；`agy` 只读审查工作区移至 `<cache>/delegate/review-ro/<run>` 修复 `rg`/`fd` 被父仓 `.gitignore` 屏蔽；`apply` 清理改走异步 trash 并按失败关闭条件自动归档清理已合入链（`diff`/`show` 回退读归档）；`_gc-worker` 补索引冷启动/版本不符重建与 `.usage.json` 预热，`install.sh` 升级后后台拉起一次 `_gc-worker`。`delegate` 6.32.0：根治死锁与查询阻塞，交互命令全异步（评审 V1）——全量状态写路径改为 PID+nonce 临时文件 `write_all`→`sync_data`→`rename` 原子写；收拢控制面写锁为单把短锁 `state/.state.lock`（<5ms，锁内禁止 git/子进程/sleep，过渡兼容旧锁文件）；所有只读路径（`status`/`work`/`events`/`wait`/`snapshot`/TUI/Stop hook）零锁、零写盘、零阻塞，新增物化索引 `state/index.json` 与 `delegate snapshot --json`，TUI 改读一次 snapshot 并以 Linux `inotify` 监听 `events.seq`（`wait --since` 与 `events --follow` 同用 `inotify`）；`start`、`reply`（默认 Auto 同步）与 `clean`（rename 至 trash 后由 `_gc-worker` 异步回收，支持 `--sync`）前台 50ms 内返回，慢活下沉到 `_supervise` 且 prepare 失败立写 `failed` 事件，按二进制 mtime 缓存 `pi_models` 与 `agent_identity`；修复手动合入 HEAD 后 `clean --force` 导致 `run_applied` 前置条件永久阻塞的问题；新增 V1 并发压测套件并预留 V2 premerge 快进校验接口。`delegate` 6.31.1：统一全局锁序（`locks.rs`）与只读路径非阻塞/原子读快照消除 `start --work` 与 `status`/TUI 死锁，等锁超 10s 报持锁 PID 与命令行（仅报告不强杀）；`start` 锁内仅登记 run 与槽位、慢活移出锁外；`status`「待合入」正确计入续做链末环；机械重复检测排除 `sleep`/`wait`/`tail` 纯等待轮询。`delegate` 6.31.0：全机统一工作表（`<state>/work.jsonl`，一行一件事，7 态状态机 `attention`/`ready`/`verify`/`active`/`blocked`/`deferred`/`done`，预留 `artifacts[]`/`followups[]`）与零前缀适时提示——`delegate work` 展示第 0 层计数与 6 列紧凑表并自动合入未绑定 run（`R:<id>`），`work show` 展卡片与宿主原文指针；`work add`/`wait` 支持 `run_applied`（`--after-applied`）、`run_delivered`、`work_closed`、只读观察 `docs/owner/README.md` 的 `owner:Qxx`、`after_time` 与 `--auto` 自动派发，`queue add` 同步支持 `--after-applied`；`start --work <ID>` 关联工作项并由 `reply` 自动继承，`apply --closes` 支持合入即闭环，`work close --evidence`/`drop --reason`/`defer --reason` 显式管理闭环与暂缓，`work import-md --dry-run` 解析 Markdown 待办并比对 `history.jsonl` 标出疑似已合入条目；`wait`/`apply`/`merge`/`clean` 尾部按需附带原文指针的 1–4 行提示（绝不进系统前缀、零 token）；Stop hook 分级只拦当下阻断项（未读告警含 `work_ready`/`work_defer_expired`/`work_stale`、提问、无 waiter 在跑、未暂缓未合入），不再因 `blocked`/`deferred`/未超期 `verify` 反复拦；首次访问自动迁入 `notes.json` 与 `deferred.tsv`，`delegate note` 保留为薄壳。`delegate` 6.30.0：合入员（`delegate merge`）、发散侦察（`delegate radar`）与按线程自动推导编译并发——`delegate merge <run>...` / `--ready` 在独立集成 worktree 基于源 HEAD 串行 3-way 合入、命中 `generated.inputs` 重生、逐个经 lane 跑 `accept`、末尾跑 `mergeAccept`（默认等于 `accept`），原样保留同事提交标题并加 `Delegate-Run: <run>` trailer，遇冲突/红测/受保护路径/未签字或失效 `controllerReviewPaths` 原子回退标 `SKIPPED` 后继续；`.delegate.json` `autoMerge`（支持 `!` 排除与 `add:` 纯新增）标「可自主合入 / 待主控点头」并产出 `merge-report.md`，主控用 `delegate apply --keep-commits <merge-run>` 拿回提交链并自行 push。`delegate radar` 按 `radar.collect` 顺序采集算指纹，未变零 token 退出，变化时派只读 scout 侦察，Host 校验锚点（`文件:行`/commit/输出行号，无锚丢弃）去重写 `<state>/radar/<repo>/board.md`，`delegate radar pick <id>` 生成任务说明草稿不自动派活，`schedule` 输出 00:30–04:30 每 2 小时定时说明；编译并发按 `min(available_parallelism, cgroup v2 cpu.max, nproc) - 2` 算预算，`maxHeavy` 默认 `max(2, budget / 12)` 作内存/IO 保护，CPU 由 `<state>/jobserver.fifo` 共享令牌池与 `lane` 进程内代理 FIFO 动态按资历权重（`1, 1/2, 1/4…`，保底 2）公平分配（覆盖旧 `env.CARGO_BUILD_JOBS`，显式覆盖用 `heavyJobs` 并在 `lane`/`status` 注明「显式」，远端不做代理照旧按远端线程与 `remote.maxHeavy` 相除注入 `CARGO_BUILD_JOBS`），`merge_*`/`radar_*` 接入事件流，TUI 新增「3合入 / 4候选」标签页并在底栏展示令牌池与各命令份额。`delegate` 6.29.0：审-改闭环与 `controllerReviewPaths` 签字——`.delegate.json` 开启 `review`（默认关）后，写入验收通过自动派新上下文只读审查员（禁写文件、禁派活），输入任务说明、`diff --total` 与默认/自定义审查要点（`references/review-rubric.md`）；首行受控判定 `PASS`/`REWORK`/`NEEDS_CONTROLLER`（格式不对失败关闭），结论写入 `review.json` 与 `summary.review`；`REWORK` 原样把结论文件退回作者且续审只看本轮增量 diff，不设轮数上限、第 3 轮起发 `needs_attention` 提醒不强杀；改到 `controllerReviewPaths` 一律升 `NEEDS_CONTROLLER` 并附 diff 全文与 SHA-256 哈希，主控用 `delegate sign <run>` 签字（改一字即失效，供 6.30 合入员用，不改 `apply` 语义）；`status`/TUI 与 Stop hook 完整展示与拦截。`delegate` 6.28.0：确定性事件流、transcript 判活、Stop Hook 竞态锁、卡住只提醒与确定性 DAG 续派——全机 state 目录写入只追加 `events.jsonl`（单调 `seq`，写入失败仅记诊断，支持 `events --rebuild` 从 meta 重建），新增 `delegate events [--since|--follow|--rebuild|--json]` 与游标等待 `delegate wait --since SEQ`；`claude-code-stop.py` 依据 `wait`/`result` 记录的游标拦截未读交付/提问/卡住提醒/续派暂停事件；`status` 与 TUI 读取同事 transcript/日志尾部展示判活状态（「等待中 / 在思考 / 在跑命令 / 配额或报错」）；`agents.rs` 的 idle/command stall 与机械重复自检（ADR 0120）以及验收无输出均改为只发同事不可见的 `stall_warning`（验收默认不设超时，`stallKind: "accept"` 带已跑时长与最后一行输出，仅显式 `--accept-timeout`/`acceptTimeout` 或 `stop` 才终止）；新增 `delegate queue add/list/remove/cancel` 有序 DAG 续派（上游交付+验收+可选 6.29 审查 PASS 时零模型调用自动起下一步，异常暂停并写事件）；`delegate tui` 新增「事件」标签页并展示判活状态与 DAG 续派队列。`delegate` 6.27.0：修复 pool slot 续做链在主干前进后把主干改动混入本链改动与保护检查的问题，统一按本链净基线统计并单列一行主干漂移（仅重叠非零时醒目）；`reply` 默认自动三方同步基线、无损重放同事提交并保留 slot `target/`（遇冲突回退旧基线并提示 `--no-sync` / `--sync=agent`）。`delegate` 6.26.0：修复 `apply` 失败诊断与 `start` 阻塞——`apply` 返回 `ok: false` 时报告 `failedStep`/`failureDetail` 与已落盘 `writtenFiles`，Markdown 围栏代码示例与 Setext 标题不再误报冲突标记；`applyVerify: false` 且主干前进时改用事实措辞提示批量门禁命令；默认 `apply` 提示「改动未提交：N 个文件」并列同事原提交标题，新增失败关闭的 `--commit` 压成一条提交；`apply` 用独立 `apply.lock` 串行且仅在写盘短窗口持 `source-watch` 锁，不再阻塞 `start`（确需等写盘时 stderr 打一行进度）。`delegate` 6.24.0：委派留痕、自动看板与渐进式披露——`status` 默认首行计数且只列待处理任务、末行指向 `delegate history` 与归档占用（`result`/`apply` 记入 `meta`，`--all` 保持全量）；每源仓库永久保留只追加 `history.jsonl`（单条 <2 KB），首次运行自动补写已有 run 与归档（不迁移目录）；新增 `history`（老的在上新的在下，中文单字+二元组匹配，`--since`/`--state`）、`show`（提示词、确定性结论段、验收行、现查 apply 后同文件 `git log`、续做链与归档原文）与 `board`（每次状态变化自动原子重写看板 Markdown，路径由仓库 `.delegate.json` `board` 指定、默认 `history` 旁 `BOARD.md`，顶行含整机在途，写失败仅记诊断）；`start`/`run` 比对在跑与近 14 天历史在 stderr 提示最多 3 条相似任务（不拦截）；归档保留提示词、答复、meta、patch、`events.jsonl.gz` 且不设总量硬上限，仅文件系统临满时按最久未读淘汰事件日志（核心文件与 `history.jsonl` 永不删）。`delegate` 6.22.1：将每个 run 依赖的快照对象钉成源仓库 `refs/delegate/<run-id>/*`（`clean` 时移除，`cleanOnApply` 仅清 worktree 时保留），防止 `git prune`/`git gc` 删掉不可达对象；升级后首次命令自动为旧 run 补写 ref，对象已丢时 `status`/`diff`/`apply` 给出 `git -C <worktree> diff HEAD`（未跟踪文件先 `add -N`）救回命令；Claude Code 钩子在含 `.local/run/delegate` 或 `refs/delegate/` 的仓库拦截立即删不可达对象的 `git prune`/`git gc --prune=now|all`/`git reflog expire`/`git repack` 组合，放行普通 `git gc`。`delegate` 6.22.0：移除 `--after` 自动串联及跳过状态（保留 `--in`，6.21.x 遗留等待标「需要重派」并给原样重派命令）；`reply` 支持 `--allow` 放开保护、`start` 提示正文提及的受保护路径并在 `status`/`wait` 标记；`wait --any` 报齐间隔内全部终态与提问（首个给全文、其余一行摘要加查看命令）；`status` 默认只列眼前任务并一行汇总更早未清理、`--all` 列全部；`worktree.cleanOnApply`（默认 true）合入归档后自动清 worktree（续做链整链合入后再清）；远程验收同步托管 copy/link，超限缺失时验收回退本地并写明原因。6.20.0：remote=off 续做继承、hook 按子命令位置判断；验收未执行可重排或本地跑、远端槽排队不耗执行超时；提问保留 lane 原排队时间且不抢占。6.19.0：用户级 remote 把 cargo 等重命令经 SSH/rsync 转到构建机，独立 lane、回传源改动与退出码、取消清理，断线失败关闭不回退本机。6.18.0：清理前归档证据、源 Git 不拿可选索引锁、未提交文件提醒、`reply --sync=agent` 冲突交同事解决。6.17.0：按次开放保护、后台待续答、wait 忽略停止/跳过；apply 正确登记零改动续轮并安静归还池位；status 用缓存与非阻塞锁，Bash 防误写源树、复制/链接按目标判断且允许读出源树（非沙箱）。6.15.0：并行合入拒绝叠冲突、释放工作树后保留补丁，`patch` 可按路径恢复；检测误写源仓库，`wait --all` 等全部结束，生成器按 inputs 触发；只读 `--shell` 查日志/查询并核对写入，tier/model 可同用。6.14.0：到上限从拒绝改为排队；模型满了从换模型改为排队，需旧行为加 `overflow: next`，单次可用 `--overflow next|queue`。防失控默认整机 32、仓库 0（不限），实际限流靠模型并发、maxHeavy 和内存水位。6.13.0：仓库配置 `appendOnly` 让追加型文件冲突时两段都保留；派出写入任务时告知未提交的已占用编号。6.12.4：旧版本记下的旁文件会话 id 也能续做。6.12.3：续做时忽略 rpi 的会话进度旁文件。6.12.2：rpi 回答中途被截断的 SSE 响应体（`error reading sse body` / `unexpected EOF`）按流中断原地续做。6.12.1：池槽位构建缓存先清被取代的增量会话、再按最近使用淘汰，可用 `DELEGATE_POOL_TARGET_ROOT` 放到专用盘。6.11.0：用途、模型、思维长度三个独立维度；未指定用途时明确推断并标记，模型只来自 models 别名表，Pi 只校验与提供能力，图片仅守卫不换模型。本版不兼容旧档名与模型 thinking 后缀，迁移为三用途档、models 别名和 thinking 字段。6.10.0：worktree 池（`worktree.pool`），写入任务复用固定路径的 worktree，构建缓存保留、只重编改动部分；`--model` 固定的模型在回显与状态行显示模型名。6.9.0：`delegate probe` 在派发前经 Pi 本身并行探测各档模型连通（规则列表展开），不建任务、不占名额，默认档会选的模型失败退出 1。6.5.0：`--after` 的写入步骤从上游改动起步；`apply A B …` 按序合入后重跑一次验收。6.4.0：同事不清楚时先提问（状态「提问」），回答提问不算返工。6.3.0：未配默认档时走强档，便宜档只读。6.2.0：可选默认档 `tiers.default`，不写 `--tier` 的任务都用它。6.1.0：两档模型都配置后才能派任务，未配置时报错列出 Pi 可用模型，`delegate tiers` 一步配置。6.0.0：同事只用 Pi，`--tier cheap|strong` 改为选配置 `tiers` 里的 Pi 模型，便宜档失败换强档模型重跑；删除 Codex 同事、`--agent` 与 Codex 容量项。

### `delegate tui`（只读终端看板）

`delegate tui` 会自动查找并 `exec` 独立二进制 `delegate-tui`（独立 crate `crates/delegate-tui`，不把 TUI 依赖引入 `delegate` 核心；随 `release-binary.sh` / `fetch-binary.sh` / `install.sh` 提供 Linux x86_64 与 aarch64 静态 musl 构建）。优先通过单次 `delegate snapshot --json` 零锁采集全机状态并用 Linux `inotify` 监听 `<state>/events.seq` 驱动增量刷新（向下兼容旧 CLI 的多命令采集），按需调用 `result --no-mark` 与 `diff --stat`，默认无 `--repo`/`--root` 时直接由 `delegate snapshot --json` 读取 `state/repos.json`、`state/*.slot` 与当前仓发现全部已登记仓库（兼容旧 CLI 时回退扫描 `$HOME`；也可用 `--repo <路径>`、`--root <目录>` 或 `--interval <秒>` 覆盖；仅展示本机登记的含远端执行 run，远端机器本地直接派发的 run 暂不跨机聚合）：

- **左栏 / 上栏**：顶部带「0总览 / 1任务 / 2事件 / 3合入 / 4候选 / 5工作 / 6账号」七标签页切换；总览页展示各 `agy` 账号与额度池真实剩余 `Gauge`（仅经 `agy -p /usage` 采集并附重置倒计时与数据年龄）、按账号近 24h `Sparkline`、按用途/模型切换（`v`）的 `BarChart` 及近 7 天派发→合入耗时与一轮通过率走势；账号页同时提供基于同周期 `/usage` 历史的额度耗尽区间预测、档位状态与就地切号/选 `auto`；任务页采用圆角边框与对齐列头，按仓库与状态分组并附确定性续派队列分组，续做链折叠为末环并标轮次，运行中任务展示判活状态与当前动作；事件页跟随全机 `events.jsonl`；合入页展示各批次合并/跳过/自主合入状态；候选页展示跨仓库 Radar 候选板；工作页展示全机工作表与依赖链；仅用 4 种状态色加灰度层级，窄屏（80 列）自动降级为无错位文本并按中文显示宽度对齐。
- **右栏 / 下栏**：任务页显示「正在做什么」、卡住提醒、选中任务的结论摘要（可滚动）、最近命令、带比例条的按目录改动分布与下一步提示；事件页显示选中事件的完整 payload；合入页展示逐 run 合并/跳过明细与 `apply --keep-commits` 命令；候选页展示证据锚点、建议做法、粗估成本与 `radar pick` 命令；底栏展示 lane 占用迷你仪表、主机负载与刷新时间；数据在后台线程并发采集，刷新使用终端同步更新与差异重绘，无变化不重绘、不闪屏、不卡界面。
- **常用键位**：`0-6`/`o`/`e`/`m`/`b`/`w`/`A` 切总览/任务/事件/合入/候选/工作/账号页、`v` 切总览分布图维度（用途/模型）、`Q` 刷新 `agy` 真实额度、`f` 切换事件自动跟随、`↑↓`/`jk` 选择或滚动、`Tab` 切栏、`Enter` 看完整结论/切换选中账号或生成主命令、`d` 看 `diff --stat`、`/` 即时过滤、`r` 手动刷新、`a`/`c`/`s`/`p` 预览命令（只读不执行，可复制或退出打印）、`?` 帮助、`q` 退出。

`delegate` 5.27.1：取消 agent 默认 timeout，加入 idle/command stall guard 与 Claude Code 50 分钟 check-in；配置支持 `stall`/`checkIn`。

`delegate` 5.26.0：apply 后保留七天轻量记录，reply 自动从当前 HEAD 续做；apply 列提交并可 `--keep-commits`。`start --share` 共享按次输入，`resources`/`busy` 查询实际占用，`acceptBlind` 提醒验收盲区；压缩失败按上限显示，wait 钩子给混合命令拆分步骤，续跑命令沿用调用路径。

`delegate` 5.25.0 少写少选：仓库 `standing` 固定说明与 `agentDeny` 提示自动附到每份任务说明，`defaults` 给 worktree/protect/acceptAlso/timeout/evidence 默认值并在启动时回显来源；省略 `--name` 取说明首行；无参或带名字的 `wait` 交付下一个结束的任务并在末行写明还剩几个、再跑哪条命令；答复显示上限默认 20000（`resultChars`），`--max-answer` 超长时自动压缩一次；`apply` 合入后自动清理并说明验收是否仍有效；任务收尾不再终止 sccache 这类共享守护进程。入口 `SKILL.md` 精简为三步、场景表与档位，状态与边界细节移到 `references/reading-results.md`。

`delegate` 5.24.0 源构建目录提示：写入收尾在 worktree 测量后限时测源仓库的现存 target（根有 Cargo.toml），用户/仓库 `sourceBuild.paths` 可替换、空数组关闭；status/wait 取同源最近缓存，默认 60 GiB 提示总量、各路径大小及[先量再删的参考](skills/delegate/references/cleanup.md)，`DELEGATE_SOURCE_BUILD_WARN_GIB` 可调、0 关闭，JSON 独立追加 `sourceBuildDisk`；失败不改结论，只提示不删。

`delegate` 5.23.0 清理能力：移除 worktree 连带回收 compose 容器和匿名卷，`clean` 同时回收目录已消失的旧孤儿，Docker 失败只提示；写入任务结束时缓存磁盘占用，`status`/`wait` 达默认 20 GiB 时提示总量、最大三项与清理命令（`DELEGATE_WORKTREE_WARN_GIB` 可调、0 关闭）。改写后手动合入并提交立即 `clean <run> --force`；门禁直接后台跑命令本身或 `lane`，避免 `pgrep -f` 轮询匹配自身。

`delegate` 5.18.0 配置能力：用户级 `${XDG_CONFIG_HOME}/delegate/config.json`（未设变量时使用系统约定的用户配置目录）与仓库 `.delegate.json` 合并，通用 deny/env/返工预算/默认验收与证据只写一次；`--evidence` 失败不改状态，deny 支持精确 argv，手动合入自动检测，固定同事仍显示默认档位。当前整机/仓库防失控默认 32/0，重命令默认 2（worktree 同计），环境变量优先；仓库可按 `(argv, exact)` 撤销用户 deny，`configSources` 可追溯来源。规则见 [用户与仓库配置](skills/delegate/references/configuration.md)。

---

## 🚀 安装与使用

### 一键安装（新机器）

```bash
# 主仓库
curl -fsSL https://git.aiatechco.com:31443/zji996/AIA-skills/raw/branch/main/scripts/bootstrap.sh | bash

# GitHub 镜像
curl -fsSL https://raw.githubusercontent.com/zji996/AIA-skills/main/scripts/bootstrap.sh | bash
```

脚本把仓库克隆到 `~/.local/share/aia-skills`（主仓库克隆失败时自动改用 GitHub），然后以符号链接方式安装全部技能。重复运行同一条命令即可更新。常用参数：

```bash
curl -fsSL <上面任一链接> | bash -s -- --ref v3.0.0              # 安装指定版本
curl -fsSL <上面任一链接> | bash -s -- --copy                    # 拷贝安装
curl -fsSL <上面任一链接> | bash -s -- agent-handoff             # 只安装指定技能
curl -fsSL <上面任一链接> | bash -s -- --github --dir ~/aia-skills # 指定来源与位置
curl -fsSL <上面任一链接> | bash -s -- --with-pi                 # 同时安装或更新 Pi（delegate 需要）
```

依赖 `git` 与 bash 4+（macOS 需先 `brew install bash`）。`delegate` 仅支持 Linux x86_64 / aarch64：安装时按 `skills/delegate/bin.sha256` 校验并下载 GitHub Release 中的静态二进制；下载不到而本机有 cargo 时从 `crates/delegate` 编译，不匹配校验和会提示未发布校验，`AIA_SKILLS_REQUIRE_VERIFIED=1` 可拒绝该构建。另需同事的 CLI `pi`。

### Pi 与 pi-kit

`delegate` 调度的 Pi 由子模块 [`third_party/pi-kit`](third_party/pi-kit) 安装。子模块地址是相对地址，从主仓库克隆时指向主仓库的 pi-kit，从 GitHub 克隆时指向 GitHub 上的 pi-kit。

- `--with-pi`：拉取子模块并运行 `pi-kit --additive`，安装或升级 Pi 与 pi-kit 管理的 Pi 包，不改动现有设置；没有 Node.js 22.19+ 时会免 root 安装便携版。完成后提示是否缺少 `python3`。
- `--with-pi-sync`：改为运行 `pi-kit --sync`，应用 pi-kit 的完整声明式配置，保留 `defaultProvider`、`defaultModel` 等本机设置。
- 需要镜像加速时可设置 `PI_KIT_MIRROR=cn` 强制使用 npmmirror。

修改 pi-kit 时直接在 `third_party/pi-kit` 中提交，并先推送 pi-kit；然后在本仓库提交子模块指针，否则别人拉不到指针指向的提交。

### 从克隆的仓库安装

```bash
git clone https://git.aiatechco.com:31443/zji996/AIA-skills.git
# 或 GitHub：git clone https://github.com/zji996/AIA-skills.git
cd AIA-skills

./scripts/install.sh                                  # 安装全部技能（符号链接）
./scripts/install.sh repo-governance agent-handoff    # 只安装指定技能
./scripts/install.sh --copy                           # 拷贝安装，适合没有本仓库工作区的机器
./scripts/install.sh --status                         # 查看已安装条目，标出过期的拷贝
./scripts/install.sh --uninstall [<skill>...]         # 只删除本仓库安装的条目
./scripts/install.sh repo-governance --force          # 替换指向其他来源的同名符号链接
./scripts/install-git-hooks.sh                         # 本仓库启用 pre-push 门禁
./scripts/install-claude-hooks.sh                      # 写入 ~/.claude/settings.json 的 delegate Stop / PreToolUse hooks
./scripts/sync-hosts.sh                               # 经 SSH 并行更新其他机器（列表在 ~/.config/aia-skills/hosts）
```

**安装模式**：开发机用默认的符号链接，只存一份源码，保存即生效，`git pull` 就是更新。服务器、容器或其他机器用 `--copy`，每份拷贝带 `.aia-skills-install` 标记，记录来源、版本和提交号，`--status` 据此判断是否过期；更新时重新运行 `--copy` 即可。两种模式可以互相切换。

**安装目录**：

| 目录 | 读取它的 Agent |
| --- | --- |
| `~/.agents/skills/` | Pi、Codex、Cursor、Kilo |
| `~/.claude/skills/` | Claude Code |

普通技能只装到这两个目录。在 frontmatter `metadata.exclude-agents` 中声明了排除对象的技能（目前只有 `delegate` 排除了 `pi`，防止 Pi 调度自己）会跳过 `~/.agents/skills/`，改为装到其余 Agent 各自的目录：`~/.codex/skills/`、`~/.cursor/skills/`、`~/.kilo/skills/`。

安装脚本只处理本仓库安装的条目：不会覆盖别人的目录；会移除多余位置上的旧条目，也会清理源技能已删除的条目。退役技能时直接从 `skills/` 删除，重新运行安装脚本即可清理干净。

### 技能间约定

技能彼此独立，唯一的数据约定是：`agent-handoff` 的快照脚本读取 `delegate` 写在 `.local/run/delegate/<run_id>/`（也兼容旧 `.local/run/pi/`）下的 `meta.json`（含 `worktree.path`）、`exit_code`、`.delivered` 与 `.applied` 来判断未完成任务和未合并 worktree。修改这些文件名或含义时要同步修改 `handoff-snapshot.sh` 及其测试。

### 版本与发布

- 每个技能的 `metadata.version` 按语义化版本维护：只改文档写法升修订号，新增能力升次版本号，命令、参数或文件格式不兼容时升主版本号。
- **版本号延迟分配**：开发与委派同事不改 `SKILL.md` / `Cargo.toml` / `Cargo.lock` 的版本号，只把改动条目追加到 [CHANGELOG.md](CHANGELOG.md) 顶部的 `## Unreleased` 段（`- \`<技能>\`：...`），避免并发任务互撞版本号。
- **一步发版（带二进制的技能）**：运行 `./scripts/release-binary.sh release <技能> <X.Y.Z|patch|minor|major>`，依次执行 `bump` → `build` → 提交版本文件 → `git push origin HEAD:main` → `publish` → `./scripts/install.sh` → `./scripts/sync-hosts.sh`；任一步失败立即停止并打印续跑命令，可用 `./scripts/release-binary.sh release <技能> --from <step>` 从中断步骤续跑。
- **分步发版与失败关闭**：也可手动分步执行 `bump` / `build` / `verify` / `publish`（纯文档技能执行 `bump`、提交并推送即可；全仓发版可再打 `git tag vX.Y.Z`）。`publish` 在打标签与上传前会先 `git fetch origin`，确认 `HEAD` 已推到 `origin/main`（是 `origin/main` 的祖先或等于）且本地同名标签未指向其他提交（指向 `HEAD` 则复用），否则直接报错退出，不打标签、不上传。
- 拷贝安装的机器更新到某个版本：`git checkout vX.Y.Z && ./scripts/install.sh --copy`。

### 验证

```bash
./scripts/verify.sh   # check.sh、并行单元测试（scripts/run-tests.py）；有 cargo 时运行 crates/delegate 的 clippy；输入未变的步骤复用上次通过
```

`check.sh` 检查 frontmatter、`description` 是否含中英文触发场景、README 索引、`evals/` 触发示例、断链与脚本语法。`install-git-hooks.sh` 只设置本仓库的 `core.hooksPath`，pre-push 会调用 `verify.sh`。依赖 Python 3.11+ 和 PyYAML；图像生成脚本另需 `curl`、`jq` 和有效的 OpenAI API key，调用会产生 API 费用。

---

## 📄 授权与许可

本项目采用 [MIT License](LICENSE) 开源。
