# Changelog

## 未发布

### 仓库脚本

- 新增 `scripts/sync-hosts.sh`：经 SSH 并行更新其他机器上的 AIA-skills（`git pull --ff-only` 后重跑 `install.sh`，顺带取与新校验和匹配的二进制），每台一行汇报提交与 delegate 版本，任一失败退出 1 并附日志尾部。主机从参数或 `~/.config/aia-skills/hosts` 读取，`--` 之后的参数传给远端 `install.sh`；远端需已有检出（首次用 bootstrap.sh）。

### 技能

- `delegate` 5.21.0：同源已有写入任务在运行时，不带 `--worktree` 的写入任务自动改用 worktree，并在 stderr 说明一行；没有其他写入任务时仍原地写入。`apply` 时如果有原地写入任务正在改源工作区，警告它的提交可能把刚应用的改动一并带走。起因：polaris-os 一个忘加 `--worktree` 的写入任务在主控 apply 时原地改同一工作区，两边改动混在一个文件里。
- `delegate` 5.20.1：`apply` 合并删除后，顺带删掉因此变空的父目录（止于仓库根）。此前整包删除或改名后源仓库会留下一串空目录。
- `delegate` 5.20.0：`.delegate.json` 的 `generated` 新增可选 `inputs`（匹配语义同 `paths`）。设了它时，`apply` 只有合并路径命中 `inputs` 才排队重新生成，否则输出一行 `not regenerated` 跳过；此前纯文档改动也要在 lane 后面等生成器。未设时行为不变。
- `delegate` 5.19.0：既有同事/setup/验收后台回收阶段补查项目脚本经 `systemd-run --user` 启动的 service/scope，按完整 `DELEGATE_RUN_DIR` 环境标记或隔离 worktree 内的 WorkingDirectory/ExecStart 路径归属；in-place 只用标记，不按 unit 名前缀匹配。stop 超时后 SIGKILL，查询与停止均限时；缺 systemctl、无 user bus、回收失败仅记 cleanup 诊断，不改 state/退出码。`clean` 与过期清理删除前回收历史遗留，结论追加已停 unit 数与详情。补假 systemctl 黑盒回归与起因记录；二进制与校验和留待主控构建。
- `delegate` 5.18.1：运行中任务的短行只在同事进程启动后（`agent.pid` 已写）才实时统计改动。此前 `start` 刚返回时 worktree 还在检出，短行会显示“改 127 个文件 +0/-11052”这类假删除。
- `delegate` 5.18.0：新增 `--evidence`/`--no-evidence`/`--evidence-timeout`（默认 30m）及用户/仓库写入默认 `evidence`，reply 继承；验收通过后（无验收则答复后）经 lane 运行，沿用验收环境、原 PATH 与回收，结果 `evidence{exit,timedOut,seconds,tail,log}` 只作证据，任何失败永不改 state/退出码，验收失败与保护违规跳过。agentDeny 可选 `exact: true` 仅拦完全相等 argv，合并去重与 allow 撤销按 `(argv, exact)`。status/wait/clean 按源工作树最终内容、删除与执行位检测手动合入，全含写 `appliedBy: "detected"` 并建议清理，部分包含或读取失败沿用未合入逻辑。`--agent` 显示适配表默认档位并记 `agentPinned`，旧 meta 仅推断显示，不自动升档。容量改为整机默认 12/6/2、每仓库 `repoMaxActive`/`repoMaxCodex` 默认 8/4，支持 `DELEGATE_REPO_MAX_ACTIVE/CODEX`，worktree 按 git common dir 共计；拒绝解释命中层级与计数，等待同时遵守两层。protocol 新字段均可选，保持 1；同步参考文档、实战记录、触发示例及 Monitor 优先 stream 的说明。二进制与校验和留待主控审后构建。 `apply --merge` 写入冲突标记时不再提示“应用失败”，改为列出待解决的文件（JSON `apply.conflictMarkers`），退出码仍为 1；证据耗时取整到毫秒，避免 meta 与 `--json` 的浮点表示不一致。
- `repo-governance` 3.1.0：新增待收敛清单约定。早期为跑通而凑出来的命名、规则与实现登记在 `docs/convergence.md`（编号 `Cnn`：现状、问题、收敛方向、时机），代码落点标 `PROVISIONAL(Cnn)`，入口文件一行指向清单；信息分层表加一行。`audit-context.py` 新增 `provisional` 类别：git 跟踪文件里的标记在清单中找不到编号时报 WARN，可纳入 `--fail-on`。起因：polaris-os 的 processor 包叫 `harness-v3.0.0`，所有者以为是新一版提示词，并指出不少早期决定是 Agent 为完成任务凑的。
- `delegate` 5.17.1：自然语言短行不再显示“范围外：…”。这些路径只可能是托管 copy/link 或未初始化的参考 gitlink，都是按设计排除，每条结论都重复它们只是噪音；`--json` 的 `excluded` 不变。
- `delegate` 5.17.0：新增用户级 `${XDG_CONFIG_HOME:-~/.config}/delegate/config.json`，与仓库 `.delegate.json` 按标量仓库优先、env 按键覆盖、agentDeny 按 argv 合并去重；仓库 `allow: true` 可撤销精确用户规则。用户级默认验收、返工预算与机器容量跨仓库生效，容量环境变量仍最高。worktree/generated/applyVerify 只认仓库，用户出现时启动 stderr 提示一次；独立校验两份文件，错误指出文件与字段并退出 2。meta、summary 与 JSON 结论记录 configSources，自然语言短行不变；新增用户/仓库/合并/撤销/容量优先级/忽略提示/错误的黑盒回归。二进制和校验和留待主控审后构建。

