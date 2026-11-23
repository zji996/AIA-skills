---
name: delegate
description: 把可独立验收的任务交给同事 Agent 在后台并行完成，你只收结果并把关。遇到这些情况时主动使用，不必等用户开口：要读很多文件但只需结论；改动前想要第二意见或独立审查；有两个以上互不依赖、能用命令验收的子任务；需要有人看截图或设计图给意见。同事是 Pi 或 Antigravity CLI（agy，登记为 agy/<模型> 的别名走它），按用途 scout（只读侦察）、review（只读严密审查）、write（写代码带验收）派发；也可用 models 别名指定模型与默认 thinking；fit 适配卡记录四类任务的首选，新模型按同题重评。脚本代跑验收命令，只给一行结论与下一步，过程不进主控上下文；容量满会排队，重检查也限流。Use to delegate or parallelize work proactively to Pi or agy (Antigravity CLI) colleagues, and to get a second opinion, code review or image review from another model, judged by results.
license: MIT
compatibility: Linux x86_64 或 aarch64；入口是安装时下载的静态二进制 bin/delegate，不需要 Python；需要 pi CLI（pi-kit 的 rpi 通道下 `pi` 即 rpi）。
metadata:
  version: "6.31.1"
  binary: delegate
  extra-binaries: delegate-tui
  exclude-agents: pi
---

# Delegate（按结果委派）

你是主控：把边界清楚的任务交给同事（Pi 或 agy），自己继续推进，回来看结果、决定是否采纳。同事不能再委派。

## 用法

```bash
D=<本技能目录>/bin/delegate
$D start --read-only "审查 apps/api 的错误处理，列真实缺陷并写文件:行号"   # 放出，可连放多个
$D wait                                                                  # 放后台；下一个结束就返回，末行给出继续等的命令
```

总路数、仓库路数、模型并发或内存不足时自动排队、不换模型；`status` 看排队原因与前方人数，`--json` 看 `queue`。只有愿意模型满时改用下一候选才加 `--overflow next`。

每个任务回一条短行（状态、改动、验收、下一步）和答复。写入任务看 `$D diff <name>`，满意后 `$D apply <name>`（默认落为未提交改动并列同事提交标题，`--commit` 仅暂存本批文件压成一条提交，`ok: false` 明报失败阶段与已写入文件）；冲突未解拒绝重叠合入，补丁保留可用 `patch` 恢复；等齐用 `wait --all`，见 [并行合入与只读 shell](references/parallel-safety.md)。清理默认归档忽略的证据文件，细节与上限见 [清理](references/cleanup.md)。

