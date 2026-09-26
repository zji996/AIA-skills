# Changelog

## 未发布

### 技能

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

### 安装

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
