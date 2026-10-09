# delegate

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
- `apply` 成功后该任务的 worktree 与 run 目录已清理（`--keep` 保留），末行写明验收是否仍有效及需在主干重跑的命令；清理失败只提示、退出码仍为 0。
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
- 显式 `--agent pi` 未指定 tier：短行 pi/cheap，JSON tier cheap、agentPinned true；codex 默认 strong；旧记录只推断显示，固定同事仍不自动升档。
- 多项目同时委派：整机默认 12/6/2，每仓库默认 repoMaxActive 8/repoMaxCodex 4；git worktree 与主仓库同计，非 git 按规范化路径；命中上限说明层级、计数并只列该层任务，--after 等容量遵守两层。
- Claude Code Monitor 能将后台逐行输出变事件：优先后台 wait --stream；只有结束通知则重复后台 wait --any。新字段可选，protocol 保持 1。

## 5.17 用户配置与仓库事实

- 多个仓库共用禁止同事全量检查的规则：把 agentDeny、通用 env 与 maxRework 写入用户级 delegate/config.json，仓库只补自己的字段。Shared user config applies across repositories.
- 某仓库需要同事运行用户级禁止的一条命令：仓库 agentDeny 用完全相同 argv 和 allow: true 撤销，其他用户规则仍生效。
- 用户 env 与仓库 env 同键：仓库覆盖，其他键合并；仓库未声明 accept 时用用户默认，显式 --accept / --no-accept 优先。
- 换机器时设置 maxActive/maxCodex/maxHeavy：用户配置提供容量，仓库可覆盖，DELEGATE_MAX_* 最高；同事不得自行放宽容量。
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
- 让 GPT 用 Codex 独立审一下这个迁移方案，和 Gemini 的意见对照看看。
- （主控自发）我继续改代码，同时让 Codex 分运行时、API/调度、前端三路只读审查这批提交。
- 这十几篇文档让便宜的模型各自概括一下要点，我汇总。
- 这个修复交给强一点的模型做，测试过了再给我看 diff。
- 这是移动端设置页的截图，找个同事看看布局哪里别扭。
- 这个锁释放的并发 bug 交给 GPT 修，`go test ./...` 通过为准。
- （主控自发）要审 api、worker、web 三块的错误处理，各自独立，分给同事并行做。
- 让 Codex 在单独的 worktree 里改上传接口，别碰我现在的工作区，改完给我看改了哪些文件再合并。
- 刚才那个任务让 Pi 接着把测试补上，就在它原来的会话里继续。
- 把 `.local/scan` 里准备好的忽略材料复制进同事的 worktree，让它接着审查；我先去忙，回头再收 reply 结果。
- 继续上轮任务，并把我后来补的测试同步给同事；这次等它答完再返回。
- 让同事在 worktree 里完成接口改动，结论里要看目录分布、最大文件和配置文件变化，再决定是否合并。
- 让两个同事并行改同仓库不同目录，用 `--protect` 划分文件；按仓库 `.delegate.json` 的默认验收收结果。
- 给同事审查一个目录内最多五个高风险点，不要漫无边际扫描全仓库。

## 不应触发

- 把这个函数的变量名改成驼峰。（委派与验收的开销高于直接修改）
- 帮我配置 Pi 的默认模型。（是 Pi 配置问题，不是委派任务）
