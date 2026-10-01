---
name: delegate
description: 把可独立验收的任务交给同事 Agent 在后台并行完成，你只收结果并把关。遇到这些情况时主动使用，不必等用户开口：要读很多文件但只需结论；改动前想要第二意见或独立审查；有两个以上互不依赖、能用命令验收的子任务；需要有人看截图或设计图给意见。同事分两档：便宜档 Pi（Gemini，读材料、摘要、文案、看图）与强档 Codex（GPT，写代码、严密审查），只读默认便宜档、写入默认强档，便宜档失败自动升档。脚本代跑验收命令，只给一行结论与下一步，过程不进主控上下文；整机与仓库容量、重检查有上限。Use to delegate or parallelize work proactively, and to get a second opinion, code review or image review from Gemini (Pi) or GPT (Codex), judged by results.
license: MIT
compatibility: Linux x86_64 或 aarch64；入口是安装时下载的静态二进制 bin/delegate，不需要 Python；需要所选同事的 CLI：pi 或 codex。
metadata:
  version: "5.18.0"
  binary: delegate
  exclude-agents: pi
---

# Delegate（按结果委派）

你是主控：把边界清楚的任务交给同事，自己继续推进，回来只看结果。**是否采纳、如何整合始终由你决定。** 同事不能再委派，所有结果都回到你这里。

## 三步用法

```bash
D=<本技能目录>/bin/delegate
$D start --read-only --name review-api "审查 apps/api 的错误处理，只列真实缺陷，写明文件:行号"   # 1. 放出（可连放多个）
$D wait                                                                                  # 2. 等：放后台，结束时会返回
#                                                                                        # 3. 按结论行的下一步处理
```

- **等待**：`wait`、`run`、`reply --wait` 阻塞到结束，默认放后台，不用 `status` 轮询。宿主能把后台进程的每行输出变成事件时（Claude Code 的 Monitor）优先 `$D wait --stream`；只有结束通知时重复后台 `$D wait --any`，处理当前已结束任务后再放同一命令。Claude Code 的 Bash 用 `run_in_background: true`（`hooks/claude-code-background.py` 可拒绝前台等待）。没有后台通知且调用限时时用 `--max 4m`，返回 75 后再等。无参 `wait` 只收本会话，无会话标识时收全部，`--all` 收本仓库全部。
- **逐个处理**：按“答复”命令读正文、按“下一步”合并。每任务只允许一个等待者；已有活等待者时跳过，全被覆盖退出 `76`，通知给原等待者。脚本加 `--json`，`completionTiming` 区分旧结果与等待中新完成，`sourceDrift.overlap` 提示源改动重叠。
- **结论**：每任务一条短行，随后附答复；细节用 `status --json`、`diff` 按需读取。答复超过 6000 字（`DELEGATE_RESULT_CHARS`）显示头尾，全文用 `$D result <name>`；只看过截断答复的任务 `clean --finished` 会保留，读过全文或加 `--force` 才删。后台进程回收与 worktree 源问题会提示。

## 场景速查

| 想要 | 命令 |
|---|---|
| 读材料、只要结论 | `$D start --read-only "…"` |
| 独立审查、第二意见 | `$D start --read-only --tier strong "…"`（多路并行时各起一个，按子系统拆开） |
| 看截图或设计图 | `$D start --read-only --image shot.png "…"` |
| 改代码，测试通过才算完 | `$D run --worktree --accept "make check" "…"`，满意后 `$D apply <name>`；不许同事碰的路径加 `--protect tests/ --protect docs/spec.md` |
| 接着上一轮追问或返工 | `$D reply <name> "…"`（后台启动，同一会话、同一 worktree；需当场收结果加 `--wait`）；开新会话加 `--fresh`；换同事加 `--agent codex`；你之后又改了代码想让它看到，加 `--sync` |
| 先 A 后 B | `$D start --after <A> …`；审 A 的结果加 `--in <A> --read-only` |
| 自己跑重检查 | `$D lane make check`（与同事验收、证据共用队列） |
| 同时在几个仓库派了任务 | 放一个后台 `$D wait --machine`，整机的都会收到 |
| 多路并行，谁先完工先处理谁 | `$D wait --stream`（流式通知）或重复后台 `$D wait --any`（收当前已结束的） |
| 看改了什么 / 停掉 / 清理 | `$D diff <name>` / `$D stop <name>` / `$D clean --finished` |

多行说明用 `--prompt-file -` 加 heredoc；总加 `--name`，之后用它指代整段对话（各命令都落到最新一轮回复）。更多完整示例、任务说明模板与常见坑见 [references/recipes.md](references/recipes.md)。

`--protect`/`--protect-reason` 与 `generated.paths` 重叠时只允许生成命令改动，收尾复跑核对，产出不一致报“生成物被手改”。

## 选档位