- `delegate` 5.16.0：`.delegate.json` 新增 `agentDeny` argv 前缀规则，启动时解析真实程序绝对路径并生成同事专用 PATH shim；命中打印 hint、退出 77，同事 lane 同样受限，环境变量无放行开关。未命中 exec 透传参数、stdin/stdout 与退出码；验收、setup、apply 生成与复验、主控 lane 保持原 PATH。结论统计 `denied: N`，自然语言提示“拦下 N 次全量检查”。写入 timeout 的下一次 reply 记 `continuation: timeout`，不耗 maxRework，超限与连续超时也能续做；next 给可复制命令，其他失败仍按原规则。防护针对误用，不是同 UID 恶意绕过的沙箱。补黑盒回归并纳入现有验证入口；二进制与校验和留待主控审后构建。
- `iteration-speed` 1.0.0（新增）：先测无改动、上下游修改与完整门禁反馈，再选择分层验收、跨 worktree 缓存和模块拆分；bash 工具只读探测 Rust/Node/Python/Go/Docker，记录用户级耗时历史，预览或备份后写入用户级 sccache/mold 配置；附生态实践、Polaris 测量基线、触发示例与临时目录测试。
- `delegate` 5.15.0：托管 copy/link 路径从验收证据排除，结论用 `excluded` 标明；未初始化且目录空/缺失、指针未改的 gitlink 内容也列为范围外，指针仍参与树比较，源初始化后重新核验，非托管索引标记和脏子模块仍判不完整；保护路径与 `generated.paths` 重叠时允许生成命令改动，收尾经 lane 复跑并核对产出，漂移报“生成物被手改”；每任务等待者 flock 记录 PID，重复等待退出 76，死亡自动释放，逐个/流式/整机收取均避开覆盖；默认状态与结论改为自然语言短行，文件数、增删行及前三目录用仓库相对路径，运行中显示最近命令，完整原字段留在 `--json`，答复仍随后附上。旧测试显式加 `--json`，默认短行与子模块边界补回归，5.15 用例纳入仓库门禁。二进制与校验和留待主控审后构建。
- `scroll-gesture` 1.0.0（新增）：`scripts/input-probe.html` 自包含探针页，在目标环境记录滚轮/触摸/指针拖动，给出事件频率、间隔 p50/p90、deltaMode、增量分布、鼠标格占比与按 160ms 切分的事件流，可切换 preventDefault 模拟自定义手势接管，一键复制 JSON。SKILL.md 只列判断点：时间只区分同一段惯性、结果交给位置，动画可被接住，鼠标格缓动、触控板直跟，只接管过界部分，不劫持整页原生滚动，测试覆盖慢输入。起因：polaris-os 会话翻页在远程桌面划不动（第二段靠静默计时判定），Windows 鼠标格整格跳，且动画不可打断。
- `delegate` 5.14.0：写入对话的返工预算。`.delegate.json` 新增 `maxRework`（缺省 1，`null` 不限），沿 reply 链统计有改动的返工轮次，超限的 `reply` 退出码 2 并提示主控 `diff --total`/`apply` 后接手。`reply --minor` 用于短修正（说明 ≤600 字符、默认超时 10m），不计次数，但改动超过 60 行时事后补记；`reply --over-limit REASON` 显式越限并记录原因。零改动的追问与只读对话不计数，换同事照样计数。meta 记 `rework{kind,used,limit,overLimit?}`。起因：项目所有者要求同事最多返工一次、之后主控自己改，并希望由脚本约束而不是靠自觉。
- `delegate` 5.13.0：`.delegate.json` 新增 `applyVerify`（`true` 用顶层 `accept`，或直接写命令），`apply` 合并成功且 `acceptStillValid` 不为 true 时经 lane 在源工作目录复验，结论带 `verify`，失败退出 1（合并已写入），`--verify` 强制、`--no-verify` 跳过；多路合并后的类型漂移不再依赖主控记得跑全量。`start`/`run` 创建写入任务时列出同源仍在运行的写入任务及其已改文件，任务说明点名其中路径时以 `overlap:` 提示 `--after`/`--protect`，只提示不拒绝。二进制和校验和留待发布时构建。
- `delegate` 5.12.0：`wait --any/--stream` 新增首次检查边界的 `completionTiming`，统一终态写入 `finishedAt`（旧记录与推断 crashed 不补造时间），保持收取顺序、state、退出码及 protocol 1。`apply` 的检查、lane 排队和生成阶段实时写 stderr，附绝对日志路径与执行超时；生成失败保存待生成标记，零文件动作重试也运行生成器。末尾新增 apply 结论：以 lane 内验收前后全树快照和生成后的源全树判断 `acceptStillValid`，证据不全省略布尔值并解释，大型未跟踪文件与脏/不可读取子模块不误报有效，忽略文件、环境、数据库、Git 历史不在范围内。新增可重复 `--protect-reason PATH REASON`，归一化、冲突原因拒绝、reply/fresh 继承，附“需改保护路径时停下报告，不得搬逻辑/削弱测试绕路”的中英文约定。新增文件与源中计划保留文件的同目录 ASCII 数字前缀冲突只警告（含 dry-run，保留前导零、排除删除和生成路径），同仓库须串行 apply。零改动 reply 用 `pendingChanges` 保留累计 diff/apply 建议；修正 spec 的冲突标记推进基准说明。源码与文档更新，二进制和校验和留待主控审后构建。
- `openai-image-gen` 2.0.0（不兼容）：`generate-image.sh` 换成标准库 Python 脚本 `imagegen.py`，不再依赖 curl/jq，分为 `draft`/`final`/`edit`/`status`/`reset` 子命令。`draft` 用 `gpt-image-2.5-flare`、`low`、同比例缩到长边 1536 出草稿，存在 `$XDG_STATE_HOME/openai-image-gen/` 下；`final` 默认用 `gpt-image-2.5-sunburst`、`high`，以选中的草稿作 edits 参考并追加保持构图的指令，可用 `-f` 换修正后的提示词，`--fresh` 从头生成；`edit` 只能在正式图之后，原图留作 `previous`。每个输出路径限定草稿 3、正式 1、编辑 1 次（只计成功调用），超出以退出码 3 拒绝且不请求接口，`--allow-extra` 仅在用户同意时使用。结果 JSON 带 `budget` 与 `next`。SKILL.md 精简为流程与预算，提示词写法、模型与尺寸细节移到 `references/prompting.md`。起因：一张架构海报用了约 8 次正式档调用。首次实网使用时发现：edits 的图片部分必须带图片 MIME（按文件内容判断）；公司网关忽略 `size` 与 `output_format`，一律返回 1672x941 PNG，所以结果 JSON 新增 `returned`（按文件头读出的实际格式与尺寸），与请求不符时带 `warning`，草稿文件按实际格式命名。
- `openai-image-gen` 1.3.0：新增 `-i/--image` 走 edits 接口在已有图上局部修改（multipart，提示词经文件传，引号与换行不丢）；`--format` 与 `--compression`，输出扩展名为 `.webp`/`.jpg` 时自动请求对应格式；结果 JSON 增加 `quality`、`mode` 与 `seconds`。SKILL.md 按 gpt-image-2.5 更新：sunburst 与 flare 的取舍、`xhigh`/`max` 何时用（实测 2560x1440 下 `xhigh` 与 `high` 耗时相当）、任意尺寸的约束、文档配图用 webp、连续编辑会让别处走样、箭头方向跟随元素位置、费用意识与“差不多就行”时停手。
- `openai-image-gen` 1.2.0：新增 `--prompt-file <文件|->`，长提示词从文件或 stdin 读取，不再受 shell 引号限制；与 `--prompt` 互斥，空提示词报错。输出图片的权限改按调用者 umask（此前沿用 mktemp 的 0600，别人和 Web 服务读不到）。SKILL.md 补充示意图与带文字画面的写法：按 LAYOUT / STYLE / TEXT RULES / AVOID 分节，箭头写明起止元素，列全文字串并禁止其它文字；提示词一律用英文，画面文字保留原语言；提示词存成输出旁的 `.prompt.txt`；逐字核对后再迭代；`high` 质量单次约 45 秒，超时不低于 300 秒。起因：一次中文架构图请求只用了一句中文提示词、90 秒超时。
- `repo-governance` 3.0.3：信息分层表新增 `docs/explainers/*.html`，放给人看的设计与原理图解，是 ADR 与代码的派生说明，Agent 不必读取。
- `delegate` 5.11.0：逐个收取。`wait --stream` 每个任务结束时输出一行结论（`report` 字段给出读答复的命令，不标记已送达），全部结束后退出；不带任务参数时每 5 秒按同一规则重选，期间新派出的任务也纳入。`wait --any` 在任一任务结束时返回，只输出已结束的，其余在 stderr 列出；已送达的具名任务会跳过，重复同一条命令即可收完。两者与宿主无关：能把每行输出变成通知的宿主用 `--stream`，只有结束通知或只能分段调用的宿主用 `--any`（配合 `--max`）。结论块与流式行新增 `sourceDrift{files,overlap}`：worktree 写入任务未 apply 时，报告源工作区自快照以来变化的文件数及与同事改动重叠的文件；有重叠时 `next` 改为提示 `apply` / `--merge` / `reply --sync` 的取舍（`status` 不计算）。协议文档写明三种宿主能力下的收取方式，Claude Code hook 的拒绝提示补充 Monitor + `wait --stream` 与 `wait --any`。
- `delegate` 5.10.1：`apply --merge` 写入冲突标记后也记为已合并（此前只在无冲突时记录，`clean` 会误报“its worktree was never applied”，再次 `apply` 还会把已合并的改动当作未合并）；只有被跳过的二进制、符号链接等文件仍让该任务保持未合并。
- `delegate` 5.10.0：默认超时改为 Pi 25m、Codex 50m，超时宽限 `DELEGATE_TIMEOUT_GRACE` 默认从 50% 降到 10%：最坏约 55 分钟，落在主控 1 小时提示缓存内（此前 30m×1.5 可达 45 分钟，而大任务常在 30 分钟时被截断）。`timeout` 的下一步提示改为用 `reply` 在保留的改动上续做（只读任务则让它先答复已有结论）。SKILL.md 补充并行路数的经验：写入 3–4 路，主控在同事跑测试时用 `lane` 跑自己的重检查。边界 5 写明默认值有意偏宽的理由：截断后的返工比多等几分钟更贵。
- `delegate` 5.9.1：`protocol` 的 `shadowed` 按 PATH 中的原路径列出（此前解析为真实路径，.6 上被遮住的 `/snap/bin/codex` 显示成 `/usr/bin/snap`）。
- `delegate` 5.9.0：与宿主解耦。新增 `docs/delegate-protocol.md`（宿主接入协议 protocol 1）：宿主只需提供后台执行与通知、会话标识和技能加载，可依赖的状态行字段、state 与退出码在其中承诺。会话标识在 `DELEGATE_CALLER`、`CLAUDE_CODE_SESSION_ID` 之后依次读取 `CODEX_THREAD_ID`、`PI_SESSION_ID`（Codex 与 Pi 给所执行命令导出的会话 id），meta 新增 `callerSource`。新增 `delegate protocol`：一行 JSON 输出协议版本、caller 及来源、各同事担任的档位、实际执行文件、版本与 PATH 中被遮住的同名文件。同事差异（命令行、事件解析、会话续接、只读契约、默认超时、是否占强档名额、安装提示）收拢到 `agents.rs` 的 `AGENTS` 适配表，行为不变。