| 想要 | 命令 |
|---|---|
| 读材料、审查、第二意见 | `$D start --read-only "…"`；限篇幅加 `--max-answer 800` |
| 看截图或设计图 | `$D start --read-only --image shot.png "…"` |
| 改代码，测试通过才算完 | `$D start --worktree --accept "make test" "…"`；不许碰加 `--protect tests/`，本次放开加 `--allow tests/unit/`（正文提及未放开的保护路径会提示）；验收时限可加 `--accept-timeout` 或在 `.delegate.json` 设 `acceptTimeout` |
| 追问或返工 | `$D reply <name> "…"`（默认自动把本链同步到当前源基线且保留 slot 缓存，冲突时保旧基线并提示；跳过加 `--no-sync`，冲突交同事解用 `--sync=agent`）；放开受保护路径加 `--allow PATH`；开新会话加 `--fresh`，运行中先记追问加 `--when-done` |
| 审某个已完成任务的工作树 | `$D start --read-only --in <name> "…"`；先 `wait` 确认上游结果再派下一步 |
| 自己跑重检查、测性能 | `$D lane make check`；测量用 `lane --measure` |
| 节点不稳、换档前先测连通 | `$D probe`：走 Pi 本身（不是裸 HTTP）测各档模型，不建任务、不占名额 |
| 看全文 / 改动 / 停掉 / 清理 | `$D result` / `$D diff` / `$D stop` / `$D clean --finished`；`status` 首行计数并只列待处理任务与未完成待办（在跑行显示 transcript 尾部判活、卡住提醒与审查状态「审查中 / PASS / REWORK 第N轮 / NEEDS_CONTROLLER」；续做链折成末环一行，详情看 `status <run>`，`--all` 列全部，`--json` 加 `--files` 才带文件数组），末行指向 `history` |
| 自动审-改闭环与主控签字 | 仓库 `.delegate.json` 开 `review`（默认关）后，写入验收通过自动派新上下文只读审查员（不能写文件/派活），输入任务说明、`diff --total` 与 [审查要点](references/review-rubric.md)（可覆盖）；首行严格 `PASS`/`REWORK`/`NEEDS_CONTROLLER`（格式不对失败关闭），`REWORK` 原样退回作者且续审只看增量 diff（不设轮数上限，第 3 轮起发 `needs_attention` 提醒不强杀）。改到 `controllerReviewPaths` 一律升 `NEEDS_CONTROLLER` 并附 diff 全文与 SHA-256 哈希；主控 `$D sign <run>` 签字（`$D sign --check <run>` 验签，改动任意一字即失效），不改 `apply` 语义 |
| 事件流 / 游标等待 / 确定性续派 | `$D events [--since SEQ] [--follow] [--rebuild] [--json]` 查全机只追加事件流（含 `review_*` / `merge_*` / `radar_*`）；`$D wait --since SEQ` 返回新事件与新游标（Stop hook 按游标拦下未读交付/提问/需关注/`NEEDS_CONTROLLER`）；`$D queue add <name> --after <run\|name> --prompt-file …` / `queue list` 建有序 DAG，上游交付通过且审查 `PASS` 后零模型调用自动起下一步，异常暂停提醒 |
| 串行合入员与自主合入白名单 | `$D merge <run>...` 或 `$D merge --ready` 在独立集成 worktree 基于源 HEAD 串行 3-way 合入、命中 `generated.inputs` 重生、逐个经 lane 跑 `accept`、末尾跑 `mergeAccept`，沿用原提交标题并带 `Delegate-Run` trailer，冲突/红测/受保护路径/未签字或失效 `controllerReviewPaths` 自动回退标 `SKIPPED`；`.delegate.json` `autoMerge`（支持 `!` 排除与 `add:` 纯新增）标「可自主合入 / 待主控点头」并出 `merge-report.md`，主控 `$D apply --keep-commits <merge-run>` 落回后自行 push，详见 [合入员与 Radar](references/merge-and-radar.md) |
| 发散侦察（Radar）与候选板 | `$D radar` 按 `.delegate.json` `radar.collect` 顺序采集并算指纹，未变零 token 退出；变化时派只读 scout 侦察，Host 校验锚点（`文件:行`/commit/输出行号，无锚丢弃）去重写 `<state>/radar/<repo>/board.md`；`$D radar list` 查看，`$D radar pick <id>` 生成任务说明草稿不自动派活，`$D radar schedule` 输出 00:30–04:30 每 2 小时定时安装说明，见 [合入员与 Radar](references/merge-and-radar.md) |
| 统一工作表与适时提示 | `$D work`（6 列紧凑表并合入未绑定 run）、`$D work show/add/wait/close/drop/defer/import-md`、`start --work <ID>`（`reply` 继承）、`--after-applied`；`wait`/`apply`/`merge`/`clean` 尾部附带原文指针提示，Stop hook 分级只拦当下阻断项；自动迁移 `notes.json`/`deferred.tsv`，`note` 保留为薄壳，见 [工作表](references/worktable.md) |
| `/clear` 前交接 / 查历史与看板 | `$D handoff` 写一页交接索引（含待办）；`$D history [关键词…]` 查永久索引；`$D show <run>` 看摘要、合入后同文件提交与归档；`$D board` 打印/刷新自动看板（`.delegate.json` `board` 配路径） |
| 交互式终端看板（只读 TUI） | `$D tui`（自动 exec 同目录或 PATH 中的 `delegate-tui`，随 `install.sh` / `fetch-binary.sh` 安装）：跨仓库按状态分组、任务行判活与审查状态、确定性续派队列及「任务 / 事件 / 合入 / 候选」四标签页（`1-4`/`e`/`m`/`b` 切页、`f` 跟随、`↑↓`/`jk` 选择、`Tab` 切栏、`Enter` 结论/命令、`d` diff 统计、`/` 过滤、`r` 刷新、`a/c/s/p` 生成写命令并复制、`q` 退出） |

`start`/`run` 派发前比对在跑与近 14 天 `history`，相似时在 stderr 提示最多 3 条（只提示不拦截）。

重编译可用用户配置 remote 转到 SSH 构建机、独立排队且传回改动；`DELEGATE_REMOTE=0` 随 reply 继承、显式环境变量可覆盖，见 [配置](references/configuration.md) 与 [运行](references/runtime.md)。

多行说明用 `--prompt-file -` 加 heredoc；示例见 [references/recipes.md](references/recipes.md)，全部选项见 `$D --help`。

## 档位