| `--tier` | 默认给 | 适合 | 不适合 |
|---|---|---|---|
| `cheap`（Pi） | 只读任务 | 广度侦察（在代码里找事实、列清单）、界面与前端实现和审查（界面实现的首选）、文案、看图；重要审查时作为另一路视角 | 需要严密判断与风险评估的逻辑（量化边界、并发、事务、迁移） |
| `strong`（Codex） | 写入任务 | 后端与协议代码、疑难 bug、严密审查与风险评估、核实便宜档的结论 | 界面视觉与交互打磨（功能对、层级和审美弱）；只图省事的批量杂活 |

便宜档答复畸形、出错、超时或验收失败且未改文件（或只读）时，自动强档重跑一次，带 `escalatedFrom`。`--agent pi|codex` 固定同事，不升档；显示其默认档位（cheap/strong），JSON 带 `agentPinned: true`，旧记录只推断显示。

## 写好任务说明

同事不知道你的会话，说明需要自足：**目标**（要什么结果）、**边界**（能改哪、不能改哪）、**完成标准**（`--accept` 命令，或答复要包含什么）、**已知事实与决定**（别让它重新猜）。新增旁路能力（投影、索引、记忆、缓存、通知）时再写一句**失败语义**：哪些失败必须中止主路径，哪些只记诊断、稍后重试；不写时强档倾向于处处失败关闭。验收命令会自动附在说明末尾，并教同事用 `lane` 自检。

## 经验建议

以下来自实际使用，是建议而非规则，按具体情况取舍。

- **续接还是新开**：返工建立在它已有理解上（审查意见、补测试）时用 `reply`；会话很长、答复开始畸形或方向已变时 `--fresh` 更好。worktree 是启动时的快照，之后你在自己工作区的改动它看不到，用 `reply --sync` 同步进去。
- **什么时候自己改**：根因已定位、改动小、你清楚怎么改时，自己改常常更快——派发、等待、审查、合并有固定开销。需要只有你知道的背景时也自己接手。
- **返工预算**：写入对话默认只给同事 1 次返工（`.delegate.json` 的 `maxRework`，`null` 不限），第二次 `reply` 会被拒绝（退出码 2）——这时 `diff --total` / `apply` 后自己改。上一轮为 `timeout` 的 `reply` 是续做，不计返工次数，即使预算已用完也可继续，meta 记 `continuation: timeout`；其他失败仍按原规则计数。短修正（改名、补一处断言，说明 ≤600 字）用 `reply --minor`，不计次数，但该轮改动超过 60 行会补记一次；确需越限用 `--over-limit '<原因>'`，原因记在 meta。零改动的追问不计次。
- **两档怎么搭配**：Pi 找到的代码事实更多、偶有把现状说混；Codex 更准确、风险意识更强。分量重的审查两路并行、由你合并，通常好于任一方。界面实现默认 `--tier cheap` 并要求截图，强档只接界面里的状态与数据逻辑；Pi 的实现要单独扫一遍静默吞错（空 `catch`）和答复里的夸大。便宜档近乎免费，侦察不必压缩范围。
- **让错误当场暴露**：审查说明给重点并注明不限于此，给待证伪的假设而不是结论；要求现状断言附 `文件:行号`、仓库外事实（配置键、CLI 参数、API 字段、版本号）附一手出处——便宜档会编出看似合理的名字。看图审查把影响判断的真实数据写进说明，并要求写明从图上哪里读出。
- **开几路**：写入 3–4 路最划算，瓶颈是主控审 diff 和跨路一致性；先提交共用基础（公共组件、约定），说明其误用方式；单路一个子系统、约 15 个文件以内。只读侦察可再并行几路。主控重检查也走 `lane`，避免 CPU 超卖拖过超时。
- **并行写入**：用 `--protect-reason model_loop.rs '另一任务负责；恢复逻辑必须留在这里'` 划清所有权（原 `--protect` 仍可用）；同事需要改受保护路径时须停下报告，不得搬逻辑或削弱测试绕路。同仓库串行 `apply`；新增 `NNNN_` 文件的编号冲突会警告，由你审查和改号。重写、迁移类任务把测试也保护起来，免得“改测试”成为最省事的通过方式；`--accept` 覆盖仓库级门禁，worktree 跑不了全量时至少跑相关 gate；合并后要跑全量（除非结论给出 `acceptStillValid: true`），多路合并的类型漂移只会在这里暴露：`.delegate.json` 设 `"applyVerify": true` 让 `apply` 自动在主干跑默认验收（结论 `verify`，失败退出 1；需要时加 `--verify` / `--no-verify`），开启后慢 apply 放后台。`start` 发现同仓库仍在运行的写入任务会在 stderr 列出其已改文件，任务说明点名其中路径时以 `overlap:` 提示，考虑 `--after` 或 `--protect`。
- **编排**：常用两种——强档实现后接 `--after impl --in impl --read-only` 的便宜档预审（你拿着预审看 diff）；便宜档侦察后接 `--after scout --worktree` 的强档按清单实现。链条不宜太长，每多一步误差叠加一次，需要判断的节点你插进来看。

## 读结论并把关