## 3.0.0 - 2026-09-29

### 仓库脚本

- `release-binary.sh verify` 在发布前本地核对技能与 crate 版本、提交状态、dist 校验和及 tag 指向；`publish` 自动执行。
- `install.sh` 的复制和链接切换可回滚；`fetch-binary.sh` 对本机构建校验并标记来源，严格模式拒绝不匹配产物。
- 新增 `verify.sh` 一键门禁与可选的仓库级 pre-push hook；`check.sh` 要求中英文触发词同时出现。
- `release-binary.sh publish`：本机没有 Forgejo token 时，可用 `FORGEJO_TOKEN_SSH=<user@host>`（或 `~/.config/aia-skills/forgejo-token-ssh` 中的同样内容）在发布时经 SSH 读取那台主机上 `FORGEJO_TOKEN_FILE` 同路径的 token，token 只保存在一台主机上；构建不访问网络取 token。

### 技能

- `delegate` 5.8.1：修复失败路径。`apply` 的删除、复制、写入失败会写明文件与原因并以非 0 退出，不再写 `.applied`，`clean --finished` 保留未合入的写入 worktree（此前删除失败被吞掉，仍标记为完整合入）；supervisor 启动超时时先回收其进程组再写 `crashed`；`generated.command` 走限时执行与进程组回收（`DELEGATE_GENERATE_TIMEOUT`，默认 10m），超时即失败并释放重任务名额；`wait --help` 改正无参与 `--all` 的范围说明；meta 与状态行记录同事可执行文件的实际路径 `agentBin` 与 `agentVersion`（某机器非交互 shell 解析到旧版 snap codex 时，失败原因此前看不出来）。
- `agent-handoff` 2.1.3：正确解析含转义字符的 worktree 路径，并说明新旧委派目录与未合并 worktree。
- `repo-governance` 3.0.2：description 增补 token 预算和上下文健康度触发词。
- `delegate` 参考文档补充 `.delegate.json` 的 `generated` 配置与 §6.4 行为，技能版本不变。
- `delegate`（文档与 hook，版本不变）：新增 `hooks/claude-code-background.py`，作为 Claude Code 的 PreToolUse hook 拒绝前台执行的 `wait`、`run`、`reply --wait`，提示改用 Bash 的 `run_in_background`；只匹配真实调用（`…/bin/delegate` 或 `$D`），跳过 heredoc 正文与 `--help`。SKILL.md“等待”一节写明 Claude Code 下默认后台运行，`--max` 只用于没有后台通知的主控。
- `repo-governance` 3.0.1：预算配置中精确路径总是优先于 glob，与书写顺序无关；此前写在通配之前的单文件上限会被通配覆盖。
- `repo-governance` 3.0.0：审计体积改按估算 token 预算，阈值由被审计仓库的 `.repo-governance.json` 管理；新增 glob 预算、`--report` 与 `--only`，保留旧行数和 KB 参数并提示弃用。aia-skills 的 `check.sh` 改用同一预算机制检查 SKILL.md，超预算仍只警告。
- `delegate` 5.8.0：派发时记录主控 caller，无参 `wait` 只收本会话任务，其他未收任务汇总提示且不影响退出码；`start` / `run` / `reply` 提示其他会话遗留的已结束任务，`status` 增加任务年龄。未识别主控会话时，无参 `wait` 沿用收取全部任务的行为。
- `delegate` 5.7.0：`.delegate.json` 新增 `worktree.writeSetup`，只在写入任务里、`setup` 之后执行，给慢且只有构建测试才用得上的环境（如含 torch 的 venv）；只读任务跳过，侦察不必等。
- `delegate` 5.6.0：用户 systemd 可用时为同事、worktree setup 与验收各建独立 scope，结束时按 cgroup 回收并报告脱离进程组、清掉环境标记的后台进程；不可用或 `DELEGATE_CGROUP=0` 时沿用进程组与环境标记清理；回收先 SIGTERM 宽限 3 秒再 `cgroup.kill`，给同事留出写完会话的时间。`wait`/`status` 等按 run id 查找时，本仓库找不到会退到整机正在运行的任务，从别的目录执行 `next` 提示也能找到。
- `delegate` 5.5.0：新 run 默认放 `.local/run/delegate`，继续查找旧 `.local/run/pi`；长答复保留头尾；结束时清理并报告后台进程及监听端口；空/缺失的 worktree link/copy 源进入 `warnings`；写入任务支持 `.delegate.json` 顶层默认 `accept`、`--no-accept` 关闭。补充并行文件所有权与仓库门禁经验。
- `repo-governance` 2.2.0：新增 `adr-index.py`，从各 ADR 的标题与状态行生成 `INDEX.md` 中标记之间的表格（兼容 `状态: x`、`- 状态：x`、`- **Status**: x`），默认只核对、`--write` 重写；审计同时报告过期索引。审计新增 `--fail-on`，门禁可只让断链、点名不存在和索引过期失败，体积与过期类只打印。
- `repo-governance` 2.1.1：点名路径检查只看带目录的路径；不带目录的裸文件名（如技能里的 `audit-context.py`）多半在仓库外，此前会误报。
- `repo-governance` 2.1.0：审计新增三项只读检查：`docs/current.md` 体积超预算（默认 8 KB）、带日期的条目过多（默认 5 条，常见于把上线流水写进 current.md）、入口文件与 `current.md` 用反引号点名的路径或 `make` 目标不存在（被 gitignore 的路径、通配写法与首段不在仓库里的记法不报）。信息分层表写明 AGENTS.md 放"改动如何生效"，current.md 只留未收口的状态、已完成记录在提交信息里。
- `delegate` 5.4.0：`.delegate.json` 的 `worktree.copy` 明确支持复制被 git 忽略的文件与目录，缺源跳过并向 stderr 提示；`reply` 默认后台立即返回，`--wait` 显式等待结论；`reply --sync` 忽略未改变指针的 gitlink，保留对指针变化的冲突检查；写入任务结论从快照 diff 汇总目录、改后最大文件、配置路径与删除路径的 `shape`，列表有界且可调；用对话名引用时 `status`、`wait`、`result`、`diff`、`stop` 跟到最新一轮回复（与 `apply`、`reply` 一致），run id 仍精确指向该轮。
- `delegate` 5.3.2：实战中发现并修复。`wait` 只显示了长答复的末尾时，run 记为 `.truncated`，`clean --finished` 与自动过期清理都保留它，并提示用 `result` 读全文（此前长报告读到一半就被清理，全文丢失）；完整打印或 `result` 成功写出全文后解除。SKILL.md 与 recipes 补充：便宜档在配置键、CLI 参数等仓库外事实上会编造，采纳前须对照一手来源核实。
- `delegate` 5.3.1：实战中发现并修复。`clean` 按 worktree 自身的 `git-common-dir` 找到所属仓库再移除并 prune，不再依赖记录的来源路径；此前 `--in` 审查者的来源是上游 worktree，若先清理上游再清理审查者，审查者的 worktree 登记会残留在仓库里（`git worktree list` 显示 prunable）。
- `delegate` 5.3.0：两项易用性改进。`reply --sync`：续接前把主控在任务启动后的改动（比如新写的测试）三方合并进对话的 worktree，冲突则停下、不启动同事；同步来的文件不算同事的改动，之后的 `apply` 也不会重复合并它们（此前只能另起任务）。`wait --machine`：从整机登记收取所有仓库中运行中或等待中的任务（此前每个仓库要各放一个 `wait`）。
- `delegate` 5.2.1：在真实任务中使用 5.2.0 时发现并修复。**分轮合并**：同一对话已合并过一轮后，再次 `apply` 以上次合入的状态为基准，只合并新增的改动（此前第二轮改到第一轮新加的行就会整体冲突）。**`--in`** 的快照排除上游的 link/copy 路径，审查者不再看到子模块变成符号链接的假改动。等待容量改为事件唤醒，不再每 200 ms 醒一次。
- `delegate` 5.2.0：**编排**——主控声明"先 A 后 B"，由 delegate 执行，每一步仍是一层委派、结果都回到主控。`--after <run>`：立即创建 `waiting` 的 run，上游以 delivered/answered 结束后执行（阻塞在上游生命周期锁上），否则以 `skipped` 结束；等待期间不占名额，上游答复与改动的路径自动附在说明末尾。`--in <run>`：只读地在上游 worktree 的快照里工作（`git diff HEAD` 即上游改动），绝不改动上游。`reply --agent/--tier`：同一 worktree 换一位同事接着做。`.delegate.json` 的 `generated`：生成文件不参与三方合并，`apply` 后在源仓库重新生成，让都改契约的任务也能并行。任务名可按完全匹配的名称引用（`impl` 不再与 `impl-review` 冲突）。SKILL.md 的经验建议加入两种常用编排写法。
- `delegate` 5.1.1：整机并发默认上限调为 8 个任务、其中 Codex 4 个（原为 6 与 3）。按这个项目的实测，能真正并行的独立工作流约 4～5 条，再多会让交付扎堆、主控审查质量下降；机器资源（每个 Codex 约 185 MB）不是瓶颈。
- `delegate` 5.1.0：**同事为跑测试而初始化的子模块不再算作改动**：worktree 中起始未初始化、结束时干净且检出提交等于记录值的子模块视为仅初始化，不进改动清单、`apply` 不处理（此前每次合并前都要手动 deinit）。新增 **`--protect <路径>`**：受保护路径被改动即判 `rejected` 并带 `protectViolation`，不运行验收，替代在验收命令里手写 `git diff --quiet`；任务说明会写明哪些路径不许改，reply 沿用。SKILL.md 按同题对比重新定位两档（Pi：广度侦察、界面与前端、另一路视角；Codex：严密判断与风险、核实），新增"什么时候自己接手"与续接会话的判断；任务说明模板要求现状断言附 `文件:行号`。
- `delegate` 5.0.1：实际使用中发现并修复。**只读 Pi 在自己的 worktree 中不再被限制工具**：隔离加事后核对已经兜底，它与 Codex 一样拿到全部工具（能看 git 历史与 diff、跑测试）、收到只读约定、执行 `setup`；只有无法隔离（非 git 或 `--in-place`）时才只保留读文件、搜索、列目录。**只读 worktree 保留快照 index**（`reset --soft`），主控未提交的子模块指针不再被误报为违规。新增 `--help`（顶层与各子命令）与 `--version`，状态行以 `run`、`name`、`state` 开头；`diff` 支持 `--` 分隔路径；升档补只读约定时不重复。Cargo 版本与技能版本一致（`check.sh` 检查）。`install.sh` 可用 `AIA_SKILLS_SKIP_BINARIES=1` 跳过二进制，安装测试不再依赖网络与编译器。新增 `docs/delegate-field-notes.md`，记录实战中的问题、做法与数据。
- `delegate` 5.0.0（不兼容）：**只保留 Rust 实现**，入口由 `scripts/delegate.py` 改为 `bin/delegate`（Linux x86_64 / aarch64 的 musl 静态二进制，不再需要 Python）；Python 实现移除，最后一版见 tag `delegate-py-4.5.0`。行为不变，契约仍是 `docs/delegate-spec.md`，`tests/test_delegate.py` 默认测试已安装的二进制。
- `delegate` 4.5.0：**只有一层委派**——同事（设有 `DELEGATE_AGENT` 等）调用 `start`/`run`/`reply` 一律拒绝，去掉父 run 的写入互斥豁免与 `DELEGATE_PARENT_RUN`，所有结果回到主控。**按档位选同事**：新增 `--tier cheap|strong`，只读默认便宜档、写入默认强档，档位映射由 `DELEGATE_CHEAP_AGENT` / `DELEGATE_STRONG_AGENT` 配置（默认 pi / codex），`--agent` 仍可直接指定；便宜档未安装时自动用强档。**自动升档**：便宜档畸形、出错、超时或验收失败，且为只读或未产生改动时，在同一 run 中换强档重跑一次（强档有容量时），结论带 `escalatedFrom`，升到 Codex 的只读任务补上只读约定。`wait` 不带参数时等所有未结束与未读取的任务。Rust 实现同步到 4.5（Codex 实现、主控审查），两个实现通过同一套 50 项黑盒用例。SKILL.md 改写为三步用法、场景速查与档位表，新增按需阅读的 `references/recipes.md`（任务说明模板、场景示例、常见坑）。
- `delegate` Rust 实现（`crates/delegate`，与 Python 版并存，暂不替换）：由 Codex 按 `docs/delegate-spec.md` 实现，依赖仅 serde_json、libc、sha1_smol；release 约 1.1 MB，可 musl 静态链接。运行中的 supervisor 常驻约 2.9 MB（Python 版约 22 MB），空闲时零周期唤醒（10 秒内各线程上下文切换 0 次，修复前 697 次）。通过全部黑盒一致性测试（`DELEGATE_BIN`）。主控审查后修复了首版的 6 处问题：非 UTF-8 输出行会使读循环停止、三处周期轮询改为事件驱动（self-pipe、条件变量、阻塞 `waitid`）、lane 停止信号丢失唤醒、看门狗可能向复用的 PID 发信号。
- `delegate` 测试：新增非 UTF-8 输出行不中断读取的用例；规格注明 JSON 空白与字段顺序不属于契约。
- `agent-handoff` 2.1.2：读取 delegate 的 `meta.json` 时容忍任意 JSON 空白，Rust 实现写出的紧凑 JSON 此前会使"未合并 worktree"漏报。
- `delegate` 4.4.0：新增整机重任务队列 `lane`：验收命令、worktree `setup`、同事自检与主控的 `delegate.py lane <命令>` 先进先出排队，默认同时一个（`DELEGATE_MAX_HEAVY`）；基于 flock 由内核唤醒，不轮询，进程崩溃即释放。排队时间不计入验收、setup 与同事的超时；同事超过 `--timeout` 时若仍在执行命令或刚有动静，最多宽限 50%（`DELEGATE_TIMEOUT_GRACE`）。验收与 setup 在独立进程组运行，超时或结束后整组清理，此前超时只杀 shell、子进程会残留。可用内存低于 `DELEGATE_MIN_AVAILABLE_MB`（默认 4096）拒绝启动。`.delegate.json` 新增 `env`，注入同事、验收与 setup（如 `CUDA_VISIBLE_DEVICES=""`）。`wait`/`run` 改为阻塞在 supervisor 的生命周期锁上，任务结束即返回。修复快照漏记：复制 index 时未保留 mtime，同一秒内写入且大小不变的改动可能不计入改动清单（git racy-clean 检测失效）。
- `delegate` 4.4.0 经 Codex 审查后修复：排队中收到 stop 不再可能在取得名额后照常验收；`DELEGATE_LANE_HELD` 不再泄漏给经 lane 启动的 run；终止 `lane` 进程会先结束其命令的整个进程组；命令组在回收 shell 之前清理，不会按可能被复用的 PGID 发信号；同事超时改用单调时钟；信号处理函数中不再做不可重入的等待或启动线程。只读 worktree 的 HEAD 重置为主控的提交，未提交改动在 `git diff HEAD` 中可见。
- 新增 `docs/delegate-spec.md`：delegate 的实现契约（命令、输出、状态机、快照、lane、锁与文件），供其他实现与 harness 接入；不在技能目录内，技能加载时不读取。`tests/test_delegate.py` 改为纯黑盒一致性套件，`DELEGATE_BIN` 可指向任意实现。
- `delegate` 4.3.0：只读任务在 git 仓库里默认于独立 worktree 读启动时的工作区快照（含未提交改动），主控同时编辑不再被误判为同事违规；`--in-place` 退回原地读取。state 只描述答复：只读任务改了文件时仍为 `answered`，另带 `readOnlyViolation`（worktree 内，已隔离、不能 `apply`）或 `workspaceChanged`（原地运行，无法区分改动归属）与 `warning`，不再判为 `failed`。结论行新增 `next`，按状态给出可直接执行的下一步（`wait` / `diff` / `apply` / `reply` 等），SKILL.md 状态表相应精简；补充按子系统拆分审查、用完成通知代替轮询的指引。只读 Pi 的 worktree 不执行 `setup`。
- `agent-handoff` 2.1.1：只读委派的 worktree 不再列为"未合并"。
- `openai-image-gen` 1.1.1：凭据改为优先使用 Codex 当前选中 provider 的 `base_url` 与 Bearer key，再退回 `auth.json` 的 key；`auth.json` 的 key 按 Codex 的规则发往选中 provider（`requires_openai_auth`）或官方 API。此前 `auth.json` 有 key 时总被发往官方 API，配置了代理的机器会直接 401。
- `delegate` 4.2.0：在真实前端任务中发现并修复：`reply` 改为每次尝试都从上一轮会话分叉（Pi `--fork`、Codex `exec fork`），上一轮会话从不被改动，畸形重跑从同一处重来；结局为 `malformed` 的轮次不算对话的延续，之后的 `reply`/`apply` 从它的上一轮接着；新增 `reply --fresh`，在同一 worktree 与对话里开新会话（Pi 在很长的会话上续接时容易连续畸形）；`apply` 合并 worktree 的现状，包含最后一轮之后在 worktree 里做的手工修改，并给共用该 worktree 的所有 run 标记 `.applied`。
- `delegate` 4.1.2：修复第二轮审查的 7 个问题：supervisor 被杀后同事进程仍在运行时，`stop` 会终止它，`clean` 与写入互斥、整机并发都把它算作运行中；两个并发 `reply` 不再写同一 worktree；`reply` 启动时的过期清理不再删掉尚未复制的父会话；非 UTF-8 文件名不再使快照失败，快照失败时结论带 `warning` 并显示“changes unknown”，不再默默当作没有改动；子模块检出内的改动计入改动清单（`submodule contents`），worktree 中的子模块可写进 `link`；worktree 改为以快照提交（`commit-tree`，父提交为 HEAD）为起点，没有提交的新仓库也能用；`reply` 取消或隐藏已变更的验收命令时，告知同事旧的完成标准不再适用。
- `delegate` 4.1.1：脚本按职责拆分，`scripts/delegate.py` 仍是唯一入口（命令与参数），实现放在 `scripts/delegate_core/`：`common`（设置与小工具）、`runs`（run 记录、状态、整机并发）、`agents`（Pi / Codex 的启动、续接与事件解析）、`changes`（快照与改动清单）、`worktree`（准备、清理与 `apply`）、`supervise`（重跑与验收）、`launch`（创建 run）。依赖单向、无循环；各定义逐字搬移，行为不变，仍只用标准库。
- `delegate` 4.1.0：结论后输出 `changes` 改动清单（状态、路径、+/- 行数，没改时显示 `none`），按运行前后的工作区快照（借用 index 副本写 tree 对象，不动真实 index）计算，shell 改的文件也算，运行前的脏改动与验收副产物不算；新增 `diff` 查看完整差异。新增 `--worktree`：在仓库外的 detached worktree 里运行，从当前工作区快照起步，按仓库根 `.delegate.json` 复制/链接被忽略的文件并执行 `setup`（如 pnpm/uv 离线安装依赖）；`apply` 把整段对话的改动三方合并回原工作区，冲突时什么都不写（`--merge` 写冲突标记）。新增 `reply`：在同一会话、同一 worktree 里追问（Pi 改为保存会话，Codex 用 `exec resume`）。`agent-handoff` 会列出尚未合并的 worktree。
- `agent-handoff` 2.1.0：快照列出 `delegate --worktree` 尚未 `apply` 的 worktree（同一对话只报最新一轮）。
- `delegate` 4.0.0（不兼容）：由 `pi-delegation` 改名，脚本改为 `scripts/delegate.py`，移除 `pi-delegate.sh` 转发入口；重新运行 `install.sh` 会清理旧名链接。环境变量改为 `DELEGATE_*`，旧的 `PI_DELEGATE_*` 仍可读取；run 目录仍为 `.local/run/pi/`。新增整机并发上限（跨项目、含嵌套子任务，默认 6 个，其中 Codex 3 个，`DELEGATE_MAX_ACTIVE` / `DELEGATE_MAX_CODEX` 调整，超出即拒绝并列出运行中的任务）；新增 `--image` 给两位同事附图；Codex 固定以 full access（`--dangerously-bypass-approvals-and-sandbox`）运行，不再依赖各机器的 `config.toml`。description 与 SKILL.md 改为写明主动委派的时机和两位同事的特点与分工。
- `pi-delegation` 3.2.0：新增 `--agent codex`，通过 `codex exec --json` 让 GPT 作为同事，共用验收、结论、run 目录与写入互斥；委派层级改为 Pi 不能再委派、Codex 可委派给 Pi 但不能委派给 Codex（`PI_DELEGATE_AGENT` / `PI_DELEGATE_PARENT_RUN`），委派者自己的 run 不阻塞其子任务写入。Codex 只读任务在说明中写明边界并以 git 核对结果（部分系统的 AppArmor 限制使其沙箱不可用）。SKILL.md 按任意主控模型可读的方式重写，明确采纳由主控把关。
- `pi-delegation` 3.1.0：`--accept` 的命令默认以“完成标准”附在提示词末尾（按提示词语言用中文或英文），减少 Pi 自行摸索验证方式的轮次；`--hide-accept` 保留盲验。
- `pi-delegation` 3.0.0（不兼容）：改为按结果委派，实现换成单文件 Python 标准库脚本 `scripts/pi_delegate.py`，不再依赖 `jq`、`setsid`、`timeout`；`pi-delegate.sh` 保留为转发入口，`pi-json-stream.sh` 移除（前台同步调用改用 `run`）。
  - 新增 `--accept <命令>`：Pi 结束后由脚本在 workdir 执行，状态分为 `delivered` / `answered` / `rejected` / `malformed` / `failed` / `timeout` / `killed` / `stopped` / `crashed`，取代原先只表示“有非空答复”的 `ok`。
  - 答复为空或是泄漏的工具调用（如 `call:default_api:read{...}`）判为 `malformed`，默认自动重跑一次（`--retries`）。
  - `run`/`wait` 默认一直等到结束且不打印过程，`--max` 可限时（仍返回 75），`--progress` 按需显示写入与错误；`files` 合并编辑记录与 workdir 的 git 变化，不计验收命令的副产物。
  - 保留 2.1.1 的启动加固：启动时持有运行记录根目录的锁（标准库 `fcntl`，不再需要 `flock` 命令）直到 supervisor 就绪；超过 15 秒仍未就绪的 `starting` 记录判为 `crashed`，不再阻塞同目录写入。
  - `meta.json`、`exit_code`、`.delivered` 的含义不变，`agent-handoff` 无需修改。