三个独立维度：`--tier scout|review|write` 选用途（scout 侦察/摘要/找文件/文案，review 严密审查/根因/第二意见，write 改代码带验收）；`--model <别名>` 从配置 `models` 指定模型，可与 `--tier` 同用；`--thinking off|minimal|low|medium|high|xhigh` 选思维长度，不再写进模型名。省略用途与模型时，写入推断 write、只读推断 review，回显提醒且状态标「推断」；scout/review 只读，`--shell` 可查日志/跑只读查询。图片只检查最终模型的 Pi images 声明，不改选模型。上游瞬时失败先同会话续做，仍失败且未改文件才换本档下一个模型；固定模型不换。首次使用用 `$D tiers --alias name=provider/id --scout name --review name --write name --thinking high` 登记并配齐三档；只选已登记的模型，细节见 [references/configuration.md](references/configuration.md)。

强模型按任务性质选思考档，不是一律 high；别名可配 `{"model":"provider/id","thinking":"medium"}` 作为默认。
规格写死的实现（前端、逐项改批量、修测试、只读查代码）用 medium；内核/账本、并发时序、沙箱安全、迁移、找根因用 high。
便宜模型保持配置默认；拿不准先 medium，返工一次就升 high。
优先级：环境变量 > `--thinking` > 档位规则 > 别名默认 > 顶层 thinking > Pi 默认，启动回显生效档及来源。

### 模型适配卡

四类固定：`scout` 侦察（只读代码/数据/根因/审查）；`build` 按规格实现（后端/测试/批量化/机械前端）。
`hard` 疑难改动（内核账本/并发时序/沙箱安全/迁移）；`design` 界面实现（照主控定稿把界面做进产品）。
设计归主控：界面的结构、取舍、文案与观感由主控自己画成设计稿，不委派；`design` 类的说明里给设计稿源码路径，观感仍由主控看截图收尾。
同类任务首次把同份规格各派一路对比 → `$D fit set <类别> <别名> first|ok|avoid "一句话" --sample <规格文件>` 记结论 → 之后直接派首选。
`$D fit` 查看四类卡与未评项，`--json` 看完整 note/sample；记录在用户配置目录的 `fit.json`，每格仅留最新一句（上限 80 字符）。
配置新增别名会列为未评，用各类别记录的 sample 重跑即可；`start --class <类别>` 提示避用、首选与未评，不拦派发。

## 任务说明

同事的 `pkill`/`killall`/`pgrep` 仅匹配本次 run 的全部已记录 scope；无可用 cgroup 时提示改用启动时保存的 `kill <pid>`，只读 `--shell` 同样生效。

同事看不到你的会话：写清目标、能改的范围、完成标准（最好是验收命令，复杂的写成脚本）和已知事实。仓库每次都要说的话放进 `.delegate.json` 的 `standing`，见 [references/configuration.md](references/configuration.md)。

## 收结果

- 已交付表示验收通过，改法仍要看 diff；已答复没有验收，关键结论自己核实。
- 状态为「提问」时保留未执行 lane 作业的原排队时间（不抢占运行者）；同事等你决定：`$D reply <name> "<决定>"` 回答，不算返工；只有改过文件后再要求改才算。`wait --any` 每次列出上次 wait 之后所有终态与提问态 run（首个给全文，其余一行摘要加查看命令）。
- 验收未执行不算执行失败：按提示重排或本地跑。
- 同一仓库的写入一次 `apply A B …` 按序合入，成功后默认归档并清理 worktree（`worktree.cleanOnApply`，续做链等整条链合入后再清）；同事在 worktree 里 rebase/merge 源主干后，改动、保护检查、`diff`/`apply` 均按 merge-base（`meta.netBase`）算净改动。验收没法沿用的任务在合并后的主干重跑一次各自的验收，全量检查仍由你按批跑。
- 每个 run 依赖的快照对象钉在源仓库 `refs/delegate/<run-id>/*` 下（`clean` 时一并移除，仅 `cleanOnApply` 删 worktree 时保留），普通 `git gc` 安全；切勿在有 delegate 记录的仓库执行 `git prune`、`git gc --prune=now|all` 等立即删不可达对象的命令（若 git 提示 `run 'git prune'`，删除 `.git/gc.log` 即可）。旧 run 对象已丢时，`status`/`diff` 会提示并给出 `git -C <worktree> diff HEAD`（未跟踪文件先 `add -N`）救回命令。

状态、退出码与等待细节见 [references/reading-results.md](references/reading-results.md)；续接、并行与长时间推进见 [references/practice.md](references/practice.md)、[references/long-run.md](references/long-run.md)；run 目录见 [references/output-and-files.md](references/output-and-files.md)。