`status`/`wait` 短行给名字、状态、同事/档位、耗时、改动、最近命令、验收、证据与下一步；答复附后。完整诊断取 `--json`、`diff`、`result`。

| 状态（JSON state） | 含义 |
|---|---|
| 已交付（`delivered`） | 验收通过；改法仍要看 `diff` |
| 已答复（`answered`） | 未设验收；关键结论去代码核实 |
| 未通过（`rejected`） | 验收失败或保护违规，`--json` 查看 `accept.tail`/`protectViolation` |
| `malformed` / `failed` / `timeout` / `killed` / `crashed` / `stopped` | 答复畸形 / 出错 / 超时 / 进程异常 / 被终止 |

state 只描述答复。只读任务写文件仍是 `answered`，短行提示，JSON 带 `readOnlyViolation`（隔离未合并）或 `workspaceChanged`（原地无法归属）。改动来自前后快照：含 shell 改动，排除原有脏改动与验收副产物。退出码：`0` 成功，`1` 其他结局，`2` 用法错误/拒绝，`75` 仍运行，`76` 已有等待者。

`agentDeny` 默认按 argv 前缀拦截，`exact: true` 仅拦完全相等的参数；同事及其 `lane` 命中退出 `77` 并给替代提示，结论带 `denied`。同事跑相关检查，主控合入一批后跑全量；验收、证据、setup、apply 生成与复验、主控 lane 用原 PATH。

`--evidence <命令>` 收集模型评测等旁路证据，验收通过后（无验收则答复后）经 lane 运行；`--evidence-timeout` 默认 30m，排队不计时。失败/超时只记 `evidence`，永不改 state 或退出码；验收失败或保护违规跳过。reply 继承，`--no-evidence` 关闭。手动合入后，`status`/`wait`/`clean` 检查源工作树是否包含全部最终内容、删除与执行位；全含则显示“已合入（主干已含改动）”，JSON `appliedBy: "detected"`，可清理；部分包含或读取失败仍提示 apply。

通用 deny/env/返工预算/默认 accept 与 evidence/容量写到 `${XDG_CONFIG_HOME:-~/.config}/delegate/config.json`，仓库 `.delegate.json` 合并覆盖。`configSources` 记来源；配置错误指出文件与字段，退出 2。合并与撤销见 [references/output-and-files.md](references/output-and-files.md#用户与仓库配置)。

`--json` 保留原完整结构：`shape` 含目录增删行、改后最大文件、配置与删除路径；`*More` 是省略数量，逐文件在 `changes.json`。`changes` 是本轮，`pendingChanges` 是累计待合入量，零改动 reply 仍可能需 `diff --total`/`apply`。

慢 `apply` 可后台执行；失败后重试生成。JSON 的 `acceptStillValid: true` 证明验收成功且未改树、最终源树相同，可按 `repository-snapshot` 复用；`false` 重验，缺字段看原因。托管 copy/link、未初始化且未改动的 gitlink 内容列在 `excluded`；指针、其他索引标记及脏子模块仍核验。忽略文件、环境、数据库、Git 历史在范围外，改动/reply 后重判。

## 边界

1. **容量**：整机默认任务 12/Codex 6、重命令 2；每仓库任务 8/Codex 4（git worktree 算同仓库）。可用内存低于 4 GB 拒绝启动；命中上限说明层级、计数并列该层任务，先 `wait` 收一批；`--after` 等容量时两层都检查。验收、证据、setup、`lane` 排队不计时。上限由用户设定，同事不要自行调整。
2. **只读**：git 仓库里默认读启动时的工作区快照（独立 worktree，含未提交改动），你可以同时改代码；要读实时工作区加 `--in-place`。在快照里两位同事都有全部工具（能看 git 历史、跑测试），只读靠约定与事后核对，写了也只留在它自己的 worktree；非 git 目录或 `--in-place` 时 Pi 只剩读文件、搜索和列目录。
3. **写入**：原地写入同一目录同时只能有一个，且你同时改的文件会算进它的改动；要并行或不想被打扰就加 `--worktree`。
4. **worktree 依赖与验收**：`worktree.copy` 复制忽略材料，或用 `link`/`setup`（`writeSetup` 仅写入）；缺源/空源带 `warnings`。`env` 注入同事、验收、证据和 setup；`accept`/`evidence` 是写入默认，只读不用；CLI 覆盖或关闭。`applyVerify` 合并后主干复验。别 link `node_modules`/`.venv`。
5. **超时**：`--timeout` 默认 Pi 25 分钟、Codex 50 分钟（不含排队），到时若仍在执行命令再宽限最多 10%，最坏约 55 分钟，落在主控 1 小时提示缓存内。超时的写入任务改动保留，先 `reply` 续做；任务太大就拆小。默认值有意偏宽：被截断后的返工（续接、重读上下文、重跑验收）比多等几分钟更贵，不要为求快把 `--timeout` 压短。

命令与全部选项见 `$D --help`；run 目录、文件、环境变量与清理策略见 [references/output-and-files.md](references/output-and-files.md)。
