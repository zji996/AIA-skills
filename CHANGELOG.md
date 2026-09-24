# Changelog

## 未发布

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
