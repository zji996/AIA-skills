---
name: delegate
description: 把可独立验收的任务交给同事 Agent 在后台并行完成，你只收结果并把关。遇到这些情况时主动使用，不必等用户开口：要读很多文件但只需结论；改动前想要第二意见或独立审查；有两个以上互不依赖、能用命令验收的子任务；需要有人看截图或设计图给意见。同事是 Pi 或 Antigravity CLI（agy，登记为 agy/<模型> 的别名走它），按用途 scout（只读侦察）、review（只读严密审查）、write（写代码带验收）派发；也可用 models 别名指定模型与默认 thinking；fit 适配卡记录四类任务的首选，新模型按同题重评。脚本代跑验收命令，只给一行结论与下一步，过程不进主控上下文；容量满会排队，重检查也限流。Use to delegate or parallelize work proactively to Pi or agy (Antigravity CLI) colleagues, and to get a second opinion, code review or image review from another model, judged by results.
license: MIT
compatibility: Linux x86_64 或 aarch64；入口是安装时下载的静态二进制 bin/delegate，不需要 Python；需要 pi CLI（pi-kit 的 rpi 通道下 `pi` 即 rpi）。
metadata:
  version: "6.45.2"
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
$D wait                                                                  # 放后台（agy 用 run_command 设 WaitMsBeforeAsync=500）；下一个结束即返并印续等命令
```

总路数、仓库路数、模型并发或内存不足时自动排队、不换模型；`status` 看排队原因与前方人数，`--json` 看 `queue`。愿意满时换候选再加 `--overflow next`。

每个任务回短行（状态/改动/验收/下一步）与答复。写入任务看 `$D diff <name>`，满意后 `$D apply <name>`（锁内先查 `.applied`，已合入报「已于 `<时间>` 合入」零改动退出；未提交重叠默认拒，`--allow-dirty-overlap` 接受；末行印 `$D files <run>`，`--commit` 仅暂存本批压单提交，`ok: false` 明报失败阶段）；细节见 [并行合入](references/parallel-safety.md) 与 [清理](references/cleanup.md)。

| 想要 | 命令 |
|---|---|
| 读材料、审查、第二意见 | `$D start --read-only "…"`；限篇幅加 `--max-answer 800` |
| 看截图或设计图 | `$D start --read-only --image shot.png "…"` |
| 改代码，测试通过才算完 | `$D start --worktree --accept "make test" "…"`；不许碰加 `--protect tests/`，本次放开加 `--allow tests/unit/`（正文提及未放开的保护路径会提示）；验收时限可加 `--accept-timeout` 或在 `.delegate.json` 设 `acceptTimeout` |
| 追问或返工 | `$D reply <name> "…"`（运行中默认直接追加，Pi 支持 RPC `steer` 时轮中即注入，否则同 `agy` 本轮末同会话合并接着答，收尾后退回新 round，`status`/`card`/TUI 显「已追加 N 条，待投递 M 条」，`stop` 取消待投递，`--when-done` 兼容；已结束默认同步源基线保 slot 缓存，跳过 `--no-sync`，冲突交同事 `--sync=agent`，放开保护 `--allow PATH`，新会话 `--fresh`） |
| 审某个已完成任务的工作树 | `$D start --read-only --in <name> "…"`（已清理时自动从归档重建，不可重建则报错退出）；先 `wait` 确认上游结果再派下一步 |
| 自己跑重检查、测性能 | `$D lane make check`；测量用 `lane --measure` |
| 节点不稳、换档前先测连通 | `$D probe`：走 Pi 本身（不是裸 HTTP）测各档模型，不建任务、不占名额 |
| 看审查卡 / 预合并快进 / 改动 / 停 / 清 | `$D card <run>`（L1 一屏审查卡：摘要与各轮原文、风险项、测试与验收、预合并、折叠改动）、`$D diff <run> --risky`（L2 风险 hunk）、`$D diff`（L3 完整）、`$D result [--round N\|--all\|--rounds]` / `$D stop` / `$D clean --finished`；交付或 `$D sign` 后 `_merge-worker` 在 `<cache>` 预合并并钉 `refs/delegate/premerge/<run>`，L0 实显预合并/快进状态，`$D apply` 命中快进 <2s 写回并跳过复验，否则回退三方合并，见 [审查卡与预合并](references/review-card.md) |
| 自动审-改闭环与分层并行验收 | `.delegate.json` 配 `acceptRules` 按改动路径分层（未命中或改构建配置 fail-closed 跑全量）；快层通过后慢层与只读审查员并行（任一失败互停、被取消不记 `PASS`，两者均过才 `delivered`，返工只重跑受影响层）；审查模型按 `--review-model` > 仓库 > 用户 > review 档选，不可用回退 review 档并在卡上明示；首行缺结论同会话尾部补问 1 次；`review.extra` 命中 `controllerReviewPaths`/`risk:high` 且 Claude 池余量够时并行给第二意见（不一致升 `NEEDS_CONTROLLER`），改到 `controllerReviewPaths` 由 `$D sign <run>` 签字，见 [审查要点](references/review-rubric.md) |
| 事件流 / 游标等待 / 确定性续派 | `$D events [--since SEQ] [--follow] [--rebuild] [--json]` 查全机只追加事件流（含 `review_*` / `merge_*` / `radar_*`）；`$D wait --since SEQ` 返回新事件与新游标（Stop hook 按游标拦下未读交付/提问/需关注/`NEEDS_CONTROLLER`）；`$D queue add <name> --after <run\|name> --prompt-file …` / `queue list` 建有序 DAG，上游交付通过且审查 `PASS` 后零模型调用自动起下一步，异常暂停提醒 |
| 串行合入员与自主合入白名单 | `$D merge <run>...` 或 `$D merge --ready` 在独立集成 worktree 基于源 HEAD 串行 3-way 合入、命中 `generated.inputs` 重生、逐个经 lane 跑 `accept`、末尾跑 `mergeAccept`，沿用原提交标题并带 `Delegate-Run` trailer，冲突/红测/受保护路径/未签字或失效 `controllerReviewPaths` 自动回退标 `SKIPPED`；`.delegate.json` `autoMerge`（支持 `!` 排除与 `add:` 纯新增）标「可自主合入 / 待主控点头」并出 `merge-report.md`，主控 `$D apply --keep-commits <merge-run>` 落回后自行 push，详见 [合入员与 Radar](references/merge-and-radar.md) |
| 发散侦察（Radar）与候选板 | `$D radar` 按 `.delegate.json` `radar.collect` 顺序采集并算指纹，未变零 token 退出；变化时派只读 scout 侦察，Host 校验锚点（`文件:行`/commit/输出行号，无锚丢弃）去重写 `<state>/radar/<repo>/board.md`；`$D radar list` 查看，`$D radar pick <id>` 生成任务说明草稿不自动派活，`$D radar schedule` 输出 00:30–04:30 每 2 小时定时安装说明，见 [合入员与 Radar](references/merge-and-radar.md) |
| 统一工作表、补位与 Stop 钩子 | `$D work`（6 列紧凑表并入未绑定 run，缺类别标「缺类别」）、`work show/add/wait/close/drop/defer/import-md/import-dag`（`--class feat\|unblock\|infra\|docs`、`import-dag` 批量导依赖图、`--needs-owner` 建 Q、`--after-applied <run\|Wxx>` 合入解锁且默认关项，需验加 `--needs-verify`/`apply --keep-open`）、`start --work <ID>`；`.delegate.json` 配 `fill`/`hotspots` 按价值配额补位（只数写链、避热点），`$D fill stats` 报停止线（不自动降档）；零锁 Stop 钩子（`$D hook check --format claude\|agy\|json` 单件渐进披露与卡链 30m 穿透）+ `$D hook pending`/`stats`，见 [工作表](references/worktable.md) |
| `/clear` 前交接 / 接手 / 查历史与看板 | `$D handoff` 写交接索引；`/clear` 后 `$D adopt <run>…` 接手整链 caller；`$D history [词…]` / `$D show <run>` / `$D board` |
| 终端看板与 agy 账号/额度 | `$D tui`（`0`/`O` 总览图表、`6`/`A` 账号与真实额度、`Q` 刷新、`Enter`/`u` 切换）；`$D agy account list\|status [--quota]\|add\|use\|note\|remove\|import-agy-swap\|restore-entry`（经 `agy -p /usage` 取真实额度，不自动换号）；`$D usage [--since 24h\|7d] [--by account\|day\|run\|repo] [--json]`，见 [agy 账号与用量](references/agy-accounts.md) |
| 工期校准与进度预估 | `$D speed [--backtest]` 算 `t_chain`/`t_crit`/`P_eff`/`T_merge` 与回测，`$D eta [<run>\|<Wxx>\|--work\|--queue]` 查预估与停表归责（`status`/`work`/`queue`/`wait` 已直显），见 [容量与工期](references/capacity.md) |

`start`/`run` 派发前比对在跑与近 14 天 `history`，相似时在 stderr 提示最多 3 条（只提示不拦截）。

重编译可用用户配置 remote 转到 SSH 构建机、独立排队且传回改动；`DELEGATE_REMOTE=0` 随 reply 继承、显式环境变量可覆盖，见 [配置](references/configuration.md) 与 [运行](references/runtime.md)。

多行说明用 `--prompt-file -` 加 heredoc；示例见 [references/recipes.md](references/recipes.md)，全部选项见 `$D --help`。

## 档位

三个独立维度：`--tier scout|review|write` 选用途（scout 侦察/摘要/找文件/文案，review 严密审查/根因/第二意见，write 改代码带验收）；`--model <别名>` 从配置 `models` 指定模型，可与 `--tier` 同用；`--thinking off|minimal|low|medium|high|xhigh` 选思维长度，不再写进模型名。省略用途与模型时，写入推断 write、只读推断 review，回显提醒且状态标「推断」；scout/review 只读，`--shell` 可查日志/跑只读查询。图片只检查最终模型的 Pi images 声明，不改选模型。上游瞬时失败先同会话续做，仍失败且未改文件才换本档下一个模型；固定模型不换。首次使用用 `$D tiers --alias name=provider/id --scout name --review name --write name --thinking high` 登记并配齐三档；只选已登记的模型，细节见 [references/configuration.md](references/configuration.md)。

强模型按任务性质选思考档，不是一律 high；别名可配 `{"model":"provider/id","thinking":"medium"}` 作为默认。
规格写死的实现（前端、逐项改批量、修测试、只读查代码）用 medium；内核/账本、并发时序、沙箱安全、迁移、找根因用 high。
便宜模型保持配置默认；拿不准先 medium，返工一次就升 high。
优先级：环境变量 > `--thinking` > `--class`（build/design/scout→medium，hard→high）> 档位规则 > 别名默认 > 顶层 thinking > Pi 默认，启动回显生效档及来源。

### 模型适配卡

四类固定：`scout` 侦察（只读代码/数据/根因/审查）；`build` 按规格实现（后端/测试/批量化/机械前端）。
`hard` 疑难改动（内核账本/并发时序/沙箱安全/迁移）；`design` 界面实现（照主控定稿把界面做进产品）。
设计归主控：界面的结构、取舍、文案与观感由主控自己画成设计稿，不委派；`design` 类的说明里给设计稿源码路径，观感仍由主控看截图收尾。
同类任务首次把同份规格各派一路对比 → `$D fit set <类别> <别名> first|ok|avoid "一句话" --sample <规格文件>` 记结论 → 之后直接派首选。
`$D fit` 查看四类卡与未评项，`--json` 看完整 note/sample；记录在用户配置目录的 `fit.json`，每格仅留最新一句（上限 80 字符）。
配置新增别名会列为未评，用各类别记录的 sample 重跑即可；`start --class <类别>` 未传 `--thinking` 时按类别定默认思考档（`thinking <档>〔类别 <类>〕`）并提示避用、首选与未评，不拦派发。

## 任务说明

同事的 `pkill`/`killall`/`pgrep` 仅匹配本次 run 的全部已记录 scope；无可用 cgroup 时提示改用启动时保存的 `kill <pid>`，只读 `--shell` 同样生效。

同事看不到你的会话：写清目标、能改的范围、完成标准（最好是验收命令，复杂的写成脚本）和已知事实。仓库每次都要说的话放进 `.delegate.json` 的 `standing`，见 [references/configuration.md](references/configuration.md)。

## 收结果

- 已交付表示验收通过，仍看 diff；已答复无验收，关键结论自核；Gemini/agy 结论命中内容策略时自动同会话尾部重写 1 次，仍拒则 `result.md` 指向 `review.md` 与 `delegate files <run>` 并标「结论被模型服务拒绝」。
- 状态为「提问」时同事等你决定：`$D reply <name> "<决定>"` 不算返工；只有改过文件后再要求改才算。`wait --any` 报齐上次 wait 后所有终态与提问 run（首个全文，其余一行摘要）。
- 验收未执行不算失败：按提示重排或本地跑。
- 同一仓库多次写入可 `apply A B …` 按序合入，默认归档清 worktree（`worktree.cleanOnApply`）；worktree 内 rebase/merge 后按 `meta.netBase` 算净改动；验收无法沿用时在主干重跑，全量检查按批跑。
- 快照对象钉在 `refs/delegate/<run-id>/*`（`clean` 时移除），普通 `git gc` 安全；切勿执行 `git prune` 或 `git gc --prune=now|all`（见 `gc.log` 删该文件即可）。

状态、退出码与等待细节见 [references/reading-results.md](references/reading-results.md)；续接、并行与长时间推进见 [references/practice.md](references/practice.md)、[references/long-run.md](references/long-run.md)；run 目录见 [references/output-and-files.md](references/output-and-files.md)。