- `pi-delegation` 2.1.2：JSON stream 的控制台输出收敛为错误、写入、每 20 次动作计数及最终结果；完整过滤事件继续留档。更新对应 mock 测试，并实测只读 Pi 调用。
- `pi-delegation` 2.1.1：更早委派可独立验收的任务，短评审建议限制运行时间与答复长度；启动写入任务时持有 `flock` 至 supervisor 就绪，过期 `starting` 记录不再永久阻塞同目录写入，并明确互斥仅覆盖同一运行记录根目录。
- `pi-delegation` 2.1.0：缺少 `pi`、`jq`、`setsid`、`timeout` 时一次列出全部缺失项和安装命令，`pi` 指向随仓库附带的 pi-kit 安装脚本。

### 仓库

- `tests/test_delegate.py` 未设 `DELEGATE_BIN` 时先 `cargo build` `crates/delegate` 并测试产物；没有源码或 cargo 才退回已安装的 `bin/delegate` 并在 stderr 提示。此前默认测已安装的二进制，改了源码忘记重建时，测试会在旧代码上"通过"（5.4.0 开发中实际发生过）。

### 安装

- 技能可在 frontmatter 声明 `binary: <名>`：`install.sh` 在链接或复制前调用新增的 `scripts/fetch-binary.sh`，从 GitHub Release（`<名>-v<版本>`，次选 Forgejo）下载对应架构的二进制，按技能内提交的 `bin.sha256` 校验，不符即拒绝；下载不到而本机有 cargo 时从 `crates/<名>` 编译，`--build` 强制编译。新增 `scripts/release-binary.sh build|publish <技能>` 负责构建两个架构并发布（GitHub 为主，设了 `FORGEJO_TOKEN` 时也发到 Forgejo；可重复执行，已有的附件不重复上传）；`check.sh` 检查 `bin.sha256` 与源码齐全。`bootstrap.sh` 不再检查 python3。
- `install.sh` 清理旧条目时，指向不含 `SKILL.md` 的目录的链接也视为过期：技能改名后旧目录常因残留 `__pycache__` 而未被删除，此前这类链接不会被清理。
- `bootstrap.sh` 新增 `--with-pi` / `--with-pi-sync`：拉取 `third_party/pi-kit` 子模块并安装 Pi，最后提示仍缺少的委派依赖。
- `third_party/pi-kit` 子模块改用相对地址，GitHub 克隆会从 GitHub 拉取 pi-kit。
- pi-kit 升级到 v1.5.0：`--sync` 保留 `defaultProvider`、`defaultModel` 等本机设置，只有键顺序或格式不同时不再算作改动；删除旧版写入的 pi-hashline-edit-pro 配置；优先使用 npm 全局目录里的可执行文件，避开指向旧 nvm 版本的链接；npm 全局目录不可写时改装到 `~/.local`。

## 2.0.0 - 2026-09-24

首个公开版本。

### 技能

- `repo-governance` 2.0.0：仓库上下文信息分层，附只读审计脚本 `audit-context.py`。
- `agent-handoff` 2.0.0：`handoff-snapshot.sh` 自动采集 Git 与委派任务状态，生成交接账本草稿。
- `pi-delegation` 2.0.0：通过 Pi 在后台并行委派任务；`PI_DELEGATE_ACTIVE` 拒绝嵌套委派，且声明 `exclude-agents: pi` 不安装给 Pi。
- `openai-image-gen` 1.1.0：调用 OpenAI Image API 生成图像并直接落盘。

### 安装

- `scripts/bootstrap.sh` 一键安装：默认从主仓库克隆，失败时回退到 GitHub；重复运行即更新。
- `scripts/install.sh` 默认以符号链接安装到 `~/.agents/skills/` 与 `~/.claude/skills/`；支持 `--copy`（带版本标记）、`--status`、`--uninstall` 与 `metadata.exclude-agents`。

### 仓库

- `check.sh` 校验 frontmatter、触发描述、版本号、`evals/` 触发示例、断链与脚本语法。
