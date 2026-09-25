# Changelog

## 未发布

### 技能

- `pi-delegation` 3.0.0（不兼容）：改为按结果委派，实现换成单文件 Python 标准库脚本 `scripts/pi_delegate.py`，不再依赖 `jq`、`setsid`、`timeout`；`pi-delegate.sh` 保留为转发入口，`pi-json-stream.sh` 移除（前台同步调用改用 `run`）。
  - 新增 `--accept <命令>`：Pi 结束后由脚本在 workdir 执行，状态分为 `delivered` / `answered` / `rejected` / `malformed` / `failed` / `timeout` / `killed` / `stopped` / `crashed`，取代原先只表示“有非空答复”的 `ok`。
  - 答复为空或是泄漏的工具调用（如 `call:default_api:read{...}`）判为 `malformed`，默认自动重跑一次（`--retries`）。
  - `run`/`wait` 默认一直等到结束且不打印过程，`--max` 可限时（仍返回 75），`--progress` 按需显示写入与错误；`files` 合并编辑记录与 workdir 的 git 变化，不计验收命令的副产物。
  - `meta.json`、`exit_code`、`.delivered` 的含义不变，`agent-handoff` 无需修改。
- `pi-delegation` 2.1.1：JSON stream 的控制台输出收敛为错误、写入、每 20 次动作计数及最终结果；完整过滤事件继续留档。更新对应 mock 测试，并实测只读 Pi 调用。
- `pi-delegation` 2.1.0：缺少 `pi`、`jq`、`setsid`、`timeout` 时一次列出全部缺失项和安装命令，`pi` 指向随仓库附带的 pi-kit 安装脚本。

### 安装

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
