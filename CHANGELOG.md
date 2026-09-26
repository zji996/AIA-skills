# Changelog

## 未发布

### 技能

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
